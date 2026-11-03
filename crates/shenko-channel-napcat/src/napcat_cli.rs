//! napcat-cli as a SOURCE of the same QQ channel plugin — SPEC §16.
//!
//! [`NapCatCliSource`] polls the event feed of an external napcat-cli daemon
//! and ingests every event into the SAME durable ingress spool the WebSocket
//! connector uses. The spool's source identity is
//! `["napcat", account, "onebot_v11", conversation, message_id]`, so a message
//! observed by both transports is admitted exactly once, under the same QQ
//! plugin personality: one registry entry, one classification, one ledger.
//!
//! The wire shape is the documented one — `GET {base}/events?after=<seq>&limit=<n>`
//! answering `{"events": [{"seq": <n>, "event": {<OneBot v11 message>}}]}` with
//! monotonically increasing `seq` — and MUST be verified against the pinned
//! upstream napcat-cli (SPEC §15.4) before this source is enabled against a
//! real daemon. A shape mismatch is a typed [`CliSourceError`], never a
//! guessed parse.
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::client::OneBotEvent;
use crate::spool::IngressSpool;

/// Errors of the napcat-cli event source.
#[derive(Debug, thiserror::Error)]
pub enum CliSourceError {
    #[error("napcat-cli HTTP request failed: {0}")]
    Http(String),
    #[error("napcat-cli event feed shape mismatch: {0}")]
    Shape(String),
    #[error("napcat-cli ingress spool write failed: {0}")]
    Spool(String),
}

/// One page of the napcat-cli event feed.
#[derive(Debug, Deserialize)]
struct EventPage {
    events: Vec<SeqEvent>,
}

/// One sequenced event of the feed.
#[derive(Debug, Deserialize)]
struct SeqEvent {
    seq: i64,
    event: serde_json::Value,
}

/// Polls a napcat-cli daemon and feeds the shared ingress spool.
pub struct NapCatCliSource {
    base_url: String,
    account: u64,
    poll_interval: Duration,
}

impl NapCatCliSource {
    /// A poller for the daemon at `base_url` serving `account`.
    pub fn new(base_url: impl Into<String>, account: u64) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            account,
            poll_interval: Duration::from_millis(500),
        }
    }

    /// Run until `cancel` fires. Every event is spooled before it is forwarded,
    /// so a crash between spool and forward loses nothing: the next pass
    /// re-reads the same event and the spool dedup makes the repeat harmless.
    pub async fn run(
        self,
        spool: Arc<IngressSpool>,
        events: tokio::sync::mpsc::Sender<OneBotEvent>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), CliSourceError> {
        let client = reqwest::Client::new();
        // The cursor is process-local; the durable dedup lives in the spool.
        // A restart may re-ingest an already-spooled event, which the spool
        // collapses to the same row.
        let mut after: Option<i64> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(self.poll_interval) => {}
            }
            let mut url = format!("{}/events?limit=256", self.base_url);
            if let Some(after) = after {
                url.push_str(&format!("&after={after}"));
            }
            let response = client
                .get(&url)
                .send()
                .await
                .map_err(|error| CliSourceError::Http(error.to_string()))?;
            if !response.status().is_success() {
                return Err(CliSourceError::Http(format!(
                    "napcat-cli answered {}",
                    response.status()
                )));
            }
            let page: EventPage = response
                .json()
                .await
                .map_err(|error| CliSourceError::Shape(error.to_string()))?;
            for item in page.events {
                after = Some(item.seq);
                let raw = item.event;
                // Only OneBot message events enter the subject inbox; the
                // spool keeps everything else as raw evidence either way.
                let spooled = spool
                    .ingest(raw.clone())
                    .map_err(|error| CliSourceError::Spool(error.to_string()))?;
                let Some(spooled) = spooled else {
                    continue; // duplicate of an event the WS path already spooled
                };
                let event = OneBotEvent {
                    ingress_id: Some(spooled.spool_id),
                    post_type: raw
                        .get("post_type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("message")
                        .to_string(),
                    time: raw
                        .get("time")
                        .and_then(|value| value.as_i64())
                        .or_else(|| {
                            raw.get("time")
                                .and_then(|value| value.as_u64())
                                .and_then(|seconds| i64::try_from(seconds).ok())
                        }),
                    self_id: Some(self.account),
                    data: raw,
                };
                if events.send(event).await.is_err() {
                    // The admission loop is gone; spooled events stay pending
                    // and are re-driven by the acknowledgement mirror.
                    return Ok(());
                }
            }
        }
    }
}
