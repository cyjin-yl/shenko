//! Prepared transient context contributions — docs/specs/context-policies.md#spec-37-7, docs/specs/context-policies.md#spec-41-8-1.
//!
//! A contribution is material a plugin prepared *before* the request that
//! uses it: a transient memory reminder, an awareness perspective, a task
//! state note, the notice that a compacted range is only a derivative. It
//! names its producer, the scope it applies to and when it stops being
//! valid, and the registry records what was actually sent — so "what did the
//! model see in this turn" stays reconstructable without the contribution
//! becoming a permanent part of the history.
//!
//! The contribution type itself is shared protocol data
//! (`shenko_types::plugin_runtime`); this module owns the behaviour around
//! it:
//!
//! * [`ContributionProvider::prepare`] is the slow path. It runs off the
//!   request path (a worker, a preparation task) and produces contributions
//!   bound to one [`ContributionScope`].
//! * [`ContributionRegistry::collect`] is the request path. It is a local
//!   read of already prepared material: it merges by slot and key with the
//!   kernel's one merge rule, drops expired entries, honours the declared
//!   budget and never calls a provider.
//!
//! The registry is a Cordis service (`shenko/context-contribution`), so any
//! plugin can register a provider on the same root and remove it again on
//! cleanup.
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shenko_kernel::contribution::{ContributionConflict, merge_contributions, same_contribution};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

// One canonical definition of the contribution contract; re-exported so a
// consumer needs one import path.
pub use shenko_types::plugin_runtime::{
    ContextContribution, ContributionKind, ContributionLifetime, ContributionOmission,
    ContributionScope, ContributionSlot, ContributionSource, ContributionUsage,
};

/// The material one request may use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPlan {
    pub contributions: Vec<ContextContribution>,
    pub total_tokens: u64,
    pub omitted: Vec<ContributionOmission>,
    /// Keys of required contributions that could not be included. The caller
    /// decides what an unsatisfied requirement means; the collector never
    /// drops one silently.
    pub missing_required: Vec<String>,
}

impl ContributionPlan {
    pub fn is_empty(&self) -> bool {
        self.contributions.is_empty()
    }

    /// The usage evidence of one request, built from this plan.
    ///
    /// `sent` holds each complete prepared identity adopted by the request,
    /// including scope and input revision. `omitted` carries the
    /// reason for every prepared contribution that did not travel.
    /// `total_tokens` is the cost of the contributions alone, so the caller
    /// charges it on top of the projected payload.
    pub fn usage(
        &self,
        request_id: EntityId,
        request_revision: u64,
        input_revision: impl Into<String>,
        seal_digest: Digest,
    ) -> ContributionUsage {
        ContributionUsage {
            request_id,
            request_revision,
            input_revision: input_revision.into(),
            seal_digest,
            sent: self.contributions.clone(),
            omitted: self.omitted.clone(),
            total_tokens: self.total_tokens,
        }
    }
}

/// The document a prepared contribution's `data` reference points at.
///
/// The model sees the bounded text; the provenance — producer, version and
/// artifact digest — travels in the seal and the raw call record, never in
/// front of the model. The same document shape is used by the durable inbox
/// events, so a consumer reads one convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionPayload {
    /// Document version. A reader refuses a newer one instead of guessing.
    pub v: u32,
    /// The bounded text the model receives.
    pub text: String,
}

impl ContributionPayload {
    pub const VERSION: u32 = 1;

    /// The document bytes of a prepared text.
    pub fn seal(text: impl Into<String>) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&Self {
            v: Self::VERSION,
            text: text.into(),
        })
    }

    /// The text of a sealed document.
    pub fn read(bytes: &[u8]) -> Result<String, String> {
        let payload: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if payload.v > Self::VERSION {
            return Err(format!(
                "contribution payload version {} is newer than the {} this reader understands",
                payload.v,
                Self::VERSION
            ));
        }
        Ok(payload.text)
    }
}

/// The record of one contribution that was prepared but not sent.
pub fn omission_record(
    contribution: &ContextContribution,
    reason: &str,
    detail: impl Into<String>,
) -> ContributionOmission {
    ContributionOmission {
        key: contribution.key.clone(),
        slot: contribution.slot,
        source: contribution.source.clone(),
        reason: format!("{reason}: {}", detail.into()),
    }
}

/// A scope that is not bound to one task.
///
/// Scope-wide material — a reminder that belongs to the branch rather than
/// to one task — is prepared and collected under this scope. Task-bound
/// material carries the task id on both the prepared and the collected side.
pub fn scope_wide(scope_id: EntityId) -> ContributionScope {
    ContributionScope {
        scope_id,
        task_id: None,
    }
}

/// A failure while preparing or merging contributions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum ContributionError {
    #[error(
        "CONTRIBUTION_CONFLICT: slot {slot:?} key {key} was contributed differently by {producers:?}"
    )]
    Conflict {
        slot: ContributionSlot,
        key: String,
        producers: Vec<EntityId>,
    },
    #[error("contribution {key} does not apply to this scope: {detail}")]
    ScopeMismatch { key: String, detail: String },
    #[error("contribution provider {provider} failed: {reason}")]
    Provider { provider: String, reason: String },
    #[error("contribution registration {registration} is no longer live")]
    RegistrationGone { registration: EntityId },
    #[error("contribution preparation for registration {registration} has been superseded")]
    StalePublication { registration: EntityId },
    #[error("contribution {key} does not belong to registration {registration}")]
    SourceMismatch { registration: EntityId, key: String },
    #[error("contribution usage for request {request_id} conflicts with its recorded seal")]
    UsageConflict { request_id: EntityId },
    #[error("contribution budget exceeded: {used} tokens > {budget} tokens")]
    BudgetExceeded { used: u64, budget: u64 },
}

impl From<ContributionConflict> for ContributionError {
    fn from(conflict: ContributionConflict) -> Self {
        Self::Conflict {
            slot: conflict.slot,
            key: conflict.key,
            producers: conflict.producers,
        }
    }
}

/// A producer of prepared contributions.
///
/// `prepare` is the slow path and must run off the request path. An asynchronous
/// producer registers a publisher and obtains a publication ticket BEFORE
/// starting work; a late completion cannot publish after replacement or disposal.
pub trait ContributionProvider: Send + Sync + 'static {
    /// The producer identity carried by every contribution it prepares.
    fn manifest(&self) -> ContributionSource;
    /// Prepare contributions for one scope.
    fn prepare(
        &self,
        scope: &ContributionScope,
    ) -> Result<Vec<ContextContribution>, ContributionError>;
}

/// One preparation, bound to a live registration and its scope. Tickets cannot
/// be cloned or constructed by a caller. Starting newer work supersedes an
/// older ticket without withdrawing the last successfully published batch.
#[derive(Debug)]
pub struct ContributionPublication {
    registration: EntityId,
    scope: ContributionScope,
    generation: EntityId,
}

struct PreparedPublication {
    scope: ContributionScope,
    generation: EntityId,
    contributions: Vec<ContextContribution>,
}

struct ContributionRegistration {
    id: EntityId,
    source: ContributionSource,
    provider: Option<Arc<dyn ContributionProvider>>,
    prepared: Vec<PreparedPublication>,
}

#[derive(Default)]
struct ContributionState {
    providers: Vec<ContributionRegistration>,
    /// Append-only positions also index consumed identities without copying
    /// their strings, sources and object references into a second collection.
    usages: Vec<ContributionUsage>,
    usage_positions: HashMap<EntityId, usize>,
    consumed: HashMap<Digest, Vec<(usize, usize)>>,
}

impl ContributionState {
    fn is_consumed(&self, item: &ContextContribution) -> bool {
        item.single_use()
            && self
                .consumed
                .get(&item.data.digest)
                .is_some_and(|positions| {
                    positions.iter().any(|&(usage, sent)| {
                        same_contribution(item, &self.usages[usage].sent[sent])
                    })
                })
    }
}

fn begin_publication(
    provider: &mut ContributionRegistration,
    scope: &ContributionScope,
) -> ContributionPublication {
    let generation = EntityId::new();
    match provider
        .prepared
        .iter_mut()
        .find(|prepared| &prepared.scope == scope)
    {
        Some(prepared) => prepared.generation = generation,
        None => provider.prepared.push(PreparedPublication {
            scope: scope.clone(),
            generation,
            contributions: Vec::new(),
        }),
    }
    ContributionPublication {
        registration: provider.id,
        scope: scope.clone(),
        generation,
    }
}

fn publication_position(
    state: &ContributionState,
    publication: &ContributionPublication,
) -> Result<(usize, usize), ContributionError> {
    let provider = state
        .providers
        .iter()
        .position(|provider| provider.id == publication.registration)
        .ok_or(ContributionError::RegistrationGone {
            registration: publication.registration,
        })?;
    let prepared = state.providers[provider]
        .prepared
        .iter()
        .position(|prepared| {
            prepared.scope == publication.scope && prepared.generation == publication.generation
        })
        .ok_or(ContributionError::StalePublication {
            registration: publication.registration,
        })?;
    Ok((provider, prepared))
}

/// The installed producers, their prepared material and the usage evidence.
///
/// The registry holds no business content: it merges what producers prepared
/// and records what was sent.
#[derive(Default)]
pub struct ContributionRegistry {
    state: Mutex<ContributionState>,
}

impl Service for ContributionRegistry {
    const NAME: &'static str = "shenko/context-contribution";
}

impl ContributionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a producer. The returned handle identifies this exact
    /// registration, so a late cleanup cannot remove a later replacement.
    pub fn register(&self, provider: Arc<dyn ContributionProvider>) -> EntityId {
        self.register_source(provider.manifest(), Some(provider))
    }

    /// Register an asynchronous publisher. Its identity is fixed for this
    /// lifetime; a new implementation/source needs a new registration.
    pub fn register_publisher(&self, source: ContributionSource) -> EntityId {
        self.register_source(source, None)
    }

    fn register_source(
        &self,
        source: ContributionSource,
        provider: Option<Arc<dyn ContributionProvider>>,
    ) -> EntityId {
        let mut state = self.state.lock();
        let id = EntityId::new();
        state.providers.push(ContributionRegistration {
            id,
            source,
            provider,
            prepared: Vec::new(),
        });
        id
    }

    /// Remove exactly that registration, its prepared material and every
    /// outstanding publication ticket. Consumption evidence outlives it.
    pub fn unregister(&self, id: EntityId) -> bool {
        let mut state = self.state.lock();
        let before = state.providers.len();
        state.providers.retain(|provider| provider.id != id);
        state.providers.len() != before
    }

    pub fn providers(&self) -> Vec<ContributionSource> {
        self.state
            .lock()
            .providers
            .iter()
            .map(|provider| provider.source.clone())
            .collect()
    }

    /// Acquire a ticket before preparing work, not after it completes.
    pub fn begin_publication(
        &self,
        registration: EntityId,
        scope: &ContributionScope,
    ) -> Result<ContributionPublication, ContributionError> {
        let mut state = self.state.lock();
        let provider = state
            .providers
            .iter_mut()
            .find(|provider| provider.id == registration)
            .ok_or(ContributionError::RegistrationGone { registration })?;
        Ok(begin_publication(provider, scope))
    }

    /// Replace only this registration's batch for this scope. Empty material
    /// withdraws that batch; other registrations and scopes are untouched.
    pub fn publish(
        &self,
        publication: ContributionPublication,
        contributions: Vec<ContextContribution>,
    ) -> Result<(), ContributionError> {
        self.publish_batch(&mut [(publication, contributions)])
    }

    fn publish_batch(
        &self,
        batches: &mut [(ContributionPublication, Vec<ContextContribution>)],
    ) -> Result<(), ContributionError> {
        let mut state = self.state.lock();
        // Verify the whole refresh before replacing anything. No callback is
        // invoked while holding this lock.
        for (publication, contributions) in batches.iter() {
            let (provider, _) = publication_position(&state, publication)?;
            for contribution in contributions {
                if contribution.source != state.providers[provider].source {
                    return Err(ContributionError::SourceMismatch {
                        registration: publication.registration,
                        key: contribution.key.clone(),
                    });
                }
                if contribution.scope != publication.scope {
                    return Err(ContributionError::ScopeMismatch {
                        key: contribution.key.clone(),
                        detail: "prepared scope differs from publication ticket".into(),
                    });
                }
            }
        }
        for (publication, contributions) in batches.iter_mut() {
            contributions.retain(|item| !state.is_consumed(item));
            let (provider, prepared) = publication_position(&state, publication)?;
            state.providers[provider].prepared[prepared].contributions =
                std::mem::take(contributions);
        }
        Ok(())
    }

    /// The slow path: ask every registered producer to prepare material for
    /// this scope and file the result. It must not run inside a request.
    pub fn refresh(
        &self,
        scope: &ContributionScope,
        now_ms: i64,
    ) -> Result<ContributionPlan, ContributionError> {
        let providers: Vec<_> = self
            .state
            .lock()
            .providers
            .iter_mut()
            .filter_map(|registration| {
                let provider = registration.provider.clone()?;
                Some((begin_publication(registration, scope), provider))
            })
            .collect();
        let mut prepared = Vec::with_capacity(providers.len());
        for (publication, provider) in providers {
            match provider.prepare(scope) {
                Ok(contributions) => prepared.push((publication, contributions)),
                Err(error) => {
                    return Err(ContributionError::Provider {
                        provider: provider.manifest().plugin_id.to_string(),
                        reason: error.to_string(),
                    });
                }
            }
        }
        self.publish_batch(&mut prepared)?;
        Ok(self.collect(scope, now_ms))
    }

    /// The request path: merge the prepared material for this scope.
    ///
    /// Contributions with the same slot and key are merged by the kernel's
    /// one rule: identical material collapses, differing material is a
    /// conflict and is reported instead of letting the later producer win.
    /// Expired contributions are omitted with their reason. A contribution
    /// that occupies more tokens than it declared is omitted, and a required
    /// contribution that could not be included is named in
    /// [`ContributionPlan::missing_required`].
    pub fn collect(&self, scope: &ContributionScope, now_ms: i64) -> ContributionPlan {
        self.collect_for(scope, None, now_ms)
    }

    /// Like [`ContributionRegistry::collect`], but restricted to contributions
    /// prepared against `input_revision`. A contribution bound to another
    /// revision is omitted with a scope mismatch.
    ///
    /// The scope match is exact: a contribution prepared for a task is only
    /// eligible when the request asks for that same task. A request that
    /// asks with `task_id: None` therefore receives only scope-wide
    /// material — a producer that prepares task-bound material must be
    /// collected with the matching task id, or it is reported as a scope
    /// mismatch rather than sent to the wrong task.
    pub fn collect_for(
        &self,
        scope: &ContributionScope,
        input_revision: Option<&str>,
        now_ms: i64,
    ) -> ContributionPlan {
        let prepared: Vec<ContextContribution> = {
            let state = self.state.lock();
            state
                .providers
                .iter()
                .flat_map(|provider| &provider.prepared)
                .filter(|prepared| prepared.scope.scope_id == scope.scope_id)
                .flat_map(|prepared| prepared.contributions.iter().cloned())
                .collect()
        };
        let mut plan = ContributionPlan::default();
        let mut positions: HashMap<ContributionSlot, HashMap<String, usize>> = HashMap::new();
        let mut groups: Vec<(ContextContribution, bool)> = Vec::new();
        let mut required_keys: Vec<(ContributionSlot, String)> = Vec::new();
        for contribution in prepared {
            if contribution.scope.task_id != scope.task_id {
                plan.omitted.push(omission(
                    &contribution,
                    "scope_mismatch",
                    format!(
                        "prepared for task {:?}, requested for task {:?}",
                        contribution.scope.task_id, scope.task_id
                    ),
                ));
                continue;
            }
            if let Some(revision) = input_revision {
                if contribution.input_revision != revision {
                    plan.omitted.push(omission(
                        &contribution,
                        "scope_mismatch",
                        format!(
                            "prepared against input revision {}, requested {revision}",
                            contribution.input_revision
                        ),
                    ));
                    continue;
                }
            }
            if contribution.required
                && !required_keys
                    .iter()
                    .any(|(slot, key)| *slot == contribution.slot && key == &contribution.key)
            {
                required_keys.push((contribution.slot, contribution.key.clone()));
            }
            if contribution.is_expired(now_ms) {
                plan.omitted.push(omission(
                    &contribution,
                    "expired",
                    format!(
                        "lifetime {:?} ended before collection",
                        contribution.lifetime
                    ),
                ));
                continue;
            }
            // Keep the group's conflict state even after its first pair is
            // rejected. A third producer must not resurrect the same key.
            let slot_positions = positions.entry(contribution.slot).or_default();
            if let Some(&index) = slot_positions.get(&contribution.key) {
                let (existing, conflicted) = &mut groups[index];
                if !*conflicted && same_contribution(existing, &contribution) {
                    continue;
                }
                if !*conflicted {
                    plan.omitted.push(omission(
                        existing,
                        shenko_kernel::contribution::CONTRIBUTION_CONFLICT,
                        "different prepared material shares this slot/key".into(),
                    ));
                    *conflicted = true;
                }
                plan.omitted.push(omission(
                    &contribution,
                    shenko_kernel::contribution::CONTRIBUTION_CONFLICT,
                    "different prepared material shares this slot/key".into(),
                ));
            } else {
                slot_positions.insert(contribution.key.clone(), groups.len());
                groups.push((contribution, false));
            }
        }
        plan.contributions = groups
            .into_iter()
            .filter_map(|(contribution, conflicted)| (!conflicted).then_some(contribution))
            .collect();
        // Slot order is the assembly order; within a slot the producer order
        // is stable, so the same inputs always produce the same plan. The
        // order is the standard profile's, declared once in the shared
        // protocol types — not restated here.
        plan.contributions.sort_by(|left, right| {
            slot_rank(left.slot)
                .cmp(&slot_rank(right.slot))
                .then_with(|| left.source.plugin_id.cmp(&right.source.plugin_id))
                .then_with(|| left.key.cmp(&right.key))
        });
        let mut used = 0u64;
        let mut kept = Vec::with_capacity(plan.contributions.len());
        for contribution in plan.contributions.drain(..) {
            if contribution.tokens > contribution.budget_tokens {
                plan.omitted.push(omission(
                    &contribution,
                    "budget_exceeded",
                    format!(
                        "occupies {} tokens but declared a budget of {}",
                        contribution.tokens, contribution.budget_tokens
                    ),
                ));
                continue;
            }
            let Some(next_used) = used.checked_add(contribution.tokens) else {
                plan.omitted.push(omission(
                    &contribution,
                    "budget_exceeded",
                    "the aggregate contribution token count exceeds u64".into(),
                ));
                continue;
            };
            used = next_used;
            kept.push(contribution);
        }
        plan.contributions = kept;
        plan.total_tokens = used;
        for (slot, key) in required_keys {
            if !plan
                .contributions
                .iter()
                .any(|contribution| contribution.slot == slot && contribution.key == key)
                && !plan.missing_required.contains(&key)
            {
                plan.missing_required.push(key);
            }
        }
        plan
    }

    /// Merge contributions for one (slot, key) with the kernel's single
    /// rule, so the hook graph's `collect` synthesis and this registry can
    /// never disagree about what a conflict is.
    pub fn merge(
        &self,
        slot: ContributionSlot,
        key: &str,
        contributions: Vec<ContextContribution>,
    ) -> Result<ContextContribution, ContributionError> {
        merge_contributions(slot, key, contributions).map_err(ContributionError::from)
    }

    /// Project an accepted S0. This is reservation/consumption evidence, not
    /// proof that a provider received the request. Replay is immutable and
    /// restores consumption even before a producer republishes its material.
    pub fn record_usage(&self, usage: ContributionUsage) -> Result<(), ContributionError> {
        let mut state = self.state.lock();
        if let Some(&position) = state.usage_positions.get(&usage.request_id) {
            return if state.usages[position] == usage {
                Ok(())
            } else {
                Err(ContributionError::UsageConflict {
                    request_id: usage.request_id,
                })
            };
        }
        for provider in &mut state.providers {
            for prepared in &mut provider.prepared {
                prepared.contributions.retain(|contribution| {
                    !(contribution.single_use()
                        && usage
                            .sent
                            .iter()
                            .any(|used| same_contribution(contribution, used)))
                });
            }
        }
        let position = state.usages.len();
        state.usage_positions.insert(usage.request_id, position);
        state.usages.push(usage);
        for sent in 0..state.usages[position].sent.len() {
            let contribution = &state.usages[position].sent[sent];
            if contribution.single_use() && !state.is_consumed(contribution) {
                let digest = contribution.data.digest.clone();
                state
                    .consumed
                    .entry(digest)
                    .or_default()
                    .push((position, sent));
            }
        }
        Ok(())
    }

    /// The usage evidence of one request.
    pub fn usage(&self, request_id: &EntityId) -> Option<ContributionUsage> {
        let state = self.state.lock();
        state
            .usage_positions
            .get(request_id)
            .map(|&position| state.usages[position].clone())
    }

    /// Every recorded usage, newest first.
    pub fn usages(&self) -> Vec<ContributionUsage> {
        self.state.lock().usages.iter().rev().cloned().collect()
    }
}

fn omission(
    contribution: &ContextContribution,
    reason: &str,
    detail: String,
) -> ContributionOmission {
    omission_record(contribution, reason, detail)
}

/// The render position of a slot in the standard profile.
///
/// The order is declared once, in the shared protocol types; an unknown slot
/// sorts last rather than silently taking a place.
fn slot_rank(slot: ContributionSlot) -> usize {
    ContributionSlot::ALL
        .iter()
        .position(|known| *known == slot)
        .unwrap_or(usize::MAX)
}

/// Publishes a caller-owned registry so plugins can find it by name.
pub struct ContributionHost;

impl Plugin for ContributionHost {
    type Config = Arc<ContributionRegistry>;
    type Input = Arc<ContributionRegistry>;
    type PrepareError = std::convert::Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("context-contribution-host")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(input.clone()).map_err(std::io::Error::other)?;
        Ok(())
    }
}
