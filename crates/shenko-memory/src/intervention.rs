//! The pluggable memory-intervention policy and its bounded worker.
//! SPEC §14.4 (主动记忆 worker), §14.2 (四种状态不同处理).
//!
//! The memory worker is a *bounded* task, not a second subject: it observes a
//! limited trajectory and the structured bank of one research lineage, and it
//! returns exactly one decision — stay silent, or one reminder that carries
//! its provenance, its relevance and the scope it applies to. It has no
//! goal loop of its own, it cannot take over the subject's plan, and its
//! inference goes through the same broker and the same durable request
//! sealing as every other model call.
//!
//! The split between policy and runtime is deliberate:
//!
//! * the **policy** decides *what* to ask and *how* to read the answer
//!   ([`MemoryInterventionPolicy::plan`] / [`MemoryInterventionPolicy::interpret`]);
//! * the **runtime** owns the durable send: it seals the request, admits it
//!   against the owner's quota, submits it to the broker and records the
//!   receipt. A policy never talks to a provider itself, so replacing the
//!   policy cannot weaken the request sealing.
//!
//! Two properties are deliberate as well:
//!
//! * **No personal-interest filter.** The policy decides whether a fact
//!   matters for the work that is running now. It never has to argue that
//!   something is personally interesting to the user, and no human approval
//!   gates a reminder: the worker decides and the decision is recorded.
//! * **Pluggable, not fixed.** [`MemoryInterventionPolicy`] is the contract
//!   the runtime publishes; the shipped model-backed policy is one
//!   implementation of it and can be replaced at runtime through
//!   [`MemoryInterventionRegistry`].
//!
//! The reminder itself leaves this module as a durable decision in the
//! store and as one context contribution for the request path.

use std::sync::Arc;

use cordis_core::Service;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shenko_context_policy::contribution::{
    ContributionError, ContributionKind, ContributionLifetime, ContributionProvider,
    ContributionScope, ContributionSlot, ContributionSource, ContextContribution,
};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

/// The role contract of the memory-intervention worker. It is authored text
/// that travels with the request; it is not a dispatch table and it contains
/// no keyword rules.
pub const INTERVENTION_ROLE: &str = "\
你是记忆干预 worker：一次有界的判断任务，不是持续主体。
你观察一段有限轨迹和一份结构化记忆库，判断此刻是否值得提醒正在工作的执行者。
你没有任何工具、文件、shell、网络或发信入口，也不能接管主体的计划；你只输出一个决定。
决定只有两种：
1) 保持沉默：现在没有值得打断工作的事实。
2) 给出一条提醒：内容、出处（必须引用观察到的记忆条目 id 或来源）、相关性、作用范围、有效期秒数。
相关性指这条提醒与当前正在推进的工作/研究线的关系；不需要论证它对用户个人兴趣是否重要，也不需要请求任何批准。
如果没有把握，保持沉默比编造提醒更好。
只输出一个 JSON 对象，不要解释，不要 markdown 代码块之外的内容：\
{\"decision\":\"silent\",\"reason\":\"...\"} 或 \
{\"decision\":\"remind\",\"content\":\"...\",\"provenance\":[\"...\"],\"relevance\":\"...\",\"scope\":\"...\",\"expires_in_secs\":600}";

/// The observation a policy decides on. It is bounded on purpose: the worker
/// sees a limited trajectory and a limited bank, never the whole history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionObservation {
    pub epoch_id: EntityId,
    pub lineage_id: EntityId,
    pub now_ms: i64,
    pub budget: InterventionBudget,
    pub trajectory: Vec<TrajectoryItem>,
    pub bank: Vec<BankSnapshot>,
}

/// What the worker is allowed to look at and to spend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionBudget {
    pub max_tokens: u64,
    pub max_trajectory_items: usize,
    pub max_bank_entries: usize,
}

impl Default for InterventionBudget {
    fn default() -> Self {
        Self { max_tokens: 1024, max_trajectory_items: 16, max_bank_entries: 32 }
    }
}

/// One observed step of the trajectory the worker reasons about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryItem {
    /// `step`, `tool`, `error`, `effect`, `message`, …
    pub kind: String,
    pub summary: String,
    pub at_ms: i64,
}

/// One entry of the structured bank as the worker sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BankSnapshot {
    pub entry_id: EntityId,
    /// `requirement`, `environment_fact`, `attempt`, `diagnosis`, `open_goal`.
    pub kind: String,
    pub content: String,
    pub source_refs: Vec<String>,
    pub scope: String,
    pub confidence: Option<f64>,
    pub age_ms: i64,
}

/// The decision of one bounded intervention call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum InterventionDecision {
    /// Nothing worth saying right now. The reason is recorded so a missed
    /// reminder stays an evaluable fact.
    Silent { reason: String },
    /// One reminder with provenance, relevance and scope.
    Remind {
        content: String,
        provenance: Vec<String>,
        relevance: String,
        scope: String,
        expires_in_secs: Option<u64>,
    },
}

impl InterventionDecision {
    pub fn is_silent(&self) -> bool {
        matches!(self, Self::Silent { .. })
    }
}

/// Why an intervention call failed.
#[derive(Debug, thiserror::Error)]
pub enum InterventionError {
    #[error("intervention transport failed: {0}")]
    Transport(String),
    #[error("intervention decision is invalid: {0}")]
    InvalidDecision(String),
    #[error("intervention budget exceeded: {0}")]
    Budget(String),
    #[error("intervention store failed: {0}")]
    Store(String),
}

/// One bounded generation the runtime performs on the policy's behalf.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionCall {
    /// `None` keeps the broker's bound model.
    pub model: Option<String>,
    pub system_prompt: String,
    pub user_prompt: String,
    pub max_tokens: u64,
}

/// What the policy wants the runtime to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionPlan {
    /// `None` when the policy already decided and no inference is needed.
    pub call: Option<InterventionCall>,
    /// The decision the policy takes without an inference.
    pub decision: Option<InterventionDecision>,
}

/// The answer of one bounded generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionModelReply {
    pub text: String,
    pub tokens: u64,
}

/// The pluggable intervention policy (SPEC §14.4).
///
/// A policy is asked for a plan and reads the answer. It never performs the
/// call itself: the runtime seals the request, admits it against the owner's
/// quota and records the receipt, so a replaced policy cannot weaken the
/// durable request contract.
pub trait MemoryInterventionPolicy: Send + Sync + 'static {
    /// The stable name of this policy. It is recorded with every decision.
    fn name(&self) -> &str;
    /// The revision of this policy's decision contract.
    fn revision(&self) -> &str;
    /// What the runtime should do for this observation.
    fn plan(
        &self,
        observation: &InterventionObservation,
    ) -> Result<InterventionPlan, InterventionError>;
    /// How the answer is read as one decision.
    fn interpret(
        &self,
        observation: &InterventionObservation,
        reply: &InterventionModelReply,
    ) -> Result<InterventionDecision, InterventionError>;
}

/// The shipped policy: the model itself decides, over a bounded observation.
///
/// The policy does not contain any keyword rule. It asks the bounded worker
/// for one decision and validates the answer structurally: a reminder without
/// provenance, relevance or scope is refused and recorded as a failed
/// decision rather than being passed on.
pub struct ModelMemoryInterventionPolicy;

impl ModelMemoryInterventionPolicy {
    /// The prompt the worker receives. The observation is the whole context:
    /// a bounded trajectory and a bounded bank.
    pub fn build_prompt(
        observation: &InterventionObservation,
    ) -> Result<(String, String), InterventionError> {
        let trajectory: Vec<&TrajectoryItem> = observation
            .trajectory
            .iter()
            .take(observation.budget.max_trajectory_items)
            .collect();
        let bank: Vec<&BankSnapshot> = observation
            .bank
            .iter()
            .take(observation.budget.max_bank_entries)
            .collect();
        let user = serde_json::json!({
            "epoch_id": observation.epoch_id.to_string(),
            "lineage_id": observation.lineage_id.to_string(),
            "now_ms": observation.now_ms,
            "trajectory": trajectory,
            "bank": bank,
        });
        let user_prompt = serde_json::to_string_pretty(&user)
            .map_err(|error| InterventionError::InvalidDecision(error.to_string()))?;
        Ok((INTERVENTION_ROLE.to_string(), user_prompt))
    }

    /// Parse the worker's answer into one decision. The answer must be a
    /// single JSON object.
    pub fn parse_decision(
        text: &str,
        observation: &InterventionObservation,
    ) -> Result<InterventionDecision, InterventionError> {
        let trimmed = text.trim();
        if trimmed.len() > 64 * 1024 {
            return Err(InterventionError::InvalidDecision(
                "the decision text exceeds the bounded size".to_string(),
            ));
        }
        let start = trimmed.find('{').ok_or_else(|| {
            InterventionError::InvalidDecision("the answer contains no JSON object".to_string())
        })?;
        let end = trimmed.rfind('}').ok_or_else(|| {
            InterventionError::InvalidDecision("the answer contains no JSON object".to_string())
        })?;
        if end < start {
            return Err(InterventionError::InvalidDecision(
                "the answer contains no complete JSON object".to_string(),
            ));
        }
        let candidate = &trimmed[start..=end];
        let raw: serde_json::Value = serde_json::from_str(candidate).map_err(|error| {
            InterventionError::InvalidDecision(format!("the answer is not one JSON value: {error}"))
        })?;
        let object = raw.as_object().ok_or_else(|| {
            InterventionError::InvalidDecision("the answer is not a JSON object".to_string())
        })?;
        match object.get("decision").and_then(|value| value.as_str()) {
            Some("silent") => {
                let reason = object
                    .get("reason")
                    .and_then(|value| value.as_str())
                    .unwrap_or("the policy stayed silent")
                    .trim();
                if reason.is_empty() {
                    return Err(InterventionError::InvalidDecision(
                        "a silent decision needs a reason".to_string(),
                    ));
                }
                Ok(InterventionDecision::Silent { reason: reason.to_string() })
            }
            Some("remind") => {
                let content = object
                    .get("content")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .trim();
                if content.is_empty() || content.len() > 8192 {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder needs a non-empty content of bounded size".to_string(),
                    ));
                }
                let provenance: Vec<String> = object
                    .get("provenance")
                    .and_then(|value| value.as_array())
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| item.as_str())
                            .map(|item| item.trim().to_string())
                            .filter(|item| !item.is_empty())
                            .collect()
                    })
                    .unwrap_or_default();
                if provenance.is_empty() {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder needs at least one provenance reference".to_string(),
                    ));
                }
                let known: Vec<String> = observation
                    .bank
                    .iter()
                    .map(|entry| entry.entry_id.to_string())
                    .collect();
                if !known.is_empty()
                    && !provenance
                        .iter()
                        .any(|item| known.iter().any(|entry| entry == item))
                {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder must reference an observed bank entry".to_string(),
                    ));
                }
                let relevance = object
                    .get("relevance")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .trim();
                if relevance.is_empty() {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder needs a relevance statement".to_string(),
                    ));
                }
                let scope = object
                    .get("scope")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .trim();
                if scope.is_empty() {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder needs a scope".to_string(),
                    ));
                }
                let expires_in_secs = object.get("expires_in_secs").and_then(|value| value.as_u64());
                if expires_in_secs == Some(0) {
                    return Err(InterventionError::InvalidDecision(
                        "a reminder needs a positive expiry or none".to_string(),
                    ));
                }
                Ok(InterventionDecision::Remind {
                    content: content.to_string(),
                    provenance,
                    relevance: relevance.to_string(),
                    scope: scope.to_string(),
                    expires_in_secs,
                })
            }
            Some(other) => Err(InterventionError::InvalidDecision(format!(
                "unknown decision: {other}"
            ))),
            None => Err(InterventionError::InvalidDecision(
                "the answer has no decision field".to_string(),
            )),
        }
    }
}

impl MemoryInterventionPolicy for ModelMemoryInterventionPolicy {
    fn name(&self) -> &str {
        "model.memory-intervention"
    }

    fn revision(&self) -> &str {
        "v1"
    }

    fn plan(
        &self,
        observation: &InterventionObservation,
    ) -> Result<InterventionPlan, InterventionError> {
        let (system_prompt, user_prompt) = Self::build_prompt(observation)?;
        Ok(InterventionPlan {
            call: Some(InterventionCall {
                model: None,
                system_prompt,
                user_prompt,
                max_tokens: observation.budget.max_tokens,
            }),
            decision: None,
        })
    }

    fn interpret(
        &self,
        observation: &InterventionObservation,
        reply: &InterventionModelReply,
    ) -> Result<InterventionDecision, InterventionError> {
        Self::parse_decision(&reply.text, observation)
    }
}

/// The baseline policy: it never interrupts and it never calls a model. It
/// exists so the cost of a reminder can be measured against a run that
/// carries none, and so a deployment without a usable model still has an
/// honest, recorded decision instead of an unhandled error.
pub struct SilentMemoryInterventionPolicy;

impl MemoryInterventionPolicy for SilentMemoryInterventionPolicy {
    fn name(&self) -> &str {
        "silent.memory-intervention"
    }

    fn revision(&self) -> &str {
        "v1"
    }

    fn plan(
        &self,
        _observation: &InterventionObservation,
    ) -> Result<InterventionPlan, InterventionError> {
        Ok(InterventionPlan {
            call: None,
            decision: Some(InterventionDecision::Silent {
                reason: "the silent baseline policy is installed".to_string(),
            }),
        })
    }

    fn interpret(
        &self,
        _observation: &InterventionObservation,
        _reply: &InterventionModelReply,
    ) -> Result<InterventionDecision, InterventionError> {
        Err(InterventionError::InvalidDecision(
            "the silent baseline policy never calls a model".to_string(),
        ))
    }
}

struct PolicyState {
    revision: u64,
    policy: Arc<dyn MemoryInterventionPolicy>,
}

/// The installed intervention policy, published as one Cordis service. The
/// policy can be replaced while the runtime lives; every decision records
/// the name and the revision of the policy that produced it.
pub struct MemoryInterventionRegistry {
    state: RwLock<PolicyState>,
}

impl Service for MemoryInterventionRegistry {
    const NAME: &'static str = "shenko/memory-intervention";
}

impl MemoryInterventionRegistry {
    pub fn new(policy: Arc<dyn MemoryInterventionPolicy>) -> Self {
        Self { state: RwLock::new(PolicyState { revision: 1, policy }) }
    }

    /// The revision of the installed policy. It changes with every install.
    pub fn revision(&self) -> u64 {
        self.state.read().revision
    }

    /// The installed policy.
    pub fn current(&self) -> Arc<dyn MemoryInterventionPolicy> {
        Arc::clone(&self.state.read().policy)
    }

    /// Replace the installed policy and return the new revision.
    pub fn install(&self, policy: Arc<dyn MemoryInterventionPolicy>) -> u64 {
        let mut state = self.state.write();
        state.revision += 1;
        state.policy = policy;
        state.revision
    }
}

/// The identity of the plugin that produces the reminders.
pub fn pma_plugin_id() -> EntityId {
    "b0f3d5a4-6c2e-4a17-9f0d-3f5c1c2a7e41"
        .parse()
        .expect("fixed pma plugin UUID")
}

/// The scope of a reminder that is meant for the subject itself.
pub const SUBJECT_SCOPE: &str = "subject";

/// The scope prefix of a reminder that belongs to one research epoch.
pub fn epoch_scope(epoch_id: EntityId) -> String {
    format!("epoch:{epoch_id}")
}

/// The runtime seam that publishes a prepared reminder into the request
/// path. The implementation owns the context-contribution registry; the
/// memory domain only knows this trait.
pub trait ReminderPublisher: Send + Sync + 'static {
    fn publish(
        &self,
        scope: &ContributionScope,
        contributions: Vec<ContextContribution>,
    ) -> Result<(), ContributionError>;
}

/// The provider that turns the durable reminder ledger into context
/// contributions for the request path. It is the only producer of
/// `TransientReminder` contributions in the memory domain.
pub struct PmaContributionProvider {
    store: Arc<Store>,
    plugin_version: String,
}

impl PmaContributionProvider {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store, plugin_version: "pma.intervention/v1".to_string() }
    }

    /// The reminders that may still be shown for a scope: not expired, not
    /// already spent (a transient reminder leaves the visible context after
    /// the request it was prepared for) and directed at this scope.
    pub fn pending_reminders(
        &self,
        scope_id: EntityId,
        now_ms: i64,
    ) -> Result<Vec<shenko_store::InterventionReminder>, InterventionError> {
        let mut reminders = self
            .store
            .pending_interventions_by_scope(SUBJECT_SCOPE, now_ms)
            .map_err(|error| InterventionError::Store(error.to_string()))?;
        let epoch_scope = epoch_scope(scope_id);
        reminders.extend(
            self.store
                .pending_interventions_by_scope(&epoch_scope, now_ms)
                .map_err(|error| InterventionError::Store(error.to_string()))?,
        );
        Ok(reminders)
    }

    /// The body of one reminder, sealed into the content store. The body is
    /// never inlined into a contribution.
    pub fn seal_body(
        &self,
        reminder: &shenko_store::InterventionReminder,
    ) -> Result<ObjectRef, InterventionError> {
        let body = serde_json::json!({
            "reminder_id": reminder.reminder_id.to_string(),
            "content": reminder.content,
            "provenance": reminder.provenance,
            "relevance": reminder.relevance,
            "scope": reminder.scope,
            "expires_at_ms": reminder.expires_at_ms,
            "policy": reminder.policy,
            "policy_revision": reminder.policy_revision,
            "bank_entry_ids": reminder
                .bank_entry_ids
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<String>>(),
            "created_at_ms": reminder.created_at_ms,
        });
        let bytes =
            serde_json::to_vec(&body).map_err(|error| InterventionError::Store(error.to_string()))?;
        match self.store.cas.seal_object(
            &bytes,
            "application/json",
            "memory-intervention",
            "pma-reminder-body",
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => Err(InterventionError::Store(error.to_string())),
        }
    }

    /// Build the contribution of one reminder for one scope.
    pub fn contribution_for(
        &self,
        reminder: &shenko_store::InterventionReminder,
        scope: &ContributionScope,
        now_ms: i64,
    ) -> Result<ContextContribution, InterventionError> {
        let Some(content) = reminder.content.clone() else {
            return Err(InterventionError::InvalidDecision(
                "a silent decision has no reminder body".to_string(),
            ));
        };
        let body = self.seal_body(reminder)?;
        let tokens = estimate_tokens(&content);
        Ok(ContextContribution {
            key: format!("pma.reminder.{}", reminder.reminder_id),
            slot: ContributionSlot::TransientReminder,
            source: self.manifest(),
            scope: scope.clone(),
            input_revision: format!("pma:{}:{}", reminder.epoch_id, reminder.reminder_id),
            lifetime: match reminder.expires_at_ms {
                Some(expires_at_ms) => ContributionLifetime::ExpiresAtMillis { expires_at_ms },
                None => ContributionLifetime::ThisRequest,
            },
            required: false,
            budget_tokens: tokens,
            tokens,
            data: body,
            prepared_at_ms: now_ms,
        })
    }
}

impl ContributionProvider for PmaContributionProvider {
    fn manifest(&self) -> ContributionSource {
        ContributionSource {
            plugin_id: pma_plugin_id(),
            plugin_version: self.plugin_version.clone(),
            artifact_digest: None,
            kind: ContributionKind::Reminder,
        }
    }

    fn prepare(
        &self,
        scope: &ContributionScope,
    ) -> Result<Vec<ContextContribution>, ContributionError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let reminders = self
            .pending_reminders(scope.scope_id, now_ms)
            .map_err(|error| ContributionError::Provider {
                provider: self.plugin_version.clone(),
                reason: error.to_string(),
            })?;
        let mut contributions = Vec::with_capacity(reminders.len());
        for reminder in reminders {
            let contribution = self
                .contribution_for(&reminder, scope, now_ms)
                .map_err(|error| ContributionError::Provider {
                    provider: self.plugin_version.clone(),
                    reason: error.to_string(),
                })?;
            contributions.push(contribution);
        }
        Ok(contributions)
    }
}

/// A conservative token estimate of a reminder body. It is a budgeting fact,
/// not a provider measurement.
pub fn estimate_tokens(text: &str) -> u64 {
    let mut units = 0_u64;
    for character in text.chars() {
        units += if character.is_ascii() { 1 } else { 2 };
    }
    units.div_ceil(4).max(1)
}

/// The digest of a sealed body, used as the artifact identity of a
/// contribution that carries sealed content.
pub fn body_digest(body: &ObjectRef) -> Digest {
    body.digest.clone()
}
