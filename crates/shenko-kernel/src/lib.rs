//! Shenko kernel — minimal core: plugin lifecycle, service binding, epoch/cancel/capability handles.
//! Uses cordis-rs as the plugin backbone (user-confirmed in SPEC §7, U45).
//! The kernel does NOT embed any business logic — that lives in plugins.
pub mod plugin;
pub mod service;
pub mod capability;
pub mod lifecycle;
pub mod supervisor;

pub use plugin::*;
pub use service::*;
pub use capability::*;
pub use lifecycle::*;
pub use supervisor::*;
