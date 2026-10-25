//! Inference broker — fair asynchronous dispatch over one compute budget.
//! SPEC §13, §22.1, §41.8–41.10, §47.
//!
//! Provider selection is fixed at construction. No provider is a typed
//! unavailable binding, never a synthetic successful model response.
use anyhow::Result;
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::admission::{AdmissionController, AdmissionGuard, CapacitySnapshot};
use crate::local_provider::LocalModelProvider;
use crate::provider::AnthropicProvider;
use crate::request::{
    EffectiveInference, InferenceRequest, InferenceResponse, InferenceResult, InferenceStatus,
    ProviderBinding, ProviderRejection, ProviderWireError, TerminationEvidence, TerminationKind,
};

#[cfg(test)]
#[async_trait::async_trait]
trait FixtureProvider: Send + Sync {
    fn binding(&self) -> ProviderBinding;
    async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse>;
}

#[derive(Clone)]
enum ActiveProvider {
    Anthropic(AnthropicProvider),
    Local(LocalModelProvider),
    Stub,
    #[cfg(test)]
    Fixture(Arc<dyn FixtureProvider>),
}

impl ActiveProvider {
    fn binding(&self) -> ProviderBinding {
        match self {
            Self::Stub => ProviderBinding::Stub { codec: 2 },
            Self::Local(provider) => ProviderBinding::Local {
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                allow_invalid_certs: provider.allow_invalid_certs,
                serializer_revision: crate::local_provider::SERIALIZER_REVISION,
                default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
            },
            Self::Anthropic(provider) => ProviderBinding::Anthropic {
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                api_version: crate::provider::ANTHROPIC_API_VERSION.into(),
                serializer_revision: crate::provider::SERIALIZER_REVISION,
                default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
            },
            #[cfg(test)]
            Self::Fixture(provider) => provider.binding(),
        }
    }

    fn from_env() -> Result<Self> {
        if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() == Ok("1") {
            let provider = LocalModelProvider::from_env()?;
            info!(model=%provider.model, "local model provider selected (§22.1)");
            return Ok(Self::Local(provider));
        }
        if let Some(provider) = AnthropicProvider::from_env() {
            info!("Anthropic API key found — real inference enabled");
            return Ok(Self::Anthropic(provider));
        }
        info!("no inference provider configured — typed unavailable mode");
        Ok(Self::Stub)
    }

    fn terminal_stop_reason(&self, stop_reason: &str) -> bool {
        match self {
            Self::Anthropic(_) => matches!(
                stop_reason,
                "end_turn" | "max_tokens" | "stop_sequence" | "tool_use" | "pause_turn" | "refusal"
            ),
            Self::Local(_) => matches!(
                stop_reason,
                "stop" | "length" | "tool_calls" | "function_call" | "content_filter"
            ),
            #[cfg(test)]
            Self::Fixture(_) => matches!(stop_reason, "end_turn" | "stop" | "length"),
            Self::Stub => false,
        }
    }

    async fn complete(&self, req: &InferenceRequest) -> InferenceResult {
        if matches!(self, Self::Stub) {
            return InferenceResult {
                request_id: req.request_id,
                status: InferenceStatus::Unavailable,
                response: None,
                effective: EffectiveInference::for_request(req),
                termination: TerminationEvidence::never_dispatched(),
            };
        }

        let outcome = match self {
            Self::Anthropic(provider) => provider.complete(req).await,
            Self::Local(provider) => provider.complete(req).await,
            #[cfg(test)]
            Self::Fixture(provider) => provider.complete(req).await,
            Self::Stub => unreachable!("unavailable returned above"),
        };
        match outcome {
            Ok(response) => {
                let terminal = self.terminal_stop_reason(&response.stop_reason);
                provider_response(req, response, terminal)
            }
            Err(error) => failure_result(req, &error),
        }
    }
}

fn provider_response(
    req: &InferenceRequest,
    response: InferenceResponse,
    terminal: bool,
) -> InferenceResult {
    let effective = EffectiveInference::for_request(req);
    let output_tokens = response.usage.as_ref().map(|usage| usage.output_tokens);
    let termination = if terminal {
        TerminationEvidence {
            kind: TerminationKind::ProviderTerminal,
            stop_reason: Some(response.stop_reason.clone()),
            max_tokens: Some(effective.max_tokens),
            output_tokens,
            length: matches!(response.stop_reason.as_str(), "max_tokens" | "length"),
        }
    } else {
        TerminationEvidence {
            kind: TerminationKind::Unknown,
            stop_reason: (!response.stop_reason.is_empty()).then(|| response.stop_reason.clone()),
            max_tokens: Some(effective.max_tokens),
            output_tokens,
            length: false,
        }
    };
    InferenceResult {
        request_id: req.request_id,
        status: if terminal {
            InferenceStatus::Success
        } else {
            InferenceStatus::Failed
        },
        response: Some(response),
        effective,
        termination,
    }
}

fn failure_status(error: &anyhow::Error) -> InferenceStatus {
    if let Some(rejection) = error.downcast_ref::<ProviderRejection>() {
        return InferenceStatus::Rejected {
            http_status: rejection.http_status,
        };
    }
    if let Some(wire_error) = error.downcast_ref::<ProviderWireError>() {
        return InferenceStatus::InvalidRequest {
            code: wire_error.code(),
        };
    }
    InferenceStatus::Failed
}

fn failure_result(req: &InferenceRequest, error: &anyhow::Error) -> InferenceResult {
    let status = failure_status(error);
    let termination = match status {
        InferenceStatus::Rejected { .. } => TerminationEvidence {
            kind: TerminationKind::TransportRejected,
            stop_reason: None,
            max_tokens: None,
            output_tokens: None,
            length: false,
        },
        InferenceStatus::InvalidRequest { .. } => TerminationEvidence::never_dispatched(),
        _ => TerminationEvidence::unknown(),
    };
    warn!(
        ?status,
        "provider inference did not produce a successful terminal result"
    );
    InferenceResult {
        request_id: req.request_id,
        status,
        response: None,
        effective: EffectiveInference::for_request(req),
        termination,
    }
}

#[derive(Clone)]
pub struct BrokerHandle {
    tx: mpsc::UnboundedSender<InferenceRequest>,
    binding: ProviderBinding,
    admission: AdmissionController,
}

impl BrokerHandle {
    pub fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    pub fn active_count(&self) -> u64 {
        self.admission.active_count()
    }

    pub fn capacity(&self) -> CapacitySnapshot {
        self.admission.capacity()
    }

    pub fn submit(&self, req: InferenceRequest) -> Result<()> {
        anyhow::ensure!(
            req.provider_binding == self.binding,
            "MODEL_PROVIDER_BINDING_MISMATCH: request not admitted; seal a new revision"
        );
        self.tx
            .send(req)
            .map_err(|error| anyhow::anyhow!("broker: {error}"))
    }
}

/// FIFO lanes with explicit round-robin across urgency, main/child and child
/// priorities. Urgent work can go first but cannot indefinitely starve ordinary
/// work; ordinary main/child turns alternate whenever both sides are ready.
struct FairQueue {
    lanes: [VecDeque<InferenceRequest>; 4],
    main_next: bool,
    normal_child_next: bool,
    last_was_urgent: bool,
}

impl Default for FairQueue {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| VecDeque::new()),
            main_next: true,
            normal_child_next: true,
            last_was_urgent: false,
        }
    }
}

impl FairQueue {
    fn push(&mut self, req: InferenceRequest) {
        self.lanes[req.priority as usize].push_back(req);
    }

    fn is_empty(&self) -> bool {
        self.lanes.iter().all(VecDeque::is_empty)
    }

    fn pop(&mut self) -> Option<InferenceRequest> {
        let urgent_ready = !self.lanes[3].is_empty();
        let main_ready = !self.lanes[2].is_empty();
        let normal_ready = !self.lanes[1].is_empty();
        let low_ready = !self.lanes[0].is_empty();
        let ordinary_ready = main_ready || normal_ready || low_ready;

        if urgent_ready && (!self.last_was_urgent || !ordinary_ready) {
            self.last_was_urgent = true;
            return self.lanes[3].pop_front();
        }
        self.last_was_urgent = false;
        if !ordinary_ready {
            return None;
        }

        let use_main = match (main_ready, normal_ready || low_ready) {
            (true, true) => self.main_next,
            (true, false) => true,
            (false, true) => false,
            (false, false) => return None,
        };
        self.main_next = !use_main;
        if use_main {
            return self.lanes[2].pop_front();
        }

        let use_normal = match (normal_ready, low_ready) {
            (true, true) => self.normal_child_next,
            (true, false) => true,
            (false, true) => false,
            (false, false) => return None,
        };
        self.normal_child_next = !use_normal;
        self.lanes[usize::from(use_normal)].pop_front()
    }
}

struct CompletedRequest {
    req: InferenceRequest,
    result: InferenceResult,
    guard: AdmissionGuard,
}

pub struct Broker {
    rx: mpsc::UnboundedReceiver<InferenceRequest>,
    admission: AdmissionController,
    provider: ActiveProvider,
}

impl Broker {
    pub fn new() -> Result<(Self, BrokerHandle)> {
        Self::from_env_with_admission(AdmissionController::nominal())
    }

    /// Configure a compute-only concurrency budget. This does not assert a
    /// measured physical stream count or any KV residency capacity.
    pub fn with_capacity(max: u64) -> Result<(Self, BrokerHandle)> {
        Self::from_env_with_admission(AdmissionController::new(max))
    }

    fn from_env_with_admission(admission: AdmissionController) -> Result<(Self, BrokerHandle)> {
        let provider = ActiveProvider::from_env()?;
        provider.binding().validate_endpoint()?;
        Ok(Self::with_provider(provider, admission))
    }

    /// Explicit offline construction; does not inspect ambient credentials and
    /// returns typed Unavailable rather than a fabricated successful completion.
    pub fn offline() -> (Self, BrokerHandle) {
        Self::with_provider(ActiveProvider::Stub, AdmissionController::nominal())
    }

    fn with_provider(
        provider: ActiveProvider,
        admission: AdmissionController,
    ) -> (Self, BrokerHandle) {
        let (tx, rx) = mpsc::unbounded_channel();
        let binding = provider.binding();
        (
            Self {
                rx,
                admission: admission.clone(),
                provider,
            },
            BrokerHandle {
                tx,
                binding,
                admission,
            },
        )
    }

    #[cfg(test)]
    fn with_fixture_provider(provider: Arc<dyn FixtureProvider>, max: u64) -> (Self, BrokerHandle) {
        Self::with_provider(
            ActiveProvider::Fixture(provider),
            AdmissionController::new(max),
        )
    }

    fn finish(completed: CompletedRequest, unknown_allocations: &mut Vec<AdmissionGuard>) {
        let CompletedRequest { req, result, guard } = completed;
        let termination = result.termination.kind;
        // Dropping the receiver does not cancel the already executing provider
        // future. Its actual completion is still accounted before this guard.
        let _ = req.result_tx.send(result);
        match termination {
            TerminationKind::ProviderTerminal | TerminationKind::TransportRejected => {
                guard.release_terminal();
            }
            TerminationKind::NeverDispatched => guard.release_never_dispatched(),
            TerminationKind::Unknown => unknown_allocations.push(guard),
        }
    }

    pub async fn run(mut self) {
        info!("inference broker running");
        let (completed_tx, mut completed_rx) = mpsc::unbounded_channel::<CompletedRequest>();
        let mut queue = FairQueue::default();
        let mut input_closed = false;
        let mut inflight = 0_u64;
        let mut prefer_input = false;
        // Unknown outcomes remain charged for the broker lifetime. Dropping this
        // vector is also non-releasing; reconciliation must supply real proof.
        let mut unknown_allocations = Vec::new();

        enum Event {
            Completed(Option<CompletedRequest>),
            Incoming(Option<InferenceRequest>),
        }

        loop {
            // Fill only currently available slots. A full budget leaves the
            // request in FairQueue instead of losing it or failing it spuriously.
            while !queue.is_empty() {
                let Some(guard) = self.admission.try_admit() else {
                    break;
                };
                let req = queue.pop().expect("queue checked non-empty");
                let provider = self.provider.clone();
                let completed_tx = completed_tx.clone();
                tokio::spawn(async move {
                    let result = provider.complete(&req).await;
                    let _ = completed_tx.send(CompletedRequest { req, result, guard });
                });
                inflight += 1;
            }

            if input_closed && queue.is_empty() && inflight == 0 {
                break;
            }

            // One event per round, alternating priority when both channels stay
            // ready. Neither a completion flood nor an inbound flood can prevent
            // the other side from being reaped on the next round.
            let event = if prefer_input {
                tokio::select! {
                    biased;
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    else => std::future::pending::<Event>().await,
                }
            } else {
                tokio::select! {
                    biased;
                    completed = completed_rx.recv(), if inflight > 0 => Event::Completed(completed),
                    incoming = self.rx.recv(), if !input_closed => Event::Incoming(incoming),
                    else => std::future::pending::<Event>().await,
                }
            };
            match event {
                Event::Completed(completed) => {
                    if let Some(completed) = completed {
                        inflight -= 1;
                        Self::finish(completed, &mut unknown_allocations);
                    }
                    prefer_input = true;
                }
                Event::Incoming(incoming) => {
                    match incoming {
                        Some(req) => queue.push(req),
                        None => input_closed = true,
                    }
                    prefer_input = false;
                }
            }
        }
        info!("inference broker stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admission::{CapacityPool, CapacityProvenance};
    use crate::request::{
        InferenceMessage, InferencePayload, InferencePriority, TokenUsage, WireErrorCode,
    };
    use shenko_types::ids::{ControllerEpoch, EntityId};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Semaphore, mpsc, oneshot};

    struct ControlledProvider {
        binding: ProviderBinding,
        starts: mpsc::UnboundedSender<(EntityId, InferencePriority)>,
        gate: Arc<Semaphore>,
        active: AtomicU64,
        max_active: AtomicU64,
    }

    impl ControlledProvider {
        fn new() -> (
            Arc<Self>,
            mpsc::UnboundedReceiver<(EntityId, InferencePriority)>,
        ) {
            let (starts, starts_rx) = mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    binding: ProviderBinding::Local {
                        base_url: "http://127.0.0.1".into(),
                        model: "controlled-fixture".into(),
                        allow_invalid_certs: false,
                        serializer_revision: crate::local_provider::SERIALIZER_REVISION,
                        default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
                    },
                    starts,
                    gate: Arc::new(Semaphore::new(0)),
                    active: AtomicU64::new(0),
                    max_active: AtomicU64::new(0),
                }),
                starts_rx,
            )
        }

        async fn next_start(
            rx: &mut mpsc::UnboundedReceiver<(EntityId, InferencePriority)>,
        ) -> (EntityId, InferencePriority) {
            tokio::time::timeout(Duration::from_secs(2), rx.recv())
                .await
                .expect("timed out waiting for fixture dispatch")
                .expect("fixture start channel remains open")
        }
    }

    #[async_trait::async_trait]
    impl FixtureProvider for ControlledProvider {
        fn binding(&self) -> ProviderBinding {
            self.binding.clone()
        }

        async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.starts.send((req.request_id, req.priority)).unwrap();
            let _permit = self
                .gate
                .clone()
                .acquire_owned()
                .await
                .expect("fixture gate remains open");
            self.active.fetch_sub(1, Ordering::SeqCst);
            // Each request needs an explicit test release. Returning the
            // semaphore permit here would let completed work release its
            // successor and make this gate useless.
            std::mem::forget(_permit);
            Ok(InferenceResponse {
                content: "controlled fixture".into(),
                stop_reason: "end_turn".into(),
                raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                usage: Some(TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }),
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fair_rotation_real_concurrency_fifo_and_zero_capacity_queueing() {
        let submit = |handle: &BrokerHandle, priority| {
            let request_id = EntityId::new();
            let (result_tx, result_rx) = oneshot::channel();
            handle
                .submit(InferenceRequest {
                    request_id,
                    agent_id: EntityId::new(),
                    conversation_id: EntityId::new(),
                    provider_binding: handle.binding().clone(),
                    priority,
                    epoch: ControllerEpoch(1),
                    payload: InferencePayload {
                        system_prompt: String::new(),
                        messages: vec![InferenceMessage {
                            role: "user".into(),
                            content: "controlled scheduling fixture".into(),
                        }],
                        max_tokens: Some(16),
                        tools: vec![],
                        native_options: BTreeMap::new(),
                    },
                    result_tx,
                })
                .unwrap();
            (request_id, result_rx)
        };

        // C=1: continuous Subject work cannot starve Low children.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_fixture_provider(provider.clone(), 1);
        let (subject_one, subject_one_rx) = submit(&handle, InferencePriority::Subject);
        let (low_one, low_one_rx) = submit(&handle, InferencePriority::Low);
        let (subject_two, subject_two_rx) = submit(&handle, InferencePriority::Subject);
        let (low_two, low_two_rx) = submit(&handle, InferencePriority::Low);
        let run = tokio::spawn(broker.run());
        for (index, expected) in [subject_one, low_one, subject_two, low_two]
            .into_iter()
            .enumerate()
        {
            let observed = ControlledProvider::next_start(&mut starts).await;
            assert_eq!(observed.0, expected, "dispatch index {index}");
            assert_eq!(
                handle.active_count(),
                1,
                "active after dispatch index {index}"
            );
            provider.gate.add_permits(1);
        }
        for rx in [subject_one_rx, low_one_rx, subject_two_rx, low_two_rx] {
            assert_eq!(rx.await.unwrap().status, InferenceStatus::Success);
        }
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 1);
        let observer = handle.admission.clone();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observer.active_count(), 0);

        // C=2: two same-tier futures really overlap and start in FIFO order.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_fixture_provider(provider.clone(), 2);
        assert_eq!(handle.capacity().pool, CapacityPool::Compute);
        assert_eq!(handle.capacity().value, Some(2));
        assert_eq!(handle.capacity().provenance, CapacityProvenance::Configured);
        let (normal_one, normal_one_rx) = submit(&handle, InferencePriority::Normal);
        let (normal_two, normal_two_rx) = submit(&handle, InferencePriority::Normal);
        let run = tokio::spawn(broker.run());
        let observed = [
            ControlledProvider::next_start(&mut starts).await,
            ControlledProvider::next_start(&mut starts).await,
        ];
        assert_eq!(
            observed.iter().map(|entry| entry.0).collect::<Vec<_>>(),
            vec![normal_one, normal_two]
        );
        assert_eq!(provider.max_active.load(Ordering::SeqCst), 2);
        assert_eq!(handle.active_count(), 2);
        provider.gate.add_permits(2);
        assert_eq!(
            normal_one_rx.await.unwrap().status,
            InferenceStatus::Success
        );
        assert_eq!(
            normal_two_rx.await.unwrap().status,
            InferenceStatus::Success
        );
        let observer = handle.admission.clone();
        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observer.active_count(), 0);

        // C=0: work stays pending instead of receiving synthetic capacity failure.
        let (provider, mut starts) = ControlledProvider::new();
        let (broker, handle) = Broker::with_fixture_provider(provider.clone(), 0);
        let (_, mut pending_rx) = submit(&handle, InferencePriority::Subject);
        let run = tokio::spawn(broker.run());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut pending_rx)
                .await
                .is_err()
        );
        assert!(starts.try_recv().is_err());
        assert_eq!(handle.active_count(), 0);
        run.abort();
        let _ = run.await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn frozen_local_wire_length_evidence_and_unknown_occupancy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (wire_tx, wire_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut body_start = None;
            let mut body_length = 0_usize;
            loop {
                let mut chunk = [0_u8; 1024];
                let read = socket.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "request ended before complete headers/body");
                bytes.extend_from_slice(&chunk[..read]);
                if body_start.is_none() {
                    if let Some(header_end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let start = header_end + 4;
                        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
                        body_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse().ok())
                                    .flatten()
                            })
                            .unwrap();
                        body_start = Some(start);
                    }
                }
                if body_start.is_some_and(|start| bytes.len() >= start + body_length) {
                    break;
                }
            }
            let start = body_start.unwrap();
            let body: serde_json::Value =
                serde_json::from_slice(&bytes[start..start + body_length]).unwrap();
            wire_tx.send(body).unwrap();
            let raw = r#"{"id":"local-fixture","choices":[{"message":{"role":"assistant","content":"partial"},"finish_reason":"length"}],"usage":{"prompt_tokens":3,"completion_tokens":7}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                raw.len(),
                raw
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let provider = LocalModelProvider::new(format!("http://{address}"), "fixture-local".into());
        let (broker, handle) =
            Broker::with_provider(ActiveProvider::Local(provider), AdmissionController::new(1));
        let binding = handle.binding().clone();
        let payload = InferencePayload {
            system_prompt: "fixture system".into(),
            messages: vec![InferenceMessage {
                role: "user".into(),
                content: "fixture input".into(),
            }],
            max_tokens: Some(7),
            tools: vec![],
            native_options: BTreeMap::from([
                ("temperature".into(), serde_json::json!(0.25)),
                ("top_k".into(), serde_json::json!(40)),
            ]),
        };
        let request_id = EntityId::new();
        let (result_tx, result_rx) = oneshot::channel();
        let run = tokio::spawn(broker.run());
        handle
            .submit(InferenceRequest {
                request_id,
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: payload.clone(),
                result_tx,
            })
            .unwrap();

        let body = tokio::time::timeout(Duration::from_secs(2), wire_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body["model"], "fixture-local");
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["top_k"], 40);
        assert_eq!(body["max_tokens"], 7);
        server.await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.request_id, request_id);
        assert_eq!(result.status, InferenceStatus::Success);
        assert!(result.effective.matches_seal(&binding, &payload));
        assert_eq!(result.termination.kind, TerminationKind::ProviderTerminal);
        assert_eq!(result.termination.stop_reason.as_deref(), Some("length"));
        assert_eq!(result.termination.max_tokens, Some(7));
        assert_eq!(result.termination.output_tokens, Some(7));
        assert!(result.termination.length);

        // A changed model is a different seal, never an in-place provider override.
        let mut mismatched = binding.clone();
        let ProviderBinding::Local { model, .. } = &mut mismatched else {
            unreachable!()
        };
        *model = "different-revision".into();
        let (result_tx, _) = oneshot::channel();
        let error = handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: mismatched,
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: payload.clone(),
                result_tx,
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("MODEL_PROVIDER_BINDING_MISMATCH")
        );

        // Claude's dated restriction is scoped to Claude, not the local wire above.
        let sampling = BTreeMap::from([("temperature".into(), serde_json::json!(1.0))]);
        let error =
            crate::provider::merge_native_options(&mut serde_json::json!({}), &sampling, true)
                .unwrap_err();
        assert_eq!(error.code(), WireErrorCode::ForbiddenSampling);

        // The fixture is gone. Transport failure is unknown and keeps C=1 charged;
        // a later request cannot silently acquire that physical slot.
        let unknown_payload = InferencePayload {
            native_options: BTreeMap::new(),
            ..payload.clone()
        };
        let (result_tx, result_rx) = oneshot::channel();
        handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: unknown_payload.clone(),
                result_tx,
            })
            .unwrap();
        let unknown = tokio::time::timeout(Duration::from_secs(3), result_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(unknown.status, InferenceStatus::Failed);
        assert_eq!(unknown.termination.kind, TerminationKind::Unknown);
        assert!(unknown.effective.matches_seal(&binding, &unknown_payload));
        assert_eq!(handle.active_count(), 1);

        let (result_tx, mut queued_rx) = oneshot::channel();
        handle
            .submit(InferenceRequest {
                request_id: EntityId::new(),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding,
                priority: InferencePriority::Low,
                epoch: ControllerEpoch(1),
                payload: unknown_payload,
                result_tx,
            })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut queued_rx)
                .await
                .is_err()
        );
        let observer = handle.clone();
        drop(handle);
        run.abort();
        let _ = run.await;
        assert_eq!(observer.active_count(), 1);
    }
}
