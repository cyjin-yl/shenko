pub mod broker;
pub mod admission;
pub mod request;
pub mod provider;
pub mod local_provider;

/// The only response-header family a quota observer may ever read. Kept as
/// one constant so a provider cannot accidentally leak unrelated headers,
/// credentials or gateway extensions into observation evidence.
pub(crate) const QUOTA_HEADER_PREFIX: &str = "anthropic-ratelimit-";

pub use broker::*;
pub use admission::*;
pub use request::*;
pub use provider::*;
pub use local_provider::*;
