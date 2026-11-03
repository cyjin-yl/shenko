//! Owned synthetic task/keys/server replies only. Real Store T1/E1 and russh
//! packets over 1 KiB memory streams; no remote helper process or policy grant.
use super::*;
use crate::native_transport::{NativeInterruption, NativeSshConnection, NativeSshTarget};
use russh::{Channel, ChannelId, ChannelMsg, keys::{PrivateKey, PrivateKeyWithHashAlg,
    ssh_key::private::Ed25519Keypair}, server};
use shenko_types::{store::WriteBatch, task::*};
use std::{path::PathBuf, sync::{Arc, Mutex}, time::Duration};
use tokio::{io::duplex, task::JoinHandle, time::timeout};

fn key(seed: u8) -> PrivateKey { Ed25519Keypair::from_seed(&[seed; 32]).into() }
fn limits() -> NativeCommandLimits {
    NativeCommandLimits { deadline: Duration::from_secs(5), shutdown_grace: Duration::from_secs(1),
        stdout_bytes: 128 * 1024, stderr_bytes: 1024 }
}
struct Fixture { root: PathBuf, store: Store, binding: SshBinding, operation_id: EntityId }
fn fixture() -> Fixture {
    let root = std::env::temp_dir().join(format!("shenko-native-query-{}", EntityId::new()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(root.join("ledger.db"), root.join("cas")).unwrap();
    // Both endpoint labels deliberately share one public fixture key: changing
    // a target must not pass merely because peer-key authentication succeeded.
    let pin = key(71).public_key().to_openssh().unwrap();
    let trust = store.cas.seal_object(format!("fixture.invalid,other.invalid {pin}\n").as_bytes(),
        "text/plain", "ssh-public-trust", "openssh-known-hosts", 1).unwrap().object;
    let binding = SshBinding { target_id: "query-fixture".into(), target: SshTarget {
        host: "fixture.invalid".into(), user: "fixture".into(), port: 22,
        key_path: Some(root.join("never-created-or-read-key")), ..SshTarget::default()
    }, receipt_root: "/owned-fixture-receipts".into(), known_hosts: trust };
    let ctx = TaskInvocation { task_id: EntityId::new(), task_revision: TaskRevision(1),
        record_revision: RecordRevision(1), invocation_id: EntityId::new(), epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::new("query-fixture"),
        worker_id: store.capabilities.incarnation, node_id: store.ledger.ledger_id() };
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let runner = SshTaskRunner { store: &store };
    let frozen = FrozenInput { binding: binding.clone(),
        action: SshAction::exec(vec!["NEVER-DISPATCH-OR-REPLAY".into()], b"private-owned-fixture-input"),
        selection: None, installation: None };
    let input = runner.seal(&frozen, "ssh-frozen-input").unwrap();
    let conversation_id = EntityId::new();
    let task = TaskRecord { record_schema_version: 1, task_id: ctx.task_id,
        task_revision: ctx.task_revision, record_revision: ctx.record_revision,
        agent_id: EntityId::new(), conversation_id, owner: TaskOwner::Conversation { id: conversation_id },
        background: false, kind: "synthetic.native-query-fixture".into(), definition_version: 1,
        implementation_ref: ImplementationRef { plugin_id: EntityId::nil(),
            artifact_digest: Digest::blake3(b"owned-query-fixture"), registry_revision: 1 },
        input_ref: input.clone(), state: TaskState::Running { checkpoint_ref: None,
            invocation_id: ctx.invocation_id, mode: RunMode::Run, epoch: ctx.epoch },
        abort_request: None, memos: Default::default(), residual_case_ids: vec![],
        created_by_step_id: None, dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0, first_run_at: None, terminal_at: None,
        created_commit: zero, updated_commit: zero };
    let mut batch = WriteBatch::new(); batch.task_upserts.push(task); store.commit(batch).unwrap();
    let operation_id = EntityId::new();
    let digest = Digest::blake3(HELPER.as_bytes());
    let version = VersionRef { artifact: ArtifactRef { digest: digest.clone(),
        locator: "builtin:openssh-python3-v1".into() }, symbol: "receipt".into(),
        schema: SchemaRef { name: "ssh-receipt-v1".into(), major: 1, digest: digest.clone() } };
    let mut op = OperationRecord { record_schema_version: 1, operation_id,
        record_revision: RecordRevision(0), task_id: ctx.task_id, logical_key: "owned-query".into(),
        operation_name: "ssh.native.v1".into(), input_schema_version: 1, output_schema_version: 1,
        final_input_ref: input.clone(), input_digest: input.digest.clone(),
        contract_ref: OperationContractRef { plugin_id: EntityId::nil(), artifact_digest: digest.clone(),
            contract_digest: digest, receipt_reader_ref: version.clone() },
        resource_bindings_ref: runner.seal(&binding, "ssh-binding").unwrap(),
        authority_ref: AuthorityRef { delegation_id: ctx.task_id,
            delegation_revision: RecordRevision(1), decision_ref: EntityId::new() },
        acceptance_predicate_ref: runner.seal(&"owned fixture", "ssh-acceptance").unwrap(),
        retry_contract_ref: runner.seal(&"query only", "ssh-retry").unwrap(),
        checkpoint_scope_ref: None, state: OperationState::InFlight, outcome: None,
        outcome_conflict: false, effect_status: EffectStatus::Unknown, retry_of: None,
        compensates: None, possible_duplicate_of: vec![], prepared_commit: zero, evidence_through: None };
    let mut att = AttemptRecord { record_schema_version: 1, attempt_id: EntityId::new(), operation_id,
        attempt_no: 1, invocation_id: ctx.invocation_id, worker_id: ctx.worker_id, node_id: ctx.node_id,
        epoch: ctx.epoch, implementation_ref: version, dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: zero, provider_handle: None, provider_idempotency: None, transport_ref: None,
        started_at: None, last_observed_at: None, final_receipt_id: None };
    assert!(store.arm_external_operation(&ctx, &mut op, &mut att).unwrap().is_none());
    Fixture { root, store, binding, operation_id }
}

#[derive(Clone, Copy)]
enum ReplyMode { Complete, CompleteNoEof, Silent, Overflow, Partial, WrongOperation, WrongSeal }

struct Peer {
    expected_command: Vec<u8>, reply: Option<(Vec<u8>, bool)>, calls: Arc<Mutex<Vec<Vec<u8>>>>,
    channel: Option<Channel<server::Msg>>, workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl server::Handler for Peer {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, account: &str, verified: server::VerifiedPublicKey<'_>)
        -> Result<server::Auth, Self::Error> {
        Ok(if account == "fixture" && verified.public_key.key_data() == key(72).public_key().key_data() {
            server::Auth::Accept
        } else { server::Auth::reject() })
    }
    async fn channel_open_session(&mut self, channel: Channel<server::Msg>,
        reply: server::ChannelOpenHandle, _: &mut server::Session) -> Result<(), Self::Error> {
        self.channel = Some(channel); reply.accept().await; Ok(())
    }
    async fn exec_request(&mut self, id: ChannelId, command: &[u8], session: &mut server::Session)
        -> Result<(), Self::Error> {
        assert_eq!(command, self.expected_command);
        self.calls.lock().unwrap().push(command.to_vec());
        session.channel_success(id)?;
        let channel = self.channel.take().unwrap(); let reply = self.reply.take();
        let worker = tokio::spawn(async move {
            let (mut read, write) = channel.split();
            let mut input = Vec::new(); let mut eof = false;
            while let Some(msg) = read.wait().await {
                match msg { ChannelMsg::Data { data } => input.extend_from_slice(&data),
                    ChannelMsg::Eof => { eof = true; break; },
                    ChannelMsg::Close => panic!("query closed before input EOF"), _ => {} }
            }
            assert!(eof); assert!(input.is_empty(), "query must never replay original action bytes");
            if let Some((reply, send_eof)) = reply {
                write.data(&reply[..]).await.unwrap(); write.exit_status(0).await.unwrap();
                if send_eof { write.eof().await.unwrap(); }
                write.close().await.unwrap();
            } else {
                // Retain the write half until the client's original deadline
                // closes the session. No sleep or changed protocol timeout.
                while read.wait().await.is_some() {}
                drop(write);
            }
        });
        self.workers.lock().unwrap().push(worker); Ok(())
    }
}
struct Running { server: JoinHandle<Result<(), russh::Error>>,
    calls: Arc<Mutex<Vec<Vec<u8>>>>, workers: Arc<Mutex<Vec<JoinHandle<()>>>> }
impl Running {
    async fn finish(self, expected_calls: usize) {
        let _ = timeout(Duration::from_secs(2), self.server).await.unwrap().unwrap();
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers { timeout(Duration::from_secs(2), worker).await.unwrap().unwrap(); }
        assert_eq!(self.calls.lock().unwrap().len(), expected_calls);
    }
}
fn completed_receipt(op: &OperationRecord) -> SshReceipt {
    SshReceipt { operation_id: op.operation_id, schema: Some(1),
        seal: op.input_digest.0.clone(), state: "completed".into(), exit_code: Some(0),
        stdout_b64: String::new(), stderr_b64: String::new(), stdout_truncated: false,
        stderr_truncated: false, sha256: None }
}
async fn session(store: &Store, binding: &SshBinding, op: &OperationRecord, mode: ReplyMode)
    -> (NativeSshSession, Running) {
    let mut payload = completed_receipt(op);
    if matches!(mode, ReplyMode::WrongOperation) { payload.operation_id = EntityId::new(); }
    if matches!(mode, ReplyMode::WrongSeal) { payload.seal = Digest::blake3(b"different-input").0; }
    let mut bytes = serde_json::to_vec(&payload).unwrap();
    if matches!(mode, ReplyMode::Overflow) { bytes.extend_from_slice(b"UNRETAINED-EXTRA-BYTES"); }
    if matches!(mode, ReplyMode::Partial) { assert_eq!(bytes.pop(), Some(b'}')); }
    let reply = (!matches!(mode, ReplyMode::Silent))
        .then_some((bytes, !matches!(mode, ReplyMode::CompleteNoEof | ReplyMode::Partial)));
    let calls = Arc::new(Mutex::new(vec![])); let workers = Arc::new(Mutex::new(vec![]));
    let peer = Peer { expected_command: SshTaskRunner::helper_command(op, binding, "query").into_bytes(),
        reply, calls: calls.clone(),
        workers: workers.clone(), channel: None };
    let config = Arc::new(server::Config { keys: vec![key(71)], window_size: 64 * 1024,
        maximum_packet_size: 32 * 1024, channel_buffer_size: 4, event_buffer_size: 4,
        auth_rejection_time: Duration::ZERO, auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(10)), ..Default::default() });
    let (client_io, server_io) = duplex(1024);
    let server = tokio::spawn(async move { server::run_stream(config, server_io, peer).await?.await });
    let target = NativeSshTarget::from_binding(store, binding).unwrap();
    let connection = NativeSshConnection::connect_stream(target, client_io, Duration::from_secs(5)).await.unwrap();
    let session = connection.authenticate_private_key(PrivateKeyWithHashAlg::new(Arc::new(key(72)), None)).await.unwrap();
    (session, Running { server, calls, workers })
}

#[tokio::test]
async fn native_query_records_receipt_on_original_operation_without_dispatch() {
    let f = fixture(); let before = f.store.external_operation(f.operation_id).unwrap();
    let attempt = f.store.external_operation_attempt(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &before, ReplyMode::Complete).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.operation_id, before.operation_id);
    assert_eq!(result.operation.input_digest, before.input_digest);
    assert_eq!(result.operation.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(result.operation.effect_status, EffectStatus::Unknown); // arbitrary exec effects not proven
    assert!(result.receipt.is_some() && result.transport.is_none());
    assert_eq!(result.native_transport.unwrap().exit, Some(NativeExit::Status { code: 0 }));
    assert_eq!(f.store.external_operation_attempt(f.operation_id).unwrap().attempt_id, attempt.attempt_id);
    assert_eq!(f.store.external_operations_after("ssh.native.v1", None, 10).unwrap().len(), 1);
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_rejects_other_endpoint_even_with_same_host_key_and_account() {
    let f = fixture(); let before = f.store.external_operation(f.operation_id).unwrap();
    let mut other = f.binding.clone(); other.target.host = "other.invalid".into();
    let (native, running) = session(&f.store, &other, &before, ReplyMode::Complete).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await;
    assert!(result.is_err());
    assert_eq!(serde_json::to_value(f.store.external_operation(f.operation_id).unwrap()).unwrap(),
        serde_json::to_value(&before).unwrap());
    running.finish(0).await;
}

#[tokio::test]
async fn native_query_deadline_survives_reopen_and_requeries_original_without_replay() {
    let f = fixture(); let before = f.store.external_operation(f.operation_id).unwrap();
    let attempt = f.store.external_operation_attempt(f.operation_id).unwrap().attempt_id;
    let (native, running) = session(&f.store, &f.binding, &before, ReplyMode::Silent).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native,
        NativeCommandLimits { deadline: Duration::from_millis(100), ..limits() }).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    assert_eq!(result.operation.effect_status, EffectStatus::Unknown);
    assert_eq!(result.native_transport.unwrap().interruption, Some(NativeInterruption::Deadline));
    running.finish(1).await; drop(f.store);
    let reopened = Store::open(f.root.join("ledger.db"), f.root.join("cas")).unwrap();
    let original = reopened.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&reopened, &f.binding, &original, ReplyMode::Complete).await;
    let result = SshTaskRunner { store: &reopened }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(result.operation.input_digest, before.input_digest);
    assert_eq!(reopened.external_operation_attempt(f.operation_id).unwrap().attempt_id, attempt);
    assert_eq!(reopened.external_operations_after("ssh.native.v1", None, 10).unwrap().len(), 1);
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_accepts_complete_receipt_on_close_without_eof() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::CompleteNoEof).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.outcome, Some(OperationOutcome::Succeeded));
    assert!(result.receipt.is_some());
    let transport = result.native_transport.unwrap();
    assert!(transport.channel_closed); assert_eq!(transport.interruption, None);
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_rejects_valid_json_prefix_after_transport_output_overflow() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let retained = serde_json::to_vec(&completed_receipt(&op)).unwrap().len();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::Overflow).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native,
        NativeCommandLimits { stdout_bytes: retained, ..limits() }).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    assert_eq!(result.operation.effect_status, EffectStatus::Unknown);
    let transport = result.native_transport.unwrap();
    assert!(serde_json::from_slice::<SshReceipt>(&transport.stdout).is_ok());
    assert_eq!(transport.interruption, Some(NativeInterruption::OutputLimit));
    assert_eq!(transport.exit, Some(NativeExit::Status { code: 0 }));
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_rejects_partial_json_despite_exit_zero_and_clean_close() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::Partial).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    let transport = result.native_transport.unwrap();
    assert!(transport.exec_accepted && transport.channel_closed);
    assert_eq!(transport.interruption, None);
    assert_eq!(transport.exit, Some(NativeExit::Status { code: 0 }));
    assert!(serde_json::from_slice::<SshReceipt>(&transport.stdout).is_err());
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_rejects_receipt_for_another_operation() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::WrongOperation).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    let transport = result.native_transport.unwrap();
    assert_eq!(transport.interruption, None);
    let received: SshReceipt = serde_json::from_slice(&transport.stdout).unwrap();
    assert_ne!(received.operation_id, op.operation_id); assert_eq!(received.seal, op.input_digest.0);
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_rejects_receipt_for_another_input_seal() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::WrongSeal).await;
    let result = SshTaskRunner { store: &f.store }.query_native(f.operation_id, native, limits()).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    let transport = result.native_transport.unwrap();
    assert_eq!(transport.interruption, None);
    let received: SshReceipt = serde_json::from_slice(&transport.stdout).unwrap();
    assert_eq!(received.operation_id, op.operation_id); assert_ne!(received.seal, op.input_digest.0);
    running.finish(1).await;
}

#[tokio::test]
async fn native_query_error_keeps_native_codec_without_changing_legacy_encoding() {
    let f = fixture(); let op = f.store.external_operation(f.operation_id).unwrap();
    let (native, running) = session(&f.store, &f.binding, &op, ReplyMode::Complete).await;
    let runner = SshTaskRunner { store: &f.store };
    let result = runner.query_native(f.operation_id, native,
        NativeCommandLimits { deadline: Duration::ZERO, ..limits() }).await.unwrap();
    assert_eq!(result.operation.outcome, None); assert!(result.receipt.is_none());
    assert!(result.transport.is_none() && result.native_transport.is_none());
    let evidence = f.store.external_operation_evidence(f.operation_id).unwrap();
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].raw_ref.codec.name, "ssh-native-observation-v1");
    let native_json: serde_json::Value = serde_json::from_slice(
        &f.store.cas.read_object(&evidence[0].raw_ref.digest).unwrap()).unwrap();
    assert_eq!(native_json, serde_json::json!([null, null, "invalid native SSH limits"]));
    running.finish(0).await;
    // One codec migration journey on the same original operation. No old
    // typed SshObservation deserializer exists (the API is Serialize-only).
    // The historical tuple and public JSON shape must remain byte/schema-safe.
    let (op, att, frozen) = runner.query_input(f.operation_id).unwrap();
    let legacy = runner.record_observation(op, att, frozen,
        ObservedTransport::OpenSsh(Err("owned fixture error".into()))).unwrap();
    let old_shape = serde_json::json!({"operation": &legacy.operation, "receipt": null,
        "transport": null, "diagnostic": "owned fixture error"});
    assert_eq!(serde_json::to_value(&legacy).unwrap(), old_shape);
    let evidence = f.store.external_operation_evidence(f.operation_id).unwrap();
    assert_eq!(evidence.len(), 2); assert_eq!(evidence[1].raw_ref.codec.name, "ssh-observation");
    let legacy_json: serde_json::Value = serde_json::from_slice(
        &f.store.cas.read_object(&evidence[1].raw_ref.digest).unwrap()).unwrap();
    assert_eq!(legacy_json, serde_json::json!([null, null, "owned fixture error"]));
}
