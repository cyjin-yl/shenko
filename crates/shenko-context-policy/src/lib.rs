//! Replaceable ContextPolicy contract — SPEC §12.3.1–12.3.2.
//!
//! Strategies (budget compact, snapcompact, others) live in their own plugins
//! and only implement [`ContextPolicy`]. This crate owns what they share: stable
//! history references, the prepare → publish → commit window transaction,
//! unfinished-call preservation, availability declarations and cost reports.
//! A window changes what the model sees next, never the raw history: every
//! committed window keeps its source refs and derivatives, so it stays readable
//! after the plugin that produced it is updated or uninstalled.
//!
//! The preparation is split so the slow part stays off the request path:
//! [`registry::PolicyRegistry::prepare_candidate`] reads the history and
//! renders the new representation, [`registry::PolicyRegistry::publish`]
//! binds the result to the live revisions, and
//! [`registry::PolicyRegistry::adopt_prepared`] is the only thing a request
//! path calls.
//!
//! [`contribution`] holds the prepared transient context contribution
//! contract: material a plugin prepared for one request, with its producer,
//! scope, expiry and the evidence of what was actually sent.
pub mod contribution;
pub mod effective_window;
pub mod history;
pub mod policy;
pub mod preparation;
pub mod registry;
#[cfg(feature = "store")]
pub mod store_history;
pub mod window;

pub use contribution::*;
pub use effective_window::*;
pub use history::*;
pub use policy::*;
pub use preparation::*;
pub use registry::*;
#[cfg(feature = "store")]
pub use store_history::*;
pub use window::*;
