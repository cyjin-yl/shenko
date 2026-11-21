//! S1 验证：SQLite WAL store round-trip + CAS seal/read
//! 验证: 单 mutation line, CAS object seal/read, FTS5 detection

use anyhow::{Context, Result};
use shenko_store::store::Store;
use shenko_types::store::WriteBatch;
use std::path::PathBuf;
use std::sync::Arc;

#[path = "support/subject_genesis.rs"]
mod subject_genesis;
#[path = "support/canonical_native.rs"]
mod canonical_native;
use canonical_native::{CanonicalGuard, NativeT2Authority, NativeT2AuthorityState,
    recorded_result_with_docket, committed_candidate_with, draft_dispatch_operation};
use subject_genesis::initialize_subject_in_with_route;


fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "shenko-test-{}",
        shenko_types::ids::EntityId::new()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn store_opens_with_wal_and_fts5_detection() {
    let dir = tmp_dir();
    let db = dir.join("test.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    assert!(store.capabilities.wal_mode, "WAL mode should be enabled");
    assert_eq!(store.capabilities.synchronous, "2", "synchronous=FULL");
    // FTS5 may or may not be available depending on build; just check it's detected
    let _ = store.capabilities.fts5_available;
    // Local model health is a CAS-bound observation chain, independent of
    // authority selection. The fixture checks storage, not model usability.
    let binding = shenko_types::object::Digest::blake3(b"local health storage fixture");
    let body = serde_json::json!({"schema_version":1,"binding_digest":binding,
        "previous_ref":null,"eligibility":{"fixture":"no capability claim"}});
    let snapshot = store.cas.seal_object(&serde_json::to_vec(&body).unwrap(),
        "application/json", "model-health", "model-health-state", 1).unwrap().object;
    store.record_local_model_health(&binding, None, &snapshot).unwrap();
    store.record_local_model_health(&binding, None, &snapshot).unwrap();
    let next_body = serde_json::json!({"schema_version":1,"binding_digest":binding,
        "previous_ref":snapshot,"eligibility":{"fixture":"later retained observation"}});
    let next = store.cas.seal_object(&serde_json::to_vec(&next_body).unwrap(),
        "application/json", "model-health", "model-health-state", 1).unwrap().object;
    assert!(store.record_local_model_health(&binding, None, &next).is_err());
    store.record_local_model_health(&binding, Some(&snapshot), &next).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("test.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.local_model_health(&binding).unwrap(), Some(next));
}

#[test]
fn cas_seal_and_read_roundtrip() {
    let dir = tmp_dir();
    let db = dir.join("cas_test.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let data = b"hello shenko cas";
    let sealed = store
        .cas
        .seal_object(data, "text/plain", "test", "raw", 1)
        .unwrap();

    assert_eq!(sealed.object.bytes, 16);
    assert_eq!(sealed.object.media_type, "text/plain");

    // Read back
    let read = store.cas.read_object(&sealed.object.digest).unwrap();
    assert_eq!(read, data);

    // Exists
    assert!(store.cas.object_exists(&sealed.object.digest));
}

#[test]
fn empty_batch_commit_returns_receipt() {
    let dir = tmp_dir();
    let db = dir.join("empty.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let batch = WriteBatch::new();
    let receipt = store.commit(batch).unwrap();

    assert_eq!(
        receipt.durability_profile,
        shenko_types::commit::DurabilityProfile::PowerLossLocal
    );
    assert!(!receipt.commit_ref.ledger_id.as_uuid().is_nil());
}

fn initialized_subject() -> (PathBuf, Arc<Store>, shenko_types::ids::EntityId) {
    initialized_subject_with_bootstrap_event(b"seed")
}

fn initialized_subject_with_bootstrap_event(
    bootstrap_event: &[u8],
) -> (PathBuf, Arc<Store>, shenko_types::ids::EntityId) {
    let dir = tmp_dir();
    let db = dir.join("head.db");
    let cas = dir.join("cas");
    let store = Arc::new(Store::open(db, cas).unwrap());
    let agent_id = initialize_subject_in(&store, bootstrap_event);
    (dir, store, agent_id)
}

fn initialize_subject_in(store: &Store, bootstrap_event: &[u8]) -> shenko_types::ids::EntityId {
    initialize_subject_in_with_route(store, bootstrap_event, None).unwrap()
}


#[test]
fn bootstrap_identity_survives_reopen_without_an_index_and_rejects_divergence() -> Result<()> {
    use shenko_types::{error::ShenkoError, ids::EntityId};
    let installation = EntityId::new();
    let event = serde_json::to_vec(&serde_json::json!({"installation_id": installation}))?;
    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("head.db"), dir.join("cas"))?);
    let agent = initialize_subject_in_with_route(&store, &event, Some(&dir))?;
    let initial_route = store.activation_selector(agent)?.context("genesis selector missing")?.route_authority_ref;
    canonical_source(&store, agent, false)?;
    let taken_over = store.activation_selector(agent)?.context("takeover selector missing")?;
    assert!(taken_over.selector_revision > 0 && taken_over.active_epoch > 1);
    assert_eq!(taken_over.route_authority_ref, initial_route);
    assert_ne!(installation, store.ledger.ledger_id());
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    assert_eq!(reopened.installation_id_for_subject(agent)?, installation);
    // This storage fixture records the seed caller's evidence boundary; the
    // actual filesystem preparation is exercised by the seed/CLI journeys.
    let local_home = reopened.cas.seal_object(&serde_json::to_vec(&serde_json::json!({
        "schema_version":1,"installation_id":installation,"state":"ready",
        "mounts":[{"source":dir.join("home"),"guest_path":"/home"}],
        "created_at":"2026-11-16T00:00:00Z","home_guest_path":"/home",
    }))?, "application/json", shenko_store::LOCAL_HOME_NAMESPACE,
        shenko_store::LOCAL_HOME_CODEC, 1)?.object;
    let local_provisioning = reopened.record_local_installation_provisioning(installation, local_home)?;
    assert_eq!(reopened.installation_id_for_home()?, installation);

    #[cfg(target_os = "linux")]
    {
        use shenko_store::{ContextWindowAppend, StoredContextWindow, StoredDerivative};
        let scope = EntityId(reopened.load_subject(agent)?.0.branch_id.0);
        let genesis = StoredContextWindow {
            window_id: EntityId::new(), generation: 0,
            body: "opaque policy-owned genesis metadata".into(), derivatives: Vec::new(),
        };
        assert_eq!(reopened.append_context_window(scope, None, None, &genesis)?, ContextWindowAppend::Appended);
        let payload = b"retained derivative payload".to_vec();
        let window = StoredContextWindow {
            window_id: EntityId::new(), generation: 1,
            body: "opaque policy-owned current metadata".into(),
            derivatives: vec![StoredDerivative {
                media_type: "application/octet-stream".into(),
                blake3: blake3::hash(&payload).to_hex().to_string(), bytes: payload.clone(),
            }],
        };
        assert_eq!(reopened.rebind_context_model_generation(scope, genesis.window_id, 0, 7,
            "snapshot-profile", &window)?, ContextWindowAppend::Appended);
        // The newly committed metadata exists only in WAL. A main-only
        // immutable reader would miss it, even though it appears read-only.
        let main_only = rusqlite::Connection::open_with_flags(
            format!("file:{}?immutable=1", dir.join("head.db").display()),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        let stale: i64 = main_only.query_row("SELECT count(*) FROM context_windows", [], |row| row.get(0))?;
        assert_eq!(stale, 0);
        drop(main_only);
        // Finish live-connection fixture reads before taking the zero-touch
        // baseline: even SELECT can update SQLite's WAL read mark in -shm.
        // Only inspectors and raw file reads access the live database tuple
        // inside the measured interval, preserving full byte equality checks.
        let route = reopened.activation_selector(agent)?.context("genesis selector missing")?.route_authority_ref;
        let route_bytes = reopened.cas.read_object(&route.digest)?;
        // Raw slot metadata needs no CAS. The installation query must reject
        // this missing route source instead of recreating it or trusting a path.
        std::fs::rename(dir.join("cas"), dir.join("retained-cas"))?;
        let read_tuple = || -> Result<Vec<Vec<u8>>> {
            ["head.db", "head.db-wal", "head.db-shm"].into_iter()
                .map(|name| std::fs::read(dir.join(name)).map_err(Into::into)).collect()
        };
        let before = read_tuple()?;
        let observed = Store::inspect_subject_context_windows(&dir.join("head.db"), None, 1)?
            .context("the existing subject's window inspection is missing")?;
        assert_eq!(observed.scope_id, scope);
        assert_eq!(observed.model_generation, 7);
        assert_eq!(observed.profile_digest.as_deref(), Some("snapshot-profile"));
        assert_eq!(observed.page.total, 2);
        assert!(observed.page.has_older);
        let [metadata] = observed.page.windows.as_slice() else { anyhow::bail!("one bounded window expected") };
        assert_eq!(metadata.window_id, window.window_id);
        assert_eq!(metadata.body, window.body);
        let [derivative] = metadata.derivatives.as_slice() else { anyhow::bail!("one derivative identity expected") };
        assert_eq!(derivative.blake3, window.derivatives[0].blake3);
        assert_eq!(derivative.bytes, payload.len() as u64);
        let Err(missing_route) = Store::inspect_active_subject_context_windows(
            &dir.join("head.db"), &dir, &dir.join("cas"), None, 1,
        ) else { anyhow::bail!("a current-slot claim requires the sealed route") };
        assert!(missing_route.to_string().contains("CAS directory does not exist"), "{missing_route:#}");
        assert_eq!(read_tuple()?, before, "inspection changed the live SQLite tuple");
        assert!(!dir.join("cas").exists(), "inspection created a live CAS directory");
        std::fs::rename(dir.join("retained-cas"), dir.join("cas"))?;
        let selected = Store::inspect_active_subject_context_windows(
            &dir.join("head.db"), &dir, &dir.join("cas"), None, 1,
        )?.context("selected window metadata missing")?;
        assert_eq!(selected, observed);
        // A copied inactive slot retains its source's selector. That sealed
        // route must refuse a launcher pointer to the copy, even if every
        // database fact and window in the copy is otherwise valid.
        let copied_slot = dir.join("versions").join("inactive-copy");
        std::fs::create_dir_all(&copied_slot)?;
        for name in ["head.db", "head.db-wal"] {
            std::fs::copy(dir.join(name), copied_slot.join(name))?;
        }
        let copied_before = ["head.db", "head.db-wal"].map(|name| std::fs::read(copied_slot.join(name)))
            .into_iter().collect::<std::io::Result<Vec<_>>>()?;
        let Err(mismatched) = Store::inspect_active_subject_context_windows(
            &copied_slot.join("head.db"), &dir, &dir.join("cas"), None, 1,
        ) else { anyhow::bail!("a copied slot cannot acquire its source's route authority") };
        assert!(mismatched.to_string().contains("launcher slot disagrees"), "{mismatched:#}");
        let copied_after = ["head.db", "head.db-wal"].map(|name| std::fs::read(copied_slot.join(name)))
            .into_iter().collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(copied_after, copied_before);
        assert!(!copied_slot.join("head.db-shm").exists());
        assert_eq!(read_tuple()?, before, "route inspection changed the live SQLite tuple");
        assert_eq!(reopened.cas.read_object(&route.digest)?, route_bytes);

        // Before a candidate's selector is mirrored, the source still has
        // its original selector and route. Its real paused boundary and
        // durable, source-bound Handoff record must make the query refuse
        // that ambiguous selection; this does not claim a candidate ran.
        use shenko_types::activation::{ActivationPhase, ActivationRecord};
        use shenko_types::object::{ArtifactRef, VersionRef};
        let current = reopened.activation_selector(agent)?.context("selected authority missing")?;
        reopened.request_subject_quiesce(agent, EntityId::new())?;
        reopened.pause_subject_at_boundary(&reopened.load_subject(agent)?.0)?
            .context("handoff source did not reach its paused boundary")?;
        reopened.create_activation_record(&ActivationRecord {
            activation_id: EntityId::new(), agent_id: agent,
            from_generation: current.generation.clone(),
            to_generation: shenko_types::ids::RuntimeGeneration("unstarted-route-candidate".into()),
            from_state_ref: current.state_ref.clone(), to_state_ref: current.state_ref.clone(),
            migrator_ref: VersionRef {
                artifact: ArtifactRef { digest: route.digest.clone(), locator: route.locator.clone() },
                symbol: "unexecuted-context-inspection-fixture".into(), schema: route.codec.clone(),
            },
            recovery_closure_ref: route.clone(), contract_revision: "context-route-inspection".into(),
            phase: ActivationPhase::Handoff, w0: None, w1: None, imported_through: None,
            ingress_cursors_ref: None, evidence_cursors_ref: None, pending_operations_ref: None,
            inference_allocations_ref: None, expected_selector_revision: current.selector_revision,
            target_epoch: current.active_epoch + 1, smoke_evidence_ref: None,
            rollback_plan_ref: route.clone(), last_error_ref: None, record_revision: 0,
            expected_primary_claim: None,
        })?;
        let handoff_before = read_tuple()?;
        let Err(pending) = Store::inspect_active_subject_context_windows(
            &dir.join("head.db"), &dir, &dir.join("cas"), None, 1,
        ) else { anyhow::bail!("an unresolved source handoff cannot prove the current route") };
        assert!(pending.to_string().contains("selection requires reconciliation"), "{pending:#}");
        assert_eq!(read_tuple()?, handoff_before);
        assert_eq!(reopened.cas.read_object(&route.digest)?, route_bytes);
    }

    let genesis = reopened.get_subject_genesis(agent)?.context("subject genesis missing")?;
    let foreign = EntityId::new();
    let conn = rusqlite::Connection::open(dir.join("head.db"))?;
    conn.execute(
        "INSERT INTO bootstrap_records(bootstrap_id,installation_id,subject_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![genesis.bootstrap_id.to_string(), foreign.to_string(), agent.to_string(),
            serde_json::json!({"installation_id": foreign}).to_string(), genesis.created_commit.seq.0 as i64],
    )?;
    assert!(matches!(reopened.installation_id_for_subject(agent), Err(ShenkoError::Conflict(_))));
    assert!(matches!(reopened.installation_id_for_home(), Err(ShenkoError::Conflict(_))));
    assert_eq!(reopened.local_installation_provisioning()?, Some(local_provisioning));
    assert_eq!(reopened.cas.read_object(&genesis.bootstrap_event_ref.digest)?, event);
    Ok(())
}

#[test]
fn bootstrap_owner_projection_resolves_the_committed_grant_after_reopen() {
    use shenko_store::config::OwnerGrant;
    use shenko_types::{ids::EntityId, object::Digest};
    let dir = tmp_dir();
    let store = Store::open(dir.join("owner.db"), dir.join("cas")).unwrap();
    let installation = EntityId::new();
    let principal = store.seal_config_object(&serde_json::json!({"kind": "principal"}), "principal").unwrap();
    let authority = store.seal_config_object(&serde_json::json!({"kind": "authority"}), "authority").unwrap();
    let (_, granted) = store.claim_owner(installation, EntityId::new(), principal, authority,
        Digest::blake3(b"owner-projection-test-token")).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("owner.db"), dir.join("cas")).unwrap();
    let reference = reopened.get_bootstrap_owner_grant(installation).unwrap().unwrap();
    let bytes = std::fs::read(reopened.cas.verified_object_path(&reference).unwrap()).unwrap();
    let projected: OwnerGrant = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(serde_json::to_value(projected).unwrap(), serde_json::to_value(granted.unwrap()).unwrap());
}

#[test]
fn boundary_waits_for_recorded_step_and_survives_reopen() -> Result<()> {
    use shenko_store::runtime_boundary::SubjectQuiesceState;
    use shenko_types::{ids::*, subject::*};

    let (dir, store, agent) = initialized_subject();
    let (head, step) = store.load_subject(agent).unwrap();
    let recorded = recorded_result(&store, &head, step)?;
    let request_id = EntityId::new();
    let request = store.request_subject_quiesce(agent, request_id).unwrap();
    assert_eq!(request.state, SubjectQuiesceState::Requested);
    assert!(store.pause_subject_at_boundary(&head).unwrap().is_none());
    assert_eq!(store.load_subject(agent).unwrap().1.state, SubjectStepState::ResultRecorded);

    let successor = EntityId::new();
    let candidate = committed_candidate(&store, recorded, successor);
    store.commit_step_and_advance_head(&candidate, &head, successor).unwrap();
    let (current, planned) = store.load_subject(agent).unwrap();
    let paused = store.pause_subject_at_boundary(&current).unwrap().unwrap();
    assert_eq!(paused.mode, SubjectMode::Paused);
    assert_eq!(paused.next_step_id, planned.step_id);
    assert!(store.export_cognition(&Default::default()).is_err(),
        "an archive must not omit the local pause record and claim a complete paused head");
    let mut stale_step = planned.clone();
    stale_step.state = SubjectStepState::Preparing;
    stale_step.record_revision = stale_step.record_revision.next();
    assert!(store.upsert_step(&stale_step, &current, planned.record_revision).is_err());
    let boundary = store.subject_quiesce(request_id).unwrap().unwrap();
    assert_eq!(boundary.state, SubjectQuiesceState::Paused);
    assert!(boundary.boundary_at.unwrap().seq > request.requested_at.seq);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let (retained, step) = reopened.load_subject(agent).unwrap();
    assert_eq!(retained.mode, SubjectMode::Paused);
    assert_eq!(step.step_id, successor);
    let closed = reopened.resume_subject_quiesce(agent, request_id).unwrap();
    assert_eq!(closed.state, SubjectQuiesceState::Resumed);
    assert!(closed.closed_at.unwrap().seq > boundary.boundary_at.unwrap().seq);
    let (active, step) = reopened.load_subject(agent).unwrap();
    assert_eq!(active.mode, SubjectMode::Active);
    assert_eq!(active.controller_epoch, current.controller_epoch);
    assert_eq!(active.software_generation, current.software_generation);
    assert_eq!(step.step_id, successor);
    assert!(reopened.export_cognition(&Default::default()).is_ok());
    drop(reopened);
    std::fs::remove_dir_all(dir).unwrap();
    Ok(())
}

#[test]
fn peer_cannot_advance_successor_of_a_locally_paused_head() -> Result<()> {
    use shenko_store::cognition::CognitionImportOptions;
    use shenko_types::ids::EntityId;
    use shenko_types::subject::{SubjectMode, SubjectStepState};

    let (_source_dir, source, agent) = initialized_subject();
    let receiver_dir = tmp_dir();
    let receiver = Store::open(receiver_dir.join("receiver.db"), receiver_dir.join("cas")).unwrap();
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    let mut initial = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut initial).unwrap();
    assert!(receiver.import_cognition_archive(&initial[..], &options).unwrap().ack(peer).verified);
    let (local, successor) = receiver.load_subject(agent).unwrap();
    let request = EntityId::new();
    receiver.request_subject_quiesce(agent, request).unwrap();
    let paused = receiver.pause_subject_at_boundary(&local).unwrap().unwrap();
    assert_eq!(paused.mode, SubjectMode::Paused);
    let before = receiver.current_commit_seq().unwrap();

    // A real source Store advances its existing S0/result while the receiver
    // retains a different local head. The old row-by-row merge could apply the
    // source step despite keeping the conflicting paused head.
    let (source_head, source_step) = source.load_subject(agent).unwrap();
    let recorded = recorded_result(&source, &source_head, source_step)?;
    assert_eq!(recorded.state, SubjectStepState::ResultRecorded);
    let mut newer = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut newer).unwrap();
    assert!(receiver.import_cognition_archive(&newer[..], &options).is_err());
    assert_eq!(receiver.current_commit_seq().unwrap(), before);
    let (retained, step) = receiver.load_subject(agent).unwrap();
    assert_eq!(serde_json::to_value(&retained).unwrap(), serde_json::to_value(&paused).unwrap());
    assert_eq!(serde_json::to_value(&step).unwrap(), serde_json::to_value(&successor).unwrap());
    receiver.resume_subject_quiesce(agent, request).unwrap();
    assert_eq!(receiver.load_subject(agent).unwrap().1.state, SubjectStepState::Planned);
    Ok(())
}


fn recorded_result(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    step: shenko_types::subject::SubjectStep,
) -> Result<shenko_types::subject::SubjectStep> {
    recorded_result_with_wait(store, head, step, None)
}

/// The frozen S0 input of one step, with the semantic wait snapshot the model
/// was shown.
fn recorded_result_with_wait(
    store: &Arc<Store>,
    head: &shenko_types::subject::SubjectBranchHead,
    step: shenko_types::subject::SubjectStep,
    semantic_wait: Option<shenko_types::subject::SemanticWait>,
) -> Result<shenko_types::subject::SubjectStep> {
    recorded_result_with_docket(store, head, step, semantic_wait, &[])
}

fn committed_candidate(
    store: &Store,
    step: shenko_types::subject::SubjectStep,
    next: shenko_types::ids::EntityId,
) -> shenko_types::subject::SubjectStep {
    committed_candidate_with(store, step, next, vec![])
}

/// Attach one semantic wait change to a committed candidate, exactly as the
/// subject does: the parsed control and the policy intent carry the same
/// action id and the same change.
fn attach_wait(
    step: &mut shenko_types::subject::SubjectStep,
    store: &Store,
    change: &shenko_types::subject::SemanticWaitChange,
    action_id: shenko_types::ids::EntityId,
) {
    use shenko_types::output::{OutputAction, OutputControl};
    use shenko_types::subject::{PolicyIntent, PolicyKind, StepDecision};
    let mut decision: StepDecision = serde_json::from_slice(
        &store
            .cas
            .read_object(&step.decision_ref.as_ref().unwrap().digest)
            .unwrap(),
    )
    .unwrap();
    decision.output.controls.push(OutputControl {
        action_id,
        spans: vec![],
        action: OutputAction::SemanticWait {
            change: change.clone(),
        },
    });
    decision.policy_intents.push(PolicyIntent {
        intent_id: action_id,
        kind: PolicyKind::SemanticWait {
            change: change.clone(),
        },
        description: "subject semantic wait".into(),
        target_revision: None,
    });
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision).unwrap(),
                "application/json",
                "decisions",
                "StepDecision",
                8,
            )
            .unwrap()
            .object,
    );
}

#[test]
fn s1_commit_advances_branch_head() -> Result<()> {
    use shenko_types::{ids::*, subject::*};
    let (dir, store, agent_id) = initialized_subject();
    let (head, first_step) = store.load_subject(agent_id).unwrap();
    let first_id = first_step.step_id;
    let ledger_id = store.ledger.ledger_id();
    let genesis_json: String = rusqlite::Connection::open(dir.join("head.db"))
        .unwrap()
        .query_row(
            "SELECT data FROM subject_genesis WHERE subject_id=?1",
            [agent_id.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    let genesis: SubjectGenesis = serde_json::from_str(&genesis_json).unwrap();
    assert_eq!(head.committed_at, genesis.created_commit);
    assert_eq!(first_step.expected_parent_commit, genesis.created_commit);
    assert!(genesis.created_commit.seq.0 > 0);
    let second_step_id = EntityId::new();
    let result = recorded_result(&store, &head, first_step)?;
    let step = committed_candidate(&store, result, second_step_id);
    assert!(
        store
            .event_disposition_commit(agent_id, first_id)
            .unwrap()
            .is_none()
    );
    let receipt = store
        .commit_step_and_advance_head(&step, &head, second_step_id)
        .unwrap();
    assert_eq!(receipt.updated.len(), 3);
    assert_eq!(
        receipt
            .updated
            .iter()
            .find(|u| u.entity_id == agent_id)
            .unwrap()
            .record_revision,
        RecordRevision(1)
    );
    drop(store);

    // 真实重开并读回，而不是只检查回执声称写了什么。
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.ledger.ledger_id(), ledger_id);
    assert_eq!(
        reopened
            .event_disposition_commit(agent_id, first_id)
            .unwrap(),
        Some(receipt.commit_ref)
    );
    let (restored, successor) = reopened.load_subject(agent_id).unwrap();
    assert_eq!(restored.branch_id, head.branch_id);
    assert_eq!(restored.subject_root_task_id, head.subject_root_task_id);
    assert_eq!(restored.genesis_ref.genesis_id, head.genesis_ref.genesis_id);
    assert_eq!(restored.genesis_ref.ledger_id, head.genesis_ref.ledger_id);
    assert_eq!(restored.controller_epoch, head.controller_epoch);
    assert_eq!(restored.software_generation, head.software_generation);
    assert_eq!(restored.committed_step_no, 1);
    assert_eq!(restored.committed_step_id, Some(first_id));
    assert_eq!(restored.committed_at, receipt.commit_ref);
    assert_eq!(restored.next_step_id, second_step_id);
    assert_eq!(successor.step_id, second_step_id);
    assert_eq!(successor.step_no, 2);
    assert_eq!(successor.state, SubjectStepState::Planned);
    assert_eq!(successor.expected_parent_commit, receipt.commit_ref);
    assert_eq!(successor.created_by_step_id, Some(first_id));
    let committed = reopened
        .get_step(&head.branch_id, first_id)
        .unwrap()
        .unwrap();
    assert_eq!(committed.state, SubjectStepState::Committed);
    assert_eq!(committed.commit_ref, Some(receipt.commit_ref));
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM subject_steps WHERE branch_id=?1 AND step_no=2",
            [head.branch_id.0.to_string()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let next = reopened.commit(WriteBatch::new()).unwrap();
    assert!(next.commit_ref.seq.0 > receipt.commit_ref.seq.0);
    Ok(())
}

#[test]
fn imported_committed_steps_replay_in_step_order_with_a_one_item_page() -> Result<()> {
    use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
    use shenko_types::ids::EntityId;

    let (dir, source, agent) = initialized_subject();
    let (head, first) = source.load_subject(agent).unwrap();
    // The second UUID sorts before every generated v4 UUID. Ordering only by
    // local import sequence + UUID would deterministically return step 2 first.
    let second_id: EntityId = "00000000-0000-0000-0000-000000000001".parse().unwrap();
    let first_id = first.step_id;
    let result = recorded_result(&source, &head, first)?;
    let first = committed_candidate(&source, result, second_id);
    let first_receipt = source.commit_step_and_advance_head(&first, &head, second_id).unwrap();
    let (head, second) = source.load_subject(agent).unwrap();
    let third_id = EntityId::new();
    let result = recorded_result(&source, &head, second)?;
    let second = committed_candidate(&source, result, third_id);
    let second_receipt = source.commit_step_and_advance_head(&second, &head, third_id).unwrap();
    let mut archive = Vec::new();
    source.write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut archive).unwrap();
    let target = Arc::new(Store::open(dir.join("replay.db"), dir.join("replay-cas"))?);
    let local_agent = initialize_subject_in(&target, b"local replay owner");
    let (local_head, local_step) = target.load_subject(local_agent)?;
    let local_step_id = local_step.step_id;
    let local_next = EntityId::new();
    let local_result = recorded_result(&target, &local_head, local_step)?;
    let local_committed = committed_candidate(&target, local_result, local_next);
    target.commit_step_and_advance_head(&local_committed, &local_head, local_next)?;
    target.import_cognition_archive(&archive[..], &CognitionImportOptions::default()).unwrap();

    assert_eq!(target.get_existing_agent_id()?, Some(local_agent));
    let local = target.committed_decisions_after(local_agent, 0, None, 64)?;
    assert_eq!(local.iter().map(|(_, step, _)| step.step_id).collect::<Vec<_>>(), [local_step_id]);
    assert!(target.committed_decisions_after(EntityId::new(), 0, None, 64)?.is_empty());
    let first_page = target.committed_decisions_after(agent, 0, None, 1)?;
    assert_eq!(first_page.len(), 1);
    let (local_seq, first, _) = &first_page[0];
    assert_eq!(first.step_id, first_id);
    assert_eq!(first.step_no, 1);
    assert_eq!(first.commit_ref, Some(first_receipt.commit_ref));
    let second_page = target.committed_decisions_after(agent, *local_seq, Some(first.step_id), 1)?;
    assert_eq!(second_page.len(), 1);
    let (next_seq, second, _) = &second_page[0];
    assert_eq!(*next_seq, *local_seq);
    assert_eq!(second.step_id, second_id);
    assert_eq!(second.step_no, 2);
    assert_eq!(second.commit_ref, Some(second_receipt.commit_ref));
    assert!(target.committed_decisions_after(agent, *next_seq, Some(second.step_id), 1)?.is_empty());
    Ok(())
}

#[test]
fn imported_owner_inputs_page_through_one_shared_commit_without_loss() -> Result<()> {
    use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
    use shenko_store::web_input::{WEB_INPUT_SOURCE, WebAdmission};
    use shenko_types::inbox::{InboxEvent, InboxEventKind};
    use shenko_types::ids::EntityId;
    use shenko_types::store::RawInboxEvent;

    let (dir, source, agent) = initialized_subject();
    let ids = ["cursor-a", "cursor-\u{e000}", "cursor-\u{10000}"];
    for id in ids {
        let payload = serde_json::json!({
            "schema_version": 2, "type": "submit", "subject_id": agent.to_string(),
            "submission_id": id, "message_id": format!("message-{id}"),
            "mode": "append", "content": format!("原始输入 {id}"), "attachment_refs": [],
        });
        let event = InboxEvent {
            event_id: EntityId::new(), kind: InboxEventKind::Message,
            payload: serde_json::json!({"role": "user", "content": payload["content"], "submission_id": id}),
            created_at: chrono::Utc::now(), source: WEB_INPUT_SOURCE.into(),
        };
        let raw = RawInboxEvent {
            event_id: event.event_id, agent_id: agent, source: event.source.clone(),
            kind: "message".into(), payload_json: serde_json::to_string(&event)?,
            received_at_ms: event.created_at.timestamp_millis(),
        };
        assert!(matches!(source.admit_web_submission(agent, id, &payload.to_string(), &raw)?,
            WebAdmission::Admitted(_)));
    }
    let mut archive = Vec::new();
    source.write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut archive)?;
    let target = Store::open(dir.join("owner-history.db"), dir.join("owner-history-cas"))?;
    target.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    let imported_seq = target.web_submissions_page(agent, None, 10)?;
    assert_eq!(imported_seq.items.len(), 3);
    let seq = imported_seq.items[0].record.accepted_commit.seq.0;
    assert!(imported_seq.items.iter().all(|row| row.record.accepted_commit.seq.0 == seq));
    let mut before = None;
    let mut recovered = Vec::new();
    loop {
        let page = target.web_submissions_page(agent, before.as_ref(), 1)?;
        assert_eq!(page.total, 3);
        recovered.extend(page.items.into_iter().map(|row| row.record.submission_id));
        if !page.has_older { assert_eq!(page.next_before, None); break; }
        before = page.next_before;
    }
    assert_eq!(recovered, ids.into_iter().rev().collect::<Vec<_>>());
    assert_eq!(target.web_submissions_page(EntityId::new(), None, 1)?.total, 0);
    drop(target);
    let reopened = Store::open(dir.join("owner-history.db"), dir.join("owner-history-cas"))?;
    assert_eq!(reopened.web_submissions_page(agent, before.as_ref(), 1)?
        .items[0].record.submission_id, ids[0]);
    Ok(())
}

#[test]
fn memory_receipt_resolves_the_same_raw_call_after_reopen() -> Result<()> {
    use shenko_context_policy::{CallStatus, HistorySource, ItemKind, StoreHistory, estimate_tokens};
    use shenko_types::{ids::*, subject::*};
    let (_dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let result = recorded_result(&store, &head, planned)?;
    let next = EntityId::new();
    let mut step = committed_candidate(&store, result, next);
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&step.decision_ref.as_ref().unwrap().digest).unwrap(),
    ).unwrap();
    let intent_id = EntityId::new();
    let content = store.cas.seal_object(b"remember the actual receipt", "text/plain",
        "memory", "text", 1).unwrap().object;
    decision.memory_intents = vec![MemoryIntent {
        intent_id, kind: MemoryIntentKind::Save, scope: "subject".into(), content_ref: Some(content),
    }];
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", 8).unwrap().object);
    store.commit_step_and_advance_head(&step, &head, next).unwrap();
    let history = StoreHistory::new(store.clone(), EntityId(head.branch_id.0), Arc::new(estimate_tokens));
    let pending = history.read_range(0, u64::MAX).unwrap();
    assert!(pending.iter().any(|item| matches!(&item.kind,
        ItemKind::ToolCall { call_id, status: CallStatus::Pending, .. } if *call_id == intent_id.to_string())));
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.settle_subject_memory().unwrap(), 1);
    drop(history);
    drop(store);
    let reopened = Arc::new(Store::open(_dir.join("head.db"), _dir.join("cas")).unwrap());
    let history = StoreHistory::new(reopened, EntityId(head.branch_id.0), Arc::new(estimate_tokens));
    let settled = history.read_range(0, u64::MAX).unwrap();
    for item in &settled {
        match &item.kind {
            ItemKind::ToolCall { call_id, status, .. } | ItemKind::ToolResult { call_id, status }
                if *call_id == intent_id.to_string() =>
            {
                assert_eq!(*status, CallStatus::Executed { ok: true });
            }
            _ => {}
        }
    }
    assert!(settled.iter().any(|item| matches!(&item.kind,
        ItemKind::ToolResult { call_id, .. } if *call_id == intent_id.to_string())));
    Ok(())
}

/// One unusable subject memory intent does not wedge the whole batch.
///
/// The subject's memory settler owns the `save` operation. A ready intent
/// that names another operation (the shape a legacy database carries, or
/// one written before the commit-time block) is isolated with an honest
/// outcome, while the subject's other memory intents still settle.
#[test]
fn one_unusable_subject_memory_intent_does_not_wedge_the_batch() -> Result<()> {
    use shenko_types::{ids::*, subject::*};
    let (dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let result = recorded_result(&store, &head, planned)?;
    let next = EntityId::new();
    let mut step = committed_candidate(&store, result, next);
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&step.decision_ref.as_ref().unwrap().digest).unwrap(),
    ).unwrap();
    let intent_id = EntityId::new();
    let content = store.cas.seal_object(b"remember the actual receipt", "text/plain",
        "memory", "text", 1).unwrap().object;
    decision.memory_intents = vec![MemoryIntent {
        intent_id, kind: MemoryIntentKind::Save, scope: "subject".into(), content_ref: Some(content),
    }];
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", 8).unwrap().object);
    store.commit_step_and_advance_head(&step, &head, next).unwrap();

    // A ready subject memory intent that names an operation the subject's
    // settler does not own, with the library-side row beside it. It is
    // written directly so it sorts BEFORE the freshly committed intent:
    // the batch must meet the unusable intent first and still settle the
    // rest.
    let unusable = EntityId::new();
    let malformed = EntityId::new();
    let missing = EntityId::new();
    let terminal = EntityId::new();
    let payload = serde_json::to_string(&MemoryIntent {
        intent_id: unusable, kind: MemoryIntentKind::Correct, scope: "subject".into(), content_ref: None,
    })?;
    let decision_ref = serde_json::to_string(step.decision_ref.as_ref().unwrap())?;
    let seq = store.current_commit_seq().unwrap() as i64;
    {
        let connection = rusqlite::Connection::open(dir.join("head.db")).unwrap();
        connection.execute(
            "INSERT INTO subject_intents
                (intent_id,step_id,agent_id,kind,contract,payload,decision_ref,state,owner,revision,created_seq,updated_seq,result_ref)
             VALUES(?1,?2,?3,'memory','shenko.memory/sqlite-save/1',?4,?5,'ready',NULL,0,0,?6,NULL)",
            rusqlite::params![unusable.to_string(), step.step_id.to_string(), agent.to_string(),
                payload, decision_ref, seq],
        ).unwrap();
        connection.execute(
            "INSERT INTO memory_intents
                (intent_id,agent_id,kind,knowledge_kind,scope,content,content_ref,entry_id,state,created_at_ms,updated_at_ms)
             VALUES(?1,?2,'save','note','subject','',NULL,?1,'accepted',?3,?3)",
            rusqlite::params![unusable.to_string(), agent.to_string(), seq],
        ).unwrap();
        // A syntactically valid JSON value with no memory contract must be
        // isolated too, while retaining the payload for the normal reader.
        connection.execute(
            "INSERT INTO subject_intents
                (intent_id,step_id,agent_id,kind,contract,payload,decision_ref,state,revision,created_seq,updated_seq)
             VALUES(?1,?2,?3,'memory','shenko.memory/sqlite-save/1','{}',?4,'ready',0,0,?5)",
            rusqlite::params![malformed.to_string(), step.step_id.to_string(), agent.to_string(), decision_ref, seq],
        ).unwrap();
        // An absent memory row must not disappear behind a JOIN filter.
        let missing_payload = serde_json::to_string(&MemoryIntent {
            intent_id: missing, kind: MemoryIntentKind::Save, scope: "subject".into(),
            content_ref: decision.memory_intents[0].content_ref.clone(),
        })?;
        connection.execute(
            "INSERT INTO subject_intents
                (intent_id,step_id,agent_id,kind,contract,payload,decision_ref,state,revision,created_seq,updated_seq)
             VALUES(?1,?2,?3,'memory','shenko.memory/sqlite-save/1',?4,?5,'ready',0,0,?6)",
            rusqlite::params![missing.to_string(), step.step_id.to_string(), agent.to_string(), missing_payload, decision_ref, seq],
        ).unwrap();
        let terminal_payload = serde_json::to_string(&MemoryIntent {
            intent_id: terminal, kind: MemoryIntentKind::Save, scope: "subject".into(),
            content_ref: decision.memory_intents[0].content_ref.clone(),
        })?;
        connection.execute(
            "INSERT INTO subject_intents
                (intent_id,step_id,agent_id,kind,contract,payload,decision_ref,state,revision,created_seq,updated_seq)
             VALUES(?1,?2,?3,'memory','shenko.memory/sqlite-save/1',?4,?5,'ready',0,0,?6)",
            rusqlite::params![terminal.to_string(), step.step_id.to_string(), agent.to_string(), terminal_payload, decision_ref, seq],
        ).unwrap();
        connection.execute(
            "INSERT INTO memory_intents
                (intent_id,agent_id,kind,knowledge_kind,scope,content,entry_id,state,reason,created_at_ms,updated_at_ms)
             VALUES(?1,?2,'save','note','subject','retained terminal input',?1,'failed','original failure',?3,?3)",
            rusqlite::params![terminal.to_string(), agent.to_string(), seq],
        ).unwrap();
    }

    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.settle_subject_memory().unwrap(), 1,
        "the usable intent settles even though the batch holds an unusable one");

    // The usable intent really settled: it owns its entry.
    let entry = store.memory_entry(intent_id).unwrap()
        .expect("the settled intent owns the entry it produced");
    assert_eq!(entry.content, "remember the actual receipt");
    let committed = store.committed_intents(Some("memory"), None, 10).unwrap();
    let settled = committed.iter().find(|intent| intent.intent_id == intent_id).unwrap();
    assert_eq!(settled.state, "settled");

    // The unusable intent is isolated with an honest outcome: it is not
    // ready any more, it says which operation the settler owns, and its
    // library row is failed instead of accepted forever.
    let isolated = committed.iter().find(|intent| intent.intent_id == unusable).unwrap();
    assert_eq!(isolated.state, "blocked");
    assert!(isolated.result_ref.as_deref().unwrap_or_default().contains("save"),
        "the isolated intent says what the settler owns: {:?}", isolated.result_ref);
    assert_eq!(store.memory_intent_state(unusable).unwrap().as_deref(), Some("failed"));
    assert_eq!(store.committed_intent(malformed)?.unwrap().state, "blocked");
    assert_eq!(store.committed_intent(malformed)?.unwrap().payload, serde_json::json!({}));
    assert!(store.committed_intent(missing)?.unwrap().result_ref.unwrap().contains("request binding"));
    assert!(store.memory_intent(missing)?.is_none());
    let retained = store.memory_intent(terminal)?.unwrap().record;
    assert_eq!(retained.state, "failed");
    assert_eq!(retained.reason.as_deref(), Some("original failure"));
    assert_eq!(retained.content, "retained terminal input");
    assert!(store.memory_entry(unusable)?.is_none());
    assert!(store.memory_entry(malformed)?.is_none());
    assert!(store.memory_entry(terminal)?.is_none());

    // The batch is not wedged: the next pass has nothing left to settle.
    assert_eq!(store.settle_subject_memory().unwrap(), 0);
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    assert_eq!(reopened.committed_intent(unusable)?.unwrap().state, "blocked");
    assert_eq!(reopened.committed_intent(missing)?.unwrap().state, "blocked");
    assert_eq!(reopened.memory_intent_state(intent_id)?.as_deref(), Some("committed"));
    let history = reopened.session_events_range(EntityId(head.branch_id.0), 0, i64::MAX)?;
    assert!(history.iter().any(|row| row.data.contains(&unusable.to_string()) && row.data.contains("failed")));
    Ok(())
}

#[test]
fn competing_s1_rejects_stale_writer_without_partial_commit() -> Result<()> {
    use shenko_types::{error::ShenkoError, ids::*};
    use std::sync::{Arc, Barrier};
    let (dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let result = recorded_result(&store, &head, planned)?;
    let next_a = EntityId::new();
    let next_b = EntityId::new();
    let step_a = committed_candidate(&store, result.clone(), next_a);
    let step_b = committed_candidate(&store, result, next_b);
    // 同一竞争前提下，错 parent 和陈旧 epoch 都不能占用提交槽。
    let mut wrong_parent = step_a.clone();
    wrong_parent.expected_parent_commit.seq = LedgerSeq(0);
    assert!(matches!(
        store.commit_step_and_advance_head(&wrong_parent, &head, next_a),
        Err(ShenkoError::Conflict(_))
    ));
    let mut stale_head = head.clone();
    stale_head.controller_epoch = ControllerEpoch(head.controller_epoch.0 + 1);
    assert!(matches!(
        store.commit_step_and_advance_head(&step_a, &stale_head, next_a),
        Err(ShenkoError::Conflict(_))
    ));
    let other = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let barrier_a = barrier.clone();
    let head_a = head.clone();
    let a = std::thread::spawn(move || {
        barrier_a.wait();
        store.commit_step_and_advance_head(&step_a, &head_a, next_a)
    });
    let b = std::thread::spawn(move || {
        barrier.wait();
        other.commit_step_and_advance_head(&step_b, &head, next_b)
    });
    let a = a.join().unwrap();
    let b = b.join().unwrap();
    assert_ne!(a.is_ok(), b.is_ok());
    let (winner, loser) = if a.is_ok() {
        (next_a, next_b)
    } else {
        (next_b, next_a)
    };
    assert!(matches!(
        a.as_ref().err().or(b.as_ref().err()).unwrap(),
        ShenkoError::Conflict(_)
    ));
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    let (head, successor) = reopened.load_subject(agent).unwrap();
    assert_eq!(head.next_step_id, winner);
    assert_eq!(successor.step_id, winner);
    assert!(reopened.get_step(&head.branch_id, loser).unwrap().is_none());
    let after: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before + 1);
    let steps: i64 = conn
        .query_row("SELECT count(*) FROM subject_steps", [], |r| r.get(0))
        .unwrap();
    assert_eq!(steps, 2);
    let committed = reopened
        .get_step(&head.branch_id, head.committed_step_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(committed.commit_ref, Some(head.committed_at));
    Ok(())
}

#[test]
fn semantic_wait_is_durable_and_a_stale_change_settles_without_failing_s1() -> Result<()> {
    use shenko_types::{ids::*, subject::*};
    let (dir, store, agent) = initialized_subject();
    let (head, planned) = store.load_subject(agent).unwrap();
    let wait_id = EntityId::new();
    let next = EntityId::new();
    let condition = "实验出现足以判断的结果".to_string();

    // S1 #1 — the subject starts a semantic wait.
    let result = recorded_result_with_wait(&store, &head, planned, None)?;
    let mut step = committed_candidate(&store, result, next);
    attach_wait(
        &mut step,
        &store,
        &SemanticWaitChange::Start {
            condition: condition.clone(),
            max_wait_secs: 3,
        },
        wait_id,
    );
    store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap();

    let wait = store.semantic_wait(agent).unwrap().expect("durable wait");
    assert_eq!(wait.wait_id, wait_id);
    assert_eq!(wait.condition, condition);
    assert!(matches!(wait.status, SemanticWaitStatus::Active));
    // The store owns the deadline: it is fixed once, when the wait starts.
    assert_eq!(
        wait.deadline_at,
        wait.started_at + chrono::TimeDelta::seconds(3)
    );
    let receipt = store
        .semantic_wait_receipt(step.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(receipt.outcome, SemanticWaitOutcome::Started));
    // The handled wait intent is settled with that receipt, never blocked.
    let intents = store.committed_intents(Some("policy"), None, 16).unwrap();
    let settled = intents
        .iter()
        .find(|intent| intent.intent_id == wait_id)
        .expect("wait policy intent");
    assert_eq!(settled.state, "settled");
    assert_eq!(settled.contract, "shenko.subject/semantic-wait/1");

    // S1 #2 — a new wait id with the same condition cannot renew the wait.
    let (head2, second) = store.load_subject(agent).unwrap();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    // The observed wall-clock high water is durable before the next S0.
    let observed = store.observe_semantic_wait(&head2).unwrap().unwrap();
    assert!(matches!(observed.status, SemanticWaitStatus::Active));
    assert!(observed.last_observed_at >= wait.last_observed_at);
    let after_observe: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert!(after_observe > before);
    assert_eq!(store.load_subject(agent).unwrap().0.record_revision, head2.record_revision);

    let result2 = recorded_result_with_wait(&store, &head2, second, Some(observed))?;
    let next2 = EntityId::new();
    let mut step2 = committed_candidate(&store, result2, next2);
    attach_wait(
        &mut step2,
        &store,
        &SemanticWaitChange::Start {
            condition: condition.clone(),
            max_wait_secs: 3,
        },
        EntityId::new(),
    );
    // The stale wait change does not fail the whole S1: the step and its
    // unique successor are still committed.
    store
        .commit_step_and_advance_head(&step2, &head2, next2)
        .unwrap();
    let receipt2 = store
        .semantic_wait_receipt(step2.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(
        receipt2.outcome,
        SemanticWaitOutcome::Rejected { .. }
    ));
    let wait2 = store.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(wait2.wait_id, wait_id);
    // The capped deadline of the running wait was not renewed.
    assert_eq!(wait2.deadline_at, wait.deadline_at);
    assert_eq!(wait2.started_at, wait.started_at);
    let (head3, _) = store.load_subject(agent).unwrap();
    assert_eq!(head3.next_step_id, next2);

    // S1 #3 — after the deadline a continue expires the wait instead of
    // reviving it.
    std::thread::sleep(std::time::Duration::from_millis(3100));
    let (head4, third) = store.load_subject(agent).unwrap();
    let result3 = recorded_result_with_wait(&store, &head4, third, Some(wait2.clone()))?;
    let next3 = EntityId::new();
    let mut step3 = committed_candidate(&store, result3, next3);
    attach_wait(
        &mut step3,
        &store,
        &SemanticWaitChange::Continue { wait_id },
        EntityId::new(),
    );
    store
        .commit_step_and_advance_head(&step3, &head4, next3)
        .unwrap();
    let receipt3 = store
        .semantic_wait_receipt(step3.step_id)
        .unwrap()
        .expect("exact receipt");
    assert!(matches!(
        receipt3.outcome,
        SemanticWaitOutcome::Rejected { .. }
    ));
    let expired = store.semantic_wait(agent).unwrap().unwrap();
    assert!(matches!(expired.status, SemanticWaitStatus::Expired { .. }));
    assert_eq!(expired.deadline_at, wait.deadline_at);
    drop(store);

    // Reopening keeps the original deadline and the terminal fact: a finished
    // wait is never resurrected and its deadline is never reset.
    let reopened = Arc::new(Store::open(dir.join("head.db"), dir.join("cas")).unwrap());
    let restored = reopened.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(restored.wait_id, wait_id);
    assert_eq!(restored.started_at, wait.started_at);
    assert_eq!(restored.deadline_at, wait.deadline_at);
    assert!(matches!(restored.status, SemanticWaitStatus::Expired { .. }));
    // Observing a finished wait changes nothing, commits nothing and leaves
    // the branch head exactly as the subject left it.
    let (head5, _) = reopened.load_subject(agent).unwrap();
    let before: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    let observed = reopened.observe_semantic_wait(&head5).unwrap().unwrap();
    assert!(matches!(observed.status, SemanticWaitStatus::Expired { .. }));
    let after: i64 = conn
        .query_row("SELECT count(*) FROM commit_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(after, before);
    let (head6, _) = reopened.load_subject(agent).unwrap();
    assert_eq!(head6.record_revision, head5.record_revision);

    // A fresh id must not turn the just-expired identical condition into an
    // automatic renewal, even when it is proposed in a later committed step.
    let (head, planned) = reopened.load_subject(agent).unwrap();
    let result = recorded_result_with_wait(&reopened, &head, planned, Some(restored.clone()))?;
    let next = EntityId::new();
    let mut repeated = committed_candidate(&reopened, result, next);
    attach_wait(&mut repeated, &reopened, &SemanticWaitChange::Start {
        condition: condition.clone(), max_wait_secs: 120,
    }, EntityId::new());
    reopened.commit_step_and_advance_head(&repeated, &head, next).unwrap();
    assert!(matches!(reopened.semantic_wait_receipt(repeated.step_id).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Rejected { .. }));
    assert_eq!(reopened.semantic_wait(agent).unwrap().unwrap().wait_id, wait_id);

    // A different new decision may start a distinct wait. A cognition archive
    // must carry both wait records and choose the later one on the receiver,
    // even though all imported rows receive the same local import sequence.
    let (head, planned) = reopened.load_subject(agent).unwrap();
    let result = recorded_result_with_wait(&reopened, &head, planned, Some(restored))?;
    let next = EntityId::new();
    let mut distinct = committed_candidate(&reopened, result, next);
    let next_wait_id = EntityId::new();
    attach_wait(&mut distinct, &reopened, &SemanticWaitChange::Start {
        condition: "收到新的独立观察".into(), max_wait_secs: 120,
    }, next_wait_id);
    reopened.commit_step_and_advance_head(&distinct, &head, next).unwrap();
    let before_migration = reopened.semantic_wait(agent).unwrap().unwrap();
    assert_eq!(before_migration.wait_id, next_wait_id);
    let mut archive = Vec::new();
    reopened.write_cognition_archive(
        &shenko_store::cognition::CognitionSnapshotOptions::default(), None, &mut archive,
    ).unwrap();
    let receiver = Store::open(dir.join("wait-receiver.db"), dir.join("wait-receiver-cas")).unwrap();
    receiver.import_cognition_archive(
        &archive[..], &shenko_store::cognition::CognitionImportOptions::default(),
    ).unwrap();
    let migrated = receiver.semantic_wait(agent).unwrap().expect("wait imported");
    assert_eq!(migrated.wait_id, next_wait_id);
    assert_eq!(migrated.started_at, before_migration.started_at);
    assert_eq!(migrated.deadline_at, before_migration.deadline_at);
    assert!(matches!(receiver.semantic_wait_receipt(distinct.step_id).unwrap().unwrap().outcome,
        SemanticWaitOutcome::Started));
    Ok(())
}

#[test]
fn task_roundtrip() {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::{Digest, ObjectRef, SchemaRef};
    use shenko_types::task::{
        DispatchEligibility, ImplementationRef, TaskOwner, TaskRecord, TaskState,
    };

    let dir = tmp_dir();
    let db = dir.join("task.db");
    let cas = dir.join("cas");
    let store = Store::open(db, cas).unwrap();

    let agent_id = EntityId::new();
    let conv_id = EntityId::new();
    let task_id = EntityId::new();
    let commit_ref = CommitRef::new(store.ledger.ledger_id(), shenko_types::ids::LedgerSeq(1));

    let codec = SchemaRef {
        name: "raw".into(),
        major: 1,
        digest: Digest::blake3(b"raw"),
    };
    let input_ref = ObjectRef {
        digest: Digest::blake3(b"task input"),
        bytes: 10,
        media_type: "text/plain".into(),
        store_namespace: "test".into(),
        locator: "file:///test".into(),
        codec,
    };

    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id,
        conversation_id: conv_id,
        owner: TaskOwner::Conversation { id: conv_id },
        background: false,
        kind: "test".into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: EntityId::new(),
            artifact_digest: Digest::blake3(b"plugin"),
            registry_revision: 1,
        },
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: Default::default(),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: commit_ref,
        updated_commit: commit_ref,
    };

    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task);
    let receipt = store.commit(batch).unwrap();

    assert!(!receipt.updated.is_empty());
    assert_eq!(receipt.updated[0].entity_id, task_id);
}

fn recovery_task(
    store: &Store,
    agent: shenko_types::ids::EntityId,
    number: u64,
    running: bool,
) -> shenko_types::task::TaskRecord {
    use shenko_types::{commit::CommitRef, ids::*, task::*};
    let input = store.cas.seal_object(b"recovery checkpoint", "text/plain", "test", "raw", 1)
        .unwrap().object;
    let task_id = format!("00000000-0000-0000-0000-{number:012x}").parse().unwrap();
    let commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    TaskRecord {
        record_schema_version: 1, task_id, task_revision: TaskRevision(3),
        record_revision: RecordRevision(5), agent_id: agent, conversation_id: agent,
        owner: TaskOwner::Conversation { id: agent }, background: false,
        kind: "host.printf".into(), definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: agent, artifact_digest: input.digest.clone(), registry_revision: 1,
        },
        input_ref: input.clone(),
        state: if running {
            TaskState::Running { checkpoint_ref: Some(input), invocation_id: EntityId::new(),
                mode: RunMode::Run, epoch: ControllerEpoch(7) }
        } else { TaskState::Pending { checkpoint_ref: Some(input) } },
        abort_request: None,
        memos: serde_json::Map::from_iter([("native_child".into(), serde_json::json!(true))]),
        residual_case_ids: vec![], created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready, no_progress_crashes: 0,
        first_run_at: None, terminal_at: None, created_commit: commit, updated_commit: commit,
    }
}

#[test]
fn child_tool_creation_claim_and_reopen_keep_one_identity() {
    use shenko_types::{ids::*, task::*};
    let (dir, store, agent) = initialized_subject();
    let parent = claimed_native_task(&store, agent).task;
    let id = EntityId::new();
    let implementation = parent.implementation_ref.clone();
    let claim = store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation.clone(),
    ).unwrap().unwrap();
    assert_eq!(claim.task.owner, TaskOwner::Task { id: parent.task_id });
    assert_eq!(claim.task.record_revision, RecordRevision(1));
    assert!(matches!(claim.task.state, TaskState::Running { invocation_id, mode: RunMode::Run, .. }
        if invocation_id == claim.invocation.invocation_id));
    assert_eq!(claim.task.created_commit, claim.task.updated_commit);
    let commit = store.current_commit_seq().unwrap();
    assert!(store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation.clone(),
    ).unwrap().is_none());
    assert_eq!(store.current_commit_seq().unwrap(), commit);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert!(reopened.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), implementation,
    ).unwrap().is_none());
    let retained = reopened.external_task(id).unwrap();
    assert_eq!(retained.state, claim.task.state);
    assert_eq!(retained.memos, claim.task.memos);
    assert_eq!(retained.updated_commit, claim.task.updated_commit);
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
}

#[test]
fn failed_child_tool_claim_rolls_back_its_creation() {
    use shenko_types::ids::*;
    let (dir, store, agent) = initialized_subject();
    let parent = claimed_native_task(&store, agent).task;
    let id = EntityId::new();
    let connection = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    connection.execute_batch(&format!(
        "CREATE TRIGGER fail_tool_claim BEFORE UPDATE ON tasks WHEN NEW.task_id='{id}'
         BEGIN SELECT RAISE(ABORT,'injected claim failure'); END;"
    )).unwrap();
    let before = store.current_commit_seq().unwrap();
    let error = store.claim_child_tool_task(
        &parent, id, "host.printf", 1, parent.input_ref.clone(), parent.implementation_ref.clone(),
    ).unwrap_err();
    assert!(format!("{error:#}").contains("injected claim failure"));
    assert!(store.get_task(id).unwrap().is_none());
    assert_eq!(store.current_commit_seq().unwrap(), before);
    assert_eq!(store.external_task(parent.task_id).unwrap().state, parent.state);
}

fn claimed_native_task(
    store: &Store, agent: shenko_types::ids::EntityId,
) -> shenko_store::child::NativeTaskClaim {
    let mut task = recovery_task(store, agent, 1, false);
    task.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object"));
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(task.clone());
    store.commit(batch).unwrap();
    store.start_subject_dispatcher().unwrap();
    store.claim_native_task(task.task_id, task.implementation_ref, false)
        .unwrap().expect("the pending native task has an admitted input")
}

#[test]
fn peer_dialogue_fences_fresh_invocations_but_replays_recorded_outcomes() -> Result<()> {
    use shenko_store::child::PeerMessage;
    use shenko_types::{ids::*, task::*};
    let (dir, store, agent) = initialized_subject();
    let claim = claimed_native_task(&store, agent);
    let recipient = recovery_task(&store, agent, 2, false);
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(recipient.clone());
    store.commit(batch)?;
    let message = PeerMessage {
        message_id: EntityId::new(), sender_task_id: claim.task.task_id,
        sender_name: "sender".into(), sender_operation_id: claim.operation_id,
        step_index: 0, to: "peer".into(), recipient_task_id: Some(recipient.task_id),
        unresolved_reason: None, message: "sealed peer observation".into(),
    };
    let sent = store.deliver_peer_dialogue(&claim.invocation, &message)?;
    assert_eq!(sent.state, "delivered");
    // Mutable record revisions advance at delivery. They do not replace the
    // invocation that still owns a second message from the same sealed step.
    let second = PeerMessage { message_id: EntityId::new(), ..message.clone() };
    store.deliver_peer_dialogue(&claim.invocation, &second)?;
    let fresh = PeerMessage { message_id: EntityId::new(), ..message.clone() };
    let mut stale = vec![claim.invocation.clone(); 5];
    stale[0].invocation_id = EntityId::new();
    stale[1].task_revision = stale[1].task_revision.next();
    stale[2].epoch = ControllerEpoch(claim.invocation.epoch.0 + 1);
    stale[3].software_generation = RuntimeGeneration("stale-peer-generation".into());
    stale[4].task_id = recipient.task_id;
    let before = store.current_commit_seq()?;
    for invocation in stale {
        assert!(store.deliver_peer_dialogue(&invocation, &fresh).is_err());
    }
    assert_eq!(store.current_commit_seq()?, before);
    assert_eq!(store.peer_outbox(claim.task.task_id)?.len(), 2);
    assert_eq!(store.pending_child_dialogue(recipient.task_id)?.len(), 2);

    let current = store.external_task(claim.task.task_id)?;
    assert!(store.park_native_task(current.task_id, current.record_revision,
        "reconciling", "replace the original worker", false)?);
    let replacement = store.claim_native_task(current.task_id, current.implementation_ref, false)?
        .context("a replacement invocation owns the still-unfinished run")?;
    assert_ne!(replacement.invocation.invocation_id, claim.invocation.invocation_id);
    let before = store.current_commit_seq()?;
    assert!(store.deliver_peer_dialogue(&claim.invocation, &fresh).is_err());
    assert_eq!(store.current_commit_seq()?, before);
    store.deliver_peer_dialogue(&replacement.invocation, &fresh)?;
    assert_eq!(store.pending_child_dialogue(recipient.task_id)?.len(), 3);
    store.request_native_cancellation(claim.task.task_id, EntityId::new(), EntityId::new(),
        "cancel further peer sends", AbortScope::Task)?;
    let next = PeerMessage { message_id: EntityId::new(), ..message.clone() };
    let before = store.current_commit_seq()?;
    assert!(store.deliver_peer_dialogue(&replacement.invocation, &next).is_err());
    assert_eq!(store.deliver_peer_dialogue(&claim.invocation, &message)?, sent);
    assert_eq!(store.current_commit_seq()?, before);
    let ids: Vec<_> = store.pending_child_dialogue(recipient.task_id)?
        .into_iter().map(|turn| turn.intent_id).collect();
    assert_eq!(store.consume_child_dialogue(recipient.task_id, &ids)?, 3);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    // No new dispatcher has been installed. Reading the original outcome
    // needs neither a current dispatcher nor the cancelled invocation.
    let before = reopened.current_commit_seq()?;
    assert_eq!(reopened.deliver_peer_dialogue(&claim.invocation, &message)?, sent);
    assert!(reopened.deliver_peer_dialogue(&replacement.invocation, &next).is_err());
    reopened.close_external_writer();
    assert_eq!(reopened.deliver_peer_dialogue(&claim.invocation, &message)?, sent);
    assert_eq!(reopened.current_commit_seq()?, before);
    assert!(reopened.pending_child_dialogue(recipient.task_id)?.is_empty());
    assert_eq!(reopened.peer_outbox(claim.task.task_id)?.len(), 3);
    Ok(())
}

#[test]
fn peer_dialogue_bounds_inbox_bodies_and_preserves_old_outcomes() -> Result<()> {
    use shenko_store::child::{MAX_CHILD_DIALOGUE_BYTES, PeerMessage};
    use shenko_types::ids::EntityId;
    let (dir, store, agent) = initialized_subject();
    let claim = claimed_native_task(&store, agent);
    let recipient = recovery_task(&store, agent, 2, false);
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(recipient.clone());
    store.commit(batch)?;
    let bounded = format!("{}a", "界".repeat(MAX_CHILD_DIALOGUE_BYTES / 3));
    assert_eq!(bounded.len(), MAX_CHILD_DIALOGUE_BYTES);
    let accepted = PeerMessage {
        message_id: EntityId::new(), sender_task_id: claim.task.task_id,
        sender_name: "sender".into(), sender_operation_id: claim.operation_id,
        step_index: 0, to: "peer".into(), recipient_task_id: Some(recipient.task_id),
        unresolved_reason: None, message: bounded,
    };
    let sent = store.deliver_peer_dialogue(&claim.invocation, &accepted)?;
    assert_eq!(sent.state, "delivered");
    let recipient_before = store.external_task(recipient.task_id)?;
    let oversized = PeerMessage {
        message_id: EntityId::new(), message: format!("{}界", accepted.message), ..accepted.clone()
    };
    let rejected = store.deliver_peer_dialogue(&claim.invocation, &oversized)?;
    assert_eq!((rejected.state.as_str(), rejected.reason.as_deref()),
        ("undelivered", Some("message_too_large")));
    let recipient_after = store.external_task(recipient.task_id)?;
    assert_eq!(recipient_after.record_revision, recipient_before.record_revision);
    assert_eq!(recipient_after.memos, recipient_before.memos);
    assert_eq!(recipient_after.updated_commit, recipient_before.updated_commit);
    let inbox = store.pending_child_dialogue(recipient.task_id)?;
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message, accepted.message);
    // The same sealed identities return their original outcomes without a
    // second recipient write, including the oversized historical tell.
    let before = store.current_commit_seq()?;
    assert_eq!(store.deliver_peer_dialogue(&claim.invocation, &accepted)?, sent);
    assert_eq!(store.deliver_peer_dialogue(&claim.invocation, &oversized)?, rejected);
    assert_eq!(store.current_commit_seq()?, before);
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    let before = reopened.current_commit_seq()?;
    assert_eq!(reopened.deliver_peer_dialogue(&claim.invocation, &oversized)?, rejected);
    assert_eq!(reopened.peer_outbox(claim.task.task_id)?.len(), 2);
    assert_eq!(reopened.pending_child_dialogue(recipient.task_id)?, inbox);
    assert_eq!(reopened.current_commit_seq()?, before);
    Ok(())
}

fn arm_dispatch_probe(
    store: &Store, invocation: &shenko_types::task::TaskInvocation, key: &str,
) -> (shenko_types::ids::EntityId, shenko_types::ids::EntityId) {
    arm_dispatch_operation(store, invocation, key, shenko_types::ids::EntityId::new())
}

fn arm_dispatch_operation(
    store: &Store, invocation: &shenko_types::task::TaskInvocation, key: &str,
    operation_id: shenko_types::ids::EntityId,
) -> (shenko_types::ids::EntityId, shenko_types::ids::EntityId) {
    let (current, mut operation, mut attempt) = draft_dispatch_operation(store, invocation, key, operation_id).unwrap();
    assert!(store.arm_external_operation(&current, &mut operation, &mut attempt).unwrap().is_none());
    (operation.operation_id, attempt.attempt_id)
}


#[test]
fn native_dispatch_is_once_per_attempt_across_steps_cancellation_and_reopen() {
    use shenko_store::child::NativeDispatchTransition::{AlreadyCommitted, Fenced, Won};
    use shenko_types::{ids::*, operation::AttemptDispatchStage, task::*};
    let (dir, store, agent) = initialized_subject();
    let ctx = claimed_native_task(&store, agent).invocation;
    let first = arm_dispatch_probe(&store, &ctx, "step/0");
    let second = arm_dispatch_probe(&store, &ctx, "step/1");
    let cancelled = arm_dispatch_probe(&store, &ctx, "step/2");
    let before = store.current_commit_seq().unwrap();
    assert!(store.commit_native_dispatch(&ctx, first.0, second.1).is_err());
    let mut foreign = ctx.clone();
    foreign.invocation_id = EntityId::new();
    assert!(store.commit_native_dispatch(&foreign, first.0, first.1).is_err());
    assert_eq!(store.current_commit_seq().unwrap(), before);

    let barrier = Arc::new(std::sync::Barrier::new(2));
    let racers: Vec<_> = (0..2).map(|_| {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            barrier.wait();
            store.commit_native_dispatch(&ctx, first.0, first.1).unwrap()
        })
    }).collect();
    let results: Vec<_> = racers.into_iter().map(|racer| racer.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| **result == Won).count(), 1);
    assert_eq!(results.iter().filter(|result| **result == AlreadyCommitted).count(), 1);
    // A matching historical marker must not hide a different attempt epoch.
    foreign.invocation_id = ctx.invocation_id;
    foreign.epoch = ControllerEpoch(ctx.epoch.0 + 1);
    assert!(store.commit_native_dispatch(&foreign, first.0, first.1).is_err());
    assert_eq!(store.commit_native_dispatch(&ctx, second.0, second.1).unwrap(), Won);
    // A send-boundary commit is not a fabricated provider acknowledgement.
    assert_eq!(store.external_attempt(first.1).unwrap().dispatch_stage, AttemptDispatchStage::Armed);
    store.request_native_cancellation(ctx.task_id, EntityId::new(), EntityId::new(),
        "cancel remaining step", AbortScope::Task).unwrap();
    let before = store.current_commit_seq().unwrap();
    assert_eq!(store.commit_native_dispatch(&ctx, first.0, first.1).unwrap(), AlreadyCommitted);
    assert_eq!(store.commit_native_dispatch(&ctx, cancelled.0, cancelled.1).unwrap(), Fenced);
    assert_eq!(store.current_commit_seq().unwrap(), before);
    drop(store);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert_eq!(reopened.commit_native_dispatch(&ctx, first.0, first.1).unwrap(), AlreadyCommitted);
    assert_eq!(reopened.commit_native_dispatch(&ctx, second.0, second.1).unwrap(), AlreadyCommitted);
    assert_eq!(reopened.commit_native_dispatch(&ctx, cancelled.0, cancelled.1).unwrap(), Fenced);
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
}

#[test]
fn legacy_unattributed_dispatch_never_authorizes_another_attempt() {
    use shenko_store::child::{NATIVE_DISPATCH, NativeDispatchTransition::AlreadyCommitted};
    let (dir, store, agent) = initialized_subject();
    let ctx = claimed_native_task(&store, agent).invocation;
    let first = arm_dispatch_probe(&store, &ctx, "step/0");
    let second = arm_dispatch_probe(&store, &ctx, "step/1");
    let mut task = store.external_task(ctx.task_id).unwrap();
    let marker = serde_json::json!({"task_revision": ctx.task_revision,
        "invocation_id": ctx.invocation_id, "commit": task.updated_commit});
    task.memos.insert(NATIVE_DISPATCH.into(), marker.clone());
    rusqlite::Connection::open(dir.join("head.db")).unwrap().execute(
        "UPDATE tasks SET data=?1 WHERE task_id=?2",
        rusqlite::params![serde_json::to_string(&task).unwrap(), task.task_id.to_string()],
    ).unwrap();
    let before = store.current_commit_seq().unwrap();
    for (operation, attempt) in [first, second] {
        assert_eq!(store.commit_native_dispatch(&ctx, operation, attempt).unwrap(), AlreadyCommitted);
    }
    assert_eq!(store.current_commit_seq().unwrap(), before);
    assert_eq!(store.external_task(ctx.task_id).unwrap().memos.get(NATIVE_DISPATCH), Some(&marker));
}

#[test]
fn native_recovery_scans_past_pending_tasks_and_preserves_checkpoint() -> Result<()> {
    use shenko_types::{ids::*, task::*};
    let (dir, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=256 { batch.task_upserts.push(recovery_task(&store, agent, number, false)); }
    let mut running = recovery_task(&store, agent, 257, true);
    running.abort_request = Some(AbortRequest {
        cancel_id: EntityId::new(), decision_ref: EntityId::new(),
        requested_commit: running.created_commit, reason: "cancel before crash".into(), scope: AbortScope::Task,
    });
    let checkpoint = match &running.state {
        TaskState::Running { checkpoint_ref, .. } => checkpoint_ref.clone(), _ => unreachable!(),
    };
    batch.task_upserts.push(running.clone());
    store.commit(batch).unwrap();
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(reopened.recover_native_tasks().unwrap(), 1);
    let recovered = reopened.external_task(running.task_id).unwrap();
    assert_eq!(recovered.state, TaskState::Pending { checkpoint_ref: checkpoint });
    assert_eq!(recovered.abort_request, running.abort_request);
    assert_eq!(recovered.task_revision, running.task_revision);
    assert_eq!(recovered.record_revision, RecordRevision(6));
    assert_eq!(recovered.memos.get("native_recovery"), Some(&serde_json::json!(true)));
    assert!(matches!(recovered.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    assert_eq!(reopened.recover_native_tasks().unwrap(), 0);
    assert_eq!(reopened.external_task("00000000-0000-0000-0000-000000000001".parse().unwrap())
        .unwrap().record_revision, RecordRevision(5));

    // The same cancellation also fences a still-running descendant. Recovery
    // preserves its original checkpoint as parked cleanup instead of leaving
    // it forever running or granting a fresh invocation under an inactive owner.
    let mut child = recovery_task(&reopened, agent, 258, true);
    child.owner = TaskOwner::Task { id: running.task_id };
    child.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object"));
    let child_checkpoint = match &child.state {
        TaskState::Running { checkpoint_ref, .. } => checkpoint_ref.clone(),
        _ => unreachable!(),
    };
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(child.clone());
    reopened.commit(batch)?;
    assert_eq!(reopened.recover_native_tasks()?, 1);
    let cleaned = reopened.external_task(child.task_id)?;
    assert_eq!(cleaned.state, TaskState::Pending { checkpoint_ref: child_checkpoint });
    assert_eq!(cleaned.abort_request, child.abort_request);
    assert_eq!(cleaned.task_revision, child.task_revision);
    assert_eq!(cleaned.record_revision, child.record_revision.next());
    assert!(matches!(cleaned.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    assert_eq!(reopened.external_task(running.task_id)?.abort_request, running.abort_request);
    let before = reopened.current_commit_seq()?;
    assert!(reopened.claim_native_task(child.task_id, child.implementation_ref, false)?.is_none());
    assert_eq!(reopened.current_commit_seq()?, before);
    assert_eq!(reopened.recover_native_tasks()?, 0);
    Ok(())
}

#[test]
fn native_recovery_recovers_more_than_one_page_of_running_work() {
    use shenko_types::task::TaskState;
    let (_, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=257 { batch.task_upserts.push(recovery_task(&store, agent, number, true)); }
    store.commit(batch).unwrap();
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.recover_native_tasks().unwrap(), 257);
    for number in 1..=257 {
        let id = format!("00000000-0000-0000-0000-{number:012x}").parse().unwrap();
        let task = store.external_task(id).unwrap();
        assert!(matches!(task.state, TaskState::Pending { checkpoint_ref: Some(_) }));
        assert_eq!(task.memos.get("native_recovery"), Some(&serde_json::json!(true)));
    }
    assert_eq!(store.recover_native_tasks().unwrap(), 0);
}

#[test]
fn dispatch_cursor_reaches_later_work_and_wraps_without_starving() {
    let (_, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=40 { batch.task_upserts.push(recovery_task(&store, agent, number, false)); }
    store.commit(batch).unwrap();
    let mut seen = Vec::new();
    let mut after: Option<(String, shenko_types::ids::EntityId)> = None;
    let mut pages = 0;
    let last_key = loop {
        let page = store.native_task_candidates_after(
            after.as_ref().map(|(created, id)| (created.as_str(), *id)), 32,
        ).unwrap();
        pages += 1;
        assert!(pages <= 3, "scan did not terminate");
        seen.extend(page.iter().map(|(_, task)| task.task_id.to_string()));
        let page_end = page.last().map(|(created, task)| (created.clone(), task.task_id));
        if page.len() < 32 {
            // Short page: the scan reached the end and the next pass restarts.
            break page_end;
        }
        after = page_end;
    };
    assert_eq!(seen.len(), 40, "a later page hid work: {seen:?}");
    assert!(seen.windows(2).all(|pair| pair[0] < pair[1]), "pagination duplicated or reordered work");
    let upper = store.native_task_scan_upper().unwrap().unwrap();
    assert_eq!(Some(upper), last_key, "the round boundary is the last scanned key");
    // Wraparound returns the same complete set rather than staying at the end.
    let restarted = store.native_task_candidates_after(None, 32).unwrap();
    assert_eq!(restarted.len(), 32);
    assert_eq!(restarted[0].1.task_id.to_string(), seen[0]);
}

#[test]
fn native_recovery_rolls_back_all_pages_when_later_identity_is_corrupt() {
    use shenko_types::{ids::*, task::TaskState};
    let (dir, store, agent) = initialized_subject();
    let mut batch = WriteBatch::new();
    for number in 1..=257 { batch.task_upserts.push(recovery_task(&store, agent, number, true)); }
    store.commit(batch).unwrap();
    drop(store);

    let corrupt = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    corrupt.execute(
        "UPDATE tasks SET data=json_set(data,'$.task_id',?1) WHERE task_id=?2",
        rusqlite::params![EntityId::nil().to_string(), "00000000-0000-0000-0000-000000000101"],
    ).unwrap();
    drop(corrupt);

    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    let before = reopened.current_commit_seq().unwrap();
    assert!(reopened.recover_native_tasks().is_err());
    assert_eq!(reopened.current_commit_seq().unwrap(), before);
    let first = reopened.external_task("00000000-0000-0000-0000-000000000001".parse().unwrap()).unwrap();
    assert!(matches!(first.state, TaskState::Running { .. }));
    assert_eq!(first.record_revision, RecordRevision(5));
    assert!(!first.memos.contains_key("native_recovery"));
}

// ---------------------------------------------------------------------------
// Subject outbox: one atomic T1, and a dispatch loop that committed facts can
// never poison. X07/X11.
// ---------------------------------------------------------------------------

/// The outbox intent one committed subject step authors.
fn publication_intent() -> shenko_types::subject::PublicationIntent {
    shenko_types::subject::PublicationIntent {
        intent_id: shenko_types::ids::EntityId::new(),
        target: shenko_types::output::PublicationTarget::new(
            "qq",
            "group",
            "123456789",
            "qq:10001",
        ),
        content: "已经提交的回复正文".into(),
        reply_grant_id: None,
    }
}

/// What an installed channel plugin describes for that target. The store owns
/// the ledger; the plugin owns the platform action.
fn publication_dispatch() -> shenko_types::channel::ChannelDispatch {
    shenko_types::channel::ChannelDispatch {
        channel: "qq".into(),
        action: shenko_types::channel::ChannelAction::Send,
        platform_action: "send_group_msg".into(),
        params: serde_json::json!({"group_id":"123456789","message":"已经提交的回复正文"}),
        account: "qq:10001".into(),
        tenant: None,
        conversation: Some("group:123456789".into()),
        attachments: vec![],
        contract_digest: shenko_types::object::Digest::blake3(b"shenko.channel/qq/test/1"),
        acceptance_predicate: "onebot_retcode_zero".into(),
        echo: None,
    }
}

/// Commit one real subject step whose S1 authored `intent`.
fn commit_publication_step(
    store: &Arc<Store>,
    agent: shenko_types::ids::EntityId,
    intent: &shenko_types::subject::PublicationIntent,
) -> Result<shenko_types::subject::SubjectStep> {
    let (head, first) = store.load_subject(agent).unwrap();
    let next = shenko_types::ids::EntityId::new();
    let result = recorded_result(store, &head, first)?;
    let step = committed_candidate_with(store, result, next, vec![intent.clone()]);
    store
        .commit_step_and_advance_head(&step, &head, next)
        .unwrap();
    Ok(step)
}

#[test]
fn channel_conversations_interleave_publications_with_recorded_message_times() -> Result<()> {
    use chrono::{DateTime, Duration};
    use shenko_types::{channel::ChannelEnvelope, ids::EntityId, inbox::{InboxEvent, InboxEventKind}};

    let (dir, store, agent) = initialized_subject();
    let publication = publication_intent();
    commit_publication_step(&store, agent, &publication)?;
    let task = store.list_task_summaries(agent, 100)?.into_iter()
        .find(|task| task.task_id == publication.intent_id.to_string())
        .context("the committed publication must have a task summary")?;
    let published = DateTime::parse_from_rfc3339(&task.created_at)?.to_utc();
    let admit = |text: &str, at: chrono::DateTime<chrono::Utc>| -> Result<EntityId> {
        let event_id = EntityId::new();
        let envelope: ChannelEnvelope = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "provider": "qq", "account": "qq:10001",
            "transport_identity_domain": "test:qq", "conversation": "group:123456789",
            "message_id": event_id.to_string(), "received_at": at, "occurred_at": at,
            "actor": {"platform_id": "member", "display_name": "成员"},
            "content": {"text": text},
            "reply_target": {"kind": "group", "id": "123456789", "label": "群聊", "account": "qq:10001"}
        }))?;
        let event = InboxEvent {
            event_id, kind: InboxEventKind::Message, payload: serde_json::to_value(envelope)?,
            created_at: at, source: "channel/qq".into(),
        };
        let (head, planned) = store.load_subject(agent)?;
        store.stage_subject_inputs(&head, &planned, &[shenko_store::raw_session::SubjectHistoryInput {
            event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        }])?;
        Ok(event_id)
    };
    // A delayed delivery can precede an already committed publication in
    // event time. The later message must follow that same publication.
    let before = admit("较早的来信", published - Duration::seconds(1))?;
    let after = admit("较后的来信", published + Duration::seconds(1))?;
    let expected = vec![before.to_string(), publication.intent_id.to_string(), after.to_string()];
    let messages = store.channel_conversation_messages(agent, 10)?;
    assert_eq!(messages.iter().map(|message| message.message_id.clone()).collect::<Vec<_>>(), expected);
    assert_eq!(messages.iter().map(|message| message.direction).collect::<Vec<_>>(),
        vec!["unknown", "outbound", "unknown"],
        "historical envelopes without ownership facts do not assert a counterpart");
    assert!(messages.iter().all(|message| message.thread_key == "qq:10001|group|123456789"));
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
    assert_eq!(reopened.channel_conversation_messages(agent, 10)?.into_iter()
        .map(|message| message.message_id).collect::<Vec<_>>(), expected);
    Ok(())
}

#[test]
fn channel_conversations_limit_selects_latest_publications() -> Result<()> {
    let (_dir, store, agent) = initialized_subject();
    let first = publication_intent();
    let second = publication_intent();
    let third = publication_intent();
    for intent in [&first, &second, &third] {
        commit_publication_step(&store, agent, intent)?;
    }
    let actual = store.channel_conversation_messages(agent, 2)?.into_iter()
        .map(|message| message.message_id).collect::<std::collections::BTreeSet<_>>();
    let expected = [second.intent_id.to_string(), third.intent_id.to_string()]
        .into_iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn inspection_projections_keep_each_subjects_tasks_and_messages_private() -> Result<()> {
    use shenko_types::{channel::ChannelEnvelope, ids::EntityId, inbox::{InboxEvent, InboxEventKind}};
    let (_dir, store, first) = initialized_subject();
    let second = initialize_subject_in(&store, b"second subject");
    let first_publication = publication_intent();
    let second_publication = publication_intent();
    commit_publication_step(&store, first, &first_publication)?;
    commit_publication_step(&store, second, &second_publication)?;
    let mut events = Vec::new();
    for (agent, text) in [(first, "first private input"), (second, "second private input")] {
        let event_id = EntityId::new();
        let at = chrono::Utc::now();
        let envelope: ChannelEnvelope = serde_json::from_value(serde_json::json!({
            "schema_version": 1, "provider": "qq", "account": "qq:10001",
            "transport_identity_domain": "test:qq", "conversation": "group:123456789",
            "message_id": event_id, "received_at": at, "actor": {"platform_id": "member"},
            "content": {"text": text},
            "reply_target": {"kind": "group", "id": "123456789", "label": "", "account": "qq:10001"}
        }))?;
        let event = InboxEvent {
            event_id, kind: InboxEventKind::Message, payload: serde_json::to_value(envelope)?,
            created_at: at, source: "channel/qq".into(),
        };
        let (head, planned) = store.load_subject(agent)?;
        store.stage_subject_inputs(&head, &planned, &[shenko_store::raw_session::SubjectHistoryInput {
            event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        }])?;
        events.push(event_id);
    }
    for (agent, publication, foreign, event) in [
        (first, &first_publication, &second_publication, events[0]),
        (second, &second_publication, &first_publication, events[1]),
    ] {
        let (head, _) = store.load_subject(agent)?;
        let ids = store.list_task_summaries(agent, 100)?.into_iter()
            .map(|task| task.task_id).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids, [head.subject_root_task_id.to_string(), publication.intent_id.to_string()]
            .into_iter().collect());
        assert!(store.task_trajectory(agent, foreign.intent_id)?.is_none());
        let own = store.task_trajectory(agent, publication.intent_id)?
            .context("the subject must see its own publication trajectory")?;
        assert_eq!(own.task.task_id, publication.intent_id.to_string());
        assert!(own.children.is_empty());
        let messages = store.channel_conversation_messages(agent, 100)?.into_iter()
            .map(|message| message.message_id).collect::<std::collections::BTreeSet<_>>();
        assert_eq!(messages, [event.to_string(), publication.intent_id.to_string()].into_iter().collect());
    }
    // The installation cannot select an arbitrary local subject for an API
    // request: ambiguous identity is an error, never an unscoped fallback.
    assert!(store.get_existing_agent_id().is_err());
    Ok(())
}

#[test]
fn structural_task_roles_survive_reopen_and_archive_without_kind_inference() -> Result<()> {
    use shenko_types::{ids::EntityId, subject::*, task::*};

    let (dir, store, agent) = initialized_subject();
    let (head, first) = store.load_subject(agent)?;
    let next = EntityId::new();
    let result = recorded_result(&store, &head, first)?;
    let mut step = committed_candidate(&store, result, next);
    let decision_ref = step.decision_ref.as_ref().context("the committed candidate must seal its decision")?;
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&decision_ref.digest)?,
    )?;
    let delegation_id = EntityId::new();
    decision.delegated_task_intents.push(DelegationIntent {
        intent_id: delegation_id,
        goal: "Observe through a future plugin".into(),
        system_prompt: String::new(), model: None, capabilities: vec![],
        budget_hint: None, reply_grant: None,
        definition: Some(NativeTaskDefinition {
            kind: "future.plugin.observe".into(), definition_version: 7,
            input: serde_json::json!({"source":"retained evidence"}),
        }),
    });
    step.decision_ref = Some(store.cas.seal_object(
        &serde_json::to_vec(&decision)?, "application/json", "test", "StepDecision", 8,
    )?.object);
    store.commit_step_and_advance_head(&step, &head, next)?;
    store.start_subject_dispatcher().unwrap();
    let delegated = store.external_task(delegation_id)?;
    let implementation = delegated.implementation_ref.clone();
    let parent = store.claim_native_task(delegation_id, implementation.clone(), false)?
        .context("the delegated task must be claimable")?;
    let tool_id = EntityId::new();
    let input = store.cas.seal_object(b"{}", "application/json", "test", "tool", 1)?.object;
    store.claim_child_tool_task(&parent.task, tool_id, "future.plugin.tool", 3, input, implementation)?
        .context("the delegated task must claim its child tool")?;

    // A familiar definition name, or ownership by the root alone, is no
    // evidence that an older task was delegated or invoked as a tool.
    let mut unclassified = recovery_task(&store, agent, 0x296, false);
    unclassified.kind = "model.child".into();
    unclassified.owner = TaskOwner::Task { id: head.subject_root_task_id };
    let unknown_id = unclassified.task_id;
    let mut batch = WriteBatch::new();
    batch.task_upserts.push(unclassified);
    store.commit(batch)?;

    let roles = |store: &Store| -> Result<std::collections::BTreeMap<String, TaskRole>> {
        Ok(store.list_task_summaries(agent, 100)?.into_iter()
            .map(|task| (task.task_id, task.role)).collect())
    };
    let expected = [
        (head.subject_root_task_id.to_string(), TaskRole::SubjectRoot),
        (delegation_id.to_string(), TaskRole::Delegation),
        (tool_id.to_string(), TaskRole::Tool),
        (unknown_id.to_string(), TaskRole::Unknown),
    ].into_iter().collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(roles(&store)?, expected);
    let trajectory = store.task_trajectory(agent, delegation_id)?
        .context("the delegated task must have a trajectory")?;
    assert_eq!(trajectory.task.role, TaskRole::Delegation);
    assert_eq!(trajectory.children.len(), 1);
    assert_eq!(trajectory.children[0].role, TaskRole::Tool);
    assert_eq!(trajectory.children[0].parent_task_id.as_deref(), Some(delegation_id.to_string().as_str()));
    assert_eq!(serde_json::to_value(&trajectory)?["task"]["role"], "delegation");
    let root = store.task_trajectory(agent, head.subject_root_task_id)?
        .context("the subject root must have a trajectory")?;
    assert!(root.task.parent_task_id.is_none());
    assert!(!root.children.iter().any(|task| task.task_id == root.task.task_id));

    let mut archive = Vec::new();
    store.write_cognition_archive(&Default::default(), None, &mut archive)?;
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    assert_eq!(roles(&reopened)?, expected);
    let receiver = Store::open(dir.join("role-receiver.db"), dir.join("role-receiver-cas"))?;
    receiver.import_cognition_archive(&archive[..], &Default::default())?;
    assert_eq!(roles(&receiver)?, expected);
    let imported = receiver.task_trajectory(agent, delegation_id)?
        .context("the imported delegation must retain its trajectory")?;
    assert_eq!(imported.task.role, TaskRole::Delegation);
    assert_eq!(imported.children[0].role, TaskRole::Tool);
    assert_eq!(receiver.external_task(delegation_id)?.input_ref, delegated.input_ref);
    Ok(())
}

fn publication_state(store: &Store, intent_id: shenko_types::ids::EntityId) -> String {
    store
        .committed_intents(Some("publication"), None, 16)
        .unwrap()
        .into_iter()
        .find(|intent| intent.intent_id == intent_id)
        .expect("the admitted publication intent is readable")
        .state
}

#[test]
fn publication_t1_claims_and_arms_in_one_commit() -> Result<()> {
    use shenko_types::ids::*;
    use shenko_types::operation::{AttemptDispatchStage, OperationState};
    use shenko_types::task::TaskState;

    let (_dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    let step = commit_publication_step(&store, agent, &intent)?;

    // S1 already exposed the intent and its task together: never a half outbox.
    assert_eq!(store.ready_publications(8).unwrap().len(), 1);
    assert!(matches!(
        store.external_task(intent_id).unwrap().state,
        TaskState::Pending { .. }
    ));

    let before = store.current_commit_seq().unwrap();
    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap()
        .expect("a ready publication arms");
    let after = store.current_commit_seq().unwrap();
    assert_eq!(after, before + 1, "T1 must advance the ledger exactly once");
    assert_eq!(armed.operation_id, intent_id);
    assert_eq!(armed.task_id, intent_id);
    assert_eq!(armed.account, "qq:10001");
    assert_eq!(
        armed.armed_commit,
        shenko_types::commit::CommitRef::new(store.ledger.ledger_id(), LedgerSeq(after))
    );

    // The same commit carries the claim, the operation and the armed attempt.
    assert_eq!(publication_state(&store, intent_id), "claimed");
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::InFlight);
    assert_eq!(operation.prepared_commit, armed.armed_commit);
    assert_eq!(operation.authority_ref.decision_ref, step.step_id);
    let attempts = store.external_operation_attempts(intent_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].dispatch_stage, AttemptDispatchStage::Armed);
    assert_eq!(attempts[0].attempt_id, armed.attempt_id);
    assert_eq!(attempts[0].armed_commit, armed.armed_commit);
    match store.external_task(intent_id).unwrap().state {
        TaskState::Running {
            invocation_id,
            epoch,
            ..
        } => {
            assert_eq!(invocation_id, attempts[0].invocation_id);
            assert_eq!(epoch, attempts[0].epoch);
        }
        other => panic!("the publication task must be running at T1, got {other:?}"),
    }

    // A second claim never authorizes a second send.
    assert!(
        store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none(),
        "an already claimed publication must not arm again"
    );
    assert!(store.ready_publications(8).unwrap().is_empty());
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
    assert_eq!(store.current_commit_seq().unwrap(), after);
    Ok(())
}

#[test]
fn cancelled_publication_is_settled_never_dispatched_without_stopping_dispatch() -> Result<()> {
    use shenko_types::ids::*;
    use shenko_types::task::{AbortScope, TaskState};

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent)?;

    // The owner withdraws the reply before any T1: a real, committed fact.
    let cancellation = store
        .request_native_cancellation(
            intent_id,
            EntityId::new(),
            EntityId::new(),
            "the subject withdrew this reply",
            AbortScope::Task,
        )
        .unwrap();
    assert!(cancellation.cancelled.contains(&intent_id));

    // Before this contract the call answered `Err(publication_cancelled)`,
    // which the publication consumer propagated as a fatal pass: one cancelled
    // reply took the whole runtime down on every restart, forever.
    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap();
    assert!(armed.is_none(), "a cancelled reply is never armed");
    assert!(
        store.external_operation(intent_id).is_err(),
        "nothing was ever armed, so no external effect is possible"
    );
    assert!(store.ready_publications(8).unwrap().is_empty());
    let settled = store
        .committed_intent(intent_id)
        .unwrap()
        .expect("the settled intent is readable by identity");
    assert_eq!(settled.state, "blocked");
    assert!(
        settled.result_ref.is_some(),
        "the never-dispatched intent names its sealed outcome"
    );
    assert!(
        store.committed_intent(EntityId::new()).unwrap().is_none(),
        "an unknown identity is absent, not an error"
    );

    let outcome_ref = match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { outcome_ref } => outcome_ref,
        other => panic!("the never-dispatched reply must retire its task, got {other:?}"),
    };
    let detail: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&outcome_ref.digest).unwrap()).unwrap();
    assert_eq!(detail["kind"], "publication_never_dispatched");
    assert_eq!(detail["reason_kind"], "cancelled");
    assert_eq!(detail["dispatched"], false);
    assert_eq!(detail["effect_status"], "none");

    // Durable: a reopen reads the same settlement and still refuses to send.
    drop(store);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(publication_state(&reopened, intent_id), "blocked");
    assert!(
        reopened
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none()
    );
    Ok(())
}

#[test]
fn superseded_authority_is_settled_never_dispatched_instead_of_failing() -> Result<()> {
    use shenko_types::ids::*;
    use shenko_types::task::TaskState;

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent)?;

    // Model a completed controller handoff: head and registered successor
    // advance together, while the already committed authoring step keeps its
    // original epoch and can no longer authorize this reply.
    let (mut head, mut successor) = store.load_subject(agent)?;
    head.controller_epoch = ControllerEpoch(head.controller_epoch.0 + 1);
    head.record_revision = head.record_revision.next();
    successor.controller_epoch = head.controller_epoch;
    successor.record_revision = successor.record_revision.next();
    let mut connection = rusqlite::Connection::open(dir.join("head.db"))?;
    let transaction = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    transaction.execute(
            "UPDATE subject_branch_heads SET record_revision=?2,data=?3 WHERE agent_id=?1",
            rusqlite::params![
                agent.to_string(),
                head.record_revision.0 as i64,
                serde_json::to_string(&head)?
            ],
        )?;
    transaction.execute(
        "UPDATE subject_steps SET record_revision=?2,data=?3 WHERE step_id=?1",
        rusqlite::params![successor.step_id.to_string(), successor.record_revision.0 as i64,
            serde_json::to_string(&successor)?],
    )?;
    transaction.commit()?;

    let armed = store
        .arm_publication(intent_id, &publication_dispatch())
        .unwrap();
    assert!(armed.is_none());
    assert!(store.external_operation(intent_id).is_err());
    assert_eq!(publication_state(&store, intent_id), "blocked");
    let outcome_ref = match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { outcome_ref } => outcome_ref,
        other => panic!("the superseded reply must retire its task, got {other:?}"),
    };
    let detail: serde_json::Value =
        serde_json::from_slice(&store.cas.read_object(&outcome_ref.digest).unwrap()).unwrap();
    assert_eq!(detail["reason_kind"], "authority_superseded");
    assert_eq!(detail["effect_status"], "none");
    Ok(())
}

/// Real process boundary: one process arms the send and dies; the next open
/// must see the same single attempt, record the loss of control as UNKNOWN,
/// never arm a second send, and accept the platform's late answer for that
/// original attempt.
#[test]
fn armed_publication_survives_a_real_process_death_without_a_second_send() -> Result<()> {
    use shenko_types::ids::*;
    use shenko_types::operation::{AttemptDispatchStage, EffectStatus, OperationState};
    use shenko_types::task::TaskState;

    if let Some(dir) = std::env::var_os("SHENKO_C01_PUBLICATION_ARM_CHILD") {
        let dir = PathBuf::from(dir);
        let store = Store::open(dir.join("head.db"), dir.join("cas")).unwrap();
        store.start_subject_dispatcher().unwrap();
        let intent_id: EntityId = std::fs::read_to_string(dir.join("intent"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let armed = store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .expect("the child process arms the send");
        let invocation_id = store
            .external_operation_attempts(intent_id)
            .unwrap()
            .first()
            .map(|attempt| attempt.invocation_id);
        std::fs::write(
            dir.join("armed.json"),
            serde_json::to_vec(&serde_json::json!({
                "operation_id": armed.operation_id,
                "attempt_id": armed.attempt_id,
                "armed_commit": armed.armed_commit,
                "invocation_id": invocation_id,
            }))
            .unwrap(),
        )
        .unwrap();
        // Actual process death immediately after T1 committed: no send was
        // observed by anyone and no destructor ever runs.
        #[cfg(unix)]
        {
            std::process::Command::new("kill")
                .args(["-KILL", &std::process::id().to_string()])
                .status()
                .unwrap();
            unreachable!("SIGKILL must end the process");
        }
        #[cfg(not(unix))]
        std::process::exit(73);
    }

    let (dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher().unwrap();
    let intent = publication_intent();
    let intent_id = intent.intent_id;
    commit_publication_step(&store, agent, &intent)?;
    std::fs::write(dir.join("intent"), intent_id.to_string()).unwrap();
    drop(store);

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "armed_publication_survives_a_real_process_death_without_a_second_send",
            "--nocapture",
        ])
        .env("SHENKO_C01_PUBLICATION_ARM_CHILD", &dir)
        .status()
        .unwrap();
    assert!(!status.success(), "the child must die, not return");
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
    let child: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("armed.json")).unwrap()).unwrap();
    let attempt_id: EntityId = child["attempt_id"].as_str().unwrap().parse().unwrap();
    let armed_commit: shenko_types::commit::CommitRef =
        serde_json::from_value(child["armed_commit"].clone()).unwrap();

    // A new incarnation takes the dispatch owner. The armed send lost its
    // process: that is local loss of control, never proof the platform did not
    // receive it.
    let store = Arc::new(Store::open(dir.join("head.db"), dir.join("cas")).unwrap());
    store.start_subject_dispatcher()?;
    assert_eq!(publication_state(&store, intent_id), "unknown");
    assert!(
        store
            .arm_publication(intent_id, &publication_dispatch())
            .unwrap()
            .is_none(),
        "the dead process's armed attempt must never be re-armed"
    );
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::InFlight);
    assert_eq!(operation.prepared_commit, armed_commit);

    // Reconciliation records the restart as UNKNOWN evidence and stops there.
    store.reconcile_subject_publications().unwrap();
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::Reconciling);
    assert_eq!(operation.effect_status, EffectStatus::Unknown);
    assert_eq!(operation.outcome, None);
    assert_eq!(publication_state(&store, intent_id), "unknown");
    let attempt = store
        .external_operation_attempts(intent_id)
        .unwrap()
        .remove(0);
    assert_eq!(attempt.attempt_id, attempt_id);
    assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Unknown);

    // The platform's own answer for that same attempt is the late E1 the
    // recovery contract admits; it resolves the operation without a resend.
    store
        .record_publication_receipt(
            &shenko_store::channel::ArmedPublication {
                operation_id: intent_id,
                attempt_id,
                task_id: intent_id,
                account: "qq:10001".into(),
                armed_commit,
            },
            &shenko_types::channel::ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("message-1".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"message_id":"message-1"}),
                observed_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    store.reconcile_subject_publications().unwrap();
    let operation = store.external_operation(intent_id).unwrap();
    assert_eq!(operation.state, OperationState::Resolved);
    assert_eq!(operation.effect_status, EffectStatus::Complete);
    assert_eq!(publication_state(&store, intent_id), "settled");
    assert_eq!(
        store
            .committed_intent(intent_id)
            .unwrap()
            .expect("the settled publication is readable by identity")
            .state,
        "settled"
    );
    match store.external_task(intent_id).unwrap().state {
        TaskState::Terminal { .. } => {}
        other => panic!("the settled publication must retire its task, got {other:?}"),
    }
    // Both originals are preserved; there is still exactly one attempt.
    assert_eq!(
        store.external_operation_evidence(intent_id).unwrap().len(),
        2
    );
    assert_eq!(store.external_operation_attempts(intent_id).unwrap().len(), 1);
    Ok(())
}

/// The durable ready lane keeps row identity next to the original bytes, so a
/// consumer can name a result it cannot read instead of failing the whole
/// dispatch pass — while the strict reader still refuses that corruption.
#[test]
fn unreadable_ready_native_result_keeps_its_identity_and_the_row() {
    use shenko_types::ids::*;
    use shenko_types::task::{TaskOutcome, TaskResult};

    let (dir, store, agent) = initialized_subject();
    let evidence_ref = store
        .cas
        .seal_object(b"evidence", "application/json", "test", "raw", 1)
        .unwrap()
        .object;
    let result_id = EntityId::new();
    let healthy = TaskResult {
        schema_version: 1,
        result_id,
        task_id: EntityId::new(),
        task_revision: TaskRevision(0),
        task_record_revision: RecordRevision(0),
        invocation_id: EntityId::new(),
        operation_id: EntityId::new(),
        receipt_id: EntityId::new(),
        outcome: TaskOutcome::Succeeded,
        result: Some(serde_json::json!({"stdout_utf8":"ok"})),
        evidence_ref,
        commit_ref: shenko_types::commit::CommitRef::new(store.ledger.ledger_id(), LedgerSeq(1)),
    };
    // The exact durable shape `finalize_task` writes (kind/source/consumed),
    // with one row from a build whose result shape this one cannot read.
    let unreadable_id = EntityId::new();
    let conn = rusqlite::Connection::open(dir.join("head.db")).unwrap();
    for (event_id, data) in [
        (
            unreadable_id,
            r#"{"schema_version":99,"future_shape":true}"#.to_string(),
        ),
        (result_id, serde_json::to_string(&healthy).unwrap()),
    ] {
        conn.execute(
            "INSERT INTO inbox_events(event_id,agent_id,kind,source,consumed,data)
             VALUES(?1,?2,'task_result','native-child',0,?3)",
            rusqlite::params![event_id.to_string(), agent.to_string(), data],
        )
        .unwrap();
    }
    drop(conn);

    // The row lane preserves identity and bytes for both rows: a consumer can
    // journal the one it cannot read and still deliver the other.
    let rows = store.ready_native_result_rows(agent, 8).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, unreadable_id);
    assert_eq!(rows[0].1, r#"{"schema_version":99,"future_shape":true}"#);
    assert_eq!(rows[1].0, result_id);

    // The strict reader still refuses to invent a result from bad bytes.
    assert!(store.ready_native_results(agent, 8).is_err());

    // Nothing was deleted or consumed by the refusal.
    let pending = store.ready_native_result_rows(agent, 8).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|(event_id, _)| *event_id == unreadable_id));
}


/// The built-in IM lists the subject's external conversations from its own
/// records. A channel message is read where the runtime keeps it (the staged
/// input of the step that took it); the plugin's audience shape travels with
/// the thread; a publication carries the time the provider answered; and the
/// platform's echo of that publication is the publication, not a second line.
#[test]
fn channel_conversation_reads_staged_messages_and_hides_publication_echoes() -> Result<()> {
    use shenko_types::channel::{
        AudienceShape, ChannelActor, ChannelContent, ChannelEnvelope, ChannelReceipt,
        ChannelReplyTarget,
    };
    use shenko_types::ids::{BranchId, EntityId};
    let (_dir, store, agent) = initialized_subject();
    store.start_subject_dispatcher()?;

    // A second real subject shares this Store, including its raw sessions
    // and publication ledger. Channel accounts do not identify the subject.
    let (mut foreign_head, mut foreign_first) = store.load_subject(agent)?;
    let mut foreign_genesis = store.get_subject_genesis(agent)?
        .context("the original subject must retain its genesis")?;
    let foreign_agent = EntityId::new();
    foreign_genesis.genesis_id = EntityId::new();
    foreign_genesis.subject_id = foreign_agent;
    foreign_genesis.bootstrap_id = EntityId::new();
    foreign_genesis.launch_id = EntityId::new();
    foreign_head.agent_id = foreign_agent;
    foreign_head.branch_id = BranchId::new();
    foreign_head.subject_root_task_id = EntityId::new();
    foreign_head.genesis_ref.genesis_id = foreign_genesis.genesis_id;
    foreign_head.next_step_id = EntityId::new();
    foreign_first.step_id = foreign_head.next_step_id;
    foreign_first.branch_id = foreign_head.branch_id.clone();
    let foreign_root = shenko_types::task::TaskRecord::subject_root(
        foreign_head.subject_root_task_id,
        foreign_agent,
        foreign_genesis.bootstrap_event_ref.clone(),
        foreign_genesis.created_commit,
    );
    store
        .initialize_genesis(&foreign_genesis, &foreign_head, &foreign_first, &foreign_root)?;

    let intent = publication_intent();
    commit_publication_step(&store, agent, &intent)?;
    let armed = store
        .arm_publication(intent.intent_id, &publication_dispatch())?
        .context("the committed publication arms once")?;
    let sent_at = chrono::Utc::now();
    let sent_at_text = sent_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .record_publication_receipt(
            &armed,
            &ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("900".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"data":{"message_id":900}}),
                observed_at: sent_at,
            },
        )?;

    let audience = ChannelReplyTarget {
        kind: "group".into(),
        id: "123456789".into(),
        label: "群 123456789".into(),
        account: "qq:10001".into(),
        tenant: None,
        conversation: Some("123456789".into()),
        shape: Some(AudienceShape::Group),
    };
    let line = |message_id: &str,
                actor: &str,
                own_account: bool,
                text: &str,
                provider: &str,
                target: &ChannelReplyTarget| -> Result<shenko_store::raw_session::SubjectHistoryInput> {
        let envelope = ChannelEnvelope {
            schema_version: 1,
            provider: provider.into(),
            account: target.account.clone(),
            tenant: target.tenant.clone(),
            transport_identity_domain: "onebot".into(),
            conversation: target.conversation.clone(),
            thread: None,
            message_id: message_id.into(),
            source_event_revision: None,
            occurred_at: Some(sent_at),
            received_at: sent_at,
            actor: ChannelActor {
                platform_id: actor.into(),
                display_name: Some(actor.into()),
                alias: None,
                person_ref: None,
                role_grant_ref: None,
                group_role: None,
                own_account: Some(own_account),
            },
            content: ChannelContent {
                text: text.into(),
                segments: vec![],
                mentions: vec![],
                reply_to: None,
            },
            reply_target: Some(target.clone()),
            attachments: vec![],
            raw_ref: None,
        };
        let event = shenko_types::inbox::InboxEvent {
            event_id: shenko_types::ids::EntityId::new(),
            kind: shenko_types::inbox::InboxEventKind::Message,
            payload: serde_json::to_value(&envelope)?,
            created_at: sent_at,
            source: provider.into(),
        };
        Ok(shenko_store::raw_session::SubjectHistoryInput {
            event_id: event.event_id,
            event_type: "user".into(),
            data: serde_json::json!({"v":1, "inbox_event": event, "text": text}),
        })
    };
    let counterpart = line("901", "20002", false, "有人在吗", "qq", &audience)?;
    let same_id_counterpart = line("900", "20002", false, "对方的同号消息", "qq", &audience)?;
    let echo = line("900", "10001", true, "已经提交的回复正文", "qq", &audience)?;
    let other_account = line(
        "900",
        "30003",
        true,
        "另一账号的消息",
        "qq",
        &ChannelReplyTarget {
            account: "qq:30003".into(),
            ..audience.clone()
        },
    )?;
    let other_audience = line(
        "900",
        "10001",
        true,
        "另一群的消息",
        "qq",
        &ChannelReplyTarget {
            id: "987654321".into(),
            label: "群 987654321".into(),
            conversation: Some("987654321".into()),
            ..audience.clone()
        },
    )?;
    let other_kind = line(
        "900",
        "10001",
        true,
        "私聊的同号消息",
        "qq",
        &ChannelReplyTarget {
            kind: "private".into(),
            label: "私聊 123456789".into(),
            shape: Some(AudienceShape::OneToOne),
            ..audience.clone()
        },
    )?;
    let other_provider = line("900", "10001", true, "另一通道的消息", "relay", &audience)?;
    let mut own = line("902", "10001", true, "手机上补一句", "qq", &audience)?;
    own.data["inbox_event"]["payload"]["occurred_at"] = serde_json::Value::Null;
    let (head, planned) = store.load_subject(agent)?;
    store
        .stage_subject_inputs(
            &head,
            &planned,
            &[
                counterpart.clone(),
                same_id_counterpart.clone(),
                echo,
                other_account.clone(),
                other_audience.clone(),
                other_kind.clone(),
                other_provider.clone(),
                own.clone(),
            ],
        )?;

    // Two later steps each re-stage the same real message, followed by
    // unclassified and audience-less message inputs. The six newest raw
    // rows contain only one admissible identity: a three-message window
    // must continue into the older step, past both noise and duplicates.
    for _ in 0..2 {
        let (head, planned) = store.load_subject(agent)?;
        let result = recorded_result(&store, &head, planned)?;
        let next = shenko_types::ids::EntityId::new();
        let committed = committed_candidate(&store, result, next);
        store
            .commit_step_and_advance_head(&committed, &head, next)?;
        let (head, planned) = store.load_subject(agent)?;
        let mut unclassified = line("903", "20002", false, "原始通知", "qq", &audience)?;
        unclassified.data["inbox_event"]["payload"] = serde_json::json!({"notice":true});
        let mut without_audience = line("904", "20002", false, "没有回复受众", "qq", &audience)?;
        without_audience.data["inbox_event"]["payload"]["reply_target"] = serde_json::Value::Null;
        store
            .stage_subject_inputs(
                &head,
                &planned,
                &[own.clone(), unclassified, without_audience],
            )?;
    }

    // The foreign publication deliberately shares the first subject's
    // canonical channel target and its phone message's provider id. Its
    // receipt must not suppress that phone line in the first subject's view.
    let mut foreign_intent = publication_intent();
    foreign_intent.content = "另一个主体的回复".into();
    commit_publication_step(&store, foreign_agent, &foreign_intent)?;
    let mut foreign_dispatch = publication_dispatch();
    foreign_dispatch.params["message"] = serde_json::json!(foreign_intent.content);
    let foreign_armed = store
        .arm_publication(foreign_intent.intent_id, &foreign_dispatch)?
        .context("the foreign subject's own publication arms once")?;
    store
        .record_publication_receipt(
            &foreign_armed,
            &ChannelReceipt {
                status: "ok".into(),
                retcode: 0,
                accepted: true,
                provider_message_id: Some("902".into()),
                raw: serde_json::json!({"status":"ok","retcode":0,"data":{"message_id":902}}),
                observed_at: sent_at,
            },
        )?;
    let foreign_input = line("905", "40004", false, "另一个主体的来信", "qq", &audience)?;
    let foreign_echo = line("902", "10001", true, &foreign_intent.content, "qq", &audience)?;
    let (foreign_head, foreign_planned) = store.load_subject(foreign_agent)?;
    store
        .stage_subject_inputs(
            &foreign_head,
            &foreign_planned,
            &[foreign_input.clone(), foreign_echo],
        )?;

    let before_read = store.current_commit_seq()?;
    let all_rows = store.channel_conversation_messages(agent, 50)?;
    assert_eq!(all_rows.len(), 8, "seven unique messages plus one publication");
    for input in [
        &counterpart,
        &same_id_counterpart,
        &other_account,
        &other_audience,
        &other_kind,
        &other_provider,
        &own,
    ] {
        let event_id = input.event_id.to_string();
        let matches: Vec<_> = all_rows
            .iter()
            .filter(|row| row.message_id == event_id)
            .collect();
        assert_eq!(matches.len(), 1, "the original inbox identity appears once");
        assert_eq!(
            matches[0].text,
            input.data["inbox_event"]["payload"]["content"]["text"].as_str()
                .context("the staged channel message must retain its text")?,
        );
        assert_eq!(matches[0].at.as_deref(), Some(sent_at_text.as_str()));
        assert_eq!(
            matches[0].direction,
            if input.data["inbox_event"]["payload"]["actor"]["own_account"].as_bool() == Some(true) {
                "own"
            } else {
                "inbound"
            },
        );
    }

    let recent = store.channel_conversation_messages(agent, 3)?;
    let mut recent_ids: Vec<_> = recent
        .iter()
        .filter(|row| row.direction != "outbound")
        .map(|row| row.message_id.clone())
        .collect();
    recent_ids.sort();
    let mut expected_ids = vec![
        own.event_id.to_string(),
        other_provider.event_id.to_string(),
        other_kind.event_id.to_string(),
    ];
    expected_ids.sort();
    assert_eq!(
        recent_ids,
        expected_ids,
        "fill the window with the most recent unique admitted messages",
    );
    let foreign_rows = store.channel_conversation_messages(foreign_agent, 50)?;
    assert_eq!(foreign_rows.len(), 2, "the foreign subject sees only its own messages");
    let foreign_input_id = foreign_input.event_id.to_string();
    assert!(foreign_rows.iter().any(|row| {
        row.message_id == foreign_input_id && row.direction == "inbound"
    }));
    let foreign_intent_id = foreign_intent.intent_id.to_string();
    assert!(foreign_rows.iter().any(|row| {
        row.message_id == foreign_intent_id && row.direction == "outbound"
    }));
    assert_eq!(
        store.current_commit_seq()?,
        before_read,
        "conversation reads do not commit facts",
    );

    let rows: Vec<_> = all_rows
        .iter()
        .filter(|row| row.thread_key == "qq:10001|group|123456789")
        .collect();
    let lines: Vec<_> = rows.iter().map(|row| (row.direction, row.text.as_str())).collect();
    assert!(lines.contains(&("inbound", "有人在吗")), "{lines:?}");
    assert!(lines.contains(&("own", "手机上补一句")), "{lines:?}");
    assert_eq!(
        rows.iter()
            .filter(|row| row.text == "已经提交的回复正文")
            .map(|row| row.direction)
            .collect::<Vec<_>>(),
        vec!["outbound"],
        "the platform's echo of a publication is the publication itself"
    );
    let outbound = rows.iter().find(|row| row.direction == "outbound")
        .context("the subject's publication must remain in the conversation")?;
    assert_eq!(outbound.message_id, intent.intent_id.to_string());
    assert_eq!(outbound.at.as_deref(), Some(sent_at_text.as_str()));
    assert!(rows
        .iter()
        .filter(|row| row.direction != "outbound")
        .all(|row| row.thread_shape == Some(AudienceShape::Group)));
    Ok(())
}

/// Store-focused accepted-value fixture. Real Node consensus/transport has its
/// own journeys; here a retained chosen grant can outlive its source caller.
struct CanonicalAuthority {
    source: String,
    stamp: Option<shenko_types::authority::CommitAuthorityStamp>,
    chosen: Arc<parking_lot::Mutex<Option<shenko_types::authority::CommitAuthorityGrant>>>,
    lose_reply: bool,
}

impl shenko_types::authority::CommitAuthorityProvider for CanonicalAuthority {
    fn source_id(&self) -> &str { &self.source }
    fn prepare(&self, admission: &shenko_types::authority::CommitAdmission)
        -> std::result::Result<Option<shenko_types::authority::CommitAuthorityGrant>, shenko_types::error::ShenkoError>
    {
        let grant = shenko_types::authority::CommitAuthorityGrant { admission:admission.clone(),
            grant_id:format!("chosen:{}",admission.action_id), proof:serde_json::json!({"test":"retained chosen value"}), replayed:false };
        *self.chosen.lock() = Some(grant.clone());
        if self.lose_reply { return Err(shenko_types::error::ShenkoError::conflict("fixture lost chosen reply")); }
        Ok(Some(grant))
    }
    fn acquire(&self, grant: Option<&shenko_types::authority::CommitAuthorityGrant>)
        -> std::result::Result<Box<dyn shenko_types::authority::CommitAuthorityGuard>, shenko_types::error::ShenkoError>
    {
        if let Some(grant) = grant {
            let chosen = self.chosen.lock();
            if chosen.as_ref().is_none_or(|original| original.admission != grant.admission || original.grant_id != grant.grant_id) {
                return Err(shenko_types::error::ShenkoError::conflict("not the retained chosen value"));
            }
        }
        Ok(Box::new(CanonicalGuard(self.stamp.clone())))
    }
}

fn canonical_source(store: &Arc<Store>, agent: shenko_types::ids::EntityId, lose_reply: bool)
    -> Result<Arc<parking_lot::Mutex<Option<shenko_types::authority::CommitAuthorityGrant>>>>
{
    use shenko_types::{authority::CommitAuthorityStamp,ids::EntityId,object::Digest};
    let chosen = Arc::new(parking_lot::Mutex::new(None));
    let head = store.load_subject(agent)?.0;
    store.attach_commit_authority(Arc::new(CanonicalAuthority { source:"canonical-source".into(),
        stamp:Some(CommitAuthorityStamp { source_id:"canonical-source".into(),holder_id:EntityId::new(),subject_id:agent,
            branch_id:head.branch_id.clone(),revision:Digest::blake3(b"canonical source authority") }),
        chosen:Arc::clone(&chosen),lose_reply }))?;
    store.activate_runtime_authority(&head)?;
    Ok(chosen)
}

fn canonical_follower(store: &Store, chosen: Arc<parking_lot::Mutex<Option<shenko_types::authority::CommitAuthorityGrant>>>) -> Result<()> {
    store.attach_commit_authority(Arc::new(CanonicalAuthority { source:"canonical-follower".into(),stamp:None,chosen,lose_reply:false }))?;
    Ok(())
}

#[test]
fn canonical_s1_recovers_identical_results_on_a_distinct_ledger_without_dispatch() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, cognition::CognitionImportOptions};
    use shenko_types::{ids::EntityId, inbox::{InboxEvent, InboxEventKind}, store::RawInboxEvent};
    let (dir, source, agent) = initialized_subject();
    let chosen = canonical_source(&source,agent,true)?;
    let (head, first) = source.load_subject(agent)?;
    let result = recorded_result(&source,&head,first)?;
    let next = EntityId::new();
    let publication = publication_intent();
    let publication_id = publication.intent_id;
    let step = committed_candidate_with(&source,result,next,vec![publication]);
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(),None,&mut archive)?;
    let follower = Store::open(dir.join("canonical-follower.db"),dir.join("follower-cas"))?;
    follower.import_cognition_archive(&archive[..],&CognitionImportOptions::default())?;
    assert_ne!(follower.ledger.ledger_id(),source.ledger.ledger_id());
    assert_eq!(follower.get_existing_agent_id()?,None,"plain imported history is not a local subject");
    assert!(follower.installation_binding()?.subject.is_none());
    canonical_follower(&follower,Arc::clone(&chosen))?;
    let parked = follower.get_task(head.subject_root_task_id)?.context("imported root")?;
    assert!(matches!(parked.dispatch_eligibility,shenko_types::task::DispatchEligibility::Parked { .. }));
    assert!(source.commit_step_and_advance_head(&step,&head,next).is_err());
    let grant = chosen.lock().clone().context("retained chosen grant")?;
    let objects = source.admission_objects(&grant.admission.payload_ref)?;
    assert_eq!(source.load_subject(agent)?.0.committed_step_no,0);
    assert!(!source.control_admission_applied_here(&grant.admission)?);
    assert_eq!(source.recover_control_admission(&grant,&objects,true)?,ControlRecoveryOutcome::RecoveredCanonical);
    // A replicated source event proves what the source applied, never that
    // this receiving Store has performed its own atomic materialization.
    let source_conn = rusqlite::Connection::open(dir.join("head.db"))?;
    let applied_id = EntityId::derived("commit-authority-admission-v1",grant.admission.conflict_key.as_bytes());
    let applied: String = source_conn.query_row("SELECT data FROM inbox_events WHERE event_id=?1",[applied_id.to_string()],|row| row.get(0))?;
    let observation = InboxEvent {
        event_id: EntityId::new(),
        kind: InboxEventKind::SystemEvent,
        source: "fixture-runtime-observation".into(),
        payload: serde_json::json!({"kind": "runtime.observation"}),
        created_at: chrono::Utc::now(),
    };
    source.retain_inbox_event(agent, &observation)?;
    let pending = source.pending_inbox_events(agent, 1)?;
    let [(event_id, event)] = pending.as_slice() else {
        anyhow::bail!("expected one pending runtime observation");
    };
    assert_eq!(*event_id, observation.event_id);
    assert_eq!(serde_json::from_str::<InboxEvent>(event)?, observation);
    assert!(source.control_admission_applied_here(&grant.admission)?);
    let mut batch = WriteBatch::new();
    batch.inbox_event_upserts.push(RawInboxEvent { event_id:applied_id,agent_id:agent,source:"commit-authority".into(),kind:"system_event".into(),payload_json:applied,received_at_ms:0 });
    follower.commit(batch)?;
    assert!(follower.control_admission_applied(&grant.admission)?);
    assert!(!follower.control_admission_applied_here(&grant.admission)?);
    assert_eq!(follower.get_existing_agent_id()?,None,"an imported source receipt supplies no local installation binding");
    assert_eq!(follower.recover_control_admission(&grant,&objects,false)?,ControlRecoveryOutcome::RecoveredCanonical);
    let source_head = source.load_subject(agent)?.0;
    let follower_head = follower.load_subject(agent)?.0;
    assert_eq!(serde_json::to_value(&follower_head)?,serde_json::to_value(&source_head)?);
    assert_eq!(follower_head.committed_at.ledger_id,source.ledger.ledger_id());
    assert_eq!(follower.get_existing_agent_id()?,Some(agent));
    assert_eq!(follower.installation_binding()?.subject,source.installation_binding()?.subject,
        "local materialization keeps the original subject and origin proof without renaming its source commit");
    assert_eq!(serde_json::to_value(follower.get_task(publication_id)?)?,serde_json::to_value(source.get_task(publication_id)?)?);
    assert_eq!(serde_json::to_value(follower.get_task(head.subject_root_task_id)?)?,serde_json::to_value(Some(parked))?);
    let source_events = source.session_events_range(EntityId(head.branch_id.0),0,100)?;
    let follower_events = follower.session_events_range(EntityId(head.branch_id.0),0,100)?;
    assert_eq!(serde_json::to_value(source_events)?,serde_json::to_value(follower_events)?);
    let follower_conn = rusqlite::Connection::open(dir.join("canonical-follower.db"))?;
    assert_eq!(follower_conn.query_row("SELECT count(*) FROM operations",[],|row| row.get::<_,i64>(0))?,0);
    assert_eq!(follower_conn.query_row("SELECT count(*) FROM attempts",[],|row| row.get::<_,i64>(0))?,0);
    let local_receipt: String = follower_conn.query_row("SELECT data FROM control_materializations WHERE action_id=?1",[grant.admission.action_id.to_string()],|row| row.get(0))?;
    let local_receipt: serde_json::Value = serde_json::from_str(&local_receipt)?;
    assert_eq!(local_receipt["committed_at"]["ledger_id"],follower.ledger.ledger_id().to_string());
    assert_eq!(local_receipt["logical_result"],serde_json::to_value(source_head.committed_at)?);
    drop(follower_conn); drop(follower);
    let reopened = Store::open(dir.join("canonical-follower.db"),dir.join("follower-cas"))?;
    assert!(reopened.control_admission_applied_here(&grant.admission)?);
    assert_eq!(reopened.get_existing_agent_id()?,Some(agent));
    let watermark = reopened.current_commit_seq()?;
    assert_eq!(reopened.recover_control_admission(&grant,&objects,false)?,ControlRecoveryOutcome::Applied);
    assert_eq!(watermark,reopened.current_commit_seq()?);
    let predecessor = rusqlite::Connection::open(dir.join("canonical-follower.db"))?;
    // A receipt on the current ledger still requires its exact physical commit.
    let local_raw: String = predecessor.query_row(
        "SELECT data FROM control_materializations WHERE action_id=?1",
        [grant.admission.action_id.to_string()], |row| row.get(0))?;
    let mut damaged: serde_json::Value = serde_json::from_str(&local_raw)?;
    damaged["committed_at"]["seq"] = serde_json::json!(i64::MAX);
    predecessor.execute("UPDATE control_materializations SET data=?1 WHERE action_id=?2",
        rusqlite::params![serde_json::to_string(&damaged)?, grant.admission.action_id.to_string()])?;
    assert!(matches!(reopened.get_existing_agent_id(), Err(error)
        if error.to_string().contains("installation subject lacks its exact local control application receipt")));
    assert!(reopened.installation_binding().is_err());
    predecessor.execute("UPDATE control_materializations SET data=?1 WHERE action_id=?2",
        rusqlite::params![local_raw, grant.admission.action_id.to_string()])?;
    assert_eq!(reopened.get_existing_agent_id()?, Some(agent));
    // A real consistent slot copy keeps immutable predecessor receipts, but
    // rotating its physical writer identity must not adopt those receipts.
    drop(reopened);
    let original_rows = {
        let mut query = predecessor.prepare(
            "SELECT conflict_key,action_id,data,updated_commit_seq FROM control_materializations ORDER BY conflict_key")?;
        query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, String>(2)?, row.get::<_, i64>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    assert!(!original_rows.is_empty());
    let original_event: String = predecessor.query_row(
        "SELECT data FROM inbox_events WHERE event_id=?1", [applied_id.to_string()], |row| row.get(0))?;
    let original_commits = {
        let mut query = predecessor.prepare("SELECT ledger_id,seq FROM commit_log ORDER BY ledger_id,seq")?;
        query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let copied_path = dir.join("canonical-copied-slot.db");
    predecessor.backup(rusqlite::DatabaseName::Main, &copied_path, None)?;
    predecessor.close().map_err(|(_, error)| error)?;
    let copied_ledger = EntityId::new();
    let copied_conn = rusqlite::Connection::open(&copied_path)?;
    assert_eq!(copied_conn.execute("UPDATE ledger_identity SET ledger_id=?1 WHERE singleton=1",
        [copied_ledger.to_string()])?, 1);
    copied_conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    copied_conn.close().map_err(|(_, error)| error)?;
    let copied_conn = rusqlite::Connection::open(&copied_path)?;
    let copied = Store::open(copied_path, dir.join("follower-cas"))?;
    assert_eq!(copied.ledger.ledger_id(), copied_ledger);
    assert_eq!(copied.get_existing_agent_id()?, None,
        "copied physical receipts belong to the predecessor writer, not this slot");
    assert!(copied.installation_binding()?.subject.is_none());
    let preserved_rows = {
        let mut query = copied_conn.prepare(
            "SELECT conflict_key,action_id,data,updated_commit_seq FROM control_materializations ORDER BY conflict_key")?;
        query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, String>(2)?, row.get::<_, i64>(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    assert_eq!(preserved_rows, original_rows);
    assert_eq!(copied_conn.query_row("SELECT data FROM inbox_events WHERE event_id=?1",
        [applied_id.to_string()], |row| row.get::<_, String>(0))?, original_event);
    let copied_commits = {
        let mut query = copied_conn.prepare("SELECT ledger_id,seq FROM commit_log ORDER BY ledger_id,seq")?;
        query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    assert_eq!(copied_commits, original_commits);
    assert_eq!(serde_json::to_value(copied.load_subject(agent)?.0)?, serde_json::to_value(&follower_head)?);
    assert!(copied.control_admission_applied(&grant.admission)?, "the copied source fact remains historical evidence");
    assert_eq!(copied_conn.query_row("SELECT count(*) FROM operations", [], |row| row.get::<_, i64>(0))?, 0);
    assert_eq!(copied_conn.query_row("SELECT count(*) FROM attempts", [], |row| row.get::<_, i64>(0))?, 0);
    drop(copied_conn);
    drop(copied);
    let reopened = Store::open(dir.join("canonical-follower.db"), dir.join("follower-cas"))?;
    let other = initialize_subject_in(&reopened,b"{\"other_local_genesis\":true}");
    assert_ne!(other,agent);
    assert!(matches!(reopened.get_existing_agent_id(),
        Err(error) if error.to_string().contains("more than one local subject")));
    assert!(reopened.installation_binding().is_err(),"multiple real local subjects never choose an arbitrary identity");
    Ok(())
}

#[test]
fn canonical_s1_conflicting_readset_and_partial_sql_fault_never_publish_a_receipt() -> Result<()> {
    use shenko_store::ControlRecoveryOutcome;
    use shenko_types::{ids::EntityId,object::ObjectRef};
    let (dir,source,agent) = initialized_subject();
    let chosen = canonical_source(&source,agent,true)?;
    let (head,first) = source.load_subject(agent)?;
    let result = recorded_result(&source,&head,first)?;
    let next = EntityId::new();
    let publication = publication_intent();
    let publication_id = publication.intent_id;
    let step = committed_candidate_with(&source,result,next,vec![publication]);
    assert!(source.commit_step_and_advance_head(&step,&head,next).is_err());
    let grant = chosen.lock().clone().context("chosen S1")?;
    let objects = source.admission_objects(&grant.admission.payload_ref)?;
    let conn = rusqlite::Connection::open(dir.join("head.db"))?;
    let original: String = conn.query_row("SELECT created_at FROM subject_steps WHERE step_id=?1",[step.step_id.to_string()],|row| row.get(0))?;
    conn.execute("UPDATE subject_steps SET created_at='changed-after-prepare' WHERE step_id=?1",[step.step_id.to_string()])?;
    assert_eq!(source.recover_control_admission(&grant,&objects,true)?,ControlRecoveryOutcome::ReconciliationRequired);
    assert!(!source.control_admission_applied_here(&grant.admission)?);
    assert!(source.get_task(publication_id)?.is_none());
    conn.execute("UPDATE subject_steps SET created_at=?1 WHERE step_id=?2",rusqlite::params![original,step.step_id.to_string()])?;
    // A malformed final row change fails after earlier SQL writes. All those
    // writes and the local materialization marker must roll back together.
    let mut envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    let transition_ref: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let mut transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&transition_ref.digest)?)?;
    let tables = transition["tables"].as_array_mut().context("transition tables")?;
    let last = tables.iter_mut().rev().find(|table| table["changes"].as_array().is_some_and(|rows| !rows.is_empty())).context("changed table")?;
    last["changes"][0]["key"] = serde_json::json!("not the frozen row key");
    let changed = source.cas.seal_object(&serde_json::to_vec(&transition)?,"application/json","commit-authority","store-s1-transition",transition_ref.codec.major)?.object;
    envelope["action"]["transition_ref"] = serde_json::to_value(changed)?;
    let mut fault = grant.clone();
    fault.admission.payload_ref = source.cas.seal_object(&serde_json::to_vec(&envelope)?,"application/json","commit-authority","commit-admission",1)?.object;
    *chosen.lock() = Some(fault.clone());
    let fault_objects = source.admission_objects(&fault.admission.payload_ref)?;
    assert!(source.recover_control_admission(&fault,&fault_objects,true).is_err());
    assert!(!source.control_admission_applied_here(&grant.admission)?);
    assert!(source.get_task(publication_id)?.is_none());
    assert_eq!(source.load_subject(agent)?.0.committed_step_no,0);
    assert_eq!(conn.query_row("SELECT count(*) FROM subject_identity_receipts WHERE step_id=?1",[step.step_id.to_string()],|row| row.get::<_,i64>(0))?,0);
    *chosen.lock() = Some(grant.clone());
    assert_eq!(source.recover_control_admission(&grant,&objects,true)?,ControlRecoveryOutcome::RecoveredCanonical);
    assert!(source.pending_control_reconciliations(agent)?.is_empty());
    Ok(())
}

/// One real S1 sequence with the installed authority fixture: unrelated large
/// history and large complete read ranges permit a bounded canonical result;
/// oversized actual deltas use the separate original-plan capacity consumers.
/// The row-engine journey separately verifies rollback and observer failures.
#[test]
fn canonical_s1_capacity_is_local_to_captured_rows_and_retains_the_decision() -> Result<()> {
    use shenko_types::{ids::EntityId, object::ObjectRef, subject::SubjectStepState};
    let dir = tempfile::tempdir()?;
    let source = Arc::new(Store::open(dir.path().join("head.db"), dir.path().join("cas"))?);
    let agent = initialize_subject_in_with_route(&source, b"capacity journey", None)?;
    let chosen = canonical_source(&source, agent, false)?;
    let conn = rusqlite::Connection::open(dir.path().join("head.db"))?;
    // This real table is never read or written by S1. Neither canonical
    // payload capture nor physical-write discovery scans its retained bytes.
    conn.execute_batch("CREATE TABLE capacity_history(id INTEGER PRIMARY KEY,payload BLOB);
        INSERT INTO capacity_history VALUES(1,zeroblob(129*1024*1024));")?;
    let (head, first) = source.load_subject(agent)?;
    let result = recorded_result(&source, &head, first)?;
    let next = EntityId::new();
    let step = committed_candidate(&source, result, next);
    let receipt = source.commit_step_and_advance_head(&step, &head, next)?;
    let grant = chosen.lock().clone().context("actual canonical admission")?;
    let envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&reference.digest)?)?;
    assert!(transition["tables"].as_array().context("canonical tables")?
        .iter().all(|table| table["before"]["table"]["name"] != "capacity_history"));
    let materialized: serde_json::Value = serde_json::from_str(&conn.query_row(
        "SELECT data FROM control_materializations WHERE action_id=?1", [grant.admission.action_id.to_string()],
        |row| row.get::<_, String>(0))?)?;
    assert_eq!(materialized["committed_at"], serde_json::to_value(receipt.commit_ref)?);
    assert_eq!(materialized["transition_ref"], serde_json::to_value(&reference)?);
    let (advanced, successor) = source.load_subject(agent)?;
    assert_eq!(advanced.next_step_id, next);
    assert_eq!(advanced.committed_step_id, Some(step.step_id));
    assert_eq!(materialized["logical_result"], serde_json::to_value(advanced.committed_at)?);
    assert_eq!(successor.created_by_step_id, Some(step.step_id));
    assert!(source.control_admission_applied_here(&grant.admission)?);
    assert_eq!(conn.query_row("SELECT length(payload) FROM capacity_history", [], |row| row.get::<_, i64>(0))?, 129*1024*1024);

    // A real trigger gives S1 a complete read dependency. Even histories
    // larger than the old retained-image or JSON bounds remain canonical.
    conn.execute_batch("CREATE TABLE capacity_extension(id INTEGER PRIMARY KEY,payload TEXT);
        CREATE TRIGGER capacity_read AFTER UPDATE ON subject_branch_heads BEGIN
            SELECT length(payload) FROM capacity_extension WHERE id=1; END;")?;
    for bytes in [65*1024*1024i64, 12*1024*1024i64] {
        conn.execute("INSERT OR REPLACE INTO capacity_extension VALUES(1,CAST(zeroblob(?1) AS TEXT))", [bytes])?;
        let (head, planned) = source.load_subject(agent)?;
        let result = recorded_result(&source, &head, planned)?;
        let next = EntityId::new();
        let step = committed_candidate(&source, result, next);
        let decision = step.decision_ref.as_ref().context("original decision")?;
        let original_decision = source.cas.read_object(&decision.digest)?;
        let receipt = source.commit_step_and_advance_head(&step, &head, next)?;
        let grant = chosen.lock().clone().context("actual bounded canonical admission")?;
        let envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&grant.admission.payload_ref.digest)?)?;
        let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
        assert_eq!(reference.codec.major, 2);
        assert!(reference.bytes < 1024 * 1024, "unchanged read history must not fill the admission");
        let transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&reference.digest)?)?;
        let range = transition["tables"].as_array().context("tables")?.iter()
            .find(|table| table["before"]["table"]["name"] == "capacity_extension").context("complete read range")?;
        assert_eq!(range["complete_read"], true);
        assert!(range["before"]["rows"].as_object().context("range rows")?.is_empty());
        assert!(range["read_range"]["before"].is_string());
        let logical: shenko_types::commit::CommitRef = serde_json::from_value(transition["result_commit"].clone())?;
        assert_eq!(source.cas.read_object(&decision.digest)?, original_decision);
        let stored = source.get_step(&head.branch_id, step.step_id)?.context("committed original step")?;
        let mut expected = step.clone();
        expected.commit_ref = Some(logical);
        assert_eq!(serde_json::to_value(&stored)?, serde_json::to_value(&expected)?);
        let (advanced, successor) = source.load_subject(agent)?;
        assert_eq!(advanced.committed_at, logical);
        assert_eq!(advanced.committed_step_id, Some(step.step_id));
        assert_eq!(successor.step_id, next);
        assert_eq!(successor.created_by_step_id, Some(step.step_id));
        assert_eq!(successor.state, SubjectStepState::Planned);
        assert_eq!(conn.query_row("SELECT count(*) FROM subject_steps WHERE json_extract(data,'$.created_by_step_id')=?1",
            [step.step_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
        let applied: serde_json::Value = serde_json::from_str(&conn.query_row(
            "SELECT data FROM control_materializations WHERE action_id=?1", [grant.admission.action_id.to_string()],
            |row| row.get::<_, String>(0))?)?;
        assert_eq!(applied["committed_at"], serde_json::to_value(receipt.commit_ref)?);
        assert_eq!(applied["transition_ref"], serde_json::to_value(reference)?);
        assert!(source.control_admission_applied_here(&grant.admission)?);
    }
    Ok(())
}

// Production limits, real trigger writes, original authority and ordinary
// receipts. A lost choose reply is an admission failure, never S1 success;
// recovery must use that same chosen original plan without canonical retry.
fn s1_capacity_original_plan_commits(retained_rows: bool, lose_reply: bool) -> Result<()> {
    use shenko_store::ControlRecoveryOutcome;
    use shenko_types::{ids::EntityId, subject::SubjectStepState};
    let installation = tempfile::tempdir()?;
    let dir = installation.path();
    let source = Arc::new(Store::open(dir.join("head.db"), dir.join("cas"))?);
    let agent = initialize_subject_in_with_route(&source, b"S1 two capacity limits", None)?;
    let chosen = canonical_source(&source, agent, lose_reply)?;
    let conn = rusqlite::Connection::open(dir.join("head.db"))?;
    conn.execute_batch("CREATE TABLE capacity_delta(id INTEGER PRIMARY KEY,payload);")?;
    let bytes = if retained_rows { 129 * 1024 * 1024i64 } else { 12 * 1024 * 1024i64 };
    if retained_rows {
        // One retained preimage exceeds 128 MiB before JSON is considered.
        conn.execute("INSERT INTO capacity_delta VALUES(1,zeroblob(?1))", [bytes])?;
        conn.execute_batch("CREATE TRIGGER capacity_write AFTER UPDATE ON subject_branch_heads BEGIN
            UPDATE capacity_delta SET payload=substr(payload,1,length(payload)-1) WHERE id=1; END;")?;
    } else {
        // Both 12 MiB NUL text images fit retained memory, but escaping each
        // NUL as six JSON bytes independently exceeds the 64 MiB image cap.
        conn.execute("INSERT INTO capacity_delta VALUES(1,CAST(zeroblob(?1) AS TEXT))", [bytes])?;
        conn.execute_batch("CREATE TRIGGER capacity_write AFTER UPDATE ON subject_branch_heads BEGIN
            UPDATE capacity_delta SET payload=payload||char(0) WHERE id=1; END;")?;
    }
    let (head, planned) = source.load_subject(agent)?;
    let result = recorded_result(&source, &head, planned)?;
    let next = EntityId::new();
    let step = committed_candidate(&source, result.clone(), next);
    let decision = step.decision_ref.as_ref().context("original decision")?;
    let original_decision = source.cas.read_object(&decision.digest)?;
    let before_seq = source.current_commit_seq()?;
    let receipt = source.commit_step_and_advance_head(&step, &head, next);
    let grant = chosen.lock().clone().context("ordinary original-plan admission was actually ordered")?;
    let envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    assert!(envelope["action"].get("transition_ref").is_none(), "not a canonical transition");
    assert_eq!(envelope["action"]["step"], serde_json::to_value(&step)?);
    assert_eq!(envelope["action"]["expected_head"], serde_json::to_value(&head)?);
    assert_eq!(envelope["action"]["next_step_id"], serde_json::to_value(next)?);
    assert_eq!(conn.query_row("SELECT count(*) FROM control_prepared_transitions", [], |row| row.get::<_, i64>(0))?, 0,
        "capacity rollback must leave no durable canonical preparation");
    let reports = source.session_events_range(EntityId(head.branch_id.0), 0, i64::MAX)?
        .into_iter().filter(|event| event.event_type == "runtime_error")
        .map(|event| serde_json::from_str::<serde_json::Value>(&event.data))
        .collect::<serde_json::Result<Vec<_>>>()?.into_iter()
        .filter(|event| event["kind"] == "store_capture_budget").collect::<Vec<_>>();
    assert_eq!(reports.len(), 1);
    let message = reports[0]["message"].as_str().context("precise capacity diagnostic")?;
    assert!(message.contains(if retained_rows { "128 MiB" } else { "64 MiB admission budget" }), "{message}");
    assert!(message.contains("admission and COMMIT are not yet confirmed"), "{message}");
    let committed = if lose_reply {
        let error = receipt.unwrap_err();
        assert!(error.to_string().contains("fixture lost chosen reply"), "{error}");
        assert_eq!(serde_json::to_value(source.load_subject(agent)?.0)?, serde_json::to_value(&head)?);
        assert_eq!(serde_json::to_value(source.get_step(&head.branch_id, step.step_id)?)?, serde_json::to_value(Some(&result))?);
        assert!(source.get_step(&head.branch_id, next)?.is_none());
        assert_eq!(conn.query_row("SELECT length(CAST(payload AS BLOB)) FROM capacity_delta", [], |row| row.get::<_, i64>(0))?, bytes);
        assert!(!source.control_admission_applied_here(&grant.admission)?);
        let objects = source.admission_objects(&grant.admission.payload_ref)?;
        assert_eq!(source.recover_control_admission(&grant, &objects, true)?, ControlRecoveryOutcome::RecoveredOriginal);
        source.load_subject(agent)?.0.committed_at
    } else {
        receipt?.commit_ref
    };
    assert!(committed.seq.0 > before_seq);
    assert_eq!(source.cas.read_object(&decision.digest)?, original_decision);
    let mut expected = step.clone();
    expected.commit_ref = Some(committed);
    assert_eq!(serde_json::to_value(source.get_step(&head.branch_id, step.step_id)?)?, serde_json::to_value(Some(expected))?);
    let (advanced, successor) = source.load_subject(agent)?;
    assert_eq!(advanced.committed_at, committed);
    assert_eq!(advanced.committed_step_id, Some(step.step_id));
    assert_eq!(advanced.next_step_id, next);
    assert_eq!(successor.step_id, next);
    assert_eq!(successor.created_by_step_id, Some(step.step_id));
    assert_eq!(successor.state, SubjectStepState::Planned);
    assert_eq!(conn.query_row("SELECT count(*) FROM subject_steps WHERE json_extract(data,'$.created_by_step_id')=?1",
        [step.step_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
    let applied: serde_json::Value = serde_json::from_str(&conn.query_row(
        "SELECT data FROM control_materializations WHERE action_id=?1", [grant.admission.action_id.to_string()],
        |row| row.get::<_, String>(0))?)?;
    assert_eq!(applied["admission"], serde_json::to_value(&grant.admission)?);
    assert_eq!(applied["committed_at"], serde_json::to_value(committed)?);
    assert!(applied.get("transition_ref").is_none());
    assert_eq!(conn.query_row("SELECT count(*) FROM commit_log WHERE ledger_id=?1 AND seq=?2",
        rusqlite::params![committed.ledger_id.to_string(), committed.seq.0 as i64], |row| row.get::<_, i64>(0))?, 1);
    assert!(source.control_admission_applied_here(&grant.admission)?);
    assert_eq!(conn.query_row("SELECT length(CAST(payload AS BLOB)) FROM capacity_delta", [], |row| row.get::<_, i64>(0))?,
        bytes + if retained_rows { -1 } else { 1 }, "exactly one committed trigger effect");
    assert_eq!(conn.query_row("SELECT count(*) FROM operations", [], |row| row.get::<_, i64>(0))?, 0);
    assert_eq!(conn.query_row("SELECT count(*) FROM attempts", [], |row| row.get::<_, i64>(0))?, 0);
    let watermark = source.current_commit_seq()?;
    let objects = source.admission_objects(&grant.admission.payload_ref)?;
    assert_eq!(source.recover_control_admission(&grant, &objects, true)?, ControlRecoveryOutcome::Applied);
    assert_eq!(source.current_commit_seq()?, watermark);
    drop(conn);
    drop(source);
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    assert!(reopened.control_admission_applied_here(&grant.admission)?);
    assert_eq!(reopened.load_subject(agent)?.0.committed_at, committed);
    Ok(())
}

#[test]
fn canonical_s1_retained_row_capacity_uses_original_ordinary_commit() -> Result<()> {
    for lose_reply in [false, true] { s1_capacity_original_plan_commits(true, lose_reply)?; }
    Ok(())
}

#[test]
fn canonical_s1_serialized_image_capacity_uses_original_ordinary_commit() -> Result<()> {
    for lose_reply in [false, true] { s1_capacity_original_plan_commits(false, lose_reply)?; }
    Ok(())
}


struct NativeT2Fixture {
    dir: PathBuf,
    store: Arc<Store>,
    agent: shenko_types::ids::EntityId,
    parent: shenko_store::NativeTaskClaim,
    child: Option<shenko_store::NativeTaskClaim>,
    sibling: Option<shenko_store::NativeTaskClaim>,
    state: Arc<parking_lot::Mutex<NativeT2AuthorityState>>,
    stamp: shenko_types::authority::CommitAuthorityStamp,
}

impl NativeT2Fixture {
    fn new(with_child: bool) -> Result<Self> {
        Self::with_tasks(with_child, false)
    }

    fn with_tasks(with_child: bool, with_sibling: bool) -> Result<Self> {
        use shenko_types::{authority::CommitAuthorityStamp, ids::EntityId, object::Digest, task::TaskOwner};
        let (dir, store, agent) = initialized_subject();
        let mut parent = recovery_task(&store, agent, 1, false);
        parent.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object"));
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(parent.clone());
        let child = if with_child {
            let mut child = recovery_task(&store, agent, 2, false);
            child.owner = TaskOwner::Task { id: parent.task_id };
            child.memos.insert(shenko_store::child::NATIVE_INPUT.into(), serde_json::json!("object"));
            batch.task_upserts.push(child.clone());
            Some(child)
        } else { None };
        // Bootstrap all requested tasks before authority is attached. Once
        // bound, a bare WriteBatch cannot admit a new independently owned task.
        let sibling = if with_sibling {
            let sibling = recovery_task(&store, agent, 3, false);
            batch.task_upserts.push(sibling.clone());
            Some(sibling)
        } else { None };
        store.commit(batch)?;
        let head = store.load_subject(agent)?.0;
        let stamp = CommitAuthorityStamp {
            source_id: "native-t2-source".into(), holder_id: EntityId::new(), subject_id: agent,
            branch_id: head.branch_id.clone(), revision: Digest::blake3(b"native T2 test authority"),
        };
        let state = Arc::new(parking_lot::Mutex::new(NativeT2AuthorityState::default()));
        let attach = |stamp| NativeT2Authority { source: "native-t2-source", stamp, state: Arc::clone(&state) };
        store.attach_commit_authority(Arc::new(attach(Some(stamp.clone()))))?;
        store.activate_runtime_authority(&head)?;
        let parent = store.claim_native_task(parent.task_id, parent.implementation_ref, false)?.context("parent claim")?;
        let child = child.map(|child| store.claim_native_task(child.task_id, child.implementation_ref, false))
            .transpose()?.flatten();
        let sibling = sibling.map(|sibling| store.claim_native_task(sibling.task_id, sibling.implementation_ref, false))
            .transpose()?.flatten();
        Ok(Self { dir, store, agent, parent, child, sibling, state, stamp })
    }

    fn reopen(&self) -> Result<Arc<Store>> {
        let store = Store::open(self.dir.join("head.db"), self.dir.join("cas"))?;
        store.attach_commit_authority(Arc::new(NativeT2Authority {
            source: "native-t2-source", stamp: Some(self.stamp.clone()), state: Arc::clone(&self.state),
        }))?;
        Ok(Arc::new(store))
    }

    fn chosen(&self) -> Result<shenko_types::authority::CommitAuthorityGrant> {
        let state = self.state.lock();
        state.grants.get(&state.last).cloned().context("retained native T2 grant")
    }
}

fn native_t2_evidence(store: &Store, claim: &shenko_store::NativeTaskClaim)
    -> Result<(shenko_types::operation::OperationRecord, shenko_types::evidence::EvidenceReceipt)>
{
    use shenko_store::child::NativeDispatchTransition;
    use shenko_types::{evidence::*, ids::EntityId, operation::*};
    let (operation_id, attempt_id) = arm_dispatch_operation(store, &claim.invocation, "native-result", claim.operation_id);
    assert_eq!(store.commit_native_dispatch(&claim.invocation, operation_id, attempt_id)?, NativeDispatchTransition::Won);
    let attempt = store.external_attempt(attempt_id)?;
    let raw = store.cas.seal_object(br#"{"observed":"original completion","value":7}"#,
        "application/json", "native-t2-test", "json", 1)?.object;
    let receipt = EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(operation_id), attempt_id: Some(attempt_id),
        source_id: "native-t2-test".into(), source_instance_id: "original-executor".into(),
        source_event_key: Some(format!("ended:{attempt_id}")), producer_epoch: Some(attempt.epoch.0), producer_schema_version: Some("1".into()),
        raw_ref: raw.clone(), decoder_ref: Some(attempt.implementation_ref.clone()), decode_status: DecodeStatus::Decoded,
        observation_level: Some(ObservationLevel::PostconditionVerified), normalized_ref: Some(raw), observed_at: Some(chrono::Utc::now()),
        received_commit: attempt.armed_commit,
    };
    let receipt_id = receipt.receipt_id;
    let operation = store.record_external_evidence(operation_id, attempt_id, receipt, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
    Ok((operation, store.external_evidence(receipt_id)?))
}

#[test]
fn canonical_first_claim_preserves_s1_birth_and_never_reissues_a_recovered_worker() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, child::{NATIVE_ASSIGNMENT, NATIVE_RECOVERY}, cognition::CognitionImportOptions};
    use shenko_types::{authority::CommitAuthorityStamp, ids::EntityId, object::{Digest, ObjectRef},
        subject::{DelegationIntent, StepDecision}, task::{DispatchEligibility, ImplementationRef, NativeTaskDefinition, TaskState}};

    let (dir, source, agent) = initialized_subject();
    let state = Arc::new(parking_lot::Mutex::new(NativeT2AuthorityState::default()));
    let head = source.load_subject(agent)?.0;
    let stamp = CommitAuthorityStamp { source_id: "native-claim-source".into(), holder_id: EntityId::new(), subject_id: agent,
        branch_id: head.branch_id.clone(), revision: Digest::blake3(b"first native claim authority") };
    source.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-claim-source", stamp: Some(stamp.clone()), state: Arc::clone(&state),
    }))?;
    source.activate_runtime_authority(&head)?;
    let chosen = || -> Result<shenko_types::authority::CommitAuthorityGrant> {
        let state = state.lock();
        state.grants.get(&state.last).cloned().context("retained chosen first claim")
    };
    let (head, first) = source.load_subject(agent)?;
    let result = recorded_result(&source, &head, first)?;
    let next = EntityId::new();
    let mut step = committed_candidate(&source, result, next);
    let mut decision: StepDecision = serde_json::from_slice(&source.cas.read_object(
        &step.decision_ref.as_ref().context("original decision")?.digest)?)?;
    let [fresh_id, retry_id, lost_id, restart_id] = std::array::from_fn(|_| EntityId::new());
    for task_id in [fresh_id, retry_id, lost_id, restart_id] {
        decision.delegated_task_intents.push(DelegationIntent { intent_id: task_id,
            goal: "Observe with the original native request".into(), system_prompt: String::new(), model: None,
            capabilities: vec![], budget_hint: None, reply_grant: None,
            definition: Some(NativeTaskDefinition { kind: "fixture.native.observe".into(), definition_version: 7,
                input: serde_json::json!({"source":"original S1 request", "task":task_id}) }),
        });
    }
    step.decision_ref = Some(source.cas.seal_object(&serde_json::to_vec(&decision)?,
        "application/json", "test", "StepDecision", 8)?.object);
    // The receiving installation retains its ordinarily parked root. Only
    // chosen S1 creates these new delegation rows on that distinct ledger.
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let before_birth_archive = archive.clone();
    let follower = Store::open(dir.join("claim-follower.db"), dir.join("claim-follower-cas"))?;
    follower.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    follower.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-claim-follower", stamp: None, state: Arc::clone(&state),
    }))?;
    let parked_root = follower.external_task(head.subject_root_task_id)?;
    assert!(matches!(parked_root.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    source.commit_step_and_advance_head(&step, &head, next)?;
    let birth = chosen()?;
    let birth_objects = source.admission_objects(&birth.admission.payload_ref)?;
    assert_eq!(follower.recover_control_admission(&birth, &birth_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert_ne!(source.ledger.ledger_id(), follower.ledger.ledger_id());
    let pending = source.external_task(fresh_id)?;
    assert_eq!(pending.record_revision.0, 0);
    assert_eq!(serde_json::to_value(follower.external_task(fresh_id)?)?, serde_json::to_value(&pending)?);

    // A later ordinary archive carries the source's applied fact, but parks
    // the target and does not carry this installation's application receipt.
    archive.clear();
    source.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let imported = Store::open(dir.join("claim-parked.db"), dir.join("claim-parked-cas"))?;
    imported.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    imported.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-claim-parked", stamp: None, state: Arc::clone(&state),
    }))?;
    let parked = imported.external_task(fresh_id)?;
    assert!(matches!(parked.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    assert!(imported.control_admission_applied(&birth.admission)?);
    assert!(!imported.control_admission_applied_here(&birth.admission)?);
    let implementation = ImplementationRef { plugin_id: EntityId::new(), artifact_digest: Digest::blake3(b"native claim implementation"), registry_revision: 1 };
    // A trigger can introduce a different step predicate. The direct-reader
    // proof does not cover it, so refuse before preparing or choosing a claim.
    let injected = rusqlite::Connection::open(dir.join("head.db"))?;
    injected.execute_batch("CREATE TRIGGER claim_subject_dependency AFTER UPDATE ON tasks BEGIN
        SELECT data FROM subject_steps; END;")?;
    let grants_before = serde_json::to_value(&state.lock().grants)?;
    let refused = source.claim_native_task(fresh_id, implementation.clone(), false);
    injected.execute_batch("DROP TRIGGER claim_subject_dependency")?;
    assert!(matches!(&refused, Err(error) if error.to_string().contains("cannot narrow a trigger or view read")), "{refused:#?}");
    assert_eq!(serde_json::to_value(&state.lock().grants)?, grants_before);
    assert_eq!(serde_json::to_value(source.external_task(fresh_id)?)?, serde_json::to_value(&pending)?);
    let claim_action = EntityId::derived("commit-admission-action-v1", format!("task/{fresh_id}/claim/0").as_bytes());
    assert_eq!(injected.query_row("SELECT count(*) FROM control_prepared_transitions WHERE action_id=?1",
        [claim_action.to_string()], |row| row.get::<_, i64>(0))?, 0);
    let fresh = source.claim_native_task(fresh_id, implementation.clone(), false)?.context("one current claim permit")?;
    let claim_grant = chosen()?;
    let claim_objects = source.admission_objects(&claim_grant.admission.payload_ref)?;
    let envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&claim_grant.admission.payload_ref.digest)?)?;
    assert_eq!(envelope["action"]["kind"], "native.claim");
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&reference.digest)?)?;
    assert_eq!(reference.codec.major, 3);
    assert_eq!(transition["rows"]["schema_version"], 4);
    assert_eq!(transition["plan"]["task"], serde_json::to_value(&pending)?);
    assert_eq!(transition["plan"]["birth"]["admission"], serde_json::to_value(&birth.admission)?);
    assert_eq!(transition["plan"]["invocation"], serde_json::to_value(&fresh.invocation)?);
    assert_eq!(transition["rows"]["result_commit"], serde_json::to_value(fresh.task.updated_commit)?);
    assert_eq!(transition["rows"]["created_at"], serde_json::to_value(fresh.task.first_run_at)?);
    assert_eq!(fresh.task.updated_commit.ledger_id, source.ledger.ledger_id());
    assert_eq!(fresh.task.memos[NATIVE_ASSIGNMENT]["assigned_commit"], serde_json::to_value(fresh.task.updated_commit)?);
    let task_table = transition["rows"]["tables"].as_array().context("claim tables")?.iter()
        .find(|table| table["before"]["table"]["name"] == "tasks").context("target task dependency")?;
    assert_eq!(task_table["complete_read"], false);
    assert_eq!(task_table["before"]["rows"].as_object().context("claim task preimage")?.len(), 1);
    let watermark = source.current_commit_seq()?;
    assert!(source.claim_native_task(fresh_id, implementation.clone(), false)?.is_none());
    assert_eq!(source.current_commit_seq()?, watermark);
    // Each refusal owns a separate real receiver: import the pre-birth archive
    // and locally materialize the original S1 before presenting its claim.
    // Corrupt only after recovery's initial subject read, at grant acquisition.
    // Keep the corruption and durable reconciliation fact; restoring a JSON
    // row would neither restore every SQL cell nor erase the observed debt.
    // The original follower remains untouched for the successful learned path.
    for fault in ["missing-successor", "wrong-successor-binding", "changed-creator"] {
        let db = dir.join(format!("claim-{fault}.db"));
        let refused_receiver = Store::open(db.clone(), dir.join(format!("claim-{fault}-cas")))?;
        refused_receiver.import_cognition_archive(&before_birth_archive[..], &CognitionImportOptions::default())?;
        refused_receiver.attach_commit_authority(Arc::new(NativeT2Authority {
            source: "native-claim-follower", stamp: None, state: Arc::clone(&state),
        }))?;
        assert_eq!(refused_receiver.recover_control_admission(&birth, &birth_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
        let successor = refused_receiver.load_subject(agent)?.0.next_step_id;
        let id = if fault == "changed-creator" { step.step_id } else { successor };
        state.lock().before_materialize = Some(Box::new(move || {
            let connection = rusqlite::Connection::open(db)?;
            if fault == "missing-successor" {
                assert_eq!(connection.execute("DELETE FROM subject_steps WHERE step_id=?1", [id.to_string()])?, 1);
            } else {
                let data: String = connection.query_row("SELECT data FROM subject_steps WHERE step_id=?1",
                    [id.to_string()], |row| row.get(0))?;
                let mut current: shenko_types::subject::SubjectStep = serde_json::from_str(&data)?;
                if fault == "changed-creator" { current.record_revision.0 += 1; }
                else { current.step_no += 1; }
                assert_eq!(connection.execute("UPDATE subject_steps SET data=?1 WHERE step_id=?2",
                    rusqlite::params![serde_json::to_string(&current)?, id.to_string()])?, 1);
            }
            Ok(())
        }));
        let refused = refused_receiver.recover_control_admission(&claim_grant, &claim_objects, false);
        assert!(state.lock().before_materialize.is_none(), "{fault}: application boundary was not reached");
        assert!(matches!(&refused, Ok(ControlRecoveryOutcome::ReconciliationRequired)), "{fault}: {refused:?}");
        assert!(!refused_receiver.control_admission_applied_here(&claim_grant.admission)?);
        assert_eq!(serde_json::to_value(refused_receiver.external_task(fresh_id)?)?, serde_json::to_value(&pending)?);
        let debts = refused_receiver.pending_control_reconciliations(agent)?;
        assert_eq!(debts.len(), 1, "{fault}: preserve the refused chosen fact");
        assert_eq!(debts[0].payload["grant_id"], serde_json::to_value(&claim_grant.grant_id)?);
    }
    assert_eq!(follower.recover_control_admission(&claim_grant, &claim_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert_eq!(serde_json::to_value(follower.external_task(fresh_id)?)?, serde_json::to_value(&fresh.task)?);
    assert_eq!(serde_json::to_value(follower.external_task(head.subject_root_task_id)?)?, serde_json::to_value(&parked_root)?);
    assert_eq!(serde_json::to_value(follower.external_task(retry_id)?)?, serde_json::to_value(source.external_task(retry_id)?)?);
    assert!(follower.claim_native_task(fresh_id, implementation.clone(), false)?.is_none());
    let watermark = follower.current_commit_seq()?;
    assert_eq!(follower.recover_control_admission(&claim_grant, &claim_objects, false)?, ControlRecoveryOutcome::Applied);
    assert_eq!(follower.current_commit_seq()?, watermark);
    assert_eq!(imported.recover_control_admission(&claim_grant, &claim_objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert_eq!(serde_json::to_value(imported.external_task(fresh_id)?)?, serde_json::to_value(&parked)?);
    assert!(!imported.control_admission_applied_here(&claim_grant.admission)?);
    assert!(imported.claim_native_task(fresh_id, implementation.clone(), false)?.is_none());
    assert_eq!(imported.pending_control_reconciliations(agent)?.len(), 1);

    // First actual choose does not make an older local preparation fresh.
    let before_retry = source.external_task(retry_id)?;
    state.lock().fail_before_choose = true;
    let refused = source.claim_native_task(retry_id, implementation.clone(), false);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture failed before choosing dispatch")), "{refused:#?}");
    assert_eq!(serde_json::to_value(source.external_task(retry_id)?)?, serde_json::to_value(before_retry)?);
    assert!(source.claim_native_task(retry_id, implementation.clone(), false)?.is_none());
    assert!(!chosen()?.replayed);
    assert!(matches!(source.external_task(retry_id)?.state, TaskState::Running { .. }));

    // Even a retained grant cannot omit the typed absence dependency. Claim 3
    // rejects the missing row-4 selector before checking the dependency image,
    // with no Running write or materialization marker.
    let before_lost = source.external_task(lost_id)?;
    state.lock().lose_reply = true;
    let refused = source.claim_native_task(lost_id, implementation.clone(), false);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture lost chosen T2 reply")), "{refused:#?}");
    let lost = chosen()?;
    let objects = source.admission_objects(&lost.admission.payload_ref)?;
    let mut envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&lost.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let mut transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&reference.digest)?)?;
    assert_eq!(reference.codec.major, 3);
    assert_eq!(transition["rows"]["schema_version"], 4);
    let tables = transition["rows"]["tables"].as_array_mut().context("lost claim tables")?;
    let operations = tables.iter().find(|table| table["before"]["table"]["name"] == "operations")
        .context("original operation absence dependency")?;
    assert_eq!(operations["read_selector"]["kind"], "original_operation");
    tables.retain(|table| table["before"]["table"]["name"] != "operations");
    assert!(tables.iter().all(|table| table["before"]["table"]["name"] != "operations"));
    let fault_ref = source.cas.seal_object(&serde_json::to_vec(&transition)?, "application/json",
        "commit-authority", "store-native-claim-transition", reference.codec.major)?.object;
    envelope["action"]["transition_ref"] = serde_json::to_value(fault_ref)?;
    let mut fault = lost.clone();
    fault.admission.payload_ref = source.cas.seal_object(&serde_json::to_vec(&envelope)?, "application/json",
        "commit-authority", "commit-admission", 1)?.object;
    state.lock().grants.insert(fault.admission.conflict_key.clone(), fault.clone());
    let malformed = source.recover_control_admission(&fault, &source.admission_objects(&fault.admission.payload_ref)?, true);
    assert!(matches!(&malformed, Err(error) if error.to_string().contains("native read selectors differ from the original action plan")), "{malformed:#?}");
    assert_eq!(serde_json::to_value(source.external_task(lost_id)?)?, serde_json::to_value(before_lost)?);
    assert!(!source.control_admission_applied_here(&fault.admission)?);
    state.lock().grants.insert(lost.admission.conflict_key.clone(), lost.clone());
    assert_eq!(source.recover_control_admission(&lost, &objects, true)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert!(source.claim_native_task(lost_id, implementation.clone(), false)?.is_none());

    // A source crash before choose retains its original worker/timestamp. The
    // preactivation recovery installs that fact without granting a new worker.
    state.lock().fail_before_choose = true;
    let refused = source.claim_native_task(restart_id, implementation.clone(), false);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture failed before choosing dispatch")), "{refused:#?}");
    let connection = rusqlite::Connection::open(dir.join("head.db"))?;
    let prepared_ref: String = connection.query_row("SELECT transition_ref FROM control_prepared_transitions WHERE action_id=?1",
        [EntityId::derived("commit-admission-action-v1", format!("task/{restart_id}/claim/0").as_bytes()).to_string()], |row| row.get(0))?;
    let prepared_ref: ObjectRef = serde_json::from_str(&prepared_ref)?;
    let prepared: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&prepared_ref.digest)?)?;
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    reopened.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-claim-source", stamp: Some(stamp), state: Arc::clone(&state),
    }))?;
    let recovery = reopened.resume_prepared_step_commits(agent)?;
    assert_eq!(recovery.applied, 1);
    assert!(recovery.unresolved.is_empty());
    assert!(!chosen()?.replayed);
    let installed = reopened.external_task(restart_id)?;
    assert_eq!(serde_json::to_value(installed.updated_commit)?, prepared["rows"]["result_commit"]);
    assert_eq!(serde_json::to_value(installed.first_run_at)?, prepared["rows"]["created_at"]);
    assert_eq!(installed.memos[NATIVE_ASSIGNMENT]["worker_id"], prepared["plan"]["invocation"]["worker_id"]);
    assert_ne!(installed.memos[NATIVE_ASSIGNMENT]["worker_id"], serde_json::to_value(reopened.capabilities.incarnation)?);
    assert!(reopened.claim_native_task(restart_id, implementation.clone(), false)?.is_none());
    assert_eq!(reopened.resume_prepared_step_commits(agent)?.applied, 0);
    // The existing, separately ordered worker-loss protocol makes progress;
    // it does not upgrade the historical first claim into another permit.
    let head = reopened.load_subject(agent)?.0;
    reopened.activate_runtime_authority(&head)?;
    assert_eq!(reopened.recover_native_tasks()?, 4);
    let pending = reopened.external_task(restart_id)?;
    assert_eq!(pending.record_revision.0, 2);
    assert_eq!(pending.memos[NATIVE_RECOVERY], true);
    let new_claim = reopened.claim_native_task(restart_id, implementation, false)?.context("separate no-operation recovery invocation")?;
    assert_eq!(new_claim.task.record_revision.0, 3);
    assert_ne!(serde_json::to_value(new_claim.invocation.invocation_id)?, prepared["plan"]["invocation"]["invocation_id"]);
    assert_eq!(new_claim.invocation.worker_id, reopened.capabilities.incarnation);
    assert_eq!(new_claim.task.first_run_at, installed.first_run_at);
    for database in ["head.db", "claim-follower.db", "claim-parked.db"] {
        let connection = rusqlite::Connection::open(dir.join(database))?;
        assert_eq!(connection.query_row("SELECT count(*) FROM operations", [], |row| row.get::<_, i64>(0))?, 0);
        assert_eq!(connection.query_row("SELECT count(*) FROM attempts", [], |row| row.get::<_, i64>(0))?, 0);
    }
    Ok(())
}

#[test]
fn canonical_direct_native_chain_retains_original_e1_and_only_one_effect() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, child::NativeDispatchTransition::AlreadyCommitted,
        cognition::CognitionImportOptions};
    use shenko_types::{authority::CommitAuthorityStamp, evidence::*, ids::EntityId, object::{Digest, ObjectRef},
        operation::*, subject::{DelegationIntent, StepDecision}, task::{DispatchEligibility, ImplementationRef, NativeTaskDefinition}};
    use std::io::Write;

    let (dir, source, agent) = initialized_subject();
    eprintln!("issue316-native-capacity {}", serde_json::json!({
        "phase": "world", "path": dir, "agent_id": agent,
    }));
    let state = Arc::new(parking_lot::Mutex::new(NativeT2AuthorityState::default()));
    let head = source.load_subject(agent)?.0;
    let stamp = CommitAuthorityStamp { source_id: "native-chain-source".into(), holder_id: EntityId::new(), subject_id: agent,
        branch_id: head.branch_id.clone(), revision: Digest::blake3(b"direct native chain authority") };
    source.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-chain-source", stamp: Some(stamp.clone()), state: Arc::clone(&state),
    }))?;
    source.activate_runtime_authority(&head)?;
    let chosen = || -> Result<shenko_types::authority::CommitAuthorityGrant> {
        let state = state.lock();
        state.grants.get(&state.last).cloned().context("retained direct native grant")
    };
    let (head, first) = source.load_subject(agent)?;
    let result = recorded_result(&source, &head, first)?;
    let next = EntityId::new();
    let mut step = committed_candidate(&source, result, next);
    let mut decision: StepDecision = serde_json::from_slice(&source.cas.read_object(
        &step.decision_ref.as_ref().context("original decision")?.digest)?)?;
    let task_id = EntityId::new();
    let delayed_id = EntityId::new();
    let independent_id = EntityId::new();
    for id in [task_id, delayed_id, independent_id] {
        decision.delegated_task_intents.push(DelegationIntent { intent_id: id,
            goal: "Append one observed local effect".into(), system_prompt: String::new(), model: None,
            capabilities: vec![], budget_hint: None, reply_grant: None,
            definition: Some(NativeTaskDefinition { kind: "fixture.native.append".into(), definition_version: 1,
                input: serde_json::json!({"task":id}) }),
        });
    }
    step.decision_ref = Some(source.cas.seal_object(&serde_json::to_vec(&decision)?,
        "application/json", "test", "StepDecision", 8)?.object);
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let receiver = Store::open(dir.join("chain-receiver.db"), dir.join("chain-receiver-cas"))?;
    let blocked = Store::open(dir.join("chain-blocked.db"), dir.join("chain-blocked-cas"))?;
    for replica in [&receiver, &blocked] {
        replica.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
        replica.attach_commit_authority(Arc::new(NativeT2Authority {
            source: "native-chain-follower", stamp: None, state: Arc::clone(&state),
        }))?;
    }
    let parked_root = receiver.external_task(head.subject_root_task_id)?;
    assert!(matches!(parked_root.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    source.commit_step_and_advance_head(&step, &head, next)?;
    let birth = chosen()?;
    let birth_objects = source.admission_objects(&birth.admission.payload_ref)?;
    for replica in [&receiver, &blocked] {
        assert_eq!(replica.recover_control_admission(&birth, &birth_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
    }
    // Issue #316: keep real unrelated bytes throughout the existing native
    // claim -> paired T1/dispatch -> E1 -> chosen T2/restart journey. Use the
    // production Store budgets; do not substitute a smaller test threshold.
    // Insert after S1 birth so a passing S1 alone cannot satisfy this window.
    let history = rusqlite::Connection::open(dir.join("head.db"))?;
    history.execute_batch("CREATE TABLE native_capacity_history(id INTEGER PRIMARY KEY,payload BLOB);
        INSERT INTO native_capacity_history VALUES(1,zeroblob(129*1024*1024));")?;
    let history_bytes = 129 * 1024 * 1024i64;
    let assert_history = || -> Result<()> {
        let (count, bytes, unchanged): (i64, i64, bool) = history.query_row(
            "SELECT count(*),length(payload),payload=zeroblob(129*1024*1024) FROM native_capacity_history WHERE id=1",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        assert_eq!((count, bytes, unchanged), (1, history_bytes, true));
        Ok(())
    };
    // Read actual Store requests and local atomic receipts, including the
    // physical COMMIT. Emit identities for the parent's retained raw log.
    let capacity_receipt = |store: &Store, grant: &shenko_types::authority::CommitAuthorityGrant,
                            phase: &str, elapsed: std::time::Duration| -> Result<()> {
        assert_history()?;
        assert!(store.control_admission_applied_here(&grant.admission)?);
        let envelope: serde_json::Value = serde_json::from_slice(
            &store.cas.read_object(&grant.admission.payload_ref.digest)?)?;
        let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
        let frozen: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&reference.digest)?)?;
        assert!(frozen["rows"]["tables"].as_array().context("native transition tables")?.iter()
            .all(|table| table["before"]["table"]["name"] != "native_capacity_history"));
        assert_eq!(frozen["rows"]["schema_version"], 4);
        let creator = frozen["rows"]["tables"].as_array().context("native transition tables")?.iter()
            .find(|table| table["before"]["table"]["name"] == "subject_steps").context("creator point witness")?;
        assert_eq!(creator["complete_read"], false);
        assert_eq!(creator["before"]["rows"].as_object().context("creator rows")?.len(), 1);
        assert!(creator["changes"].as_array().context("creator writes")?.is_empty());
        let conn = rusqlite::Connection::open(dir.join("head.db"))?;
        let materialized: serde_json::Value = serde_json::from_str(&conn.query_row(
            "SELECT data FROM control_materializations WHERE action_id=?1",
            [grant.admission.action_id.to_string()], |row| row.get::<_, String>(0))?)?;
        assert_eq!(materialized["transition_ref"], serde_json::to_value(&reference)?);
        let commit: shenko_types::commit::CommitRef = serde_json::from_value(materialized["committed_at"].clone())?;
        assert_eq!(commit.ledger_id, store.ledger.ledger_id());
        assert_eq!(conn.query_row("SELECT count(*) FROM commit_log WHERE ledger_id=?1 AND seq=?2",
            rusqlite::params![commit.ledger_id.to_string(), commit.seq.0], |row| row.get::<_, i64>(0))?, 1);
        eprintln!("issue316-native-capacity {}", serde_json::json!({
            "phase": phase, "elapsed_ms": elapsed.as_millis(), "history_bytes": history_bytes,
            "admission": grant.admission, "receipt": materialized,
        }));
        Ok(())
    };
    let implementation = ImplementationRef { plugin_id: EntityId::new(), artifact_digest: Digest::blake3(b"bounded append implementation"), registry_revision: 1 };
    // A separate real Store operation can receive E1 while the target's
    // chosen claim/T1 is waiting to materialize. Its unknown observation grants
    // no success and no local effect, and remains outside the target witness.
    let independent = source.claim_native_task(independent_id, implementation.clone(), false)?.context("independent source claim")?;
    let (independent_ctx, mut independent_op, mut independent_attempt) = draft_dispatch_operation(
        &source, &independent.invocation, "independent-observation", independent.operation_id)?;
    assert!(source.arm_native_operation_for_dispatch(&independent_ctx, &mut independent_op, &mut independent_attempt)?.is_none());
    let independent_raw = source.cas.seal_object(b"observation ended without a known effect",
        "text/plain", "native-chain", "text", 1)?.object;
    let independent_receipt = EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(),
        operation_id: Some(independent_op.operation_id), attempt_id: Some(independent_attempt.attempt_id),
        source_id: "independent-native-chain-observation".into(), source_instance_id: "fixture-observer".into(),
        source_event_key: Some("independent-before-target-claim".into()), producer_epoch: Some(independent_attempt.epoch.0),
        producer_schema_version: Some("1".into()), raw_ref: independent_raw, decoder_ref: Some(independent_attempt.implementation_ref.clone()),
        decode_status: DecodeStatus::Decoded, observation_level: None,
        normalized_ref: None, observed_at: Some(chrono::Utc::now()), received_commit: independent_attempt.armed_commit,
    };
    // Reproduce the real interleaving: capture the claim while the successor
    // is Planned, then advance that same step through real S0/result Store
    // writes after choose and before materialization. The head and committed
    // creator remain unchanged. Followers deliberately retain Planned across
    // claim, paired arm and T2, so all three learned readers face different
    // successor progress without receiving another execution permit.
    let progressing = Arc::clone(&source);
    let observation = independent_receipt.clone();
    state.lock().before_materialize = Some(Box::new(move || {
        let (head, successor) = progressing.load_subject(agent)?;
        assert_eq!(successor.state, shenko_types::subject::SubjectStepState::Planned);
        recorded_result(&progressing, &head, successor)?;
        progressing.record_external_evidence(independent_op.operation_id, independent_attempt.attempt_id,
            observation, None, EffectStatus::Unknown)?;
        Ok(())
    }));
    let claim_started = std::time::Instant::now();
    let claim = source.claim_native_task(task_id, implementation.clone(), false)?.context("fresh source claim")?;
    assert!(state.lock().before_materialize.is_none());
    assert_eq!(source.external_attempt(independent_attempt.attempt_id)?.final_receipt_id, Some(independent_receipt.receipt_id));
    assert_eq!(source.external_operation(independent_op.operation_id)?.effect_status, EffectStatus::Unknown);
    assert_eq!(source.load_subject(agent)?.1.state, shenko_types::subject::SubjectStepState::ResultRecorded);
    assert_eq!(receiver.load_subject(agent)?.1.state, shenko_types::subject::SubjectStepState::Planned);
    let claim_grant = chosen()?;
    capacity_receipt(&source, &claim_grant, "native.claim", claim_started.elapsed())?;
    let claim_objects = source.admission_objects(&claim_grant.admission.payload_ref)?;
    for replica in [&receiver, &blocked] {
        assert_eq!(replica.recover_control_admission(&claim_grant, &claim_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
    }

    // One actual local effect follows only the fresh paired T1 permit. The
    // receiver installs the original armed intent, and never obtains None.
    let (ctx, mut operation, mut attempt) = draft_dispatch_operation(&source, &claim.invocation, "native-chain", claim.operation_id)?;
    let injected = rusqlite::Connection::open(dir.join("head.db"))?;
    injected.execute_batch("CREATE VIEW indirect_native_attempts AS SELECT data FROM attempts;
        CREATE TRIGGER arm_attempt_dependency AFTER UPDATE ON tasks BEGIN
            SELECT data FROM indirect_native_attempts; END;")?;
    let grants_before = serde_json::to_value(&state.lock().grants)?;
    let refused = source.arm_native_operation_for_dispatch(&ctx, &mut operation.clone(), &mut attempt.clone());
    injected.execute_batch("DROP TRIGGER arm_attempt_dependency; DROP VIEW indirect_native_attempts")?;
    assert!(matches!(&refused, Err(error) if error.to_string().contains("cannot narrow a trigger or view read")), "{refused:#?}");
    assert_eq!(serde_json::to_value(&state.lock().grants)?, grants_before);
    assert_eq!(injected.query_row("SELECT count(*) FROM operations WHERE operation_id=?1",
        [operation.operation_id.to_string()], |row| row.get::<_, i64>(0))?, 0);
    let progressing = Arc::clone(&source);
    let observation = EvidenceReceipt {
        receipt_id: EntityId::new(), source_event_key: Some("independent-before-target-arm".into()),
        observed_at: Some(chrono::Utc::now()), ..independent_receipt.clone()
    };
    let arm_receipt_id = observation.receipt_id;
    state.lock().before_materialize = Some(Box::new(move || {
        progressing.record_external_evidence(independent_op.operation_id, independent_attempt.attempt_id,
            observation, None, EffectStatus::Unknown)?;
        Ok(())
    }));
    let arm_started = std::time::Instant::now();
    let permit = source.arm_native_operation_for_dispatch(&ctx, &mut operation, &mut attempt)?;
    assert!(permit.is_none());
    assert!(state.lock().before_materialize.is_none());
    assert_eq!(source.external_attempt(independent_attempt.attempt_id)?.final_receipt_id, Some(arm_receipt_id));
    assert_eq!(source.external_operation(independent_op.operation_id)?.effect_status, EffectStatus::Unknown);
    if permit.is_none() {
        let mut effect = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("effect.log"))?;
        effect.write_all(b"effect\n")?;
        effect.sync_all()?;
    }
    let arm = chosen()?;
    capacity_receipt(&source, &arm, "native.arm_and_dispatch", arm_started.elapsed())?;
    let arm_objects = source.admission_objects(&arm.admission.payload_ref)?;
    let envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&arm.admission.payload_ref.digest)?)?;
    let arm_ref: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    assert_eq!(arm_ref.codec.major, 4);
    // Independent receivers preserve each refused chosen fact. A concurrent
    // logical-key collision and a JSON UUID alias under another physical PK
    // must both invalidate absence; neither failure is repaired for reuse.
    for fault in ["logical-key", "json-uuid-alias"] {
        let db = dir.join(format!("arm-{fault}.db"));
        let refused_receiver = Store::open(db.clone(), dir.join(format!("arm-{fault}-cas")))?;
        refused_receiver.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
        refused_receiver.attach_commit_authority(Arc::new(NativeT2Authority {
            source: "native-chain-follower", stamp: None, state: Arc::clone(&state),
        }))?;
        for (grant, originals) in [(&birth, &birth_objects), (&claim_grant, &claim_objects)] {
            assert_eq!(refused_receiver.recover_control_admission(grant, originals, false)?, ControlRecoveryOutcome::RecoveredCanonical);
        }
        let before = refused_receiver.external_task(task_id)?;
        let sequence = refused_receiver.current_commit_seq()?;
        let mut collision = operation.clone();
        collision.operation_id = EntityId::new();
        let physical_id = collision.operation_id;
        let mut collision = serde_json::to_value(collision)?;
        if fault == "json-uuid-alias" {
            collision["operation_id"] = operation.operation_id.to_string().to_uppercase().into();
            collision["logical_key"] = "independent-physical-row".into();
            assert_eq!(serde_json::from_value::<OperationRecord>(collision.clone())?.operation_id, operation.operation_id);
        }
        state.lock().before_materialize = Some(Box::new(move || {
            let conn = rusqlite::Connection::open(db)?;
            conn.execute("INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq,created_at) VALUES(?1,0,?2,?3,?4)",
                rusqlite::params![physical_id.to_string(), serde_json::to_string(&collision)?, sequence, chrono::Utc::now().to_rfc3339()])?;
            Ok(())
        }));
        assert_eq!(refused_receiver.recover_control_admission(&arm, &arm_objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
        assert!(state.lock().before_materialize.is_none());
        assert!(!refused_receiver.control_admission_applied_here(&arm.admission)?);
        assert_eq!(serde_json::to_value(refused_receiver.external_task(task_id)?)?, serde_json::to_value(before)?);
        assert!(refused_receiver.external_operation_attempts(operation.operation_id)?.is_empty());
        assert_eq!(refused_receiver.pending_control_reconciliations(agent)?.len(), 1);
    }
    for replica in [&receiver, &blocked] {
        assert_eq!(replica.recover_control_admission(&arm, &arm_objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
        assert!(replica.arm_native_operation_for_dispatch(&ctx, &mut operation.clone(), &mut attempt.clone())?.is_some());
        assert_eq!(replica.commit_native_dispatch(&ctx, operation.operation_id, attempt.attempt_id)?, AlreadyCommitted);
        assert_eq!(serde_json::to_value(replica.external_task(task_id)?)?, serde_json::to_value(source.external_task(task_id)?)?);
    }
    assert_eq!(serde_json::to_value(receiver.external_task(head.subject_root_task_id)?)?, serde_json::to_value(&parked_root)?);
    assert!(receiver.external_operation_evidence(operation.operation_id)?.is_empty());

    // An ordinary archive still parks the target. Copied applied facts do
    // not manufacture a local first-claim/T1 application on that installation.
    archive.clear();
    source.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let imported = Store::open(dir.join("chain-imported.db"), dir.join("chain-imported-cas"))?;
    imported.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    imported.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-chain-imported", stamp: None, state: Arc::clone(&state),
    }))?;
    let parked = imported.external_task(task_id)?;
    assert!(matches!(parked.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    assert!(!imported.control_admission_applied_here(&arm.admission)?);

    let observed = std::fs::read(dir.join("effect.log"))?;
    assert_eq!(observed, b"effect\n");
    let raw = source.cas.seal_object(&observed, "text/plain", "native-chain", "text", 1)?.object;
    let normalized = source.cas.seal_object(&serde_json::to_vec(&serde_json::json!({"observed_bytes":observed}))?,
        "application/json", "native-chain", "json", 1)?.object;
    let receipt_id = EntityId::new();
    let receipt = EvidenceReceipt { record_schema_version: 1, receipt_id,
        operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
        source_id: "native-chain-file".into(), source_instance_id: "original-source-executor".into(),
        source_event_key: Some(format!("append:{}", attempt.attempt_id)), producer_epoch: Some(attempt.epoch.0),
        producer_schema_version: Some("1".into()), raw_ref: raw, decoder_ref: Some(attempt.implementation_ref.clone()),
        decode_status: DecodeStatus::Decoded, observation_level: Some(ObservationLevel::PostconditionVerified),
        normalized_ref: Some(normalized), observed_at: Some(chrono::Utc::now()), received_commit: attempt.armed_commit };
    let completed = source.record_external_evidence(operation.operation_id, attempt.attempt_id, receipt,
        Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
    let original_receipt = source.external_evidence(receipt_id)?;
    let original_attempt = source.external_attempt(attempt.attempt_id)?;
    assert_eq!(original_receipt.received_commit.ledger_id, source.ledger.ledger_id());
    assert_eq!(completed.evidence_through, Some(original_receipt.received_commit));
    let armed_task = receiver.external_task(task_id)?;

    // The provider's actual fact is already committed when the T2 chosen
    // reply is lost. The receiver only has its original paired T1 so far.
    state.lock().lose_reply = true;
    let complete_started = std::time::Instant::now();
    let lost = source.complete_native_task(&claim, &completed);
    assert!(matches!(&lost, Err(error) if error.to_string().contains("fixture lost chosen T2 reply")), "{lost:#?}");
    assert_history()?;
    eprintln!("issue316-native-capacity {}", serde_json::json!({
        "phase": "native.complete.lost_reply", "elapsed_ms": complete_started.elapsed().as_millis(),
        "error": lost.as_ref().err().context("retained lost reply")?.to_string(),
        "operation_id": operation.operation_id, "attempt_id": attempt.attempt_id, "receipt_id": receipt_id,
    }));
    let t2 = chosen()?;
    assert!(!source.control_admission_applied_here(&t2.admission)?);
    let objects = source.admission_objects(&t2.admission.payload_ref)?;
    let mut envelope: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&t2.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    assert_eq!(reference.codec.major, 4);
    let transition: serde_json::Value = serde_json::from_slice(&source.cas.read_object(&reference.digest)?)?;
    let expected_result: shenko_types::task::TaskResult = serde_json::from_value(transition["result"].clone())?;
    assert!(source.native_task_result(task_id)?.is_none());
    assert!(receiver.external_operation_evidence(operation.operation_id)?.is_empty());

    // Unsupported E1 readers and selectors naming another attempt are fatal.
    // No fallback may reinterpret them as
    // the original chosen T2 or reconstruct different receipt provenance.
    for fault in ["evidence-reader", "wrong-attempt"] {
        let mut malformed = transition.clone();
        let expected_error = match fault {
            "evidence-reader" => {
                malformed["evidence_fact"]["codec"]["major"] = 99.into();
                "unsupported typed object"
            }
            _ => {
                let attempts = malformed["rows"]["tables"].as_array_mut().context("T2 tables")?.iter_mut()
                    .find(|table| table["before"]["table"]["name"] == "attempts").context("original attempt selector")?;
                attempts["read_selector"]["attempt_id"] = serde_json::to_value(EntityId::new())?;
                "native read selectors differ"
            }
        };
        let bad_ref = source.cas.seal_object(&serde_json::to_vec(&malformed)?, "application/json", "commit-authority", "store-native-t2-transition", reference.codec.major)?.object;
        envelope["action"]["transition_ref"] = serde_json::to_value(bad_ref)?;
        let mut bad = t2.clone();
        bad.admission.payload_ref = source.cas.seal_object(&serde_json::to_vec(&envelope)?, "application/json", "commit-authority", "commit-admission", 1)?.object;
        state.lock().grants.insert(bad.admission.conflict_key.clone(), bad.clone());
        let refused = receiver.recover_control_admission(&bad, &source.admission_objects(&bad.admission.payload_ref)?, false);
        assert!(matches!(&refused, Err(error) if error.to_string().contains(expected_error)), "{fault}: {refused:#?}");
        assert!(receiver.external_operation_evidence(operation.operation_id)?.is_empty());
        assert_eq!(serde_json::to_value(receiver.external_task(task_id)?)?, serde_json::to_value(&armed_task)?);
        assert!(!receiver.control_admission_applied_here(&bad.admission)?);
    }
    state.lock().grants.insert(t2.admission.conflict_key.clone(), t2.clone());

    // A real local inbox identity conflict makes T2 refuse after the staged
    // E1 writes. No E1 row, resolved projection or local E1 marker can leak.
    let collision = serde_json::json!({"event_id":expected_result.result_id, "kind":"message", "source":"local-observation",
        "payload":{"text":"independent local event"}, "created_at":chrono::Utc::now()});
    blocked.admit_inbox_event(expected_result.result_id, agent, "message", "local-observation", &collision)?;
    assert_eq!(blocked.recover_control_admission(&t2, &objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert!(blocked.external_operation_evidence(operation.operation_id)?.is_empty());
    assert_eq!(serde_json::to_value(blocked.external_operation(operation.operation_id)?)?, serde_json::to_value(&operation)?);
    assert_eq!(serde_json::to_value(blocked.external_attempt(attempt.attempt_id)?)?, serde_json::to_value(&attempt)?);
    assert!(blocked.native_task_result(task_id)?.is_none());
    let connection = rusqlite::Connection::open(dir.join("chain-blocked.db"))?;
    assert_eq!(connection.query_row("SELECT count(*) FROM native_evidence_materializations", [], |row| row.get::<_, i64>(0))?, 0);
    assert_eq!(blocked.pending_control_reconciliations(agent)?.len(), 1);
    assert_eq!(imported.recover_control_admission(&t2, &objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert_eq!(serde_json::to_value(imported.external_task(task_id)?)?, serde_json::to_value(parked)?);
    assert!(imported.external_operation_evidence(operation.operation_id)?.is_empty());

    assert_eq!(receiver.recover_control_admission(&t2, &objects, false)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert_eq!(receiver.native_task_result(task_id)?, Some(expected_result.clone()));
    assert_eq!(receiver.external_evidence(receipt_id)?, original_receipt);
    assert_eq!(serde_json::to_value(receiver.external_operation(operation.operation_id)?)?, serde_json::to_value(&completed)?);
    assert_eq!(serde_json::to_value(receiver.external_attempt(attempt.attempt_id)?)?, serde_json::to_value(&original_attempt)?);
    assert_eq!(serde_json::to_value(receiver.external_task(head.subject_root_task_id)?)?, serde_json::to_value(parked_root)?);
    let connection = rusqlite::Connection::open(dir.join("chain-receiver.db"))?;
    let marker: String = connection.query_row("SELECT data FROM native_evidence_materializations WHERE receipt_id=?1", [receipt_id.to_string()], |row| row.get(0))?;
    let marker: serde_json::Value = serde_json::from_str(&marker)?;
    assert_eq!(marker["logical_result"], serde_json::to_value(original_receipt.received_commit)?);
    assert_eq!(marker["committed_at"]["ledger_id"], serde_json::to_value(receiver.ledger.ledger_id())?);
    assert_ne!(marker["committed_at"], marker["logical_result"]);

    // Reopen the source after its lost chosen reply and the receiver after
    // application. Both retain the original result and refuse another effect.
    let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
    reopened.attach_commit_authority(Arc::new(NativeT2Authority { source: "native-chain-source", stamp: Some(stamp), state: Arc::clone(&state) }))?;
    let recovery_started = std::time::Instant::now();
    let recovered = reopened.resume_prepared_step_commits(agent)?;
    assert_eq!(reopened.external_attempt(independent_attempt.attempt_id)?.final_receipt_id, Some(arm_receipt_id));
    assert_eq!(reopened.external_operation_evidence(independent_op.operation_id)?.len(), 2);
    assert_eq!(reopened.external_operation(independent_op.operation_id)?.effect_status, EffectStatus::Unknown);
    assert_eq!(recovered.applied, 1);
    assert!(recovered.unresolved.is_empty());
    capacity_receipt(&reopened, &t2, "native.complete.recovered", recovery_started.elapsed())?;
    assert_eq!(reopened.native_task_result(task_id)?, Some(expected_result.clone()));
    assert_eq!(reopened.external_evidence(receipt_id)?, original_receipt);
    let receiver_reopened = Store::open(dir.join("chain-receiver.db"), dir.join("chain-receiver-cas"))?;
    receiver_reopened.attach_commit_authority(Arc::new(NativeT2Authority { source: "native-chain-follower", stamp: None, state: Arc::clone(&state) }))?;
    let watermark = receiver_reopened.current_commit_seq()?;
    assert_eq!(receiver_reopened.recover_control_admission(&t2, &objects, false)?, ControlRecoveryOutcome::Applied);
    assert_eq!(receiver_reopened.current_commit_seq()?, watermark);
    assert_eq!(receiver_reopened.native_task_result(task_id)?, Some(expected_result));
    for store in [&reopened, &receiver_reopened] {
        assert!(store.arm_native_operation_for_dispatch(&ctx, &mut operation.clone(), &mut attempt.clone())?.is_some());
        assert_eq!(store.commit_native_dispatch(&ctx, operation.operation_id, attempt.attempt_id)?, AlreadyCommitted);
        assert_eq!(store.external_operation_evidence(operation.operation_id)?.len(), 1);
        assert_eq!(serde_json::to_value(store.external_attempt(attempt.attempt_id)?)?, serde_json::to_value(&original_attempt)?);
    }
    assert_eq!(connection.query_row("SELECT count(*) FROM native_evidence_materializations WHERE receipt_id=?1", [receipt_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
    assert_eq!(std::fs::read(dir.join("effect.log"))?, b"effect\n");
    let watermark = reopened.current_commit_seq()?;
    assert_eq!(reopened.recover_control_admission(&t2, &objects, true)?, ControlRecoveryOutcome::Applied);
    assert_eq!(reopened.current_commit_seq()?, watermark);
    assert_eq!(reopened.resume_prepared_step_commits(agent)?.applied, 0);
    assert_eq!(reopened.external_evidence(receipt_id)?, original_receipt);
    assert_eq!(std::fs::read(dir.join("effect.log"))?, b"effect\n");
    assert_history()?;
    eprintln!("issue316-native-capacity {}", serde_json::json!({
        "phase": "native.complete.replay_noop", "operation_id": operation.operation_id,
        "attempt_id": attempt.attempt_id, "receipt_id": receipt_id, "commit_seq": watermark,
        "history_bytes": history_bytes, "effect_bytes": std::fs::read(dir.join("effect.log"))?,
    }));

    // The original external observation remains admissible if a derived
    // canonical proof becomes unavailable. The E1 and an explicit seal debt
    // commit together; restoring the proof does not invent a successful seal.
    reopened.activate_runtime_authority(&reopened.load_subject(agent)?.0)?;
    let delayed = reopened.claim_native_task(delayed_id, implementation, false)?.context("delayed source claim")?;
    let (delayed_ctx, mut delayed_op, mut delayed_attempt) = draft_dispatch_operation(&reopened, &delayed.invocation, "delayed-native-chain", delayed.operation_id)?;
    assert!(reopened.arm_native_operation_for_dispatch(&delayed_ctx, &mut delayed_op, &mut delayed_attempt)?.is_none());
    std::fs::write(dir.join("delayed-effect.log"), b"one delayed observation")?;
    let late_raw = reopened.cas.seal_object(&std::fs::read(dir.join("delayed-effect.log"))?, "text/plain", "native-chain", "text", 1)?.object;
    let late_id = EntityId::new();
    let late_receipt = EvidenceReceipt { receipt_id: late_id, operation_id: Some(delayed_op.operation_id),
        attempt_id: Some(delayed_attempt.attempt_id), source_event_key: Some(format!("delayed:{}", delayed_attempt.attempt_id)),
        producer_epoch: Some(delayed_attempt.epoch.0), decoder_ref: Some(delayed_attempt.implementation_ref.clone()),
        raw_ref: late_raw, normalized_ref: None, received_commit: delayed_attempt.armed_commit, ..original_receipt.clone() };
    let delayed_arm = chosen()?;
    let envelope: serde_json::Value = serde_json::from_slice(&reopened.cas.read_object(&delayed_arm.admission.payload_ref.digest)?)?;
    let delayed_ref: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    assert_eq!(delayed_ref.codec.major, 4);
    let original_path = reopened.cas.verified_object_path(&delayed_ref)?;
    let saved_path = dir.join("temporarily-unavailable-arm");
    std::fs::rename(&original_path, &saved_path)?;
    let late = reopened.record_external_evidence(delayed_op.operation_id, delayed_attempt.attempt_id, late_receipt.clone(),
        Some(OperationOutcome::Succeeded), EffectStatus::Complete);
    std::fs::rename(saved_path, original_path)?;
    let late = late?;
    let retained = reopened.external_evidence(late_id)?;
    assert!(retained.same_source_original(&late_receipt));
    assert_eq!(late.evidence_through, Some(retained.received_commit));
    assert_eq!(late.outcome, Some(OperationOutcome::Succeeded));
    let debts = reopened.native_evidence_seal_debts(delayed_op.operation_id)?;
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].receipt_id, late_id);
    assert_eq!(debts[0].received_commit, retained.received_commit);
    let refused = reopened.complete_native_task(&delayed, &late);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("retained seal debt")), "{refused:#?}");
    assert!(reopened.native_task_result(delayed_id)?.is_none());
    assert!(reopened.arm_native_operation_for_dispatch(&delayed_ctx, &mut delayed_op, &mut delayed_attempt)?.is_some());
    assert_eq!(std::fs::read(dir.join("delayed-effect.log"))?, b"one delayed observation");
    assert_history()?;
    Ok(())
}

/// Native capture still owns complete table witnesses for semantic reads.
/// A trigger's point read therefore makes this whole table a dependency;
/// unlike S1's streamed ranges, native capture must refuse its oversized set.
#[cfg(feature = "test-support")]
#[test]
fn canonical_native_t2_refuses_oversized_touched_readset_before_publication() -> Result<()> {
    use shenko_store::test_support::with_sqlite_work_observation;
    use shenko_types::{evidence::*, ids::EntityId, operation::*};

    // Separate fresh production consumers: one with unrelated large history,
    // one with exactly the same bytes read by its real task-update trigger.
    // Neither case changes the 128 MiB capture or 64 MiB image budgets.
    for touched in [false, true] {
        let fixture = NativeT2Fixture::new(false)?;
        let store = &fixture.store;
        let claim = &fixture.parent;
        let (ctx, mut operation, mut attempt) = draft_dispatch_operation(
            store, &claim.invocation, "touched-readset-effect", claim.operation_id)?;
        assert!(store.arm_native_operation_for_dispatch(&ctx, &mut operation, &mut attempt)?.is_none());
        let effect_path = fixture.dir.join("effect.log");
        std::fs::write(&effect_path, b"one original effect\n")?;
        let effect = std::fs::read(&effect_path)?;
        let raw = store.cas.seal_object(&effect, "text/plain", "native-readset", "text", 1)?.object;
        let receipt_id = EntityId::new();
        let receipt = EvidenceReceipt {
            record_schema_version: 1, receipt_id, operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id), source_id: "native-readset".into(),
            source_instance_id: "original-executor".into(),
            source_event_key: Some(format!("ended:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0), producer_schema_version: Some("1".into()),
            raw_ref: raw.clone(), decoder_ref: Some(attempt.implementation_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: Some(ObservationLevel::PostconditionVerified), normalized_ref: None,
            observed_at: Some(chrono::Utc::now()), received_commit: attempt.armed_commit,
        };
        let operation = store.record_external_evidence(operation.operation_id, attempt.attempt_id,
            receipt, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
        let original_e1 = store.external_evidence(receipt_id)?;
        let original_task = store.external_task(claim.task.task_id)?;
        let original_attempt = store.external_attempt(attempt.attempt_id)?;
        let conn = rusqlite::Connection::open(fixture.dir.join("head.db"))?;
        // Multiple rows distinguish the trigger's single keyed read from the
        // complete postimage scan that runs out of the real capture budget.
        conn.execute_batch("CREATE TABLE native_capacity_readset(id INTEGER PRIMARY KEY,payload BLOB);
            WITH RECURSIVE n(id) AS (SELECT 1 UNION ALL SELECT id+1 FROM n WHERE id<129)
                INSERT INTO native_capacity_readset SELECT id,zeroblob(1024*1024) FROM n;")?;
        if touched {
            conn.execute_batch("CREATE TRIGGER native_capacity_read AFTER UPDATE ON tasks BEGIN
                SELECT length(payload) FROM native_capacity_readset WHERE id=1; END;")?;
        }
        let before = store.current_commit_seq()?;
        let grants = serde_json::to_value(&fixture.state.lock().grants)?;
        let snapshot = || -> Result<Vec<(String, i64)>> {
            ["commit_log", "control_prepared_transitions", "control_materializations",
                "operations", "attempts", "evidence_receipts", "native_evidence_materializations",
                "task_results", "inbox_events"].into_iter().map(|table| {
                    Ok((table.to_owned(), conn.query_row(&format!("SELECT count(*) FROM {table}"), [],
                        |row| row.get(0))?))
                }).collect()
        };
        let rows_before = snapshot()?;
        let started = std::time::Instant::now();
        let (completion, work) = with_sqlite_work_observation(store,
            || store.complete_native_task(claim, &operation));
        let elapsed = started.elapsed();
        let scans = work.iter().filter(|statement|
            statement.sql == "SELECT \"id\",\"payload\" FROM \"native_capacity_readset\"")
            .collect::<Vec<_>>();
        if touched {
            let error = completion.expect_err("a native full read witness must exhaust the production budget");
            assert!(error.to_string().contains("capture budget exceeded: row capture exceeds 128 MiB memory budget"), "{error:#}");
            assert_eq!(scans.len(), 1, "refuse during the first complete postimage; no preimage scan");
            let scan = scans[0];
            assert!(scan.rows > 1 && scan.rows < 129, "a real prefix of the complete table was visited: {scan:?}");
            assert!(i64::from(scan.fullscan_steps) >= scan.rows as i64 - 1, "{scan:?}");
            assert!(scan.vm_steps > 0);
            assert_eq!(store.current_commit_seq()?, before);
            assert_eq!(snapshot()?, rows_before, "no preparation, grant receipt, result or inbox publication");
            assert_eq!(serde_json::to_value(&fixture.state.lock().grants)?, grants);
            assert_eq!(serde_json::to_value(store.external_task(claim.task.task_id)?)?, serde_json::to_value(&original_task)?);
            assert!(store.native_task_result(claim.task.task_id)?.is_none());
        } else {
            let result = completion?.context("unrelated history allows ordinary T2")?;
            assert_eq!(result.receipt_id, receipt_id);
            assert!(scans.is_empty(), "unrelated history has no capture scan");
            assert!(store.control_admission_applied_here(&fixture.chosen()?.admission)?);
        }
        assert_eq!(store.external_evidence(receipt_id)?, original_e1);
        assert_eq!(serde_json::to_value(store.external_operation(operation.operation_id)?)?, serde_json::to_value(&operation)?);
        assert_eq!(serde_json::to_value(store.external_attempt(attempt.attempt_id)?)?, serde_json::to_value(&original_attempt)?);
        assert_eq!(store.cas.read_object(&raw.digest)?, effect);
        assert_eq!(std::fs::read(&effect_path)?, effect);
        assert_eq!(conn.query_row("SELECT count(*) FROM native_capacity_readset
            WHERE length(payload)=1024*1024 AND payload=zeroblob(1024*1024)", [],
            |row| row.get::<_, i64>(0))?, 129);
        // Emit actual SQL, delivered rows, fullscan/VM steps and durations.
        // No latency threshold is added: the outer native observer supplies
        // its normal deadline, and zero-ns SQLite samples remain honest zeros.
        eprintln!("native_touched_readset_work {}", serde_json::json!({
            "touched": touched, "consumer": "Store::complete_native_task",
            "task_id": claim.task.task_id, "operation_id": operation.operation_id,
            "attempt_id": attempt.attempt_id, "receipt_id": receipt_id,
            "history_rows": 129, "payload_bytes_per_row": 1024*1024,
            "history_bytes": 129*1024*1024, "capture_budget_bytes": 128*1024*1024,
            "image_budget_bytes": 64*1024*1024, "elapsed_nanoseconds": elapsed.as_nanos(),
            "readset_scan": scans, "writer_statements": work,
        }));
        // Reopen must retain the same original E1 and, for the refused case,
        // no prepared T2 to recover. This is readback, not a second consumer.
        let reopened = fixture.reopen()?;
        assert_eq!(reopened.external_evidence(receipt_id)?, original_e1);
        if touched {
            assert_eq!(reopened.current_commit_seq()?, before);
            assert_eq!(serde_json::to_value(reopened.external_task(claim.task.task_id)?)?, serde_json::to_value(original_task)?);
            assert!(reopened.native_task_result(claim.task.task_id)?.is_none());
        }
    }
    Ok(())
}

#[test]
fn canonical_native_arming_keeps_original_intent_without_reissuing_execution() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, child::{NATIVE_DISPATCH, NativeDispatchTransition::AlreadyCommitted}, cognition::CognitionImportOptions};
    use shenko_types::{commit::CommitRef, ids::EntityId, object::ObjectRef, operation::*, task::DispatchEligibility};
    let fixture = NativeT2Fixture::new(false)?;
    let connection = rusqlite::Connection::open(fixture.dir.join("head.db"))?;
    let initial = &fixture.parent.invocation;
    let (ctx, mut operation, mut attempt) = draft_dispatch_operation(&fixture.store, initial, "fresh-native-arm", EntityId::new())?;
    let request = operation.clone();
    let original_attempt = attempt.clone();
    assert!(fixture.store.arm_external_operation(&ctx, &mut operation, &mut attempt)?.is_none());
    let grant = fixture.chosen()?;
    let envelope: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    assert_eq!(envelope["action"]["kind"], "native.arm");
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let transition: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&reference.digest)?)?;
    assert_eq!(transition["plan"]["operation"], serde_json::to_value(&request)?);
    assert_eq!(transition["plan"]["attempt"], serde_json::to_value(original_attempt)?);
    assert_eq!(transition["plan"]["invocation"], serde_json::to_value(&ctx)?);
    assert_eq!(operation.prepared_commit, attempt.armed_commit);
    assert_eq!(operation.prepared_commit, fixture.store.external_task(ctx.task_id)?.updated_commit);
    assert_eq!(serde_json::to_value(operation.prepared_commit)?, transition["rows"]["result_commit"]);
    let timestamp: chrono::DateTime<chrono::Utc> = serde_json::from_value(transition["rows"]["created_at"].clone())?;
    for (sql, id) in [
        ("SELECT created_at FROM operations WHERE operation_id=?1", operation.operation_id),
        ("SELECT created_at FROM attempts WHERE attempt_id=?1", attempt.attempt_id),
    ] {
        let actual: String = connection.query_row(sql, [id.to_string()], |row| row.get(0))?;
        assert_eq!(actual.parse::<chrono::DateTime<chrono::Utc>>()?, timestamp);
    }
    let after_fresh = fixture.store.current_commit_seq()?;
    let mut stale = ctx.clone();
    stale.invocation_id = EntityId::new();
    let replay = fixture.store.arm_external_operation(&stale, &mut operation, &mut attempt)?.context("factual duplicate")?;
    assert_eq!(serde_json::to_value(&replay)?, serde_json::to_value(&operation)?);
    assert_eq!(fixture.store.current_commit_seq()?, after_fresh);
    assert!(fixture.store.external_operation_evidence(operation.operation_id)?.is_empty());

    // The paired entry point commits the original marker with T1. Neither a
    // duplicate arm nor the later dispatch API obtains another live permit.
    let (ctx, mut paired, mut paired_attempt) = draft_dispatch_operation(&fixture.store, initial, "paired-native-arm", EntityId::new())?;
    assert!(fixture.store.arm_native_operation_for_dispatch(&ctx, &mut paired, &mut paired_attempt)?.is_none());
    assert_eq!(fixture.store.external_task(ctx.task_id)?.memos[NATIVE_DISPATCH]["attempts"][paired_attempt.attempt_id.to_string()]["commit"],
        serde_json::to_value(paired.prepared_commit)?);
    assert_eq!(paired.prepared_commit, paired_attempt.armed_commit);
    assert!(fixture.store.arm_native_operation_for_dispatch(&ctx, &mut paired, &mut paired_attempt)?.is_some());
    assert_eq!(fixture.store.commit_native_dispatch(&ctx, paired.operation_id, paired_attempt.attempt_id)?, AlreadyCommitted);

    let (ctx, mut queued, mut queued_attempt) = draft_dispatch_operation(&fixture.store, initial, "arm-before-choose", EntityId::new())?;
    let before = fixture.store.external_task(ctx.task_id)?;
    let original = serde_json::to_value((&queued, &queued_attempt))?;
    fixture.state.lock().fail_before_choose = true;
    let refused = fixture.store.arm_external_operation(&ctx, &mut queued, &mut queued_attempt);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture failed before choosing dispatch")), "{refused:#?}");
    assert_eq!(serde_json::to_value((&queued, &queued_attempt))?, original);
    assert_eq!(serde_json::to_value(fixture.store.external_task(ctx.task_id)?)?, serde_json::to_value(&before)?);
    assert_eq!(connection.query_row("SELECT count(*) FROM operations WHERE operation_id=?1", [queued.operation_id.to_string()], |row| row.get::<_, i64>(0))?, 0);
    let replay = fixture.store.arm_external_operation(&ctx, &mut queued, &mut queued_attempt)?.context("old preparation must not send")?;
    let chosen = fixture.chosen()?;
    assert!(!chosen.replayed, "first actual choose is still recovery of an older preparation");
    assert_eq!(replay.operation_id, queued.operation_id);
    assert_eq!(queued.prepared_commit, queued_attempt.armed_commit);
    assert_eq!(queued.effect_status, EffectStatus::Unknown);
    assert!(fixture.store.external_operation_evidence(queued.operation_id)?.is_empty());

    // Preserve a foreign imported task's parking. The retained source grant
    // cannot install an executable predecessor on the receiving ledger.
    let (ctx, mut lost, mut lost_attempt) = draft_dispatch_operation(&fixture.store, initial, "arm-chosen-reply-lost", EntityId::new())?;
    let before = fixture.store.external_task(ctx.task_id)?;
    let mut archive = Vec::new();
    fixture.store.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let follower = Store::open(fixture.dir.join("arm-follower.db"), fixture.dir.join("arm-follower-cas"))?;
    follower.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    follower.attach_commit_authority(Arc::new(NativeT2Authority { source: "arm-follower", stamp: None, state: Arc::clone(&fixture.state) }))?;
    let parked = follower.external_task(ctx.task_id)?;
    assert!(matches!(parked.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    fixture.state.lock().lose_reply = true;
    let refused = fixture.store.arm_native_operation_for_dispatch(&ctx, &mut lost, &mut lost_attempt);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture lost chosen T2 reply")), "{refused:#?}");
    let chosen = fixture.chosen()?;
    let objects = fixture.store.admission_objects(&chosen.admission.payload_ref)?;
    assert_eq!(follower.recover_control_admission(&chosen, &objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert_eq!(serde_json::to_value(follower.external_task(ctx.task_id)?)?, serde_json::to_value(&parked)?);
    assert!(!follower.control_admission_applied_here(&chosen.admission)?);
    assert_eq!(follower.pending_control_reconciliations(fixture.agent)?.len(), 1);

    // Even a retained grant cannot smuggle a different invocation into the
    // delta. Malformation is fatal and leaves all three T1 rows unapplied.
    let mut envelope: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&chosen.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let mut transition: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&reference.digest)?)?;
    let logical: CommitRef = serde_json::from_value(transition["rows"]["result_commit"].clone())?;
    let table = transition["rows"]["tables"].as_array_mut().context("arm tables")?.iter_mut()
        .find(|table| table["before"]["table"]["name"] == "attempts").context("attempt write")?;
    let data_column = table["before"]["table"]["columns"].as_array().context("attempt columns")?.iter()
        .position(|column| column == "data").context("attempt JSON column")?;
    let data = &mut table["changes"][0]["after"][data_column]["value"];
    let mut substituted: serde_json::Value = serde_json::from_str(data.as_str().context("attempt JSON")?)?;
    substituted["invocation_id"] = serde_json::to_value(EntityId::new())?;
    *data = serde_json::to_value(serde_json::to_string(&substituted)?)?;
    let bad_ref = fixture.store.cas.seal_object(&serde_json::to_vec(&transition)?, "application/json", "commit-authority", "store-native-arm-transition", 1)?.object;
    envelope["action"]["transition_ref"] = serde_json::to_value(bad_ref)?;
    let mut fault = chosen.clone();
    fault.admission.payload_ref = fixture.store.cas.seal_object(&serde_json::to_vec(&envelope)?, "application/json", "commit-authority", "commit-admission", 1)?.object;
    fixture.state.lock().grants.insert(fault.admission.conflict_key.clone(), fault.clone());
    let malformed = fixture.store.recover_control_admission(&fault, &fixture.store.admission_objects(&fault.admission.payload_ref)?, true);
    assert!(matches!(&malformed, Err(error) if error.to_string().contains("T1 write differs from its frozen request")), "{malformed:#?}");
    assert_eq!(serde_json::to_value(fixture.store.external_task(ctx.task_id)?)?, serde_json::to_value(&before)?);
    assert_eq!(connection.query_row("SELECT count(*) FROM operations WHERE operation_id=?1", [lost.operation_id.to_string()], |row| row.get::<_, i64>(0))?, 0);
    assert_eq!(connection.query_row("SELECT count(*) FROM attempts WHERE attempt_id=?1", [lost_attempt.attempt_id.to_string()], |row| row.get::<_, i64>(0))?, 0);
    assert!(!fixture.store.control_admission_applied_here(&fault.admission)?);
    fixture.state.lock().grants.insert(chosen.admission.conflict_key.clone(), chosen.clone());
    assert_eq!(fixture.store.recover_control_admission(&chosen, &objects, true)?, ControlRecoveryOutcome::RecoveredCanonical);
    let recovered = fixture.store.external_attempt(lost_attempt.attempt_id)?;
    let mut expected_attempt = lost_attempt.clone();
    expected_attempt.armed_commit = logical;
    assert_eq!(serde_json::to_value(&recovered)?, serde_json::to_value(expected_attempt)?);
    let mut expected_operation = lost.clone();
    expected_operation.prepared_commit = logical;
    assert_eq!(serde_json::to_value(fixture.store.external_operation(lost.operation_id)?)?, serde_json::to_value(expected_operation)?);
    assert!(fixture.store.arm_native_operation_for_dispatch(&ctx, &mut lost, &mut lost_attempt)?.is_some());
    assert_eq!(fixture.store.commit_native_dispatch(&ctx, lost.operation_id, lost_attempt.attempt_id)?, AlreadyCommitted);
    assert!(fixture.store.external_operation_evidence(lost.operation_id)?.is_empty());

    // A new Store incarnation can order an unchosen source preparation before
    // activation, but the original worker still receives only a factual read.
    let (ctx, mut restart, mut restart_attempt) = draft_dispatch_operation(&fixture.store, initial, "arm-preactivation-recovery", EntityId::new())?;
    fixture.state.lock().fail_before_choose = true;
    let refused = fixture.store.arm_external_operation(&ctx, &mut restart, &mut restart_attempt);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("fixture failed before choosing dispatch")), "{refused:#?}");
    let reopened = fixture.reopen()?;
    let recovered = reopened.resume_prepared_step_commits(fixture.agent)?;
    assert_eq!(recovered.applied, 1);
    assert!(recovered.unresolved.is_empty());
    assert!(!fixture.chosen()?.replayed);
    assert!(reopened.arm_external_operation(&ctx, &mut restart, &mut restart_attempt)?.is_some());
    assert_eq!(reopened.external_operation_attempts(restart.operation_id)?.len(), 1);
    assert_eq!(reopened.external_operation(restart.operation_id)?.effect_status, EffectStatus::Unknown);
    assert_eq!(reopened.resume_prepared_step_commits(fixture.agent)?.applied, 0);
    let head = reopened.load_subject(fixture.agent)?.0;
    reopened.activate_runtime_authority(&head)?;
    let (stale, mut fenced, mut fenced_attempt) = draft_dispatch_operation(&reopened, &ctx, "stale-worker-after-reopen", EntityId::new())?;
    let before = reopened.external_task(ctx.task_id)?;
    let refused = reopened.arm_external_operation(&stale, &mut fenced, &mut fenced_attempt);
    assert!(matches!(&refused, Err(error) if error.to_string().contains("source precondition changed")), "{refused:#?}");
    assert_eq!(serde_json::to_value(reopened.external_task(ctx.task_id)?)?, serde_json::to_value(before)?);
    assert_eq!(connection.query_row("SELECT count(*) FROM operations WHERE operation_id=?1", [fenced.operation_id.to_string()], |row| row.get::<_, i64>(0))?, 0);
    Ok(())
}

#[test]
fn canonical_native_dispatch_retains_one_live_permit_through_prepare_learn_and_restart() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, child::{NATIVE_DISPATCH, NativeDispatchTransition::{AlreadyCommitted, Won}}, cognition::CognitionImportOptions};
    use shenko_types::{authority::CommitAuthorityGrant, commit::CommitRef, evidence::*, ids::EntityId, object::ObjectRef, operation::*, task::DispatchEligibility};
    let fixture = NativeT2Fixture::with_tasks(true, true)?;
    let ctx = &fixture.child.as_ref().context("descendant dispatch claim")?.invocation;
    let sibling = fixture.sibling.as_ref().context("independent dispatch observation")?;
    let sibling_task_id = sibling.task.task_id;
    let independent = arm_dispatch_probe(&fixture.store, &sibling.invocation, "independent-queued-operation");
    let independent_attempt = fixture.store.external_attempt(independent.1)?;
    let raw = fixture.store.cas.seal_object(b"independent observation ended without an effect proof",
        "text/plain", "dispatch-test", "text", 1)?.object;
    let receipt = EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(independent.0), attempt_id: Some(independent.1),
        source_id: "independent-dispatch-observer".into(), source_instance_id: "fixture".into(),
        source_event_key: Some("independent-after-dispatch-choice".into()), producer_epoch: Some(independent_attempt.epoch.0),
        producer_schema_version: Some("1".into()), raw_ref: raw, decoder_ref: Some(independent_attempt.implementation_ref),
        decode_status: DecodeStatus::Decoded, observation_level: None, normalized_ref: None,
        observed_at: Some(chrono::Utc::now()), received_commit: independent_attempt.armed_commit,
    };
    let receipt_id = receipt.receipt_id;
    let first = arm_dispatch_probe(&fixture.store, ctx, "fresh-model-step");
    let first_attempt = fixture.store.external_attempt(first.1)?;
    let first_operation = fixture.store.external_operation(first.0)?;
    // One real concurrent window advances S0, a separate task's cancellation,
    // and its late Unknown E1 after this dispatch is chosen. The target still
    // depends on its complete parent ancestry, not every task/attempt row.
    let progressing = Arc::clone(&fixture.store);
    let agent = fixture.agent;
    fixture.state.lock().before_materialize = Some(Box::new(move || {
        let (head, successor) = progressing.load_subject(agent)?;
        recorded_result(&progressing, &head, successor)?;
        progressing.request_native_cancellation(sibling_task_id, EntityId::new(), EntityId::new(),
            "independent task stopped", shenko_types::task::AbortScope::Task)?;
        progressing.record_external_evidence(independent.0, independent.1, receipt, None, EffectStatus::Unknown)?;
        Ok(())
    }));
    assert_eq!(fixture.store.commit_native_dispatch(ctx, first.0, first.1)?, Won);
    assert!(fixture.state.lock().before_materialize.is_none());
    assert_eq!(fixture.store.external_attempt(independent.1)?.final_receipt_id, Some(receipt_id));
    assert_eq!(fixture.store.external_operation(independent.0)?.effect_status, EffectStatus::Unknown);
    assert_eq!(fixture.store.load_subject(fixture.agent)?.1.state, shenko_types::subject::SubjectStepState::ResultRecorded);
    let first_grant = fixture.state.lock().grants.get(&format!("attempt/{}/dispatch", first.1))
        .cloned().context("original dispatch choice")?;
    let ancestor = fixture.store.external_task(fixture.parent.task.task_id)?;
    assert_eq!(first_grant.admission.required_unconsumed,
        vec![format!("task/{}/revision/{}", ancestor.task_id, ancestor.record_revision.0)]);
    assert_eq!(fixture.store.commit_native_dispatch(ctx, first.0, first.1)?, AlreadyCommitted);
    assert_eq!(serde_json::to_value(fixture.store.external_attempt(first.1)?)?, serde_json::to_value(first_attempt)?);
    assert_eq!(serde_json::to_value(fixture.store.external_operation(first.0)?)?, serde_json::to_value(first_operation)?);

    // A source call sealed its plan, but its provider never chose anything.
    // The same live runtime retry must recognize that older preparation;
    // the provider's first grant is false-replayed, not a fresh send permit.
    let queued = arm_dispatch_probe(&fixture.store, ctx, "queued-before-choose");
    let queued_attempt = fixture.store.external_attempt(queued.1)?;
    fixture.state.lock().fail_before_choose = true;
    assert!(matches!(fixture.store.commit_native_dispatch(ctx, queued.0, queued.1),
        Err(error) if error.to_string().contains("fixture failed before choosing dispatch")));
    assert!(!fixture.state.lock().grants.contains_key(&format!("attempt/{}/dispatch", queued.1)));
    assert_eq!(fixture.store.commit_native_dispatch(ctx, queued.0, queued.1)?, AlreadyCommitted);
    let queued_grant = fixture.chosen()?;
    assert!(!queued_grant.replayed, "this is the provider's first actual choose");
    assert!(fixture.store.control_admission_applied_here(&queued_grant.admission)?);
    assert_eq!(serde_json::to_value(fixture.store.external_attempt(queued.1)?)?, serde_json::to_value(queued_attempt)?);
    assert!(fixture.store.external_operation_evidence(queued.0)?.is_empty());

    let ids = arm_dispatch_probe(&fixture.store, ctx, "chosen-reply-lost");
    let original_attempt = fixture.store.external_attempt(ids.1)?;
    let original_operation = fixture.store.external_operation(ids.0)?;
    let before = fixture.store.external_task(ctx.task_id)?;
    let mut archive = Vec::new();
    fixture.store.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let follower = Store::open(fixture.dir.join("dispatch-follower.db"), fixture.dir.join("dispatch-follower-cas"))?;
    follower.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    follower.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "dispatch-follower", stamp: None, state: Arc::clone(&fixture.state),
    }))?;
    let parked = follower.external_task(ctx.task_id)?;
    assert!(matches!(parked.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    fixture.state.lock().lose_reply = true;
    assert!(matches!(fixture.store.commit_native_dispatch(ctx, ids.0, ids.1),
        Err(error) if error.to_string().contains("fixture lost chosen T2 reply")));
    let grant = fixture.chosen()?;
    let objects = fixture.store.admission_objects(&grant.admission.payload_ref)?;
    assert_eq!(follower.recover_control_admission(&grant, &objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert_eq!(serde_json::to_value(follower.external_task(ctx.task_id)?)?, serde_json::to_value(&parked)?);
    assert!(!follower.control_admission_applied_here(&grant.admission)?);
    assert_eq!(follower.pending_control_reconciliations(fixture.agent)?.len(), 1);

    // A nominally chosen delta cannot replace the original source commit in
    // the task marker. Validation rejects it before any task/receipt write.
    let mut envelope: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let mut transition: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&reference.digest)?)?;
    let logical = transition["rows"]["result_commit"].clone();
    let task_table = transition["rows"]["tables"].as_array_mut().context("transition tables")?.iter_mut()
        .find(|table| table["before"]["table"]["name"] == "tasks").context("task delta")?;
    let data_column = task_table["before"]["table"]["columns"].as_array().context("task columns")?
        .iter().position(|column| column == "data").context("task JSON column")?;
    let data = &mut task_table["changes"][0]["after"][data_column]["value"];
    let mut substituted: serde_json::Value = serde_json::from_str(data.as_str().context("task JSON")?)?;
    substituted["memos"][NATIVE_DISPATCH]["attempts"][ids.1.to_string()]["commit"]["ledger_id"] = serde_json::to_value(EntityId::new())?;
    *data = serde_json::to_value(serde_json::to_string(&substituted)?)?;
    assert_eq!(reference.codec.major, 2);
    assert_eq!(transition["rows"]["schema_version"], 5);
    let bad_ref = fixture.store.cas.seal_object(&serde_json::to_vec(&transition)?, "application/json", "commit-authority", "store-native-dispatch-transition", reference.codec.major)?.object;
    envelope["action"]["transition_ref"] = serde_json::to_value(bad_ref)?;
    let mut fault: CommitAuthorityGrant = grant.clone();
    fault.admission.payload_ref = fixture.store.cas.seal_object(&serde_json::to_vec(&envelope)?, "application/json", "commit-authority", "commit-admission", 1)?.object;
    fixture.state.lock().grants.insert(fault.admission.conflict_key.clone(), fault.clone());
    let bad_objects = fixture.store.admission_objects(&fault.admission.payload_ref)?;
    assert!(matches!(fixture.store.recover_control_admission(&fault, &bad_objects, true),
        Err(error) if error.to_string().contains("dispatch write differs from its named marker")));
    assert_eq!(serde_json::to_value(fixture.store.external_task(ctx.task_id)?)?, serde_json::to_value(&before)?);
    assert!(!fixture.store.control_admission_applied_here(&fault.admission)?);
    fixture.state.lock().grants.insert(grant.admission.conflict_key.clone(), grant.clone());

    assert_eq!(fixture.store.recover_control_admission(&grant, &objects, true)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert_eq!(fixture.store.commit_native_dispatch(ctx, ids.0, ids.1)?, AlreadyCommitted);
    assert_eq!(fixture.store.external_task(ctx.task_id)?.memos[NATIVE_DISPATCH]["attempts"][ids.1.to_string()]["commit"], logical);
    assert_eq!(serde_json::to_value(fixture.store.external_attempt(ids.1)?)?, serde_json::to_value(&original_attempt)?);
    assert_eq!(serde_json::to_value(fixture.store.external_operation(ids.0)?)?, serde_json::to_value(original_operation)?);
    assert!(fixture.store.external_operation_evidence(ids.0)?.is_empty());

    // A later original provider fact remains independent of replay. Reading
    // the materialized decision cannot replace that E1 or rename its attempt.
    let raw = fixture.store.cas.seal_object(b"original late provider receipt", "text/plain", "dispatch-test", "text", 1)?.object;
    let evidence = EvidenceReceipt {
        record_schema_version: 1, receipt_id: EntityId::new(), operation_id: Some(ids.0), attempt_id: Some(ids.1),
        source_id: "original-dispatch-provider".into(), source_instance_id: "original-worker".into(),
        source_event_key: Some(format!("ended:{}", ids.1)), producer_epoch: Some(original_attempt.epoch.0), producer_schema_version: Some("1".into()),
        raw_ref: raw, decoder_ref: Some(original_attempt.implementation_ref), decode_status: DecodeStatus::Decoded,
        observation_level: Some(ObservationLevel::RemoteCompleted), normalized_ref: None, observed_at: Some(chrono::Utc::now()),
        received_commit: original_attempt.armed_commit,
    };
    let receipt_id = evidence.receipt_id;
    fixture.store.record_external_evidence(ids.0, ids.1, evidence, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
    let retained = fixture.store.external_evidence(receipt_id)?;
    assert_eq!(fixture.store.recover_control_admission(&grant, &objects, true)?, ControlRecoveryOutcome::Applied);
    assert_eq!(fixture.store.commit_native_dispatch(ctx, ids.0, ids.1)?, AlreadyCommitted);
    assert_eq!(fixture.store.external_evidence(receipt_id)?, retained);
    assert_eq!(fixture.store.external_attempt(ids.1)?.final_receipt_id, Some(receipt_id));

    // Repeat the actual unchosen crash window across Store incarnation loss.
    // Startup has only the durable prepared descriptor, never the old live
    // caller's right to receive Won, even when this choose is also new.
    let interrupted = arm_dispatch_probe(&fixture.store, ctx, "restart-before-choose");
    let original_attempt = fixture.store.external_attempt(interrupted.1)?;
    let original_operation = fixture.store.external_operation(interrupted.0)?;
    fixture.state.lock().fail_before_choose = true;
    assert!(matches!(fixture.store.commit_native_dispatch(ctx, interrupted.0, interrupted.1),
        Err(error) if error.to_string().contains("fixture failed before choosing dispatch")));
    assert!(!fixture.state.lock().grants.contains_key(&format!("attempt/{}/dispatch", interrupted.1)));
    let before = fixture.store.external_task(ctx.task_id)?;
    assert!(before.memos[NATIVE_DISPATCH]["attempts"].get(interrupted.1.to_string()).is_none());
    let reopened = fixture.reopen()?;
    let recovery = reopened.resume_prepared_step_commits(fixture.agent)?;
    assert_eq!(recovery.applied, 1);
    assert!(recovery.unresolved.is_empty());
    let grant = fixture.chosen()?;
    assert!(!grant.replayed, "startup did the first actual choose");
    assert_eq!(reopened.commit_native_dispatch(ctx, interrupted.0, interrupted.1)?, AlreadyCommitted);
    assert_eq!(reopened.resume_prepared_step_commits(fixture.agent)?.applied, 0);
    assert_eq!(serde_json::to_value(reopened.external_attempt(interrupted.1)?)?, serde_json::to_value(original_attempt)?);
    assert_eq!(serde_json::to_value(reopened.external_operation(interrupted.0)?)?, serde_json::to_value(original_operation)?);
    assert!(reopened.external_operation_evidence(interrupted.0)?.is_empty());

    let envelope: serde_json::Value = serde_json::from_slice(&reopened.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    let transition_ref: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let transition: serde_json::Value = serde_json::from_slice(&reopened.cas.read_object(&transition_ref.digest)?)?;
    let logical: CommitRef = serde_json::from_value(transition["rows"]["result_commit"].clone())?;
    let task = reopened.external_task(ctx.task_id)?;
    assert_eq!(task.updated_commit, logical);
    assert_eq!(task.memos[NATIVE_DISPATCH]["attempts"][interrupted.1.to_string()]["commit"], serde_json::to_value(logical)?);
    assert_eq!(task.task_revision, before.task_revision);
    assert_eq!(task.record_revision, before.record_revision.next());
    let conn = rusqlite::Connection::open(fixture.dir.join("head.db"))?;
    let physical: String = conn.query_row("SELECT data FROM control_materializations WHERE action_id=?1",
        [grant.admission.action_id.to_string()], |row| row.get(0))?;
    let physical: serde_json::Value = serde_json::from_str(&physical)?;
    assert_eq!(physical["logical_result"], serde_json::to_value(logical)?);
    assert_ne!(physical["committed_at"], physical["logical_result"]);
    let event_id = shenko_types::ids::EntityId::derived("commit-authority-admission-v1", grant.admission.conflict_key.as_bytes());
    let event: String = conn.query_row("SELECT data FROM inbox_events WHERE event_id=?1",
        [event_id.to_string()], |row| row.get(0))?;
    let event: serde_json::Value = serde_json::from_str(&event)?;
    assert_eq!(event["created_at"], transition["rows"]["created_at"]);
    assert_eq!(event["payload"]["committed_at"], serde_json::to_value(logical)?);
    assert_eq!(conn.query_row("SELECT count(*) FROM control_materializations WHERE action_id=?1",
        [grant.admission.action_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
    Ok(())
}

#[test]
fn canonical_native_t2_keeps_original_e1_through_settlement_restart_and_foreign_debt() -> Result<()> {
    use shenko_store::{ControlRecoveryOutcome, cognition::CognitionImportOptions};
    use shenko_types::{authority::CommitAuthorityGrant, ids::EntityId, object::ObjectRef, task::DispatchEligibility};
    let mut fixture = NativeT2Fixture::new(false)?;
    let (operation, evidence) = native_t2_evidence(&fixture.store, &fixture.parent)?;
    let original_invocation = fixture.parent.invocation.clone();
    let original_attempt = fixture.store.external_attempt(evidence.attempt_id.context("original E1 attempt")?)?;

    // A new current recovery invocation may settle the original effect. Its
    // identity must never replace the original attempt/result provenance.
    fixture.store = fixture.reopen()?;
    fixture.store.activate_runtime_authority(&fixture.store.load_subject(fixture.agent)?.0)?;
    fixture.store.recover_native_tasks()?;
    fixture.parent = fixture.store.claim_native_task(fixture.parent.task.task_id,
        fixture.parent.task.implementation_ref.clone(), true)?.context("settlement claim")?;
    assert_ne!(fixture.parent.invocation.invocation_id, original_invocation.invocation_id);
    let before = fixture.store.external_task(fixture.parent.task.task_id)?;
    let mut archive = Vec::new();
    fixture.store.write_cognition_archive(&Default::default(), None, &mut archive)?;
    let follower = Store::open(fixture.dir.join("native-follower.db"), fixture.dir.join("native-follower-cas"))?;
    follower.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
    follower.attach_commit_authority(Arc::new(NativeT2Authority {
        source: "native-t2-follower", stamp: None, state: Arc::clone(&fixture.state),
    }))?;
    let parked = follower.external_task(before.task_id)?;
    assert!(matches!(parked.dispatch_eligibility, DispatchEligibility::Parked { .. }));
    fixture.state.lock().lose_reply = true;
    let error = fixture.store.complete_native_task(&fixture.parent, &operation).err().context("chosen T2 reply should be lost")?;
    assert!(error.to_string().contains("fixture lost chosen T2 reply"), "{error:#}");
    let grant = fixture.chosen()?;
    let objects = fixture.store.admission_objects(&grant.admission.payload_ref)?;
    assert_eq!(follower.recover_control_admission(&grant, &objects, false)?, ControlRecoveryOutcome::ReconciliationRequired);
    assert_eq!(serde_json::to_value(follower.external_task(before.task_id)?)?, serde_json::to_value(&parked)?);
    assert!(follower.native_task_result(before.task_id)?.is_none());
    assert!(!follower.control_admission_applied_here(&grant.admission)?);
    assert_eq!(follower.pending_control_reconciliations(fixture.agent)?.len(), 1);

    // Even a provider-retained value must pass the named native result
    // validator. Substituting another invocation cannot write any result row.
    let mut envelope: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    let reference: ObjectRef = serde_json::from_value(envelope["action"]["transition_ref"].clone())?;
    let original: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&reference.digest)?)?;
    let logical_commit: shenko_types::commit::CommitRef = serde_json::from_value(original["rows"]["result_commit"].clone())?;
    let mut substituted = original;
    substituted["result"]["invocation_id"] = serde_json::to_value(EntityId::new())?;
    let changed = fixture.store.cas.seal_object(&serde_json::to_vec(&substituted)?, "application/json", "commit-authority", "store-native-t2-transition", reference.codec.major)?.object;
    envelope["action"]["transition_ref"] = serde_json::to_value(changed)?;
    let mut fault: CommitAuthorityGrant = grant.clone();
    fault.admission.payload_ref = fixture.store.cas.seal_object(&serde_json::to_vec(&envelope)?, "application/json", "commit-authority", "commit-admission", 1)?.object;
    fixture.state.lock().grants.insert(fault.admission.conflict_key.clone(), fault.clone());
    let fault_objects = fixture.store.admission_objects(&fault.admission.payload_ref)?;
    assert!(fixture.store.recover_control_admission(&fault, &fault_objects, true).is_err());
    assert!(!fixture.store.control_admission_applied_here(&fault.admission)?);
    assert!(fixture.store.native_task_result(before.task_id)?.is_none());
    assert_eq!(serde_json::to_value(fixture.store.external_task(before.task_id)?)?, serde_json::to_value(&before)?);
    fixture.state.lock().grants.insert(grant.admission.conflict_key.clone(), grant.clone());

    let reopened = fixture.reopen()?;
    let recovered = reopened.resume_prepared_step_commits(fixture.agent)?;
    assert_eq!(recovered.applied, 1);
    assert!(recovered.unresolved.is_empty());
    let result = reopened.native_task_result(before.task_id)?.context("original T2 result")?;
    assert_eq!(result.commit_ref, logical_commit);
    assert_eq!(result.invocation_id, original_invocation.invocation_id);
    assert_eq!(result.receipt_id, evidence.receipt_id);
    assert_eq!(result.operation_id, operation.operation_id);
    assert_eq!(reopened.external_evidence(evidence.receipt_id)?, evidence);
    assert_eq!(serde_json::to_value(reopened.external_attempt(original_attempt.attempt_id)?)?, serde_json::to_value(&original_attempt)?);
    assert_eq!(reopened.commit_native_dispatch(&original_invocation, operation.operation_id, original_attempt.attempt_id)?,
        shenko_store::child::NativeDispatchTransition::AlreadyCommitted);
    assert_eq!(reopened.recover_control_admission(&grant, &objects, true)?, ControlRecoveryOutcome::Applied);
    assert_eq!(reopened.resume_prepared_step_commits(fixture.agent)?.applied, 0);
    let connection = rusqlite::Connection::open(fixture.dir.join("head.db"))?;
    let physical: String = connection.query_row("SELECT data FROM control_materializations WHERE action_id=?1",
        [grant.admission.action_id.to_string()], |row| row.get(0))?;
    let physical: serde_json::Value = serde_json::from_str(&physical)?;
    assert_ne!(physical["committed_at"], serde_json::to_value(result.commit_ref)?);
    assert_eq!(physical["logical_result"], serde_json::to_value(result.commit_ref)?);
    assert_eq!(connection.query_row("SELECT count(*) FROM task_results WHERE task_id=?1", [before.task_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
    assert_eq!(connection.query_row("SELECT count(*) FROM inbox_events WHERE event_id=?1 AND kind='task_result'", [result.result_id.to_string()], |row| row.get::<_, i64>(0))?, 1);
    Ok(())
}

#[test]
fn canonical_native_t2_completing_and_finish_recover_as_separate_atomic_results() -> Result<()> {
    use shenko_store::ControlRecoveryOutcome;
    use shenko_types::{evidence::EvidenceReceipt, ids::EntityId,
        operation::{EffectStatus, OperationOutcome}, subject::SubjectStepState, task::TaskState};
    let fixture = NativeT2Fixture::new(true)?;
    let child = fixture.child.as_ref().context("ordinary child")?;
    let (parent_operation, parent_evidence) = native_t2_evidence(&fixture.store, &fixture.parent)?;
    let (child_operation, child_evidence) = native_t2_evidence(&fixture.store, child)?;
    fixture.state.lock().lose_reply = true;
    let error = fixture.store.complete_native_task(&fixture.parent, &parent_operation).err().context("chosen Completing reply should be lost")?;
    assert!(error.to_string().contains("fixture lost chosen T2 reply"), "{error:#}");
    let completing = fixture.chosen()?;
    let objects = fixture.store.admission_objects(&completing.admission.payload_ref)?;
    assert!(fixture.store.native_task_result(fixture.parent.task.task_id)?.is_none());
    // The selected generic T2 depends on its own E1 and on the ordinary
    // child's task state, not that child's operation/receipt progress or the
    // main step's S0 state. Interleave both through real Store writes while
    // keeping that child unfinished. The original Completing must still win.
    let progressing = Arc::clone(&fixture.store);
    let agent = fixture.agent;
    let receipt = EvidenceReceipt {
        receipt_id: EntityId::new(), source_event_key: Some("second-independent-child-observation".into()),
        observed_at: Some(chrono::Utc::now()), ..child_evidence.clone()
    };
    let receipt_id = receipt.receipt_id;
    let child_attempt_id = child_evidence.attempt_id.context("child original attempt")?;
    fixture.state.lock().before_materialize = Some(Box::new(move || {
        let (head, successor) = progressing.load_subject(agent)?;
        assert_eq!(successor.state, SubjectStepState::Planned);
        recorded_result(&progressing, &head, successor)?;
        progressing.record_external_evidence(child_operation.operation_id, child_attempt_id,
            receipt, Some(OperationOutcome::Succeeded), EffectStatus::Complete)?;
        Ok(())
    }));
    assert_eq!(fixture.store.recover_control_admission(&completing, &objects, true)?, ControlRecoveryOutcome::RecoveredCanonical);
    assert!(fixture.state.lock().before_materialize.is_none());
    assert_eq!(fixture.store.load_subject(agent)?.1.state, SubjectStepState::ResultRecorded);
    assert_eq!(fixture.store.external_attempt(child_attempt_id)?.final_receipt_id, Some(receipt_id));
    let child_operation = fixture.store.external_operation(child_operation.operation_id)?;
    let parent = fixture.store.external_task(fixture.parent.task.task_id)?;
    assert!(matches!(parent.state, TaskState::Completing { .. }));
    assert!(fixture.store.native_task_result(parent.task_id)?.is_none());
    assert!(fixture.store.finish_native_completing(parent.task_id)?.is_none(), "ordinary child still owns unfinished work");

    let child_result = fixture.store.complete_native_task(child, &child_operation)?.context("child result")?;
    assert_eq!(child_result.invocation_id, child.invocation.invocation_id);
    fixture.state.lock().lose_reply = true;
    let error = fixture.store.finish_native_completing(parent.task_id).err().context("chosen finish reply should be lost")?;
    assert!(error.to_string().contains("fixture lost chosen T2 reply"), "{error:#}");
    let finishing = fixture.chosen()?;
    assert_ne!(finishing.admission.action_id, completing.admission.action_id);
    assert_eq!(serde_json::to_value(fixture.store.external_task(parent.task_id)?)?, serde_json::to_value(&parent)?);
    assert!(fixture.store.native_task_result(parent.task_id)?.is_none());

    let reopened = fixture.reopen()?;
    let recovery = reopened.resume_prepared_step_commits(fixture.agent)?;
    assert_eq!(recovery.applied, 1);
    assert!(recovery.unresolved.is_empty());
    let result = reopened.native_task_result(parent.task_id)?.context("finished parent result")?;
    assert_eq!(result.receipt_id, parent_evidence.receipt_id);
    assert_eq!(result.invocation_id, fixture.parent.invocation.invocation_id);
    assert_eq!(reopened.external_evidence(parent_evidence.receipt_id)?, parent_evidence);
    assert!(reopened.external_task(parent.task_id)?.is_terminal());
    assert_eq!(reopened.finish_native_completing(parent.task_id)?, Some(result.clone()));
    assert_eq!(reopened.resume_prepared_step_commits(fixture.agent)?.applied, 0);
    let connection = rusqlite::Connection::open(fixture.dir.join("head.db"))?;
    assert_eq!(connection.query_row("SELECT count(*) FROM task_results", [], |row| row.get::<_, i64>(0))?, 2);
    assert_eq!(connection.query_row("SELECT count(*) FROM inbox_events WHERE kind='task_result'", [], |row| row.get::<_, i64>(0))?, 2);
    assert_eq!(connection.query_row("SELECT count(*) FROM attempts", [], |row| row.get::<_, i64>(0))?, 2);
    assert!(reopened.control_admission_applied_here(&finishing.admission)?);
    Ok(())
}

#[test]
fn peer_dialogue_reserves_both_writers_and_replays_without_the_source() -> Result<()> {
    use shenko_store::child::PeerMessage;
    use shenko_types::ids::EntityId;

    let fixture = NativeT2Fixture::new(true)?;
    let recipient = fixture.child.as_ref().context("recipient claim")?;
    let message = PeerMessage {
        message_id: EntityId::new(), sender_task_id: fixture.parent.task.task_id,
        sender_name: "sender".into(), sender_operation_id: fixture.parent.operation_id,
        step_index: 0, to: "peer".into(), recipient_task_id: Some(recipient.task.task_id),
        unresolved_reason: None, message: "retained peer observation".into(),
    };
    let sender_before = fixture.store.external_task(message.sender_task_id)?;
    let recipient_before = fixture.store.external_task(recipient.task.task_id)?;
    let before = fixture.store.current_commit_seq()?;
    let grants_before = fixture.state.lock().grants.len();
    fixture.state.lock().fail_before_choose = true;
    let error = fixture.store.deliver_peer_dialogue(&fixture.parent.invocation, &message)
        .expect_err("a refused source admission cannot publish either task");
    assert!(format!("{error:#}").contains("fixture failed before choosing dispatch"));
    assert_eq!(fixture.state.lock().grants.len(), grants_before);
    assert_eq!(fixture.store.current_commit_seq()?, before);
    assert_eq!(serde_json::to_value(fixture.store.external_task(message.sender_task_id)?)?,
        serde_json::to_value(&sender_before)?);
    assert_eq!(serde_json::to_value(fixture.store.external_task(recipient.task.task_id)?)?,
        serde_json::to_value(&recipient_before)?);

    let sent = fixture.store.deliver_peer_dialogue(&fixture.parent.invocation, &message)?;
    assert_eq!(sent.state, "delivered");
    let grant = fixture.chosen()?;
    for task in [&sender_before, &recipient_before] {
        assert!(grant.admission.conflict_aliases.contains(&format!(
            "task/{}/revision/{}", task.task_id, task.record_revision.0)));
    }
    assert!(fixture.store.control_admission_applied_here(&grant.admission)?);
    let sender_after = fixture.store.external_task(message.sender_task_id)?;
    let recipient_after = fixture.store.external_task(recipient.task.task_id)?;
    assert_eq!(sender_after.updated_commit, recipient_after.updated_commit);
    assert_eq!(sender_after.record_revision, sender_before.record_revision.next());
    assert_eq!(recipient_after.record_revision, recipient_before.record_revision.next());
    assert_eq!(fixture.store.pending_child_dialogue(recipient.task.task_id)?.len(), 1);

    // A fresh connection has the durable required policy but no attached
    // source, activated writer or dispatcher. Historical outcomes stay readable.
    let reopened = Store::open(fixture.dir.join("head.db"), fixture.dir.join("cas"))?;
    let before = reopened.current_commit_seq()?;
    assert_eq!(reopened.deliver_peer_dialogue(&fixture.parent.invocation, &message)?, sent);
    let fresh = PeerMessage { message_id: EntityId::new(), ..message.clone() };
    assert!(reopened.deliver_peer_dialogue(&fixture.parent.invocation, &fresh).is_err());
    reopened.close_external_writer();
    assert_eq!(reopened.deliver_peer_dialogue(&fixture.parent.invocation, &message)?, sent);
    assert_eq!(reopened.current_commit_seq()?, before);
    assert_eq!(reopened.pending_child_dialogue(recipient.task.task_id)?.len(), 1);
    Ok(())
}

#[test]
fn s1_dialogue_inbox_decode_failure_rolls_back_ordinary_and_captured_writes() -> Result<()> {
    use shenko_store::child::DIALOGUE_INBOX;
    use shenko_types::{ids::EntityId, output::{DialogueSpec, OutputAction, OutputControl},
        subject::{DialogueDirection, DialogueIntent, StepDecision}, task::TaskOwner};

    fn attach_dialogue(
        store: &Store,
        step: &mut shenko_types::subject::SubjectStep,
        messages: &[(EntityId, &str)],
    ) -> Result<()> {
        let mut decision: StepDecision = serde_json::from_slice(&store.cas.read_object(
            &step.decision_ref.as_ref().context("candidate decision")?.digest,
        )?)?;
        for &(target_task_id, message) in messages {
            let intent = DialogueIntent {
                intent_id: EntityId::new(), target_task_id, message: message.into(),
                in_reply_to: None, direction: DialogueDirection::ToChild,
            };
            decision.output.controls.push(OutputControl {
                action_id: intent.intent_id, spans: vec![],
                action: OutputAction::Dialogue { spec: DialogueSpec {
                    target_task_id, message: message.into(), in_reply_to: None,
                    direction: DialogueDirection::ToChild,
                } },
            });
            decision.child_dialogue_intents.push(intent);
        }
        step.decision_ref = Some(store.cas.seal_object(
            &serde_json::to_vec(&decision)?, "application/json", "test", "StepDecision", 8,
        )?.object);
        Ok(())
    }

    fn retained_rows(conn: &rusqlite::Connection)
        -> Result<Vec<Vec<Vec<rusqlite::types::Value>>>>
    {
        let mut retained = Vec::new();
        for sql in [
            "SELECT * FROM tasks ORDER BY task_id",
            "SELECT * FROM subject_steps ORDER BY step_id",
            "SELECT * FROM subject_intents ORDER BY intent_id",
            "SELECT * FROM subject_intent_steps ORDER BY step_id",
            "SELECT * FROM control_prepared_transitions ORDER BY action_id",
            "SELECT * FROM control_materializations ORDER BY conflict_key",
            "SELECT * FROM inbox_events ORDER BY event_id",
            "SELECT * FROM commit_log ORDER BY ledger_id,seq",
        ] {
            let mut query = conn.prepare(sql)?;
            let columns = query.column_count();
            let rows = query.query_map([], |row| {
                (0..columns).map(|index| row.get(index))
                    .collect::<rusqlite::Result<Vec<rusqlite::types::Value>>>()
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            retained.push(rows);
        }
        Ok(retained)
    }

    for captured in [false, true] {
        let (dir, store, agent) = initialized_subject();
        let (head, first) = store.load_subject(agent)?;
        let mut damaged = recovery_task(&store, agent, 1, false);
        let mut clean = recovery_task(&store, agent, 2, false);
        damaged.owner = TaskOwner::Task { id: head.subject_root_task_id };
        clean.owner = TaskOwner::Task { id: head.subject_root_task_id };
        let damaged_id = damaged.task_id;
        let clean_id = clean.task_id;
        let mut batch = WriteBatch::new();
        batch.task_upserts.extend([damaged, clean]);
        store.commit(batch)?;
        assert!(store.pending_child_dialogue(clean_id)?.is_empty());
        assert_eq!(store.consume_child_dialogue(clean_id, &[EntityId::new()])?, 0);

        // Retain two real S1 deliveries before introducing the damaged memo.
        let result = recorded_result(&store, &head, first)?;
        let next = EntityId::new();
        let mut initial = committed_candidate(&store, result, next);
        attach_dialogue(&store, &mut initial, &[
            (damaged_id, "retain the first original owner turn"),
            (damaged_id, "retain the second original owner turn"),
        ])?;
        store.commit_step_and_advance_head(&initial, &head, next)?;
        assert_eq!(store.pending_child_dialogue(damaged_id)?.len(), 2);
        let chosen = if captured { Some(canonical_source(&store, agent, false)?) } else { None };

        let connection = rusqlite::Connection::open(dir.join("head.db"))?;
        let original: String = connection.query_row(
            "SELECT data FROM tasks WHERE task_id=?1", [damaged_id.to_string()], |row| row.get(0),
        )?;
        let mut invalid: serde_json::Value = serde_json::from_str(&original)?;
        invalid["memos"][DIALOGUE_INBOX].as_array_mut().context("two retained deliveries")?
            .push(serde_json::json!({"message":"invalid member without delivery bindings"}));
        let damaged_row = serde_json::to_string(&invalid)?;
        assert_eq!(connection.execute("UPDATE tasks SET data=?2 WHERE task_id=?1",
            rusqlite::params![damaged_id.to_string(), damaged_row])?, 1);
        let decoding_error = store.pending_child_dialogue(damaged_id).unwrap_err();
        let expected_error = decoding_error.downcast_ref::<serde_json::Error>()
            .context("retain the original inbox decoding cause")?.to_string();

        let (head, first) = store.load_subject(agent)?;
        let result = recorded_result(&store, &head, first)?;
        let next = EntityId::new();
        let mut candidate = committed_candidate(&store, result, next);
        attach_dialogue(&store, &mut candidate, &[
            (clean_id, "this earlier sibling delivery must roll back"),
            (damaged_id, "do not replace an unreadable inbox"),
        ])?;
        let before = retained_rows(&connection)?;
        let head_and_step_before = serde_json::to_value(store.load_subject(agent)?)?;
        let seq_before = store.current_commit_seq()?;
        let history_before = serde_json::to_value(
            store.session_events_range(EntityId(head.branch_id.0), 0, 100)?,
        )?;
        let chosen_before = chosen.as_ref().map(|chosen| {
            serde_json::to_value(chosen.lock().as_ref())
        }).transpose()?;
        let mut objects_before = Vec::new();
        for reference in [
            candidate.input_plan_ref.as_ref(), candidate.model_result_ref.as_ref(),
            candidate.model_outcome_ref.as_ref(), candidate.decision_ref.as_ref(),
        ] {
            let reference = reference.context("original S0/model/decision binding")?;
            objects_before.push((reference.clone(), store.cas.read_object(&reference.digest)?));
        }

        let error = store.commit_step_and_advance_head(&candidate, &head, next)
            .expect_err("the entire ordinary/captured S1 must reject the malformed existing memo");
        assert!(error.to_string().contains(&expected_error), "captured={captured}: {error}");
        assert_eq!(store.current_commit_seq()?, seq_before);
        assert_eq!(serde_json::to_value(store.load_subject(agent)?)?, head_and_step_before);
        assert_eq!(retained_rows(&connection)?, before,
            "captured={captured}: no sibling, intent, successor, preparation or receipt may survive");
        assert_eq!(serde_json::to_value(
            store.session_events_range(EntityId(head.branch_id.0), 0, 100)?,
        )?, history_before);
        assert_eq!(chosen.as_ref().map(|chosen| {
            serde_json::to_value(chosen.lock().as_ref())
        }).transpose()?, chosen_before, "failed capture must not ask the source to choose S1");
        for (reference, bytes) in &objects_before {
            assert_eq!(store.cas.read_object(&reference.digest)?, *bytes);
        }
        drop(connection);
        drop(store);

        let reopened = Store::open(dir.join("head.db"), dir.join("cas"))?;
        let connection = rusqlite::Connection::open(dir.join("head.db"))?;
        assert_eq!(reopened.current_commit_seq()?, seq_before);
        assert_eq!(serde_json::to_value(reopened.load_subject(agent)?)?, head_and_step_before);
        assert_eq!(retained_rows(&connection)?, before);
        let retained: String = connection.query_row(
            "SELECT data FROM tasks WHERE task_id=?1", [damaged_id.to_string()], |row| row.get(0),
        )?;
        assert_eq!(retained, damaged_row);
        assert!(reopened.pending_child_dialogue(clean_id)?.is_empty());
        let error = reopened.pending_child_dialogue(damaged_id).unwrap_err();
        assert_eq!(error.downcast_ref::<serde_json::Error>()
            .context("reopen must preserve the same original decoding cause")?.to_string(), expected_error);
        assert_eq!(serde_json::to_value(
            reopened.session_events_range(EntityId(head.branch_id.0), 0, 100)?,
        )?, history_before);
        for (reference, bytes) in &objects_before {
            assert_eq!(reopened.cas.read_object(&reference.digest)?, *bytes);
        }
    }
    Ok(())
}
