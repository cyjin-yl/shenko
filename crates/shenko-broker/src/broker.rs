//! 进程内推理派发与公平轮转。依据 SPEC §§13、35.9、41.9。
//! 计算许可不等于 KV 驻留；当前不提供跨进程/重启的容量账本。
use std::collections::{BTreeMap, VecDeque};
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use parking_lot::Mutex;
use shenko_types::ids::EntityId;
use tokio::{sync::mpsc, task::JoinSet};
use tracing::{info, warn};

use crate::admission::{AdmissionController, AdmissionGuard};
use crate::local_provider::LocalModelProvider;
use crate::provider::AnthropicProvider;
use crate::request::{
    InferencePriority, InferenceRequest, InferenceResponse, InferenceResult, InferenceStatus,
};

/// 配置的是本 broker 的受控并发上限，不是已经测得的后端容量。
#[derive(Debug, Clone, Copy)]
pub struct BrokerConfig {
    pub max_concurrent: NonZeroUsize,
    /// 每等待多少次派发提高一级有效优先级；不用墙钟决定公平性。
    pub aging_rounds: NonZeroU64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: NonZeroUsize::new(1).unwrap(),
            aging_rounds: NonZeroU64::new(4).unwrap(),
        }
    }
}

impl BrokerConfig {
    pub fn new(max_concurrent: usize) -> Result<Self> {
        Ok(Self {
            max_concurrent: NonZeroUsize::new(max_concurrent)
                .context("broker 并发上限必须大于零")?,
            ..Self::default()
        })
    }

    pub fn from_env() -> Result<Self> {
        match std::env::var("SHENKO_INFERENCE_CONCURRENCY") {
            Ok(value) => Self::new(value.parse().context("broker 并发上限必须为正整数")?),
            Err(std::env::VarError::NotPresent) => Ok(Self::default()),
            Err(_) => anyhow::bail!("broker 并发配置不是有效文本"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InFlightState {
    Active,
    TerminationUnknown,
}

/// 独立于 LLM 可读；不含 prompt、endpoint、凭据或远端错误正文。
#[derive(Debug, Clone)]
pub struct BrokerSnapshot {
    pub configured_concurrency: usize,
    pub provider_configured: bool,
    pub held: u64,
    pub requests: BTreeMap<EntityId, InFlightState>,
}

struct Diagnostics {
    admission: AdmissionController,
    provider_configured: bool,
    requests: Mutex<BTreeMap<EntityId, InFlightState>>,
}

// 终止未知不能映射成 Failed/Cancelled：现有调用者会据此开始下一步。
// 保留原 oneshot 和许可，另由 snapshot 显式呈现 unknown，不自动重发。
enum ProviderOutcome {
    Terminal(Option<InferenceResponse>),
    Unknown,
}

#[async_trait]
trait InferenceProvider: Send + Sync {
    fn available(&self) -> bool {
        true
    }
    async fn complete(&self, req: &InferenceRequest) -> ProviderOutcome;
}

enum ActiveProvider {
    Anthropic(AnthropicProvider),
    Local(LocalModelProvider),
    Unavailable,
}

impl ActiveProvider {
    fn from_env() -> Self {
        if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() == Ok("1") {
            info!("已选择本地模型 provider；未据此验证容量");
            return Self::Local(LocalModelProvider::from_env());
        }
        if let Some(p) = AnthropicProvider::from_env() {
            return Self::Anthropic(p);
        }
        warn!("未配置推理 provider；请求将明确失败，不生成 stub 成功结果");
        Self::Unavailable
    }
}

#[async_trait]
impl InferenceProvider for ActiveProvider {
    fn available(&self) -> bool {
        !matches!(self, Self::Unavailable)
    }

    async fn complete(&self, req: &InferenceRequest) -> ProviderOutcome {
        let result = match self {
            Self::Anthropic(p) => p.complete(&req.payload).await,
            Self::Local(p) => p.complete(&req.payload).await,
            Self::Unavailable => return ProviderOutcome::Terminal(None),
        };
        match result {
            Ok(response) => ProviderOutcome::Terminal(Some(response)),
            // 现有 HTTP adapter 的 anyhow 错误不含可核验的静止证明。
            // 不猜测所有 HTTP 错误都无效果；宁可保守占用，不能超配。
            Err(_) => ProviderOutcome::Unknown,
        }
    }
}

#[derive(Clone)]
pub struct BrokerHandle {
    tx: mpsc::UnboundedSender<InferenceRequest>,
    diagnostics: Arc<Diagnostics>,
}

impl BrokerHandle {
    pub fn submit(&self, req: InferenceRequest) -> Result<()> {
        self.tx
            .send(req)
            .map_err(|e| anyhow::anyhow!("broker: {e}"))
    }

    pub fn snapshot(&self) -> BrokerSnapshot {
        let requests = self.diagnostics.requests.lock();
        BrokerSnapshot {
            configured_concurrency: self.diagnostics.admission.max_count() as usize,
            provider_configured: self.diagnostics.provider_configured,
            held: self.diagnostics.admission.active_count(),
            requests: requests.clone(),
        }
    }
}

struct Queued {
    req: InferenceRequest,
    seq: u64,
    entered_round: u64,
}

struct ReadyQueue {
    tiers: [VecDeque<Queued>; 4],
    seq: u64,
    round: u64,
    last_subject: bool,
    streak: usize,
    aging_rounds: u64,
}

impl ReadyQueue {
    fn new(config: BrokerConfig) -> Self {
        Self {
            tiers: Default::default(),
            seq: 0,
            round: 0,
            last_subject: false,
            streak: 0,
            aging_rounds: config.aging_rounds.get(),
        }
    }

    fn push(&mut self, req: InferenceRequest) {
        let tier = req.priority as usize;
        self.tiers[tier].push_back(Queued {
            req,
            seq: self.seq,
            entered_round: self.round,
        });
        self.seq += 1;
    }

    fn is_empty(&self) -> bool {
        self.tiers.iter().all(VecDeque::is_empty)
    }

    fn pop(&mut self) -> Option<InferenceRequest> {
        // 每级只比较队首，所以同级严格 FIFO。等待轮次最终提升到最高级，
        // 以更早入队序胜过后来的紧急请求；新高优先级仍有即时响应优势。
        let best = self
            .tiers
            .iter()
            .enumerate()
            .filter_map(|(tier, queue)| {
                queue.front().map(|q| {
                    let age = (self.round - q.entered_round) / self.aging_rounds;
                    let effective = (tier as u64 + age).min(3);
                    (tier, effective, q.seq)
                })
            })
            .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.2.cmp(&a.2)))?
            .0;

        // 当前请求类型未单列角色，Subject 级代表主体，其余级代表 child。
        // 没有真正紧急请求当选时，两侧同时 ready 不连续服务一侧超过两次。
        let subject = InferencePriority::Subject as usize;
        let mut selected = best;
        if best != InferencePriority::Urgent as usize
            && self.streak >= 2
            && (best == subject) == self.last_subject
        {
            if self.last_subject {
                selected = [0, 1]
                    .into_iter()
                    .filter(|&t| !self.tiers[t].is_empty())
                    .min_by_key(|&t| self.tiers[t].front().unwrap().seq)
                    .unwrap_or(best);
            } else if !self.tiers[subject].is_empty() {
                selected = subject;
            }
        }
        let is_subject = selected == subject;
        self.streak = if is_subject == self.last_subject {
            self.streak.saturating_add(1)
        } else {
            1
        };
        self.last_subject = is_subject;
        self.round += 1;
        self.tiers[selected].pop_front().map(|q| q.req)
    }
}

/// 一旦 armed，Drop/任务 panic/本地取消都不能作为远端静止证明。
struct Flight {
    request_id: EntityId,
    guard: Option<AdmissionGuard>,
    diagnostics: Arc<Diagnostics>,
}

impl Flight {
    fn unknown(&self) {
        self.diagnostics
            .requests
            .lock()
            .insert(self.request_id, InFlightState::TerminationUnknown);
        warn!(request_id=%self.request_id, "远端终止未知，容量仍占用；需要独立对账或确认引擎销毁");
    }

    fn terminal(mut self) {
        let mut requests = self.diagnostics.requests.lock();
        self.guard.take().unwrap().release_confirmed();
        requests.remove(&self.request_id);
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        if self.guard.is_some() {
            self.unknown();
        }
    }
}

pub struct Broker {
    rx: mpsc::UnboundedReceiver<InferenceRequest>,
    config: BrokerConfig,
    diagnostics: Arc<Diagnostics>,
    provider: Arc<dyn InferenceProvider>,
}

impl Broker {
    pub fn new() -> (Self, BrokerHandle) {
        let config = BrokerConfig::from_env().unwrap_or_else(|_| {
            warn!("无效 broker 并发配置，保守退回 C=1");
            BrokerConfig::default()
        });
        Self::with_config(config)
    }

    pub fn with_config(config: BrokerConfig) -> (Self, BrokerHandle) {
        Self::with_provider(config, Arc::new(ActiveProvider::from_env()))
    }

    fn with_provider(
        config: BrokerConfig,
        provider: Arc<dyn InferenceProvider>,
    ) -> (Self, BrokerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let diagnostics = Arc::new(Diagnostics {
            admission: AdmissionController::new(config.max_concurrent.get() as u64),
            provider_configured: provider.available(),
            requests: Mutex::new(BTreeMap::new()),
        });
        let handle = BrokerHandle {
            tx,
            diagnostics: diagnostics.clone(),
        };
        (
            Self {
                rx,
                config,
                diagnostics,
                provider,
            },
            handle,
        )
    }

    pub async fn run(mut self) {
        const BATCH: usize = 64;
        let mut queue = ReadyQueue::new(self.config);
        let mut running = JoinSet::new();
        let mut input_open = true;
        info!(
            concurrency = self.config.max_concurrent.get(),
            "推理 broker 启动；容量来源为配置，非实测"
        );

        loop {
            // 输入与完成回收都有有限工作量，不让持续输入饿死完成分支。
            for _ in 0..BATCH {
                match running.try_join_next() {
                    Some(Err(_)) => warn!("推理任务异常退出；无终态证明的容量保持占用"),
                    Some(Ok(())) => {}
                    None => break,
                }
            }
            for _ in 0..BATCH {
                match self.rx.try_recv() {
                    Ok(req) => queue.push(req),
                    Err(mpsc::error::TryRecvError::Disconnected) => {
                        input_open = false;
                        break;
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                }
            }
            for _ in 0..BATCH {
                if queue.is_empty() {
                    break;
                }
                let Some(mut guard) = self.diagnostics.admission.try_admit() else {
                    break;
                };
                let req = queue.pop().unwrap();
                guard.arm();
                self.diagnostics
                    .requests
                    .lock()
                    .insert(req.request_id, InFlightState::Active);
                let flight = Flight {
                    request_id: req.request_id,
                    guard: Some(guard),
                    diagnostics: self.diagnostics.clone(),
                };
                let provider = self.provider.clone();
                running.spawn(async move {
                    match provider.complete(&req).await {
                        ProviderOutcome::Terminal(response) => {
                            let status = if response.is_some() {
                                InferenceStatus::Success
                            } else {
                                InferenceStatus::Failed
                            };
                            flight.terminal();
                            let _ = req.result_tx.send(InferenceResult {
                                request_id: req.request_id,
                                status,
                                response,
                            });
                        }
                        ProviderOutcome::Unknown => {
                            flight.unknown();
                            // 不关闭 oneshot 来触发调用者错误重试；不通过丢 guard 伪取消。
                            std::future::pending::<()>().await;
                            drop((req, flight));
                        }
                    }
                });
            }

            if !input_open
                && queue.is_empty()
                && running.is_empty()
                && self.diagnostics.admission.active_count() == 0
            {
                break;
            }
            if !queue.is_empty()
                && self.diagnostics.admission.active_count()
                    < self.config.max_concurrent.get() as u64
            {
                tokio::task::yield_now().await;
                continue;
            }
            tokio::select! {
                biased;
                result = running.join_next(), if !running.is_empty() => {
                    if matches!(result, Some(Err(_))) { warn!("推理任务异常退出；许可未提前归还"); }
                },
                req = self.rx.recv(), if input_open => match req {
                    Some(req) => queue.push(req),
                    None => input_open = false,
                },
                // 所有任务异常退出但许可未知时，不能因 select 无分支而 panic/重发。
                else => std::future::pending::<()>().await,
            }
        }
        info!("推理 broker 已回收所有已知在途请求");
    }
}

#[cfg(test)]
mod tests;
