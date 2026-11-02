//! The Feishu/Lark open-platform channel adapter — SPEC §15.3, §16, §35.6.
//!
//! The platform speaks a WebSocket event dispatcher (`wss://…/callback/ws`)
//! for ingress and an HTTPS open API for egress, authenticated with an
//! app ticket (`app_id` + `app_secret`). Messages are built from typed
//! blocks and carry an edit and a recall endpoint.

pub mod api;
pub mod auth;
pub mod config;
pub mod events;
pub mod media;
pub mod monitor;
pub mod plugin;

pub use auth::{AppTicket, TenantToken};
pub use config::FeishuConfig;
pub use plugin::FeishuChannel;
