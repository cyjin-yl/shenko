//! The composition root of the channel adapters.
//!
//! One [`Adapters`] value wires each configured platform end to end: the
//! health observation and the reply-context cache are built once and shared
//! between the outbound plugin and the inbound monitor, so the context a
//! reply needs is exactly the context the monitor recorded. The plugins are
//! registered into the [`ChannelRegistry`] the Shenko runtime assembles; the
//! monitors are spawned by [`Adapters::run_ingress`].
//!
//! This module is deliberately free of `shenko-store`: the Store ledger owns
//! intents, attempts and receipts. The adapters only classify, describe,
//! dispatch and report health.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use shenko_types::channel::{ChannelRegistry, ChannelRegistryError};

use crate::common::{ChannelHealth, HealthState, PlatformHttp, ReplyContextCache};
use crate::feishu::api::FeishuApi;
use crate::feishu::auth::TenantAuth;
use crate::feishu::monitor::FeishuMonitor;
use crate::feishu::{FeishuChannel, FeishuConfig};
use crate::ilink::api::ILinkApi;
use crate::ilink::monitor::{ILinkMonitor, cursor_path as ilink_cursor_path};
use crate::ilink::{ILinkChannel, ILinkConfig};
use crate::qq_official::api::QqOfficialApi;
use crate::qq_official::events::QQ_OFFICIAL_CHANNEL_ID;
use crate::qq_official::gateway::{QqGatewaySession, cursor_path as qq_cursor_path};
use crate::qq_official::{QqOfficialChannel, QqOfficialConfig};
use crate::InboundEvent;

/// The plain inputs the adapters need. No Store, no runtime handles: only
/// configuration and where to keep the durable cursors.
#[derive(Debug, Clone, Default)]
pub struct AdapterDeps {
    /// Where the gateway and monitor cursors are persisted.
    pub data_dir: PathBuf,
    /// The official QQ bot account, when configured.
    pub qq: Option<Arc<QqOfficialConfig>>,
    /// The WeChat iLink account, when configured.
    pub ilink: Option<Arc<ILinkConfig>>,
    /// The Feishu/Lark app, when configured.
    pub feishu: Option<Arc<FeishuConfig>>,
}

/// One configured platform's monitor, kept so ingress can be spawned.
enum Ingress {
    Qq(Arc<QqGatewaySession>),
    ILink(Arc<ILinkMonitor>),
    Feishu(Arc<FeishuMonitor>),
}

/// The three adapters, wired and ready to register and run.
pub struct Adapters {
    /// The official QQ plugin, when configured.
    pub qq: Option<Arc<QqOfficialChannel>>,
    /// The WeChat iLink plugin, when configured.
    pub ilink: Option<Arc<ILinkChannel>>,
    /// The Feishu/Lark plugin, when configured.
    pub feishu: Option<Arc<FeishuChannel>>,
    /// The monitors of the configured platforms.
    ingress: Vec<Ingress>,
}

impl Adapters {
    /// Build every configured adapter, sharing health and reply contexts
    /// between the outbound plugin and the inbound monitor of each platform.
    pub fn build(deps: AdapterDeps) -> Result<Self, String> {
        let mut ingress = Vec::new();
        let mut qq = None;
        let mut ilink = None;
        let mut feishu = None;

        if let Some(config) = deps.qq.clone() {
            let http = PlatformHttp::new()?;
            let account = config.account_scope();
            let health = HealthState::new(ChannelHealth::new(
                QQ_OFFICIAL_CHANNEL_ID,
                &account,
                "starting",
                None,
            ));
            let replies = ReplyContextCache::new(
                config.reply_context_capacity,
                Duration::from_secs(config.reply_window_seconds),
            );
            let api = QqOfficialApi::new(config.clone(), http);
            qq = Some(Arc::new(QqOfficialChannel::new(
                config.clone(),
                api.clone(),
                health.clone(),
                replies.clone(),
            )));
            let cursor = qq_cursor_path(&deps.data_dir, &account);
            ingress.push(Ingress::Qq(Arc::new(QqGatewaySession::new(
                config, api, health, replies, cursor,
            ))));
        }

        if let Some(config) = deps.ilink.clone() {
            let http = PlatformHttp::new()?;
            let account = config.account_scope();
            let health = HealthState::new(ChannelHealth::new(
                crate::ilink::events::ILINK_CHANNEL_ID,
                &account,
                "starting",
                None,
            ));
            let replies = ReplyContextCache::new(
                config.reply_context_capacity,
                Duration::from_secs(config.reply_window_seconds),
            );
            let api = ILinkApi::new(config.clone(), http);
            ilink = Some(Arc::new(ILinkChannel::new(
                config.clone(),
                api.clone(),
                health.clone(),
                replies.clone(),
            )));
            let cursor = ilink_cursor_path(&deps.data_dir, &account);
            ingress.push(Ingress::ILink(Arc::new(ILinkMonitor::new(
                config, api, health, replies, cursor,
            ))));
        }

        if let Some(config) = deps.feishu.clone() {
            let http = PlatformHttp::new()?;
            let account = config.account_scope();
            let health = HealthState::new(ChannelHealth::new(
                crate::feishu::events::FEISHU_CHANNEL_ID,
                &account,
                "starting",
                None,
            ));
            let replies = ReplyContextCache::new(
                config.reply_context_capacity,
                Duration::from_secs(config.reply_window_seconds),
            );
            let auth = Arc::new(TenantAuth::new(config.clone(), http.clone()));
            let api = FeishuApi::new(config.clone(), http, auth.clone());
            feishu = Some(Arc::new(FeishuChannel::new(
                config.clone(),
                api,
                health.clone(),
                replies.clone(),
            )));
            ingress.push(Ingress::Feishu(Arc::new(FeishuMonitor::new(
                config, auth, health, replies,
            ))));
        }

        Ok(Self {
            qq,
            ilink,
            feishu,
            ingress,
        })
    }

    /// Register every built plugin into the runtime registry.
    pub fn register(&self, registry: &mut ChannelRegistry) -> Result<(), ChannelRegistryError> {
        if let Some(plugin) = &self.qq {
            registry.register(plugin.clone())?;
        }
        if let Some(plugin) = &self.ilink {
            registry.register(plugin.clone())?;
        }
        if let Some(plugin) = &self.feishu {
            registry.register(plugin.clone())?;
        }
        Ok(())
    }

    /// Spawn the inbound monitor of every configured platform.
    ///
    /// Every monitor forwards its classified events into `sender` and honours
    /// `cancel`. The returned handles end when the monitor stops.
    pub async fn run_ingress(
        &self,
        sender: mpsc::Sender<InboundEvent>,
        cancel: CancellationToken,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut handles = Vec::new();
        for monitor in &self.ingress {
            let sender = sender.clone();
            let cancel = cancel.clone();
            let handle = match monitor {
                Ingress::Qq(session) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(error) = session.run(sender, cancel).await {
                            tracing::warn!(%error, "qq official gateway ended");
                        }
                    })
                }
                Ingress::ILink(session) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(error) = session.run(sender, cancel).await {
                            tracing::warn!(%error, "ilink monitor ended");
                        }
                    })
                }
                Ingress::Feishu(session) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        if let Err(error) = session.run(sender, cancel).await {
                            tracing::warn!(%error, "feishu monitor ended");
                        }
                    })
                }
            };
            handles.push(handle);
        }
        handles
    }
}

/// Build every configured adapter and register it into one registry.
///
/// This is the one call the Shenko runtime makes to bring the three adapters
/// up; it returns the wired [`Adapters`] so the runtime can also run ingress.
pub fn register_adapters(
    registry: &mut ChannelRegistry,
    deps: AdapterDeps,
) -> Result<Adapters, String> {
    let adapters = Adapters::build(deps)?;
    adapters
        .register(registry)
        .map_err(|error| format!("channel adapter registration failed: {error}"))?;
    Ok(adapters)
}
