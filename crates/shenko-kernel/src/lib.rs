//! Shenko kernel — minimal core: plugin lifecycle, service binding, epoch/cancel/capability handles.
//! Uses cordis-rs as the plugin backbone (user-confirmed, U45).
//! The kernel does NOT embed any business logic — that lives in plugins.
pub mod contribution;
pub mod errors;
pub mod hooks;
pub mod lifecycle;
pub mod native_artifact;
pub mod plugin;
pub mod preparation;
pub mod runtime;
pub mod services;
pub mod supervisor;
pub mod task;

pub use contribution::*;
pub use errors::*;
pub use hooks::*;
pub use lifecycle::*;
pub use plugin::*;
pub use runtime::*;
pub use services::*;
pub use supervisor::*;
pub use task::*;

// These are the actual upstream contracts, not Shenko aliases or adapters.
pub use cordis_core::{Context, FiberHandle, InjectSpec, Plugin, PreparedPlugin, Service};
