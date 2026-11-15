//! Store-first branch cutover with recoverable Node identity projection.
use std::path::Path;

use anyhow::{Context, Result, ensure};
use shenko_node::{Identity, Node, SubjectBranchProjection};
use shenko_store::Store;
use shenko_types::{ids::{BranchId, EntityId}, object::Digest};

/// Repair only an exact, locally committed parent→child fork. Importing a peer
/// head or changing the genesis is never sufficient to authorize this repair.
pub fn repair_pending_projection(data_dir: &Path, store: &Store) -> Result<()> {
    let Some(binding) = store.installation_binding()?.subject else { return Ok(()); };
    let (head, _) = store.load_subject(binding.agent_id)?;
    let Some(origin) = &head.branch_origin else { return Ok(()); };
    let node = Node::open(&data_dir.join("node"))?;
    let Some(old) = node.local_identity()? else { return Ok(()); };
    let target = Identity { agent_id: binding.agent_id, branch_id: binding.branch_id,
        origin_proof: binding.origin_proof };
    if old == target { return Ok(()); }
    let guard = node.reserve_authority()?;
    // Re-read after the Node reservation in Node → Store lock order.
    let (current, _) = store.load_subject(binding.agent_id)?;
    ensure!(current.branch_id == head.branch_id && current.branch_origin == head.branch_origin,
        "subject branch changed during projection recovery");
    let fork = store.subject_branch_fork(head.branch_id.clone())?.context("fork receipt row missing")?;
    let bytes = store.cas.read_object(&origin.digest)?;
    ensure!(bytes.len() as u64 == origin.bytes && Digest::blake3(&bytes) == origin.digest
        && serde_json::from_slice::<serde_json::Value>(&bytes)? == serde_json::to_value(&fork)?
        && fork.created_commit.ledger_id == store.ledger.ledger_id()
        && fork.agent_id == old.agent_id && old.agent_id == target.agent_id
        && old.origin_proof == target.origin_proof
        && EntityId(fork.parent_head.branch_id.0) == old.branch_id
        && EntityId(fork.branch_id.0) == target.branch_id,
        "Node/Store mismatch has no exact local fork receipt");
    guard.project_subject_branch(&SubjectBranchProjection {
        parent: old, branch_id: target.branch_id, store_commit: fork.created_commit,
        fork_receipt: origin.clone(),
    })?;
    Ok(())
}

/// Materialize the branch selected by the failed quorum round before any loop
/// state/context registry is constructed. Existing unknown work stays parented
/// to its original branch. Authority activation is a separate, required step.
pub fn fork_availability_branch(data_dir: &Path, store: &Store, branch_id: EntityId, reason: &str) -> Result<()> {
    repair_pending_projection(data_dir, store)?;
    let node = crate::node_sync::open_node(data_dir, store)?;
    let guard = node.reserve_authority()?;
    let identity = guard.snapshot().identity.clone();
    let (head, _) = store.load_subject(identity.agent_id)?;
    ensure!(head.branch_id.0 == identity.branch_id.0, "Node/Store branch differs before fork");
    if identity.branch_id == branch_id { return Ok(()); }
    let fork = store.fork_subject_branch(&head, BranchId(branch_id.0), reason)?;
    let (head, _) = store.load_subject(identity.agent_id)?;
    guard.project_subject_branch(&SubjectBranchProjection {
        parent: identity, branch_id, store_commit: fork.created_commit,
        fork_receipt: head.branch_origin.context("new branch has no origin receipt")?,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_node::{Request, ResultReceipt, Ticket};
    use shenko_store::{CognitionImportOptions, RuntimeErrorContext, SubjectRequestAttempt, SubjectRequestState, SubjectRequestTerminal};
    use shenko_types::{commit::CommitRef, ids::{ControllerEpoch, LedgerSeq, RecordRevision, RuntimeGeneration},
        subject::{GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState}, task::TaskRecord};

    struct Root(std::path::PathBuf);
    impl Root {
        fn new() -> Result<Self> {
            let path = std::env::temp_dir().join(format!("shenko-branch-{}", EntityId::new()));
            std::fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
        fn store(&self, name: &str) -> Result<Store> {
            Store::open(self.0.join(format!("{name}.db")), self.0.join(format!("{name}-cas")))
        }
    }
    impl Drop for Root {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    fn genesis(store: &Store) -> Result<EntityId> {
        let agent_id = EntityId::new();
        let origin = store.cas.seal_object(b"{}", "application/json", "test", "raw", 1)?.object;
        let commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let genesis = SubjectGenesis { genesis_id: EntityId::new(), subject_id: agent_id,
            bootstrap_id: EntityId::new(), launch_id: EntityId::new(), seed_manifest_ref: origin.clone(),
            initial_profile_ref: origin.clone(), home_manifest_ref: origin.clone(),
            bootstrap_event_ref: origin.clone(), created_commit: commit };
        let head = SubjectBranchHead { branch_origin: None, agent_id, branch_id: BranchId::new(),
            subject_root_task_id: EntityId::new(), controller_epoch: ControllerEpoch(1),
            software_generation: RuntimeGeneration::default(), genesis_ref: GenesisRef {
                ledger_id: store.ledger.ledger_id(), genesis_id: genesis.genesis_id },
            committed_step_no: 0, committed_step_id: None, committed_at: commit,
            next_step_id: EntityId::new(), mode: SubjectMode::Active, mode_reason_ref: None,
            record_revision: RecordRevision(0) };
        let step = SubjectStep { step_id: head.next_step_id, branch_id: head.branch_id.clone(), step_no: 1,
            expected_parent_commit: commit, controller_epoch: head.controller_epoch,
            software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
            input_plan_ref: None, context_revision: None, active_request_id: None, active_attempt_id: None,
            model_result_ref: None, model_outcome_ref: None, decision_ref: None, commit_ref: None,
            retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None };
        store.initialize_genesis(&genesis, &head, &step,
            &TaskRecord::subject_root(head.subject_root_task_id, agent_id, origin, commit))?;
        Ok(agent_id)
    }

    fn event(store: &Store, agent: EntityId, message: &str) -> Result<shenko_store::StoredSessionEvent> {
        Ok(store.record_runtime_error(agent, RuntimeErrorContext {
            kind: "branch-fixture".into(), source_module: module_path!().into(), message: message.into(),
            ..Default::default()
        })?)
    }

    fn archive(store: &Store) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        store.write_cognition_archive(&Default::default(), None, &mut bytes)?;
        Ok(bytes)
    }

    fn stage_web_input(store: &Store, head: &SubjectBranchHead, step: &SubjectStep)
        -> Result<(shenko_types::inbox::InboxEvent, shenko_store::StoredSessionEvent)>
    {
        let original = shenko_types::inbox::InboxEvent {
            event_id: EntityId::new(), kind: shenko_types::inbox::InboxEventKind::Message,
            payload: serde_json::json!({"text":"retain this unresolved input"}),
            created_at: chrono::Utc::now(), source: shenko_store::WEB_INPUT_SOURCE.into(),
        };
        store.admit_web_submission(head.agent_id, "branch-input", &original.payload.to_string(),
            &shenko_types::store::RawInboxEvent {
                event_id: original.event_id, agent_id: head.agent_id, kind: "message".into(),
                source: original.source.clone(), payload_json: serde_json::to_string(&original)?,
            })?;
        assert_eq!(store.pending_inbox_events(head.agent_id, 8)?.len(), 1);
        let staged = store.stage_subject_inputs(head, step, &[shenko_store::SubjectHistoryInput {
            event_id: original.event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1,"text":"retain this unresolved input","inbox_event":original}),
        }])?;
        assert!(store.pending_inbox_events(head.agent_id, 8)?.is_empty());
        Ok((original, staged[0].clone()))
    }

    #[test]
    fn fork_recovers_between_stores_without_replaying_original_attempts() -> Result<()> {
        let root = Root::new()?;
        let store = std::sync::Arc::new(root.store("subject")?);
        let agent = genesis(&store)?;
        let (parent, mut parent_step) = store.load_subject(agent)?;
        let raw = event(&store, agent, "parent's retained observation")?;
        let (original, staged) = stage_web_input(&store, &parent, &parent_step)?;
        let plan = crate::recovery_integration_tests::prepare_fixture_s0(&store, &parent, &parent_step);
        parent_step.input_plan_ref = Some(plan.clone());
        parent_step.state = SubjectStepState::Preparing;
        parent_step.record_revision = RecordRevision(1);
        store.upsert_step(&parent_step, &parent, RecordRevision(0))?;
        let request = SubjectRequestAttempt { schema_version: 1, request_id: EntityId::new(),
            attempt_id: EntityId::new(), agent_id: agent, step_id: parent_step.step_id, send_id: None,
            state: SubjectRequestState::Armed, input_plan_ref: plan, payload_digest: Digest::blake3(b"frozen request"),
            provider_binding_digest: Digest::blake3(b"original provider binding"), terminal: None,
            created_commit: parent.committed_at };
        // Real S0 and atomic request ownership precede the durable send marker.
        // No model service is contacted: this proves journal/crash semantics.
        parent_step.state = SubjectStepState::AwaitingModel;
        parent_step.active_request_id = Some(request.request_id);
        parent_step.active_attempt_id = Some(request.attempt_id);
        parent_step.record_revision = RecordRevision(2);
        store.upsert_step_arming_request(&parent_step, &parent, RecordRevision(1), None, &request)?;
        store.mark_subject_request_dispatched(request.request_id)?;
        let predecessor = store.prepare_control_predecessor(&parent)?;
        let admission = shenko_types::authority::CommitAdmission {
            schema_version: 1, subject_id: agent, branch_id: parent.branch_id.clone(),
            conflict_key: "fixture/original-dispatch".into(), action_id: EntityId::new(),
            conflict_aliases: vec![], required_unconsumed: vec![],
            expected_parent: None, expected_head_revision: None,
            controller_epoch: parent.controller_epoch, software_generation: parent.software_generation.clone(),
            operation_id: None, attempt_id: Some(request.attempt_id), payload_ref: predecessor.clone(),
        };
        let replica = root.store("evidence-only-replica")?;
        for object in store.admission_objects(&predecessor)? {
            replica.cas.seal_object(&object.bytes, &object.reference.media_type,
                &object.reference.store_namespace, &object.reference.codec.name, object.reference.codec.major)?;
        }
        let evidence = replica.inspect_control_predecessor(&predecessor, &admission)?;
        assert!(!evidence.original_source);
        assert_eq!(evidence.source_ledger_id, store.ledger.ledger_id());
        assert!(replica.get_existing_agent_id()?.is_none());
        assert!(replica.subject_request_attempt(request.request_id)?.is_none());
        let mut node = crate::node_sync::open_node(&root.0, &store)?;
        let identity = node.local_identity()?.context("node identity")?;
        let node_id = node.local_node_id()?.context("node id")?;
        node.handle(Request::Join { operation_id: EntityId::new(), expected_node: node_id,
            identity: identity.clone(), manifest: shenko_node::running_manifest()? })?;
        node.handle(Request::Register { node_id, target_id: "local original".into(), binding_digest: Digest::blake3(b"fixture") })?;
        let task_id = EntityId::new();
        let created = node.handle(Request::Create { task_id, node_id, input: b"original input".to_vec() })?;
        let ticket: Ticket = serde_json::from_value(created["ticket"].clone())?;
        let executed = node.handle(Request::Execute { ticket: ticket.clone(), input: b"original input".to_vec() })?;
        let receipt: ResultReceipt = serde_json::from_value(executed["receipt"].clone())?;
        let branch = EntityId::new();
        node.open_availability_branch(branch, Some(agent), "quorum unavailable")?;
        let guard = node.reserve_authority()?;
        let fork = store.fork_subject_branch(&parent, BranchId(branch.0), "quorum unavailable")?;
        drop(guard); // Crash after durable Store publication, before Node projection.
        drop(store);

        let store = std::sync::Arc::new(root.store("subject")?);
        assert_eq!(Node::open(&root.0.join("node"))?.local_identity()?, Some(identity));
        repair_pending_projection(&root.0, &store)?;
        let mut node = crate::node_sync::open_node(&root.0, &store)?;
        assert_eq!(node.local_identity()?.unwrap().branch_id, branch);
        repair_pending_projection(&root.0, &store)?; // Recovery is idempotent.
        let (head, mut step) = store.load_subject(agent)?;
        assert_eq!(head.branch_id, fork.branch_id);
        assert_eq!(head.genesis_ref, parent.genesis_ref);
        assert_ne!(head.subject_root_task_id, parent.subject_root_task_id);
        assert_eq!(step.state, SubjectStepState::Planned);
        assert!(step.active_request_id.is_none() && step.active_attempt_id.is_none());
        assert_eq!(store.stage_subject_inputs(&head, &step, &[])?, vec![staged.clone()]);
        assert_eq!(store.first_subject_input(agent, original.event_id)?, Some(staged.clone()));
        assert!(store.pending_inbox_events(agent, 8)?.is_empty());
        assert_eq!(store.web_input_selected_step(agent, original.event_id)?, None);
        assert_eq!(fork.inherited_inputs.len(), 1);
        assert_eq!(fork.inherited_inputs[0].source_step_id, parent_step.step_id);
        assert_eq!(fork.inherited_inputs[0].raw_event_id, staged.event_id);
        // Seal a new S0 containing the original selection. This is a new
        // branch decision input, not a replay of the parent's dispatched call.
        step.input_plan_ref = Some(crate::recovery_integration_tests::prepare_fixture_s0(&store, &head, &step));
        step.state = SubjectStepState::Preparing;
        step.record_revision = RecordRevision(1);
        store.upsert_step(&step, &head, RecordRevision(0))?;
        assert_eq!(store.web_input_selected_step(agent, original.event_id)?, Some(step.step_id));
        let retained = store.subject_request_attempt(request.request_id)?.unwrap();
        assert_eq!(retained.state, SubjectRequestState::Dispatched);
        assert_eq!(retained.attempt_id, request.attempt_id);
        assert_eq!(retained.step_id, parent_step.step_id);
        assert_eq!(store.fork_subject_branch(&parent, fork.branch_id.clone(), "quorum unavailable")?.created_commit, fork.created_commit);
        assert!(store.fork_subject_branch(&parent, fork.branch_id.clone(), "different facts").is_err());
        let child_raw = event(&store, agent, "child's observation")?;
        assert_eq!(child_raw.seq, staged.seq + 1);
        assert_eq!(store.branch_history_range(branch, 0, i64::MAX)?, vec![raw.clone(), staged, child_raw]);
        assert_eq!(store.branch_history_event(branch, raw.event_id)?, Some(raw));
        let late = node.handle(Request::Accept { from_node: node_id, receipt: receipt.clone() })?;
        assert_eq!(late["accepted"], false);
        assert_eq!(late["status"], "evidence_only_parent_branch");
        assert!(node.handle(Request::Execute { ticket, input: b"original input".to_vec() }).is_err());
        node.handle(Request::Accept { from_node: node_id, receipt })?;
        assert_eq!(node.handle(Request::ExportReceipts { after: "0".into(), limit: 128 })?["receipts"].as_array().unwrap().len(), 1);
        store.record_subject_request_terminal(request.request_id, &SubjectRequestTerminal {
            outcome: serde_json::json!({"kind":"unknown","reason":"original source lost"}), result_ref: None,
            recorded_commit: parent.committed_at,
        })?;
        let before_inspection = store.current_commit_seq()?;
        assert!(store.inspect_control_predecessor(&predecessor, &admission)?.original_source);
        assert_eq!(store.current_commit_seq()?, before_inspection);
        assert_eq!(store.subject_request_attempt(request.request_id)?.unwrap().state, SubjectRequestState::Terminal);
        assert_eq!(store.subject_request_attempt(request.request_id)?.unwrap().attempt_id, request.attempt_id);
        assert_eq!(store.load_subject(agent)?.1.active_request_id, None);
        Ok(())
    }

    #[test]
    fn parallel_branch_rejoin_keeps_local_head_and_refuses_incomplete_ancestry() -> Result<()> {
        let root = Root::new()?;
        let left = root.store("left")?;
        let right = root.store("right")?;
        let agent = genesis(&left)?;
        let (parent, parent_step) = left.load_subject(agent)?;
        let common = event(&left, agent, "shared before partition")?;
        let (original, staged) = stage_web_input(&left, &parent, &parent_step)?;
        left.append_context_note(EntityId(parent.branch_id.0), None, "old working note", "tool", 1, true)?;
        left.append_context_note(EntityId(parent.branch_id.0), Some(1), "", "reset", 2, false)?;
        right.import_cognition_archive(&archive(&left)?[..], &CognitionImportOptions::default())?;
        let left_branch = BranchId::new();
        let right_branch = BranchId::new();
        left.fork_subject_branch(&parent, left_branch.clone(), "left partition")?;
        right.fork_subject_branch(&parent, right_branch.clone(), "right partition")?;
        let left_event = event(&left, agent, "left continued")?;
        let right_event = event(&right, agent, "right continued")?;
        let before = serde_json::to_value(right.load_subject(agent)?.0)?;
        right.import_cognition_archive(&archive(&left)?[..], &CognitionImportOptions::default())?;
        assert_eq!(serde_json::to_value(right.load_subject(agent)?.0)?, before);
        assert_eq!(right.branch_history_range(EntityId(left_branch.0), 0, i64::MAX)?, vec![common.clone(), staged.clone(), left_event]);
        assert_eq!(right.branch_history_range(EntityId(right_branch.0), 0, i64::MAX)?, vec![common.clone(), staged.clone(), right_event]);
        let inherited_note = right.context_note_head(EntityId(right_branch.0))?.unwrap();
        assert!(!inherited_note.visible && inherited_note.body.is_empty());
        assert_eq!(inherited_note.source, "fork");
        let next_branch = BranchId::new();
        let (right_head, _) = right.load_subject(agent)?;
        right.fork_subject_branch(&right_head, next_branch.clone(), "later independent partition")?;
        let (next_head, next_step) = right.load_subject(agent)?;
        assert_eq!(right.stage_subject_inputs(&next_head, &next_step, &[])?, vec![staged.clone()]);
        assert_eq!(right.first_subject_input(agent, original.event_id)?, Some(staged.clone()));
        let frozen = right.branch_history_range(EntityId(next_branch.0), 0, i64::MAX)?;
        let late = event(&left, agent, "parent peer continued after child fork")?;
        right.import_cognition_archive(&archive(&left)?[..], &CognitionImportOptions::default())?;
        assert_eq!(right.branch_history_range(EntityId(next_branch.0), 0, i64::MAX)?, frozen);
        assert!(right.branch_history_event(EntityId(next_branch.0), late.event_id)?.is_none());

        // A receipt without its inherited selection would lose this consumed
        // but uncommitted input. Reject before publishing any imported facts.
        let fault = rusqlite::Connection::open(root.0.join("left.db"))?;
        let first = left.subject_branch_fork(left_branch.clone())?.unwrap().first_step_id;
        fault.execute("CREATE TEMP TABLE saved_inherited_input AS SELECT * FROM subject_raw_inputs WHERE step_id=?1",
            [first.to_string()])?;
        fault.execute("DELETE FROM subject_raw_inputs WHERE step_id=?1", [first.to_string()])?;
        let missing_input = archive(&left)?;
        fault.execute_batch("INSERT INTO subject_raw_inputs SELECT * FROM saved_inherited_input; DROP TABLE saved_inherited_input;")?;
        let before = serde_json::to_value(right.load_subject(agent)?.0)?;
        assert!(right.import_cognition_archive(&missing_input[..], &CognitionImportOptions::default()).is_err());
        assert_eq!(serde_json::to_value(right.load_subject(agent)?.0)?, before);

        // A historical head's registered step is also required ancestry, even
        // when the child has a valid first step and a complete raw prefix.
        let fault = rusqlite::Connection::open(root.0.join("left.db"))?;
        fault.execute("CREATE TEMP TABLE saved_parent_step AS SELECT * FROM subject_steps WHERE step_id=?1",
            [parent.next_step_id.to_string()])?;
        fault.execute("DELETE FROM subject_steps WHERE step_id=?1", [parent.next_step_id.to_string()])?;
        let missing_step = archive(&left)?;
        fault.execute_batch("INSERT INTO subject_steps SELECT * FROM saved_parent_step; DROP TABLE saved_parent_step;")?;
        let before = serde_json::to_value(right.load_subject(agent)?.0)?;
        assert!(right.import_cognition_archive(&missing_step[..], &CognitionImportOptions::default()).is_err());
        assert_eq!(serde_json::to_value(right.load_subject(agent)?.0)?, before);

        // Fault injection removes a required original at the source. Its
        // normally generated archive has a VALID fingerprint for those bad
        // bytes, so the receiver must reject lineage, not merely framing.
        rusqlite::Connection::open(root.0.join("left.db"))?.execute(
            "DELETE FROM session_events WHERE event_id=?1", [common.event_id.to_string()])?;
        let new_event = event(&left, agent, "must not partially publish")?;
        let before = serde_json::to_value(right.load_subject(agent)?.0)?;
        assert!(right.import_cognition_archive(&archive(&left)?[..], &CognitionImportOptions::default()).is_err());
        assert_eq!(serde_json::to_value(right.load_subject(agent)?.0)?, before);
        assert!(right.session_event(new_event.event_id)?.is_none());
        assert!(right.session_event(common.event_id)?.is_some());
        Ok(())
    }
}
