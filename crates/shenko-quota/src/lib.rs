//! Provider quota observation and forecasting — SPEC §42.2–42.6 (R130, B42).
//!
//! Observers turn what a provider actually exposes into [`QuotaSnapshot`]s
//! without calling any model; the forecaster derives planning budgets from
//! them. Every unit, account and window stays separate, and anything that is
//! not observed stays unknown: a missing window is never "full", a failed
//! query is never "exhausted", an API token-bucket refill is never a
//! subscription reset. Policy, admission pacing and routing build on this and
//! are separate, replaceable pieces: [`decide`] chooses a profile and pacing
//! from forecasts, [`admit`] applies it to one request, and a capacity block
//! is never reported as a quota state.
//!
//! The runtime surface is [`plugin`]: named providers register versioned
//! observers ([`sources::anthropic`], [`sources::local_meter`]) that read one
//! real source payload. [`scope`] names the real account a reading belongs
//! to — an organization, a workspace, the Default Workspace, one API key or a
//! consumer subscription — and keeps credentials on the node, out of the
//! ledger. [`event`] carries the durable events of §42.6.
//!
//! [`view`] is the subject-readable projection of a recorded reading: the
//! remaining range, the expected recovery, the freshness and sharing of the
//! source and the model the current policy decision selected. It renders
//! what the observation supports and never becomes a source itself.
pub mod admission;
pub mod event;
pub mod forecast;
pub mod model;
pub mod observe;
pub mod plugin;
pub mod policy;
pub mod scope;
pub mod sources;
pub mod view;

pub use admission::*;
pub use event::*;
pub use forecast::*;
pub use model::*;
pub use observe::*;
pub use plugin::*;
pub use policy::*;
pub use scope::*;
pub use sources::*;
pub use view::*;
