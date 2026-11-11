//! The execution of one admitted request — SPEC §41.9.
//!
//! The dispatch fence is checked at the send boundary, the provider runtime
//! is resolved from the sealed binding, and the terminal result is produced
//! from what the provider actually reported. A fenced request provably never
//! left; an unknown outcome keeps its capacity and stays recoverable.

use std::sync::Arc;

use tracing::warn;

use crate::kv::{KvAllocator, LeaseDemand, LeaseSide, ResidencyIdentity};
use crate::registry::{ProviderRegistry, ProviderRuntime};
use crate::request::{
    EffectiveInference, InferenceRequest, InferenceResponse, InferenceResult, InferenceStatus,
    ProviderRejection, ProviderWireError, RequestPurpose, StreamEvidence,
    TerminationEvidence, TerminationKind,
};
use crate::resource::ResourceDomain;
use crate::stream::{BoundedStreamPublisher, StreamSinkError};

use crate::broker::{DispatchFence, InferenceReceiptSink, StreamSubmission};

/// The residency context one request executes against.
///
/// It is the conversation the request belongs to, so successive turns of one
/// window renew the same lease and a different window never inherits it.
fn residency_context(request: &InferenceRequest) -> String {
    request.conversation_id.to_string()
}

/// Execute one admitted request and produce its terminal result.
pub(crate) async fn execute_request(
    request: &InferenceRequest,
    receipt_sink: Option<&Arc<dyn InferenceReceiptSink>>,
    purpose: RequestPurpose,
    stream: Option<StreamSubmission>,
    registry: &Arc<ProviderRegistry>,
    domain: &Arc<ResourceDomain>,
    kv: &Arc<KvAllocator>,
    error_sink: &crate::error_sink::ErrorSinkCell,
) -> InferenceResult {
    // A running request is a real reader of its context's residency. While it
    // runs the context is not an eviction candidate, and a release requested in
    // the meantime stays pending until this read ends (§13.3 step 6). A first
    // request for a context has no lease yet, so there is nothing to hold.
    let context_id = residency_context(request);
    let reading = {
        let mut pool = domain.kv_mut();
        kv.begin_read(&mut pool, &context_id).is_ok()
    };
    let result = execute_admitted(
        request,
        receipt_sink,
        purpose,
        stream,
        registry,
        domain,
        kv,
        error_sink,
    )
    .await;
    if reading {
        let mut pool = domain.kv_mut();
        if result.termination.kind == TerminationKind::Unknown {
            // The backend may still be reading this context: a dropped
            // connection, a deadline or an unknown dispatch fence is not proof
            // that it stopped. The read hold stays charged and off the
            // eviction list until the original terminal evidence arrives and
            // the reconciliation path drains it (§13.3 step 6, §41.9).
            kv.hold_unknown_read(&mut pool, &request.request_id.to_string(), &context_id);
        } else {
            let _ = kv.end_read(&mut pool, &context_id);
        }
    }
    result
}

/// Dispatch one admitted request and settle its terminal result.
async fn execute_admitted(
    request: &InferenceRequest,
    receipt_sink: Option<&Arc<dyn InferenceReceiptSink>>,
    purpose: RequestPurpose,
    stream: Option<StreamSubmission>,
    registry: &Arc<ProviderRegistry>,
    domain: &Arc<ResourceDomain>,
    kv: &Arc<KvAllocator>,
    error_sink: &crate::error_sink::ErrorSinkCell,
) -> InferenceResult {
    let admitted = receipt_sink.map_or(Ok(DispatchFence::Send), |sink| sink.may_dispatch());
    match admitted {
        Ok(DispatchFence::Send) => {
            let Some(runtime) = registry.runtime_for(&request.provider_binding) else {
                crate::error_sink::journal_runtime_error(
                    error_sink,
                    &crate::error_sink::report_for(
                        request,
                        purpose,
                        "INFERENCE_PROVIDER_UNAVAILABLE",
                        "no installed provider runtime can serve this binding",
                    ),
                );
                return failure_result(
                    request,
                    purpose,
                    &anyhow::Error::new(LocalDispatchError::NoProviderRuntime),
                    None,
                );
            };
            if !runtime.available() {
                return InferenceResult {
                    request_id: request.request_id,
                    status: InferenceStatus::Unavailable,
                    response: None,
                    effective: EffectiveInference::for_request(request),
                    termination: TerminationEvidence::never_dispatched(),
                    purpose,
                    stream: None,
                    quota_headers: Vec::new(),
                };
            }
            match stream {
                Some(stream) => execute_stream(request, purpose, runtime, stream, domain, kv).await,
                None => execute_complete(request, purpose, runtime, domain, kv).await,
            }
        }
        otherwise => {
            let status = match otherwise {
                Ok(DispatchFence::Fenced) => InferenceStatus::Cancelled,
                Err(error) => {
                    warn!(request_id=%request.request_id, %error, "model dispatch fence could not be checked");
                    crate::error_sink::journal_runtime_error(
                        error_sink,
                        &crate::error_sink::report_for(
                            request,
                            purpose,
                            "INFERENCE_FENCE_UNCHECKED",
                            "the dispatch fence could not be checked; the request stays recoverable",
                        ),
                    );
                    InferenceStatus::Failed
                }
                Ok(DispatchFence::Unknown) => InferenceStatus::Failed,
                Ok(DispatchFence::Send) => unreachable!("dispatched above"),
            };
            // A fenced request provably never sent; an unknown verdict means a
            // dispatch may already exist, so its terminal evidence stays
            // recoverable instead of a false never-sent claim.
            let termination = if status == InferenceStatus::Cancelled {
                TerminationEvidence::never_dispatched()
            } else {
                TerminationEvidence::unknown()
            };
            InferenceResult {
                request_id: request.request_id,
                status,
                response: None,
                effective: EffectiveInference::for_request(request),
                termination,
                purpose,
                stream: None,
                quota_headers: Vec::new(),
            }
        }
    }
}

/// Run one complete request.
async fn execute_complete(
    request: &InferenceRequest,
    purpose: RequestPurpose,
    runtime: Arc<dyn ProviderRuntime>,
    domain: &Arc<ResourceDomain>,
    kv: &Arc<KvAllocator>,
) -> InferenceResult {
    match runtime.complete(request).await {
        Ok(response) => {
            let terminal = terminal_stop_reason(&runtime, &response.stop_reason);
            let termination = termination_evidence(&response, terminal,
                request.payload.max_tokens.unwrap_or_else(|| request.provider_binding.default_max_tokens()));
            observe_kv(domain, kv, request, purpose, response.usage.as_ref());
            let quota_headers = response.ratelimit_headers.clone();
            InferenceResult {
                request_id: request.request_id,
                status: if terminal { InferenceStatus::Success } else { InferenceStatus::Failed },
                response: Some(response),
                effective: EffectiveInference::for_request(request),
                termination,
                purpose,
                stream: None,
                quota_headers,
            }
        }
        Err(error) => failure_result(request, purpose, &error, None),
    }
}

/// Run one streamed request.
///
/// The stream starts at the provider. The durable journal receives the
/// ordered batches and the terminal unit; the public surface receives the
/// gated text. Both happen independently of the caller: a subject that
/// disappears does not remove the evidence.
async fn execute_stream(
    request: &InferenceRequest,
    purpose: RequestPurpose,
    runtime: Arc<dyn ProviderRuntime>,
    stream: StreamSubmission,
    domain: &Arc<ResourceDomain>,
    kv: &Arc<KvAllocator>,
) -> InferenceResult {
    let publisher = BoundedStreamPublisher::new(
        request.request_id,
        stream.identity,
        stream.journal,
        stream.public,
    );
    let journaled = publisher.has_journal();
    if let Err(error) = publisher.open(&request.provider_binding, now_ms()) {
        return failure_result(
            request,
            purpose,
            &anyhow::Error::new(LocalDispatchError::JournalOpenFailed(error)),
            None,
        );
    }
    let outcome = runtime.stream(request, &publisher).await;
    let (response, termination, status, events, batches) = match outcome {
        Ok(outcome) => {
            let terminal = terminal_stop_reason(&runtime, &outcome.response.stop_reason);
            let termination = termination_evidence(&outcome.response, terminal,
                request.payload.max_tokens.unwrap_or_else(|| request.provider_binding.default_max_tokens()));
            let status = if terminal { InferenceStatus::Success } else { InferenceStatus::Failed };
            (
                Some(outcome.response),
                termination,
                status,
                outcome.events,
                outcome.batches,
            )
        }
        Err(error) => {
            warn!(request_id=%request.request_id, %error, "provider stream did not complete");
            (None, TerminationEvidence::unknown(), InferenceStatus::Failed, 0, 0)
        }
    };
    // The terminal receipt is written here, independently of the caller.
    let _ = publisher.finish(&termination, now_ms());
    let public_chunks = publisher.published_chunks();
    let quota_headers = response
        .as_ref()
        .map(|response| response.ratelimit_headers.clone())
        .unwrap_or_default();
    if let Some(response) = &response {
        observe_kv(domain, kv, request, purpose, response.usage.as_ref());
    }
    InferenceResult {
        request_id: request.request_id,
        status,
        response,
        effective: EffectiveInference::for_request(request),
        termination,
        purpose,
        stream: Some(StreamEvidence { events, batches, public_chunks, journaled }),
        quota_headers,
    }
}

/// Whether the provider's stop reason is a real terminal outcome.
pub(crate) fn terminal_stop_reason(
    runtime: &Arc<dyn ProviderRuntime>,
    stop_reason: &str,
) -> bool {
    match runtime.profile().backend_family {
        crate::profile::BackendFamily::Anthropic => matches!(
            stop_reason,
            "end_turn" | "max_tokens" | "stop_sequence" | "tool_use" | "pause_turn" | "refusal"
        ),
        _ => matches!(
            stop_reason,
            "stop" | "length" | "tool_calls" | "function_call" | "content_filter" | "end_turn"
        ),
    }
}

/// The terminal evidence of one provider response. `max_tokens` is the
/// effective limit of the sealed request: the payload's own reservation, or
/// the bound provider default when the payload leaves it open. Reporting the
/// binding default while the wire carried a smaller reservation would
/// contradict the request this evidence belongs to.
pub(crate) fn termination_evidence(
    response: &InferenceResponse,
    terminal: bool,
    max_tokens: u64,
) -> TerminationEvidence {
    let output_tokens = response.usage.as_ref().and_then(|usage| usage.output_tokens);
    if terminal {
        TerminationEvidence {
            kind: TerminationKind::ProviderTerminal,
            stop_reason: Some(response.stop_reason.clone()),
            max_tokens: Some(max_tokens),
            output_tokens,
            length: matches!(response.stop_reason.as_str(), "max_tokens" | "length"),
        }
    } else {
        TerminationEvidence {
            kind: TerminationKind::Unknown,
            stop_reason: (!response.stop_reason.is_empty()).then(|| response.stop_reason.clone()),
            max_tokens: Some(max_tokens),
            output_tokens,
            length: false,
        }
    }
}

/// A failure that provably happened before the provider was called.
///
/// The request never reached the wire, so it is not unknown remote debt:
/// `failure_result` records it as `never_dispatched` and releases the slot it
/// held instead of holding it as an unknown outcome. This is a local fact the
/// dispatch boundary knows, not a second failure taxonomy — `failure_status`
/// and `failure_result` still produce every status and termination kind.
#[derive(Debug)]
pub(crate) enum LocalDispatchError {
    /// No installed provider runtime can serve the sealed binding.
    NoProviderRuntime,
    /// The durable stream journal could not be opened before dispatch.
    JournalOpenFailed(StreamSinkError),
}

impl std::fmt::Display for LocalDispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoProviderRuntime => {
                f.write_str("no installed provider runtime can serve this binding")
            }
            Self::JournalOpenFailed(error) => {
                write!(f, "stream journal could not be opened: {error}")
            }
        }
    }
}

impl std::error::Error for LocalDispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoProviderRuntime => None,
            Self::JournalOpenFailed(error) => Some(error),
        }
    }
}

pub(crate) fn failure_status(error: &anyhow::Error) -> InferenceStatus {
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
    if let Some(local) = error.downcast_ref::<LocalDispatchError>() {
        return match local {
            // No installed runtime can serve the binding, so the provider is
            // unavailable exactly as the `runtime.available()` branch reports.
            LocalDispatchError::NoProviderRuntime => InferenceStatus::Unavailable,
            // The provider was reachable; the local durable journal failed.
            LocalDispatchError::JournalOpenFailed(_) => InferenceStatus::Failed,
        };
    }
    InferenceStatus::Failed
}

pub(crate) fn failure_result(
    req: &InferenceRequest,
    purpose: RequestPurpose,
    error: &anyhow::Error,
    stream: Option<StreamEvidence>,
) -> InferenceResult {
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
        // A local failure that provably happened before the provider call is
        // never dispatched; its capacity releases instead of staying debt.
        _ if error.downcast_ref::<LocalDispatchError>().is_some() => {
            TerminationEvidence::never_dispatched()
        }
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
        purpose,
        stream,
        quota_headers: error
            .downcast_ref::<ProviderRejection>()
            .map(|rejection| rejection.ratelimit_headers.clone())
            .unwrap_or_default(),
    }
}

/// Record the observed context size of one completed call.
///
/// The size comes from what the provider reported, not from an estimate. A
/// provider that did not report its input tokens leaves the pool unchanged.
///
/// The reusable prefix is what this call actually reported as cached, so a
/// backend that reports nothing keeps zero reuse instead of a guess. One hit is
/// an observation about this request; it is not proof that the backend still
/// retains KV after the context was released, and it never raises the lease to
/// a hard reservation.
fn observe_kv(
    domain: &Arc<ResourceDomain>,
    kv: &Arc<KvAllocator>,
    request: &InferenceRequest,
    purpose: RequestPurpose,
    usage: Option<&crate::request::TokenUsage>,
) {
    let Some(usage) = usage else { return };
    let Some(input_tokens) = usage.input_tokens else { return };
    let side = match purpose {
        RequestPurpose::Subject | RequestPurpose::External => LeaseSide::Main,
        _ => LeaseSide::Child,
    };
    // The handle belongs to the backend identity discovery established (engine
    // identity/incarnation, tokenizer, template, toolset) and to this exact
    // model. An engine rebuild, a re-rendered template or a model switch
    // invalidates every older handle instead of letting the new backend inherit
    // their residency. What was never observed stays unknown, so it neither
    // proves a match nor invents a change.
    let reusable_units = usage
        .cache_read_tokens
        .map(|cached| cached.min(input_tokens))
        .unwrap_or(0);
    let domain_identity = domain.identity();
    let mut pool = domain.kv_mut();
    let mut identity = pool.identity.clone();
    identity.adopt(&ResidencyIdentity {
        engine_identity: Some(domain_identity.engine_identity),
        engine_incarnation: domain_identity.engine_incarnation,
        model: Some(request.provider_binding.model().to_string()),
        tokenizer_fingerprint: None,
        template_fingerprint: None,
        toolset_digest: None,
    });
    pool.invalidate_stale(&identity);
    let demand = LeaseDemand {
        context_id: residency_context(request),
        side,
        window_units: input_tokens,
        reusable_units,
        recompute_units: input_tokens.saturating_sub(reusable_units),
        return_probability: 0.5,
        priority: 0,
        identity,
        affinity_options: request
            .payload
            .native_options
            .keys()
            .filter(|option| crate::capability::is_residency_option(option.as_str()))
            .cloned()
            .collect(),
    };
    if let Err(error) = kv.acquire(&mut pool, demand) {
        tracing::debug!(%error, "context residency pool kept its current leases");
    }
}

/// The current wall clock in milliseconds.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;

    use crate::broker::{Broker, InferenceRecoverySource};
    use crate::capability::BackendCapabilitySnapshot;
    use crate::profile::{
        AuthBinding, BackendFamily, ProviderProfile, SamplingCapabilities, TransportKind,
        UsageFieldMapping,
    };
    use crate::registry::TokenCount;
    use crate::request::{
        InferenceMessage, InferencePayload, InferencePriority, ProviderBinding, ProviderRejection,
        TokenUsage, WireErrorCode,
    };
    use crate::resource::ResourceDomainIdentity;
    use crate::stream::{StreamBatch, StreamIdentity, StreamJournalSink};
    use shenko_types::ids::{ControllerEpoch, EntityId};
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    fn fixture_binding() -> ProviderBinding {
        ProviderBinding::Local {
            base_url: "http://127.0.0.1".into(),
            model: "counting-fixture".into(),
            allow_invalid_certs: false,
            serializer_revision: crate::local_provider::SERIALIZER_REVISION,
            default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
        }
    }

    /// A runtime that counts how many requests actually reached it and can be
    /// told to drop the connection after specific ones leave.
    struct CountingRuntime {
        binding: ProviderBinding,
        complete_calls: AtomicU64,
        stream_calls: AtomicU64,
        reached: Mutex<Vec<EntityId>>,
        dropped: Mutex<HashSet<EntityId>>,
    }

    impl CountingRuntime {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                binding: fixture_binding(),
                complete_calls: AtomicU64::new(0),
                stream_calls: AtomicU64::new(0),
                reached: Mutex::new(Vec::new()),
                dropped: Mutex::new(HashSet::new()),
            })
        }

        /// After this request leaves, its connection drops before an outcome
        /// is known.
        fn drop_after_dispatch(&self, request_id: EntityId) {
            self.dropped.lock().expect("dropped set poisoned").insert(request_id);
        }

        fn reached_ids(&self) -> Vec<EntityId> {
            self.reached.lock().expect("reached set poisoned").clone()
        }
    }

    #[async_trait::async_trait]
    impl ProviderRuntime for CountingRuntime {
        fn profile(&self) -> &ProviderProfile {
            static PROFILE: std::sync::LazyLock<ProviderProfile> =
                std::sync::LazyLock::new(|| ProviderProfile {
                    profile_id: "local.openai.chat_completions".into(),
                    revision: crate::local_provider::SERIALIZER_REVISION,
                    backend_family: BackendFamily::OpenAiCompatible,
                    transport: TransportKind::HttpJson,
                    endpoint_origin: "http://127.0.0.1".into(),
                    model_id: "counting-fixture".into(),
                    api_version: None,
                    sdk: None,
                    request_schema_revision: crate::local_provider::SERIALIZER_REVISION,
                    response_schema_revision: crate::local_provider::SERIALIZER_REVISION,
                    gateway_profile: None,
                    beta_features: Vec::new(),
                    sampling: SamplingCapabilities::default(),
                    auth: AuthBinding::none(),
                    usage: UsageFieldMapping::default(),
                    native_namespaces: Vec::new(),
                    resource_domain_id: "default.unclassified".into(),
                    engine_incarnation: None,
                    default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
                    serializer_revision: crate::local_provider::SERIALIZER_REVISION,
                });
            &PROFILE
        }

        fn binding(&self) -> ProviderBinding {
            self.binding.clone()
        }

        async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
            self.complete_calls.fetch_add(1, Ordering::SeqCst);
            self.reached.lock().expect("reached set poisoned").push(request.request_id);
            if self
                .dropped
                .lock()
                .expect("dropped set poisoned")
                .contains(&request.request_id)
            {
                return Err(anyhow::anyhow!(
                    "the connection dropped after the request left"
                ));
            }
            Ok(InferenceResponse {
                content: "counting fixture".into(),
                stop_reason: "stop".into(),
                raw_response: r#"{"fixture":true,"stop_reason":"stop"}"#.into(),
                usage: Some(TokenUsage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                }),
                ratelimit_headers: Vec::new(),
                citations: Vec::new(),
            })
        }

        async fn stream(
            &self,
            request: &InferenceRequest,
            _publisher: &BoundedStreamPublisher,
        ) -> Result<crate::stream::StreamOutcome> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            self.reached.lock().expect("reached set poisoned").push(request.request_id);
            if self
                .dropped
                .lock()
                .expect("dropped set poisoned")
                .contains(&request.request_id)
            {
                return Err(anyhow::anyhow!(
                    "the connection dropped after the request left"
                ));
            }
            Ok(crate::stream::StreamOutcome {
                response: InferenceResponse {
                    content: "counting fixture".into(),
                    stop_reason: "stop".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"stop"}"#.into(),
                    usage: None,
                    ratelimit_headers: Vec::new(),
                    citations: Vec::new(),
                },
                events: 1,
                batches: 1,
            })
        }

        async fn count_tokens(&self, _request: &InferenceRequest) -> Result<TokenCount> {
            Err(anyhow::anyhow!("the fixture does not count tokens"))
        }

        async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
            Err(anyhow::anyhow!("the fixture does not probe"))
        }
    }

    /// A receipt owner that records every delivered result and can publish a
    /// recovered terminal result independently, like the durable ledger does.
    struct TestReceipt {
        request_id: EntityId,
        binding: ProviderBinding,
        payload: InferencePayload,
        recorded: Mutex<Vec<InferenceResult>>,
        terminal: Mutex<Option<InferenceResult>>,
    }

    impl TestReceipt {
        fn new(request: &InferenceRequest) -> Arc<Self> {
            Arc::new(Self {
                request_id: request.request_id,
                binding: request.provider_binding.clone(),
                payload: request.payload.clone(),
                recorded: Mutex::new(Vec::new()),
                terminal: Mutex::new(None),
            })
        }

        fn publish_terminal(&self, result: InferenceResult) {
            *self.terminal.lock().expect("terminal poisoned") = Some(result);
        }

        fn recorded(&self) -> Vec<InferenceResult> {
            self.recorded.lock().expect("recorded poisoned").clone()
        }
    }

    impl InferenceRecoverySource for TestReceipt {
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
            Ok(self.terminal.lock().expect("terminal poisoned").clone())
        }
    }

    impl InferenceReceiptSink for TestReceipt {
        fn may_dispatch(&self) -> anyhow::Result<DispatchFence> {
            Ok(DispatchFence::Send)
        }

        fn record(&self, result: &InferenceResult) -> anyhow::Result<()> {
            self.recorded.lock().expect("recorded poisoned").push(result.clone());
            Ok(())
        }
    }

    /// A durable stream journal whose open always fails before dispatch.
    struct FailingJournal;

    impl StreamJournalSink for FailingJournal {
        fn open_stream(&self, _request_id: EntityId, _binding: &ProviderBinding) -> Result<(), StreamSinkError> {
            Err(StreamSinkError::Journal("fixture journal is unavailable".into()))
        }

        fn append_batch(&self, _request_id: EntityId, _batch: &StreamBatch) -> Result<(), StreamSinkError> {
            Ok(())
        }

        fn append_terminal(&self, _request_id: EntityId, _terminal: &TerminationEvidence) -> Result<(), StreamSinkError> {
            Ok(())
        }

        fn close_stream(&self, _request_id: EntityId) -> Result<(), StreamSinkError> {
            Ok(())
        }
    }

    fn domain(capacity: u64) -> Arc<ResourceDomain> {
        ResourceDomain::new(
            ResourceDomainIdentity {
                domain_id: "dispatch-classification.test".into(),
                engine_identity: "counting-fixture".into(),
                engine_incarnation: None,
                epoch: 1,
            },
            capacity,
            crate::admission::CapacityProvenance::Configured,
            None,
            crate::admission::CapacityProvenance::Unknown,
        )
    }

    fn new_request(
        request_id: EntityId,
        binding: &ProviderBinding,
    ) -> (InferenceRequest, oneshot::Receiver<InferenceResult>) {
        let (result_tx, result_rx) = oneshot::channel();
        (
            InferenceRequest {
                request_id,
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                provider_binding: binding.clone(),
                priority: InferencePriority::Subject,
                epoch: ControllerEpoch(1),
                payload: InferencePayload {
                    system_prompt: "fixture system".into(),
                    messages: vec![InferenceMessage::text("user", "fixture input")],
                    max_tokens: Some(16),
                    tools: vec![],
                    native_options: BTreeMap::new(),
                },
                result_tx,
            },
            result_rx,
        )
    }

    /// The dispatch-boundary classification truth table. A local failure that
    /// provably happened before the provider was called is never dispatched;
    /// the pre-existing InvalidRequest mapping and the post-dispatch unknown
    /// outcome do not regress.
    #[test]
    fn failure_classification_keeps_never_dispatched_local_and_unknown_remote() {
        let (request, _rx) = new_request(EntityId::new(), &fixture_binding());

        // A local validation failure before dispatch stays never-dispatched.
        let wire = anyhow::Error::new(ProviderWireError::ReservedOption("temperature".into()));
        let result = failure_result(&request, RequestPurpose::Subject, &wire, None);
        assert_eq!(
            result.status,
            InferenceStatus::InvalidRequest {
                code: WireErrorCode::ReservedNativeOption
            }
        );
        assert_eq!(result.termination.kind, TerminationKind::NeverDispatched);

        // A real HTTP rejection stays transport-rejected.
        let rejection = anyhow::Error::new(ProviderRejection {
            http_status: 429,
            ratelimit_headers: vec![("retry-after".into(), "1".into())],
        });
        let result = failure_result(&request, RequestPurpose::Subject, &rejection, None);
        assert_eq!(result.status, InferenceStatus::Rejected { http_status: 429 });
        assert_eq!(result.termination.kind, TerminationKind::TransportRejected);
        assert_eq!(result.quota_headers, vec![("retry-after".to_string(), "1".to_string())]);

        // A post-dispatch transport failure stays unknown.
        let dropped = anyhow::anyhow!("connection closed mid-response");
        let result = failure_result(&request, RequestPurpose::Subject, &dropped, None);
        assert_eq!(result.status, InferenceStatus::Failed);
        assert_eq!(result.termination.kind, TerminationKind::Unknown);

        // The local dispatch-boundary failures map to never-dispatched.
        let missing = anyhow::Error::new(LocalDispatchError::NoProviderRuntime);
        let result = failure_result(&request, RequestPurpose::Subject, &missing, None);
        assert_eq!(result.status, InferenceStatus::Unavailable);
        assert_eq!(result.termination.kind, TerminationKind::NeverDispatched);
        let journal = anyhow::Error::new(LocalDispatchError::JournalOpenFailed(
            StreamSinkError::Journal("journal gone".into()),
        ));
        let result = failure_result(&request, RequestPurpose::Subject, &journal, None);
        assert_eq!(result.status, InferenceStatus::Failed);
        assert_eq!(result.termination.kind, TerminationKind::NeverDispatched);
    }

    /// Issue #274: a runtime that is gone before dispatch consumes the slot.
    /// With C=1 the request never reaches a provider, its result is the typed
    /// unavailable outcome, the receipt records NeverDispatched, and the freed
    /// slot admits the next valid request.
    #[tokio::test(flavor = "current_thread")]
    async fn a_missing_provider_runtime_is_never_dispatched_and_releases_its_slot() {
        let registry = Arc::new(ProviderRegistry::new());
        let runtime = CountingRuntime::new();
        let epoch = registry.install(runtime.clone());
        let (broker, handle) = Broker::with_registry(Arc::clone(&registry), domain(1));
        let binding = handle.binding().clone();

        let (request_one, result_one_rx) = new_request(EntityId::new(), &binding);
        let request_one_id = request_one.request_id;
        let receipt_one = TestReceipt::new(&request_one);
        handle.submit_recorded(request_one, receipt_one.clone()).unwrap();

        // The request was submitted while the runtime existed; the runtime is
        // uninstalled before the broker drains the queue, so dispatch cannot
        // reach a provider.
        let profile_id = runtime.profile().profile_id.clone();
        assert!(registry.uninstall(&profile_id, epoch));
        let run = tokio::spawn(broker.run());

        let result_one = tokio::time::timeout(Duration::from_secs(2), result_one_rx)
            .await
            .expect("the admitted request answers")
            .expect("the request delivered its result");
        assert_eq!(result_one.request_id, request_one_id);
        assert_eq!(result_one.status, InferenceStatus::Unavailable);
        assert_eq!(result_one.termination.kind, TerminationKind::NeverDispatched);
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.stream_calls.load(Ordering::SeqCst), 0);
        assert!(runtime.reached_ids().is_empty());
        assert_eq!(handle.active_count(), 0);
        let recorded = receipt_one.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].termination.kind, TerminationKind::NeverDispatched);
        assert_eq!(recorded[0].status, InferenceStatus::Unavailable);

        // The released slot admits the next valid request on a reinstalled
        // runtime; the C=1 budget is not permanently held.
        registry.install(runtime.clone());
        let (request_two, result_two_rx) = new_request(EntityId::new(), &binding);
        handle.submit(request_two).unwrap();
        let result_two = tokio::time::timeout(Duration::from_secs(2), result_two_rx)
            .await
            .expect("the second request answers")
            .expect("the second request delivered its result");
        assert_eq!(result_two.status, InferenceStatus::Success);
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.reached_ids().len(), 1);
        assert_eq!(handle.active_count(), 0);

        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("the broker drains")
            .expect("the broker exits");
    }

    /// Issue #274: the durable stream journal fails before the provider is
    /// called, so the streamed request is never dispatched and its slot frees.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stream_journal_open_failure_is_never_dispatched_and_releases_its_slot() {
        let registry = Arc::new(ProviderRegistry::new());
        let runtime = CountingRuntime::new();
        registry.install(runtime.clone());
        let (broker, handle) = Broker::with_registry(registry, domain(1));
        let binding = handle.binding().clone();

        let (request, result_rx) = new_request(EntityId::new(), &binding);
        let request_id = request.request_id;
        let receipt = TestReceipt::new(&request);
        handle
            .submit_stream_recorded(
                request,
                StreamIdentity {
                    message_id: EntityId::new().to_string(),
                    part_id: "text/0".into(),
                    stream_id: request_id,
                    attempt_id: EntityId::new(),
                },
                Some(Arc::new(FailingJournal)),
                None,
                receipt.clone(),
            )
            .expect("the streamed submission is admitted");
        let run = tokio::spawn(broker.run());

        let result = tokio::time::timeout(Duration::from_secs(2), result_rx)
            .await
            .expect("the streamed request answers")
            .expect("the request delivered its result");
        assert_eq!(result.status, InferenceStatus::Failed);
        assert_eq!(result.termination.kind, TerminationKind::NeverDispatched);
        assert_eq!(runtime.stream_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 0);
        assert!(runtime.reached_ids().is_empty());
        assert_eq!(handle.active_count(), 0);
        let recorded = receipt.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].termination.kind, TerminationKind::NeverDispatched);
        assert_eq!(recorded[0].status, InferenceStatus::Failed);

        // The released slot admits the next valid request.
        let (request_two, result_two_rx) = new_request(EntityId::new(), &binding);
        handle.submit(request_two).unwrap();
        let result_two = tokio::time::timeout(Duration::from_secs(2), result_two_rx)
            .await
            .expect("the second request answers")
            .expect("the second request delivered its result");
        assert_eq!(result_two.status, InferenceStatus::Success);
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 1);

        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("the broker drains")
            .expect("the broker exits");
    }

    /// The control case that must not regress: a request that actually left
    /// and then dropped its connection stays unknown, keeps its C=1 slot, and
    /// only original terminal evidence releases it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_connection_after_dispatch_stays_unknown_until_reconciled() {
        let registry = Arc::new(ProviderRegistry::new());
        let runtime = CountingRuntime::new();
        registry.install(runtime.clone());
        let (broker, handle) = Broker::with_registry(registry, domain(1));
        let binding = handle.binding().clone();

        let (request, result_rx) = new_request(EntityId::new(), &binding);
        let request_id = request.request_id;
        let effective = EffectiveInference::for_request(&request);
        let receipt = TestReceipt::new(&request);
        runtime.drop_after_dispatch(request_id);
        handle.submit_recorded(request, receipt.clone()).unwrap();
        let run = tokio::spawn(broker.run());

        let result = tokio::time::timeout(Duration::from_secs(2), result_rx)
            .await
            .expect("the dropped request answers")
            .expect("the request delivered its result");
        assert_eq!(result.status, InferenceStatus::Failed);
        assert_eq!(result.termination.kind, TerminationKind::Unknown);
        // The provider was called; its outcome is unknown and the slot holds.
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.reached_ids(), vec![request_id]);
        assert_eq!(handle.active_count(), 1);
        let recorded = receipt.recorded();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].termination.kind, TerminationKind::Unknown);

        // A second request queues behind the occupied slot instead of entering.
        let (request_two, mut result_two_rx) = new_request(EntityId::new(), &binding);
        handle.submit(request_two).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(150), &mut result_two_rx)
                .await
                .is_err(),
            "the queued request must not be admitted while the slot is held"
        );
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(handle.active_count(), 1);

        // Original terminal evidence ends the debt and frees the slot.
        receipt.publish_terminal(InferenceResult {
            request_id,
            status: InferenceStatus::Success,
            response: None,
            effective,
            termination: TerminationEvidence {
                kind: TerminationKind::ProviderTerminal,
                stop_reason: Some("stop".into()),
                max_tokens: Some(16),
                output_tokens: Some(1),
                length: false,
            },
            purpose: RequestPurpose::Subject,
            stream: None,
            quota_headers: Vec::new(),
        });
        handle.reconcile_recorded(request_id).unwrap();

        let result_two = tokio::time::timeout(Duration::from_secs(2), result_two_rx)
            .await
            .expect("the queued request answers")
            .expect("the queued request delivered its result");
        assert_eq!(result_two.status, InferenceStatus::Success);
        assert_eq!(runtime.complete_calls.load(Ordering::SeqCst), 2);
        assert_eq!(handle.active_count(), 0);

        drop(handle);
        tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("the broker drains")
            .expect("the broker exits");
    }
}
