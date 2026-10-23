//! # shenko-channel-napcat-link
//!
//! Optional NapCatQQ transport backend for Shenko (SPEC §18.1 ecosystem
//! compatibility), implemented on top of the [`napcat-link`] crate — a modern
//! Rust SDK for NapCatQQ's OneBot 11 WebSocket protocol.
//!
//! The crate exposes one small provider interface, [`ChannelTransport`], so the
//! rest of Shenko can depend on it without knowing whether the upstream SDK is
//! compiled in. The real implementation lives behind the `napcat-link` feature;
//! the default build is a compilable stub that fails clearly at runtime.
//!
//! ## Probe result for napcat-link 0.1.0-alpha.2
//!
//! Probed on 2026-10-23 by fetching the crate into a scratch project
//! (`/tmp/napcat-probe`) and reading `~/.cargo/registry/src/.../napcat-link-0.1.0-alpha.2`.
//!
//! * Fetch/compile: **success** (`cargo check` on the scratch crate with the
//!   dependency finished cleanly; ~140 transitive deps, MIT licensed, edition 2024).
//! * Public API actually present in the SDK:
//!   * `NapLink::builder(url: &str) -> NapLinkBuilder`; builder methods
//!     `.token(&str)`, `.connection_timeout_ms(u64)`, `.ping_interval_ms(u64)`,
//!     `.reconnect_enabled(bool)`, `.reconnect_max_attempts(u32)`,
//!     `.backoff_initial_ms/max_ms(u64)`, `.backoff_multiplier(f64)`,
//!     `.log_level(LogLevel)`, `.api_timeout_ms(u64)`, `.api_retries(u32)`,
//!     and `.build() -> napcat_link::Result<NapLink>`.
//!   * `NapLink::connect() -> Result<()>`, `.disconnect()`,
//!     `.state() -> ConnectionState`, `.is_connected() -> bool`,
//!     `.call_api(&str, serde_json::Value) -> Result<Value>`.
//!   * `NapLink::subscribe() -> EventSubscription`, with
//!     `EventSubscription::recv() -> Option<EventData>` (also `recv_filter`,
//!     `recv_exact`). `EventData { name: String, data: serde_json::Value }` —
//!     `data` is the raw OneBot 11 event JSON.
//!   * `NapLink::api() -> &OneBotApi`; `OneBotApi.message` is a `MessageApi`
//!     with `send_group_message(group_id: i64, message: Vec<MessageSegment>)`
//!     and `send_private_message(user_id: i64, message: Vec<MessageSegment>)`,
//!     both `async fn ... -> napcat_link::Result<serde_json::Value>`.
//!   * `napcat_link::MessageSegment::text(impl Into<String>)` builds text segments.
//!
//! The SDK is alpha and its API may change without notice, which is why it is
//! opt-in and pinned in this crate's manifest.

use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

/// Callback invoked for every raw OneBot 11 event received from the transport.
///
/// The [`Value`] is the SDK's `EventData::data`, i.e. the untouched OneBot 11
/// event object (`post_type`, `message_type`, `raw_message`, ...).
pub type EventHandler = Arc<dyn Fn(Value) + Send + Sync>;

/// Connection settings needed to reach a NapCatQQ WebSocket endpoint.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// WebSocket URL, e.g. `ws://127.0.0.1:3001`.
    pub url: String,
    /// OneBot access token, if the NapCatQQ endpoint requires one.
    pub token: Option<String>,
}

impl TransportConfig {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

/// Provider interface for a QQ transport backend.
///
/// Deliberately narrow: the transport feeds raw OneBot 11 JSON upward and
/// accepts plain-text sends downward. Message segmentation and policy live in
/// the channel layer, not here.
#[async_trait::async_trait]
pub trait ChannelTransport: Send + Sync {
    /// Connect and pump events into `on_event` until the connection ends.
    ///
    /// Returns `Ok(())` only when the event stream closes cleanly; transport
    /// failures surface as errors.
    async fn run(&self, on_event: EventHandler) -> Result<()>;

    /// Send a plain-text group message.
    async fn send_group(&self, group_id: i64, message: &str) -> Result<()>;

    /// Send a plain-text private message.
    async fn send_private(&self, user_id: i64, message: &str) -> Result<()>;
}

/// Open a transport for `config` using whatever backend is compiled in.
///
/// With the `napcat-link` feature, this is the real SDK transport; without it,
/// a stub that errors on first use.
pub fn open(config: TransportConfig) -> Result<Box<dyn ChannelTransport>> {
    #[cfg(feature = "napcat-link")]
    {
        Ok(Box::new(NapCatLinkTransport::new(config)?))
    }
    #[cfg(not(feature = "napcat-link"))]
    {
        Ok(Box::new(StubTransport { config }))
    }
}

/// Error text used by the stub and returned when the feature is not compiled in.
pub const FEATURE_DISABLED: &str = "napcat-link feature disabled: rebuild shenko-channel-napcat-link with `--features napcat-link` to use the real NapCatLink transport";

/// Placeholder transport compiled when the `napcat-link` feature is off.
///
/// It holds the configuration so callers can construct it unconditionally, and
/// fails every operation with a clear diagnostic instead of pretending to work.
#[derive(Debug)]
pub struct StubTransport {
    config: TransportConfig,
}

impl StubTransport {
    pub fn new(config: TransportConfig) -> Self {
        Self { config }
    }

    /// The configuration the stub was built with.
    pub fn config(&self) -> &TransportConfig {
        &self.config
    }
}

#[async_trait::async_trait]
impl ChannelTransport for StubTransport {
    async fn run(&self, _on_event: EventHandler) -> Result<()> {
        anyhow::bail!(FEATURE_DISABLED)
    }

    async fn send_group(&self, _group_id: i64, _message: &str) -> Result<()> {
        anyhow::bail!(FEATURE_DISABLED)
    }

    async fn send_private(&self, _user_id: i64, _message: &str) -> Result<()> {
        anyhow::bail!(FEATURE_DISABLED)
    }
}

#[cfg(feature = "napcat-link")]
pub use napcat_link_impl::NapCatLinkTransport;

#[cfg(feature = "napcat-link")]
mod napcat_link_impl {
    use super::{ChannelTransport, EventHandler, Result, TransportConfig};
    use anyhow::{Context, anyhow};
    use napcat_link::{MessageSegment, NapLink};
    use std::sync::Arc;

    /// Real transport backed by the `napcat-link` SDK.
    ///
    /// The SDK client is created up front (so `send_*` can be used as soon as
    /// `run()` has connected) and reused for the lifetime of the transport.
    pub struct NapCatLinkTransport {
        client: Arc<NapLink>,
    }

    impl NapCatLinkTransport {
        /// Build the SDK client. This does not connect yet; `run()` does.
        pub fn new(config: TransportConfig) -> Result<Self> {
            let mut builder = NapLink::builder(&config.url);
            if let Some(token) = &config.token {
                builder = builder.token(token);
            }
            let client = builder
                .build()
                .map_err(|e| anyhow!("failed to build NapLink client: {e}"))?;
            Ok(Self {
                client: Arc::new(client),
            })
        }

        /// Access to the underlying SDK client for extension APIs not covered
        /// by [`ChannelTransport`].
        pub fn client(&self) -> &NapLink {
            &self.client
        }

        fn text_segments(message: &str) -> Vec<MessageSegment> {
            vec![MessageSegment::text(message)]
        }
    }

    #[async_trait::async_trait]
    impl ChannelTransport for NapCatLinkTransport {
        async fn run(&self, on_event: EventHandler) -> Result<()> {
            self.client
                .connect()
                .await
                .map_err(|e| anyhow!("NapCatLink connect failed: {e}"))?;
            tracing::info!(
                connected = self.client.is_connected(),
                "napcat-link transport connected"
            );

            let mut subscription = self.client.subscribe();
            while let Some(event) = subscription.recv().await {
                on_event(event.data);
            }
            Err(anyhow!("napcat-link event stream closed"))
        }

        async fn send_group(&self, group_id: i64, message: &str) -> Result<()> {
            self.client
                .api()
                .message
                .send_group_message(group_id, Self::text_segments(message))
                .await
                .map(|_| ())
                .with_context(|| format!("send_group_msg to group {group_id} failed"))
        }

        async fn send_private(&self, user_id: i64, message: &str) -> Result<()> {
            self.client
                .api()
                .message
                .send_private_message(user_id, Self::text_segments(message))
                .await
                .map(|_| ())
                .with_context(|| format!("send_private_msg to user {user_id} failed"))
        }
    }
}
