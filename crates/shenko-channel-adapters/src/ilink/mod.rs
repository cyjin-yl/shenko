//! The WeChat iLink bot channel adapter — docs/specs/channel-adapters.md#spec-15-3, docs/specs/channel-adapters.md#spec-16, docs/specs/channels.md#spec-35-6.
//!
//! The protocol is a long-poll `getupdates` ingress with a per-conversation
//! `context_token`, and a CDN that stores media encrypted with AES-128-ECB.
//! It has no edit, no retract and no typing; this adapter serves a direct
//! conversation only and does not advertise a group audience.

pub mod api;
pub mod auth;
pub mod config;
pub mod events;
pub mod media;
pub mod monitor;
pub mod plugin;

pub use auth::{ILinkCredentials, QrLogin, QrStatus};
pub use config::ILinkConfig;
pub use plugin::ILinkChannel;
