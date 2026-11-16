use super::*;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;
use shenko_store::{SubjectRequestAttempt, SubjectRequestState, SubjectRequestTerminal};
use shenko_types::{
    ids::{RecordRevision, TaskRevision},
    inbox::{InboxEvent, InboxEventKind},
    store::RawInboxEvent,
    subject::SubjectStepState,
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

fn runtime(path: &Path, store: &Arc<Store>) -> Result<Arc<RuntimeAuthority>> {
    let cancel = CancellationToken::new();
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
    assert!(store.claim_native_task(id, implementation, false).is_err());
    assert!(old.recovery_required().is_some());

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
    let _global = self::runtime(&fixture.0, &successor)?;
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
    Ok(())
}
