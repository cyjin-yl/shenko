//! Real NapCat/OneBot v11 WebSocket client — SPEC §16, §35.6
//! Connects to NapCat WS endpoint, receives CQ events, sends messages.
//!
//! Outbound calls are correlated by `echo` so a caller learns the provider's
//! actual status/retcode. A missing response is an *unknown* outcome: the
//! request may or may not have been executed, so callers must reconcile
//! instead of blindly resending (SPEC §35.6, §41.4 X03).
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info, warn};
use url::Url;

use crate::config::NapCatConfig;
use crate::spool::{IngressSpool, SpooledEvent};
use shenko_types::ids::EntityId;

/// How long an outbound action waits for its `echo` response before the
/// outcome is declared unknown. SPEC §35.6.
pub const SEND_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Pending outbound actions, keyed by echo token.
pub type PendingActions =
    Arc<parking_lot::Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>;

/// OneBot v11 event from NapCat
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OneBotEvent {
    /// 由接收库赋值，不接受来自平台正文的伪造 ID；供控制 ledger 关联。
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub ingress_id: Option<EntityId>,
    pub post_type: String,
    pub time: Option<i64>,
    pub self_id: Option<u64>,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

/// Outgoing message action
#[derive(Debug, Clone, Serialize)]
pub struct OneBotAction {
    pub action: String,
    pub params: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<String>,
}

/// Provider response to one outbound action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendReceipt {
    pub echo: String,
    /// OneBot `status`: `ok`, `failed` or `async`.
    pub status: String,
    pub retcode: i64,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub message: Option<String>,
}

impl SendReceipt {
    pub fn is_success(&self) -> bool {
        self.retcode == 0 && self.status != "failed"
    }
}

/// Why an outbound call did not produce a definite answer.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error("napcat action channel closed — connector not running")]
    Disconnected,
    /// The request may have reached the provider. Never auto-retry.
    #[error("unknown outcome: {0}")]
    Unknown(String),
}

#[derive(Clone)]
pub struct NapCatClient {
    pub config: NapCatConfig,
    event_tx: mpsc::Sender<OneBotEvent>,
    action_tx: mpsc::Sender<OneBotAction>,
    /// Echo token → waiter. Shared with `run_connection_loop`.
    pub pending_actions: PendingActions,
    pub spool: Arc<IngressSpool>,
}

impl std::fmt::Debug for NapCatClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NapCatClient")
            .field("config", &self.config)
            .field("spool_pending", &self.spool.pending_count())
            .field("pending_actions", &self.pending_actions.lock().len())
            .finish()
    }
}

impl NapCatClient {
    /// 兼容临时内存模式；生产调用 open_persistent。
    pub fn new(
        config: NapCatConfig,
    ) -> (
        Self,
        mpsc::Receiver<OneBotEvent>,
        mpsc::Receiver<OneBotAction>,
    ) {
        Self::with_spool(config.clone(), Arc::new(IngressSpool::new(config.bot_qq)))
    }

    /// 生产显式选择独立持久接收库；打开失败直接返回，不退回内存。
    pub fn open_persistent(
        config: NapCatConfig,
        path: impl AsRef<std::path::Path>,
    ) -> anyhow::Result<(
        Self,
        mpsc::Receiver<OneBotEvent>,
        mpsc::Receiver<OneBotAction>,
    )> {
        let spool = Arc::new(IngressSpool::open(path, config.bot_qq)?);
        Ok(Self::with_spool(config, spool))
    }

    fn with_spool(
        config: NapCatConfig,
        spool: Arc<IngressSpool>,
    ) -> (
        Self,
        mpsc::Receiver<OneBotEvent>,
        mpsc::Receiver<OneBotAction>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(256);
        let (action_tx, action_rx) = mpsc::channel(256);
        (
            Self {
                config,
                event_tx,
                action_tx,
                pending_actions: Arc::new(parking_lot::Mutex::new(HashMap::new())),
                spool,
            },
            event_rx,
            action_rx,
        )
    }

    /// Sender for inbound events; hand to `run_connection_loop`.
    pub fn event_sender(&self) -> mpsc::Sender<OneBotEvent> {
        self.event_tx.clone()
    }

    /// Send a private message via NapCat, generating a fresh echo token.
    pub async fn send_private_message(
        &self,
        user_id: u64,
        message: &str,
    ) -> Result<SendReceipt, SendError> {
        let echo = uuid::Uuid::new_v4().to_string();
        self.send_private_message_with_echo(user_id, message, echo)
            .await
    }

    /// Send a group message via NapCat, generating a fresh echo token.
    pub async fn send_group_message(
        &self,
        group_id: u64,
        message: &str,
    ) -> Result<SendReceipt, SendError> {
        let echo = uuid::Uuid::new_v4().to_string();
        self.send_group_message_with_echo(group_id, message, echo)
            .await
    }

    /// Send a private message using a caller-supplied echo token (the durable
    /// operation ledger keeps the same token so late echoes are attributable).
    pub async fn send_private_message_with_echo(
        &self,
        user_id: u64,
        message: &str,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        self.send_action(
            "send_private_msg",
            serde_json::json!({ "user_id": user_id, "message": message }),
            echo,
        )
        .await
    }

    /// Send a group message using a caller-supplied echo token.
    pub async fn send_group_message_with_echo(
        &self,
        group_id: u64,
        message: &str,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        self.send_action(
            "send_group_msg",
            serde_json::json!({ "group_id": group_id, "message": message }),
            echo,
        )
        .await
    }

    /// Enqueue one action and await its correlated response. SPEC §35.6.
    async fn send_action(
        &self,
        action: &str,
        params: serde_json::Value,
        echo: String,
    ) -> Result<SendReceipt, SendError> {
        let (tx, rx) = oneshot::channel();
        self.pending_actions.lock().insert(echo.clone(), tx);

        let action = OneBotAction {
            action: action.to_string(),
            params,
            echo: Some(echo.clone()),
        };
        if self.action_tx.send(action).await.is_err() {
            self.pending_actions.lock().remove(&echo);
            return Err(SendError::Disconnected);
        }

        match tokio::time::timeout(SEND_RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(value)) => Ok(SendReceipt {
                echo,
                status: value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                retcode: value.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1),
                data: value
                    .get("data")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null),
                message: value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            }),
            // The connector dropped the waiter (disconnect/cancel) — the action
            // may already have been written to the socket. Unknown, not failed.
            Ok(Err(_)) => Err(SendError::Unknown(
                "connector dropped pending action".into(),
            )),
            Err(_) => {
                // Late responses for this echo are ignored by the reader loop.
                self.pending_actions.lock().remove(&echo);
                Err(SendError::Unknown(format!(
                    "no response within {}s",
                    SEND_RESPONSE_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Run the WS connection loop — reconnects on disconnect
    pub async fn run_connection_loop(
        config: NapCatConfig,
        event_tx: mpsc::Sender<OneBotEvent>,
        action_rx: mpsc::Receiver<OneBotAction>,
        pending_actions: PendingActions,
        spool: Arc<IngressSpool>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        let result = Self::connection_loop(
            config,
            event_tx,
            action_rx,
            pending_actions.clone(),
            spool,
            cancel,
        )
        .await;
        pending_actions.lock().clear();
        if let Err(err) = &result {
            error!(err=%err, "NapCat: 接收循环失败，未确认事件留待重开核对");
        }
        result
    }

    async fn connection_loop(
        config: NapCatConfig,
        event_tx: mpsc::Sender<OneBotEvent>,
        mut action_rx: mpsc::Receiver<OneBotAction>,
        pending_actions: PendingActions,
        spool: Arc<IngressSpool>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        // 进程重开本身也是未覆盖区间；不把重连当成历史已补齐。
        spool.record_disconnect()?;
        let mut after = 0;
        loop {
            let events = spool.pending_after(after, 256)?;
            if events.is_empty() {
                break;
            }
            for event in events {
                after = event.seq;
                if !Self::deliver_spooled(&event_tx, &spool, event, &cancel).await? {
                    return Ok(());
                }
            }
        }
        loop {
            if cancel.is_cancelled() {
                break;
            }

            let url = match Url::parse(&config.ws_url) {
                Ok(u) => u,
                Err(e) => return Err(anyhow::anyhow!("NapCat WS URL 无效: {e}")),
            };

            info!(url=%config.ws_url, "NapCat: connecting");

            match connect_async(url.as_str()).await {
                Err(e) => {
                    warn!(err=%e, "NapCat: connection failed, retrying in {}ms", config.reconnect_interval_ms);
                    tokio::time::sleep(tokio::time::Duration::from_millis(
                        config.reconnect_interval_ms,
                    ))
                    .await;
                    continue;
                }
                Ok((ws_stream, _)) => {
                    info!("NapCat: connected");
                    spool.record_reconnect()?;
                    let (mut write, mut read) = ws_stream.split();

                    loop {
                        tokio::select! {
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        let value: serde_json::Value = match serde_json::from_str(&text) {
                                            Ok(v) => v,
                                            Err(e) => {
                                                warn!(err=%e, "NapCat: non-JSON message");
                                                continue;
                                            }
                                        };

                                        if value.get("post_type").is_some() {
                                            // 原文先落盘，再解析和交付。解析失败仍可从接收库恢复。
                                            if let Some(event) = spool.ingest(value)? {
                                                if !Self::deliver_spooled(&event_tx, &spool, event, &cancel).await? {
                                                    spool.record_disconnect()?;
                                                    return Ok(());
                                                }
                                            }
                                        } else if let Some(echo) = value.get("echo").and_then(|v| v.as_str()) {
                                            // Action response — route to its waiter.
                                            let waiter = pending_actions.lock().remove(echo);
                                            match waiter {
                                                Some(tx) => { let _ = tx.send(value); }
                                                None => {
                                                    // E.g. a response that arrived after our timeout.
                                                    // The ledger marks it unknown and reconciles later.
                                                    tracing::debug!(echo, "NapCat: unmatched action response");
                                                }
                                            }
                                        } else {
                                            tracing::debug!("NapCat: unrecognized message");
                                        }
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        warn!("NapCat: connection closed");
                                        spool.record_disconnect()?;
                                        break;
                                    }
                                    Some(Err(e)) => {
                                        warn!(err=%e, "NapCat: WS error");
                                        spool.record_disconnect()?;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            action = action_rx.recv() => {
                                match action {
                                    Some(a) => {
                                        let json = serde_json::to_string(&a)?;
                                        if let Err(e) = write.send(Message::Text(json.into())).await {
                                            warn!(err=%e, "NapCat: send failed");
                                            spool.record_disconnect()?;
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            _ = cancel.cancelled() => { spool.record_disconnect()?; break; }
                        }
                    }

                    // This connection is gone. Every action still waiting may
                    // have reached NapCat before the socket dropped, so its
                    // outcome is unknown — wake the callers instead of letting
                    // them sit until timeout. Actions still sitting in the
                    // queue were never written; discard them so they are not
                    // silently transmitted on the next connection while their
                    // callers were told "unknown". SPEC §35.6: reconcile, no
                    // implicit retry.
                    while action_rx.try_recv().is_ok() {}
                    pending_actions.lock().clear();
                }
            }

            if cancel.is_cancelled() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(
                config.reconnect_interval_ms,
            ))
            .await;
        }
        pending_actions.lock().clear();
        info!("NapCat: connection loop terminated");
        Ok(())
    }

    async fn deliver_spooled(
        event_tx: &mpsc::Sender<OneBotEvent>,
        spool: &IngressSpool,
        event: SpooledEvent,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<bool> {
        let mut normalized = match serde_json::from_value::<OneBotEvent>(event.raw) {
            Ok(value) => value,
            Err(err) => {
                warn!(id=%event.spool_id, err=%err, "NapCat: 解析失败，保留原始 pending 事件");
                return Ok(true);
            }
        };
        // flatten 中保留的平台同名字段不能在序列化时伪造本地身份。
        if let Some(data) = normalized.data.as_object_mut() {
            data.remove("ingress_id");
        }
        normalized.ingress_id = Some(event.spool_id);
        tokio::select! {
            sent = event_tx.send(normalized) => {
                sent.map_err(|_| anyhow::anyhow!("NapCat 消费 channel 已关闭，事件仍未确认"))?;
            }
            _ = cancel.cancelled() => return Ok(false),
        }
        spool.mark_delivered(event.spool_id)?;
        // 不调用 ack：channel 成功不等于主体已提交处置。
        Ok(true)
    }
}
