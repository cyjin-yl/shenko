//! Canonical subject genesis shared by Store and installed-runtime journeys.
use anyhow::Result;
use shenko_store::Store;

pub fn initialize_subject_in_with_route(
    store: &Store,
    bootstrap_event: &[u8],
    route_dir: Option<&std::path::Path>,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::activation::{ActivationOrigin, ActivationSelector, RouteAuthority};
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::Digest;
    use shenko_types::subject::{
        GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep, SubjectStepState,
    };

    let agent_id = EntityId::new();
    let branch_id = BranchId::new();
    let genesis_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let first_step_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let dummy_ref = store
        .cas
        .seal_object(bootstrap_event, "application/json", "test", "raw", 1)?
        .object;
    let genesis_commit = CommitRef::new(ledger_id, LedgerSeq(0));

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id,
        launch_id,
        seed_manifest_ref: dummy_ref.clone(),
        initial_profile_ref: dummy_ref.clone(),
        home_manifest_ref: dummy_ref.clone(),
        bootstrap_event_ref: dummy_ref.clone(),
        created_commit: genesis_commit,
    };

    let head = SubjectBranchHead {
        branch_origin: None,
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
        branch_id: branch_id.clone(),
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

    let selector = if let Some(dir) = route_dir {
        let artifact = dir.join("route-fixture-artifact");
        let artifact_bytes = b"context metadata route fixture; never executed";
        std::fs::write(&artifact, artifact_bytes)?;
        let route = RouteAuthority {
            schema_version: RouteAuthority::SCHEMA_VERSION,
            agent_id,
            generation: head.software_generation.clone(),
            active_epoch: 1,
            selector_revision: 0,
            data_dir: dir.to_string_lossy().into_owned(),
            slot_dir: dir.to_string_lossy().into_owned(),
            artifact_path: artifact.to_string_lossy().into_owned(),
            artifact_digest: Digest::blake3(artifact_bytes),
            launch_command: Vec::new(),
            primary_claim: None,
            activation_id: None,
        };
        let route_ref = store
            .cas
            .seal_object(
                &serde_json::to_vec(&route)?,
                "application/json",
                "activation",
                "route-authority",
                1,
            )?
            .object;
        Some(ActivationSelector {
            agent_id,
            selector_revision: 0,
            active_epoch: 1,
            generation: head.software_generation.clone(),
            state_ref: dummy_ref.clone(),
            origin: ActivationOrigin::Genesis {
                genesis_ref: head.genesis_ref.clone(),
            },
            route_authority_ref: route_ref,
        })
    } else {
        None
    };
    // Initialize genesis and its optional real selector in one transaction.
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        dummy_ref,
        genesis_commit,
    );
    store.initialize_genesis_with(
        &genesis,
        &head,
        &first_step,
        &root_task,
        selector
            .as_ref()
            .map(|selector| selector as &dyn shenko_store::GenesisExtension),
    )?;

    Ok(agent_id)
}
