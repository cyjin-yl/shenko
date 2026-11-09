//! R018 real-SSH proof: drive the installed `ssh.exec` native handler
//! end-to-end against a live target — real Store genesis + real
//! claim/authority wiring, real CAS-sealed known_hosts trust, real TCP
//! connect, real publickey authentication with a node ed25519 identity
//! produced by `ensure_node_identity`, and a real remote command whose
//! exit status comes back on the operation's observation evidence.
//!
//! Gated; skipped unless all three are provisioned:
//!
//!   SHENKO_REAL_SSH_TARGET      `user@host[:port]` of the live target
//!   SHENKO_REAL_SSH_KNOWN_HOSTS known_hosts file pinning that exact host
//!   SHENKO_REAL_SSH_IDENTITY    directory for the durable node identity;
//!                               its `.pub` must already be installed in
//!                               the target account's authorized_keys
//!
//! The test never shells out to OpenSSH and never trusts an on-disk
//! known_hosts file at connect time: `SshBinding::register` seals the
//! exact trust bytes into CAS and the handler re-reads them from CAS.
//! The private key file is a handle — it is opened locally by the
//! installed connect path and its bytes never enter a receipt.

#![cfg(unix)]

use anyhow::{Context, Result};
use shenko_exec_ssh::{SshBinding, SshExecHandler, SshTarget, ensure_node_identity};
use shenko_store::{Store, child::NATIVE_INPUT};
use shenko_subject::{NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    agent::CallContext, commit::CommitRef, ids::*, operation::*,
    store::WriteBatch, subject::*, task::*,
};
use std::path::PathBuf;
use std::sync::Arc;

fn parse_target(value: &str) -> Option<(String, u16, String)> {
    let (user, rest) = value.split_once('@')?;
    let (host, port) = match rest.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (rest, 22),
    };
    if user.is_empty() || host.is_empty() {
        return None;
    }
    Some((user.to_string(), port, host.to_string()))
}

/// A real subject: genesis, branch head, first step and root task are
/// committed in the real genesis transaction, and the subject dispatcher
/// is started so `claim_native_task` admits a real claim. Returns the
/// store plus the durable step id native children must name as their
/// `created_by_step_id` origin.
fn open_subject_fixture() -> Result<(PathBuf, Arc<Store>, EntityId, EntityId)> {
    let root = std::env::temp_dir().join(format!("shenko-real-ssh-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(root.join("ledger.db"), root.join("cas"))?);
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let agent = EntityId::new();
    let branch = BranchId::new();
    let genesis_id = EntityId::new();
    let seed = store
        .cas
        .seal_object(b"r018 fixture seed", "text/plain", "fixture", "raw", 1)?
        .object;
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: branch.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::new("real-ssh-exec-fixture"),
        genesis_ref: GenesisRef { ledger_id: store.ledger.ledger_id(), genesis_id },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: EntityId::new(),
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: seed.clone(),
        initial_profile_ref: seed.clone(),
        home_manifest_ref: seed.clone(),
        bootstrap_event_ref: seed.clone(),
        created_commit: zero,
    };
    let step = SubjectStep {
        step_id: head.next_step_id,
        branch_id: branch.clone(),
        step_no: 1,
        expected_parent_commit: zero,
        controller_epoch: head.controller_epoch,
        software_generation: head.software_generation.clone(),
        state: SubjectStepState::Planned,
        input_plan_ref: None,
        context_revision: None,
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        model_outcome_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };
    let root_task = TaskRecord::subject_root(
        head.subject_root_task_id,
        agent,
        seed.clone(),
        zero,
    );
    store.initialize_genesis(&genesis, &head, &step, &root_task)?;
    store.start_subject_dispatcher()?;
    Ok((root, store, agent, step.step_id))
}

/// Commit one Pending `ssh.exec` task owned by its conversation with the
/// real native-input memo, exactly the shape the dispatcher claims.
fn commit_exec_task(
    store: &Store,
    implementation: ImplementationRef,
    agent: EntityId,
    step_id: EntityId,
) -> Result<EntityId> {
    let task_id = EntityId::new();
    let conversation_id = EntityId::new();
    let input = store
        .cas
        .seal_object(b"r018 native exec task", "text/plain", "fixture", "raw", 1)?
        .object;
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let mut memos = serde_json::Map::new();
    memos.insert(NATIVE_INPUT.into(), serde_json::json!({"fixture": "real-ssh-exec"}));
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(1),
        record_revision: RecordRevision(1),
        agent_id: agent,
        conversation_id,
        owner: TaskOwner::Conversation { id: conversation_id },
        background: false,
        kind: "ssh.exec".into(),
        definition_version: 1,
        implementation_ref: implementation,
        input_ref: input,
        state: TaskState::Pending { checkpoint_ref: None },
        abort_request: None,
        memos,
        residual_case_ids: vec![],
        created_by_step_id: Some(step_id),
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task);
    store.commit(batch)?;
    Ok(task_id)
}

#[tokio::test]
async fn real_ssh_exec_reports_remote_exit_and_stdout() -> Result<()> {
    let Some((user, port, host)) = std::env::var("SHENKO_REAL_SSH_TARGET")
        .ok()
        .and_then(|value| parse_target(&value))
    else {
        eprintln!("SKIP: SHENKO_REAL_SSH_TARGET=user@host[:port] not set");
        return Ok(());
    };
    let Some(known_hosts) = std::env::var_os("SHENKO_REAL_SSH_KNOWN_HOSTS").map(PathBuf::from)
    else {
        eprintln!("SKIP: SHENKO_REAL_SSH_KNOWN_HOSTS=<known_hosts pinning the host> not set");
        return Ok(());
    };
    let Some(identity_dir) = std::env::var_os("SHENKO_REAL_SSH_IDENTITY").map(PathBuf::from)
    else {
        eprintln!("SKIP: SHENKO_REAL_SSH_IDENTITY=<durable node identity dir> not set");
        return Ok(());
    };

    // The same durable node identity the bin provisions (R016/R017):
    // reopened if intact, generated only when absent.
    let identity = ensure_node_identity(&identity_dir, "shenko-r018-node")?;
    eprintln!(
        "node identity: {} {} (private key at {})",
        identity.algorithm, identity.fingerprint_sha256, identity.private_key_file.display()
    );
    eprintln!("install into authorized_keys if not present: {}", identity.public_key_openssh);

    let (_root, store, agent, step_id) = open_subject_fixture()?;
    let handler = SshExecHandler::new(Arc::clone(&store));
    let descriptor = handler.descriptor();
    let task_id = commit_exec_task(
        &store,
        descriptor.implementation_ref.clone(),
        agent,
        step_id,
    )?;

    // Register the real binding: the supplied known_hosts bytes are sealed
    // into CAS here and re-verified from CAS at connect time.
    let binding = SshBinding::register(
        &store,
        format!("r018-{host}"),
        SshTarget {
            host: host.clone(),
            port,
            user: user.clone(),
            key_path: Some(identity.private_key_file.canonicalize()?),
            password: None,
            known_hosts_path: Some(known_hosts),
            use_agent: false,
        },
        "/tmp/shenko-r018-receipts".into(),
    )?;
    eprintln!("registered trust digest: {}", binding.known_hosts.digest.0);

    // Real claim through the dispatcher path; the invocation mirrors
    // NativeChildService::process exactly.
    let claim = store
        .claim_native_task(task_id, descriptor.implementation_ref.clone(), false)?
        .context("pending ssh.exec task must be claimable")?;
    let invocation = NativeTaskInvocation {
        child_tools: None,
        context: CallContext {
            caller_instance_id: store.capabilities.incarnation,
            role_revision: descriptor.role_revision.clone(),
            task_id: Some(task_id),
            expected_task_revision: Some(claim.invocation.task_revision.0),
            invocation_id: Some(claim.invocation.invocation_id),
            controller_epoch: claim.invocation.epoch,
            software_generation: claim.software_generation.clone(),
            authority_refs: descriptor.authority_refs.clone(),
            trace_id: EntityId::new(),
        },
        task_invocation: claim.invocation.clone(),
        task: claim.task.clone(),
        operation_id: claim.operation_id,
        authority: AuthorityRef {
            delegation_id: task_id,
            delegation_revision: RecordRevision(claim.invocation.task_revision.0),
            decision_ref: claim
                .task
                .created_by_step_id
                .context("exec task must carry its subject step origin")?,
        },
        input: serde_json::json!({
            "binding": binding,
            "command": "echo shenko-r018-ok",
        }),
    };

    let operation = handler.execute(invocation).await?;

    // The observation evidence is the only witness; read it back from CAS.
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    let receipt = store.external_evidence(
        attempt.final_receipt_id.context("executed operation must carry final evidence")?,
    )?;
    let payload: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&receipt.raw_ref.digest)?)?;
    eprintln!("observation payload: {}", serde_json::to_string_pretty(&payload)?);
    assert_eq!(operation.state, OperationState::Resolved);
    assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(operation.effect_status, EffectStatus::Complete);
    assert_eq!(payload["status"], "executed");
    let observation = &payload["observation"];
    assert_eq!(observation["exec_request_queued"], true);
    assert_eq!(observation["exec_accepted"], true);
    assert_eq!(observation["channel_closed"], true);
    assert_eq!(observation["exit"]["kind"], "status");
    assert_eq!(observation["exit"]["code"], 0);
    assert!(observation["interruption"].is_null());
    let stdout: Vec<u8> = serde_json::from_value(observation["stdout"].clone())?;
    let stdout = String::from_utf8(stdout)?;
    eprintln!("remote stdout: {stdout:?}");
    assert_eq!(stdout.trim_end(), "shenko-r018-ok");

    // Query-only recovery on the resolved operation returns the same
    // original outcome; it never re-executes.
    let reconciled = handler.reconcile(&claim.task, operation.operation_id).await?;
    assert_eq!(reconciled.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(reconciled.operation_id, operation.operation_id);
    Ok(())
}
