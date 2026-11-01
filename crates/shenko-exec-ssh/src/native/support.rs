//! Shared at-most-once ceremony for the SSH/seat native handlers — arm once,
//! effect once, observe with evidence; reconcile query-only. Modeled on the
//! other installed native children (node selection, WASI exec), so all
//! native task authority in the product speaks one recovery language.

use crate::SshBinding;
use anyhow::{Context as _, Result, ensure};
use base64::Engine as _;
use russh::keys::PrivateKeyWithHashAlg;
use shenko_store::Store;
use shenko_subject::NativeTaskInvocation;
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::TaskRecord,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

const B64: base64::engine::general_purpose::Standard = base64::engine::general_purpose::Standard;

pub fn encode_b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn decode_b64(text: &str) -> Result<Vec<u8>> {
    Ok(B64.decode(text)?)
}

/// Freeze-time → effect-time seal helper. A CAS seal failure is classified
/// as storage by the caller's close_external_writer convention, matching the
/// other native children.
pub fn seal<T: serde::Serialize>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    let bytes = serde_json::to_vec(value)?;
    match store
        .cas
        .seal_object(&bytes, "application/json", "native-seat-ssh", schema, 1)
    {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

/// The defensive wiring checks every installed native child performs: the
/// installed registry owns claim/authority; these ensures only catch
/// mismatched wiring, they do not authenticate arbitrary Rust callers.
pub fn verify_wiring(
    store: &Store,
    invocation: &NativeTaskInvocation,
    kind: &str,
    definition_version: u32,
    plugin_id: EntityId,
) -> Result<()> {
    let claim = &invocation.task_invocation;
    let step_id = invocation
        .task
        .created_by_step_id
        .with_context(|| format!("{kind} has no subject step"))?;
    ensure!(
        invocation.task.kind == kind && invocation.task.definition_version == definition_version,
        "{kind}_task_identity_mismatch"
    );
    ensure!(
        invocation.task.task_id == claim.task_id
            && invocation.task.task_revision == claim.task_revision
            && invocation.context.task_id == Some(claim.task_id)
            && invocation.context.expected_task_revision == Some(claim.task_revision.0)
            && invocation.context.invocation_id == Some(claim.invocation_id)
            && invocation.context.controller_epoch == claim.epoch
            && invocation.context.software_generation == claim.software_generation
            && invocation.authority.delegation_id == claim.task_id
            && invocation.authority.delegation_revision.0 == claim.task_revision.0
            && invocation.authority.decision_ref == step_id,
        "{kind}_trusted_invocation_mismatch"
    );
    let (head, _) = store.load_subject(invocation.task.agent_id)?;
    store
        .get_step(&head.branch_id, step_id)
        .with_context(|| format!("{kind}_origin_outside_subject_branch"))?;
    Ok(())
}

/// Build the OperationRecord + AttemptRecord pair for one external arm.
pub fn operation_parts(
    store: &Store,
    invocation: &NativeTaskInvocation,
    plugin_id: EntityId,
    contract_digest: Digest,
    operation_name: &str,
    input_ref: ObjectRef,
    bindings: serde_json::Value,
    acceptance: &str,
    retry: &str,
) -> Result<(OperationRecord, AttemptRecord)> {
    let resource = seal(store, &bindings, "seat-ssh-record-binding")?;
    let acceptance = seal(store, &acceptance, "seat-ssh-record-acceptance")?;
    let retry = seal(store, &retry, "seat-ssh-record-retry")?;
    let version = VersionRef {
        artifact: ArtifactRef {
            digest: contract_digest.clone(),
            locator: format!("builtin:{operation_name}"),
        },
        symbol: format!("{operation_name}.execute"),
        schema: SchemaRef {
            name: format!("{operation_name}-observation-v1"),
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
        operation_name: operation_name.into(),
        input_schema_version: 1,
        output_schema_version: 1,
        input_digest: input_ref.digest.clone(),
        final_input_ref: input_ref,
        contract_ref: OperationContractRef {
            plugin_id,
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

/// Read back and verify a frozen input under its original operation.
pub fn frozen_read<T: serde::de::DeserializeOwned + serde::Serialize>(
    store: &Store,
    operation: &OperationRecord,
    schema_name: &str,
) -> Result<T> {
    let object = &operation.final_input_ref;
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes
            && Digest::blake3(&bytes) == object.digest
            && operation.input_digest == object.digest,
        "{schema_name}_frozen_input_mismatch"
    );
    let frozen: T = serde_json::from_slice(&bytes)?;
    Ok(frozen)
}

/// Record the observation payload as the operation's evidence and resolve
/// its outcome/effect in the same store call.
pub fn observe_record(
    store: &Store,
    operation: &OperationRecord,
    attempt: &AttemptRecord,
    source_id: &str,
    payload: &serde_json::Value,
    outcome: Option<OperationOutcome>,
    effect: EffectStatus,
) -> Result<OperationRecord> {
    let sealed = seal(store, payload, &format!("{source_id}-observation"))?;
    let evidence = EvidenceReceipt {
        record_schema_version: 1,
        receipt_id: EntityId::new(),
        operation_id: Some(operation.operation_id),
        attempt_id: Some(attempt.attempt_id),
        source_id: source_id.into(),
        source_instance_id: store.capabilities.incarnation.to_string(),
        source_event_key: Some(format!("{source_id}:{}", attempt.attempt_id)),
        producer_epoch: Some(attempt.epoch.0),
        producer_schema_version: Some("1".into()),
        raw_ref: sealed.clone(),
        decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
        decode_status: DecodeStatus::Decoded,
        observation_level: None,
        normalized_ref: Some(sealed),
        observed_at: Some(chrono::Utc::now()),
        received_commit: operation.prepared_commit,
    };
    store.record_external_evidence(operation.operation_id, attempt.attempt_id, evidence, outcome, effect)
}

/// Query-only reconcile shared by all four handlers: verify the original
/// contract, and if the operation is still InFlight (controller recovered
/// mid-effect) record one E1 evidence that the effect is unknown — never
/// retry the effect.
pub fn reconcile_query_only(
    store: &Store,
    task: &TaskRecord,
    operation_id: EntityId,
    kind: &str,
    definition_version: u32,
    plugin_id: EntityId,
    operation_name: &str,
    contract_digest: Digest,
    recovery_schema: &str,
    recovery_note: &str,
) -> Result<OperationRecord> {
    let operation = store.external_operation(operation_id)?;
    ensure!(
        operation.task_id == task.task_id
            && operation.operation_name == operation_name
            && task.kind == kind
            && task.definition_version == definition_version
            && operation.contract_ref.plugin_id == plugin_id
            && operation.contract_ref.contract_digest == contract_digest,
        "original_operation_contract_mismatch"
    );
    if operation.state == OperationState::Resolved {
        return Ok(operation);
    }
    let attempt = store.external_operation_attempt(operation_id)?;
    if !matches!(operation.state, OperationState::InFlight | OperationState::Reconciling)
        || !matches!(attempt.dispatch_stage, AttemptDispatchStage::Armed | AttemptDispatchStage::Unknown)
        || operation.outcome_conflict
    {
        return Ok(operation);
    }
    let payload = serde_json::json!({
        "schema_version": 1,
        "operation_id": operation_id,
        "attempt_id": attempt.attempt_id,
        "observation": recovery_note,
        "effect_proven": false,
    });
    observe_record(
        store,
        &operation,
        &attempt,
        recovery_schema,
        &payload,
        None,
        EffectStatus::Unknown,
    )
}

/// Resolve the exact selected key locally and authenticate one session over
/// an owned TCP stream. `binding` is a handle: key bytes are read here and
/// never sealed into receipts.
pub async fn connect_session(
    store: &Store,
    binding: &SshBinding,
    deadline: Duration,
) -> Result<crate::native_transport::NativeSshSession> {
    let target = crate::native_transport::NativeSshTarget::from_binding(store, binding)?;
    let stream = tcp_connect(
        binding.target.host.as_str(),
        binding.target.port,
        deadline,
    )
    .await?;
    let session = crate::native_transport::NativeSshConnection::connect_stream(target, stream, deadline)
        .await
        .map_err(|error| anyhow::anyhow!("native SSH connect failed: {error:?}"))?;
    let key_path = binding
        .target
        .key_path
        .clone()
        .context("native SSH requires an explicit key_path binding")?;
    let key = load_private_key(&key_path)?;
    session
        .authenticate_private_key(key)
        .await
        .map_err(|error| anyhow::anyhow!("native SSH authentication failed: {error:?}"))
}

pub async fn tcp_connect(host: &str, port: u16, deadline: Duration) -> Result<TcpStream> {
    use tokio::net::ToSocketAddrs;
    let connect = async {
        let mut last_error = None;
        let addresses = (host, port)
            .to_socket_addrs()
            .await
            .with_context(|| format!("resolving {host}:{port} failed"))?;
        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address")))
    };
    tokio::time::timeout(deadline, connect)
        .await
        .context("native SSH TCP deadline elapsed")?
        .context("native SSH TCP connect failed")
}

/// Load one exact private key, refusing encrypted keys (no passphrase
/// channel exists in this contract) and refusing RSA without a SHA-2 hash.
pub fn load_private_key(path: &std::path::Path) -> Result<PrivateKeyWithHashAlg> {
    let bytes = std::fs::read(path).with_context(|| format!("reading key {}", path.display()))?;
    let key = russh::keys::PrivateKey::from_openssh(&bytes)
        .with_context(|| format!("parsing key {}", path.display()))?;
    ensure!(
        !key.is_encrypted(),
        "encrypted private keys are not admitted (no passphrase channel)"
    );
    let hash = if key.algorithm().is_rsa() {
        Some(russh::keys::HashAlg::Sha512)
    } else {
        None
    };
    Ok(PrivateKeyWithHashAlg::new(Arc::new(key), hash))
}
