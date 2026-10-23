//! Shenko WebUI — axum server. SPEC §21, §43.
//! Single subject timeline, streaming output, configuration panel.
pub mod server;
pub mod routes;
pub mod ws;

pub use server::*;
pub use routes::*;
pub use ws::*;
