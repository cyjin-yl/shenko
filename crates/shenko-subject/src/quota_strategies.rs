//! Where the replaceable quota strategies live on a node — docs/specs/quota-resources.md#spec-42-1.
//!
//! docs/specs/quota-resources.md#spec-42-1 makes the forecaster, the routing policy and the admission cadence
//! replaceable plugins. The algorithms themselves are in [`shenko_quota`];
//! this module is the Cordis seam that publishes the *installed set* on the
//! runtime's root context, so a subject step, a finite model child and the
//! intervention worker all decide with the same strategy set instead of
//! each hard-coding one.
//!
//! Nothing here selects a strategy by model name, provider or keyword. The
//! set is installed by the assembly root; an absent publication is the
//! built-in rolling baseline of docs/specs/quota-resources.md#spec-42-5, which is exactly the behavior the
//! runtime had before the seam existed. A strategy that was installed but
//! not selected changes nothing, and a selected strategy that refuses is a
//! typed reconciliation fact — never a fabricated forecast, decision or
//! admission.

use std::convert::Infallible;
use std::sync::Arc;

use cordis_core::{Context, Plugin, Service};
use shenko_quota::{QuotaStrategies, StrategyDescriptor};

/// The Cordis service name of the installed quota strategy set.
pub const QUOTA_STRATEGY_SERVICE: &str = "shenko.quota.strategies/v1";

/// The published strategy set. It is a typed capability, not a second
/// registry: registration, selection and withdrawal happen on the real
/// Cordis lifecycle.
pub struct QuotaStrategyService {
    strategies: QuotaStrategies,
}

impl Service for QuotaStrategyService {
    const NAME: &'static str = QUOTA_STRATEGY_SERVICE;
}

impl QuotaStrategyService {
    pub fn new(strategies: QuotaStrategies) -> Self {
        Self { strategies }
    }

    pub fn strategies(&self) -> &QuotaStrategies {
        &self.strategies
    }

    /// The effective strategy of each kind, for diagnostics and for the
    /// runtime's own report of what it is running.
    pub fn describe(&self) -> Vec<StrategyDescriptor> {
        self.strategies.describe()
    }
}

/// Publishes one strategy set on the runtime root.
pub struct QuotaStrategyPlugin;

impl Plugin for QuotaStrategyPlugin {
    type Config = QuotaStrategies;
    type Input = Arc<QuotaStrategyService>;
    type PrepareError = Infallible;
    type ApplyError = cordis_core::service::ServicePublishError;

    fn name(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed("shenko-quota-strategies")
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(Arc::new(QuotaStrategyService::new(config)))
    }

    async fn apply(&self, ctx: Context, service: &Self::Input) -> Result<(), Self::ApplyError> {
        let _publication = ctx.provide(service.clone())?;
        Ok(())
    }
}

/// The strategy set a caller must decide with: the published one when the
/// runtime installed it, otherwise the built-in baseline. A missing
/// publication is not an error and never changes behavior; it means this
/// node runs the replayable docs/specs/quota-resources.md#spec-42-5 baseline.
pub fn installed(ctx: &Context) -> QuotaStrategies {
    ctx.try_service::<QuotaStrategyService>()
        .map(|service| service.strategies().clone())
        .unwrap_or_else(|_| QuotaStrategies::baseline())
}
