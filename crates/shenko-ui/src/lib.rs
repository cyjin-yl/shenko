//! Shenko WebUI — axum server. SPEC §21, §43.
//! Single subject timeline, streaming output, configuration panel.
pub mod server;
pub mod routes;
pub mod api;
pub mod media;
pub mod ws;
pub mod input;
pub mod presentation;
pub mod stream;
pub mod playground;
pub mod playground_backend;
pub mod playground_manage;
pub mod config_service;
pub mod config_namespaces;
pub mod config_task;
pub mod owner;

pub use server::*;
pub use routes::*;
pub use api::ApiState;
pub use ws::*;
pub use input::*;
pub use presentation::*;
pub use stream::*;
pub use playground::*;
pub use playground_backend::PlaygroundBackendHandler;
pub use config_service::ConfigService;
pub use config_task::ConfigHandler;
pub use owner::{OwnerState, AuthenticatedActor};
