//! Shared T1/E1 mechanics, not a protocol adapter or an installation authority.
//! The two plugins provide their own contracts, dispatch and lifecycle.

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskInvocation};
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::{commit::CommitRef, evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*, object::*, operation::*, task::{TaskRecord, NativeEvidenceObservation,
        NATIVE_EVIDENCE_OBSERVATION_SOURCE}};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// A normalized observation stays inline below this size. Larger observations
/// are sealed once as a raw artifact plus a small chunk-indexed manifest, so
/// every returned range is authenticated without rescanning the artifact.
pub const INLINE_OBSERVATION_BYTES: usize = 256 * 1024;
pub const ARTIFACT_CHUNK_BYTES: usize = 1024 * 1024;

pub fn seal<T: Serialize>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    match store.cas.seal_object(&serde_json::to_vec(value)?, "application/json", "ecosystem-compat", schema, 1) {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => { store.close_external_writer(); Err(error.into()) }
    }
}

/// One observation's durable form: the receipt object (manifest when
/// outlined) plus the JSON embedded in the normalized result.
struct ObservationSeal {
    receipt: ObjectRef,
    observation: Value,
}

/// Seal the actual observation bytes. Small values stay inline; larger values
/// stream once through CAS (fixed staging memory) and get a per-1 MiB chunk
/// manifest that `compat.artifact.read` can range over with per-chunk
/// authentication. The manifest is the public identity: `learning_source`
/// keeps pointing at a small, retrievable object.
fn seal_observation(store: &Store, value: &Value) -> Result<ObservationSeal> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() <= INLINE_OBSERVATION_BYTES {
        let receipt = seal(store, &value, "compat-observation")?;
        return Ok(ObservationSeal { receipt, observation: value.clone() });
    }
    let artifact = match store.cas.seal_reader(&mut std::io::Cursor::new(&bytes), "application/json", "ecosystem-compat", "compat-observation-bytes", 1) {
        Ok(sealed) => sealed.object,
        Err(error) => { store.close_external_writer(); return Err(error.into()); }
    };
    let chunks = bytes.chunks(ARTIFACT_CHUNK_BYTES).map(shenko_types::object::Digest::blake3).collect::<Vec<_>>();
    let manifest = seal(store, &json!({
        "format": "compat-indexed-observation", "schema_version": 1,
        "artifact_digest": artifact.digest, "bytes": bytes.len(),
        "chunk_bytes": ARTIFACT_CHUNK_BYTES, "chunks": chunks,
    }), "compat-observation-index")?;
    Ok(ObservationSeal {
        receipt: manifest,
        observation: json!({"status": "observation_outlined",
            "artifact": {"digest": artifact.digest, "bytes": bytes.len()},
            "read": {"handler": "compat.artifact.read", "digest": "use source_receipt digest"}}),
    })
}

pub struct RecordedCall {
    pub operation: OperationRecord,
    pub attempt: AttemptRecord,
}

impl RecordedCall {
    pub fn prepare<T: Serialize>(store: &Store, invocation: &NativeTaskInvocation,
        descriptor: &NativeTaskDescriptor, input: &T, binding: Value, acceptance: &str, retry: &str) -> Result<Self> {
        let input_ref = seal(store, input, "compat-frozen-input")?;
        let bindings = seal(store, &ResourceBindings {
            provider: descriptor.kind.clone(), account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(), mount_generation: 0,
            capability_level: "explicit_external_compatibility".into(), extra: binding,
        }, "compat-bindings")?;
        let digest = descriptor.implementation_ref.artifact_digest.clone();
        let version = VersionRef {
            artifact: ArtifactRef { digest: digest.clone(), locator: format!("builtin:{}:{}", descriptor.kind, descriptor.definition_version) },
            symbol: descriptor.kind.clone(), schema: SchemaRef { name: "compat-receipt".into(), major: 1, digest: digest.clone() },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1, operation_id: invocation.operation_id, record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id, logical_key: "native-handler".into(), operation_name: descriptor.kind.clone(),
            input_schema_version: 1, output_schema_version: 1, input_digest: input_ref.digest.clone(), final_input_ref: input_ref,
            contract_ref: OperationContractRef { plugin_id: descriptor.implementation_ref.plugin_id,
                artifact_digest: digest.clone(), contract_digest: digest, receipt_reader_ref: version.clone() },
            resource_bindings_ref: bindings, authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: seal(store, &acceptance, "compat-acceptance")?, retry_contract_ref: seal(store, &retry, "compat-retry")?,
            checkpoint_scope_ref: None, state: OperationState::InFlight, outcome: None, outcome_conflict: false,
            effect_status: EffectStatus::Unknown, retry_of: None, compensates: None, possible_duplicate_of: vec![],
            prepared_commit: zero, evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1, attempt_id: EntityId::new(), operation_id: invocation.operation_id, attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id, worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id, epoch: invocation.task_invocation.epoch, implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed, armed_commit: zero, provider_handle: None, provider_idempotency: None,
            transport_ref: None, started_at: None, last_observed_at: None, final_receipt_id: None,
        };
        Ok(Self { operation, attempt })
    }

    pub fn arm(&mut self, store: &Store, invocation: &NativeTaskInvocation) -> Result<Option<OperationRecord>> {
        store.arm_external_operation(&invocation.task_invocation, &mut self.operation, &mut self.attempt)
    }

    pub fn observe(&self, store: &Store, value: Value, outcome: Option<OperationOutcome>, effect: EffectStatus,
        level: Option<ObservationLevel>) -> Result<OperationRecord> {
        let sealed = seal_observation(store, &value)?;
        // Existing cognition.unit.write verifies this immutable CAS source.
        // It remains an observation, never an automatic adoption judgement.
        let normalized = seal(store, &json!({"observation":sealed.observation,"source_receipt":sealed.receipt,
            "learning_source":{"kind":"cas_object","reference":format!("operation:{}",self.operation.operation_id),"digest":sealed.receipt.digest}}), "compat-result")?;
        let operation = store.record_external_evidence(self.operation.operation_id, self.attempt.attempt_id, EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(self.operation.operation_id), attempt_id: Some(self.attempt.attempt_id),
            source_id: self.operation.operation_name.clone(), source_instance_id: self.attempt.node_id.to_string(), source_event_key: None,
            producer_epoch: Some(self.attempt.epoch.0), producer_schema_version: Some("1".into()), raw_ref: sealed.receipt,
            decoder_ref: Some(self.operation.contract_ref.receipt_reader_ref.clone()), decode_status: DecodeStatus::Decoded,
            observation_level: level, normalized_ref: Some(normalized), observed_at: Some(chrono::Utc::now()), received_commit: self.operation.prepared_commit,
        }, outcome, effect)?;
        publish_unresolved_observation(store, &operation)?;
        Ok(operation)
    }
}

/// Deliver the original E1 as input, without settling the task or modifying
/// the operation. Rebuilding from its durable receipt makes admission repair
/// idempotent even when the prior controller stopped after E1 committed.
fn publish_unresolved_observation(store: &Store, operation: &OperationRecord) -> Result<()> {
    if operation.state != OperationState::Reconciling || operation.outcome.is_some()
        || operation.effect_status != EffectStatus::Unknown {
        return Ok(());
    }
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    let receipt_id = attempt.final_receipt_id.context("unresolved compatibility attempt has no original E1")?;
    let evidence = store.external_evidence(receipt_id)?;
    let task = store.external_task(operation.task_id)?;
    ensure!(task.task_id == operation.task_id
        && attempt.operation_id == operation.operation_id
        && attempt.dispatch_stage == AttemptDispatchStage::Unknown
        && evidence.receipt_id == receipt_id
        && evidence.operation_id == Some(operation.operation_id)
        && evidence.attempt_id == Some(attempt.attempt_id)
        && evidence.source_id == operation.operation_name
        && evidence.source_instance_id == attempt.node_id.to_string()
        && evidence.source_event_key.is_none()
        && evidence.producer_epoch == Some(attempt.epoch.0)
        && evidence.producer_schema_version.as_deref() == Some("1")
        && task.kind == operation.operation_name && !task.is_terminal(),
        "unresolved compatibility evidence identity mismatch");
    let observed_at = evidence.observed_at.context("original compatibility E1 has no observation time")?;
    let observation = NativeEvidenceObservation {
        schema_version: 1, task_id: task.task_id,
        operation_id: operation.operation_id, operation_name: operation.operation_name.clone(),
        operation_state: operation.state, outcome: operation.outcome,
        effect_status: operation.effect_status, attempt_id: attempt.attempt_id,
        receipt_id, raw_ref: evidence.raw_ref, normalized_ref: evidence.normalized_ref,
        observed_at, received_commit: evidence.received_commit,
    };
    let event = InboxEvent {
        event_id: receipt_id, kind: InboxEventKind::SystemEvent,
        payload: serde_json::to_value(observation)?, created_at: observed_at,
        source: NATIVE_EVIDENCE_OBSERVATION_SOURCE.into(),
    };
    store.admit_inbox_event(receipt_id, task.agent_id, "system_event",
        NATIVE_EVIDENCE_OBSERVATION_SOURCE, &serde_json::to_value(event)?)
        .with_context(|| format!("admitting original native E1 task={} operation={} attempt={} receipt={receipt_id}",
            task.task_id, operation.operation_id, attempt.attempt_id))?;
    Ok(())
}

pub fn reconcile_original(store: &Store, task: &TaskRecord, operation_id: EntityId, name: &str) -> Result<OperationRecord> {
    let operation = store.external_operation(operation_id)?;
    ensure!(operation.task_id == task.task_id && operation.operation_name == name, "original compatibility operation mismatch");
    if operation.state == OperationState::InFlight {
        let attempt = store.external_operation_attempt(operation_id)?;
        if attempt.dispatch_stage != AttemptDispatchStage::Ended {
            let value = json!({"status":"unknown","reason":"original controller completion unavailable; request was not repeated",
                "frozen_input_ref":operation.final_input_ref,"remote_effects_stopped":false});
            return RecordedCall { operation, attempt }.observe(store, value, None, EffectStatus::Unknown, None);
        }
    }
    publish_unresolved_observation(store, &operation)?;
    Ok(operation)
}

/// Each plugin generation owns a distinct instance. Admission and task
/// tracking are atomic with retirement; an abandoned caller leaves its real
/// worker owned here until its external attempt is settled or marked unknown.
#[derive(Default)]
pub struct Workers {
    closed: Mutex<bool>,
    tracker: TaskTracker,
    pub stop: CancellationToken,
}

impl Workers {
    pub fn spawn<F, T>(&self, future: F) -> Result<tokio::task::JoinHandle<T>>
    where F: Future<Output = T> + Send + 'static, T: Send + 'static {
        let closed = self.closed.lock();
        ensure!(!*closed, "compatibility plugin is retiring");
        Ok(self.tracker.spawn(future))
    }

    pub async fn close(&self) {
        *self.closed.lock() = true;
        self.stop.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }
}

pub async fn joined<T>(worker: tokio::task::JoinHandle<Result<T>>) -> Result<T> {
    worker.await.context("compatibility worker lost; reconcile original attempt")?
}
