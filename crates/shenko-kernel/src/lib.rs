//! Shenko kernel — minimal core: plugin lifecycle, service binding, epoch/cancel/capability handles.
//! Uses cordis-rs as the plugin backbone (user-confirmed in SPEC §7, U45).
//! The kernel does NOT embed any business logic — that lives in plugins.
pub mod plugin;
pub mod capability;
pub mod lifecycle;
pub mod supervisor;
pub mod hooks;
pub mod effect_contract;

pub use plugin::*;
pub use capability::*;
pub use lifecycle::*;
pub use supervisor::*;
pub use hooks::*;
pub use effect_contract::*;

// These are the actual upstream contracts, not Shenko aliases or adapters.
pub use cordis_core::{Context, FiberHandle, InjectSpec, Plugin, PreparedPlugin, Service};
