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

pub use server::*;
pub use routes::*;
pub use api::ApiState;
pub use ws::*;
pub use input::*;
pub use presentation::*;
pub use stream::*;
pub use playground::*;
pub use playground_backend::PlaygroundBackendHandler;
