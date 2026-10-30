//! Shenko WebUI — axum server. SPEC §21, §43.
//! Single subject timeline, streaming output, configuration panel.
pub mod server;
pub mod routes;
pub mod api;
pub mod ws;
pub mod playground;
pub mod playground_backend;

pub use server::*;
pub use routes::*;
pub use api::ApiState;
pub use ws::*;
pub use playground::*;
pub use playground_backend::PlaygroundBackendHandler;
