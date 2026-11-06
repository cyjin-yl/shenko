pub mod broker;
pub mod admission;
mod execution;
pub mod error_sink;
pub mod request;
pub mod provider;
pub mod local_provider;
pub mod anthropic_rules;
pub mod capability;
pub mod kv;
pub mod native_provider;
pub mod policy;
pub mod profile;
pub mod registry;
pub mod resource;
pub mod stream;

/// The only response-header family a quota observer may ever read. Kept as
/// one constant so a provider cannot accidentally leak unrelated headers,
/// credentials or gateway extensions into observation evidence.
pub(crate) const QUOTA_HEADER_PREFIX: &str = "anthropic-ratelimit-";

pub use broker::*;
pub use admission::*;
pub use request::*;
pub use provider::*;
pub use local_provider::*;
pub use anthropic_rules::*;
pub use capability::*;
pub use error_sink::*;
pub use kv::*;
pub use native_provider::*;
pub use policy::*;
pub use profile::*;
pub use registry::*;
pub use resource::*;
pub use stream::*;
