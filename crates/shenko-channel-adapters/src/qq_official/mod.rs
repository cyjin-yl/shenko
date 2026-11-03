//! The official QQ bot channel adapter — SPEC §15.3, §16, §35.6.
//!
//! The platform speaks a WebSocket gateway (`v2/events`) with `OPCODE 13`
//! intents and an HTTPS OpenAPI v2. Every identity is a per-application
//! OpenID; the numeric QQ number is not available to an official bot and
//! this adapter never pretends otherwise.

pub mod api;
pub mod config;
pub mod events;
pub mod gateway;
pub mod plugin;

pub use config::QqOfficialConfig;
pub use plugin::QqOfficialChannel;
