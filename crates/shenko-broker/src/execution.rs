//! The execution of one admitted request — SPEC §41.9.
//!
//! The dispatch fence is checked at the send boundary, the provider runtime
//! is resolved from the sealed binding, and the terminal result is produced
//! from what the provider actually reported. A fenced request provably never
//! left; an unknown outcome keeps its capacity and stays recoverable.

use std::sync::Arc;

use anyhow::anyhow;
use tracing::warn;

use crate::kv::{KvAllocator, LeaseDemand, LeaseSide};
use crate::registry::{ProviderRegistry, ProviderRuntime};
use crate::request::{
    EffectiveInference, InferenceRequest, InferenceResponse, InferenceResult, InferenceStatus,
    ProviderRejection, ProviderWireError, RequestPurpose, StreamEvidence,
    TerminationEvidence, TerminationKind,
};
use crate::resource::ResourceDomain;
use crate::stream::BoundedStreamPublisher;

use crate::broker::{DispatchFence, InferenceReceiptSink, StreamSubmission};

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
                    &anyhow!("no installed provider runtime can serve this binding"),
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
            &anyhow!("stream journal could not be opened: {error}"),
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
    let mut pool = domain.kv_mut();
    let demand = LeaseDemand {
        context_id: request.conversation_id.to_string(),
        side,
        window_units: input_tokens,
        reusable_units: 0,
        recompute_units: input_tokens,
        return_probability: 0.5,
        priority: 0,
    };
    if let Err(error) = kv.acquire(&mut pool, demand) {
        tracing::debug!(%error, "context residency pool kept its current leases");
    }
}

/// The current wall clock in milliseconds.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
