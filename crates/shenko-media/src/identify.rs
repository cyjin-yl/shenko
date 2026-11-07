//! media.identify — the subject-facing native child that asks the configured
//! speaker-identity engine about ONE segment's ORIGINAL sealed audio
//! (SPEC §15.2, §20, §35.3).
//!
//! The child is a thin, honest binding: the model input is frozen and
//! CAS-sealed before anything runs, the scope is the caller's own subject,
//! and the actual identification goes through the live
//! [`MediaRuntime::identify_speaker`] of the SAME runtime the UI routes and
//! provider bindings use — never a second registry, a parallel pipeline or a
//! placeholder handler.
//!
//! What the engine reads is the audio this ledger already sealed for the
//! segment. This child never re-captures, re-downloads or re-transcribes
//! anything, never synthesizes transcript text, and never turns a speaker
//! claim into identity authority. The claim is admitted by the ingress as a
//! NEW revision of the SAME segment: the retained provider text and the
//! original audio reference are carried forward, and the receipt names the
//! fact that committed.

use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    effect::{
        CheckpointContract, EffectContract, EffectContractBuilder, ExternalEffect, GuaranteeBasis,
        IdempotencyGuarantee, LifecycleContract, ResourceAccess,
    },
    error::{RetryStrategy, ShenkoError},
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    media::*,
    object::*,
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskRecord},
};

use crate::runtime::MediaRuntime;

const MEDIA_IDENTIFY_CONTRACT: &[u8] = b"shenko.media.identify/v1: ask the configured speaker-identity engine about one segment's ORIGINAL sealed audio and admit its claim as a new revision of the SAME utterance; no re-capture, no download, no ASR, no synthesized text, no identity authority";
const MAX_ID_CHARS: usize = 256;
const MAX_NORMALIZED_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaIdentifyInput {
    provider: String,
    account: String,
    stream_id: String,
    segment: u64,
}

/// The frozen intent of one identification. Everything the engine may read is
/// named here: the subject that owns the fact, the stream, and the one
/// segment. The engine binding is configuration, not model input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenMediaIdentifyInput {
    schema_version: u32,
    agent_id: EntityId,
    stream: MediaStreamDescriptor,
    segment: u64,
}

/// Native child `media.identify` v1. Published through the native task
/// registry by the binary, next to `media.read`, against the live media
/// runtime of the same process.
pub struct MediaIdentifyHandler {
    store: Arc<Store>,
    runtime: Arc<MediaRuntime>,
}

impl MediaIdentifyHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<MediaRuntime>) -> Self {
        Self { store, runtime }
    }

    fn plugin_id() -> EntityId {
        "5d7b3c19-6e42-4f8a-9b17-2c8e0a4d6f31"
            .parse()
            .expect("fixed media.identify plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-media-identify", schema, 1)
        {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    /// Freeze the model input. The subject scope is the caller's own subject,
    /// so a caller cannot ask this runtime to spend an engine call on another
    /// subject's audio.
    fn freeze(
        &self,
        value: serde_json::Value,
        agent_id: EntityId,
    ) -> Result<FrozenMediaIdentifyInput> {
        let MediaIdentifyInput { provider, account, stream_id, segment } =
            serde_json::from_value(value)?;
        ensure!(i64::try_from(segment).is_ok(), "segment exceeds the supported persistent range");
        Ok(FrozenMediaIdentifyInput {
            schema_version: 1,
            agent_id,
            stream: MediaStreamDescriptor {
                provider: bounded_id("provider", provider)?,
                account: bounded_id("account", account)?,
                stream_id: bounded_id("stream_id", stream_id)?,
            },
            segment,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenMediaIdentifyInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "media_identify_frozen_input_mismatch"
        );
        let frozen: FrozenMediaIdentifyInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_media_identify_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        agent_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.media.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "identify_own_subject_sealed_media".into(),
            extra: serde_json::json!({ "agent_id": agent_id }),
        };
        let resource = self.seal(&bindings, "media-identify-binding")?;
        let acceptance = self.seal(
            &"one engine claim about the segment's already-sealed audio, admitted as a new revision of that same segment, or an explicit typed failure",
            "media-identify-acceptance",
        )?;
        let retry = self.seal(
            &"reconcile under the original armed attempt; a repeat is another claim of the SAME segment and never a second utterance or a re-capture",
            "media-identify-retry",
        )?;
        let contract_digest = Digest::blake3(MEDIA_IDENTIFY_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:media-identify-v1".into(),
            },
            symbol: "media.identify.execute".into(),
            schema: SchemaRef {
                name: "media-identify-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "media.identify.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    /// Run the real identification through the live runtime and describe what
    /// the ledger now holds. The observation is derived from the committed
    /// revision, so the claim, the retained text and the audio reference are
    /// reported exactly as admitted — nothing is restated from the request.
    async fn identify(
        &self,
        frozen: &FrozenMediaIdentifyInput,
    ) -> (serde_json::Value, OperationOutcome, EffectStatus) {
        // A subject identifies only its own stream facts. The store refuses a
        // cross-subject admission anyway; refusing before the engine call
        // keeps another subject's audio out of this engine entirely.
        match self.store.media_stream(&frozen.stream) {
            Ok(Some(stream)) if stream.agent_id != frozen.agent_id => {
                return (
                    failure(
                        frozen,
                        "media_stream_agent_mismatch",
                        Some(RetryStrategy::NeverAutomatically),
                        "media stream belongs to a different subject",
                    ),
                    OperationOutcome::Failed,
                    EffectStatus::None,
                );
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    failure(
                        frozen,
                        "media_identify_storage",
                        Some(RetryStrategy::NeverAutomatically),
                        error.to_string(),
                    ),
                    OperationOutcome::Failed,
                    EffectStatus::None,
                );
            }
        }
        let delivery = match self
            .runtime
            .identify_speaker(&frozen.stream, frozen.segment)
            .await
        {
            Ok(delivery) => delivery,
            Err(error) => {
                let (code, retry) = classify(&error);
                return (
                    failure(frozen, &code, retry, error.to_string()),
                    OperationOutcome::Failed,
                    EffectStatus::None,
                );
            }
        };
        let receipt = &delivery.admission.receipt;
        // Read the committed revision back: the observation reports the fact
        // the ledger holds, not what the handler asked for.
        let admitted = match self.store.media_revisions(&frozen.stream, frozen.segment) {
            Ok(revisions) => revisions
                .into_iter()
                .find(|revision| revision.revision_id == receipt.revision_id),
            Err(error) => {
                return (
                    failure(
                        frozen,
                        "media_identify_storage",
                        Some(RetryStrategy::NeverAutomatically),
                        error.to_string(),
                    ),
                    OperationOutcome::Failed,
                    EffectStatus::None,
                );
            }
        };
        let Some(admitted) = admitted else {
            // The admission returned a receipt for a revision this ledger can
            // no longer read. Reporting a claim here would be fabricated.
            return (
                failure(
                    frozen,
                    "media_identify_revision_missing",
                    Some(RetryStrategy::NeverAutomatically),
                    format!(
                        "admission reported {} but the segment has no such revision",
                        receipt.revision_id
                    ),
                ),
                OperationOutcome::Failed,
                EffectStatus::None,
            );
        };
        let audio = admitted.audio.as_ref().and_then(|audio| match audio {
            MediaAudioRef::Sealed { object } => Some(object),
            MediaAudioRef::Provider(_) => None,
        });
        let result = serde_json::json!({
            "schema_version": 1,
            "op": "identify",
            "agent_id": frozen.agent_id,
            "stream": frozen.stream,
            "segment": frozen.segment,
            "status": "succeeded",
            "receipt": receipt,
            "delivery": delivery.delivery,
            "revision": {
                "revision_id": admitted.revision_id,
                "revision": admitted.revision,
                "disposition": admitted.disposition,
                "partial": admitted.partial,
                "supersedes_revision": admitted.supersedes_revision,
                "superseded_by_revision": admitted.superseded_by_revision,
                "captured_at_ms": admitted.captured_at_ms,
                "event_id": admitted.event_id,
                "admitted_commit": admitted.admitted_commit,
            },
            // The engine's claim, with its own confidence/correction/unknown
            // semantics. It stays a model signal, never an identity binding.
            "claim": admitted.speaker,
            // The retained provider text is what the segment still says.
            "text": admitted.text,
            // The original audio identity the engine read, unchanged.
            "original_audio": admitted.audio,
            "audio_digest": audio.map(|object| object.digest.to_string()),
            "audio_bytes": audio.map(|object| object.bytes),
            // Which perception read this audio, which sealed observation it
            // reasons from and the immutable content object of the claim
            // (§51.2 「记录谁的 perception」, §51.1). Reported exactly as the
            // admitted revision records it; the claim never becomes an
            // identity binding.
            "identification": admitted.provider_facts.get("identification").cloned(),
        });
        (result, OperationOutcome::Succeeded, EffectStatus::Complete)
    }

    /// Commit one evidence receipt carrying the exact observation. A claim
    /// that committed is `Complete`; a typed refusal is `None` — nothing
    /// outside this ledger was touched either way.
    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        mut result: serde_json::Value,
        outcome: OperationOutcome,
        effect: EffectStatus,
    ) -> Result<OperationRecord> {
        // The observation names itself: this operation/attempt is the reason
        // terminal a reader points at when it holds an attitude about the
        // claim (§51.1 理由指针, §51.2). Without it the pointer would have to
        // be reconstructed from a receipt it cannot see.
        if let Some(object) = result.as_object_mut() {
            object.insert(
                "operation_id".to_owned(),
                serde_json::json!(operation.operation_id),
            );
            object.insert(
                "attempt_id".to_owned(),
                serde_json::json!(attempt.attempt_id),
            );
        }
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "media-identify-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            // The exact observation stays sealed; the normalized result says
            // so instead of pretending the claim was smaller.
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "op": "identify",
                    "status": "observation_too_large",
                    "detail": "the exact identification observation is sealed in raw evidence",
                    "observation_bytes": bytes.len(),
                }),
                "media-identify-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "media.identify.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("media-identify:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            effect,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "media.identify" && invocation.task.definition_version == 1,
            "media_identify_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("media_identify_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("media_identify_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(
            std::mem::take(&mut invocation.input),
            invocation.task.agent_id,
        )?;
        let input_ref = self.seal(&frozen, "media-identify-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, frozen.agent_id, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        let (result, outcome, effect) = self.identify(&frozen).await;
        self.record(&operation, &attempt, result, outcome, effect)
    }
}

#[async_trait]
impl NativeTaskHandler for MediaIdentifyHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "media.identify".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(MEDIA_IDENTIFY_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-media-identifier/1".into(),
            authority_refs: vec!["store.media.identify.own_subject/v1".into()],
            prompt_section: Some(
                "media.identify/v1 是已安装的说话人识别 child。它让本机已配置的声纹引擎听某一段\
                 【已经封存】的原始音频，并把引擎的说话人 claim 作为同一段的新修订承认：\
                 [delegate:media.identify:1] {\"provider\":\"…\",\"account\":\"…\",\"stream_id\":\"…\",\"segment\":1}。\
                 它不会重新采集、重新下载、重新转写或改动原文本与音频身份；claim 带 confidence/corrected，\
                 未知保持未知，永远不构成身份权威。未配置声纹引擎、缺少该段或该段没有封存音频时返回显式错误，\
                 不伪造 claim。"
                    .into(),
            ),
        }
    }

    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        vec![EffectContractBuilder::new(
            descriptor.implementation_ref.plugin_id,
            descriptor.implementation(),
            "media.identify.v1",
            descriptor.definition_version,
        )
        .resource("store_media", "own_subject", ResourceAccess::Read)
        .resource("speaker_identity_engine", "configured_binding", ResourceAccess::Execute)
        .idempotency(IdempotencyGuarantee::BestEffort)
        .external_effect(ExternalEffect {
            kind: "speaker_engine_call".into(),
            description: "Sends the bytes of one already-sealed audio object to the configured speaker-identity engine; no capture, no download, no ASR and no mutation outside this ledger, whose only new fact is a claim revision of the same segment".into(),
            reversible: true,
            compensation_possible: false,
        })
        .checkpoint(CheckpointContract {
            capturable: Vec::new(),
            excluded: vec!["engine-side voiceprint session state".into()],
            coordinated: false,
            consistency: "none".into(),
        })
        .lifecycle(LifecycleContract {
            on_upgrade: "an in-flight identification is not re-armed; the attempt stays queryable".into(),
            on_uninstall: "the definition is withdrawn; committed claims and their audio references stay readable".into(),
            on_crash: "reconciled under the original armed attempt; a repeat is another claim of the SAME segment, never a second utterance and never a re-capture".into(),
            receipt_readable_after_removal: true,
        })
        .basis(GuaranteeBasis::ClaimOnly)
        .build()]
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: "native.media.identify".into(),
            version: "1".into(),
            description: "Ask the configured speaker-identity engine about one segment's already-sealed original audio and admit its claim as a new revision of that same segment".into(),
            usage: Some(
                "delegate:media.identify:1 {\"provider\":\"...\",\"account\":\"...\",\"stream_id\":\"...\",\"segment\":1}".into(),
            ),
        }]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "media.identify.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative. A later
            // observation must not erase it or create another attempt.
            return Ok(operation);
        }
        // The attempted identification may or may not have reached the
        // engine. Repeating it under the SAME attempt is safe by contract:
        // the engine only reads bytes this ledger already sealed, and a repeat
        // admits another claim of the SAME segment — never a second utterance,
        // never a re-capture, never new text.
        let frozen = self.frozen(&operation)?;
        let (result, outcome, effect) = self.identify(&frozen).await;
        self.record(&operation, &attempt, result, outcome, effect)
    }
}

/// Typed refusal reported with the observation. The engine's own protocol
/// code and retry strategy travel with it instead of being flattened.
fn classify(error: &ShenkoError) -> (String, Option<RetryStrategy>) {
    match error {
        ShenkoError::NotFound(_) => (
            "media_identify_unavailable".to_owned(),
            Some(RetryStrategy::NeverAutomatically),
        ),
        ShenkoError::Protocol(protocol) => (protocol.code.clone(), Some(protocol.retry)),
        ShenkoError::Conflict(_) => (
            "media_identify_conflict".to_owned(),
            Some(RetryStrategy::AfterReconcile),
        ),
        ShenkoError::Storage(_) => (
            "media_identify_storage".to_owned(),
            Some(RetryStrategy::NeverAutomatically),
        ),
        _ => ("media_identify_failed".to_owned(), None),
    }
}

fn failure(
    frozen: &FrozenMediaIdentifyInput,
    code: &str,
    retry: Option<RetryStrategy>,
    detail: impl Into<String>,
) -> serde_json::Value {
    let detail: String = detail.into();
    serde_json::json!({
        "schema_version": 1,
        "op": "identify",
        "agent_id": frozen.agent_id,
        "stream": frozen.stream,
        "segment": frozen.segment,
        "status": "failed",
        "error": {
            "code": code,
            "retry": retry,
            "detail": detail,
        },
    })
}

fn bounded_id(field: &str, value: String) -> Result<String> {
    ensure!(
        !value.is_empty() && value.len() <= MAX_ID_CHARS,
        "{field} must be 1..={MAX_ID_CHARS} chars"
    );
    Ok(value)
}

#[cfg(test)]
mod tests {
    //! Behavioral consumers of the `media.identify` child.
    //!
    //! No engine, model, audio device or external call is exercised: the
    //! scripted voiceprint engine answers about the exact sealed digest it was
    //! handed, so these tests prove which bytes reached it and what the ledger
    //! holds afterwards. They run the real `MediaIngress`, the real
    //! ledger/CAS, the real `MediaRuntime` and this child's own freeze/T1/E1
    //! flow — the subject-origin guard of `run()` needs a booted subject and is
    //! left to the native smoke.

    use super::*;
    use async_trait::async_trait;
    use crate::provider::SpeakerIdentity;
    use crate::runtime::MediaProviders;
    use shenko_store::TaskInvocation;
    use shenko_subject::inbox::{InboxEvent, InboxEventKind};
    use shenko_types::agent::CallContext;
    use shenko_types::error::{ErrorScope, ProtocolError};
    use shenko_types::store::WriteBatch;
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};

    const STREAM_ID: &str = "class-identify";
    const ORIGINAL_REVISION_ID: &str = "class-identify-s1-r1";

    fn stream() -> MediaStreamDescriptor {
        MediaStreamDescriptor {
            provider: "test-asr".into(),
            account: "acct-1".into(),
            stream_id: STREAM_ID.into(),
        }
    }

    fn speaker_binding() -> MediaProviderBinding {
        MediaProviderBinding {
            provider: "test-sv".into(),
            account: "acct-1".into(),
            capability: MediaProviderCapability::SpeakerIdentity,
            endpoint: "ws://127.0.0.1:1".into(),
            credential_env: None,
            model: Some("cam++".into()),
            sample_rate: 16000,
            options: serde_json::Map::new(),
        }
    }

    /// Scripted voiceprint engine: records every sealed digest it was asked
    /// about and answers with one engine claim (or the scripted typed
    /// failure) — never with an identity.
    struct ScriptedSpeaker {
        binding: MediaProviderBinding,
        calls: tokio::sync::Mutex<Vec<String>>,
        failure: Option<ShenkoError>,
    }

    #[async_trait]
    impl SpeakerIdentity for ScriptedSpeaker {
        fn binding(&self) -> &MediaProviderBinding {
            &self.binding
        }

        async fn identify(&self, audio: &ObjectRef) -> Result<MediaSpeakerClaim, ShenkoError> {
            self.calls.lock().await.push(audio.digest.to_string());
            if let Some(failure) = &self.failure {
                return Err(failure.clone());
            }
            Ok(MediaSpeakerClaim {
                speaker: Some("spk-teacher".into()),
                person_id: None,
                confidence: Some(0.82),
                corrected: false,
                source: Some("test-sv:v1".into()),
            })
        }
    }

    fn scripted_speaker(failure: Option<ShenkoError>) -> Arc<ScriptedSpeaker> {
        Arc::new(ScriptedSpeaker {
            binding: speaker_binding(),
            calls: tokio::sync::Mutex::new(Vec::new()),
            failure,
        })
    }

    struct Fixture {
        store: Arc<Store>,
        ingress: Arc<crate::MediaIngress>,
        runtime: Arc<MediaRuntime>,
        speaker: Option<Arc<ScriptedSpeaker>>,
        stream: MediaStreamDescriptor,
        agent_id: EntityId,
        inbox: tokio::sync::mpsc::Receiver<InboxEvent>,
    }

    /// A real ledger, the real ingress and a runtime bound to the scripted
    /// engine (or to no engine at all, which is the honest unconfigured case).
    fn fixture(speaker: Option<Arc<ScriptedSpeaker>>) -> Fixture {
        let dir = std::env::temp_dir().join(format!("shenko-media-identify-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let agent_id = EntityId::new();
        let (sender, inbox) = tokio::sync::mpsc::channel(64);
        let ingress = Arc::new(crate::MediaIngress::new(Arc::clone(&store), agent_id, sender));
        let runtime = MediaRuntime::new(
            Arc::clone(&ingress),
            Arc::clone(&store),
            MediaProviders {
                realtime: Vec::new(),
                speech: None,
                speaker: speaker
                    .as_ref()
                    .map(|speaker| Arc::clone(speaker) as Arc<dyn SpeakerIdentity>),
            },
        );
        Fixture {
            store,
            ingress,
            runtime,
            speaker,
            stream: stream(),
            agent_id,
            inbox,
        }
    }

    /// A canonical mini WAVE sealed in this ledger's CAS, exactly where a
    /// recording segment seal puts the original audio.
    fn seal_wav(store: &Store, pcm: &[u8]) -> ObjectRef {
        let data_bytes = pcm.len() as u32;
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&16_000u32.to_le_bytes());
        wav.extend_from_slice(&32_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        wav.extend_from_slice(pcm);
        store
            .cas
            .seal_object(&wav, "audio/wav", "media-audio", "wav-pcm16", 1)
            .unwrap()
            .object
    }

    /// Admit one finalized segment the way a transcript provider does.
    async fn admit_final(
        fixture: &Fixture,
        segment: u64,
        text: &str,
        audio: Option<MediaAudioRef>,
    ) {
        fixture
            .ingress
            .admit_revision(MediaRevisionInput {
                revision_id: if segment == 1 {
                    ORIGINAL_REVISION_ID.to_owned()
                } else {
                    format!("{STREAM_ID}-s{segment}-r1")
                },
                stream: fixture.stream.clone(),
                segment,
                revision: 1,
                text: text.to_owned(),
                partial: false,
                captured_at_ms: Some(1_760_000_000_000),
                speaker: None,
                audio,
                provider_facts: serde_json::Map::new(),
            })
            .await
            .unwrap();
    }

    fn drain(fixture: &mut Fixture) -> Vec<InboxEvent> {
        std::iter::from_fn(|| fixture.inbox.try_recv().ok()).collect()
    }

    /// The frozen intent, the sealed input, a committed task row and a real T1
    /// arm — the exact prefix `run()` performs before it touches the engine.
    struct Armed {
        invocation: NativeTaskInvocation,
        task: TaskRecord,
        operation: OperationRecord,
        attempt: AttemptRecord,
        frozen: FrozenMediaIdentifyInput,
    }

    fn arm(
        handler: &MediaIdentifyHandler,
        fixture: &Fixture,
        segment: u64,
    ) -> Armed {
        let frozen = handler
            .freeze(
                serde_json::json!({
                    "provider": fixture.stream.provider,
                    "account": fixture.stream.account,
                    "stream_id": fixture.stream.stream_id,
                    "segment": segment,
                }),
                fixture.agent_id,
            )
            .unwrap();
        let input_ref = handler.seal(&frozen, "media-identify-frozen-input").unwrap();
        let (task_id, operation_id, invocation_id) =
            (EntityId::new(), EntityId::new(), EntityId::new());
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(fixture.store.ledger.ledger_id(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id: fixture.agent_id,
            conversation_id: fixture.agent_id,
            owner: TaskOwner::Task { id: EntityId::new() },
            background: false,
            kind: "media.identify".into(),
            definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
            input_ref: input_ref.clone(),
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        fixture.store.commit(batch).unwrap();
        let task_invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation_id);
        context.controller_epoch = epoch;
        let invocation = NativeTaskInvocation {
            context,
            task_invocation: task_invocation.clone(),
            task: task.clone(),
            operation_id,
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input: serde_json::Value::Null,
            child_tools: None,
        };
        let (mut operation, mut attempt) =
            handler
                .operation_parts(&invocation, frozen.agent_id, input_ref)
                .unwrap();
        assert!(
            fixture
                .store
                .arm_external_operation(&task_invocation, &mut operation, &mut attempt)
                .unwrap()
                .is_none(),
            "the first arm of this operation must be new"
        );
        Armed {
            invocation,
            task,
            operation,
            attempt,
            frozen,
        }
    }

    fn observed(
        store: &Store,
        operation_id: EntityId,
    ) -> (Vec<EvidenceReceipt>, serde_json::Value) {
        let evidence = store.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1, "one observation, one evidence receipt");
        let bytes = store.cas.read_object(&evidence[0].raw_ref.digest).unwrap();
        assert_eq!(bytes.len() as u64, evidence[0].raw_ref.bytes);
        (evidence, serde_json::from_slice(&bytes).unwrap())
    }

    /// A real native-side identification reads the bytes this ledger already
    /// sealed, admits the engine claim as ONE new revision of the same
    /// utterance, keeps the retained provider text and the original audio
    /// reference, and records exactly one evidence/result.
    #[tokio::test]
    async fn native_identify_claims_the_original_sealed_audio_once() {
        let mut fixture = fixture(Some(scripted_speaker(None)));
        let original = seal_wav(&fixture.store, &[1, 2, 3, 4, 5, 6]);
        admit_final(
            &fixture,
            1,
            "老师提问",
            Some(MediaAudioRef::Sealed {
                object: original.clone(),
            }),
        )
        .await;
        assert_eq!(drain(&mut fixture).len(), 1, "the final is one fact");
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.runtime),
        );
        let armed = arm(&handler, &fixture, 1);

        let (result, outcome, effect) = handler.identify(&armed.frozen).await;
        assert_eq!(outcome, OperationOutcome::Succeeded);
        assert_eq!(effect, EffectStatus::Complete);
        let resolved = handler
            .record(&armed.operation, &armed.attempt, result, outcome, effect)
            .unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(resolved.effect_status, EffectStatus::Complete);

        let (_, observation) = observed(&fixture.store, armed.invocation.operation_id);
        assert_eq!(observation["status"], "succeeded");
        assert_eq!(observation["segment"], 1);
        assert_eq!(
            observation["text"], "老师提问",
            "the engine's words are reported exactly as retained"
        );
        assert_eq!(
            observation["audio_digest"],
            original.digest.to_string(),
            "the identified audio identity is the original sealed object"
        );
        // The observation names itself (reason terminal) and the perception
        // that produced the claim (§51.1 理由指针, §51.2).
        assert_eq!(
            observation["operation_id"],
            serde_json::json!(armed.invocation.operation_id)
        );
        assert_eq!(
            observation["attempt_id"],
            serde_json::json!(armed.attempt.attempt_id)
        );
        let identification: MediaIdentification =
            serde_json::from_value(observation["identification"].clone())
                .expect("the admitted revision carries typed identification provenance");
        assert_eq!(identification.schema_version, 1);
        assert_eq!(
            identification.observed_revision_id, ORIGINAL_REVISION_ID,
            "the reason stops at the revision that sealed this audio"
        );
        assert_eq!(
            identification.observed_audio, original,
            "the perception observed the original sealed object, unchanged"
        );
        assert_eq!(identification.perception.provider, "test-sv");
        assert_eq!(identification.perception.account, "acct-1");
        assert_eq!(identification.perception.model.as_deref(), Some("cam++"));
        assert_eq!(
            identification.perception.reported_source.as_deref(),
            Some("test-sv:v1"),
            "the engine identity is reported WITH the claim, not inferred"
        );
        let content: MediaIdentificationContent = serde_json::from_slice(
            &fixture
                .store
                .cas
                .read_object(&identification.content.digest)
                .unwrap(),
        )
        .expect("the claim's content object decodes as its own bytes");
        assert_eq!(content.claim.speaker.as_deref(), Some("spk-teacher"));
        assert_eq!(content.perception, identification.perception);
        assert_eq!(content.observed_revision_id, ORIGINAL_REVISION_ID);
        assert_eq!(content.observed_audio, identification.observed_audio);
        assert_eq!(
            content.stream, fixture.stream,
            "one content identity is scoped to its own stream and segment"
        );
        assert_eq!(content.segment, 1);
        assert_eq!(observation["claim"]["speaker"], "spk-teacher");
        assert_eq!(observation["claim"]["corrected"], false);
        assert_eq!(observation["receipt"]["replayed"], false);

        // One claim revision of the SAME utterance; audio and text untouched.
        let effective = fixture
            .store
            .media_effective_segment(&fixture.stream, 1)
            .unwrap()
            .expect("segment 1 is still admitted");
        assert_eq!(effective.text, "老师提问");
        assert!(!effective.partial);
        assert_eq!(
            effective.audio,
            Some(MediaAudioRef::Sealed {
                object: original.clone()
            })
        );
        assert_eq!(
            effective.disposition,
            MediaRevisionDisposition::FinalCorrection,
            "a new observation of a finalized segment is a correction of it"
        );
        let claim = effective.speaker.clone().expect("engine claim admitted");
        assert_eq!(claim.speaker.as_deref(), Some("spk-teacher"));
        assert_eq!(claim.confidence, Some(0.82));
        assert_eq!(claim.source.as_deref(), Some("test-sv:v1"));
        assert!(
            !claim.corrected,
            "the segment had no earlier claim to correct"
        );
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2,
            "one retained provider fact plus one claim revision"
        );

        // The engine read the sealed bytes and nothing was re-captured: the
        // CAS object is unchanged and the recording usage gained no audio.
        assert_eq!(
            *fixture.speaker.as_ref().unwrap().calls.lock().await,
            vec![original.digest.to_string()],
            "the voiceprint engine reads the bytes this ledger already sealed"
        );
        assert_eq!(
            fixture
                .store
                .cas
                .read_object(&original.digest)
                .unwrap()
                .len() as u64,
            original.bytes
        );

        // The claim reached the same subject inbox as one media revision.
        let events = drain(&mut fixture);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, InboxEventKind::MediaRevision);
    }

    /// An unconfigured speaker binding, a missing segment and a segment with
    /// no archived audio are explicit typed refusals — never a fabricated
    /// claim, and never a new revision.
    #[tokio::test]
    async fn unavailable_engine_and_missing_audio_are_typed_refusals() {
        // (a) no speaker binding configured at all.
        let mut unconfigured = fixture(None);
        let original = seal_wav(&unconfigured.store, &[1, 2, 3, 4]);
        admit_final(
            &unconfigured,
            1,
            "老师提问",
            Some(MediaAudioRef::Sealed { object: original }),
        )
        .await;
        drain(&mut unconfigured);
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&unconfigured.store),
            Arc::clone(&unconfigured.runtime),
        );
        let armed = arm(&handler, &unconfigured, 1);
        let (result, outcome, effect) = handler.identify(&armed.frozen).await;
        assert_eq!(outcome, OperationOutcome::Failed);
        assert_eq!(effect, EffectStatus::None);
        assert_eq!(result["error"]["code"], "media_identify_unavailable");
        assert_eq!(result["error"]["retry"], "never_automatically");
        let refused = handler
            .record(&armed.operation, &armed.attempt, result, outcome, effect)
            .unwrap();
        assert_eq!(refused.state, OperationState::Resolved);
        assert_eq!(refused.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            unconfigured
                .store
                .media_revisions(&unconfigured.stream, 1)
                .unwrap()
                .len(),
            1,
            "a refused identification commits no revision"
        );
        assert!(drain(&mut unconfigured).is_empty());

        // (b) an engine is configured: a segment with no admitted revision and
        // a segment whose audio stayed provider-local are both unavailable.
        let mut configured = fixture(Some(scripted_speaker(None)));
        admit_final(
            &configured,
            2,
            "学生回答",
            Some(MediaAudioRef::Provider(ProviderObjectRef {
                locator: "provider://call/segment-2.wav".into(),
                digest: Some("provider-side-digest".into()),
                media_type: Some("audio/wav".into()),
            })),
        )
        .await;
        drain(&mut configured);
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&configured.store),
            Arc::clone(&configured.runtime),
        );
        for segment in [9, 2] {
            let armed = arm(&handler, &configured, segment);
            let (result, outcome, effect) = handler.identify(&armed.frozen).await;
            assert_eq!(outcome, OperationOutcome::Failed, "segment {segment}");
            assert_eq!(
                result["error"]["code"], "media_identify_unavailable",
                "segment {segment} is refused explicitly"
            );
            let resolved = handler
                .record(&armed.operation, &armed.attempt, result, outcome, effect)
                .unwrap();
            assert_eq!(resolved.outcome, Some(OperationOutcome::Failed));
        }
        assert_eq!(
            configured
                .store
                .media_revisions(&configured.stream, 2)
                .unwrap()
                .len(),
            1,
            "an unarchived audio handle is never identified"
        );
        assert!(configured.speaker.as_ref().unwrap().calls.lock().await.is_empty());
        assert!(drain(&mut configured).is_empty());
    }

    /// The engine's own typed protocol failure (an unreachable voiceprint
    /// service) travels with its code and retry strategy instead of being
    /// flattened into a generic failure.
    #[tokio::test]
    async fn engine_wire_failure_keeps_its_typed_code() {
        let failure = ShenkoError::Protocol(ProtocolError {
            code: "media_engine_unreachable".into(),
            scope: ErrorScope::Operation,
            retry: RetryStrategy::AfterReconcile,
            operation_id: None,
            commit_ref: None,
            details: serde_json::json!({ "detail": "connect refused" }),
        });
        let mut fixture = fixture(Some(scripted_speaker(Some(failure))));
        let original = seal_wav(&fixture.store, &[9, 8, 7, 6]);
        admit_final(
            &fixture,
            1,
            "老师提问",
            Some(MediaAudioRef::Sealed { object: original }),
        )
        .await;
        drain(&mut fixture);
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.runtime),
        );
        let armed = arm(&handler, &fixture, 1);
        let (result, outcome, effect) = handler.identify(&armed.frozen).await;
        assert_eq!(outcome, OperationOutcome::Failed);
        assert_eq!(effect, EffectStatus::None);
        assert_eq!(result["error"]["code"], "media_engine_unreachable");
        assert_eq!(result["error"]["retry"], "after_reconcile");
        let resolved = handler
            .record(&armed.operation, &armed.attempt, result, outcome, effect)
            .unwrap();
        assert_eq!(resolved.outcome, Some(OperationOutcome::Failed));
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            1,
            "a failed engine call admits nothing"
        );
    }

    /// A crash after T1 but before E1 is reconciled on the ORIGINAL attempt:
    /// one additional observation, one attempt, and a resolved operation. A
    /// resolved operation is never observed again.
    #[tokio::test]
    async fn reconcile_completes_the_original_attempt_once() {
        let mut fixture = fixture(Some(scripted_speaker(None)));
        let original = seal_wav(&fixture.store, &[5, 4, 3, 2]);
        admit_final(
            &fixture,
            1,
            "学生回答",
            Some(MediaAudioRef::Sealed {
                object: original.clone(),
            }),
        )
        .await;
        drain(&mut fixture);
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.runtime),
        );
        let armed = arm(&handler, &fixture, 1);
        // Deterministic crash point: T1 is durable, no observation exists.
        assert!(
            fixture
                .store
                .external_operation_evidence(armed.invocation.operation_id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fixture
                .store
                .external_operation(armed.invocation.operation_id)
                .unwrap()
                .state,
            OperationState::InFlight
        );

        let resolved = handler
            .reconcile(&armed.task, armed.invocation.operation_id)
            .await
            .unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(
            fixture
                .store
                .external_operation_attempts(armed.invocation.operation_id)
                .unwrap()
                .len(),
            1,
            "recovery never arms a second attempt"
        );
        let (_, observation) = observed(&fixture.store, armed.invocation.operation_id);
        assert_eq!(observation["status"], "succeeded");
        assert_eq!(observation["audio_digest"], original.digest.to_string());
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2
        );

        // Reconciling a resolved operation is a read, not another engine call.
        let again = handler
            .reconcile(&armed.task, armed.invocation.operation_id)
            .await
            .unwrap();
        assert_eq!(again.state, OperationState::Resolved);
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(fixture.speaker.as_ref().unwrap().calls.lock().await.len(), 1);
    }

    /// The observation result is content (§51.1): the SAME claim about the
    /// SAME sealed audio is ONE identity. A crash between admission and E1
    /// reconciles onto the original revision instead of minting a second one,
    /// and a repeat wakes nobody.
    #[tokio::test]
    async fn repeat_identification_is_one_content_identity() {
        let mut fixture = fixture(Some(scripted_speaker(None)));
        let original = seal_wav(&fixture.store, &[7, 7, 7, 7]);
        admit_final(
            &fixture,
            1,
            "老师提问",
            Some(MediaAudioRef::Sealed {
                object: original.clone(),
            }),
        )
        .await;
        drain(&mut fixture);
        let handler = MediaIdentifyHandler::new(
            Arc::clone(&fixture.store),
            Arc::clone(&fixture.runtime),
        );
        let armed = arm(&handler, &fixture, 1);
        // Deterministic crash point: the runtime admitted the claim, the
        // operation recorded no evidence yet.
        let first = fixture
            .runtime
            .identify_speaker(&fixture.stream, 1)
            .await
            .unwrap();
        assert!(!first.admission.receipt.replayed);
        let first_revision = first.admission.receipt.revision_id.clone();
        let first_event = first.admission.receipt.event_id;
        assert_eq!(drain(&mut fixture).len(), 1, "the admitted claim wakes once");
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2,
            "one retained provider revision plus one claim revision"
        );

        let resolved = handler
            .reconcile(&armed.task, armed.invocation.operation_id)
            .await
            .unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        let (_, observation) = observed(&fixture.store, armed.invocation.operation_id);
        assert_eq!(
            observation["receipt"]["revision_id"],
            serde_json::json!(first_revision),
            "the recovered observation is the one already admitted"
        );
        assert_eq!(observation["receipt"]["replayed"], true);
        assert_eq!(
            observation["receipt"]["event_id"],
            serde_json::json!(first_event),
            "a replay returns the same bound event, never a phantom one"
        );
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2,
            "identical content bytes are never a second revision"
        );
        assert!(drain(&mut fixture).is_empty(), "a replay wakes nobody");
        assert_eq!(
            fixture.speaker.as_ref().unwrap().calls.lock().await.len(),
            2,
            "recovery re-reads the sealed bytes instead of inventing a claim"
        );

        // An explicit repeat through the live runtime gets the same answer.
        let again = fixture
            .runtime
            .identify_speaker(&fixture.stream, 1)
            .await
            .unwrap();
        assert!(again.admission.receipt.replayed);
        assert_eq!(again.admission.receipt.revision_id, first_revision);
        assert!(matches!(
            again.delivery,
            crate::SubjectDelivery::Replay
        ));
        assert_eq!(
            fixture
                .store
                .media_revisions(&fixture.stream, 1)
                .unwrap()
                .len(),
            2
        );
    }

    /// The exposed binding follows the native-definition conventions: one
    /// versioned kind, one effect contract naming the engine call and its
    /// idempotency, and one capability declaration a subject can see.
    #[test]
    fn descriptor_contract_and_capability_are_aligned() {
        let f = fixture(None);
        let handler =
            MediaIdentifyHandler::new(Arc::clone(&f.store), Arc::clone(&f.runtime));
        let descriptor = handler.descriptor();
        assert_eq!(descriptor.kind, "media.identify");
        assert_eq!(descriptor.definition_version, 1);
        assert!(
            descriptor
                .prompt_section
                .as_deref()
                .unwrap()
                .contains("[delegate:media.identify:1]")
        );
        let contracts = handler.effect_contracts();
        assert_eq!(contracts.len(), 1);
        let contract = &contracts[0];
        assert_eq!(contract.operation_name, "media.identify.v1");
        assert_eq!(contract.idempotency, IdempotencyGuarantee::BestEffort);
        assert_eq!(contract.external_effects.len(), 1);
        assert_eq!(contract.external_effects[0].kind, "speaker_engine_call");
        assert!(contract.is_idempotent());
        let capabilities = handler.capabilities();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].capability_id, "native.media.identify");
        assert_eq!(capabilities[0].version, "1");
    }
}
