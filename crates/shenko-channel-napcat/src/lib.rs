//! QQ/NapCat channel plugin — SPEC §16, §35.6, §41.6.
//!
//! The plugin owns everything QQ-specific: the OneBot v11 wire protocol, the
//! numeric account ids, the reply audience kinds, the attachment segments and
//! the acceptance predicate of a send. The durable intent, attempt and
//! receipt live in the shared Store, so every channel uses one ledger.
pub mod attachments;
pub mod channel;
pub mod client;
pub mod config;
pub mod napcat_cli;
pub mod outbound;
pub mod spool;

pub use attachments::ParsedMessage;
pub use shenko_types::channel::{AttachmentError, ResolvedAttachment, MAX_ATTACHMENT_BYTES};
pub use channel::{
    qq_registry, qq_test_registry, NapCatChannel, QQ_CHANNEL_ID, QQ_REPLY_KINDS, QQ_SOURCE,
    QQ_TRANSPORT_IDENTITY_DOMAIN,
};
pub use client::{
    endpoint_provenance, ContactQuery, NapCatClient, OneBotAction, OneBotEvent, QueryReceipt,
    SendError,
};
pub use config::NapCatConfig;
pub use napcat_cli::{CliSourceError, NapCatCliSource};
pub use outbound::{OutboundMessage, build_outbound, downgrade_markdown};
pub use spool::*;

#[cfg(test)]
mod capability_tests;
#[cfg(test)]
mod notice_tests;
