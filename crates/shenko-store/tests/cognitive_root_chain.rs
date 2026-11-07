//! Chain test: capture → reinstate → verify chain — SPEC §51.4
//!
//! Exercises the cognitive root append-only version chain through
//! the store module. Does NOT run a build or any external verification.

use std::sync::Arc;

use shenko_store::Store;
use shenko_types::cognition::{AttitudeConfidence, ReasonPointer};
use shenko_types::cognitive_root::{
    CognitiveRootCapture, CognitiveRootContent, CognitiveRootReinstatement,
    CognitionSelectorRef, HomeContentRef, RootApplication, RootApplicationScope,
    RootApplicationState, RootFence, RootRef, RuntimeStateGap, RuntimeStateParticipant,
    SoftwareSlotRef, UnitSelectorEntry, UnitSelectorSnapshot,
    HOME_CONTENT_CODEC, HOME_CONTENT_CODEC_MAJOR, HOME_CONTENT_NAMESPACE,
    SOFTWARE_SLOT_CODEC, SOFTWARE_SLOT_CODEC_MAJOR, SOFTWARE_SLOT_NAMESPACE,
    UNIT_SELECTOR_CODEC, UNIT_SELECTOR_CODEC_MAJOR, UNIT_SELECTOR_NAMESPACE,
};
use shenko_types::commit::CommitRef;
use shenko_types::ids::{
    BranchId, ControllerEpoch, EntityId, LedgerSeq, RecordRevision, RuntimeGeneration,
};
use shenko_types::object::Digest;
use shenko_types::subject::{
    GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
};

fn tmp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-root-chain-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_store() -> (Arc<Store>, std::path::PathBuf) {
    let dir = tmp_dir();
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());

    let home = store
        .cas
        .seal_object(b"home manifest bytes", "application/json", "seed", "json", 1)
        .unwrap()
        .object;
    let input = store
        .cas
        .seal_object(b"task input", "text/plain", "inputs", "raw", 1)
        .unwrap()
        .object;

    let agent_id = EntityId::new();
    let genesis_id = EntityId::new();
    let branch_id = BranchId::new();
    let first_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: home.clone(),
        initial_profile_ref: home.clone(),
        home_manifest_ref: home.clone(),
        bootstrap_event_ref: home.clone(),
        created_commit: genesis_commit,
    };
    let head = SubjectBranchHead {
        agent_id,
        branch_id: branch_id.clone(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: genesis_commit,
        next_step_id: first_step_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first_step = SubjectStep {
        step_id: first_step_id,
        branch_id,
        step_no: 1,
        expected_parent_commit: genesis_commit,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
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
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        input,
        genesis_commit,
    );
    store
        .initialize_genesis(&genesis, &head, &first_step, &root_task)
        .unwrap();

    (store, dir)
}

fn seal_home(store: &Store) -> HomeContentRef {
    let bytes = b"{\"test_home\": true}";
    let sealed = store
        .cas
        .seal_object(
            bytes,
            "application/json",
            HOME_CONTENT_NAMESPACE,
            HOME_CONTENT_CODEC,
            HOME_CONTENT_CODEC_MAJOR,
        )
        .unwrap();
    HomeContentRef {
        manifest: sealed.object.digest,
    }
}

fn seal_units(store: &Store) -> CognitionSelectorRef {
    let snapshot = UnitSelectorSnapshot::ok(vec![UnitSelectorEntry {
        unit_id: "test-awareness".into(),
        revision: "1".into(),
        artifact_digest: Digest::blake3(b"test-artifact"),
        enabled: true,
    }]);
    let bytes = snapshot.canonical_bytes().unwrap();
    let sealed = store
        .cas
        .seal_object(
            &bytes,
            "application/json",
            UNIT_SELECTOR_NAMESPACE,
            UNIT_SELECTOR_CODEC,
            UNIT_SELECTOR_CODEC_MAJOR,
        )
        .unwrap();
    CognitionSelectorRef {
        snapshot: sealed.object.digest,
    }
}

fn seal_software(store: &Store) -> SoftwareSlotRef {
    let bytes = b"{\"generation\": \"default\"}";
    let sealed = store
        .cas
        .seal_object(
            bytes,
            "application/json",
            SOFTWARE_SLOT_NAMESPACE,
            SOFTWARE_SLOT_CODEC,
            SOFTWARE_SLOT_CODEC_MAJOR,
        )
        .unwrap();
    SoftwareSlotRef {
        manifest: sealed.object.digest,
        generation: RuntimeGeneration::default(),
        artifact_digest: None,
    }
}

fn default_fence() -> RootFence {
    RootFence {
        generation: RuntimeGeneration::default(),
        epoch: ControllerEpoch(1),
        selector_revision: 0,
    }
}

fn make_content(store: &Store) -> CognitiveRootContent {
    CognitiveRootContent {
        schema_version: CognitiveRootContent::SCHEMA_VERSION,
        home: seal_home(store),
        software: seal_software(store),
        units: seal_units(store),
        runtime: RuntimeStateParticipant::NotCaptured(RuntimeStateGap::new(
            "test: no wasm backend",
        )),
    }
}

#[test]
fn capture_creates_first_root_node() {
    let (store, _dir) = test_store();
    let agent_id = store.get_existing_agent_id().unwrap().unwrap();

    let content = make_content(&store);
    let capture = CognitiveRootCapture {
        request_id: EntityId::new(),
        fence: default_fence(),
        content,
        reasons: Vec::new(),
        confidence: AttitudeConfidence::Unknown,
        effects: Vec::new(),
    };

    let node = store.capture_cognitive_root(capture).unwrap();
    assert_eq!(node.root_revision, 1);
    assert!(node.reinstated_from.is_none());

    // Head is readable.
    let head = store.cognitive_root_head(agent_id).unwrap().unwrap();
    assert_eq!(head.node.root_revision, 1);
    assert_eq!(head.node_count, 1);
    assert_eq!(head.distinct_content_count, 1);
}

#[test]
fn capture_then_reinstate_chain() {
    let (store, _dir) = test_store();
    let agent_id = store.get_existing_agent_id().unwrap().unwrap();

    // First capture.
    let content1 = make_content(&store);
    let node1 = store
        .capture_cognitive_root(CognitiveRootCapture {
            request_id: EntityId::new(),
            fence: default_fence(),
            content: content1,
            reasons: Vec::new(),
            confidence: AttitudeConfidence::Unknown,
            effects: Vec::new(),
        })
        .unwrap();
    assert_eq!(node1.root_revision, 1);

    // Second capture with different home.
    let home2_bytes = b"{\"test_home\": \"version2\"}";
    let home2_sealed = store
        .cas
        .seal_object(
            home2_bytes,
            "application/json",
            HOME_CONTENT_NAMESPACE,
            HOME_CONTENT_CODEC,
            HOME_CONTENT_CODEC_MAJOR,
        )
        .unwrap();
    let content2 = CognitiveRootContent {
        schema_version: CognitiveRootContent::SCHEMA_VERSION,
        home: HomeContentRef {
            manifest: home2_sealed.object.digest.clone(),
        },
        software: seal_software(&store),
        units: seal_units(&store),
        runtime: RuntimeStateParticipant::NotCaptured(RuntimeStateGap::new("test")),
    };

    let node2 = store
        .capture_cognitive_root(CognitiveRootCapture {
            request_id: EntityId::new(),
            fence: default_fence(),
            content: content2,
            reasons: Vec::new(),
            confidence: AttitudeConfidence::Unknown,
            effects: Vec::new(),
        })
        .unwrap();
    assert_eq!(node2.root_revision, 2);

    // Reinstate to revision 1.
    let reinstatement = CognitiveRootReinstatement {
        request_id: EntityId::new(),
        fence: default_fence(),
        target: RootRef::of(&node1),
        reasons: vec![ReasonPointer::content(node2.content.digest().clone())],
        confidence: AttitudeConfidence::Unknown,
        effects: Vec::new(),
    };

    let node3 = store.reinstate_cognitive_root(reinstatement).unwrap();
    assert_eq!(node3.root_revision, 3);
    assert!(node3.is_reinstatement());
    let reinstated = node3.reinstated_from.as_ref().unwrap();
    assert_eq!(reinstated.root_revision, 1);
    assert!(reinstated.matches(&node1));

    // Content dedup: node1 and node3 point to same content digest.
    assert_eq!(node3.content.digest(), node1.content.digest());

    // Chain has 3 entries.
    let chain = store.cognitive_root_chain(agent_id, 10).unwrap();
    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].root_revision, 3);
    assert_eq!(chain[1].root_revision, 2);
    assert_eq!(chain[2].root_revision, 1);

    // Head now points to rev 3.
    let head = store.cognitive_root_head(agent_id).unwrap().unwrap();
    assert_eq!(head.node.root_revision, 3);
    assert_eq!(head.node_count, 3);
    // Only 2 distinct contents: rev1==rev3, rev2 is different.
    assert_eq!(head.distinct_content_count, 2);
}

#[test]
fn idempotent_capture_returns_same_node() {
    let (store, _dir) = test_store();

    let content = make_content(&store);
    let request_id = EntityId::new();
    let capture = CognitiveRootCapture {
        request_id,
        fence: default_fence(),
        content,
        reasons: Vec::new(),
        confidence: AttitudeConfidence::Unknown,
        effects: Vec::new(),
    };

    let node1 = store.capture_cognitive_root(capture.clone()).unwrap();
    let node2 = store.capture_cognitive_root(capture).unwrap();
    assert_eq!(node1.root_revision, node2.root_revision);
    assert_eq!(node1.attitude_id, node2.attitude_id);
}

#[test]
fn fence_mismatch_rejects_capture() {
    let (store, _dir) = test_store();

    let content = make_content(&store);
    // Wrong epoch.
    let capture = CognitiveRootCapture {
        request_id: EntityId::new(),
        fence: RootFence {
            generation: RuntimeGeneration::default(),
            epoch: ControllerEpoch(999),
            selector_revision: 0,
        },
        content,
        reasons: Vec::new(),
        confidence: AttitudeConfidence::Unknown,
        effects: Vec::new(),
    };

    let result = store.capture_cognitive_root(capture);
    assert!(result.is_err(), "capture with wrong fence should fail");
}

#[test]
fn root_application_recording() {
    let (store, _dir) = test_store();

    let content = make_content(&store);
    let node = store
        .capture_cognitive_root(CognitiveRootCapture {
            request_id: EntityId::new(),
            fence: default_fence(),
            content,
            reasons: Vec::new(),
            confidence: AttitudeConfidence::Unknown,
            effects: Vec::new(),
        })
        .unwrap();

    let zero_commit = CommitRef::new(store.ledger.ledger_id(), LedgerSeq(0));

    // Record an application.
    store
        .record_root_application(&RootApplication {
            root_revision: node.root_revision,
            scope: RootApplicationScope::HomeFiles,
            state: RootApplicationState::Applied { roots: 1, files: 3 },
            updated_commit: zero_commit,
        })
        .unwrap();

    let apps = store.root_applications(node.root_revision).unwrap();
    assert_eq!(apps.len(), 1);
    assert!(apps[0].applied());
    assert_eq!(apps[0].root_revision, 1);

    // Record a deferred application.
    store
        .record_root_application(&RootApplication {
            root_revision: node.root_revision,
            scope: RootApplicationScope::HomeFiles,
            state: RootApplicationState::Deferred {
                detail: "guest still running".into(),
            },
            updated_commit: zero_commit,
        })
        .unwrap();

    let apps = store.root_applications(node.root_revision).unwrap();
    assert_eq!(apps.len(), 2);
    assert!(apps[0].applied());
    assert!(!apps[1].applied());
}
