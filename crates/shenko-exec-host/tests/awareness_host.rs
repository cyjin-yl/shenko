//! Real native consumer for the installable awareness capability host.
//!
//! SPEC §51.2/§51.3/§51.8. This case installs the shipped awareness material
//! into a real Node authority, publishes the real `AwarenessHostPlugin` on a
//! real Cordis supervisor, runs `awareness.notice:1` as a real native task
//! against the real Store, and reads the durable result back.
//!
//! What it deliberately proves by observation, not by prose:
//!
//! * the capability is absent until the installation records an authenticated
//!   adoption for the provider — installed and enabled is not adopted;
//! * one atom joins the offered material only after its own adoption, so two
//!   atoms with one adopted are not offered together by accident;
//! * a perception atom's notice rests on the observation this node actually
//!   registered and keeps the observation digest and attribution;
//! * a modeled other subject's reading is disclosed as that subject's view and
//!   never becomes the subject's own observation;
//! * disabling the provider removes the capability and the notices, and the
//!   native read then reports `absent` instead of returning stale material;
//! * the notice child declares no effect and no capability: it reads.
//!
//! Nothing here is a model-efficacy claim. All runs are source-only until the
//! parent executes them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use parking_lot::Mutex;
use serde_json::json;
use shenko_awareness_core::{
    AwarenessHost, AwarenessHostPlugin, CapabilitySelection, CognitionCatalog, NOTICE_AUTHORITY,
    NoticeAttribution, NoticeReason, PerceptionBinding, PerceptionFact, PerceptionObserver,
    PerceptionPerspective, awareness_seed_packages, cognition_package_root,
    parse_cognition_entry, seed_packages,
};
use shenko_exec_host::AwarenessNoticeHandler;
use shenko_kernel::Supervisor;
use shenko_node::{AdoptionDecision, AdoptionEvidence, AdoptionMethod, Node, Request as NodeRequest, UnitRequest};
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService, NativeTaskHandler, NativeTaskHandlerPlugin};
use shenko_types::commit::CommitRef;
use shenko_types::ids::*;
use shenko_types::object::Digest;
use shenko_types::subject::*;
use shenko_types::task::*;

const MODELED_UNIT: &str = "perception.consumer.modeled-other";
const MODELED_SUBJECT: &str = "someone-else";

struct Sandbox {
    dir: PathBuf,
    store: Arc<Store>,
    agent: EntityId,
    node: Arc<Mutex<Node>>,
    catalog: Arc<CognitionCatalog>,
}

impl Sandbox {
    fn open(label: &str) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("shenko-awareness-host-{label}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir)?;
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas"))?);
        let agent = initialize(&store)?;
        let mut node = Node::open(&dir.join("node"))?;
        node.handle(NodeRequest::Initialize {
            node_id: EntityId::new(),
            identity: None,
        })?;
        let node = Arc::new(Mutex::new(node));
        let catalog = Arc::new(CognitionCatalog::new(Arc::clone(&node)));
        let root = cognition_package_root(&dir);
        let installed: Vec<String> = catalog.units()?.into_iter().map(|unit| unit.unit_id).collect();
        for package in seed_packages()?.into_iter().chain(awareness_seed_packages()?) {
            if installed.contains(&package.unit_id) {
                continue;
            }
            catalog.install(&package, &root)?;
        }
        Ok(Self { dir, store, agent, node, catalog })
    }

    fn package_root(&self) -> PathBuf {
        cognition_package_root(&self.dir)
    }

    fn fact(&self, unit_id: &str) -> Result<shenko_node::InstalledUnitFact> {
        self.node
            .lock()
            .installed_unit_fact(unit_id)?
            .with_context(|| format!("unit '{unit_id}' is not installed"))
    }

    /// Record one real observation of this installation against one exact
    /// unit version and return its digest.
    fn observe_locally(&self, unit_id: &str, bytes: &[u8], kind: &str) -> Result<Digest> {
        let fact = self.fact(unit_id)?;
        let digest = Digest::blake3(bytes);
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::RegisterObservation {
            unit_id: unit_id.to_string(),
            revision: fact.revision.clone(),
            artifact_digest: fact.artifact_digest.clone(),
            kind: kind.to_string(),
            bytes: bytes.to_vec(),
        })?;
        Ok(digest)
    }

    /// Record this installation's own adoption attitude for one unit version,
    /// citing the observation bytes it actually holds.
    fn adopt(&self, unit_id: &str, observation: &Digest, notes: &str) -> Result<()> {
        let fact = self.fact(unit_id)?;
        let mut node = self.node.lock();
        node.handle_unit_request(UnitRequest::RecordAdoption {
            unit_id: unit_id.to_string(),
            revision: fact.revision.clone(),
            artifact_digest: fact.artifact_digest.clone(),
            decision: AdoptionDecision::Adopted,
            method: AdoptionMethod::OperatorStatement,
            evidence: vec![AdoptionEvidence::StoredObservation {
                digest: observation.clone(),
            }],
            confidence: None,
            notes: Some(notes.to_string()),
            enable: Some(true),
        })?;
        Ok(())
    }

    fn host(&self, selection: &CapabilitySelection) -> Arc<AwarenessHost> {
        Arc::new(AwarenessHost::new(Arc::clone(&self.catalog), selection.clone()))
    }
}

fn initialize(store: &Store) -> Result<EntityId> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"synthetic awareness host test", "text/plain", "fixture", "raw", 1)?
        .object;
    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent,
        bootstrap_id: EntityId::new(),
        launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(),
        initial_profile_ref: object.clone(),
        home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(),
        created_commit: precommit,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("awareness-host-test".into()),
        genesis_ref: GenesisRef { ledger_id, genesis_id },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: precommit,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: precommit,
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
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, precommit);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    Ok(agent)
}

/// Admit one `awareness.notice` native task durably, outside a subject step.
fn admit_notice_task(
    store: &Store,
    agent: EntityId,
    implementation: ImplementationRef,
    limit: usize,
) -> Result<EntityId> {
    let task_id = EntityId::new();
    let input_ref = store
        .cas
        .seal_object(
            &serde_json::to_vec(&json!({ "limit": limit }))?,
            "application/json",
            "fixture",
            "awareness-notice-input",
            1,
        )?
        .object;
    let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: agent,
        conversation_id: agent,
        owner: TaskOwner::Task {
            id: store.load_subject(agent)?.0.subject_root_task_id,
        },
        background: false,
        kind: "awareness.notice".into(),
        definition_version: 1,
        implementation_ref: implementation,
        input_ref,
        state: TaskState::Pending { checkpoint_ref: None },
        abort_request: None,
        memos: serde_json::Map::from_iter([
            ("intent_id".into(), json!(task_id)),
            (shenko_store::child::NATIVE_INPUT.into(), json!("object")),
        ]),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: None,
        created_commit: zero,
        updated_commit: zero,
    };
    let raw = shenko_types::store::RawInboxEvent {
        event_id: EntityId::new(),
        agent_id: agent,
        source: "awareness-host-test".into(),
        kind: "artifact_changed".into(),
        payload_json: serde_json::to_string(&json!({"task_id": task_id}))?,
        received_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    ensure!(
        store.admit_native_task(&task, &raw)?.is_none(),
        "fresh native admission"
    );
    Ok(task_id)
}

/// A source that answers with one prepared fact.
struct Fixed(PerceptionFact);

impl PerceptionObserver for Fixed {
    fn observe(&self) -> Result<PerceptionFact> {
        Ok(self.0.clone())
    }
}

fn custom_perception_entry(revision: &str) -> String {
    format!(
        "---\nid: {MODELED_UNIT}\nversion: {revision}\nkind: perception\ntitle: 换位思考夹具\n\
         capability_card: 通过另一个主体的感知解释同一段经历，读法是「推测对方会这样看」\n\
         why: 用别人的感知解释经验是换位思考，它停在对方那份感知的理解上，不停在自己的观察上。\n\
         applicability: 需要推测另一个主体会如何理解同一段经历时\n\
         provenance: [SPEC §51.2]\n---\n\
         这是一则换位思考的感知定义：它教主体如何借另一个主体的感知读同一段经历。\n\
         这样得到的读法属于对方视角，不是本主体自己的观察。\n"
    )
}

#[tokio::test(flavor = "current_thread")]
async fn the_capability_is_absent_until_this_installation_adopts_it() -> Result<()> {
    let sandbox = Sandbox::open("residency")?;
    let selection = CapabilitySelection::host("awareness.host");
    let host = sandbox.host(&selection);

    // Installed and enabled, but nobody here adopted it: not resident. Being
    // present in the store is not being adopted.
    let before = host.state()?;
    ensure!(
        !before.resident && before.provider.is_some() && before.declared,
        "an installed, declaring, enabled provider without adoption is not resident: {before:?}"
    );
    ensure!(
        before.reason.contains("采纳"),
        "the absence reason names the missing adoption: {}",
        before.reason
    );
    ensure!(host.notices(8)?.is_empty(), "an absent capability offers no material");
    ensure!(
        host.render_notices(8)?.is_none(),
        "an absent capability renders no notice block"
    );

    // The provider's own authenticated adoption for this exact revision.
    let observation = sandbox.observe_locally(
        "awareness.host",
        b"consumer checked that the provider body is readable at its exact revision",
        "consumer_check",
    )?;
    sandbox.adopt("awareness.host", &observation, "consumer adopted its own host capability")?;
    let resident = host.state()?;
    ensure!(
        resident.resident,
        "the adopted provider makes the capability resident: {resident:?}"
    );
    ensure!(
        resident
            .provider
            .as_ref()
            .is_some_and(|provider| provider.unit_id == "awareness.host"),
        "residency names the exact provider revision"
    );

    // One atom is adopted, the other is not: the un-adopted one is not offered
    // merely because the metapackage shipped it.
    let atom = "awareness.foundation-cognition";
    let atom_observation = sandbox.observe_locally(
        atom,
        b"consumer checked the atom body at its exact revision",
        "consumer_check",
    )?;
    let without_atom = host.notices(8)?;
    ensure!(
        without_atom.is_empty(),
        "no atom joins the material before its own adoption: {without_atom:?}"
    );
    sandbox.adopt(atom, &atom_observation, "consumer adopted the foundation attention")?;
    let notices = host.notices(8)?;
    ensure!(notices.len() == 1, "exactly the adopted atom is offered: {notices:?}");
    let notice = &notices[0];
    ensure!(
        notice.atom.unit_id == atom && notice.attention == sandbox.fact(atom)?.descriptor.config
            .pointer("/cognition/capability_card")
            .and_then(|value| value.as_str())
            .unwrap_or_default(),
        "the notice carries the atom and its own bounded attention"
    );
    ensure!(
        notice.authority == NOTICE_AUTHORITY,
        "every notice states that it grants nothing"
    );
    match &notice.reason {
        NoticeReason::AuthoredContent { unit_id, revision, content_digest } => {
            ensure!(
                unit_id == atom && content_digest == &sandbox.fact(atom)?.artifact_digest.0,
                "the authored reason names this atom's own content digest"
            );
            ensure!(!revision.is_empty(), "the reason names the exact revision");
        }
        other => bail!("an authored atom's reading stops at its content: {other:?}"),
    }
    ensure!(
        notice.attribution == NoticeAttribution::Own,
        "authored material of this installation is its own reading"
    );
    let block = host.render_notices(8)?.context("the resident capability renders a block")?;
    ensure!(
        block.contains(atom) && block.contains(NOTICE_AUTHORITY),
        "the rendered block names the atom and its advisory status: {block}"
    );

    // The real plugin publishes the same host by service name.
    let supervisor = Supervisor::new(RuntimeGeneration("awareness-host-test".into()));
    let _plugin = supervisor.spawn(AwarenessHostPlugin, Arc::clone(&host)).await?;
    let published = supervisor
        .context()
        .try_service::<AwarenessHost>()
        .map_err(|error| anyhow::anyhow!("awareness host service: {error}"))?;
    ensure!(
        Arc::ptr_eq(&published, &host),
        "the published service is the same projection, not a second one"
    );

    // Every call re-reads the installation: disabling the provider withdraws
    // the capability without a restart, and no stale notice survives.
    sandbox.catalog.set_enabled("awareness.host", false)?;
    let disabled = host.state()?;
    ensure!(!disabled.resident, "a disabled provider leaves the capability absent: {disabled:?}");
    ensure!(host.notices(8)?.is_empty(), "a disabled provider offers nothing");
    ensure!(
        host.render_notices(8)?.is_none(),
        "nothing is rendered for an absent capability"
    );
    supervisor.stop().await?;

    let receipt = json!({
        "schema_version": 1,
        "consumer": "awareness-host-residency",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "selection": selection,
        "before": {"resident": before.resident, "reason": before.reason},
        "resident": {"provider": resident.provider, "reason": resident.reason},
        "atom": notice,
        "rendered": block,
        "disabled": {"resident": disabled.resident, "reason": disabled.reason},
        "limits": {"max_notices": shenko_awareness_core::MAX_NOTICES, "max_notice_bytes": shenko_awareness_core::MAX_NOTICE_BYTES},
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_native_read_reports_the_capability_and_grants_nothing() -> Result<()> {
    let sandbox = Sandbox::open("native")?;
    let selection = CapabilitySelection::host("awareness.host");
    let host = sandbox.host(&selection);
    let provider_observation = sandbox.observe_locally(
        "awareness.host",
        b"consumer checked the provider body before adopting it",
        "consumer_check",
    )?;
    sandbox.adopt("awareness.host", &provider_observation, "adopt the host capability")?;
    let atom = "awareness.foundation-cognition";
    let atom_observation = sandbox.observe_locally(atom, b"atom body check", "consumer_check")?;
    sandbox.adopt(atom, &atom_observation, "adopt the foundation attention")?;

    let handler: Arc<dyn NativeTaskHandler> =
        Arc::new(AwarenessNoticeHandler::new(Arc::clone(&sandbox.store), Arc::clone(&host)));
    let descriptor = handler.descriptor();
    ensure!(
        descriptor.kind == "awareness.notice" && descriptor.definition_version == 1,
        "the child is the notice reader: {descriptor:?}"
    );
    ensure!(
        handler.effect_contracts().is_empty() && handler.capabilities().is_empty(),
        "reading notices declares no effect and no capability"
    );

    let native_supervisor = Supervisor::new(RuntimeGeneration("awareness-host-test".into()));
    native_supervisor
        .spawn(NativeChildPlugin, (Arc::clone(&sandbox.store), RuntimeGeneration("awareness-host-test".into())))
        .await?;
    let native = native_supervisor.context().try_service::<NativeChildService>()?;
    native_supervisor
        .spawn(NativeTaskHandlerPlugin, Arc::clone(&handler) as Arc<dyn NativeTaskHandler>)
        .await?;
    native.recover()?;
    let (head, _) = sandbox.store.load_subject(sandbox.agent)?;
    sandbox.store.start_subject_dispatcher()?;
    let task = admit_notice_task(
        &sandbox.store,
        sandbox.agent,
        descriptor.implementation_ref.clone(),
        8,
    )?;
    drive_until(&native, &sandbox.store, task).await?;
    let result = sandbox
        .store
        .native_task_result(task)?
        .context("the notice read produced a durable T2")?;
    ensure!(
        matches!(result.outcome, TaskOutcome::Succeeded),
        "reading notices succeeds: {:?}",
        result.outcome
    );
    let payload = result.result.as_ref().context("the read result is kept")?;
    ensure!(
        payload["resident"] == json!(true) && payload["status"] == json!("ok"),
        "the resident capability reports ok: {payload}"
    );
    let notices = payload["notices"].as_array().context("notices array")?;
    ensure!(!notices.is_empty(), "the adopted atom is returned: {payload}");
    ensure!(
        notices[0]["authority"] == json!(NOTICE_AUTHORITY),
        "the notice carries its advisory status in the durable result"
    );
    let operations = sandbox
        .store
        .external_task_operations(task, "awareness.notice.v1")?;
    ensure!(operations.len() == 1, "one armed attempt, one operation: {operations:?}");
    let operation = &operations[0];
    ensure!(
        matches!(operation.effect_status, shenko_types::operation::EffectStatus::None),
        "the read declares no external effect"
    );
    ensure!(
        matches!(operation.state, shenko_types::operation::OperationState::Resolved),
        "the operation is resolved with its evidence"
    );

    // Absence is reported, not fabricated: disable the provider and read again.
    sandbox.catalog.set_enabled("awareness.host", false)?;
    let second = admit_notice_task(
        &sandbox.store,
        sandbox.agent,
        descriptor.implementation_ref.clone(),
        8,
    )?;
    drive_until(&native, &sandbox.store, second).await?;
    let absent = sandbox
        .store
        .native_task_result(second)?
        .context("the second read produced a durable T2")?;
    let absent_payload = absent.result.as_ref().context("the second result is kept")?;
    ensure!(
        absent_payload["resident"] == json!(false) && absent_payload["status"] == json!("absent"),
        "an absent capability is reported as absent: {absent_payload}"
    );
    ensure!(
        absent_payload["notices"].as_array().is_some_and(Vec::is_empty),
        "an absent capability returns no material: {absent_payload}"
    );

    native_supervisor.stop().await?;
    let receipt = json!({
        "schema_version": 1,
        "consumer": "awareness-notice-native-read",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "task_id": task,
        "agent_id": sandbox.agent,
        "generation": head.software_generation.0,
        "descriptor": {"kind": descriptor.kind, "role_revision": descriptor.role_revision},
        "resident_result": payload,
        "absent_result": absent_payload,
        "operation_id": operation.operation_id,
        "effect_status": "none",
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_modeled_other_perspective_is_never_offered_as_this_subjects_observation() -> Result<()> {
    let sandbox = Sandbox::open("perspective")?;
    let selection = CapabilitySelection::host("awareness.host");
    let host = sandbox.host(&selection);
    let provider_observation =
        sandbox.observe_locally("awareness.host", b"provider body check", "consumer_check")?;
    sandbox.adopt("awareness.host", &provider_observation, "adopt the host capability")?;

    // A perception unit that teaches reading one experience through another
    // subject's perception.
    let package = parse_cognition_entry(&custom_perception_entry("0.1.0"), Path::new("consumer/MODELED.md"))?;
    sandbox.catalog.install(&package, &sandbox.package_root())?;
    let fact = sandbox.fact(MODELED_UNIT)?;
    let binding = PerceptionBinding::of(&fact);
    let observation_bytes = b"the modeled other subject reported this reading";
    let observation_digest = sandbox.observe_locally(MODELED_UNIT, observation_bytes, "consumer_check")?;
    sandbox.adopt(MODELED_UNIT, &observation_digest, "adopt the perspective-taking perception")?;

    // The source is bound in the modeled perspective and answers with that
    // same attribution: the reading is admitted, as the other subject's view.
    let modeled_fact = PerceptionFact {
        binding: binding.clone(),
        source: "consumer:modeled-source".into(),
        target: format!("subject:{MODELED_SUBJECT}"),
        observed_at: chrono::Utc::now().to_rfc3339(),
        valid_until: None,
        summary: "对方报告看到的连接状态与自己的观察不同".into(),
        evidence_digest: Some(observation_digest.0.clone()),
        perspective: Some(PerceptionPerspective::ModeledOther {
            subject: MODELED_SUBJECT.into(),
        }),
    };
    sandbox.catalog.perception().bind_as(
        binding.clone(),
        PerceptionPerspective::ModeledOther {
            subject: MODELED_SUBJECT.into(),
        },
        Arc::new(Fixed(modeled_fact.clone())),
    )?;
    let status = sandbox.catalog.perception().observe(&binding);
    ensure!(
        matches!(status, shenko_awareness_core::PerceptionStatus::Observed { .. }),
        "the modeled reading is admitted under its own perspective: {}",
        status.line(chrono::Utc::now())
    );

    let notices = host.notices(8)?;
    let modeled = notices
        .iter()
        .find(|notice| notice.atom.unit_id == MODELED_UNIT)
        .context("the adopted perception atom is offered")?;
    ensure!(
        modeled.perspective_subject() == Some(MODELED_SUBJECT),
        "the notice keeps the modeled subject: {modeled:?}"
    );
    ensure!(
        modeled.attribution
            == NoticeAttribution::ModeledOther {
                subject: MODELED_SUBJECT.into()
            },
        "the attribution is modeled, not own: {modeled:?}"
    );
    match &modeled.reason {
        NoticeReason::Observation { observation_digest: digest, observed_at, content_digest, .. } => {
            ensure!(
                digest == &observation_digest.0,
                "the reason stops at the observation this node registered"
            );
            ensure!(
                content_digest == &fact.artifact_digest.0 && observed_at == &modeled_fact.observed_at,
                "the reading names the exact perception revision and observation time"
            );
        }
        other => bail!("a perception notice rests on its observation: {other:?}"),
    }
    let line = modeled.render();
    ensure!(
        line.contains("模拟") && line.contains("不是本主体的观察"),
        "the rendered line states the modeled perspective explicitly: {line}"
    );

    // The same reading may not be filed as this subject's own observation.
    let own = shenko_awareness_core::PerceptionRegistry::new();
    own.bind(
        binding.clone(),
        Arc::new(Fixed(PerceptionFact {
            perspective: Some(PerceptionPerspective::Own),
            ..modeled_fact
        })),
    )?;
    let refused = own.observe(&binding);
    ensure!(
        matches!(refused, shenko_awareness_core::PerceptionStatus::Unavailable { .. }),
        "a modeled reading cannot become own by re-binding: {}",
        refused.line(chrono::Utc::now())
    );

    let receipt = json!({
        "schema_version": 1,
        "consumer": "awareness-host-perspective",
        "node_dir": sandbox.dir.join("node").display().to_string(),
        "binding": binding,
        "observation_digest": observation_digest,
        "notice": modeled,
        "rendered": line,
        "refused_own_rebinding": refused.line(chrono::Utc::now()),
    });
    println!("RECEIPT:{}", serde_json::to_string(&receipt)?);
    let _ = std::fs::remove_dir_all(&sandbox.dir);
    Ok(())
}

async fn drive_until(service: &Arc<NativeChildService>, store: &Arc<Store>, task: EntityId) -> Result<()> {
    let started = Instant::now();
    loop {
        if store.native_task_result(task)?.is_some() {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(60) {
            let current = store.external_task(task)?;
            bail!(
                "the notice read did not settle: state={:?} park={:?}",
                current.state,
                shenko_store::child::park_reason(&current)
            );
        }
        let mut pass = Box::pin(service.run_once());
        match tokio::time::timeout(Duration::from_secs(30), &mut pass).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => bail!("one dispatch pass exceeded 30s"),
        }
    }
}
