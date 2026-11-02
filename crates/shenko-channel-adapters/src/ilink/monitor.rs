//! The iLink long-poll monitor — SPEC §16, §15.3, §35.6.
//!
//! Inbound messages are pulled with `POST /ilink/bot/getupdates`. The cursor
//! is the provider's own `get_updates_buf`; it is handed back unchanged and
//! persisted after every poll, so a restart continues from the exact cursor
//! instead of re-reading a window. A non-zero `ret` of `-14` pauses the
//! account for an hour, matching the provider's own behaviour.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::common::{HealthState, ReplyContext, ReplyContextCache};
use crate::InboundEvent;

use super::api::{ILinkApi, ILinkError, Updates};
use super::config::ILinkConfig;
use super::events::{ILINK_CHANNEL_ID, classify, context_token};

/// The durable cursor of one iLink account.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MonitorCursor {
    /// The provider's `get_updates_buf`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub buf: String,
}

/// Where one account's monitor cursor is stored.
pub fn cursor_path(data_dir: &Path, account: &str) -> PathBuf {
    data_dir.join(format!("ilink-monitor-{account}.json"))
}

/// Read a stored cursor; a missing or unreadable file means "no cursor".
pub async fn load_cursor(path: &Path) -> MonitorCursor {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => MonitorCursor::default(),
    }
}

/// Write the cursor so a restart continues exactly.
pub async fn store_cursor(path: &Path, cursor: &MonitorCursor) -> Result<(), String> {
    let text = serde_json::to_string_pretty(cursor)
        .map_err(|error| format!("ilink cursor encode failed: {error}"))?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("ilink cursor directory failed: {error}"))?;
    }
    tokio::fs::write(path, text)
        .await
        .map_err(|error| format!("ilink cursor write failed: {error}"))
}

/// The iLink long-poll monitor.
pub struct ILinkMonitor {
    config: Arc<ILinkConfig>,
    api: ILinkApi,
    health: HealthState,
    replies: ReplyContextCache,
    cursor_path: PathBuf,
}

impl ILinkMonitor {
    /// Build a monitor for one iLink account.
    pub fn new(
        config: Arc<ILinkConfig>,
        api: ILinkApi,
        health: HealthState,
        replies: ReplyContextCache,
        cursor_path: PathBuf,
    ) -> Self {
        Self {
            config,
            api,
            health,
            replies,
            cursor_path,
        }
    }

    /// Run the monitor until the runtime asks it to stop.
    pub async fn run(
        &self,
        sender: mpsc::Sender<InboundEvent>,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if let Err(error) = self.api.notify_start().await {
            self.health
                .transition("degraded", Some(format!("notifyStart failed: {error}")))
                .await;
            return Err(error.to_string());
        }
        let mut cursor = load_cursor(&self.cursor_path).await;
        let mut backoff = Duration::from_secs(1);
        loop {
            if cancel.is_cancelled() {
                self.health.transition("stopped", None).await;
                let _ = self.api.notify_stop().await;
                return Ok(());
            }
            self.health
                .transition("connecting", Some("polling for updates".into()))
                .await;
            match self.api.get_updates(&cursor.buf).await {
                Ok(updates) => {
                    backoff = Duration::from_secs(1);
                    // The provider suggests how long to hold the next poll.
                    let hold = Duration::from_millis(updates.long_poll_timeout_ms);
                    self.handle(&updates, &sender).await;
                    if let Some(next) = &updates.next_cursor {
                        cursor.buf = next.clone();
                        if let Err(error) = store_cursor(&self.cursor_path, &cursor).await {
                            self.health.transition("degraded", Some(error)).await;
                        }
                    }
                    self.health.transition("ready", None).await;
                    // Honour the provider's hold before polling again.
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            self.health.transition("stopped", None).await;
                            let _ = self.api.notify_stop().await;
                            return Ok(());
                        }
                        _ = tokio::time::sleep(hold) => {}
                    }
                    continue;
                }
                Err(error) => {
                    self.health.transition("degraded", Some(error.to_string())).await;
                    if pause_for_session_expiry(&error) {
                        backoff = Duration::from_secs(3_600);
                    }
                }
            }
            let wait = backoff;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.health.transition("stopped", None).await;
                    let _ = self.api.notify_stop().await;
                    return Ok(());
                }
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Classify and forward one poll's messages.
    async fn handle(&self, updates: &Updates, sender: &mpsc::Sender<InboundEvent>) {
        for message in &updates.messages {
            let Some(envelope) = classify(&self.config, message) else {
                continue;
            };
            self.health.observe_event().await;
            let token = context_token(message);
            if let Some(target) = &envelope.reply_target {
                if let Some(conversation) = &target.conversation {
                    self.replies.record(
                        conversation,
                        ReplyContext {
                            message_id: envelope.message_id.clone(),
                            context_token: token.clone(),
                            thread_id: None,
                            at: chrono::Utc::now(),
                        },
                    );
                }
            }
            let event = InboundEvent {
                envelope,
                raw: message.clone(),
                source: ILINK_CHANNEL_ID.to_string(),
            };
            if sender.send(event).await.is_err() {
                tracing::warn!("ilink ingress closed; the message stays pending");
                return;
            }
        }
    }
}

/// Whether the provider's `ret == -14` told the account to pause for an hour.
fn pause_for_session_expiry(error: &ILinkError) -> bool {
    match error {
        ILinkError::Rejected { ret, .. } => ret == &Some(-14),
        _ => false,
    }
}
