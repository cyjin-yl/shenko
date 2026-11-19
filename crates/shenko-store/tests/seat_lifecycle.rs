use shenko_store::{
    LeaseEndReason, SeatCapability, SeatFleetEntry, SeatLeaseEnd, SeatLeaseRequest, SeatLocation,
    SeatStatus, Store,
};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

fn lease_request(
    seat_id: EntityId,
    holder_task_id: EntityId,
    lease_id: EntityId,
    lease_epoch: u64,
) -> SeatLeaseRequest {
    SeatLeaseRequest {
        lease_id,
        seat_id,
        holder_task_id,
        lease_epoch,
        fence_capability: "tmux.send-keys:%1".into(),
    }
}

fn location(pane: &str) -> SeatLocation {
    SeatLocation::Tmux {
        host: "local".into(), session: "dev".into(), window: 0, pane: 0,
        pane_id: pane.into(), cwd: "/project".into(), current_command: "omp".into(),
    }
}

fn protocol_code(err: &anyhow::Error) -> Option<(&str, &serde_json::Value)> {
    match err.downcast_ref::<ShenkoError>() {
        Some(ShenkoError::Protocol(p)) => Some((p.code.as_str(), &p.details)),
        _ => None,
    }
}

fn answer_candidate() -> (
    std::path::PathBuf,
    Store,
    shenko_store::SeatDecisionExpectation,
    shenko_store::SeatAnswerDecisionRecord,
) {
    use shenko_store::{
        SeatAnswerDecisionRecord, SeatAnswerDecisionState, SeatAnswerFact, SeatConfidenceFact,
        SeatDecisionExpectation, SeatQuestionFact, SeatQuestionRecord, SeatQuestionState,
    };
    use shenko_types::{
        commit::CommitRef, ids::{LedgerSeq, RecordRevision, TaskRevision}, store::WriteBatch,
        task::{DispatchEligibility, ImplementationRef, TaskOwner, TaskRecord, TaskState},
    };

    let dir = std::env::temp_dir().join(format!("shenko-seat-answer-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let input = store.cas.seal_object(b"answer task", "text/plain", "test", "raw", 1)
        .unwrap().object;
    let agent = EntityId::new();
    let task = TaskRecord {
        record_schema_version: 1, task_id: EntityId::new(), task_revision: TaskRevision(0),
        record_revision: RecordRevision(0), agent_id: agent, conversation_id: agent,
        owner: TaskOwner::Conversation { id: agent }, background: false,
        kind: "seat.answer.test".into(), definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: agent, artifact_digest: input.digest.clone(), registry_revision: 1,
        },
        input_ref: input, state: TaskState::Pending { checkpoint_ref: None },
        abort_request: None, memos: Default::default(), residual_case_ids: vec![],
        created_by_step_id: None, dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0, first_run_at: None, terminal_at: None,
        created_commit: commit, updated_commit: commit,
    };
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task.clone());
    store.commit(batch).unwrap();
    let seat = store.register_seat(EntityId::new(), task.task_id, "omp_rpc", "1",
        SeatLocation::OmpRpc { host: "local".into(), remote_session_id: "answer".into() })
        .unwrap();
    let lease = store.acquire_seat_lease(SeatLeaseRequest {
        lease_id: EntityId::new(), seat_id: seat.seat_id, holder_task_id: task.task_id,
        lease_epoch: 1, fence_capability: "omp.ui_response".into(),
    }).unwrap();
    let question = store.open_seat_question(SeatQuestionRecord {
        schema_version: 2, question_id: EntityId::new(), seat_id: seat.seat_id,
        task_id: task.task_id, seat_incarnation: seat.incarnation,
        task_revision: task.task_revision, lease_id: lease.lease_id,
        lease_epoch: lease.lease_epoch, question_revision: 0,
        question: SeatQuestionFact {
            source_kind: "native_protocol".into(), question_kind: "text".into(),
            native_request_id: Some("request-1".into()), native_tool_use_id: None,
            native_payload: None, title: None, text: "Which file should change?".into(),
            options: vec![], selected_option: None,
            confidence: SeatConfidenceFact { level: "high".into(), basis: "Store fixture".into() },
            unknown: vec![], observed_screen_revision: 1, expires_at_ms: None,
            fingerprint: "native-request-1".into(),
        },
        state: SeatQuestionState::Open, record_revision: RecordRevision(0),
        created_commit: commit, updated_commit: commit,
    }).unwrap();
    let expected = SeatDecisionExpectation {
        question_revision: question.question_revision,
        question_record_revision: question.record_revision,
        seat_incarnation: seat.incarnation, seat_revision: seat.record_revision,
        task_revision: task.task_revision, lease_epoch: lease.lease_epoch,
    };
    let decision = SeatAnswerDecisionRecord {
        schema_version: 2, decision_id: EntityId::new(), question_id: question.question_id,
        seat_id: seat.seat_id, task_id: task.task_id, lease_id: lease.lease_id,
        lease_epoch: lease.lease_epoch, fence_capability: lease.fence_capability,
        expected_question_revision: expected.question_revision,
        expected_question_record_revision: expected.question_record_revision,
        expected_seat_incarnation: expected.seat_incarnation,
        expected_seat_revision: expected.seat_revision,
        expected_task_revision: expected.task_revision,
        deciding_actor: "Store fixture".into(), delegation_grant_ref: None,
        authority_revision: None,
        answer: SeatAnswerFact { option_id: None, text: Some("main.rs".into()), allow: None,
            reason: "Bounded Store admission journey".into() },
        exact_effect_input_digest: String::new(), transport: None, confirmation: None,
        closure_evidence: None, operation_id: None, state: SeatAnswerDecisionState::Committed,
        record_revision: RecordRevision(0), created_commit: commit, updated_commit: commit,
    };
    (dir, store, expected, decision)
}

#[test]
fn answer_arm_rechecks_the_committed_scope_after_another_store_handle_changes_it() {
    use shenko_store::{SeatAnswerDecisionState, SeatQuestionState};
    use shenko_types::store::WriteBatch;

    // Five windows, using the actual Store admissions and a second connection.
    // No adapter or external transport is executed by this Store-only journey.
    for window in ["unchanged", "incarnation", "task", "lease", "contradictory_expectation"] {
        let (dir, store, expected, mut candidate) = answer_candidate();
        if window == "contradictory_expectation" {
            candidate.expected_task_revision = candidate.expected_task_revision.next();
            let id = candidate.decision_id;
            let question_id = candidate.question_id;
            let before = serde_json::to_value(store.seat_question(question_id).unwrap().unwrap()).unwrap();
            let error = store.commit_seat_answer_decision(&expected, candidate).unwrap_err();
            assert_eq!(protocol_code(&error).unwrap().0, "seat_decision_binding_mismatch");
            assert!(store.seat_answer_decision(id).unwrap().is_none());
            assert_eq!(serde_json::to_value(store.seat_question(question_id).unwrap().unwrap()).unwrap(), before);
            continue;
        }
        let decision = store.commit_seat_answer_decision(&expected, candidate).unwrap();
        let question = store.seat_question(decision.question_id).unwrap().unwrap();
        let original_decision = serde_json::to_value(&decision).unwrap();
        let original_question = serde_json::to_value(&question).unwrap();
        let contender = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
        let refusal = match window {
            "incarnation" => {
                contender.bump_seat_incarnation(decision.seat_id).unwrap();
                // The old lease remains current: checking only its epoch
                // would admit the now-stale decision.
                assert!(contender.seat_lease(decision.lease_id).unwrap().unwrap().is_current());
                Some("seat_location_changed")
            }
            "task" => {
                let mut task = contender.get_task(decision.task_id).unwrap().unwrap();
                task.task_revision = task.task_revision.next();
                task.record_revision = task.record_revision.next();
                let mut batch = WriteBatch::new();
                batch.task_upserts.push(task);
                contender.commit(batch).unwrap();
                Some("stale_task")
            }
            "lease" => {
                contender.end_seat_lease(decision.lease_id, SeatLeaseEnd::HumanTakeover).unwrap();
                Some("stale_authority")
            }
            "unchanged" => None,
            _ => unreachable!(),
        };
        drop(contender);
        let operation = EntityId::new();
        let digest = shenko_types::object::Digest::blake3(b"exact intended answer").to_string();
        let result = store.arm_seat_answer_decision(
            decision.decision_id, question.record_revision, digest.clone(), operation,
        );
        if let Some(code) = refusal {
            assert_eq!(protocol_code(&result.unwrap_err()).unwrap().0, code, "{window}");
            assert_eq!(serde_json::to_value(store.seat_answer_decision(decision.decision_id).unwrap().unwrap()).unwrap(), original_decision);
            assert_eq!(serde_json::to_value(store.seat_question(question.question_id).unwrap().unwrap()).unwrap(), original_question);
        } else {
            let armed = result.unwrap();
            assert_eq!(armed.state, SeatAnswerDecisionState::Armed);
            assert_eq!(armed.operation_id, Some(operation));
            assert_eq!(armed.exact_effect_input_digest, digest);
            assert_eq!(store.seat_question(question.question_id).unwrap().unwrap().state, SeatQuestionState::Responding);
        }
        drop(store);
        let reopened = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
        let retained = reopened.seat_answer_decision(decision.decision_id).unwrap().unwrap();
        if let Some(code) = refusal {
            assert_eq!(serde_json::to_value(&retained).unwrap(), original_decision);
            let error = reopened.arm_seat_answer_decision(
                decision.decision_id, question.record_revision, digest, operation,
            ).unwrap_err();
            assert_eq!(protocol_code(&error).unwrap().0, code, "{window} after reopen");
        } else {
            assert_eq!(retained.state, SeatAnswerDecisionState::Armed);
            assert_eq!(retained.operation_id, Some(operation));
            assert_eq!(retained.exact_effect_input_digest, digest);
            let error = reopened.arm_seat_answer_decision(
                decision.decision_id, question.record_revision, digest, operation,
            ).unwrap_err();
            assert_eq!(protocol_code(&error).unwrap().0, "seat_decision_not_armed");
        }
    }
}

#[test]
fn displaced_writer_stays_displaced_after_reopen_and_human_takeover() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let cas = dir.join("cas");
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    let task = EntityId::new();
    let seat = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let acquisition_id = EntityId::new();
    let first = store.acquire_seat_lease(lease_request(seat.seat_id, task, acquisition_id, 1)).unwrap();
    // A takeover by the seat's own owner needs a strictly greater epoch.
    let second = store.acquire_seat_lease(lease_request(seat.seat_id, task, EntityId::new(), 2)).unwrap();
    drop(store);
    let store = Store::open(db, cas).unwrap();
    // Recover the original acquisition by the identity chosen before the
    // attempt. Replaying it after reopen cannot displace the newer writer.
    let replay = store.acquire_seat_lease(lease_request(seat.seat_id, task, acquisition_id, 1)).unwrap();
    assert_eq!(replay.lease_id, acquisition_id);
    assert_eq!(replay.created_commit, first.created_commit);
    assert!(!replay.is_current());
    assert_eq!(replay.superseded_by, Some(second.lease_id));
    let altered_epoch = store.acquire_seat_lease(lease_request(seat.seat_id, task, acquisition_id, 2)).unwrap_err();
    assert_eq!(protocol_code(&altered_epoch).unwrap().0, "seat_lease_identity_conflict");
    let mut altered_fence = lease_request(seat.seat_id, task, acquisition_id, 1);
    altered_fence.fence_capability = "another-input-path".into();
    assert_eq!(protocol_code(&store.acquire_seat_lease(altered_fence).unwrap_err()).unwrap().0,
        "seat_lease_identity_conflict");
    assert!(store.check_seat_write(first.lease_id, task).is_err());
    assert!(store.check_seat_write(second.lease_id, task).is_ok());
    assert_eq!(store.seat_lease(first.lease_id).unwrap().unwrap().superseded_by, Some(second.lease_id));
    store.end_seat_lease(second.lease_id, SeatLeaseEnd::HumanTakeover).unwrap();
    // The recorded end reason is a real JSON value, not double-encoded text.
    let err = store.check_seat_write(second.lease_id, task).unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_lease_ended");
    assert_eq!(
        serde_json::from_value::<LeaseEndReason>(details["reason"].clone()).unwrap(),
        LeaseEndReason::HumanTakeover,
    );
}

#[test]
fn relocation_preserves_history_and_invalidates_previous_location_lease() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let old = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(lease_request(old.seat_id, task, EntityId::new(), 1)).unwrap();
    let new = store.relocate_seat(old.seat_id, task, "omp", "1", location("%2")).unwrap();
    assert_eq!(new.supersedes, Some(old.seat_id));
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Retired);
    assert_eq!(store.seat_successor(old.seat_id).unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.seat_at_location("local", "%1").unwrap().is_none());
    assert_eq!(store.seat_at_location("local", "%2").unwrap().unwrap().seat_id, new.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task).is_err());
    assert_eq!(store.seat_task_seats(task).unwrap().len(), 2);
}

#[test]
fn fleet_inventory_retains_history_and_bounds_rows_with_a_truncation_witness() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-fleet-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let first = store.register_seat(EntityId::new(), task, "omp", "1", location("%1")).unwrap();
    let successor = store.relocate_seat(first.seat_id, task, "omp", "1", location("%2")).unwrap();
    let inventory = store.seat_fleet_summary(Some(task)).unwrap();
    // Relocation updates both records in one commit; UUID order breaks that
    // tie. The consumer contract is preserved history and current status.
    let current = inventory.iter().find(|seat| seat.seat_id == successor.seat_id).unwrap();
    let predecessor = inventory.iter().find(|seat| seat.seat_id == first.seat_id).unwrap();
    assert_eq!(current.status, SeatStatus::Active);
    assert_eq!(predecessor.status, SeatStatus::Retired);
    assert_eq!(current.open_question_count, 0);
    let other_task = EntityId::new();
    let other = store.register_seat(EntityId::new(), other_task, "omp", "1", location("%other")).unwrap();
    let all = store.seat_fleet_summary(None).unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].seat_id, other.seat_id);
    assert_eq!(all[0].task_id, other_task);
    assert_eq!(store.seat_fleet_summary(Some(task)).unwrap().len(), 2);
    for n in 3..=40 {
        store.register_seat(EntityId::new(), task, "omp", "1", location(&format!("%{n}"))).unwrap();
    }
    let bounded = store.seat_fleet_summary(Some(task)).unwrap();
    assert_eq!(bounded.len(), shenko_store::SEAT_FLEET_LIMIT + 1);
    assert!(bounded.iter().all(|seat| seat.last_observed_at.is_none()));
    assert!(store.seat_fleet_summary(Some(EntityId::new())).unwrap().is_empty());
}

#[test]
fn pane_admits_only_one_active_seat_for_registration_and_relocation() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let contender = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let task_c = EntityId::new();
    // Both discoverers saw no seat. The later admission must recheck inside
    // its write transaction, not rely on this now-stale discovery result.
    assert!(store.seat_at_location("local", "%2").unwrap().is_none());
    assert!(contender.seat_at_location("local", "%2").unwrap().is_none());
    let seat_a = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%2")).unwrap();
    let err = contender
        .register_seat(EntityId::new(), task_b, "omp", "1", location("%2"))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_location_occupied");
    assert_eq!(details["claimant_seat_id"], serde_json::json!(seat_a.seat_id.to_string()));
    assert_eq!(contender.seat_at_location("local", "%2").unwrap().unwrap().seat_id, seat_a.seat_id);
    // Relocation into an occupied pane is refused the same way.
    let seat_c = contender.register_seat(EntityId::new(), task_c, "omp", "1", location("%3")).unwrap();
    let lease_c = contender
        .acquire_seat_lease(lease_request(seat_c.seat_id, task_c, EntityId::new(), 1))
        .unwrap();
    let err = contender
        .relocate_seat(seat_c.seat_id, task_c, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    assert_eq!(store.seat(seat_c.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_location("local", "%3").unwrap().unwrap().seat_id, seat_c.seat_id);
    assert!(store.check_seat_write(lease_c.lease_id, task_c).is_ok());
}

#[test]
fn relocation_requires_the_predecessors_own_task() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let old = store.register_seat(EntityId::new(), task_a, "omp", "1", location("%1")).unwrap();
    let lease = store.acquire_seat_lease(lease_request(old.seat_id, task_a, EntityId::new(), 1)).unwrap();
    // Relocating under another task is refused and leaves task A's seat alive.
    let err = store
        .relocate_seat(old.seat_id, task_b, "omp", "1", location("%2"))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_task_mismatch");
    assert_eq!(store.seat(old.seat_id).unwrap().unwrap().status, SeatStatus::Active);
    assert_eq!(store.seat_at_location("local", "%1").unwrap().unwrap().seat_id, old.seat_id);
    assert!(store.check_seat_write(lease.lease_id, task_a).is_ok());
    assert_eq!(store.seat_task_seats(task_a).unwrap().len(), 1);
    assert!(store.seat_task_seats(task_b).unwrap().is_empty());
}

#[test]
fn successor_walk_resolves_long_relocation_chains() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let mut seat = store.register_seat(EntityId::new(), task, "omp", "1", location("%0")).unwrap();
    let origin = seat.seat_id;
    for i in 1..70 {
        seat = store
            .relocate_seat(seat.seat_id, task, "omp", "1", location(&format!("%{i}")))
            .unwrap();
    }
    assert_eq!(store.seat_successor(origin).unwrap().unwrap().seat_id, seat.seat_id);
}

fn omp_location(session: &str) -> SeatLocation {
    SeatLocation::OmpRpc {
        host: "local".into(),
        remote_session_id: session.into(),
    }
}

fn claude_location(session: &str) -> SeatLocation {
    SeatLocation::ClaudeSdk {
        host: "local".into(),
        remote_session_id: session.into(),
    }
}

#[test]
fn managed_session_anchor_admits_only_one_active_seat() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task_a = EntityId::new();
    let task_b = EntityId::new();
    let seat_a = store
        .register_seat(EntityId::new(), task_a, "omp_rpc", "omp-rpc-v1", omp_location("omp sess"))
        .unwrap();
    // A different adapter family at the same (host, anchor) is still one
    // location: the claim space is shared.
    let err = store
        .register_seat(
            EntityId::new(),
            task_b,
            "claude_sdk",
            "claude-control-v1",
            claude_location("omp sess"),
        )
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_occupied");
    // The same anchor under a different host is a different location.
    let remote = SeatLocation::ClaudeSdk {
        host: "remote".into(),
        remote_session_id: "omp sess".into(),
    };
    store
        .register_seat(EntityId::new(), task_b, "claude_sdk", "claude-control-v1", remote)
        .unwrap();
    // The lookup resolves both kinds by their anchor.
    assert_eq!(
        store.seat_at_location("local", "omp sess").unwrap().unwrap().seat_id,
        seat_a.seat_id
    );
    // Retiring the incumbent frees the anchor for the next claimant.
    store.retire_seat(seat_a.seat_id).unwrap();
    let seat_b = store
        .register_seat(
            EntityId::new(),
            task_b,
            "claude_sdk",
            "claude-control-v1",
            claude_location("omp sess"),
        )
        .unwrap();
    assert_eq!(
        store.seat_at_location("local", "omp sess").unwrap().unwrap().seat_id,
        seat_b.seat_id
    );
    // Idempotent re-registration of the same facts still returns the record.
    let again = store
        .register_seat(seat_b.seat_id, task_b, "claude_sdk", "claude-control-v1", claude_location("omp sess"))
        .unwrap();
    assert_eq!(again.seat_id, seat_b.seat_id);
}

#[test]
fn incarnation_bump_stales_existing_leases_without_ending_them() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let seat = store
        .register_seat(EntityId::new(), task, "omp", "1", location("%7"))
        .unwrap();
    let lease = store
        .acquire_seat_lease(lease_request(seat.seat_id, task, EntityId::new(), 1))
        .unwrap();
    assert_eq!(seat.incarnation.0, 0);
    let bumped = store.bump_seat_incarnation(seat.seat_id).unwrap();
    assert_eq!(bumped.incarnation.0, 1);
    assert_eq!(bumped.seat_id, seat.seat_id);
    assert!(bumped.record_revision.0 > seat.record_revision.0);
    assert_eq!(bumped.status, SeatStatus::Active);
    // The lease itself is not ended by an observation; the incarnation fence
    // is what refuses the write.
    let current = store.seat_current_lease(seat.seat_id).unwrap().unwrap();
    assert!(current.is_current());
    assert_eq!(current.lease_id, lease.lease_id);
    let err = store.check_seat_write(lease.lease_id, task).unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_location_changed");
    // The record's supersedes chain is untouched: an in-place bump is not a
    // relocation and mints no successor.
    assert_eq!(
        store.seat_successor(seat.seat_id).unwrap().unwrap().seat_id,
        seat.seat_id
    );
    // A retired record can never be bumped.
    store.retire_seat(seat.seat_id).unwrap();
    assert!(store.bump_seat_incarnation(seat.seat_id).is_err());
}

#[test]
fn active_seat_records_lists_only_live_seats() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let tmux_seat = store
        .register_seat(EntityId::new(), task, "tmux", "1", location("%1"))
        .unwrap();
    let omp_seat = store
        .register_seat(EntityId::new(), task, "omp_rpc", "1", omp_location("s"))
        .unwrap();
    store.retire_seat(tmux_seat.seat_id).unwrap();
    let active = store.active_seat_records().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].seat_id, omp_seat.seat_id);
}

#[test]
fn pre_v2_location_index_is_rebuilt_to_cover_session_anchors() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("store.db");
    let cas = dir.join("cas");
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    drop(store);
    // Simulate a store written before the index covered remote_session_id.
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP INDEX idx_seat_active_location;
             CREATE UNIQUE INDEX idx_seat_active_location ON seat_records(
                 json_extract(data,'$.location.host'), json_extract(data,'$.location.pane_id')
             ) WHERE status='active';",
        )
        .unwrap();
    }
    let _store = Store::open(db.clone(), cas).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='idx_seat_active_location'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sql.contains("remote_session_id"), "index was not rebuilt: {sql}");
}

/// A seat has at most one valid writer, and that writer is the seat's own
/// durable owner. A foreign holder is refused and never displaces the live
/// writer while the fleet still names the owner.
#[test]
fn a_foreign_holder_cannot_acquire_the_seats_writer_lease() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-owner-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let owner = EntityId::new();
    let foreign = EntityId::new();
    let seat = store.register_seat(EntityId::new(), owner, "omp", "1", location("%1")).unwrap();
    let lease = store
        .acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), 1))
        .unwrap();
    // A caller that names another task cannot take the writer lease, not even
    // with a much higher epoch.
    let refused = EntityId::new();
    let err = store
        .acquire_seat_lease(lease_request(seat.seat_id, foreign, refused, 9))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_task_mismatch");
    assert_eq!(details["seat_id"], serde_json::json!(seat.seat_id.to_string()));
    assert_eq!(details["seat_task_id"], serde_json::json!(owner.to_string()));
    assert_eq!(details["claimed_task_id"], serde_json::json!(foreign.to_string()));
    // The refused acquisition wrote nothing: the owner's lease still holds
    // the seat, and the fleet still names the owner.
    assert!(store.seat_lease(refused).unwrap().is_none());
    let current = store.seat_current_lease(seat.seat_id).unwrap().unwrap();
    assert_eq!(current.lease_id, lease.lease_id);
    assert_eq!(current.holder_task_id, owner);
    assert!(current.is_current());
    assert!(store.check_seat_write(lease.lease_id, owner).is_ok());
    assert!(store.check_seat_write(lease.lease_id, foreign).is_err());
    assert_eq!(store.seat(seat.seat_id).unwrap().unwrap().task_id, owner);
    let fleet = store.seat_fleet_summary(Some(owner)).unwrap();
    assert_eq!(fleet.len(), 1);
    assert_eq!(fleet[0].task_id, owner);
    // A foreign holder is refused even before the seat has any writer.
    let fresh = store.register_seat(EntityId::new(), owner, "omp", "1", location("%2")).unwrap();
    let err = store
        .acquire_seat_lease(lease_request(fresh.seat_id, foreign, EntityId::new(), 1))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_task_mismatch");
    assert!(store.seat_current_lease(fresh.seat_id).unwrap().is_none());

    // An old database may already contain a foreign-holder lease. Retain it
    // as history, but never let its holder pass write admission after reopen.
    let mut older_foreign = lease.clone();
    older_foreign.holder_task_id = foreign;
    drop(store);
    let connection = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    connection.execute(
        "UPDATE seat_leases SET holder_task_id=?2,data=?3 WHERE lease_id=?1",
        rusqlite::params![lease.lease_id.to_string(), foreign.to_string(), serde_json::to_string(&older_foreign).unwrap()],
    ).unwrap();
    drop(connection);
    let reopened = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let error = reopened.check_seat_write(lease.lease_id, foreign).unwrap_err();
    assert_eq!(protocol_code(&error).unwrap().0, "seat_task_mismatch");
    let error = reopened.acquire_seat_lease(lease_request(seat.seat_id, foreign, lease.lease_id, 1)).unwrap_err();
    assert_eq!(protocol_code(&error).unwrap().0, "seat_task_mismatch");
    assert_eq!(reopened.seat_lease(lease.lease_id).unwrap().unwrap().holder_task_id, foreign);
}

/// The writer epoch is monotonic per seat: only a strictly greater epoch is
/// a newer writer. A lower or equal epoch is refused and the current lease
/// stays current and unchanged.
#[test]
fn a_lease_epoch_must_exceed_the_current_writer_epoch() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-epoch-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let owner = EntityId::new();
    let seat = store.register_seat(EntityId::new(), owner, "omp", "1", location("%1")).unwrap();
    let lease = store
        .acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), 5))
        .unwrap();
    // A lower epoch is not a newer writer.
    let lower = EntityId::new();
    let err = store
        .acquire_seat_lease(lease_request(seat.seat_id, owner, lower, 4))
        .unwrap_err();
    let (code, details) = protocol_code(&err).unwrap();
    assert_eq!(code, "seat_lease_epoch_regression");
    assert_eq!(details["seat_id"], serde_json::json!(seat.seat_id.to_string()));
    assert_eq!(details["requested_lease_epoch"], serde_json::json!(4));
    assert_eq!(details["current_lease_epoch"], serde_json::json!(5));
    assert_eq!(details["current_lease_id"], serde_json::json!(lease.lease_id.to_string()));
    assert_eq!(details["current_holder_task_id"], serde_json::json!(owner.to_string()));
    // The refused acquisition wrote nothing, and the current lease is
    // unchanged and still carries write authority.
    assert!(store.seat_lease(lower).unwrap().is_none());
    let current = store.seat_current_lease(seat.seat_id).unwrap().unwrap();
    assert_eq!(current.lease_id, lease.lease_id);
    assert_eq!(current.lease_epoch, 5);
    assert_eq!(current.holder_task_id, owner);
    assert!(current.is_current());
    assert_eq!(current.record_revision, lease.record_revision);
    assert_eq!(current.created_commit, lease.created_commit);
    assert_eq!(current.updated_commit, lease.updated_commit);
    assert!(store.check_seat_write(lease.lease_id, owner).is_ok());
    // The same epoch is not a newer writer either.
    let same = EntityId::new();
    let err = store
        .acquire_seat_lease(lease_request(seat.seat_id, owner, same, 5))
        .unwrap_err();
    assert_eq!(protocol_code(&err).unwrap().0, "seat_lease_epoch_regression");
    assert!(store.seat_lease(same).unwrap().is_none());
    assert_eq!(
        store.seat_current_lease(seat.seat_id).unwrap().unwrap().lease_id,
        lease.lease_id
    );
    // The seat's own owner with a greater epoch takes over: the old lease
    // ends as superseded and its writes are refused afterwards.
    let next = store
        .acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), 6))
        .unwrap();
    assert_eq!(next.lease_epoch, 6);
    assert!(next.is_current());
    let displaced = store.seat_lease(lease.lease_id).unwrap().unwrap();
    assert!(!displaced.is_current());
    assert_eq!(displaced.ended_reason, Some(LeaseEndReason::Superseded));
    assert_eq!(displaced.superseded_by, Some(next.lease_id));
    assert!(store.check_seat_write(lease.lease_id, owner).is_err());
    assert!(store.check_seat_write(next.lease_id, owner).is_ok());

    store.end_seat_lease(next.lease_id, SeatLeaseEnd::Released).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    assert!(reopened.seat_current_lease(seat.seat_id).unwrap().is_none());
    let before_refusal = reopened.current_commit_seq().unwrap();
    let error = reopened.acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), 6)).unwrap_err();
    let (code, details) = protocol_code(&error).unwrap();
    assert_eq!(code, "seat_lease_epoch_regression");
    assert_eq!(details["highest_lease_epoch"], 6);
    assert!(details["current_lease_epoch"].is_null());
    assert_eq!(reopened.current_commit_seq().unwrap(), before_refusal);
    let replay = reopened.acquire_seat_lease(lease_request(seat.seat_id, owner, next.lease_id, 6)).unwrap();
    assert_eq!(replay.ended_reason, Some(LeaseEndReason::Released));

    // Epochs use their complete u64 range, including values larger than a
    // SQLite integer. Releasing the maximum value cannot reset that history.
    let last = reopened.acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), u64::MAX)).unwrap();
    reopened.end_seat_lease(last.lease_id, SeatLeaseEnd::HumanTakeover).unwrap();
    let error = reopened.acquire_seat_lease(lease_request(seat.seat_id, owner, EntityId::new(), u64::MAX - 1)).unwrap_err();
    assert_eq!(protocol_code(&error).unwrap().0, "seat_lease_epoch_regression");
    assert!(reopened.seat_current_lease(seat.seat_id).unwrap().is_none());
}

/// The fleet entry carries the capability flags the adapter declared. The
/// durable summary never invents them: only an attached adapter publishes them.
#[test]
fn the_fleet_entry_carries_the_adapter_declared_capability_flags() {
    let dir = std::env::temp_dir().join(format!("shenko-seat-capability-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let task = EntityId::new();
    let seat = store
        .register_seat(EntityId::new(), task, "omp_rpc", "omp-rpc-v1", omp_location("omp sess"))
        .unwrap();
    store
        .acquire_seat_lease(lease_request(seat.seat_id, task, EntityId::new(), 1))
        .unwrap();
    let inventory = store.seat_fleet_summary(Some(task)).unwrap();
    let entry = inventory.iter().find(|row| row.seat_id == seat.seat_id).unwrap();
    // The durable record does not attest what an adapter can do.
    assert!(entry.capability.is_none());
    // The published fleet entry carries the flags the adapter declared.
    let capability = SeatCapability {
        structured_questions: true,
        structured_answers: true,
        screen_observation: false,
        steer: true,
        interrupt: true,
        resume: true,
        notes: vec!["问题来自结构化事件".into()],
    };
    let published = SeatFleetEntry { capability: Some(capability.clone()), ..entry.clone() };
    assert_eq!(published.capability.as_ref().unwrap(), &capability);
    let json = serde_json::to_value(&published).unwrap();
    assert_eq!(json["capability"]["structured_questions"], serde_json::json!(true));
    assert_eq!(json["capability"]["screen_observation"], serde_json::json!(false));
    assert_eq!(json["capability"]["resume"], serde_json::json!(true));
    let round: SeatFleetEntry = serde_json::from_value(json).unwrap();
    assert_eq!(round.capability, published.capability);
}
