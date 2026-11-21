use super::*;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use shenko_store::{SubjectRequestAttempt, SubjectRequestState, SubjectRequestTerminal};
use shenko_types::{
    ids::{RecordRevision, TaskRevision},
    inbox::{InboxEvent, InboxEventKind},
    store::RawInboxEvent,
    subject::{StepDecision, SubjectBranchHead, SubjectModelOutcome, SubjectStep, SubjectStepState},
    task::{AbortScope, DispatchEligibility, TaskOwner, TaskRecord, TaskState},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Result<(Self, Arc<Store>, EntityId, SubjectRequestAttempt)> {
        let path = std::env::temp_dir().join(format!("shenko-authority-{}", EntityId::new()));
        std::fs::create_dir_all(&path)?;
        let fixture = Self(path);
        let store = fixture.open()?;
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: fixture.0.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id())?;
        let agent = crate::runtime_bootstrap::bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env())?;
        let (head, mut step) = store.load_subject(agent)?;
        let plan = crate::recovery_integration_tests::prepare_fixture_s0(&store, &head, &step);
        let old_revision = step.record_revision;
        step.state = SubjectStepState::Preparing;
        step.input_plan_ref = Some(plan.clone());
        step.record_revision = old_revision.next();
        store.upsert_step(&step, &head, old_revision)?;
        let request = SubjectRequestAttempt { schema_version: 1,
            request_id: EntityId::new(), attempt_id: EntityId::new(), agent_id: agent,
            step_id: step.step_id, send_id: None, state: SubjectRequestState::Armed,
            input_plan_ref: plan, payload_digest: Digest::blake3(b"original request"),
            provider_binding_digest: Digest::blake3(b"original provider"), terminal: None,
            created_commit: head.committed_at };
        let old_revision = step.record_revision;
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(request.request_id);
        step.active_attempt_id = Some(request.attempt_id);
        step.record_revision = old_revision.next();
        store.upsert_step_arming_request(&step, &head, old_revision, None, &request)?;
        store.mark_subject_request_dispatched(request.request_id)?;
        Ok((fixture, store, agent, request))
    }
    fn open(&self) -> Result<Arc<Store>> {
        Ok(Arc::new(Store::open(self.0.join("shenko.db"), self.0.join("cas"))?))
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

/// Real read-only wire evidence qualifies the fixture's local binding. The
/// finite HTTP owner is joined before returning; no generation, environment
/// override or synthetic primary/control grant is involved.
fn observe_fixture_model(path: &Path, store: &Store) -> Result<()> {
    let probe = tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let (_broker, handle) = shenko_broker::broker::Broker::with_local_provider(
            shenko_broker::local_provider::LocalModelProvider::new(endpoint, "authority-metadata-fixture".into()), 1)?;
        let server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async move {
                for (route, status, body) in [
                    ("/v1/models", "200 OK", r#"{"data":[{"id":"authority-metadata-fixture","max_model_len":65536}]}"#),
                    ("/metrics", "404 Not Found", "metrics unavailable in this metadata-only fixture"),
                ] {
                    let (mut socket, _) = listener.accept().await?;
                    let mut request = Vec::new();
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let mut chunk = [0_u8; 1024];
                        let count = socket.read(&mut chunk).await?;
                        anyhow::ensure!(count > 0, "metadata request closed before its headers");
                        request.extend_from_slice(&chunk[..count]);
                        anyhow::ensure!(request.len() <= 16 * 1024, "metadata request exceeds fixture bound");
                    }
                    let request = std::str::from_utf8(&request)?;
                    anyhow::ensure!(request.starts_with(&format!("GET {route} HTTP/1.1\r\n")),
                        "fixture received an unexpected request; only metadata GETs are permitted");
                    socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await?;
                }
                Ok::<(), anyhow::Error>(())
            }).await.context("owned metadata fixture exceeded five seconds")?
        });
        let observed = tokio::time::timeout(Duration::from_secs(5), handle.probe_binding()).await;
        // Join on probe failure too; a timeout never leaves a fixture server
        // running behind the authority/crash journey it was meant to prepare.
        let served = server.await.context("owned metadata fixture task failed")?;
        served?;
        observed.context("fixture model probe exceeded five seconds")?
    })?;
    assert_eq!(probe.usability, shenko_broker::BindingUsability::Available);
    let profile = shenko_seed::ModelProfile {
        provider: Some("local".into()), model: Some("authority-metadata-fixture".into()),
        configured: true, capacity: shenko_seed::ModelCapacity::Unprobed,
    };
    let profile_digest = Digest::blake3(&serde_json::to_vec(&profile)?);
    let original = store.cas.seal_object(&serde_json::to_vec(&probe)?, "application/json",
        "authority-fixture", "model-capability-probe", 1)?.object;
    let mut eligibility = shenko_node::ModelEligibility::default();
    eligibility.observe(profile_digest.clone(), shenko_node::ModelUsabilityOutcome::Available,
        shenko_node::ModelUsabilityEvidence {
            evidence_digest: original.digest, source: shenko_node::ModelUsabilitySource::CapabilityProbe,
            observed_at: probe.observed_at,
            valid_until: probe.observed_at + chrono::Duration::minutes(15),
        })?;
    let runtime_id = EntityId::new();
    let candidate = eligibility.candidate(runtime_id, &profile_digest, None)
        .context("real fixture metadata did not establish an eligible binding")?;
    crate::node_sync::open_node(path, store)?.advertise_component_runtime(runtime_id, Some(candidate))?;
    Ok(())
}

fn runtime(path: &Path, store: &Arc<Store>) -> Result<Arc<RuntimeAuthority>> {
    let cancel = CancellationToken::new();
    observe_fixture_model(path, store)?;
    crate::node_sync::establish_primary(path, store, &cancel)?.context("fixture singleton primary")?;
    let runtime = RuntimeAuthority::attach(path, store, cancel)?;
    runtime.recover_before_consumers()?;
    runtime.activate()?;
    Ok(runtime)
}

fn pending(store: &Store, agent: EntityId, owner: Option<EntityId>) -> Result<(TaskRecord, RawInboxEvent)> {
    let (head, _) = store.load_subject(agent)?;
    let mut task = store.get_task(head.subject_root_task_id)?.context("subject root missing")?;
    task.task_id = EntityId::new();
    task.task_revision = TaskRevision(0);
    task.record_revision = RecordRevision(0);
    task.kind = "fixture.native".into();
    task.owner = TaskOwner::Task { id: owner.unwrap_or(head.subject_root_task_id) };
    task.input_ref = store.cas.seal_object(b"{\"input\":\"frozen\"}", "application/json", "fixture", "json", 1)?.object;
    task.state = TaskState::Pending { checkpoint_ref: None };
    task.created_by_step_id = None;
    task.dispatch_eligibility = DispatchEligibility::Ready;
    task.memos.clear();
    task.memos.insert("native_input".into(), "object".into());
    task.memos.insert("intent_id".into(), serde_json::json!(task.task_id));
    let event = InboxEvent { event_id: EntityId::new(), kind: InboxEventKind::SystemEvent,
        source: "authority-fixture".into(), created_at: chrono::Utc::now(),
        payload: serde_json::json!({"task_id":task.task_id}) };
    let raw = RawInboxEvent { event_id: event.event_id, agent_id: agent, source: event.source.clone(),
        kind: "system_event".into(), payload_json: serde_json::to_string(&event)?,
        received_at_ms: event.created_at.timestamp_millis() };
    Ok((task, raw))
}

/// Pauses the real provider after it chooses a value or after it reserves
/// its actual Node connection; it does not replace authority with a mock.
struct PauseAuthority {
    inner: Arc<RuntimeAuthority>,
    before_acquire: bool,
    once: AtomicBool,
    reached: SyncSender<()>,
    release: Mutex<Receiver<()>>,
}
impl PauseAuthority {
    fn pause(&self) -> Result<(), ShenkoError> {
        if self.once.swap(false, Ordering::AcqRel) {
            self.reached.send(()).map_err(|error| ShenkoError::other(error.to_string()))?;
            self.release.lock().recv().map_err(|error| ShenkoError::other(error.to_string()))?;
        }
        Ok(())
    }
}
impl CommitAuthorityProvider for PauseAuthority {
    fn source_id(&self) -> &str { self.inner.source_id() }
    fn prepare(&self, admission: &CommitAdmission) -> Result<Option<CommitAuthorityGrant>, ShenkoError> {
        let grant = self.inner.prepare(admission)?;
        if self.before_acquire && grant.is_some() { self.pause()?; }
        Ok(grant)
    }
    fn acquire(&self, grant: Option<&CommitAuthorityGrant>) -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        let guard = self.inner.acquire(grant)?;
        if !self.before_acquire && grant.is_some() { self.pause()?; }
        Ok(guard)
    }
    fn acquire_installed(&self, grant: Option<&CommitAuthorityGrant>,
        permit: shenko_types::installation::InstallationPermit,
    ) -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        let guard = self.inner.acquire_installed(grant, permit)?;
        if !self.before_acquire && grant.is_some() { self.pause()?; }
        Ok(guard)
    }
    fn action_unmaterialized(&self, grant: &CommitAuthorityGrant) { self.inner.action_unmaterialized(grant); }
}
fn pausing(store: &Store, runtime: Arc<RuntimeAuthority>, before_acquire: bool)
    -> Result<(Receiver<()>, SyncSender<()>)>
{
    let (reached, observed) = mpsc::sync_channel(1);
    let (release, proceed) = mpsc::sync_channel(1);
    store.attach_commit_authority(Arc::new(PauseAuthority { inner: runtime, before_acquire,
        once: AtomicBool::new(true), reached, release: Mutex::new(proceed) }))?;
    Ok((observed, release))
}

/// Deterministic output goes through the real parser and S1 preparation. This
/// supplies a frozen canonical result, without claiming a provider generation.
fn prepared_fixture_step(store: &Arc<Store>, agent: EntityId)
    -> Result<(SubjectBranchHead, SubjectStep, EntityId)>
{
    let (head, mut step) = store.load_subject(agent)?;
    assert_eq!(step.state, SubjectStepState::Planned);
    let raw = store.cas.seal_object(b"continue", "text/plain", "test", "raw", 1)?.object;
    step.input_plan_ref = Some(crate::recovery_integration_tests::prepare_fixture_s0(store, &head, &step));
    let prior = step.record_revision;
    step.state = SubjectStepState::Preparing;
    step.record_revision = prior.next();
    store.upsert_step(&step, &head, prior)?;
    let prior = step.record_revision;
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    step.record_revision = prior.next();
    store.upsert_step(&step, &head, prior)?;
    let prior = step.record_revision;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(raw.clone());
    step.model_outcome_ref = Some(store.cas.seal_object(&serde_json::to_vec(&SubjectModelOutcome {
        schema_version: 2,
        request_id: step.active_request_id.context("fixture request missing")?,
        result_ref: raw.clone(),
        stop_reason: "end_turn".into(),
        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
        usage: None,
    })?, "application/json", "model_outcomes", "SubjectModelOutcome", 2)?.object);
    step.record_revision = prior.next();
    store.upsert_step(&step, &head, prior)?;
    let identity = shenko_identity::IdentityService::new(Arc::new(shenko_identity::DefaultIdentityPolicy));
    let output = shenko_subject::output::parse_output(step.step_id, raw.clone(), "continue", &identity,
        &shenko_channel_napcat::qq_test_registry(), None)?;
    let next = EntityId::new();
    let decision = StepDecision {
        schema_version: 8, step_id: step.step_id, output, identity_intent: None,
        input_plan_ref: step.input_plan_ref.clone().context("fixture S0 missing")?,
        model_result_ref: Some(raw), applied_steering_refs: vec![], event_dispositions: vec![],
        delegated_task_intents: vec![], child_dialogue_intents: vec![], memory_intents: vec![],
        policy_intents: vec![], publication_intents: vec![], pending_obligations: vec![],
        unknown_acknowledgements: vec![], attention_policy: None, evidence_refs: vec![],
        successor_step_id: next,
    };
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision)?,
        "application/json", "test", "StepDecision", 8)?.object);
    step.state = SubjectStepState::Committed;
    step.record_revision = step.record_revision.next();
    Ok((head, step, next))
}

#[test]
fn reserved_store_commit_and_ancestor_cancellation_fence_live_owners() -> Result<()> {
    let (fixture, store, agent, original_request) = Fixture::new()?;
    let old = runtime(&fixture.0, &store)?;
    let bound = store.load_subject(agent)?.0;
    assert_eq!(store.activation_selector(agent)?.context("selector missing")?.active_epoch, bound.controller_epoch.0);
    let (task, event) = pending(&store, agent, None)?;
    let id = task.task_id;
    let implementation = task.implementation_ref.clone();
    let (entered, release) = pausing(&store, old.clone(), false)?;
    let committing = std::thread::spawn({ let store = store.clone(); move || store.admit_native_task(&task, &event) });
    entered.recv_timeout(Duration::from_secs(10))?;
    let (started, ready) = mpsc::sync_channel(1);
    let (changed, observed) = mpsc::sync_channel(1);
    let writer = std::thread::spawn({ let path = fixture.0.join("node"); move || -> Result<()> {
        started.send(())?;
        let mut node = Node::open(&path)?;
        node.campaign()?;
        changed.send(())?;
        Ok(())
    }});
    ready.recv_timeout(Duration::from_secs(5))?;
    assert!(matches!(observed.recv_timeout(Duration::from_millis(100)), Err(mpsc::RecvTimeoutError::Timeout)));
    release.send(())?;
    assert!(committing.join().map_err(|_| anyhow::anyhow!("Store writer panicked"))??.is_none());
    observed.recv_timeout(Duration::from_secs(10))?;
    writer.join().map_err(|_| anyhow::anyhow!("Node writer panicked"))??;
    assert!(store.get_task(id)?.is_some());

    // A real completed binding read holds this Store's connection mutex.
    // The Node has already lost its original primary fence, so cancellation
    // and refusal of a fresh stamp must not depend on releasing that mutex.
    assert!(!old.cancel.is_cancelled());
    let reservation = Node::open(&fixture.0.join("node"))?.reserve_authority()?;
    assert!(reservation.snapshot().fence.is_none());
    assert!(!is_availability(reservation.snapshot()));
    drop(reservation);
    std::thread::scope(|scope| -> Result<()> {
        let (held, observed_held) = mpsc::sync_channel(1);
        let (release_binding, proceed) = mpsc::sync_channel(1);
        let binding_store = store.as_ref();
        let holding = scope.spawn(move || {
            let mut proceed = Some(proceed);
            shenko_store::test_support::with_sqlite_statement_completion(binding_store, move |work| {
                if work.sql == "SELECT data FROM subject_authority_bindings WHERE agent_id=?1"
                    && let Some(proceed) = proceed.take()
                {
                    let _ = held.send(());
                    let _ = proceed.recv();
                }
            }, || binding_store.subject_authority_binding(agent)).0
        });
        let held_result = observed_held.recv_timeout(Duration::from_secs(5));
        let (finished, observed_finished) = mpsc::sync_channel(1);
        let authority = old.as_ref();
        let recovering = scope.spawn(move || {
            let result = authority.acquire(None).map(|guard| guard.stamp().is_none());
            let _ = finished.send(result);
        });
        let acquired = observed_finished.recv_timeout(Duration::from_secs(3));
        let cancelled_while_held = old.cancel.is_cancelled();
        // Release and join both owners before propagating any failed
        // observation. Scope unwinding also drops this sender before joining.
        drop(release_binding);
        let binding = holding.join();
        let recovered = recovering.join();
        held_result.context("real binding lookup did not hold the Store connection")?;
        assert!(binding.map_err(|_| anyhow::anyhow!("binding reader panicked"))??.is_some());
        recovered.map_err(|_| anyhow::anyhow!("authority acquisition panicked"))?;
        assert!(acquired.context("known Node loss waited for the Store connection")??,
            "known Node loss retained a fresh authority stamp");
        assert!(cancelled_while_held, "known Node loss did not cancel while Store was held");
        Ok(())
    })?;

    assert!(store.claim_native_task(id, implementation, false).is_err());
    assert!(old.recovery_required().is_some());
    old.request_shutdown();
    assert!(!old.shutdown_requested.load(Ordering::Acquire),
        "a later user stop cannot relabel an already-cancelled authority failure");
    assert!(!old.admission_cancelled().is_runtime_admission_closed_for(store.capabilities().incarnation));

    // A new incarnation binds a coherent head/step/selector epoch. The old
    // incarnation cannot take the writer back when the source is current again.
    let successor = fixture.open()?;
    let current = runtime(&fixture.0, &successor)?;
    let (head, step) = successor.load_subject(agent)?;
    assert!(head.controller_epoch.0 > bound.controller_epoch.0);
    assert_eq!(head.controller_epoch, step.controller_epoch);
    assert_eq!(successor.activation_selector(agent)?.context("selector missing")?.active_epoch, head.controller_epoch.0);
    assert!(store.activate_runtime_authority(&head).is_err());
    let (parent, event) = pending(&successor, agent, None)?;
    successor.admit_native_task(&parent, &event)?;
    let (child, event) = pending(&successor, agent, Some(parent.task_id))?;
    successor.admit_native_task(&child, &event)?;
    let child_id = child.task_id;
    let (chosen, release) = pausing(&successor, current.clone(), true)?;
    let claiming = std::thread::spawn({ let store = successor.clone(); move ||
        store.claim_native_task(child.task_id, child.implementation_ref, false) });
    chosen.recv_timeout(Duration::from_secs(10))?;
    let cancelled = successor.request_native_cancellation(parent.task_id, EntityId::new(), EntityId::new(),
        "original owner stopped this task", AbortScope::Task)?;
    assert_eq!(cancelled.cancelled, vec![parent.task_id]);
    release.send(())?;
    assert!(claiming.join().map_err(|_| anyhow::anyhow!("child claimant panicked"))?.is_err());
    assert!(matches!(successor.get_task(child_id)?.context("child missing")?.state, TaskState::Pending { .. }));
    assert!(current.recovery_required().is_some());

    // Original E1 ingress has no new-control fence, even after both runtimes
    // lost authority. Reopen without an adapter still refuses new control.
    let terminal = SubjectRequestTerminal { outcome: serde_json::json!({"kind":"unknown","reason":"lost original response"}),
        result_ref: None, recorded_commit: original_request.created_commit };
    store.record_subject_request_terminal(original_request.request_id, &terminal)?;
    let reopened = fixture.open()?;
    assert!(reopened.start_subject_dispatcher().is_err());
    let retained = reopened.subject_request_attempt(original_request.request_id)?.context("original request missing")?;
    assert_eq!(retained.attempt_id, original_request.attempt_id);
    assert_eq!(retained.state, SubjectRequestState::Terminal);

    // A new actual runtime may close its own admission without claiming it
    // lost the Node authority. No new grant, task, head or original receipt
    // may be manufactured by that ordinary stop.
    let closing_store = fixture.open()?;
    let closing = runtime(&fixture.0, &closing_store)?;
    let before = serde_json::to_value(closing_store.load_subject(agent)?)?;
    let original = serde_json::to_value(closing_store.subject_request_attempt(original_request.request_id)?)?;
    let grants_before = crate::node_sync::open_node(&fixture.0, &closing_store)?.committed_admissions(0)?.len();
    let (unadmitted, event) = pending(&closing_store, agent, None)?;
    closing.request_shutdown();
    let refusal = closing_store.admit_native_task(&unadmitted, &event).err()
        .context("shutdown admitted a new task")?;
    assert!(refusal.downcast_ref::<ShenkoError>().is_some_and(|error|
        error.is_runtime_admission_closed_for(closing_store.capabilities().incarnation)));
    let refusal = closing.acquire(None).err().context("shutdown retained fresh admission")?;
    assert!(refusal.is_runtime_admission_closed_for(closing_store.capabilities().incarnation));
    assert!(closing_store.ledger.writer_is_open(), "ordinary admission closure is not storage damage");
    assert!(closing_store.get_task(unadmitted.task_id)?.is_none());
    assert_eq!(serde_json::to_value(closing_store.load_subject(agent)?)?, before);
    assert_eq!(serde_json::to_value(closing_store.subject_request_attempt(original_request.request_id)?)?, original);
    assert_eq!(crate::node_sync::open_node(&fixture.0, &closing_store)?.committed_admissions(0)?.len(), grants_before);
    assert!(closing.finish_result(Ok(())).is_ok());
    let storage = closing.finish_result(Err(ShenkoError::storage("retained shutdown storage failure").into()))
        .err().context("shutdown hid a storage failure")?;
    assert!(matches!(storage.downcast_ref::<ShenkoError>(), Some(ShenkoError::Storage(_))));

    // A real subsequent Node change still wins over the ordinary-stop flag.
    crate::node_sync::open_node(&fixture.0, &closing_store)?.campaign()?;
    let revoked = closing.acquire(None)?;
    assert!(revoked.stamp().is_none());
    drop(revoked);
    assert!(closing.recovery_required().is_some());
    assert!(!closing.admission_cancelled().is_runtime_admission_closed_for(closing_store.capabilities().incarnation));
    assert!(closing.finish_result(Ok(())).is_err());
    Ok(())
}

struct ExitAfterChosen { inner: Arc<RuntimeAuthority> }
impl CommitAuthorityProvider for ExitAfterChosen {
    fn source_id(&self) -> &str { self.inner.source_id() }
    fn prepare(&self, admission: &CommitAdmission) -> Result<Option<CommitAuthorityGrant>, ShenkoError> {
        let grant = self.inner.prepare(admission)?;
        if grant.is_some() { std::process::exit(71); }
        Ok(grant)
    }
    fn acquire(&self, grant: Option<&CommitAuthorityGrant>) -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        self.inner.acquire(grant)
    }
    fn acquire_installed(&self, grant: Option<&CommitAuthorityGrant>,
        permit: shenko_types::installation::InstallationPermit,
    ) -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        self.inner.acquire_installed(grant, permit)
    }
}

#[test]
fn chosen_unmaterialized_action_reopens_on_a_real_branch_with_original_unknown() -> Result<()> {
    const HELPER: &str = "SHENKO_AUTHORITY_CRASH_HELPER";
    if let Some(path) = std::env::var_os(HELPER) {
        let path = PathBuf::from(path);
        let store = Arc::new(Store::open(path.join("shenko.db"), path.join("cas"))?);
        let runtime = runtime(&path, &store)?;
        store.attach_commit_authority(Arc::new(ExitAfterChosen { inner: runtime }))?;
        let task_id: EntityId = std::fs::read_to_string(path.join("task-id"))?.parse()?;
        let task = store.get_task(task_id)?.context("crash task missing")?;
        store.claim_native_task(task_id, task.implementation_ref, false)?;
        anyhow::bail!("fault injection never reached a durable chosen action");
    }
    let (fixture, store, agent, original_request) = Fixture::new()?;
    let (task, event) = pending(&store, agent, None)?;
    store.admit_native_task(&task, &event)?;
    std::fs::write(fixture.0.join("task-id"), task.task_id.to_string())?;
    let parent = store.load_subject(agent)?.0;
    drop(store);
    let status = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "runtime_authority::tests::chosen_unmaterialized_action_reopens_on_a_real_branch_with_original_unknown", "--nocapture"])
        .env(HELPER, &fixture.0).status()?;
    assert_eq!(status.code(), Some(71));
    let store = fixture.open()?;
    let cancel = CancellationToken::new();
    observe_fixture_model(&fixture.0, &store)?;
    crate::node_sync::establish_primary(&fixture.0, &store, &cancel)?.context("selected primary")?;
    let runtime = RuntimeAuthority::attach(&fixture.0, &store, cancel)?;
    assert!(runtime.recover_before_consumers()?);
    runtime.activate()?;
    let (head, step) = store.load_subject(agent)?;
    assert_ne!(head.branch_id, parent.branch_id);
    assert_ne!(head.subject_root_task_id, parent.subject_root_task_id);
    assert!(head.branch_origin.is_some());
    assert_eq!(step.state, SubjectStepState::Planned);
    assert!(step.active_request_id.is_none() && step.active_attempt_id.is_none());
    let node = crate::node_sync::open_node(&fixture.0, &store)?;
    assert_eq!(node.local_identity()?.context("node identity missing")?.branch_id.0, head.branch_id.0);
    let debts = store.pending_control_reconciliations(agent)?;
    assert_eq!(debts.len(), 1);
    assert_eq!(debts[0].payload["reason"], "selected-plan recovery unavailable");
    assert_eq!(debts[0].payload["original_branch"], serde_json::to_value(&parent.branch_id)?);
    let grant: CommitAuthorityGrant = serde_json::from_value(debts[0].payload["grant"].clone())?;
    assert!(grant.replayed);
    assert!(!store.control_admission_applied_here(&grant.admission)?);
    assert!(!node.admission_objects(&grant.admission.payload_ref)?.is_empty());
    assert!(store.pending_inbox_events(agent, 100)?.iter().any(|(id, _)| *id == debts[0].event_id));
    assert!(matches!(store.get_task(task.task_id)?.context("parent task missing")?.state, TaskState::Pending { .. }));
    assert!(!store.native_task_candidates_after(None, 100)?.iter().any(|(_, candidate)| candidate.task_id == task.task_id));
    let original = store.subject_request_attempt(original_request.request_id)?.context("parent request missing")?;
    assert_eq!(original.attempt_id, original_request.attempt_id);
    assert_eq!(original.state, SubjectRequestState::Dispatched);

    // Reconstruct the same production SubjectLoopState and a new actual
    // branch-scoped context registry. No model call is fabricated here.
    let context = cordis_core::Context::new();
    let _identity = context.provide(Arc::new(shenko_identity::IdentityService::new(
        Arc::new(shenko_identity::DefaultIdentityPolicy))))?;
    let _registry = context.provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
        shenko_context_policy::WindowLedger::new(Arc::new(shenko_context_policy::StoreWindowLog::new(
            store.clone(), EntityId(head.branch_id.0)))))))?;
    let rebuilt = shenko_subject::loop_runner::SubjectLoopState::new(agent,
        Arc::new(shenko_subject::inbox::Inbox::new(8)), store.clone(), context,
        shenko_types::channel::ChannelRegistry::new())?;
    assert_eq!(rebuilt.head.branch_id, head.branch_id);
    assert_eq!(rebuilt.step.step_id, step.step_id);
    store.record_subject_request_terminal(original_request.request_id, &SubjectRequestTerminal {
        outcome: serde_json::json!({"kind":"unknown","reason":"retained source transport"}), result_ref: None,
        recorded_commit: original.created_commit,
    })?;
    assert_eq!(store.load_subject(agent)?.1.active_attempt_id, None);
    assert_eq!(store.pending_control_reconciliations(agent)?.len(), 1);
    let (new_task, event) = pending(&store, agent, None)?;
    store.admit_native_task(&new_task, &event)?;
    assert!(store.claim_native_task(new_task.task_id, new_task.implementation_ref, false)?.is_some());
    assert!(!runtime.recover_before_consumers()?);
    assert_eq!(store.load_subject(agent)?.0.branch_id, head.branch_id);

    // Learning another term ends this branch's opening authority. A new
    // singleton election can select the same actual branch, but fresh work
    // must then enter the quorum log instead of reusing its historical mode.
    let mut node = crate::node_sync::open_node(&fixture.0, &store)?;
    let admissions_before = node.committed_admissions(0)?.len();
    node.campaign()?;
    let revoked = runtime.acquire(None)?;
    assert!(revoked.stamp().is_none());
    drop(revoked);
    assert!(runtime.recovery_required().is_some());
    let successor = fixture.open()?;
    let global = self::runtime(&fixture.0, &successor)?;
    let selected = crate::node_sync::open_node(&fixture.0, &successor)?.reserve_authority()?;
    assert!(!selected.snapshot().is_current_availability());
    assert_eq!(selected.snapshot().identity.branch_id.0, head.branch_id.0);
    drop(selected);
    assert_eq!(successor.load_subject(agent)?.0.branch_id, head.branch_id);
    let (connected_task, event) = pending(&successor, agent, None)?;
    successor.admit_native_task(&connected_task, &event)?;
    let admissions = crate::node_sync::open_node(&fixture.0, &successor)?.committed_admissions(0)?;
    assert_eq!(admissions.len(), admissions_before + 1);
    let connected_grant = admissions.iter().find(|grant|
        grant.admission.conflict_key == format!("task/{}/create", connected_task.task_id))
        .context("connected admission missing from its actual quorum log")?;
    assert_eq!(connected_grant.admission.branch_id, head.branch_id);
    assert!(successor.control_admission_applied_here(&connected_grant.admission)?);

    // Lose model eligibility after a real quorum chose S1 but before its
    // Store materialization. Recovery consumes only those original bytes;
    // missing model configuration cannot convert a learned result into debt.
    let (expected, committed, next) = prepared_fixture_step(&successor, agent)?;
    let chosen_step = committed.step_id;
    let (chosen, release) = pausing(&successor, global.clone(), true)?;
    let committing = std::thread::spawn({
        let store = successor.clone();
        let expected = expected.clone();
        move || store.commit_step_and_advance_head(&committed, &expected, next)
    });
    chosen.recv_timeout(Duration::from_secs(10))?;
    let node = crate::node_sync::open_node(&fixture.0, &successor)?;
    let grants = node.committed_admissions(0)?;
    let grant = grants.last().context("chosen canonical S1 missing")?.clone();
    let objects = node.admission_objects(&grant.admission.payload_ref)?;
    let envelope: serde_json::Value = serde_json::from_slice(
        &successor.cas.read_object(&grant.admission.payload_ref.digest)?)?;
    assert_eq!(envelope["action"]["kind"], "subject.step_commit");
    assert_eq!(envelope["action"]["step"]["step_id"], serde_json::to_value(chosen_step)?);
    assert!(envelope["action"].get("transition_ref").is_some());
    assert!(!successor.control_admission_applied_here(&grant.admission)?);
    let health = tokio::runtime::Builder::new_current_thread().build()?.block_on(
        crate::runtime_model_health::initial_probe(&shenko_seed::ModelProfile {
            provider: None, model: None, configured: false, capacity: shenko_seed::ModelCapacity::Blocked,
        }, successor.clone(), &CancellationToken::new()))?;
    let runtime_id = EntityId::new();
    assert!(!health.is_eligible(runtime_id));
    global.bind_model_health(health, runtime_id);
    release.send(())?;
    let error = committing.join().map_err(|_| anyhow::anyhow!("canonical writer panicked"))?
        .err().context("fresh S1 unexpectedly applied without model eligibility")?;
    assert!(shenko_types::authority::is_authority_fenced(&error));
    assert_eq!(successor.load_subject(agent)?.0.next_step_id, expected.next_step_id);
    assert!(!successor.control_admission_applied_here(&grant.admission)?);
    assert!(global.prepare(&grant.admission).is_err());
    assert!(global.acquire(None).is_err());

    // A replay request still reserves the real Node and verifies the exact
    // stored descriptor. Neither a forged certificate identity nor another
    // branch gains a materialization guard merely by setting replayed=true.
    let mut replay = grant.clone();
    replay.replayed = true;
    let mut forged = replay.clone();
    forged.grant_id.push_str("-forged");
    let error = global.acquire(Some(&forged)).err().context("forged grant accepted")?;
    assert!(!shenko_types::authority::is_authority_fenced(&error));
    assert!(successor.recover_control_admission(&forged, &objects, false).is_err());
    assert!(!successor.control_admission_applied_here(&grant.admission)?);
    let mut foreign = replay.clone();
    foreign.admission.branch_id = BranchId(EntityId::new().0);
    let error = global.acquire(Some(&foreign)).err().context("foreign branch accepted")?;
    assert!(!shenko_types::authority::is_authority_fenced(&error));
    let reservation = global.acquire(Some(&replay))?;
    assert!(reservation.stamp().is_none());
    assert!(reservation.installation_permit().is_some());
    drop(reservation);
    assert_eq!(successor.recover_control_admission(&replay, &objects, false)?,
        shenko_store::ControlRecoveryOutcome::RecoveredCanonical);
    assert_eq!(successor.load_subject(agent)?.0.next_step_id, next);
    assert!(successor.control_admission_applied_here(&replay.admission)?);
    assert_eq!(successor.recover_control_admission(&replay, &objects, false)?,
        shenko_store::ControlRecoveryOutcome::Applied);
    assert!(successor.start_subject_dispatcher().is_err());
    assert_eq!(crate::node_sync::open_node(&fixture.0, &successor)?.committed_admissions(0)?.len(), grants.len());
    Ok(())
}
