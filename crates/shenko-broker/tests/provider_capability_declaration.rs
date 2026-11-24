//! Provider capability declarations — what one binding actually declared, how
//! routing reads it, and what a declared refusal does to a streamed dispatch.
//!
//! UNRUN in this packet: no cargo build, test, model call or network request
//! was made while writing it. Every fixture below is an in-process declaration
//! or a retained probe shape; none of them is evidence that a real deployment
//! returns SSE, a cache or a quota surface.

use std::sync::Arc;
use std::time::Duration;

use shenko_broker::{
    AuthBinding, BackendCapabilitySnapshot, BackendFamily, BoundedStreamPublisher, Broker,
    BrokeredCall, CacheSurface, CapabilityRequirement, CapabilityVerdict,
    CapacityProvenance, DeclaredCapability, DeclarationSource, InferenceMessage,
    InferencePayload, InferencePriority, InferenceRequest, InferenceResponse, InferenceStatus,
    OperationAvailability, ProviderBinding, ProviderCapabilityDeclaration, ProviderProfile,
    ProviderRuntime, QuotaSurface, RequestPurpose, ResourceDomain, ResourceDomainIdentity,
    StreamBatch, StreamIdentity, StreamJournalSink, StreamOutcome, StreamSinkError,
    TerminationEvidence, TerminationKind, TokenCount, TransportKind, classify_operations,
};
use shenko_types::ids::{ControllerEpoch, EntityId};

/// One probe snapshot built from evidence only: the same classifiers the real
/// providers use, with no fabricated operation entry.
fn snapshot(family: BackendFamily, evidence: Vec<serde_json::Value>) -> BackendCapabilitySnapshot {
    let operations = classify_operations(&family, &evidence);
    BackendCapabilitySnapshot {
        capability_id: "fixture/declaration".into(),
        provider_implementation_digest: "fixture".into(),
        backend_family: family,
        backend_build: None,
        transport: TransportKind::HttpJson,
        configuration_digest: None,
        engine_incarnation: None,
        model_fingerprint: None,
        tokenizer_fingerprint: None,
        template_fingerprint: None,
        toolset_digest: None,
        operations,
        capacities: Vec::new(),
        raw_evidence: evidence,
    }
}

fn fixture_profile() -> ProviderProfile {
    ProviderProfile {
        profile_id: "fixture.refusing".into(),
        revision: 1,
        backend_family: BackendFamily::OpenAiCompatible,
        transport: TransportKind::HttpJson,
        endpoint_origin: "http://127.0.0.1:9".into(),
        model_id: "fixture".into(),
        api_version: None,
        sdk: None,
        request_schema_revision: 1,
        response_schema_revision: 1,
        gateway_profile: None,
        beta_features: Vec::new(),
        sampling: Default::default(),
        auth: AuthBinding::none(),
        usage: Default::default(),
        native_namespaces: Vec::new(),
        resource_domain_id: "fixture.refusing".into(),
        engine_incarnation: None,
        default_max_tokens: 8192,
        serializer_revision: 1,
        process_generation: None,
    }
}

fn fixture_request(
    binding: ProviderBinding,
    result_tx: tokio::sync::oneshot::Sender<shenko_broker::InferenceResult>,
) -> InferenceRequest {
    InferenceRequest {
        request_id: EntityId::new(),
        agent_id: EntityId::new(),
        conversation_id: EntityId::new(),
        provider_binding: binding,
        priority: InferencePriority::Normal,
        epoch: ControllerEpoch(1),
        payload: InferencePayload {
            system_prompt: "fixture system".into(),
            messages: vec![InferenceMessage::text("user", "fixture input")],
            max_tokens: Some(8),
            tools: Vec::new(),
            native_options: Default::default(),
        },
        result_tx,
    }
}

struct NullJournal;

impl StreamJournalSink for NullJournal {
    fn open_stream(
        &self,
        _request_id: EntityId,
        _binding: &ProviderBinding,
    ) -> Result<(), StreamSinkError> {
        Ok(())
    }

    fn append_batch(
        &self,
        _request_id: EntityId,
        _batch: &StreamBatch,
    ) -> Result<(), StreamSinkError> {
        Ok(())
    }

    fn append_terminal(
        &self,
        _request_id: EntityId,
        _terminal: &TerminationEvidence,
    ) -> Result<(), StreamSinkError> {
        Ok(())
    }

    fn close_stream(&self, _request_id: EntityId) -> Result<(), StreamSinkError> {
        Ok(())
    }
}

/// A provider that declares it will not attempt anything. Its `stream` must
/// never be called: the declared refusal settles the request instead.
struct RefusingStreamRuntime {
    profile: ProviderProfile,
}

#[async_trait::async_trait]
impl ProviderRuntime for RefusingStreamRuntime {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn binding(&self) -> ProviderBinding {
        ProviderBinding::Profile {
            profile: self.profile.clone(),
            prepared: None,
        }
    }

    fn declaration(&self) -> ProviderCapabilityDeclaration {
        ProviderCapabilityDeclaration::refused()
    }

    async fn complete(&self, _request: &InferenceRequest) -> anyhow::Result<InferenceResponse> {
        Err(anyhow::anyhow!("the refusing fixture never completes"))
    }

    async fn stream(
        &self,
        _request: &InferenceRequest,
        _publisher: &BoundedStreamPublisher,
    ) -> anyhow::Result<StreamOutcome> {
        Err(anyhow::anyhow!(
            "the refusing fixture must not be asked to stream"
        ))
    }

    async fn count_tokens(&self, _request: &InferenceRequest) -> anyhow::Result<TokenCount> {
        Err(anyhow::anyhow!("the refusing fixture does not count tokens"))
    }

    async fn probe(&self) -> anyhow::Result<BackendCapabilitySnapshot> {
        Ok(snapshot(BackendFamily::OpenAiCompatible, Vec::new()))
    }
}

#[test]
fn an_unprobed_binding_declares_nothing_and_routing_stays_unknown() {
    let declaration = ProviderCapabilityDeclaration::UNDECLARED;
    assert!(declaration.is_undeclared());
    for requirement in [
        CapabilityRequirement::Streaming,
        CapabilityRequirement::Cache(CacheSurface::PrefixCache),
        CapabilityRequirement::Quota(QuotaSurface::ApiHeaders),
    ] {
        let verdict = declaration.verdict(requirement);
        assert!(
            matches!(verdict, CapabilityVerdict::Undeclared { .. }),
            "{} must stay undeclared: {verdict:?}",
            requirement.as_str()
        );
        assert!(!verdict.refuses_dispatch());
    }
}

#[test]
fn a_declared_refusal_is_a_refusal_and_is_not_softened_into_unknown() {
    let declaration = ProviderCapabilityDeclaration {
        streaming: DeclaredCapability::refused(),
        ..ProviderCapabilityDeclaration::UNDECLARED
    };
    let verdict = declaration.verdict(CapabilityRequirement::Streaming);
    assert!(verdict.refuses_dispatch(), "{verdict:?}");
    assert_eq!(
        verdict.reason(),
        Some("streaming is declared unsupported by this provider binding")
    );
    // The refusal does not spill onto facts the binding never declared.
    let cache = declaration.verdict(CapabilityRequirement::Cache(CacheSurface::CacheSalt));
    assert!(matches!(cache, CapabilityVerdict::Undeclared { .. }));
}

#[test]
fn an_engine_declared_prefix_cache_is_available_and_every_other_surface_stays_unknown() {
    let evidence = vec![serde_json::json!({
        "probe_body": {
            "retained_lines": [
                "vllm:cache_config_info{block_size=\"16\",enable_prefix_caching=\"True\"} 1.0"
            ]
        }
    })];
    let snapshot = snapshot(BackendFamily::Vllm, evidence);
    assert!(snapshot.supports(shenko_broker::NATIVE_VLLM_PREFIX_CACHING));

    let declaration = ProviderCapabilityDeclaration::from_snapshot(&snapshot);
    assert_eq!(
        declaration.cache.prefix_cache,
        DeclaredCapability {
            support: OperationAvailability::Available,
            source: DeclarationSource::Probe,
        }
    );
    // The backend family and the engine's cache configuration say nothing about
    // a request-level salt, the streaming wire or a quota surface.
    assert!(declaration.cache.cache_salt.is_undeclared());
    assert!(declaration.cache.kv_hints.is_undeclared());
    assert!(declaration.streaming.is_undeclared());
    assert!(declaration.quota.api_headers.is_undeclared());
    assert!(matches!(
        declaration.verdict(CapabilityRequirement::Cache(CacheSurface::PrefixCache)),
        CapabilityVerdict::Declared
    ));
}

#[test]
fn a_probe_that_contradicts_itself_is_unknown_instead_of_order_dependent() {
    let evidence = vec![
        serde_json::json!({ "vllm.cache_salt": true }),
        serde_json::json!({ "vllm.cache_salt": { "availability": "unsupported" } }),
    ];
    let snapshot = snapshot(BackendFamily::Vllm, evidence);
    let declaration = ProviderCapabilityDeclaration::from_snapshot(&snapshot);
    assert_eq!(
        declaration.cache.cache_salt.support,
        OperationAvailability::Unknown
    );
    // The probe did speak about the fact, so the unknown is a probe fact and
    // not silently an implementation claim.
    assert_eq!(declaration.cache.cache_salt.source, DeclarationSource::Probe);
}

#[test]
fn a_silent_probe_keeps_the_implementation_claim_and_an_open_disagreement_is_unknown() {
    let declared = ProviderCapabilityDeclaration {
        streaming: DeclaredCapability::implemented(),
        ..ProviderCapabilityDeclaration::UNDECLARED
    };

    let merged = ProviderCapabilityDeclaration::with_observation(
        &declared,
        &ProviderCapabilityDeclaration::UNDECLARED,
    );
    assert_eq!(merged.streaming, DeclaredCapability::implemented());
    assert!(merged.quota.api_headers.is_undeclared());

    let probe_refusal = ProviderCapabilityDeclaration {
        streaming: DeclaredCapability {
            support: OperationAvailability::Unsupported,
            source: DeclarationSource::Probe,
        },
        ..ProviderCapabilityDeclaration::UNDECLARED
    };
    let conflicted = ProviderCapabilityDeclaration::with_observation(&declared, &probe_refusal);
    assert_eq!(conflicted.streaming.support, OperationAvailability::Unknown);
    assert_eq!(conflicted.streaming.source, DeclarationSource::Probe);
    // A conflicted fact permits nothing and claims nothing.
    assert!(matches!(
        conflicted.verdict(CapabilityRequirement::Streaming),
        CapabilityVerdict::Undeclared { .. }
    ));
}

#[test]
fn a_bound_implementation_reports_only_the_surfaces_it_owns() {
    let provider = shenko_broker::LocalModelProvider::new(
        "http://127.0.0.1:9".into(),
        "fixture".into(),
    );
    let (broker, handle) = Broker::with_local_provider(provider, 1).expect("the fixture binds");
    let declaration = handle.capability_declaration();
    assert!(declaration.streaming.is_available());
    assert!(declaration.cache.cache_read_accounting.is_available());
    // The OpenAI-compatible surface owns no quota observation contract, so no
    // quota surface is borrowed from another provider.
    assert!(declaration.quota.api_headers.is_undeclared());
    assert!(matches!(
        handle.route_capability(CapabilityRequirement::Streaming),
        CapabilityVerdict::Declared
    ));
    assert!(matches!(
        handle.route_capability(CapabilityRequirement::Quota(QuotaSurface::ApiHeaders)),
        CapabilityVerdict::Undeclared { .. }
    ));

    // A probe that never named a surface leaves both the implementation claim
    // and the unknown quota surface exactly where they were.
    broker.adopt_capabilities(&snapshot(
        BackendFamily::OpenAiCompatible,
        vec![serde_json::json!({ "data": [] })],
    ));
    let after = handle.capability_declaration();
    assert_eq!(after.streaming, declaration.streaming);
    assert!(after.quota.api_headers.is_undeclared());
}

#[tokio::test]
async fn a_binding_that_declared_streaming_unsupported_never_dispatches() {
    let runtime = Arc::new(RefusingStreamRuntime {
        profile: fixture_profile(),
    });
    let domain = ResourceDomain::new(
        ResourceDomainIdentity {
            domain_id: "fixture.refusing".into(),
            engine_identity: "fixture".into(),
            engine_incarnation: None,
            epoch: 1,
        },
        1,
        CapacityProvenance::Configured,
        None,
        CapacityProvenance::Unknown,
    );
    let (broker, handle) =
        Broker::with_runtime(runtime, domain).expect("the fixture runtime binds");

    let verdict = handle.route_capability(CapabilityRequirement::Streaming);
    assert!(matches!(
        verdict,
        CapabilityVerdict::Refused {
            code: shenko_broker::CAPABILITY_UNSUPPORTED_CODE,
            ..
        }
    ));

    let run = tokio::spawn(broker.run());
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let request = fixture_request(handle.binding().clone(), result_tx);
    // The declared refusal is settled at the send boundary, not before
    // admission: the caller must not lose its request identity to a pre-enqueue
    // error it cannot distinguish from a possibly sent request.
    handle
        .submit_stream(
            BrokeredCall {
                purpose: RequestPurpose::Diagnostics,
                request,
            },
            StreamIdentity::default(),
            Some(Arc::new(NullJournal) as Arc<dyn StreamJournalSink>),
            None,
        )
        .expect("a declared refusal is a terminal result, not a submission error");

    let result = tokio::time::timeout(Duration::from_secs(5), result_rx)
        .await
        .expect("the broker settles the refused stream")
        .expect("the result channel stays open");
    assert_eq!(result.status, InferenceStatus::Unavailable);
    assert_eq!(result.termination.kind, TerminationKind::NeverDispatched);
    assert!(result.stream.is_none());
    run.abort();
}
