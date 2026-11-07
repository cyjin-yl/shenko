//! The replaceable-strategy seam — SPEC §42.1, §42.5, §42.6.
//!
//! §42.1 names four replaceable quota plugins: the observer, the forecaster,
//! the policy and the provider adapters. [`crate::plugin`] is the observer
//! half. This module is the other half: a forecaster, a router (the policy
//! that picks profile, effort and admission rate) and an admission cadence
//! are installed as versioned strategies, and the runtime uses whichever
//! strategy it selected instead of a hard-wired function.
//!
//! Three rules keep replacement honest:
//!
//! * **The durable state machine stays authoritative.** A forecast or a
//!   route is an input to the recorded decision; the admission transition
//!   and its single-writer [`Pacer`] remain the library's. A cadence
//!   strategy may therefore only *withhold* a fresh admission it would
//!   otherwise be granted — it can never admit without budget, release an
//!   armed reservation, downgrade dispatch evidence or loosen pacing. That
//!   is also why an installed strategy cannot be used to fake an admission
//!   outcome the portfolio could not support.
//! * **Every record names the strategy that produced it.** A forecast keeps
//!   `cost_model_version` and a decision keeps `strategy`; a reader can
//!   replay a period against the same algorithm, and a deliberate strategy
//!   change is visible instead of silently reinterpreting old numbers.
//!   Records produced before this seam keep the field absent: unknown, never
//!   guessed as the baseline.
//! * **No strategy may invent a fact.** Refusal is a typed
//!   [`StrategyError`], never a fabricated forecast, a made-up balance or a
//!   silent fallback to the baseline. The caller decides what a refusal
//!   means; it is never reported as quota exhaustion or capacity.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::admission::{AdmissionOutcome, Pacer};
use crate::forecast::{forecast, LocalAttempt, QuotaForecast, WindowAssumptions};
use crate::model::QuotaSnapshot;
use crate::policy::{decide_with, BackendState, PolicyDecision, QuotaPolicy, WorkEvidence};

/// The version of this replaceable-strategy protocol. A strategy built
/// against another version is refused at registration instead of being
/// interpreted with the wrong input or output meaning.
pub const QUOTA_STRATEGY_PROTOCOL_VERSION: u32 = 1;

/// The stable identifier of the built-in rolling planning baseline (§42.5).
pub const BASELINE_STRATEGY_ID: &str = "shenko.quota.rolling_baseline";

/// The algorithm version of the built-in baseline. It is recorded in every
/// forecast and decision the baseline produces, so a later algorithm can be
/// told apart from it in the same ledger.
pub const BASELINE_STRATEGY_VERSION: &str = "1";

/// The longest identifier or version a strategy may declare. Identifiers are
/// written into durable records and into the subject's readable view; a
/// bounded token keeps that text bounded.
pub const MAX_STRATEGY_TEXT: usize = 128;

/// The longest refusal or deferral reason a strategy may hand back. The
/// reason becomes durable text the subject reads, so it is bounded here
/// rather than trusted to every implementation.
pub const MAX_STRATEGY_REASON: usize = 512;

/// Which replaceable decision this strategy makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StrategyKind {
    /// Turns one recorded reading into a bounded forecast (§42.5).
    Forecast,
    /// Chooses profile, effort budget and admission rate for the next
    /// request under the recorded policy (§42.5).
    Route,
    /// May withhold a fresh admission the durable state would grant (§42.5).
    Cadence,
}

impl StrategyKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Forecast => "forecast",
            Self::Route => "route",
            Self::Cadence => "cadence",
        }
    }
}

/// What one installed strategy is. `strategy_id` is stable across versions
/// of the same algorithm; `version` changes whenever the algorithm's output
/// could differ for the same input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrategyDescriptor {
    pub protocol_version: u32,
    pub strategy_id: String,
    pub version: String,
    pub kind: StrategyKind,
}

impl StrategyDescriptor {
    /// The identity written into a produced record: `id@version`. It names
    /// both the algorithm and the exact revision that computed the numbers.
    pub fn tag(&self) -> String {
        format!("{}@{}", self.strategy_id, self.version)
    }
}

/// Why a strategy was refused at registration or at use. Every variant is a
/// bounded classification written by this crate; no transport error, body
/// or credential reaches the ledger through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrategyError {
    /// The strategy speaks another protocol version.
    ProtocolMismatch,
    /// An identifier, version or reason is empty, padded or over-long.
    InvalidDescriptor,
    /// The descriptor's kind is not the kind it was registered as.
    KindMismatch,
    /// A strategy with this identifier is already installed for this kind.
    DuplicateStrategy,
    /// The built-in baseline identifier is reserved for the baseline.
    ReservedIdentifier,
    /// No installed strategy carries the selected identifier.
    UnknownStrategy,
    /// The strategy declined to compute. The reason is a bounded
    /// classification written by the strategy, never a raw error.
    Refused { reason: String },
}

impl std::fmt::Display for StrategyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProtocolMismatch => write!(f, "quota strategy protocol version mismatch"),
            Self::InvalidDescriptor => write!(f, "quota strategy descriptor is invalid"),
            Self::KindMismatch => write!(f, "quota strategy descriptor kind does not match"),
            Self::DuplicateStrategy => write!(f, "quota strategy is already installed"),
            Self::ReservedIdentifier => write!(f, "quota strategy identifier is reserved"),
            Self::UnknownStrategy => write!(f, "quota strategy is not installed"),
            Self::Refused { reason } => write!(f, "quota strategy refused: {reason}"),
        }
    }
}

impl std::error::Error for StrategyError {}

/// A bounded strategy text: trimmed, non-empty, within [`MAX_STRATEGY_TEXT`].
fn valid_text(text: &str) -> bool {
    !text.is_empty() && text.trim() == text && text.chars().count() <= MAX_STRATEGY_TEXT
}

/// A bounded strategy reason: trimmed and within [`MAX_STRATEGY_REASON`].
/// The admission path uses it too, so a strategy cannot grow a durable
/// reason past the view's text budget.
pub(crate) fn bounded_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.chars().count() <= MAX_STRATEGY_REASON {
        return trimmed.to_owned();
    }
    trimmed.chars().take(MAX_STRATEGY_REASON).collect()
}

fn check_descriptor(
    descriptor: &StrategyDescriptor,
    kind: StrategyKind,
) -> Result<StrategyDescriptor, StrategyError> {
    if descriptor.protocol_version != QUOTA_STRATEGY_PROTOCOL_VERSION {
        return Err(StrategyError::ProtocolMismatch);
    }
    if !valid_text(&descriptor.strategy_id) || !valid_text(&descriptor.version) {
        return Err(StrategyError::InvalidDescriptor);
    }
    if descriptor.kind != kind {
        return Err(StrategyError::KindMismatch);
    }
    if descriptor.strategy_id == BASELINE_STRATEGY_ID {
        return Err(StrategyError::ReservedIdentifier);
    }
    Ok(descriptor.clone())
}

/// Everything a forecaster may look at. It is exactly the input of the
/// replayable baseline, so a replacement can be compared with it on the same
/// recorded evidence and nothing else: no ambient credential, no clock skew
/// and no reading the library did not admit as evidence.
pub struct ForecastRequest<'a> {
    pub snapshot: &'a QuotaSnapshot,
    pub attempts: &'a [LocalAttempt],
    pub assumptions: &'a HashMap<String, WindowAssumptions>,
    pub now: DateTime<Utc>,
    pub options: crate::forecast::ForecastOptions,
}

/// Everything a router may look at. Availability is the caller's actual
/// backend observation, never an inferred quota or permission state.
pub struct RouteRequest<'a> {
    pub policy: &'a QuotaPolicy,
    pub forecasts: &'a [QuotaForecast],
    pub availability: &'a HashMap<String, BackendState>,
    pub previous: Option<&'a PolicyDecision>,
    /// Measured useful work per admitted subject round, read from durable
    /// provenance. `None` is the honest unknown: a scenario then records no
    /// expectation instead of assuming one request is one useful outcome.
    /// A caller whose rounds are a different unit (a finite model child)
    /// leaves it `None` rather than projecting the subject's measurement.
    pub work: Option<&'a WorkEvidence>,
    pub now: DateTime<Utc>,
}

/// Everything a cadence strategy may look at: the recorded policy and
/// decision, the pending send and a read-only view of the durable pacer.
pub struct CadenceRequest<'a> {
    pub policy: &'a QuotaPolicy,
    pub decision: &'a PolicyDecision,
    pub request_id: EntityId,
    pub pacer: &'a Pacer,
    pub availability: &'a HashMap<String, BackendState>,
    pub now: DateTime<Utc>,
}

/// What a cadence strategy allows for one send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CadencePlan {
    /// No additional restriction: the durable state machine decides.
    Proceed,
    /// Withhold a *fresh* admission until `until` (or until new evidence
    /// arrives, when nothing is named). A plan never grants budget, never
    /// releases a reservation and never rewrites preserved history.
    Defer {
        until: Option<DateTime<Utc>>,
        reason: String,
    },
}

impl CadencePlan {
    pub fn defer(until: Option<DateTime<Utc>>, reason: impl Into<String>) -> Self {
        Self::Defer { until, reason: reason.into() }
    }
}

/// A forecaster strategy: one recorded reading in, one bounded forecast out.
pub trait QuotaForecaster: Send + Sync {
    fn descriptor(&self) -> StrategyDescriptor;
    fn forecast(&self, request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError>;
}

/// A router strategy: the recorded policy plus evidence in, one decision out.
pub trait QuotaRouter: Send + Sync {
    fn descriptor(&self) -> StrategyDescriptor;
    fn route(&self, request: RouteRequest<'_>) -> Result<PolicyDecision, StrategyError>;
}

/// A cadence strategy: it may withhold a fresh admission; it cannot grant,
/// release or downgrade one.
pub trait QuotaCadence: Send + Sync {
    fn descriptor(&self) -> StrategyDescriptor;
    fn plan(&self, request: CadenceRequest<'_>) -> Result<CadencePlan, StrategyError>;
}

/// The replayable rolling planning baseline of §42.5. It is the default for
/// every kind, and its identifier is reserved: an alternative algorithm must
/// declare its own name so old and new records stay distinguishable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollingBaseline;

static BASELINE: RollingBaseline = RollingBaseline;

fn baseline_descriptor(kind: StrategyKind) -> StrategyDescriptor {
    StrategyDescriptor {
        protocol_version: QUOTA_STRATEGY_PROTOCOL_VERSION,
        strategy_id: BASELINE_STRATEGY_ID.to_owned(),
        version: BASELINE_STRATEGY_VERSION.to_owned(),
        kind,
    }
}

impl QuotaForecaster for RollingBaseline {
    fn descriptor(&self) -> StrategyDescriptor {
        baseline_descriptor(StrategyKind::Forecast)
    }

    fn forecast(&self, request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError> {
        Ok(forecast(
            request.snapshot,
            request.attempts,
            request.assumptions,
            request.now,
            request.options,
        ))
    }
}

impl QuotaRouter for RollingBaseline {
    fn descriptor(&self) -> StrategyDescriptor {
        baseline_descriptor(StrategyKind::Route)
    }

    fn route(&self, request: RouteRequest<'_>) -> Result<PolicyDecision, StrategyError> {
        decide_with(
            request.policy,
            request.forecasts,
            request.availability,
            request.previous,
            request.work,
            request.now,
        )
        .map_err(|error| StrategyError::Refused { reason: bounded_reason(&error.to_string()) })
    }
}

impl QuotaCadence for RollingBaseline {
    fn descriptor(&self) -> StrategyDescriptor {
        baseline_descriptor(StrategyKind::Cadence)
    }

    fn plan(&self, _request: CadenceRequest<'_>) -> Result<CadencePlan, StrategyError> {
        Ok(CadencePlan::Proceed)
    }
}

/// The strategies installed on this node. Installing does not activate: a
/// strategy becomes effective only when it is selected, so a library user
/// cannot change admitted behavior by registering a plugin.
#[derive(Default)]
pub struct QuotaStrategyRegistry {
    forecasters: Vec<Box<dyn QuotaForecaster>>,
    routers: Vec<Box<dyn QuotaRouter>>,
    cadences: Vec<Box<dyn QuotaCadence>>,
    selected_forecast: Option<String>,
    selected_route: Option<String>,
    selected_cadence: Option<String>,
}

impl QuotaStrategyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one forecaster. A second strategy with the same identifier is
    /// refused: two implementations under one name would make a recorded
    /// `id@version` ambiguous.
    pub fn register_forecaster(
        &mut self,
        strategy: Box<dyn QuotaForecaster>,
    ) -> Result<(), StrategyError> {
        let descriptor = check_descriptor(&strategy.descriptor(), StrategyKind::Forecast)?;
        if self
            .forecasters
            .iter()
            .any(|installed| installed.descriptor().strategy_id == descriptor.strategy_id)
        {
            return Err(StrategyError::DuplicateStrategy);
        }
        self.forecasters.push(strategy);
        Ok(())
    }

    /// Install one router. Rules mirror [`Self::register_forecaster`].
    pub fn register_router(&mut self, strategy: Box<dyn QuotaRouter>) -> Result<(), StrategyError> {
        let descriptor = check_descriptor(&strategy.descriptor(), StrategyKind::Route)?;
        if self
            .routers
            .iter()
            .any(|installed| installed.descriptor().strategy_id == descriptor.strategy_id)
        {
            return Err(StrategyError::DuplicateStrategy);
        }
        self.routers.push(strategy);
        Ok(())
    }

    /// Install one cadence strategy. Rules mirror [`Self::register_forecaster`].
    pub fn register_cadence(
        &mut self,
        strategy: Box<dyn QuotaCadence>,
    ) -> Result<(), StrategyError> {
        let descriptor = check_descriptor(&strategy.descriptor(), StrategyKind::Cadence)?;
        if self
            .cadences
            .iter()
            .any(|installed| installed.descriptor().strategy_id == descriptor.strategy_id)
        {
            return Err(StrategyError::DuplicateStrategy);
        }
        self.cadences.push(strategy);
        Ok(())
    }

    /// Select the strategy of one kind that is actually used. Selecting an
    /// identifier that is not installed is refused instead of silently
    /// falling back to the baseline.
    pub fn select(
        &mut self,
        kind: StrategyKind,
        strategy_id: &str,
    ) -> Result<(), StrategyError> {
        let installed = match kind {
            StrategyKind::Forecast => self
                .forecasters
                .iter()
                .any(|installed| installed.descriptor().strategy_id == strategy_id),
            StrategyKind::Route => self
                .routers
                .iter()
                .any(|installed| installed.descriptor().strategy_id == strategy_id),
            StrategyKind::Cadence => self
                .cadences
                .iter()
                .any(|installed| installed.descriptor().strategy_id == strategy_id),
        };
        if !installed && strategy_id != BASELINE_STRATEGY_ID {
            return Err(StrategyError::UnknownStrategy);
        }
        let slot = match kind {
            StrategyKind::Forecast => &mut self.selected_forecast,
            StrategyKind::Route => &mut self.selected_route,
            StrategyKind::Cadence => &mut self.selected_cadence,
        };
        // Selecting the reserved baseline name means "use the built-in
        // baseline", which is also the default when nothing is selected.
        *slot = (strategy_id != BASELINE_STRATEGY_ID).then(|| strategy_id.to_owned());
        Ok(())
    }

    /// The selected identifier of one kind, or `None` for the built-in
    /// baseline.
    pub fn selected(&self, kind: StrategyKind) -> Option<&str> {
        match kind {
            StrategyKind::Forecast => self.selected_forecast.as_deref(),
            StrategyKind::Route => self.selected_route.as_deref(),
            StrategyKind::Cadence => self.selected_cadence.as_deref(),
        }
    }

    /// The effective forecaster: the selected one, else the baseline.
    pub fn forecaster(&self) -> &dyn QuotaForecaster {
        self.selected_forecast
            .as_ref()
            .and_then(|id| {
                self.forecasters
                    .iter()
                    .find(|installed| &installed.descriptor().strategy_id == id)
            })
            .map(|installed| installed.as_ref())
            .unwrap_or(&BASELINE)
    }

    /// The effective router: the selected one, else the baseline.
    pub fn router(&self) -> &dyn QuotaRouter {
        self.selected_route
            .as_ref()
            .and_then(|id| {
                self.routers
                    .iter()
                    .find(|installed| &installed.descriptor().strategy_id == id)
            })
            .map(|installed| installed.as_ref())
            .unwrap_or(&BASELINE)
    }

    /// The effective cadence strategy: the selected one, else the baseline.
    pub fn cadence(&self) -> &dyn QuotaCadence {
        self.selected_cadence
            .as_ref()
            .and_then(|id| {
                self.cadences
                    .iter()
                    .find(|installed| &installed.descriptor().strategy_id == id)
            })
            .map(|installed| installed.as_ref())
            .unwrap_or(&BASELINE)
    }

    /// Every installed strategy, effective or not, with the effective one
    /// first in its kind. This is what a node reports about itself: the
    /// subject reads it as a fact, not as a second policy source.
    pub fn installed(&self) -> Vec<StrategyDescriptor> {
        let mut rows = Vec::new();
        for kind in [StrategyKind::Forecast, StrategyKind::Route, StrategyKind::Cadence] {
            let selected = self.selected(kind);
            let mut descriptors: Vec<StrategyDescriptor> = match kind {
                StrategyKind::Forecast => self
                    .forecasters
                    .iter()
                    .map(|installed| installed.descriptor())
                    .collect(),
                StrategyKind::Route => {
                    self.routers.iter().map(|installed| installed.descriptor()).collect()
                }
                StrategyKind::Cadence => {
                    self.cadences.iter().map(|installed| installed.descriptor()).collect()
                }
            };
            descriptors.sort_by(|left, right| left.strategy_id.cmp(&right.strategy_id));
            let effective = match selected {
                Some(id) => descriptors.iter().find(|item| item.strategy_id == id).cloned(),
                // The baseline is not registered; it is always available.
                None => Some(baseline_descriptor(kind)),
            };
            if let Some(effective) = &effective {
                rows.push(effective.clone());
            }
            for descriptor in descriptors {
                if effective
                    .as_ref()
                    .is_some_and(|effective| effective.strategy_id == descriptor.strategy_id)
                {
                    continue;
                }
                rows.push(descriptor);
            }
        }
        rows
    }
}

/// The strategy set one runtime uses. Cloning shares the installed set;
/// [`QuotaStrategies::baseline`] is the zero-plugin configuration and keeps
/// exactly the pre-existing behavior.
#[derive(Clone, Default)]
pub struct QuotaStrategies {
    registry: Arc<QuotaStrategyRegistry>,
}

impl QuotaStrategies {
    /// The built-in baseline only: no plugin installed, no behavior change.
    ///
    /// The empty registry is shared: a node that installs no strategy clones
    /// one reference instead of rebuilding an empty strategy set for every
    /// admission.
    pub fn baseline() -> Self {
        static BASELINE_SET: std::sync::LazyLock<QuotaStrategies> =
            std::sync::LazyLock::new(|| QuotaStrategies {
                registry: Arc::new(QuotaStrategyRegistry::default()),
            });
        BASELINE_SET.clone()
    }

    /// Install a strategy set. Registering does not activate a strategy;
    /// [`QuotaStrategyRegistry::select`] does.
    pub fn from_registry(registry: QuotaStrategyRegistry) -> Self {
        Self { registry: Arc::new(registry) }
    }

    pub fn registry(&self) -> &QuotaStrategyRegistry {
        &self.registry
    }

    /// Every installed strategy and the effective one per kind.
    pub fn describe(&self) -> Vec<StrategyDescriptor> {
        self.registry.installed()
    }

    /// The effective cadence strategy. Used by the durable admission path,
    /// which applies the plan and keeps its own transition authoritative.
    pub fn cadence(&self) -> &dyn QuotaCadence {
        self.registry.cadence()
    }

    /// Forecast one recorded reading with the effective forecaster. The
    /// result names the strategy that produced it; a refusal is returned as
    /// such and never replaced by a guessed forecast.
    pub fn forecast(&self, request: ForecastRequest<'_>) -> Result<QuotaForecast, StrategyError> {
        let forecaster = self.registry.forecaster();
        let tag = forecaster.descriptor().tag();
        let mut produced = forecaster.forecast(request)?;
        produced.cost_model_version = Some(tag);
        Ok(produced)
    }

    /// Route one request with the effective router. The durable admission
    /// path still validates the decision against the recorded policy and
    /// forecasts, so a router cannot authorize an unsupported send.
    pub fn route(&self, request: RouteRequest<'_>) -> Result<PolicyDecision, StrategyError> {
        let router = self.registry.router();
        let tag = router.descriptor().tag();
        let mut produced = router.route(request)?;
        produced.strategy = Some(tag);
        Ok(produced)
    }

    /// Apply the effective cadence strategy around the durable admission
    /// transition. See [`crate::admission::admit_with_cadence`]: a plan can
    /// only withhold a fresh admission, never grant or rewrite one.
    pub fn admit(
        &self,
        policy: &QuotaPolicy,
        decision: &PolicyDecision,
        request_id: EntityId,
        pacer: &mut Pacer,
        availability: &HashMap<String, BackendState>,
        now: DateTime<Utc>,
    ) -> Result<AdmissionOutcome, StrategyError> {
        crate::admission::admit_with_cadence(
            policy,
            decision,
            request_id,
            pacer,
            availability,
            now,
            self.registry.cadence(),
        )
    }
}

