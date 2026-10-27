//! Provider quota observation and forecasting — SPEC §42.2–42.5 (R130, B42).
//!
//! Observers turn what a provider actually exposes into [`QuotaSnapshot`]s
//! without calling any model; the forecaster derives planning budgets from
//! them. Every unit, account and window stays separate, and anything that is
//! not observed stays unknown: a missing window is never "full", a failed
//! query is never "exhausted", an API token-bucket refill is never a
//! subscription reset. Policy, admission pacing and routing build on this and
//! are separate, replaceable pieces.
pub mod forecast;
pub mod model;
pub mod observe;

pub use forecast::*;
pub use model::*;
pub use observe::*;
