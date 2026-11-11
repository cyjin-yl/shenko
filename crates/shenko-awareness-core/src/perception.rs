//! Perception units and the live observations bound to them — docs/specs/cognition-units.md#spec-45-1,
//! docs/specs/cognition-units.md#spec-45-2, docs/specs/cognition-units.md#spec-45-4, docs/specs/cognition-units.md#spec-45-10-1.
//!
//! A perception unit is a definition: what can be observed, about which
//! target, and why the fact matters. It is not the fact. Installing the
//! definition, discovering it and reading its body therefore never produces an
//! observation — a fact only exists once a source on this node observed
//! something and named its own source, target and time.
//!
//! This module owns the runtime half of that distinction: which sources are
//! bound to which exact installed revision, what each source last observed,
//! and — when nothing is bound, when no source has observed yet, or when
//! every source failed — the honest `unknown`/`unavailable` that the model
//! must not read as a healthy fact.
//!
//! One binding may be answered by several independent sources. Each registers
//! under its own [`PerceptionSourceId`]; the registry refuses a second
//! registration under an id that is already live, so one source can never
//! shadow another, and the answers of distinct sources are merged by
//! freshness into the binding's single observation (see [`merge_facts`]).
//!
//! Nothing here decides which perception unit a message needs. The catalogue
//! discloses every installed card with its real observation state; matching a
//! symptom to a concept stays the model's job.

use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shenko_node::{Node, UnitKind, UnitRequest};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::cognition::truncate_utf8;

/// Maximum bytes of one observed statement.
pub const MAX_SUMMARY_BYTES: usize = 480;
/// Maximum bytes of one source or target name.
pub const MAX_NAME_BYTES: usize = 128;
/// Maximum bytes of one refusal reason.
pub const MAX_REASON_BYTES: usize = 240;
/// Maximum bytes of one rendered observation line.
pub const MAX_LINE_BYTES: usize = 720;

/// The shipped perception unit about this installation's own units.
pub const INSTALLATION_UNITS: &str = "perception.installation-units";
/// The source name that unit's runtime observer reports under.
pub const INSTALLATION_UNITS_SOURCE: &str = "node:installed_unit_facts";
/// The target that source observes.
pub const INSTALLATION_UNITS_TARGET: &str = "installation.local";

/// The exact installed unit version an observation source speaks for.
///
/// A source bound to one revision cannot answer for another: the revision and
/// the artifact digest are part of the binding, so an observation made about
/// an older version can never be read as a fact about the current one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionBinding {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: String,
}

/// Whose perception an observation was read through — docs/specs/cognition-units.md#spec-51-2.
///
/// Different subjects (a human, this Shenko, another Shenko) have different
/// perceptions. Reading one experience through someone else's perception is
/// perspective taking: the reading stops at that other perception, so it is
/// never this subject's own fact. A unit that does not say whose perception it
/// teaches stays undeclared rather than being repaired into an own view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PerceptionPerspective {
    /// This installation's own subject is the one that observed.
    Own,
    /// A modeled other subject. The bound reading is that subject's view.
    ModeledOther { subject: String },
}

impl PerceptionPerspective {
    /// The stable machine-readable name.
    pub fn state(&self) -> &'static str {
        match self {
            PerceptionPerspective::Own => "own",
            PerceptionPerspective::ModeledOther { .. } => "modeled_other",
        }
    }

    /// Whether two readings were taken through the same perspective.
    pub fn same(&self, other: &Self) -> bool {
        self == other
    }

    /// One bounded phrase for the disclosure.
    pub fn line(&self) -> String {
        match self {
            PerceptionPerspective::Own => "视角：本主体自己的观察".to_string(),
            PerceptionPerspective::ModeledOther { subject } => format!(
                "视角：模拟 {} 的感知，读起来是「推测对方会这样看」，不是本主体的观察",
                truncate_utf8(subject, MAX_NAME_BYTES).0
            ),
        }
    }
}

impl PerceptionBinding {
    /// The binding of one installed unit version.
    pub fn of(fact: &shenko_node::InstalledUnitFact) -> Self {
        Self {
            unit_id: fact.unit_id.clone(),
            revision: fact.revision.clone(),
            artifact_digest: fact.artifact_digest.0.clone(),
        }
    }
}

/// The identity of one observation source under one binding.
///
/// Several sources may answer for the same [`PerceptionBinding`]; the id
/// tells them apart. It is a registry-time label, not the `source` name an
/// observation reports: a source names itself in every `PerceptionFact`, and
/// this id only keeps two registrations from shadowing each other. The id is
/// bounded by [`MAX_NAME_BYTES`] and never empty.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PerceptionSourceId(String);

impl PerceptionSourceId {
    /// The source id the plain [`PerceptionRegistry::bind`] registers under.
    ///
    /// Binding a second observer without naming a source is therefore still a
    /// duplicate: distinct sources must name themselves through
    /// [`PerceptionRegistry::bind_source`].
    pub fn primary() -> Self {
        Self("primary".to_string())
    }

    /// The source id the shipped runtime sources register under.
    ///
    /// It is the same name the shipped observations report in
    /// `PerceptionFact::source`, so a consumer source that wants to answer
    /// alongside the shipped one registers under its own distinct id.
    pub fn builtin() -> Self {
        Self(INSTALLATION_UNITS_SOURCE.to_string())
    }

    /// One caller-chosen source id, when it is a non-empty bounded label.
    pub fn named(name: impl Into<String>) -> Result<Self> {
        let id = Self(name.into());
        ensure!(
            !id.0.trim().is_empty() && id.0.len() <= MAX_NAME_BYTES,
            "a perception source id is a non-empty name of at most {MAX_NAME_BYTES} bytes"
        );
        Ok(id)
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for PerceptionSourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One observation a source actually made.
///
/// The source fills every field. The registry never supplies a missing source,
/// target or time and never rewrites one: an observation that cannot name
/// where it came from is not an observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerceptionFact {
    /// The exact installed version this fact belongs to.
    pub binding: PerceptionBinding,
    /// Who observed, e.g. `node:installed_unit_facts`.
    pub source: String,
    /// What the observation is about, e.g. `installation.local`.
    ///
    /// When the observed thing is a provider or a live connection, the target
    /// names that thing's own binding identity (provider, instance and
    /// revision), not a display label: two connections that merely share the
    /// "QQ" channel label are different targets, and a fact about one must
    /// never be read as a fact about the other.
    pub target: String,
    /// When the source took the observation, RFC 3339.
    pub observed_at: String,
    /// When the source says the observation stops being current. `None` means
    /// the source declares no shelf life, which the projection reports as
    /// undeclared rather than as fresh.
    pub valid_until: Option<String>,
    /// The bounded statement of what was observed.
    pub summary: String,
    /// Digest of evidence bytes this installation holds, when the source
    /// sealed them.
    pub evidence_digest: Option<String>,
    /// Whose perception this observation was read through (docs/specs/cognition-units.md#spec-51-2).
    ///
    /// The source states it; the registry never labels a reading. `None` is
    /// an undeclared attribution — bytes archived before attribution existed
    /// stay readable and are never presented as this subject's own fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub perspective: Option<PerceptionPerspective>,
}

/// What this node knows about one installed perception unit.
///
/// `Unknown` and `Unavailable` are different facts. `Unknown` means nothing
/// has been observed (no source is bound, or the bound source has not answered
/// yet); `Unavailable` means a source was asked and could not deliver. Neither
/// is evidence that the thing being watched is fine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PerceptionStatus {
    /// A bound source observed something for this exact revision.
    Observed { fact: PerceptionFact },
    /// A bound source was asked and failed. The reason is its own failure.
    Unavailable { reason: String },
    /// Nothing has been observed for this revision on this node.
    Unknown { reason: String },
}

impl PerceptionStatus {
    /// The stable machine-readable state name.
    pub fn state(&self) -> &'static str {
        match self {
            PerceptionStatus::Observed { .. } => "observed",
            PerceptionStatus::Unavailable { .. } => "unavailable",
            PerceptionStatus::Unknown { .. } => "unknown",
        }
    }

    /// One bounded line for the model's disclosure.
    ///
    /// `now` is the projection's own clock. The line reports the fact's own
    /// times and never upgrades an expired or undeclared observation to a
    /// current one.
    pub fn line(&self, now: DateTime<Utc>) -> String {
        let line = match self {
            PerceptionStatus::Observed { fact } => {
                let validity = match fact.valid_until.as_deref() {
                    None => "；来源未声明有效期".to_string(),
                    Some(raw) => match DateTime::parse_from_rfc3339(raw) {
                        Ok(until) if until.with_timezone(&Utc) < now => {
                            format!("；有效期已过（止 {raw}）")
                        }
                        Ok(_) => format!("；有效期至 {raw}"),
                        Err(_) => format!("；有效期未能解析（{raw}）"),
                    },
                };
                let perspective = match fact.perspective.as_ref() {
                    Some(perspective) => format!("；{}", perspective.line()),
                    None => "；视角未声明（不能当作本主体自己的观察）".to_string(),
                };
                format!(
                    "观察：{}（来源：{}；对象：{}；观察于 {}{validity}{perspective}）",
                    fact.summary, fact.source, fact.target, fact.observed_at
                )
            }
            PerceptionStatus::Unavailable { reason } => format!("观察：不可用（{reason}）"),
            PerceptionStatus::Unknown { reason } => format!("观察：未知（{reason}）"),
        };
        truncate_utf8(&line, MAX_LINE_BYTES).0
    }
}

/// A live source of observations for one installed perception unit.
///
/// `observe` is called on the disclosure path, so a source must answer from a
/// bounded local read or from what it already published. A source that depends
/// on the network, a device or a slow probe observes on its own cadence and
/// returns its current cached fact here; it must not block the request.
pub trait PerceptionObserver: Send + Sync + 'static {
    fn observe(&self) -> Result<PerceptionFact>;
}

/// An observation source whose probe runs on its own cadence.
///
/// `observe` never waits for the probe: it returns the result the probe last
/// published. Before the first publication the source reports that it has not
/// observed yet rather than blocking the disclosure path, and a probe failure
/// is reported as the source's own failure instead of freezing the last fact.
/// A slow or asynchronous probe publishes through [`Self::publish`] /
/// [`Self::fail`] whenever it finishes a reading.
#[derive(Default)]
pub struct CachedObservationSource {
    current: Mutex<Option<std::result::Result<PerceptionFact, String>>>,
}

impl CachedObservationSource {
    /// A source with nothing published yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish the probe's latest observation.
    pub fn publish(&self, fact: PerceptionFact) {
        *self.current.lock() = Some(Ok(fact));
    }

    /// Publish the probe's latest failure.
    pub fn fail(&self, error: impl Into<String>) {
        *self.current.lock() = Some(Err(error.into()));
    }

    /// Whether the probe has published at least once.
    pub fn is_ready(&self) -> bool {
        self.current.lock().is_some()
    }
}

impl PerceptionObserver for CachedObservationSource {
    fn observe(&self) -> Result<PerceptionFact> {
        match self.current.lock().as_ref() {
            Some(Ok(fact)) => Ok(fact.clone()),
            Some(Err(error)) => anyhow::bail!("{error}"),
            None => anyhow::bail!("the source has not published an observation yet"),
        }
    }
}

struct Registration {
    id: EntityId,
    binding: PerceptionBinding,
    /// Which source this registration is. Several sources may answer for the
    /// same binding; a second registration under this id is refused so one
    /// source can never shadow another.
    source_id: PerceptionSourceId,
    /// Whose perception this source reads through. The registration states it
    /// once; an observation that arrives with another attribution is refused
    /// instead of being relabelled (docs/specs/cognition-units.md#spec-51-2).
    perspective: PerceptionPerspective,
    observer: Arc<dyn PerceptionObserver>,
    last: Option<PerceptionStatus>,
}

/// The live half of the perception catalogue.
///
/// The registry holds no installation state: which units exist, at which
/// revision, stays the Node's fact. It only binds a source to one exact
/// installed revision and remembers what that source last said.
#[derive(Default)]
pub struct PerceptionRegistry {
    registrations: Mutex<Vec<Registration>>,
}

impl PerceptionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind one source to one exact installed revision.
    ///
    /// The source registers under [`PerceptionSourceId::primary`]: a second
    /// unnamed bind is still refused rather than silently shadowing the
    /// first. Sources with their own identity bind through
    /// [`Self::bind_source`] so their observations merge by freshness (see
    /// [`merge_facts`]) instead of competing for the binding.
    pub fn bind(
        &self,
        binding: PerceptionBinding,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_source_as(
            binding,
            PerceptionSourceId::primary(),
            PerceptionPerspective::Own,
            observer,
        )
    }

    /// Bind one source that reads through a stated perspective.
    pub fn bind_as(
        &self,
        binding: PerceptionBinding,
        perspective: PerceptionPerspective,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_source_as(binding, PerceptionSourceId::primary(), perspective, observer)
    }

    /// Bind one named source to one exact installed revision.
    ///
    /// The source id must be free under this binding: a second registration
    /// under an id that is already live is refused, so the two sources can
    /// never alternate as the same fact. Distinct ids coexist and their
    /// observations merge by freshness.
    pub fn bind_source(
        &self,
        binding: PerceptionBinding,
        source_id: PerceptionSourceId,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_source_as(binding, source_id, PerceptionPerspective::Own, observer)
    }

    /// Bind one named source that reads through a stated perspective.
    pub fn bind_source_as(
        &self,
        binding: PerceptionBinding,
        source_id: PerceptionSourceId,
        perspective: PerceptionPerspective,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        check_binding(&binding)?;
        let id = EntityId::new();
        let mut registrations = self.registrations.lock();
        if let Some(existing) = registrations
            .iter()
            .find(|registration| {
                registration.binding == binding && registration.source_id == source_id
            })
        {
            anyhow::bail!(
                "perception unit '{}@{}' already has a live observation source '{}' (registration {})",
                binding.unit_id,
                binding.revision,
                source_id,
                existing.id
            );
        }
        registrations.push(Registration {
            id,
            binding,
            source_id,
            perspective,
            observer,
            last: None,
        });
        Ok(id)
    }

    /// Bind this revision's unnamed source when it has none yet.
    ///
    /// Two readers project the same installation at the same time — the
    /// subject's step and a finite child's run, for instance — and both may
    /// discover that a shipped source needs binding. The binding happens once:
    /// the later caller observes through the registration the first one made
    /// instead of failing the request or replacing the source. Sources bound
    /// under other ids do not count: a named source never satisfies the
    /// unnamed slot.
    pub fn bind_if_absent(
        &self,
        binding: PerceptionBinding,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_if_absent_as(binding, PerceptionPerspective::Own, observer)
    }

    /// Bind this revision's unnamed source in a stated perspective when it
    /// has none.
    pub fn bind_if_absent_as(
        &self,
        binding: PerceptionBinding,
        perspective: PerceptionPerspective,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_source_if_absent_as(
            binding,
            PerceptionSourceId::primary(),
            perspective,
            observer,
        )
    }

    /// Bind this revision's named source when that source id has none yet.
    pub fn bind_source_if_absent(
        &self,
        binding: PerceptionBinding,
        source_id: PerceptionSourceId,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        self.bind_source_if_absent_as(binding, source_id, PerceptionPerspective::Own, observer)
    }

    /// Bind this revision's named source in a stated perspective when that
    /// source id has none.
    ///
    /// The check is scoped to the source id: a registration under another id
    /// is a different source, not this one's.
    pub fn bind_source_if_absent_as(
        &self,
        binding: PerceptionBinding,
        source_id: PerceptionSourceId,
        perspective: PerceptionPerspective,
        observer: Arc<dyn PerceptionObserver>,
    ) -> Result<EntityId> {
        check_binding(&binding)?;
        let mut registrations = self.registrations.lock();
        if let Some(existing) = registrations
            .iter()
            .find(|registration| {
                registration.binding == binding && registration.source_id == source_id
            })
        {
            return Ok(existing.id);
        }
        let id = EntityId::new();
        registrations.push(Registration {
            id,
            binding,
            source_id,
            perspective,
            observer,
            last: None,
        });
        Ok(id)
    }

    /// Remove exactly that registration and the observation it recalled.
    pub fn unbind(&self, id: EntityId) -> bool {
        let mut registrations = self.registrations.lock();
        let before = registrations.len();
        registrations.retain(|registration| registration.id != id);
        registrations.len() != before
    }

    /// Remove every registration of this source under this binding.
    ///
    /// The other sources of the same binding keep answering; the binding's
    /// observation merges what remains.
    pub fn unbind_source(&self, binding: &PerceptionBinding, source_id: &PerceptionSourceId) -> usize {
        let mut registrations = self.registrations.lock();
        let before = registrations.len();
        registrations.retain(|registration| {
            !(registration.binding == *binding && registration.source_id == *source_id)
        });
        before - registrations.len()
    }

    /// The exact versions at least one source is currently bound to.
    pub fn bindings(&self) -> Vec<PerceptionBinding> {
        let mut bindings = Vec::new();
        for registration in self.registrations.lock().iter() {
            if !bindings.contains(&registration.binding) {
                bindings.push(registration.binding.clone());
            }
        }
        bindings
    }

    /// The sources currently answering for exactly this installed version.
    pub fn sources(&self, binding: &PerceptionBinding) -> Vec<PerceptionSourceId> {
        self.registrations
            .lock()
            .iter()
            .filter(|registration| &registration.binding == binding)
            .map(|registration| registration.source_id.clone())
            .collect()
    }

    /// Whether an observation source is bound to exactly this installed
    /// version.
    pub fn is_bound(&self, binding: &PerceptionBinding) -> bool {
        self.registrations
            .lock()
            .iter()
            .any(|registration| &registration.binding == binding)
    }

    /// Whether this source id is bound to exactly this installed version.
    pub fn is_source_bound(
        &self,
        binding: &PerceptionBinding,
        source_id: &PerceptionSourceId,
    ) -> bool {
        self.registrations
            .lock()
            .iter()
            .any(|registration| {
                &registration.binding == binding && &registration.source_id == source_id
            })
    }

    /// Whose perception the bound sources' merged observation was read
    /// through.
    ///
    /// With several sources the attribution is the merged fact's own declared
    /// perspective, so the disclosure reports the reading that actually
    /// answered rather than one source's registration. Before any observation
    /// it is the earliest registration's perspective. `None` means nothing is
    /// bound here, so no attribution is claimed.
    pub fn perspective(&self, binding: &PerceptionBinding) -> Option<PerceptionPerspective> {
        let registrations = self.registrations.lock();
        let mut candidates: Vec<&Registration> = registrations
            .iter()
            .filter(|registration| &registration.binding == binding)
            .collect();
        if candidates.is_empty() {
            return None;
        }
        if let PerceptionStatus::Observed { fact } = merge_statuses(
            candidates
                .iter()
                .map(|registration| {
                    registration
                        .last
                        .as_ref()
                        .map(|status| (&registration.source_id, status))
                }),
            Utc::now(),
        ) {
            return fact.perspective;
        }
        Some(candidates.remove(0).perspective.clone())
    }

    /// Retire the shipped sources that no longer speak for an installed
    /// version.
    ///
    /// Only the units named in `keep` are retired; every other registration
    /// belongs to whoever bound it and is left alone. A retired registration's
    /// cached observation goes with it: an observation taken for a version
    /// that is no longer installed must never answer for the new one.
    pub fn retire_builtin_except(&self, keep: &[PerceptionBinding]) -> usize {
        let mut registrations = self.registrations.lock();
        let before = registrations.len();
        registrations.retain(|registration| {
            !is_builtin_unit(&registration.binding.unit_id)
                || keep.contains(&registration.binding)
        });
        before - registrations.len()
    }

    /// Ask the bound sources for a fresh observation.
    ///
    /// Every source of this binding is asked on its own registration and
    /// perspective; the answers merge by freshness ([`merge_facts`]) into the
    /// binding's one status. A source that answers with a fact about another
    /// revision, or with fields the disclosure cannot carry, is reported
    /// `unavailable` with the contract violation as its reason. Its answer is
    /// never repaired into an observation it did not make.
    pub fn observe(&self, binding: &PerceptionBinding) -> PerceptionStatus {
        let candidates = {
            let registrations = self.registrations.lock();
            let candidates: Vec<(EntityId, Arc<dyn PerceptionObserver>, PerceptionPerspective)> =
                registrations
                    .iter()
                    .filter(|registration| &registration.binding == binding)
                    .map(|registration| {
                        (
                            registration.id,
                            Arc::clone(&registration.observer),
                            registration.perspective.clone(),
                        )
                    })
                    .collect();
            if candidates.is_empty() {
                return PerceptionStatus::Unknown {
                    reason: truncate_utf8(
                        "本机没有绑定到该版本的观察源；安装或阅读定义不等于取得现场事实",
                        MAX_REASON_BYTES,
                    )
                    .0,
                };
            }
            candidates
        };
        let mut outcomes: Vec<(EntityId, PerceptionStatus)> =
            Vec::with_capacity(candidates.len());
        for (id, observer, perspective) in &candidates {
            let status = match observer.observe() {
                Ok(fact) => match admit(binding, perspective, fact) {
                    Ok(fact) => PerceptionStatus::Observed { fact },
                    Err(error) => PerceptionStatus::Unavailable {
                        reason: truncate_utf8(
                            &format!("观察源返回的事实不成立于该绑定：{error:#}"),
                            MAX_REASON_BYTES,
                        )
                        .0,
                    },
                },
                Err(error) => PerceptionStatus::Unavailable {
                    reason: truncate_utf8(&format!("{error:#}"), MAX_REASON_BYTES).0,
                },
            };
            outcomes.push((*id, status));
        }
        // A registration may have been removed while its source was
        // answering; the merge below then covers only the registrations that
        // are still live, so a retired source never comes back to life inside
        // a cached fact.
        let mut registrations = self.registrations.lock();
        for (id, status) in outcomes {
            if let Some(registration) =
                registrations.iter_mut().find(|registration| registration.id == id)
            {
                registration.last = Some(status);
            }
        }
        merge_statuses(
            registrations
                .iter()
                .filter(|registration| &registration.binding == binding)
                .map(|registration| {
                    registration
                        .last
                        .as_ref()
                        .map(|status| (&registration.source_id, status))
                }),
            Utc::now(),
        )
    }

    /// The merged status of what this registry last recorded, without asking
    /// the sources.
    ///
    /// Each source contributes its own last answer, so a source that never
    /// observed does not downgrade a fresher observation another source made.
    pub fn cached(&self, binding: &PerceptionBinding) -> PerceptionStatus {
        let registrations = self.registrations.lock();
        let candidates: Vec<Option<(&PerceptionSourceId, &PerceptionStatus)>> = registrations
            .iter()
            .filter(|registration| &registration.binding == binding)
            .map(|registration| {
                registration
                    .last
                    .as_ref()
                    .map(|status| (&registration.source_id, status))
            })
            .collect();
        if candidates.is_empty() {
            return PerceptionStatus::Unknown {
                reason: truncate_utf8(
                    "本机没有绑定到该版本的观察源；安装或阅读定义不等于取得现场事实",
                    MAX_REASON_BYTES,
                )
                .0,
            };
        }
        merge_statuses(candidates.into_iter(), Utc::now())
    }
}

/// Merge the candidate facts of one binding into its single observation —
/// precedence by freshness.
///
/// When several sources answered for the same binding, the freshest fact
/// wins; a fact is never stitched together out of several observations.
///
/// The rule, applied against the projection's clock `now`:
///
/// 1. a fact whose declared `valid_until` is still ahead wins over a fact
///    that declares no shelf life and over one whose validity already
///    elapsed;
/// 2. a fact that declares no shelf life wins over an expired one — an
///    expired observation is a stale fact, not a current one;
/// 3. inside one freshness rank the latest `observed_at` wins;
/// 4. an exact tie goes to the lexicographically smaller source id, so the
///    merge never depends on registration order.
///
/// Every candidate fact was admitted under the same binding by [`admit`], so
/// the merge never compares observations across revisions.
fn merge_facts<'a, I>(candidates: I, now: DateTime<Utc>) -> Option<PerceptionFact>
where
    I: Iterator<Item = (&'a PerceptionSourceId, &'a PerceptionFact)>,
{
    candidates
        .max_by(|(left_id, left), (right_id, right)| {
            (
                freshness_rank(left, now),
                observed_time(left),
                std::cmp::Reverse(*left_id),
            )
                .cmp(&(
                    freshness_rank(right, now),
                    observed_time(right),
                    std::cmp::Reverse(*right_id),
                ))
        })
        .map(|(_, fact)| fact.clone())
}

/// Freshness rank of one admitted fact: 2 = declared and still valid,
/// 1 = no shelf life declared, 0 = declared validity already elapsed.
fn freshness_rank(fact: &PerceptionFact, now: DateTime<Utc>) -> u8 {
    match fact.valid_until.as_deref() {
        None => 1,
        Some(raw) => match DateTime::parse_from_rfc3339(raw) {
            Ok(until) if until.with_timezone(&Utc) > now => 2,
            // `admit` already refused unparseable times; a defensive parse
            // failure ranks as expired rather than as current.
            _ => 0,
        },
    }
}

/// The fact's observation time, when it parses. `admit` already refuses
/// unparseable times, so `None` here ranks strictly below any real reading.
fn observed_time(fact: &PerceptionFact) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&fact.observed_at)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// Merge each live source's last status into the binding's one status.
///
/// Any observed fact wins by [`merge_facts`]; an observation another source
/// made is not downgraded by a source that failed or has not answered yet.
/// With no fact at all the binding is `unavailable` when at least one source
/// answered with a failure — the reasons are joined, never dropped — and
/// `unknown` only when every source is still pending.
fn merge_statuses<'a, I>(statuses: I, now: DateTime<Utc>) -> PerceptionStatus
where
    I: Iterator<Item = Option<(&'a PerceptionSourceId, &'a PerceptionStatus)>>,
{
    let mut facts: Vec<(&PerceptionSourceId, &PerceptionFact)> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    let mut pending = 0_usize;
    for item in statuses {
        match item {
            Some((source_id, PerceptionStatus::Observed { fact })) => {
                facts.push((source_id, fact));
            }
            Some((_, PerceptionStatus::Unavailable { reason })) => reasons.push(reason.clone()),
            Some((_, PerceptionStatus::Unknown { reason })) => reasons.push(reason.clone()),
            None => pending += 1,
        }
    }
    if let Some(fact) = merge_facts(facts.into_iter(), now) {
        return PerceptionStatus::Observed { fact };
    }
    if !reasons.is_empty() {
        let mut reason = reasons.join("；");
        if pending > 0 {
            reason.push_str(&format!("；另有 {pending} 个观察源尚未取得观测"));
        }
        return PerceptionStatus::Unavailable {
            reason: truncate_utf8(&reason, MAX_REASON_BYTES).0,
        };
    }
    PerceptionStatus::Unknown {
        reason: truncate_utf8(
            "观察源已绑定但还没有取得观测；这不是正常状态的证据",
            MAX_REASON_BYTES,
        )
        .0,
    }
}

/// A registration names one complete installed identity.
fn check_binding(binding: &PerceptionBinding) -> Result<()> {
    ensure!(
        !binding.unit_id.trim().is_empty()
            && binding.unit_id.len() <= MAX_NAME_BYTES
            && !binding.revision.trim().is_empty()
            && binding.revision.len() <= MAX_NAME_BYTES
            && !binding.artifact_digest.trim().is_empty(),
        "perception binding is not a complete installed identity"
    );
    Ok(())
}

/// Admit one observation under the binding and perspective it was requested
/// for.
fn admit(
    binding: &PerceptionBinding,
    perspective: &PerceptionPerspective,
    mut fact: PerceptionFact,
) -> Result<PerceptionFact> {
    ensure!(
        &fact.binding == binding,
        "the source answered for '{}@{}' instead of '{}@{}'",
        fact.binding.unit_id,
        fact.binding.revision,
        binding.unit_id,
        binding.revision
    );
    // Perspective taking is not this subject's own observation: a reading
    // taken through another perception must say so, and a reading that says
    // nothing is never admitted as the bound perspective's fact.
    match fact.perspective.as_ref() {
        Some(declared) if declared.same(perspective) => {}
        Some(declared) => anyhow::bail!(
            "the observation was read through '{}' but this source speaks for '{}'",
            declared.state(),
            perspective.state()
        ),
        None => anyhow::bail!(
            "the observation does not declare whose perception it was read through; '{}' cannot be assumed",
            perspective.state()
        ),
    }
    ensure!(
        !fact.source.trim().is_empty() && fact.source.len() <= MAX_NAME_BYTES,
        "the observation does not name a bounded source"
    );
    ensure!(
        !fact.target.trim().is_empty() && fact.target.len() <= MAX_NAME_BYTES,
        "the observation does not name a bounded target"
    );
    ensure!(
        !fact.summary.trim().is_empty(),
        "the observation states nothing it observed"
    );
    DateTime::parse_from_rfc3339(&fact.observed_at)
        .context("the observation has no parseable observation time")?;
    if let Some(valid_until) = fact.valid_until.as_deref() {
        ensure!(
            DateTime::parse_from_rfc3339(valid_until).is_ok(),
            "the observation declares an unparseable validity end"
        );
    }
    fact.summary = truncate_utf8(&fact.summary, MAX_SUMMARY_BYTES).0;
    Ok(fact)
}

/// The source behind the shipped [`INSTALLATION_UNITS`] unit.
///
/// It reports what this node's installation authority actually holds: the
/// units installed by kind, how many are switched off, how many ordinary
/// knowledge units are still candidates, and how many required dependency
/// edges are unresolved. Every figure comes from the Node's own facts, so the
/// observation has a source, a target and a time.
pub struct InstallationFactsSource {
    node: Arc<Mutex<Node>>,
    binding: PerceptionBinding,
    /// The archived observation this binding last produced, keyed by a
    /// signature of its content. The installation usually does not change
    /// between requests, so an unchanged reading reports the record it
    /// already archived instead of minting a new observation time for the
    /// same content.
    last: Mutex<Option<(String, PerceptionFact)>>,
}

impl InstallationFactsSource {
    /// A source that speaks for one installed version.
    pub fn new(node: Arc<Mutex<Node>>, binding: PerceptionBinding) -> Result<Self> {
        ensure!(
            binding.unit_id == INSTALLATION_UNITS,
            "the installation-facts source only speaks for '{INSTALLATION_UNITS}'"
        );
        Ok(Self {
            node,
            binding,
            last: Mutex::new(None),
        })
    }

    /// Observe the installation now, and archive the observation as the
    /// unit's own durable record.
    ///
    /// The bytes stored against the exact installed version are the fact
    /// itself (without its own evidence reference), so a later reader can
    /// check what this node observed, when, about what, and under which unit
    /// version — the observation outlives the unit being uninstalled, per
    /// docs/specs/cognition-units.md#spec-45-10-1.
    pub fn observe_now(&self) -> Result<PerceptionFact> {
        let mut node = self.node.lock();
        let facts = node.installed_unit_facts()?;
        let count = |kind: UnitKind| {
            facts
                .iter()
                .filter(|fact| fact.kind == kind)
                .count()
        };
        let disabled = facts.iter().filter(|fact| !fact.enabled).count();
        let candidates = facts
            .iter()
            .filter(|fact| {
                fact.kind == UnitKind::Awareness
                    && !fact.enabled
                    && fact
                        .descriptor
                        .config
                        .pointer("/cognition/learning/state")
                        .and_then(serde_json::Value::as_str)
                        == Some("candidate")
            })
            .count();
        let unresolved = facts
            .iter()
            .flat_map(|fact| fact.edges.iter())
            .filter(|edge| {
                edge.strength == shenko_node::DependencyStrength::Required
                    && edge.status == shenko_node::EdgeStatus::Unresolved
            })
            .count();
        let summary = format!(
            "本机安装了 {} 个认知单元：感知 {}、直觉 {}、Skill {}、代码 {}；其中 {disabled} 个已停用、{candidates} 个仍是候选、{unresolved} 条必需依赖未解析。",
            facts.len(),
            count(UnitKind::Perception),
            count(UnitKind::Awareness),
            count(UnitKind::Skill),
            count(UnitKind::Code),
        );
        let mut fact = PerceptionFact {
            binding: self.binding.clone(),
            source: INSTALLATION_UNITS_SOURCE.to_string(),
            target: INSTALLATION_UNITS_TARGET.to_string(),
            observed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            valid_until: None,
            summary: truncate_utf8(&summary, MAX_SUMMARY_BYTES).0,
            evidence_digest: None,
            // The shipped source observes this installation itself; it never
            // reads another subject's perception.
            perspective: Some(PerceptionPerspective::Own),
        };
        // What the observation says, without the time it was taken: the same
        // reading of the same state is one archived observation, not a new
        // one every request.
        let signature = Digest::blake3(&serde_json::to_vec(&(
            &fact.binding,
            &fact.source,
            &fact.target,
            &fact.valid_until,
            &fact.summary,
        ))?)
        .0;
        if let Some((known, archived)) = self.last.lock().as_ref() {
            if known == &signature {
                return Ok(archived.clone());
            }
        }
        let bytes = serde_json::to_vec(&fact)?;
        let digest = Digest::blake3(&bytes);
        node.handle_unit_request(UnitRequest::RegisterObservation {
            unit_id: fact.binding.unit_id.clone(),
            revision: fact.binding.revision.clone(),
            artifact_digest: Digest(fact.binding.artifact_digest.clone()),
            kind: "perception_observation".into(),
            bytes,
        })?;
        fact.evidence_digest = Some(digest.0);
        *self.last.lock() = Some((signature, fact.clone()));
        Ok(fact)
    }
}

impl PerceptionObserver for InstallationFactsSource {
    fn observe(&self) -> Result<PerceptionFact> {
        self.observe_now()
    }
}

/// Whether a shipped runtime source speaks for this unit.
fn is_builtin_unit(unit_id: &str) -> bool {
    matches!(unit_id, INSTALLATION_UNITS)
}

/// The runtime source for one exact installed version, when this build ships
/// one for that unit.
fn builtin_source(
    node: &Arc<Mutex<Node>>,
    binding: &PerceptionBinding,
) -> Result<Option<Arc<dyn PerceptionObserver>>> {
    Ok(match binding.unit_id.as_str() {
        INSTALLATION_UNITS => Some(Arc::new(InstallationFactsSource::new(
            Arc::clone(node),
            binding.clone(),
        )?)),
        _ => None,
    })
}

/// Keep the shipped sources bound to the installed revisions of their own
/// units.
///
/// A unit update is a new revision and artifact digest, so the registration
/// that spoke for the previous version can no longer answer: it is retired
/// and the source is bound to the installed version. A shipped unit that is
/// no longer installed loses its source. Every other registration belongs to
/// whoever bound it and is left untouched.
///
/// A perception unit with no runtime source is left unbound: its card then
/// reports `unknown`, which is the truth about this node rather than a fact
/// invented to fill the slot.
pub fn reconcile_builtin_perception_sources(
    node: &Arc<Mutex<Node>>,
    registry: &PerceptionRegistry,
) -> Result<Vec<PerceptionBinding>> {
    let facts = node.lock().installed_unit_facts()?;
    reconcile_perception_sources_from(&facts, node, registry)
}

/// The same reconciliation over installation facts the caller already read.
///
/// A projection reads the installation once and derives its cards, its
/// revision and its observation bindings from that one reading, so a card can
/// never describe units from a different installation than the sources that
/// answer for them.
pub fn reconcile_perception_sources_from(
    facts: &[shenko_node::InstalledUnitFact],
    node: &Arc<Mutex<Node>>,
    registry: &PerceptionRegistry,
) -> Result<Vec<PerceptionBinding>> {
    let installed: Vec<&shenko_node::InstalledUnitFact> = facts
        .iter()
        .filter(|fact| fact.kind == UnitKind::Perception && is_builtin_unit(&fact.unit_id))
        .collect();
    let wanted: Vec<PerceptionBinding> = installed
        .iter()
        .map(|fact| PerceptionBinding::of(*fact))
        .collect();
    registry.retire_builtin_except(&wanted);
    let builtin_id = PerceptionSourceId::builtin();
    for fact in installed {
        let binding = PerceptionBinding::of(fact);
        // The check is scoped to the built-in source id: a consumer source
        // bound under its own id answers alongside the shipped one, never
        // instead of it.
        if registry.is_source_bound(&binding, &builtin_id) {
            continue;
        }
        if let Some(source) = builtin_source(node, &binding)? {
            // Two projections may discover the same unbound revision at once;
            // the second one observes through the first one's registration.
            registry.bind_source_if_absent(binding, builtin_id.clone(), source)?;
        }
    }
    Ok(wanted)
}

/// Bind the built-in perception sources to the installed units they speak
/// for, then take one observation so the first request has a fact instead of
/// a pending state.
pub fn bind_builtin_perception_sources(
    node: Arc<Mutex<Node>>,
    registry: &PerceptionRegistry,
) -> Result<Vec<String>> {
    let bound = reconcile_builtin_perception_sources(&node, registry)?;
    for binding in &bound {
        let status = registry.observe(binding);
        ensure!(
            matches!(status, PerceptionStatus::Observed { .. }),
            "the built-in source for '{}@{}' could not observe: {}",
            binding.unit_id,
            binding.revision,
            status.line(Utc::now())
        );
    }
    Ok(bound
        .into_iter()
        .map(|binding| format!("{}@{}", binding.unit_id, binding.revision))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A source that answers with one prepared fact and counts how often it
    /// was actually asked.
    struct Fixed {
        fact: PerceptionFact,
        calls: AtomicUsize,
    }

    impl PerceptionObserver for Fixed {
        fn observe(&self) -> Result<PerceptionFact> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.fact.clone())
        }
    }

    fn binding(revision: &str) -> PerceptionBinding {
        PerceptionBinding {
            unit_id: INSTALLATION_UNITS.into(),
            revision: revision.into(),
            artifact_digest: format!("blake3:{revision}"),
        }
    }

    fn fact(binding: &PerceptionBinding) -> PerceptionFact {
        PerceptionFact {
            binding: binding.clone(),
            source: INSTALLATION_UNITS_SOURCE.into(),
            target: INSTALLATION_UNITS_TARGET.into(),
            observed_at: "2026-11-05T00:00:00Z".into(),
            valid_until: None,
            summary: "本机安装了 0 个认知单元".into(),
            evidence_digest: None,
            perspective: Some(PerceptionPerspective::Own),
        }
    }

    fn source(binding: &PerceptionBinding) -> Arc<Fixed> {
        Arc::new(Fixed { fact: fact(binding), calls: AtomicUsize::new(0) })
    }

    #[test]
    fn a_second_source_cannot_shadow_the_same_installed_revision() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        registry.bind(binding.clone(), source(&binding)).unwrap();
        let error = registry.bind(binding.clone(), source(&binding)).unwrap_err().to_string();
        assert!(error.contains("already has a live observation source"), "{error}");
        assert_eq!(registry.bindings(), vec![binding]);
    }

    #[test]
    fn two_projections_bind_the_same_revision_once() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        let id = registry.bind_if_absent(binding.clone(), source(&binding)).unwrap();
        // The other reader's source is not adopted and the request is not
        // failed: the registration the first reader made answers both.
        let other = registry.bind_if_absent(binding.clone(), source(&binding)).unwrap();
        assert_eq!(other, id);
        assert_eq!(registry.bindings(), vec![binding.clone()]);
        assert!(matches!(registry.observe(&binding), PerceptionStatus::Observed { .. }));
    }

    #[test]
    fn an_unbound_revision_reports_unknown_rather_than_healthy() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        assert!(!registry.is_bound(&binding));
        let status = registry.observe(&binding);
        assert!(matches!(status, PerceptionStatus::Unknown { .. }), "{status:?}");
        assert!(matches!(registry.cached(&binding), PerceptionStatus::Unknown { .. }));
        // The line says nothing has been observed; it never reads as "fine".
        let line = status.line(Utc::now());
        assert!(line.contains("未知"), "{line}");
        assert!(!line.contains("观察：本机安装了"), "{line}");
    }

    #[test]
    fn a_source_answering_for_another_revision_becomes_unavailable() {
        let registry = PerceptionRegistry::new();
        // The registration speaks for 0.2.0; the source answers for 0.1.0.
        let binding = binding("0.2.0");
        registry.bind(binding.clone(), source(&self::binding("0.1.0"))).unwrap();
        let status = registry.observe(&binding);
        let PerceptionStatus::Unavailable { reason } = &status else {
            panic!("a foreign observation must not be admitted: {status:?}")
        };
        assert!(reason.contains("不成立于该绑定"), "{reason}");
        assert!(matches!(registry.cached(&binding), PerceptionStatus::Unavailable { .. }));
    }

    #[test]
    fn retiring_a_replaced_revision_drops_its_cached_observation() {
        let registry = PerceptionRegistry::new();
        let old = binding("0.1.0");
        let new = binding("0.2.0");
        registry.bind(old.clone(), source(&old)).unwrap();
        assert!(matches!(registry.observe(&old), PerceptionStatus::Observed { .. }));
        // The unit update is a new revision: the source that answered for the
        // previous one is retired instead of speaking for the new version.
        assert_eq!(registry.retire_builtin_except(&[new.clone()]), 1);
        assert_eq!(registry.bindings(), Vec::new());
        assert!(matches!(registry.cached(&old), PerceptionStatus::Unknown { .. }));
        assert!(matches!(registry.retire_builtin_except(&[new]), 0));
    }

    #[test]
    fn an_observation_past_its_declared_validity_is_not_upgraded() {
        let binding = binding("0.1.0");
        let observed = |valid_until: Option<&str>| PerceptionStatus::Observed {
            fact: PerceptionFact { valid_until: valid_until.map(str::to_string), ..fact(&binding) },
        };
        let now = DateTime::parse_from_rfc3339("2026-11-05T12:00:00Z").unwrap().with_timezone(&Utc);
        assert!(observed(Some("2026-11-05T11:00:00Z")).line(now).contains("有效期已过"));
        assert!(observed(Some("2026-11-05T13:00:00Z")).line(now).contains("有效期至"));
        assert!(observed(Some("not-a-time")).line(now).contains("有效期未能解析"));
        assert!(observed(None).line(now).contains("来源未声明有效期"));
    }

    /// Perspective taking is not the subject's own observation. A source bound
    /// to "my own view" that answers with another subject's reading is refused,
    /// and a modeled reading is disclosed as modeled rather than adopted as an
    /// own fact.
    #[test]
    fn a_modeled_perspective_is_never_admitted_as_the_subjects_own_observation() {
        let binding = binding("0.1.0");
        let mut foreign = fact(&binding);
        foreign.perspective = Some(PerceptionPerspective::ModeledOther {
            subject: "someone-else".into(),
        });
        // The modeled source is bound in its own perspective: the reading is
        // disclosed as the other subject's view.
        let modeled = {
            let registry = PerceptionRegistry::new();
            registry
                .bind_as(
                    binding.clone(),
                    PerceptionPerspective::ModeledOther { subject: "someone-else".into() },
                    Arc::new(Fixed { fact: foreign.clone(), calls: AtomicUsize::new(0) }),
                )
                .unwrap();
            registry.observe(&binding)
        };
        let PerceptionStatus::Observed { fact: modeled_fact } = &modeled else {
            panic!("the modeled observation is admitted under its own binding: {modeled:?}")
        };
        assert_eq!(
            modeled_fact.perspective,
            Some(PerceptionPerspective::ModeledOther { subject: "someone-else".into() })
        );
        let line = modeled.line(Utc::now());
        assert!(line.contains("模拟 someone-else"), "{line}");
        assert!(line.contains("不是本主体的观察"), "{line}");

        // The same reading cannot be filed as this subject's own observation.
        let own = PerceptionRegistry::new();
        own.bind(
            binding.clone(),
            Arc::new(Fixed { fact: foreign, calls: AtomicUsize::new(0) }),
        )
        .unwrap();
        let status = own.observe(&binding);
        let PerceptionStatus::Unavailable { reason } = &status else {
            panic!("a modeled reading must not become an own observation: {status:?}")
        };
        assert!(reason.contains("modeled_other"), "{reason}");
        assert!(reason.contains("own"), "{reason}");
    }

    /// A source that says nothing about whose perception it read through is
    /// not admitted: silence is not a default of "this subject's own view".
    #[test]
    fn an_undeclared_perspective_is_not_defaulted_to_own() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        let mut undeclared = fact(&binding);
        undeclared.perspective = None;
        registry
            .bind(binding.clone(), Arc::new(Fixed { fact: undeclared, calls: AtomicUsize::new(0) }))
            .unwrap();
        let status = registry.observe(&binding);
        let PerceptionStatus::Unavailable { reason } = &status else {
            panic!("an undeclared attribution must not be assumed: {status:?}")
        };
        assert!(reason.contains("does not declare whose perception"), "{reason}");
    }

    /// A fact with a chosen source name and observation time.
    fn sourced_fact(
        binding: &PerceptionBinding,
        source: &str,
        observed_at: &str,
        valid_until: Option<&str>,
    ) -> PerceptionFact {
        PerceptionFact {
            source: source.into(),
            observed_at: observed_at.into(),
            valid_until: valid_until.map(str::to_string),
            summary: format!("{source} 观察到了目标"),
            ..fact(binding)
        }
    }

    /// Several sources may answer for one binding under distinct ids; a
    /// second registration under an id that is already live is refused so no
    /// source ever shadows another.
    #[test]
    fn distinct_sources_share_a_binding_but_a_duplicate_source_id_is_refused() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        let alpha = PerceptionSourceId::named("alpha").unwrap();
        let beta = PerceptionSourceId::named("beta").unwrap();
        registry
            .bind_source(binding.clone(), alpha.clone(), source(&binding))
            .unwrap();
        let error = registry
            .bind_source(binding.clone(), alpha.clone(), source(&binding))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("already has a live observation source") && error.contains("alpha"),
            "{error}"
        );
        // The empty name is not a source id.
        assert!(PerceptionSourceId::named(" ").is_err());
        // A different id is a different source; the unnamed slot is still
        // free, and its own duplicate is still refused.
        registry
            .bind_source(binding.clone(), beta.clone(), source(&binding))
            .unwrap();
        registry.bind(binding.clone(), source(&binding)).unwrap();
        assert!(registry
            .bind(binding.clone(), source(&binding))
            .unwrap_err()
            .to_string()
            .contains("already has a live observation source"));
        // `bind_if_absent` adopts the live unnamed registration: the new
        // observer is not installed over it.
        let again = registry.bind_if_absent(binding.clone(), source(&binding)).unwrap();
        assert!(registry.unbind(again));
        assert_eq!(registry.bindings(), vec![binding.clone()]);
        let mut sources = registry.sources(&binding);
        sources.sort();
        assert_eq!(sources, vec![alpha, beta]);
    }

    /// Distinct perspectives may observe the same binding: the merged answer
    /// is the freshest fact, and the registry's perspective follows the fact
    /// that actually answered.
    #[test]
    fn the_binding_observation_merges_sources_by_freshness() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        // The named source saw the target earlier; the unnamed source saw it
        // later. Same freshness rank, so the later observation wins.
        let mut earlier = sourced_fact(&binding, "remote:probe", "2026-11-05T08:00:00Z", None);
        earlier.target = "node:remote".into();
        let mut later = sourced_fact(&binding, "node:probe", "2026-11-05T10:00:00Z", None);
        later.target = "node:local".into();
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("remote").unwrap(),
                Arc::new(Fixed { fact: earlier, calls: AtomicUsize::new(0) }),
            )
            .unwrap();
        registry
            .bind(binding.clone(), Arc::new(Fixed { fact: later, calls: AtomicUsize::new(0) }))
            .unwrap();
        let status = registry.observe(&binding);
        let PerceptionStatus::Observed { fact } = &status else {
            panic!("two answering sources still observe: {status:?}")
        };
        assert_eq!(fact.source, "node:probe", "the later observation wins: {fact:?}");
        // The merge is not one-shot: the cached reading is the same fact.
        let PerceptionStatus::Observed { fact: cached } = registry.cached(&binding) else {
            panic!("the merged observation is cached")
        };
        assert_eq!(cached.source, "node:probe");

        // A fact that declares a still-valid shelf life outranks a fresher
        // observation that declares none — and outranks an expired one even
        // when the expired reading was taken later.
        let registry = PerceptionRegistry::new();
        let undeclared = sourced_fact(&binding, "fresh:undeclared", "2026-11-07T00:00:00Z", None);
        let expired = sourced_fact(
            &binding,
            "old:expired",
            "2026-11-08T00:00:00Z",
            Some("2026-01-01T00:00:00Z"),
        );
        let valid = sourced_fact(
            &binding,
            "steady:valid",
            "2026-11-05T00:00:00Z",
            Some("2999-01-01T00:00:00Z"),
        );
        for (id, fact) in [("a", undeclared), ("b", expired), ("c", valid)] {
            registry
                .bind_source(
                    binding.clone(),
                    PerceptionSourceId::named(id).unwrap(),
                    Arc::new(Fixed { fact, calls: AtomicUsize::new(0) }),
                )
                .unwrap();
        }
        let status = registry.observe(&binding);
        let PerceptionStatus::Observed { fact } = &status else {
            panic!("three answering sources still observe: {status:?}")
        };
        assert_eq!(fact.source, "steady:valid", "the still-valid fact wins: {fact:?}");

        // An exact tie between two current facts resolves deterministically
        // by source id — never by registration order.
        let registry = PerceptionRegistry::new();
        for (id, source) in [("z", "z:probe"), ("a", "a:probe")] {
            registry
                .bind_source(
                    binding.clone(),
                    PerceptionSourceId::named(id).unwrap(),
                    Arc::new(Fixed {
                        fact: sourced_fact(
                            &binding,
                            source,
                            "2026-11-05T00:00:00Z",
                            Some("2999-01-01T00:00:00Z"),
                        ),
                        calls: AtomicUsize::new(0),
                    }),
                )
                .unwrap();
        }
        let PerceptionStatus::Observed { fact } = registry.observe(&binding) else {
            panic!("an exact tie still observes")
        };
        assert_eq!(fact.source, "a:probe", "the smaller source id wins a tie: {fact:?}");
    }

    /// One source's failure or pending state never downgrades the fresher
    /// fact another source observed; with no fact at all the binding reports
    /// every source's own reason.
    #[test]
    fn a_failing_source_never_downgrades_another_sources_observation() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("answering").unwrap(),
                Arc::new(Fixed {
                    fact: sourced_fact(&binding, "node:probe", "2026-11-05T10:00:00Z", None),
                    calls: AtomicUsize::new(0),
                }),
            )
            .unwrap();
        struct Failing;
        impl PerceptionObserver for Failing {
            fn observe(&self) -> Result<PerceptionFact> {
                anyhow::bail!("probe read failed")
            }
        }
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("failing").unwrap(),
                Arc::new(Failing),
            )
            .unwrap();
        let pending = Arc::new(CachedObservationSource::new());
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("pending").unwrap(),
                pending.clone(),
            )
            .unwrap();
        let PerceptionStatus::Observed { fact } = registry.observe(&binding) else {
            panic!("a failure and a pending probe do not shadow the observation")
        };
        assert_eq!(fact.source, "node:probe");

        // With no fact at all the binding is unavailable once any source
        // answered with a failure; a source that has not answered keeps its
        // pending reason in the report rather than disappearing.
        let registry = PerceptionRegistry::new();
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("failing").unwrap(),
                Arc::new(Failing),
            )
            .unwrap();
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("pending").unwrap(),
                pending.clone(),
            )
            .unwrap();
        let PerceptionStatus::Unavailable { reason } = registry.observe(&binding) else {
            panic!("two failed-to-answer sources are unavailable")
        };
        assert!(reason.contains("probe read failed"), "{reason}");
        assert!(reason.contains("not published an observation"), "{reason}");
        // Only pending sources — none has answered — stay honestly unknown.
        let registry = PerceptionRegistry::new();
        registry.bind(binding.clone(), Arc::new(Failing)).unwrap();
        assert!(matches!(registry.cached(&binding), PerceptionStatus::Unknown { .. }));
    }

    /// A probe that publishes on its own cadence never blocks `observe`: it
    /// reports its failure while the fresher fact of another source still
    /// answers for the binding.
    #[test]
    fn a_self_cadenced_source_publishes_without_blocking_the_observation() {
        let registry = PerceptionRegistry::new();
        let binding = binding("0.1.0");
        let cached = Arc::new(CachedObservationSource::new());
        registry
            .bind_source(
                binding.clone(),
                PerceptionSourceId::named("probe").unwrap(),
                cached.clone(),
            )
            .unwrap();
        assert!(!cached.is_ready());
        let PerceptionStatus::Unavailable { reason } = registry.observe(&binding) else {
            panic!("a probe with no reading yet is unavailable, not blocking")
        };
        assert!(reason.contains("not published"), "{reason}");
        // The probe's own failure is reported as the source's failure.
        cached.fail("device timeout");
        let PerceptionStatus::Unavailable { reason } = registry.observe(&binding) else {
            panic!("the probe failure is the source's status")
        };
        assert!(reason.contains("device timeout"), "{reason}");
        // Once the probe publishes, its fact answers for the binding.
        cached.publish(sourced_fact(&binding, "probe:device", "2026-11-06T00:00:00Z", None));
        let PerceptionStatus::Observed { fact } = registry.observe(&binding) else {
            panic!("the published fact is the binding's observation")
        };
        assert_eq!(fact.source, "probe:device");
    }
}
