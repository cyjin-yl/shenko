//! The composition root of the channel adapters.
//!
//! One [`Adapters`] value wires each configured platform end to end: the
//! health observation, the durable ingress ledger and the reply-context cache
//! are built once and shared between the outbound plugin and the inbound
//! monitor, so the context a reply needs is exactly the context the monitor
//! recorded, and the event the monitor received is the event the runtime later
//! admits.
//!
//! The plugins are registered into the [`ChannelRegistry`] the Shenko runtime
//! assembles; the monitors are spawned by [`Adapters::run_ingress`], and the
//! runtime's delivery loop drains [`Adapters::ingress`] into subject admission.
//!
//! This module is deliberately free of `shenko-store`: the Store ledger owns
//! intents, attempts and receipts. The adapters only classify, describe,
//! dispatch, report health and keep their own ingress durable.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use shenko_types::channel::{ChannelPlugin, ChannelRegistry, ChannelRegistryError};

use crate::common::{ChannelCapabilities, ChannelHealth, HealthState, PlatformHttp, ReplyContextCache};
use crate::feishu::api::FeishuApi;
use crate::feishu::auth::TenantAuth;
use crate::feishu::events::FEISHU_CHANNEL_ID;
use crate::feishu::monitor::FeishuMonitor;
use crate::feishu::{FeishuChannel, FeishuConfig};
use crate::ilink::api::ILinkApi;
use crate::ilink::events::ILINK_CHANNEL_ID;
use crate::ilink::monitor::{ILinkMonitor, cursor_path as ilink_cursor_path};
use crate::ilink::{ILinkChannel, ILinkConfig};
use crate::ingress::{AdapterIngress, SharedAdapterIngress};
use crate::qq_official::api::QqOfficialApi;
use crate::qq_official::events::QQ_OFFICIAL_CHANNEL_ID;
use crate::qq_official::gateway::{QqGatewaySession, cursor_path as qq_cursor_path};
use crate::qq_official::{QqOfficialChannel, QqOfficialConfig};

/// The plain inputs the adapters need. No Store, no runtime handles: only
/// configuration and where to keep the durable cursors and ingress ledgers.
#[derive(Debug, Clone, Default)]
pub struct AdapterDeps {
    /// Where the gateway/monitor cursors and the ingress ledgers are persisted.
    pub data_dir: PathBuf,
    /// The official QQ bot account, when configured.
    pub qq: Option<Arc<QqOfficialConfig>>,
    /// The WeChat iLink account, when configured.
    pub ilink: Option<Arc<ILinkConfig>>,
    /// The Feishu/Lark app, when configured.
    pub feishu: Option<Arc<FeishuConfig>>,
}

/// The on-disk description of the adapters one runtime installs.
///
/// The file is the *only* channel configuration besides the NapCat flags: a
/// platform absent from it is not registered, not advertised in the reply
/// grammar and never contacted. Unknown keys are rejected so a typo cannot
/// silently disable a channel, and the file is parsed into the same config
/// types the adapters use at runtime.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterConfigFile {
    /// The official QQ bot connector (`qq-official`) when present.
    #[serde(default)]
    pub qq_official: Option<QqOfficialConfig>,
    /// The WeChat iLink connector (`ilink`) when present.
    #[serde(default)]
    pub ilink: Option<ILinkConfig>,
    /// The Feishu/Lark connector (`feishu`) when present.
    #[serde(default)]
    pub feishu: Option<FeishuConfig>,
}

impl AdapterConfigFile {
    /// Read and validate one configuration file.
    ///
    /// Validation happens before any connector exists: an app id without its
    /// ticket (or an iLink account without a bot identity) is a configuration
    /// error, not a channel that silently never sends anything.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("channels config {} unreadable: {error}", path.display()))?;
        let file: Self = serde_json::from_str(&text)
            .map_err(|error| format!("channels config {} invalid: {error}", path.display()))?;
        file.validate()?;
        Ok(file)
    }

    /// Check every configured account is usable as an account namespace.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(qq) = &self.qq_official {
            if qq.app_id.trim().is_empty() {
                return Err("qq_official.app_id must not be empty".to_string());
            }
            if qq.app_secret.as_deref().unwrap_or("").trim().is_empty() {
                return Err(format!(
                    "qq_official {} needs app_secret to obtain an access token",
                    qq.account_scope()
                ));
            }
        }
        if let Some(ilink) = &self.ilink {
            if ilink
                .ilink_bot_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
            {
                return Err("ilink.ilink_bot_id names the account and must not be empty".to_string());
            }
            if ilink.bot_token.as_deref().unwrap_or("").trim().is_empty() {
                return Err(format!(
                    "ilink {} needs bot_token from the QR authorisation",
                    ilink.account_scope()
                ));
            }
        }
        if let Some(feishu) = &self.feishu {
            if feishu.app_id.trim().is_empty() {
                return Err("feishu.app_id must not be empty".to_string());
            }
            if feishu.app_secret.as_deref().unwrap_or("").trim().is_empty() {
                return Err(format!(
                    "feishu {} needs app_secret to obtain a tenant token",
                    feishu.account_scope()
                ));
            }
        }
        Ok(())
    }

    /// Turn the file into composition-root dependencies.
    pub fn into_deps(self, data_dir: impl Into<PathBuf>) -> AdapterDeps {
        AdapterDeps {
            data_dir: data_dir.into(),
            qq: self.qq_official.map(Arc::new),
            ilink: self.ilink.map(Arc::new),
            feishu: self.feishu.map(Arc::new),
        }
    }
}

/// One configured platform's monitor, kept so ingress can be spawned.
enum Ingress {
    Qq(Arc<QqGatewaySession>),
    ILink(Arc<ILinkMonitor>),
    Feishu(Arc<FeishuMonitor>),
}

/// One platform's observable facts: what it can do and how its connector is
/// doing right now. The runtime records these as subject-readable channel
/// facts, so capability discovery and health are real platform facts rather
/// than a claim in a document.
#[derive(Clone)]
pub struct AdapterObservation {
    /// The channel id (`qq-official` / `ilink` / `feishu`).
    pub channel: String,
    /// The account scope the connector serves.
    pub account: String,
    /// The `InboxEvent.source` label of this connector.
    pub source: String,
    /// The live health observation.
    pub health: HealthState,
    /// What this platform can actually do.
    pub capabilities: ChannelCapabilities,
}

impl std::fmt::Debug for AdapterObservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterObservation")
            .field("channel", &self.channel)
            .field("account", &self.account)
            .field("source", &self.source)
            .finish()
    }
}

/// The three adapters, wired and ready to register, run and drain.
pub struct Adapters {
    /// The official QQ plugin, when configured.
    pub qq: Option<Arc<QqOfficialChannel>>,
    /// The WeChat iLink plugin, when configured.
    pub ilink: Option<Arc<ILinkChannel>>,
    /// The Feishu/Lark plugin, when configured.
    pub feishu: Option<Arc<FeishuChannel>>,
    /// The monitors of the configured platforms.
    ingress: Vec<Ingress>,
    /// The durable ingress ledger of each configured platform.
    ledgers: Vec<SharedAdapterIngress>,
    /// The health/capability observation of each configured platform.
    observations: Vec<AdapterObservation>,
}

impl Adapters {
    /// Build every configured adapter, sharing health, ingress and reply
    /// contexts between the outbound plugin and the inbound monitor of each
    /// platform.
    pub fn build(deps: AdapterDeps) -> Result<Self, String> {
        let mut ingress = Vec::new();
        let mut ledgers = Vec::new();
        let mut observations = Vec::new();
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
            let plugin = Arc::new(QqOfficialChannel::new(
                config.clone(),
                api.clone(),
                health.clone(),
                replies.clone(),
            ));
            let ledger = open_ledger(&deps.data_dir, QQ_OFFICIAL_CHANNEL_ID, &account)?;
            observations.push(AdapterObservation {
                channel: QQ_OFFICIAL_CHANNEL_ID.to_string(),
                account: account.clone(),
                source: plugin.sources()[0].clone(),
                health: health.clone(),
                capabilities: plugin.capabilities(),
            });
            ledgers.push(ledger.clone());
            let cursor = qq_cursor_path(&deps.data_dir, &account);
            ingress.push(Ingress::Qq(Arc::new(QqGatewaySession::new(
                config, api, health, replies, cursor, ledger,
            ))));
            qq = Some(plugin);
        }

        if let Some(config) = deps.ilink.clone() {
            let http = PlatformHttp::new()?;
            let account = config.account_scope();
            let health = HealthState::new(ChannelHealth::new(
                ILINK_CHANNEL_ID,
                &account,
                "starting",
                None,
            ));
            let replies = ReplyContextCache::new(
                config.reply_context_capacity,
                Duration::from_secs(config.reply_window_seconds),
            );
            let api = ILinkApi::new(config.clone(), http);
            let plugin = Arc::new(ILinkChannel::new(
                config.clone(),
                api.clone(),
                health.clone(),
                replies.clone(),
            ));
            let ledger = open_ledger(&deps.data_dir, ILINK_CHANNEL_ID, &account)?;
            observations.push(AdapterObservation {
                channel: ILINK_CHANNEL_ID.to_string(),
                account: account.clone(),
                source: plugin.sources()[0].clone(),
                health: health.clone(),
                capabilities: plugin.capabilities(),
            });
            ledgers.push(ledger.clone());
            let cursor = ilink_cursor_path(&deps.data_dir, &account);
            ingress.push(Ingress::ILink(Arc::new(ILinkMonitor::new(
                config, api, health, replies, cursor, ledger,
            ))));
            ilink = Some(plugin);
        }

        if let Some(config) = deps.feishu.clone() {
            let http = PlatformHttp::new()?;
            let account = config.account_scope();
            let health = HealthState::new(ChannelHealth::new(
                FEISHU_CHANNEL_ID,
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
            let plugin = Arc::new(FeishuChannel::new(
                config.clone(),
                api,
                health.clone(),
                replies.clone(),
            ));
            let ledger = open_ledger(&deps.data_dir, FEISHU_CHANNEL_ID, &account)?;
            observations.push(AdapterObservation {
                channel: FEISHU_CHANNEL_ID.to_string(),
                account: account.clone(),
                source: plugin.sources()[0].clone(),
                health: health.clone(),
                capabilities: plugin.capabilities(),
            });
            ledgers.push(ledger.clone());
            ingress.push(Ingress::Feishu(Arc::new(FeishuMonitor::new(
                config, auth, health, replies, ledger,
            ))));
            feishu = Some(plugin);
        }

        Ok(Self {
            qq,
            ilink,
            feishu,
            ingress,
            ledgers,
            observations,
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

    /// The durable ingress ledger of each configured platform.
    pub fn ingress(&self) -> &[SharedAdapterIngress] {
        &self.ledgers
    }

    /// The health/capability observation of each configured platform.
    pub fn observations(&self) -> &[AdapterObservation] {
        &self.observations
    }

    /// Spawn the inbound monitor of every configured platform.
    ///
    /// Every monitor records each classified event into its own durable ledger
    /// and honours `cancel`. A monitor that cannot record returns the error to
    /// its own loop: the platform cursor is not advanced, so the window is
    /// replayed on reconnect. The returned handles end when the monitor stops;
    /// one platform's connector never takes the subject loop down with it.
    pub async fn run_ingress(
        &self,
        cancel: CancellationToken,
    ) -> Vec<tokio::task::JoinHandle<Result<(), String>>> {
        let mut handles = Vec::new();
        for monitor in &self.ingress {
            let cancel = cancel.clone();
            let handle = match monitor {
                Ingress::Qq(session) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        session
                            .run(cancel)
                            .await
                            .map_err(|error| error.to_string())
                    })
                }
                Ingress::ILink(session) => {
                    let session = session.clone();
                    tokio::spawn(async move { session.run(cancel).await })
                }
                Ingress::Feishu(session) => {
                    let session = session.clone();
                    tokio::spawn(async move {
                        session
                            .run(cancel)
                            .await
                            .map_err(|error| error.to_string())
                    })
                }
            };
            handles.push(handle);
        }
        handles
    }
}

/// Open one account's durable ingress ledger inside the runtime data dir.
///
/// The file name is derived from the channel and the account scope, with the
/// account's namespace separators folded to `-` so two accounts can never share
/// a ledger path.
fn open_ledger(
    data_dir: &Path,
    channel: &str,
    account: &str,
) -> Result<SharedAdapterIngress, String> {
    let safe: String = account
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let path = data_dir.join(format!("ingress-{channel}-{safe}.sqlite"));
    Ok(Arc::new(AdapterIngress::open(&path, channel, account)?))
}

/// Build every configured adapter and register it into one registry.
///
/// This is the one call the Shenko runtime makes to bring the three adapters
/// up; it returns the wired [`Adapters`] so the runtime can also run ingress,
/// drain the durable ledgers and record health.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shenko-adapters-runtime-{}",
            shenko_types::ids::EntityId::new()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn feishu_only() -> AdapterDeps {
        AdapterDeps {
            data_dir: scratch(),
            feishu: Some(Arc::new(FeishuConfig::for_app(
                "cli_test_app",
                Some("secret".to_string()),
            ))),
            ..Default::default()
        }
    }

    /// A configured platform becomes real channel authority: its plugin is
    /// registered (so its reply grammar/resolution exists), its source label is
    /// owned by exactly that plugin, and it owns a durable ingress ledger.
    #[test]
    fn a_configured_adapter_is_registered_with_an_ingress_ledger() {
        let deps = feishu_only();
        let data_dir = deps.data_dir.clone();
        let mut registry = ChannelRegistry::new();
        let adapters = register_adapters(&mut registry, deps).unwrap();
        assert_eq!(registry.plugins().count(), 1);
        assert_eq!(adapters.ingress().len(), 1);
        let ledger = &adapters.ingress()[0];
        assert_eq!(ledger.channel(), FEISHU_CHANNEL_ID);
        assert_eq!(ledger.account(), "feishu:cli_test_app");
        assert!(ledger.is_persistent());
        assert!(
            ledger
                .path()
                .unwrap()
                .starts_with(&data_dir)
        );
        // The monitor's source label is the plugin's own, so admission can find
        // the plugin again without guessing.
        let observations = adapters.observations();
        assert_eq!(observations[0].source, "feishu");
        assert!(registry.channel_for_source("feishu").is_some());
        assert_eq!(registry.reply_channel("feishu-chat"), Some("feishu"));
        // Two accounts of the same channel never share a ledger.
        let other = register_adapters(
            &mut ChannelRegistry::new(),
            AdapterDeps {
                data_dir: data_dir.clone(),
                feishu: Some(Arc::new(FeishuConfig::for_app(
                    "cli_other_app",
                    Some("secret".to_string()),
                ))),
                ..Default::default()
            },
        )
        .unwrap();
        assert_ne!(
            other.ingress()[0].path().unwrap(),
            ledger.path().unwrap()
        );
    }

    fn write_config(text: &str) -> PathBuf {
        let dir = scratch();
        let path = dir.join("channels.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// The configuration file is the only authority that installs a channel, so
    /// it is strict: unknown keys, an account without its ticket, and an iLink
    /// account without a bot identity are refused before any connector exists.
    #[test]
    fn the_channels_file_is_strict_about_accounts() {
        let path = write_config(
            r#"{"feishu": {"base_url": "https://open.feishu.cn", "app_id": "cli_app",
                "app_secret": "s", "reply_context_capacity": 8,
                "reply_window_seconds": 60}}"#,
        );
        let file = AdapterConfigFile::load(&path).unwrap();
        let deps = file.into_deps(scratch());
        assert!(deps.feishu.is_some() && deps.qq.is_none() && deps.ilink.is_none());

        // A typo must fail loudly instead of silently disabling a channel.
        let unknown = write_config(r#"{"feishu": {}, "feishuu": {}}"#);
        assert!(AdapterConfigFile::load(&unknown).is_err());
        // No ticket, no channel.
        let no_secret = write_config(r#"{"feishu": {"base_url": "x", "app_id": "cli_app"}}"#);
        assert!(AdapterConfigFile::load(&no_secret).is_err());
        let qq_no_secret = write_config(
            r#"{"qq_official": {"app_id": "1024", "api_base": "https://api.bot.qq.com",
                "token_base": "https://bots.qq.com", "intents": 1, "shard": [0,1],
                "reply_window_seconds": 60, "reply_context_capacity": 8}}"#,
        );
        assert!(AdapterConfigFile::load(&qq_no_secret).is_err());
        // iLink has no app ticket; the bot identity IS the account namespace.
        let ilink_no_identity = write_config(
            r#"{"ilink": {"base_url": "https://ilinkai.weixin.qq.com",
                "cdn_base_url": "https://cdn", "app_id": "bot", "bot_token": "t",
                "channel_version": "2.4.8", "bot_agent": "shenko",
                "long_poll_timeout_ms": 1000, "reply_context_capacity": 8,
                "reply_window_seconds": 60}}"#,
        );
        assert!(AdapterConfigFile::load(&ilink_no_identity).is_err());
    }

    /// Credentials never reach a debug line, a health fact or a log record: the
    /// parsed configuration is redacted at the type that owns the secret.
    #[test]
    fn credentials_are_redacted_from_debug_output() {
        let path = write_config(
            r#"{"qq_official": {"app_id": "1024", "app_secret": "qq-dummy-secret",
                    "api_base": "https://api.bot.qq.com", "token_base": "https://bots.qq.com",
                    "intents": 1, "shard": [0,1], "reply_window_seconds": 60,
                    "reply_context_capacity": 8},
                "ilink": {"base_url": "https://ilinkai.weixin.qq.com", "cdn_base_url": "https://cdn",
                    "app_id": "bot", "bot_token": "ilink-dummy-token",
                    "ilink_bot_id": "bot-1", "channel_version": "2.4.8", "bot_agent": "shenko",
                    "long_poll_timeout_ms": 1000, "reply_context_capacity": 8,
                    "reply_window_seconds": 60},
                "feishu": {"base_url": "https://open.feishu.cn", "app_id": "cli_app",
                    "app_secret": "feishu-dummy-secret", "reply_context_capacity": 8,
                    "reply_window_seconds": 60}}"#,
        );
        let file = AdapterConfigFile::load(&path).unwrap();
        let debug = format!("{file:?}");
        for secret in ["qq-dummy-secret", "ilink-dummy-token", "feishu-dummy-secret"] {
            assert!(!debug.contains(secret), "credential leaked: {debug}");
        }
        assert!(debug.contains("<redacted>"));
        assert!(debug.contains("cli_app"));
    }

    /// An unconfigured platform is absent, not degraded: nothing is registered,
    /// no ledger is created and no reply grammar advertises it.
    #[test]
    fn absent_platforms_are_not_registered() {
        let mut registry = ChannelRegistry::new();
        let adapters = register_adapters(
            &mut registry,
            AdapterDeps {
                data_dir: scratch(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(registry.plugins().count(), 0);
        assert!(adapters.ingress().is_empty());
        assert!(adapters.observations().is_empty());
    }
}
