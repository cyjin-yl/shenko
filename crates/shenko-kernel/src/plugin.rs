//! Shenko 插件 facade 与真实 Cordis 生命周期桥。
//!
//! 固定上游：cordis-rs 1754062cef67cfc119fc7a2084c0321a231b9092。
//! `CordisAdapter` 实现上游 `Plugin`，由 `Context::spawn` 创建真实 Fiber；
//! stop/清理注册为 apply generation 的 `Context::effect`，而非手工模仿 drain。
//! 注册表保留实际 `FiberHandle`，显式逆序 dispose；这是应用策略，不是上游父子关系。
//!
//! 当前支持 prepare、apply、失败回收、显式 restart 和 attempt-all teardown。
//! `PluginMeta` 能力字符串、`PluginService` 和 `PluginContext` 仍是 Shenko 类型，
//! 不等同于上游 `InjectSpec`、`Service`、`Context`。尚未桥接字符串依赖的热更新、
//! ServiceRealm 配置、typed input update/era swap 或跨进程/跨代 handoff。
//! 插件可通过 `start_with_context` 使用真实 generation-owned 上游 API；
//! 自行创建的外部效果仍必须提供真实撤销/对账，不以任务 Drop 冒充撤销。
//! legacy stop/cleanup 返回错误或 panic 会闭锁后续启动/重启，等待外部结果对账；
//! 不会因读取一次错误就清除未知状态。显式 restart 是破坏性代际重启，不是热图发布。
use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use cordis_core::{Context, FiberHandle, Plugin as CordisPlugin, PreparedPlugin};
use futures::FutureExt;
use shenko_types::ids::EntityId;
use tokio::sync::{mpsc, oneshot};

/// Shenko 插件标识，不是 Cordis FiberId。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(pub EntityId);

impl PluginId {
    pub fn new() -> Self {
        Self(EntityId::new())
    }
}

impl Default for PluginId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct PluginMeta {
    pub id: PluginId,
    pub name: String,
    pub version: String,
    pub capabilities_provided: Vec<String>,
    pub capabilities_required: Vec<String>,
}

/// Shenko 清理兼容上下文；不伪装成上游 Context 或 FiberId。
#[derive(Debug, Clone)]
pub struct PluginContext {
    /// 本注册项实际 apply 次数，包含失败尝试；不是可读取的上游 generation ID。
    pub generation: u64,
    /// 本注册项的稳定 Shenko 标识；重新 spawn 仍保留，但上游 FiberId 会改变。
    pub fiber_id: EntityId,
}

impl PluginContext {
    pub fn new(generation: u64) -> Self {
        Self {
            generation,
            fiber_id: EntityId::new(),
        }
    }
}

/// Shenko 的命名约定，不是 cordis_core::Service，也不建立 InjectSpec 边。
pub trait PluginService: Send + Sync + 'static {
    const NAME: &'static str;
}

/// 保留 meta/start/stop 的 Shenko facade，实际承载于 Cordis Fiber。
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    fn meta(&self) -> &PluginMeta;

    /// 旧插件自己的计数，兼容保留；桥不会把它当成 Cordis generation。
    fn generation(&self) -> u64 {
        0
    }

    /// 在上游生命周期 admission 之前同步验证，不应创建外部效果。
    fn prepare(&self) -> Result<()> {
        Ok(())
    }

    async fn effect_cleanup(&self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }

    async fn start(&self) -> Result<()>;

    /// 真实 apply Context；通过它注册的效果归此 Fiber generation 所有。
    /// 不应另行 spawn 子 Fiber 或在 root 注册效果而不保留显式清理责任。
    async fn start_with_context(&self, _ctx: Context) -> Result<()> {
        self.start().await
    }

    /// 部分启动失败也会调用，因此必须能处理尚未完全启动的自身状态。
    async fn stop(&self) -> Result<()>;
}

struct Registration {
    plugin: Arc<dyn Plugin>,
    fiber_id: EntityId,
    applies: AtomicU64,
}

struct CordisAdapter {
    registration: Arc<Registration>,
    failures: Arc<parking_lot::Mutex<Vec<String>>>,
}

fn bridge_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(error.to_string())
}

// Cordis reports generation-drain failures diagnostically, not via dispose/restart.
// Keep a fail-closed latch too: an unknown legacy stop cannot authorize re-apply.
fn cleanup_result(failures: &parking_lot::Mutex<Vec<String>>) -> Result<()> {
    let failures = failures.lock();
    anyhow::ensure!(
        failures.is_empty(),
        "plugin cleanup failed; reconcile external outcomes before replacing the registry: {}",
        failures.join("; ")
    );
    Ok(())
}

fn panic_diagnostic(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else {
        "non-string panic".to_owned()
    }
}

async fn cleanup_call(
    label: String,
    failures: Arc<parking_lot::Mutex<Vec<String>>>,
    work: impl Future<Output = Result<()>> + Send,
) -> std::io::Result<()> {
    let failure = match AssertUnwindSafe(work).catch_unwind().await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error)) => error.to_string(),
        Err(payload) => format!("panicked: {}", panic_diagnostic(payload)),
    };
    let diagnostic = format!("{label}: {failure}");
    failures.lock().push(diagnostic.clone());
    Err(bridge_error(diagnostic))
}

impl CordisPlugin for CordisAdapter {
    type Config = ();
    type Input = ();
    type PrepareError = std::io::Error;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.registration.plugin.meta().name)
    }

    fn prepare(&self, (): ()) -> std::result::Result<(), Self::PrepareError> {
        if self.registration.plugin.meta().name.trim().is_empty() {
            return Err(bridge_error("plugin name must not be empty"));
        }
        self.registration.plugin.prepare().map_err(bridge_error)
    }

    async fn apply(&self, ctx: Context, (): &()) -> std::result::Result<(), Self::ApplyError> {
        cleanup_result(&self.failures).map_err(bridge_error)?;
        let plugin = &self.registration.plugin;
        let cleanup_ctx = PluginContext {
            generation: self.registration.applies.fetch_add(1, Ordering::SeqCst) + 1,
            fiber_id: self.registration.fiber_id,
        };
        // 先提交兜底清理，再提交 stop：上游 LIFO 保证 stop 在兜底之前。
        // 两个独立义务让 stop 返回错误/恐慌时仍会继续 effect_cleanup。
        let cleanup_plugin = plugin.clone();
        let failures = self.failures.clone();
        ctx.effect(move || async move {
            cleanup_call(
                format!("{} cleanup", cleanup_plugin.meta().name),
                failures,
                async move { cleanup_plugin.effect_cleanup(&cleanup_ctx).await },
            )
            .await
        })
        .map_err(bridge_error)?;
        let stop_plugin = plugin.clone();
        let failures = self.failures.clone();
        ctx.effect(move || async move {
            cleanup_call(
                format!("{} stop", stop_plugin.meta().name),
                failures,
                async move { stop_plugin.stop().await },
            )
            .await
        })
        .map_err(bridge_error)?;
        // 在 start 之前已登记回收责任，部分失败和 apply panic 同样由上游回收。
        plugin.start_with_context(ctx).await.map_err(bridge_error)
    }
}

enum Command {
    Start(Vec<Arc<Registration>>, oneshot::Sender<Result<()>>),
    Restart(oneshot::Sender<Result<()>>),
    Stop(oneshot::Sender<Result<()>>),
}

struct RuntimeOwner {
    root: Context,
    active: Vec<(PluginId, FiberHandle)>,
    failures: Arc<parking_lot::Mutex<Vec<String>>>,
}

impl RuntimeOwner {
    async fn stop(&mut self) -> Result<()> {
        let mut retained = Vec::new();
        // 不持有图锁跨 await。失败的 dispose 控制柄仍保留，以便后续重试。
        while let Some((id, fiber)) = self.active.pop() {
            if let Err(error) = fiber.dispose().await {
                self.failures
                    .lock()
                    .push(format!("{} dispose: {error}", fiber.name()));
                retained.push((id, fiber));
            }
        }
        retained.reverse();
        self.active = retained;
        self.check_cleanup()
    }

    fn check_cleanup(&self) -> Result<()> {
        // Failures are not consumed: cleanup obligations are at-most-once and
        // an error/panic does not prove the external resource stopped.
        cleanup_result(&self.failures)
    }

    async fn rollback(&mut self, error: anyhow::Error) -> Result<()> {
        match self.stop().await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(error.context(format!("rollback also failed: {cleanup}"))),
        }
    }

    async fn start(&mut self, registrations: Vec<Arc<Registration>>) -> Result<()> {
        self.check_cleanup()?;
        // 所有 prepare 在本次新 admission 之前完成；预验证失败不碰已有运行项。
        let mut prepared = Vec::new();
        let mut ids = HashSet::new();
        for registration in registrations {
            let id = registration.plugin.meta().id.clone();
            anyhow::ensure!(ids.insert(id.clone()), "duplicate plugin id");
            if self.active.iter().any(|(active, _)| active == &id) {
                continue;
            }
            let adapter = CordisAdapter {
                registration,
                failures: self.failures.clone(),
            };
            std::panic::catch_unwind(AssertUnwindSafe(|| adapter.prepare(())))
                .map_err(|payload| {
                    anyhow::anyhow!("plugin preparation panicked: {}", panic_diagnostic(payload))
                })?
                .context("plugin preparation failed")?;
            prepared.push((id, PreparedPlugin::from_input(adapter, ())));
        }
        // Stage only the new batch. A rejected addition must not unload the
        // previously healthy fibers. This is not atomic hook-graph activation.
        let existing = self.active.len();
        for (id, plugin) in prepared {
            match self.root.spawn(plugin).await {
                Ok(fiber) => self.active.push((id, fiber)),
                Err(error) => {
                    let old = self.active.drain(..existing).collect();
                    let result = self.rollback(error.into()).await;
                    let retained = std::mem::replace(&mut self.active, old);
                    self.active.extend(retained);
                    return result;
                }
            }
        }
        Ok(())
    }

    async fn restart(&mut self) -> Result<()> {
        self.check_cleanup()?;
        for (_, fiber) in &self.active {
            if let Err(error) = fiber.restart().await {
                return self.rollback(error.into()).await;
            }
        }
        self.check_cleanup()
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        while let Some(command) = commands.recv().await {
            // 应用所有权在此 actor，不随调用者放弃 oneshot 等待而丢失句柄。
            // 不超时伪报成功；插件清理若不返回，结果仍未知且后续操作排队。
            let (reply, result) = match command {
                Command::Start(plugins, reply) => (reply, self.start(plugins).await),
                Command::Restart(reply) => (reply, self.restart().await),
                Command::Stop(reply) => (reply, self.stop().await),
            };
            if let Err(unobserved) = reply.send(result) {
                if unobserved.is_err() {
                    tracing::warn!("plugin lifecycle failed after caller abandoned its wait");
                }
            }
        }
        // 注册表 Drop 关闭通道；在执行器仍运行时显式完成 teardown，不依赖 Fiber Drop。
        if self.stop().await.is_err() {
            tracing::warn!("plugin owner teardown reported cleanup failure");
        }
    }
}

/// 安装项可同步发现；真实 Fiber 仅由无锁串行生命周期 actor 持有。
/// 正常退出必须 await stop_all；进程/外部 Tokio runtime 被杀不保证外部效果回收。
pub struct PluginRegistry {
    plugins: parking_lot::RwLock<Vec<Arc<Registration>>>,
    commands: OnceLock<mpsc::UnboundedSender<Command>>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self {
            plugins: parking_lot::RwLock::new(vec![]),
            commands: OnceLock::new(),
        }
    }

    pub fn register(&self, plugin: Arc<dyn Plugin>) {
        self.plugins.write().push(Arc::new(Registration {
            plugin,
            fiber_id: EntityId::new(),
            applies: AtomicU64::new(0),
        }));
    }

    pub fn list(&self) -> Vec<Arc<dyn Plugin>> {
        self.plugins
            .read()
            .iter()
            .map(|registration| registration.plugin.clone())
            .collect()
    }

    fn owner(&self) -> &mpsc::UnboundedSender<Command> {
        self.commands.get_or_init(|| {
            let (sender, receiver) = mpsc::unbounded_channel();
            let owner = RuntimeOwner {
                root: Context::new(),
                active: Vec::new(),
                failures: Arc::new(parking_lot::Mutex::new(Vec::new())),
            };
            tokio::spawn(owner.run(receiver));
            sender
        })
    }

    pub async fn start_all(&self) -> Result<()> {
        let registrations = self.plugins.read().clone();
        let (reply, receive) = oneshot::channel();
        self.owner()
            .send(Command::Start(registrations, reply))
            .map_err(|_| anyhow::anyhow!("plugin owner unavailable; outcome unknown"))?;
        receive
            .await
            .context("plugin owner stopped; start outcome unknown")?
    }

    /// 同一真实 Fiber 上替换 generation；不是再次叠加 start。
    pub async fn restart_all(&self) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.owner()
            .send(Command::Restart(reply))
            .map_err(|_| anyhow::anyhow!("plugin owner unavailable; outcome unknown"))?;
        receive
            .await
            .context("plugin owner stopped; restart outcome unknown")?
    }

    pub async fn stop_all(&self) -> Result<()> {
        let Some(owner) = self.commands.get() else {
            return Ok(());
        };
        let (reply, receive) = oneshot::channel();
        owner
            .send(Command::Stop(reply))
            .map_err(|_| anyhow::anyhow!("plugin owner unavailable; cleanup outcome unknown"))?;
        receive
            .await
            .context("plugin owner stopped; cleanup outcome unknown")?
    }
}

impl Default for PluginRegistry {
    fn default() -> Self {
        Self::new()
    }
}
