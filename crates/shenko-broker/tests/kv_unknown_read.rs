//! The real broker's unknown-read retention, driven through the actual dispatch
//! path — docs/specs/quota-resources.md#spec-13-3 (readers stay charged), docs/specs/inference.md#spec-41-9, docs/specs/inference.md#spec-41-10 (unknown outcomes keep
//! their charge), docs/specs/cognition-units.md#spec-51-5, docs/specs/cognition-units.md#spec-51-10 (mechanism facts, no new permission gate).
//!
//! Every statement below is made about a product seam, never about source text:
//!
//! - discovery and adoption: `Broker::probe_capabilities` (real `GET /v1/models`
//!   plus `GET /metrics` over the wire) → `Broker::adopt_capabilities` →
//!   `ResourceDomain::apply_kv_capacity` + `KvAllocator::adopt_identity` +
//!   `KvAllocator::apply_capabilities` + `KvAllocator::rebalance`.
//! - residency: `execute_request` → `KvAllocator::begin_read` (a running request
//!   is a reader) → `observe_kv` → `KvAllocator::acquire` (the lease comes from
//!   the input tokens the provider itself reported).
//! - unknown outcome: `KvAllocator::hold_unknown_read` keeps the read hold and
//!   records `request_id → context_id`; `KvPoolState::unknown_reads`.
//! - contention: `BrokerHandle::rebalance_kv` → `KvAllocator::reclaim`, whose
//!   eviction filter is `state == Resident && readers == 0 && !has_unknown_read`.
//! - release: `BrokerHandle::reconcile_recorded` → the original durable reader's
//!   terminal evidence → `KvAllocator::release_unknown_read`.
//!
//! The fixture is a scripted HTTP surface; no model, account or external
//! deployment is contacted. This file is source-only when handed over: it has
//! never been compiled or run by the worker that wrote it.
//!
//! Not claimed here: `KvAllocator::release`'s `UnresolvedUnknownRead` refusal is
//! exercised by `tests/kv_capability.rs` at the allocator level (the broker
//! handle deliberately exposes no raw release entry point, so this file proves
//! the same fact through rebalance/reclaim and through the durable charge).
//!
//! The pool numbers are engine configuration, not measurements: 125 attention
//! blocks × 16 tokens = a 2000-token pool, while the model advertises a 32768
//! context window. The two never collapse into each other.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use shenko_broker::{
    Broker, BrokerHandle, CapacityProvenance, DispatchFence, EffectiveInference, InferenceMessage,
    InferencePayload, InferencePriority, InferenceReceiptSink, InferenceRecoverySource,
    InferenceRequest, InferenceResult, InferenceStatus, KvPoolState, LeaseState, ProviderBinding,
    RequestPurpose, RetentionGuarantee, TerminationEvidence, TerminationKind,
};
use shenko_types::ids::{ControllerEpoch, EntityId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::oneshot;

const MODEL: &str = "fixture-kv";

/// The pool every `Usage` completion is charged against.
const POOL_TOKENS: u64 = 125 * 16;

/// The per-request ceiling the backend advertises. It is never the pool.
const MODEL_CONTEXT_WINDOW: u64 = 32_768;

/// The cache-configuration sample a vLLM-family engine exposes. The whole line
/// is retained as evidence by the real probe; only `num_gpu_blocks` and
/// `block_size` produce the figure.
const METRICS: &str = concat!(
    "# HELP vllm:cache_config_info Cache configuration information.\n",
    "# TYPE vllm:cache_config_info gauge\n",
    "vllm:cache_config_info{block_size=\"16\",cache_dtype=\"fp8_e4m3\",",
    "enable_prefix_caching=\"True\",engine=\"0\",num_gpu_blocks=\"125\"} 1.0\n",
);

/// One scripted answer to a chat completion, in arrival order.
enum ChatReply {
    /// A terminal 200 that reports this many input tokens.
    Usage(u64),
    /// The provider explicitly rejected the call.
    RateLimited,
    /// The request reached the backend and the connection died without an
    /// answer: a genuinely unknown outcome.
    DropWithoutResponse,
}

struct Fixture {
    chat: Mutex<VecDeque<ChatReply>>,
    chat_bodies: Mutex<Vec<Value>>,
    metrics_available: AtomicBool,
}

impl Fixture {
    fn new(chat: Vec<ChatReply>) -> Arc<Self> {
        Arc::new(Self {
            chat: Mutex::new(chat.into()),
            chat_bodies: Mutex::new(Vec::new()),
            metrics_available: AtomicBool::new(false),
        })
    }

    fn sent_bodies(&self) -> Vec<Value> {
        self.chat_bodies.lock().expect("fixture bodies poisoned").clone()
    }
}

async fn spawn_fixture(fixture: Arc<Fixture>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let fixture = Arc::clone(&fixture);
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut socket).await else {
                    return;
                };
                let path = head
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                match path.as_str() {
                    "/v1/models" => {
                        respond(&mut socket, 200, "application/json", &models_body()).await;
                    }
                    "/metrics" => {
                        if fixture.metrics_available.load(Ordering::SeqCst) {
                            respond(&mut socket, 200, "text/plain; version=0.0.4", METRICS).await;
                        } else {
                            // This engine has not exposed a metrics surface yet.
                            respond(&mut socket, 404, "text/plain", "").await;
                        }
                    }
                    "/v1/chat/completions" => {
                        let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
                        fixture
                            .chat_bodies
                            .lock()
                            .expect("fixture bodies poisoned")
                            .push(parsed);
                        let reply = fixture.chat.lock().expect("fixture chat poisoned").pop_front();
                        match reply {
                            Some(ChatReply::Usage(tokens)) => {
                                respond(
                                    &mut socket,
                                    200,
                                    "application/json",
                                    &completion_body(tokens),
                                )
                                .await;
                            }
                            Some(ChatReply::RateLimited) => {
                                respond(
                                    &mut socket,
                                    429,
                                    "application/json",
                                    "{\"error\":{\"message\":\"rate limited\",\"type\":\"rate_limit_error\"}}",
                                )
                                .await;
                            }
                            // No answer at all: the caller cannot learn the
                            // backend's outcome from this connection.
                            Some(ChatReply::DropWithoutResponse) | None => {}
                        }
                    }
                    _ => {
                        respond(&mut socket, 404, "text/plain", "").await;
                    }
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://{address}")
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<(String, Vec<u8>)> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut parsed: Option<(usize, String, usize)> = None;
    loop {
        if parsed.is_none() {
            if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&bytes[..end]).to_string();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                parsed = Some((end + 4, head, length));
            }
        }
        if let Some((start, _, length)) = &parsed {
            if bytes.len() >= start + length {
                break;
            }
        }
        let mut chunk = [0_u8; 1024];
        match socket.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
            Err(_) => return None,
        }
    }
    let (start, head, length) = parsed?;
    let end = (start + length).min(bytes.len());
    Some((head, bytes[start..end].to_vec()))
}

async fn respond(
    socket: &mut tokio::net::TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Status",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

fn models_body() -> String {
    json!({
        "object": "list",
        "data": [{"id": MODEL, "max_model_len": MODEL_CONTEXT_WINDOW}],
    })
    .to_string()
}

fn completion_body(prompt_tokens: u64) -> String {
    json!({
        "id": "fixture-kv-completion",
        "choices": [{
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 4},
    })
    .to_string()
}

/// The durable receipt owner of one request: the broker's `record` writes the
/// ledger row, and `read_terminal_result` is the only evidence
/// `BrokerHandle::reconcile_recorded` may act on.
///
/// The terminal evidence of a *dropped* connection is not invented here: it is
/// a later durable write of the same request's original outcome, exactly what a
/// ledger reader returns once the real completion was recorded out of band.
struct LedgerReceipt {
    request_id: EntityId,
    binding: ProviderBinding,
    payload: InferencePayload,
    writes: Mutex<Vec<InferenceResult>>,
}

impl LedgerReceipt {
    fn new(
        request_id: EntityId,
        binding: &ProviderBinding,
        payload: &InferencePayload,
    ) -> Arc<Self> {
        Arc::new(Self {
            request_id,
            binding: binding.clone(),
            payload: payload.clone(),
            writes: Mutex::new(Vec::new()),
        })
    }

    fn sink(self: &Arc<Self>) -> Arc<dyn InferenceReceiptSink> {
        let sink: Arc<dyn InferenceReceiptSink> = self.clone();
        sink
    }

    /// Record the original terminal outcome of this request in the ledger.
    fn record_terminal(&self, result: InferenceResult) {
        self.writes.lock().expect("ledger receipt poisoned").push(result);
    }

    fn write_count(&self) -> usize {
        self.writes.lock().expect("ledger receipt poisoned").len()
    }
}

impl InferenceRecoverySource for LedgerReceipt {
    fn request_id(&self) -> EntityId {
        self.request_id
    }

    fn binding(&self) -> &ProviderBinding {
        &self.binding
    }

    fn payload(&self) -> &InferencePayload {
        &self.payload
    }

    fn read_terminal_result(&self) -> anyhow::Result<Option<InferenceResult>> {
        let writes = self.writes.lock().expect("ledger receipt poisoned");
        Ok(writes
            .iter()
            .rev()
            .find(|result| result.termination.kind != TerminationKind::Unknown)
            .cloned())
    }
}

impl InferenceReceiptSink for LedgerReceipt {
    fn may_dispatch(&self) -> anyhow::Result<DispatchFence> {
        Ok(DispatchFence::Send)
    }

    fn record(&self, result: &InferenceResult) -> anyhow::Result<()> {
        self.writes.lock().expect("ledger receipt poisoned").push(result.clone());
        Ok(())
    }
}

/// The terminal evidence of one request: the provider finished, under the same
/// seal the request was sent with.
fn terminal_evidence(
    request_id: EntityId,
    binding: &ProviderBinding,
    payload: &InferencePayload,
    output_tokens: u64,
) -> InferenceResult {
    let max_tokens = payload.max_tokens.unwrap_or_else(|| binding.default_max_tokens());
    InferenceResult {
        request_id,
        status: InferenceStatus::Success,
        // The verdict, not a replay of the response body.
        response: None,
        effective: EffectiveInference {
            provider_binding: binding.clone(),
            profile_id: binding.profile_id().to_string(),
            profile_revision: binding.profile_revision(),
            api_version: binding.api_version().map(str::to_owned),
            max_tokens,
            native_options: payload.native_options.clone(),
        },
        termination: TerminationEvidence {
            kind: TerminationKind::ProviderTerminal,
            stop_reason: Some("stop".into()),
            max_tokens: Some(max_tokens),
            output_tokens: Some(output_tokens),
            length: false,
        },
        purpose: RequestPurpose::Subject,
        stream: None,
        quota_headers: Vec::new(),
    }
}

fn text_payload(max_tokens: u64, native_options: BTreeMap<String, Value>) -> InferencePayload {
    InferencePayload {
        system_prompt: String::new(),
        messages: vec![InferenceMessage::text("user", "fixture input")],
        max_tokens: Some(max_tokens),
        tools: Vec::new(),
        native_options,
    }
}

fn request(
    handle: &BrokerHandle,
    request_id: EntityId,
    conversation_id: EntityId,
    payload: InferencePayload,
) -> (InferenceRequest, oneshot::Receiver<InferenceResult>) {
    let (result_tx, result_rx) = oneshot::channel();
    let request = InferenceRequest {
        request_id,
        agent_id: EntityId::new(),
        conversation_id,
        provider_binding: handle.binding().clone(),
        priority: InferencePriority::Subject,
        epoch: ControllerEpoch(1),
        payload,
        result_tx,
    };
    (request, result_rx)
}

async fn await_result(result_rx: oneshot::Receiver<InferenceResult>) -> InferenceResult {
    tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("the broker produced a terminal result")
        .expect("the result channel stayed open")
}

/// One admitted turn through the real broker, optionally with its durable
/// receipt owner installed at submission.
async fn turn(
    handle: &BrokerHandle,
    request_id: EntityId,
    conversation_id: EntityId,
    payload: InferencePayload,
    ledger: Option<&Arc<LedgerReceipt>>,
) -> InferenceResult {
    let (request, result_rx) = request(handle, request_id, conversation_id, payload);
    match ledger {
        Some(ledger) => handle.submit_recorded(request, ledger.sink()).unwrap(),
        None => handle.submit(request).unwrap(),
    }
    await_result(result_rx).await
}

fn lease(pool: &KvPoolState, context_id: &str) -> (LeaseState, u32, u64) {
    let lease = pool
        .leases
        .get(context_id)
        .expect("the context holds a residency lease");
    (lease.state, lease.readers, lease.required_units)
}

fn assert_unknown_hold(pool: &KvPoolState, request_id: EntityId, context_id: &str) {
    let key = request_id.to_string();
    assert_eq!(
        pool.unknown_reads.get(&key).map(String::as_str),
        Some(context_id),
        "the unknown outcome is recorded against the original request id and its context"
    );
    assert!(
        pool.has_unknown_read(context_id),
        "the context is still named by an unresolved unknown read"
    );
    let (state, readers, _) = lease(pool, context_id);
    assert_eq!(state, LeaseState::Resident, "the context stays resident");
    assert!(readers > 0, "the unknown read keeps the read hold open");
}

#[tokio::test(flavor = "current_thread")]
async fn a_real_unknown_read_stays_charged_and_only_its_own_reconciliation_releases_it() {
    let unknown_request = EntityId::new();
    let conversation = EntityId::new();
    let conversation_key = conversation.to_string();
    let other_conversation = EntityId::new();
    let other_key = other_conversation.to_string();
    let control_conversation = EntityId::new();
    let control_key = control_conversation.to_string();
    let contended_conversation = EntityId::new();
    let contended_key = contended_conversation.to_string();

    // What the backend will answer, in arrival order. The reserved-option turn
    // never reaches the socket, so only six answers are consumed.
    let fixture = Fixture::new(vec![
        // 1. the conversation's first turn: 1600 input tokens become its lease
        ChatReply::Usage(1600),
        // 2. a second context, naming a residency option the backend never
        //    demonstrated even though it accepts the field
        ChatReply::Usage(300),
        // 3. the same first conversation again; the connection dies
        ChatReply::DropWithoutResponse,
        // 4. a control context
        ChatReply::Usage(100),
        // 5. an explicit provider rejection for that control context
        ChatReply::RateLimited,
        // 6. a request that cannot get residency while the hold is charged
        ChatReply::Usage(1200),
    ]);
    let base_url = spawn_fixture(Arc::clone(&fixture)).await;
    let provider = shenko_broker::LocalModelProvider::new(base_url, MODEL.to_string());
    let (broker, handle) = Broker::with_local_provider(provider, 4).expect("the fixture binds");

    // --- discovery: the model window is not the pool -------------------------
    let before = broker.probe_capabilities().await.expect("the first probe answers");
    assert_eq!(
        before.observed_kv_units(),
        None,
        "an engine without a metrics surface leaves the pool unobserved"
    );
    let pool = handle.kv_snapshot();
    assert_eq!(pool.capacity_units, None, "unknown is never promoted to a figure");
    assert_eq!(pool.provenance, CapacityProvenance::Unknown);
    assert_ne!(pool.capacity_units, Some(MODEL_CONTEXT_WINDOW));
    assert_eq!(
        before
            .raw_evidence
            .iter()
            .filter_map(|document| document.get("data"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(|entry| entry.get("max_model_len"))
            .filter_map(Value::as_u64)
            .next(),
        Some(MODEL_CONTEXT_WINDOW),
        "the ceiling really was in the evidence; it just never becomes the pool"
    );
    assert!(
        before.capacity(shenko_broker::CapacityPool::ContextKv).is_some_and(|entry| entry.value.is_none()),
        "the snapshot carries an explicit unknown ContextKv entry, not zero"
    );

    // The engine now exposes its cache configuration.
    fixture.metrics_available.store(true, Ordering::SeqCst);
    let after = broker.probe_capabilities().await.expect("the second probe answers");
    assert_eq!(after.observed_kv_units(), Some(POOL_TOKENS));
    let pool = handle.kv_snapshot();
    assert_eq!(pool.capacity_units, Some(POOL_TOKENS));
    assert_eq!(pool.provenance, CapacityProvenance::Configured);
    assert_ne!(pool.capacity_units, Some(MODEL_CONTEXT_WINDOW));
    assert_eq!(
        pool.identity.engine_identity.as_deref(),
        Some("unclassified"),
        "the pool adopts the physical domain's engine identity"
    );

    // Accepting the field is not demonstrating the operation: every residency
    // control stays undemonstrated even though the wire will accept `cache_salt`.
    let capabilities = handle.kv_capabilities();
    assert!(!capabilities.cache_salt);
    assert!(!capabilities.prompt_cache_key);
    assert!(!capabilities.kv_transfer_params);
    assert!(!capabilities.hard_retention);
    assert!(!capabilities.supports_pause);
    assert!(!capabilities.supports_spill);

    let run = tokio::spawn(broker.run());

    // --- 1. an admitted turn acquires the context's residency -----------------
    let first = turn(
        &handle,
        EntityId::new(),
        conversation,
        text_payload(64, BTreeMap::new()),
        None,
    )
    .await;
    assert_eq!(first.status, InferenceStatus::Success);
    assert_eq!(first.termination.kind, TerminationKind::ProviderTerminal);
    let pool = handle.kv_snapshot();
    assert_eq!(
        lease(&pool, &conversation_key),
        (LeaseState::Resident, 0, 1600),
        "the lease comes from the input tokens the provider reported"
    );
    assert_eq!(pool.occupied_units(), 1600);
    assert!(!pool.has_unknown_read(&conversation_key));
    assert_eq!(
        pool.identity.engine_identity, pool.leases[&conversation_key].identity.engine_identity,
        "the lease carries the engine identity the pool was discovered under"
    );
    assert_eq!(
        pool.leases[&conversation_key].identity.model.as_deref(),
        Some(MODEL)
    );

    // --- 2. an undemonstrated affinity option keeps only a soft claim ---------
    let affinity = BTreeMap::from([("cache_salt".to_string(), json!("kv-salt-fixture"))]);
    let second = turn(
        &handle,
        EntityId::new(),
        other_conversation,
        text_payload(64, affinity),
        None,
    )
    .await;
    assert_eq!(second.status, InferenceStatus::Success);
    let pool = handle.kv_snapshot();
    assert_eq!(pool.occupied_units(), 1900);
    let other = pool.leases.get(&other_key).expect("the second context is resident");
    assert_eq!(other.required_units, 300);
    assert!(
        other
            .affinity_gap
            .as_deref()
            .is_some_and(|gap| gap.contains("cache_salt")),
        "an accepted HTTP field is recorded as a gap, not as residency"
    );
    assert_eq!(other.retention.guarantee, RetentionGuarantee::Advisory);
    assert_eq!(
        fixture.sent_bodies().last().and_then(|body| body.get("cache_salt")),
        Some(&json!("kv-salt-fixture")),
        "the option did reach the wire verbatim"
    );
    assert!(!handle.kv_capabilities().cache_salt);

    // --- 3. the unknown outcome keeps the read and the charge -----------------
    let unknown_ledger = {
        let ledger = LedgerReceipt::new(
            unknown_request,
            handle.binding(),
            &text_payload(64, BTreeMap::new()),
        );
        ledger
    };
    let unknown = turn(
        &handle,
        unknown_request,
        conversation,
        text_payload(64, BTreeMap::new()),
        Some(&unknown_ledger),
    )
    .await;
    assert_eq!(unknown.status, InferenceStatus::Failed);
    assert_eq!(unknown.termination.kind, TerminationKind::Unknown);
    assert_eq!(
        unknown_ledger.write_count(),
        1,
        "the unknown outcome itself was durably recorded"
    );

    let pool = handle.kv_snapshot();
    assert_unknown_hold(&pool, unknown_request, &conversation_key);
    assert_eq!(pool.unknown_reads.len(), 1);
    assert_eq!(pool.occupied_units(), 1900, "the held units stay charged");
    assert_eq!(handle.active_count(), 1, "the unknown capacity debt stays armed");

    // An unknown outcome is never permission to send the same request again.
    let (replay, _replay_rx) = request(
        &handle,
        unknown_request,
        conversation,
        text_payload(64, BTreeMap::new()),
    );
    let replay_error = handle.submit(replay).unwrap_err().to_string();
    assert!(
        replay_error.contains("already submitted or uncertain")
            && replay_error.contains("reconcile original identity"),
        "unexpected replay refusal: {replay_error}"
    );
    assert_eq!(fixture.sent_bodies().len(), 3, "no second wire attempt was made");

    // --- 4. positive controls: a known 429 and a never-dispatched call ---------
    let control_turn = turn(
        &handle,
        EntityId::new(),
        control_conversation,
        text_payload(64, BTreeMap::new()),
        None,
    )
    .await;
    assert_eq!(control_turn.status, InferenceStatus::Success);
    let pool = handle.kv_snapshot();
    assert_eq!(lease(&pool, &control_key), (LeaseState::Resident, 0, 100));
    assert_eq!(pool.occupied_units(), 2000);

    let rejected = turn(
        &handle,
        EntityId::new(),
        control_conversation,
        text_payload(64, BTreeMap::new()),
        None,
    )
    .await;
    assert_eq!(rejected.status, InferenceStatus::Rejected { http_status: 429 });
    assert_eq!(rejected.termination.kind, TerminationKind::TransportRejected);
    let pool = handle.kv_snapshot();
    assert_eq!(
        lease(&pool, &control_key),
        (LeaseState::Resident, 0, 100),
        "a known rejection drains its reader and creates no hold"
    );
    assert!(!pool.has_unknown_read(&control_key));
    assert_eq!(pool.unknown_reads.len(), 1, "only the unknown outcome holds");
    assert_eq!(handle.active_count(), 1);

    let reserved = BTreeMap::from([("messages".to_string(), json!([{"role": "user"}]))]);
    let never_dispatched = turn(
        &handle,
        EntityId::new(),
        control_conversation,
        text_payload(64, reserved),
        None,
    )
    .await;
    assert_eq!(
        never_dispatched.status,
        InferenceStatus::InvalidRequest {
            code: shenko_broker::WireErrorCode::ReservedNativeOption
        }
    );
    assert_eq!(never_dispatched.termination.kind, TerminationKind::NeverDispatched);
    let pool = handle.kv_snapshot();
    assert_eq!(
        lease(&pool, &control_key),
        (LeaseState::Resident, 0, 100),
        "a request that provably never left drains its reader"
    );
    assert!(!pool.has_unknown_read(&control_key));
    assert_eq!(pool.unknown_reads.len(), 1);
    assert_eq!(
        fixture.sent_bodies().len(),
        5,
        "the reserved-option call never reached the wire"
    );

    // --- 5. genuine contention: the pool shrinks under the hold ---------------
    assert!(handle.domain().apply_kv_capacity(Some(900), CapacityProvenance::Configured));
    assert_eq!(handle.kv_snapshot().capacity_units, Some(900));
    handle.rebalance_kv().expect("the rebalance plan is computable");
    let pool = handle.kv_snapshot();
    assert_eq!(
        lease(&pool, &conversation_key),
        (LeaseState::Resident, 1, 1600),
        "the unknown read is not an eviction candidate even under a shrunken pool"
    );
    assert_unknown_hold(&pool, unknown_request, &conversation_key);
    assert_eq!(
        pool.leases[&other_key].state,
        LeaseState::ReclaimPending,
        "the honest candidates really were reclaimed"
    );
    assert_eq!(pool.leases[&control_key].state, LeaseState::ReclaimPending);
    assert_eq!(pool.occupied_units(), 2000);
    assert_eq!(pool.reclaim_pending_units(), 400);
    assert!(
        pool.occupied_units() > pool.capacity_units.unwrap_or(0),
        "the excess is reported honestly instead of being faked by evicting a live read"
    );

    // A completed call that needs the space cannot get residency while the
    // unknown read keeps it charged. It is not gated: the call itself runs.
    let contended = turn(
        &handle,
        EntityId::new(),
        contended_conversation,
        text_payload(64, BTreeMap::new()),
        None,
    )
    .await;
    assert_eq!(
        contended.status,
        InferenceStatus::Success,
        "no permission gate denies the call itself"
    );
    let pool = handle.kv_snapshot();
    assert!(
        !pool.leases.contains_key(&contended_key),
        "the pool refused residency instead of over-committing"
    );
    assert_unknown_hold(&pool, unknown_request, &conversation_key);
    assert_eq!(pool.occupied_units(), 2000);

    // --- 6. only the same request's terminal evidence releases it -------------
    let unrelated = handle
        .reconcile_recorded(EntityId::new())
        .expect_err("an unrelated request id has no original durable reader");
    assert!(
        unrelated.to_string().contains("no original durable recovery reader"),
        "unexpected refusal: {unrelated}"
    );
    assert_unknown_hold(&handle.kv_snapshot(), unknown_request, &conversation_key);

    // A lost receipt is not terminal evidence.
    handle
        .reconcile_recorded(unknown_request)
        .expect("the original reader is installed");
    let pool = handle.kv_snapshot();
    assert_unknown_hold(&pool, unknown_request, &conversation_key);
    assert_eq!(pool.occupied_units(), 2000);
    assert_eq!(handle.active_count(), 1, "the charge survives a lost receipt");

    // The same request's real completion arrives in the ledger.
    unknown_ledger.record_terminal(terminal_evidence(
        unknown_request,
        handle.binding(),
        &text_payload(64, BTreeMap::new()),
        4,
    ));
    handle
        .reconcile_recorded(unknown_request)
        .expect("the terminal evidence matches the original seal");

    let pool = handle.kv_snapshot();
    assert!(pool.unknown_reads.is_empty(), "the hold is drained");
    assert!(!pool.has_unknown_read(&conversation_key));
    assert_eq!(
        lease(&pool, &conversation_key),
        (LeaseState::Resident, 0, 1600),
        "the read hold ends; the lease itself is only releasable, not already gone"
    );
    assert_eq!(handle.active_count(), 0, "the unknown capacity debt is settled");
    assert!(handle.domain().quarantined_debt().is_empty());

    // The release happens exactly once: the reader is retired.
    assert!(handle.reconcile_recorded(unknown_request).is_err());

    // Now the same rebalance that could not evict it does evict it.
    handle.rebalance_kv().expect("the post-reconciliation plan is computable");
    let pool = handle.kv_snapshot();
    assert_eq!(
        pool.leases[&conversation_key].state,
        LeaseState::ReclaimPending,
        "with the hold gone the context is an ordinary eviction candidate"
    );
    assert_eq!(pool.reclaim_pending_units(), 2000);

    run.abort();
    let _ = run.await;
}
