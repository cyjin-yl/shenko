//! Root native consumer of an installed plugin generation.
//!
//! One real composition root (`Supervisor` + `PluginRuntime` +
//! `NativeChildService` + the installation observer) consumes a code plugin
//! unit installed through the real Node installation authority. The fixture
//! plugin below is compiled into this test binary, so its exact role binding is
//! a *compiled* one; what the unit record decides is whether this root prepares,
//! publishes and attaches that definition at all.
//!
//! The single case walks the same truth the composition root relies on:
//!
//! * install  — the code unit is imported and enabled in the real Node.
//! * prepare  — Cordis `Plugin::prepare` runs for the root consumer and for the
//!              generation it is about to publish.
//! * publish  — the attached generation registers its provider, capability and
//!              help in the live `PluginRuntime`.
//! * attach   — the definition is registered in `NativeChildService`.
//! * call     — a real recorded native operation appends one owned host line,
//!              arms T1 and commits its E1 receipt through the real Store/CAS.
//! * retire   — disabling the unit withdraws the definition and the live
//!              registrations, and the generation's own runtime waits for the
//!              workers it already owns.
//! * old call — a call dispatched before retirement still completes under its
//!              original implementation, while a fresh call is refused.
//! * receipt  — the operation, attempts, evidence and the retained executable
//!              object stay readable after retirement and after the restart.
//! * restart  — a second root is created while the first root's generation is
//!              still inside `apply` (mid-attach). The restarted root adopts the
//!              committed unit record and attaches exactly once; the pending
//!              attach completes in its own root afterwards and is retired there.
//!
//! Everything here is source-only until the parent runs it; this file makes no
//! model, product or CI claim.
#![cfg(target_os = "linux")]

use std::borrow::Cow;
use std::convert::Infallible;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use cordis_core::{Context, InjectSpec, Plugin};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::installation::{plugin_export, plugin_package};
use shenko_compat::recorded::{RecordedCall, Workers, joined};
use shenko_compat::runtime::{AdapterHost, watch_installed};
use shenko_kernel::{Service, Supervisor};
use shenko_kernel::task::{
    ChildToolCall, NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation,
};
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_store::child::NATIVE_IMPLEMENTATION_ARTIFACT;
use shenko_subject::{NativeChildPlugin, NativeChildService, NativeTaskHandlerPlugin};
use shenko_types::commit::CommitRef;
use shenko_types::effect::*;
use shenko_types::evidence::EvidenceReceipt;
use shenko_types::ids::*;
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::operation::{
    AuthorityRef, EffectStatus, OperationOutcome, OperationRecord, OperationState,
};
use shenko_types::plugin_runtime::NativeCapabilityDeclaration;
use shenko_types::subject::*;
use shenko_types::task::*;
use tokio::sync::Notify;

/// A code plugin unit that no built-in adapter owns: the consumer is this test.
const FIXTURE_UNIT: &str = "compat.fixture.root-consumer";
/// The exclusive native definition the fixture plugin publishes while installed.
const FIXTURE_KIND: &str = "fixture.root.call";
const FIXTURE_VERSION: u32 = 1;
/// Fixed plugin identity of the fixture implementation.
const FIXTURE_PLUGIN: &str = "9f1d6d3a-5d63-4c2e-9b7a-2a6e0c4b7f11";

// --------------------------------------------------------------- subject ---

fn initialize(store: &Store) -> Result<(EntityId, AuthorityRef)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let zero = CommitRef::new(ledger_id, LedgerSeq::ZERO);
    let object = store
        .cas
        .seal_object(
            b"synthetic plugin-root native consumer fixture",
            "text/plain",
            "fixture",
            "raw",
            1,
        )?
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
        created_commit: zero,
    };
    let head = SubjectBranchHead {
        branch_origin: None,
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("plugin-root-fixture".into()),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
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
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, zero);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    store.start_subject_dispatcher()?;
    Ok((
        root.task_id,
        AuthorityRef {
            delegation_id: root.task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: genesis_id,
        },
    ))
}

// -------------------------------------------------------------- fixtures ---

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureInput {
    /// `record` appends one line and settles; `hold` appends, then waits for the
    /// test's release so the call is genuinely in flight across retirement.
    mode: String,
}

fn descriptor() -> Result<NativeTaskDescriptor> {
    Ok(NativeTaskDescriptor {
        kind: FIXTURE_KIND.into(),
        definition_version: FIXTURE_VERSION,
        implementation_ref: ImplementationRef {
            plugin_id: FIXTURE_PLUGIN.parse().expect("fixed fixture plugin identity"),
            artifact_digest: shenko_kernel::native_artifact::native_artifact_digest()?,
            registry_revision: 0,
        },
        role_revision: "native-child/1".into(),
        authority_refs: vec!["fixture.root/v1".into()],
        prompt_section: Some(
            "Fixture native definition owned by the plugin-root consumer regression: one recorded \
             local host-file effect, plus an explicit hold mode for an in-flight call."
                .into(),
        ),
    })
}

#[derive(Default)]
struct Counters {
    /// The `hold` handler entered its wait.
    hold_entered: AtomicBool,
    /// Real dispatched calls (one effect line each).
    calls: AtomicUsize,
    /// Generation runtimes whose owned workers were retired.
    retired: AtomicUsize,
}

/// One generation's runtime: the worker owner and the real effect log.
struct FixtureRuntime {
    store: Arc<Store>,
    workers: Workers,
    effect_log: PathBuf,
    hold_release: Arc<Notify>,
    counters: Arc<Counters>,
}

impl FixtureRuntime {
    async fn retire(&self) {
        // Waits for the workers this generation already owns; that is the
        // barrier the retirement contract waits for, not a guessed end.
        self.workers.close().await;
        self.counters.retired.fetch_add(1, Ordering::SeqCst);
    }

    async fn run(self: Arc<Self>, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: FixtureInput = serde_json::from_value(invocation.input.clone())?;
        ensure!(
            input.mode == "record" || input.mode == "hold",
            "unknown fixture mode {}",
            input.mode
        );
        let descriptor = descriptor()?;
        let mut call = RecordedCall::prepare(
            &self.store,
            &invocation,
            &descriptor,
            &input,
            json!({"fixture": FIXTURE_UNIT, "implementation": FIXTURE_PLUGIN}),
            "fixture observed exactly one appended host log line; no remote effect exists",
            "an armed fixture request is never automatically repeated",
        )?;
        if let Some(existing) = call.arm(&self.store, &invocation)? {
            return Ok(existing);
        }
        {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.effect_log)?;
            writeln!(
                file,
                "{}",
                json!({
                    "task_id": invocation.task.task_id,
                    "operation_id": invocation.operation_id,
                    "attempt_id": call.attempt.attempt_id,
                    "mode": input.mode,
                })
            )?;
            file.flush()?;
            file.sync_data()?;
        }
        self.counters.calls.fetch_add(1, Ordering::SeqCst);
        if input.mode == "hold" {
            self.counters.hold_entered.store(true, Ordering::SeqCst);
            self.hold_release.notified().await;
        }
        call.observe(
            &self.store,
            json!({"status": "fixture_effect_recorded", "mode": input.mode}),
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
            None,
        )
    }
}

struct FixtureHandler(Arc<FixtureRuntime>);

#[async_trait::async_trait]
impl NativeTaskHandler for FixtureHandler {
    fn joins_execution_on_cancel(&self) -> bool {
        // The execution owns a real worker; returning before it joins would
        // claim a stop that did not happen.
        true
    }

    fn descriptor(&self) -> Result<NativeTaskDescriptor> {
        descriptor()
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: FIXTURE_KIND.into(),
            version: FIXTURE_VERSION.to_string(),
            description:
                "Fixture native definition owned by one installed plugin generation.".into(),
            usage: Some(
                "delegate:fixture.root.call:1 {\"mode\":\"record\"|\"hold\"}".into(),
            ),
        }]
    }

    fn effect_contracts(&self) -> Result<Vec<EffectContract>> {
        let descriptor = descriptor()?;
        Ok(vec![
            EffectContractBuilder::new(
                descriptor.implementation_ref.plugin_id,
                descriptor.implementation(),
                FIXTURE_KIND,
                FIXTURE_VERSION,
            )
            .resource("fixture_effect_log", "fixture_root_log", ResourceAccess::Write)
            .action(EffectActionKind::Cancel)
            .idempotency(IdempotencyGuarantee::BestEffort)
            .external_effect(ExternalEffect {
                kind: "host_file_append".into(),
                description:
                    "Appends one line to the fixture's owned log; the armed receipt keeps the \
                     original attempt addressable."
                        .into(),
                reversible: false,
                compensation_possible: false,
            })
            .cancel(CancelContract {
                can_cancel_before_start: true,
                can_cancel_in_flight: true,
                cancel_confirms_stopped: false,
                may_have_partial_effect: true,
            })
            .checkpoint(CheckpointContract {
                capturable: vec!["frozen fixture input".into()],
                excluded: vec!["the appended host line".into()],
                coordinated: false,
                consistency: "local ledger only".into(),
            })
            .lifecycle(LifecycleContract {
                on_upgrade: "Retire the replaced generation; an in-flight call keeps its original \
                             implementation and its receipt."
                    .into(),
                on_uninstall: "Withdraw the definition and its live registrations; already armed \
                               calls complete and their receipts stay readable."
                    .into(),
                on_crash: "Reconcile the original attempt; unknown is never replayed.".into(),
                receipt_readable_after_removal: true,
            })
            .basis(GuaranteeBasis::ClaimOnly)
            .build(),
        ])
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let runtime = Arc::clone(&self.0);
        let worker = runtime.workers.spawn(Arc::clone(&runtime).run(invocation))?;
        joined(worker).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        shenko_compat::recorded::reconcile_original(&self.0.store, task, operation_id, FIXTURE_KIND)
    }
}

/// One generation attach path. The gate models an attach that has been entered
/// (the runtime exists and its cleanup is armed) but has not published the
/// native definition yet.
#[derive(Clone)]
struct FixtureGeneration {
    gate: Arc<AttachGate>,
    counters: Arc<Counters>,
    prepares: Arc<AtomicUsize>,
    attaches: Arc<AtomicUsize>,
    effect_log: PathBuf,
    hold_release: Arc<Notify>,
}

impl Plugin for FixtureGeneration {
    type Config = (Arc<Store>, Arc<Mutex<Node>>);
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("plugin-root-generation")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }

    fn prepare(&self, config: Self::Input) -> Result<Self::Input, Infallible> {
        self.prepares.fetch_add(1, Ordering::SeqCst);
        Ok(config)
    }

    async fn apply(&self, ctx: Context, config: &Self::Input) -> Result<(), std::io::Error> {
        let runtime = Arc::new(FixtureRuntime {
            store: Arc::clone(&config.0),
            workers: Workers::default(),
            effect_log: self.effect_log.clone(),
            hold_release: Arc::clone(&self.hold_release),
            counters: Arc::clone(&self.counters),
        });
        let cleanup = Arc::clone(&runtime);
        // Registered before the attach gate: if this generation is rolled back
        // mid-attach, its own runtime must still be retired.
        ctx.effect(move || async move {
            cleanup.retire().await;
            Ok::<(), std::io::Error>(())
        })
        .map_err(std::io::Error::other)?;
        self.gate.enter_and_wait().await;
        let handler: Arc<dyn NativeTaskHandler> = Arc::new(FixtureHandler(runtime));
        NativeTaskHandlerPlugin.apply(ctx.clone(), &handler).await?;
        self.attaches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// A root consumer of the installed plugin unit, using the one production
/// watcher (`watch_installed`) instead of a second installation convention.
#[derive(Clone)]
struct FixturePlugin {
    gate: Arc<AttachGate>,
    counters: Arc<Counters>,
    consumer_prepares: Arc<AtomicUsize>,
    generation_prepares: Arc<AtomicUsize>,
    attaches: Arc<AtomicUsize>,
    effect_log: PathBuf,
    hold_release: Arc<Notify>,
}

impl Plugin for FixturePlugin {
    type Config = AdapterHost;
    type Input = Self::Config;
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed("plugin-root-consumer")
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(NativeChildService::NAME)
    }

    fn prepare(&self, config: Self::Config) -> Result<Self::Input, Infallible> {
        self.consumer_prepares.fetch_add(1, Ordering::SeqCst);
        Ok(config)
    }

    async fn apply(&self, ctx: Context, host: &Self::Input) -> Result<(), std::io::Error> {
        let generation = FixtureGeneration {
            gate: Arc::clone(&self.gate),
            counters: Arc::clone(&self.counters),
            prepares: Arc::clone(&self.generation_prepares),
            attaches: Arc::clone(&self.attaches),
            effect_log: self.effect_log.clone(),
            hold_release: Arc::clone(&self.hold_release),
        };
        watch_installed(
            ctx,
            host.clone(),
            FIXTURE_UNIT,
            generation,
            |_ctx: Context| async move { anyhow::Ok(()) },
        )
        .await
        .map_err(std::io::Error::other)
    }
}

/// The observable mid-attach seam. `arm` is only ever called while no attach is
/// pending; `enter_and_wait` records that the generation reached the seam and
/// waits for the test to release it.
struct AttachGate {
    armed: AtomicBool,
    entered: AtomicBool,
    release: Notify,
}

impl AttachGate {
    fn open() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            entered: AtomicBool::new(false),
            release: Notify::new(),
        })
    }

    fn arm(&self) {
        self.entered.store(false, Ordering::SeqCst);
        self.armed.store(true, Ordering::SeqCst);
    }

    fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    fn release(&self) {
        // A stored permit also covers the release raced by the waiter.
        self.release.notify_one();
    }

    async fn enter_and_wait(&self) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        self.entered.store(true, Ordering::SeqCst);
        self.release.notified().await;
    }
}

// ---------------------------------------------------------------- helpers ---

fn fixture_call(mode: &str) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall {
        call_id: task_id.to_string(),
        task_id,
        kind: FIXTURE_KIND.into(),
        definition_version: FIXTURE_VERSION,
        input: json!({"mode": mode}),
    }
}

fn effects(path: &Path) -> Result<Vec<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

async fn wait_flag(what: &str, seconds: u64, mut ready: impl FnMut() -> bool) -> Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    while !ready() {
        ensure!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(())
}

fn provider_id() -> String {
    format!("native.{FIXTURE_KIND}")
}

/// Every retained identity one real call owns. Re-reading this after retirement
/// and after the restart is the receipt/GC-retention assertion: the durable
/// roots the GC contract must keep are still addressable, and nothing was
/// rewritten or replayed.
fn assert_retained(
    store: &Store,
    operation: &OperationRecord,
    receipts: &[EvidenceReceipt],
    artifact: &ObjectRef,
) -> Result<()> {
    let current = store.external_operation(operation.operation_id)?;
    ensure!(
        current.operation_id == operation.operation_id
            && current.record_revision == operation.record_revision
            && current.task_id == operation.task_id
            && current.final_input_ref == operation.final_input_ref
            && current.state == operation.state
            && current.outcome == operation.outcome
            && current.effect_status == operation.effect_status
            && current.evidence_through == operation.evidence_through,
        "retained operation was rewritten after retirement: {current:?} != {operation:?}"
    );
    let attempts = store.external_operation_attempts(operation.operation_id)?;
    ensure!(
        attempts.len() == 1,
        "retirement or restart created a second attempt: {attempts:?}"
    );
    let current_receipts = store.external_operation_evidence(operation.operation_id)?;
    ensure!(
        current_receipts.len() == receipts.len()
            && current_receipts
                .iter()
                .zip(receipts)
                .all(|(current, original)| current.receipt_id == original.receipt_id),
        "retained evidence identity changed: {current_receipts:?}"
    );
    let frozen = store.cas.read_object(&operation.final_input_ref.digest)?;
    ensure!(
        frozen.len() as u64 == operation.final_input_ref.bytes
            && Digest::blake3(&frozen) == operation.final_input_ref.digest,
        "the frozen input object is no longer readable"
    );
    for receipt in receipts {
        let raw = store.cas.read_object(&receipt.raw_ref.digest)?;
        ensure!(
            raw.len() as u64 == receipt.raw_ref.bytes
                && Digest::blake3(&raw) == receipt.raw_ref.digest,
            "the receipt raw object is no longer readable"
        );
    }
    ensure!(
        store.cas.object_size(&artifact.digest) == Some(artifact.bytes),
        "the retained executable object is no longer readable"
    );
    let bytes = store.cas.read_object(&artifact.digest)?;
    ensure!(
        bytes.len() as u64 == artifact.bytes && Digest::blake3(&bytes) == artifact.digest,
        "the retained executable object changed"
    );
    Ok(())
}

/// The task's claim froze the exact executable object it was claimed against.
fn claim_artifact(store: &Store, task_id: EntityId) -> Result<ObjectRef> {
    let task = store.external_task(task_id)?;
    let value = task
        .memos
        .get(NATIVE_IMPLEMENTATION_ARTIFACT)
        .context("the native claim did not retain the executing implementation artifact")?;
    Ok(serde_json::from_value(value.clone())?)
}

fn call_record(
    store: &Store,
    operation: &OperationRecord,
) -> Result<(Vec<EvidenceReceipt>, ObjectRef)> {
    let receipts = store.external_operation_evidence(operation.operation_id)?;
    ensure!(
        receipts.len() == 1,
        "one fixture call produced {} receipts",
        receipts.len()
    );
    let artifact = claim_artifact(store, operation.task_id)?;
    Ok((receipts, artifact))
}

// ------------------------------------------------------------------ case ---

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_plugin_generation_publishes_attaches_retires_and_restarts_mid_attach() -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures")
        });
    let root = base.join(format!("plugin-root-native-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;

    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let effect_log = root.join("fixture-effects.jsonl");

    // ------------------------------------------------------------ install --
    let package_root = root.join("units").display().to_string();
    node.lock().handle_unit_request(UnitRequest::Import {
        export: plugin_export(FIXTURE_UNIT, true)?,
        package_root: package_root.clone(),
        enable: Some(true),
    })?;
    let expected = plugin_package(FIXTURE_UNIT)?.unit;
    let installed = node
        .lock()
        .installed_unit_fact(FIXTURE_UNIT)?
        .context("the fixture plugin unit is not installed")?;
    ensure!(
        installed.current
            && installed.enabled
            && installed.revision == expected.revision
            && installed.artifact_digest == expected.artifact_digest,
        "the installed plugin record does not name the expected generation: {installed:?}"
    );
    let content = node.lock().unit_content(
        FIXTURE_UNIT,
        &expected.revision,
        Some(&expected.artifact_digest),
    )?;
    ensure!(
        Digest::blake3(&content.bytes) == expected.artifact_digest,
        "the installation authority no longer serves the installed artifact"
    );

    let shared = FixturePieces {
        counters: Arc::new(Counters::default()),
        consumer_prepares: Arc::new(AtomicUsize::new(0)),
        generation_prepares: Arc::new(AtomicUsize::new(0)),
        attaches: Arc::new(AtomicUsize::new(0)),
        effect_log: effect_log.clone(),
        hold_release: Arc::new(Notify::new()),
    };

    // ------------------------------------------- root A: attach and retire --
    let gate_a = AttachGate::open();
    let root_a = Arc::new(Supervisor::new(RuntimeGeneration::new("fixture-root-a")));
    root_a.publish_services().await?;
    root_a
        .spawn(
            NativeChildPlugin,
            (store.clone(), RuntimeGeneration::new("fixture-root-a")),
        )
        .await?;
    let native_a = root_a.context().try_service::<NativeChildService>()?;
    root_a
        .spawn(
            shared.plugin(Arc::clone(&gate_a)),
            AdapterHost::new(&root_a, store.clone(), node.clone()),
        )
        .await?;
    wait_flag("the installed generation to attach", 20, || {
        native_a.has_handler(FIXTURE_KIND, FIXTURE_VERSION)
    })
    .await?;
    ensure!(
        shared.consumer_prepares.load(Ordering::SeqCst) == 1
            && shared.generation_prepares.load(Ordering::SeqCst) == 1
            && shared.attaches.load(Ordering::SeqCst) == 1,
        "prepare/publish/attach did not each run exactly once for the first root"
    );
    let published = root_a
        .runtime
        .provider(&provider_id())
        .context("the attached definition is absent from the live registry")?;
    ensure!(
        published.implementation.artifact.digest
            == shenko_kernel::native_artifact::native_artifact_digest()?,
        "the published provider does not name the executing implementation"
    );
    ensure!(
        root_a.runtime.capability(FIXTURE_KIND).is_some(),
        "the attached definition did not publish its capability"
    );

    // ------------------------------------------------------- the real call --
    let record = fixture_call("record");
    let first = native_a
        .call_tool(&record, &store.external_task(parent_id)?, &authority)
        .await?;
    ensure!(
        first.outcome == "succeeded",
        "the fixture call did not succeed: {first:?}"
    );
    let first_operation = store.external_operation(first.operation_id)?;
    ensure!(
        first_operation.state == OperationState::Resolved
            && first_operation.effect_status == EffectStatus::Complete,
        "the real call did not settle complete: {first_operation:?}"
    );
    let (first_receipts, first_artifact) = call_record(&store, &first_operation)?;
    ensure!(
        effects(&effect_log)?.len() == 1,
        "one successful call must append exactly one owned effect line"
    );
    assert_retained(&store, &first_operation, &first_receipts, &first_artifact)?;

    // ------------------------------- retirement with an in-flight old call --
    let hold = fixture_call("hold");
    let hold_native = Arc::clone(&native_a);
    let hold_parent = store.external_task(parent_id)?;
    let hold_authority = authority.clone();
    let worker = tokio::spawn(async move {
        hold_native
            .call_tool(&hold, &hold_parent, &hold_authority)
            .await
    });
    wait_flag("the holding call to dispatch before retirement", 20, || {
        shared.counters.hold_entered.load(Ordering::SeqCst)
    })
    .await?;
    ensure!(
        effects(&effect_log)?.len() == 2,
        "the in-flight call did not record its owned effect before retirement"
    );

    node.lock().handle_unit_request(UnitRequest::SetEnabled {
        unit_id: FIXTURE_UNIT.into(),
        enabled: false,
    })?;
    wait_flag("the retired definition to leave the root", 20, || {
        !native_a.has_handler(FIXTURE_KIND, FIXTURE_VERSION)
    })
    .await?;
    ensure!(
        root_a.runtime.provider(&provider_id()).is_none()
            && root_a.runtime.capability(FIXTURE_KIND).is_none(),
        "the retired generation stayed published in the live registry"
    );

    // The old call still owns its original handler and completes; retirement
    // only stops new discovery and waits for the workers it already owns.
    shared.hold_release.notify_one();
    let completed = tokio::time::timeout(Duration::from_secs(30), worker)
        .await
        .context("the in-flight fixture call did not complete after retirement")??;
    let completed = completed?;
    ensure!(
        completed.outcome == "succeeded",
        "the old call did not complete under its original implementation: {completed:?}"
    );
    wait_flag("the retired generation runtime to retire", 20, || {
        shared.counters.retired.load(Ordering::SeqCst) == 1
    })
    .await?;
    let hold_operation = store.external_operation(completed.operation_id)?;
    ensure!(
        hold_operation.state == OperationState::Resolved
            && effects(&effect_log)?.len() == 2,
        "the retired generation did not settle the old call exactly once"
    );
    let refused = native_a
        .call_tool(&fixture_call("record"), &store.external_task(parent_id)?, &authority)
        .await?;
    ensure!(
        refused.outcome == "unavailable" && refused.operation_id == EntityId::nil(),
        "a retired definition still admitted a fresh call: {refused:?}"
    );
    // The install record is retained but not offered: the unit is still
    // inspectable while no generation is attached.
    let disabled = node
        .lock()
        .installed_unit_fact(FIXTURE_UNIT)?
        .context("a disabled unit record disappeared")?;
    ensure!(
        !disabled.enabled
            && node
                .lock()
                .retained_unit_fact(FIXTURE_UNIT, &expected.revision, &expected.artifact_digest)?
                .is_some(),
        "retirement deleted the installed unit record or its artifact"
    );
    assert_retained(&store, &first_operation, &first_receipts, &first_artifact)?;

    // ------------------------- restart while the old root is mid-attach ----
    gate_a.arm();
    node.lock().handle_unit_request(UnitRequest::SetEnabled {
        unit_id: FIXTURE_UNIT.into(),
        enabled: true,
    })?;
    wait_flag("the re-enabled generation to enter attach", 20, || {
        gate_a.entered()
    })
    .await?;
    ensure!(
        shared.generation_prepares.load(Ordering::SeqCst) == 2
            && shared.attaches.load(Ordering::SeqCst) == 1
            && !native_a.has_handler(FIXTURE_KIND, FIXTURE_VERSION),
        "the pending attach already published a definition or skipped preparation"
    );

    let gate_b = AttachGate::open();
    let root_b = Arc::new(Supervisor::new(RuntimeGeneration::new("fixture-root-b")));
    root_b.publish_services().await?;
    root_b
        .spawn(
            NativeChildPlugin,
            (store.clone(), RuntimeGeneration::new("fixture-root-b")),
        )
        .await?;
    let native_b = root_b.context().try_service::<NativeChildService>()?;
    root_b
        .spawn(
            shared.plugin(Arc::clone(&gate_b)),
            AdapterHost::new(&root_b, store.clone(), node.clone()),
        )
        .await?;
    wait_flag("the restarted root to attach the committed unit", 20, || {
        native_b.has_handler(FIXTURE_KIND, FIXTURE_VERSION)
    })
    .await?;
    ensure!(
        root_b.runtime.provider(&provider_id()).is_some()
            && !native_a.has_handler(FIXTURE_KIND, FIXTURE_VERSION),
        "the restart did not adopt the committed generation exactly once"
    );
    ensure!(
        shared.attaches.load(Ordering::SeqCst) == 2,
        "the restarted root did not attach exactly one generation"
    );

    // The pending attach of the old root is released and completes in its own
    // root, then that root is retired. The restarted root is unaffected.
    gate_a.release();
    wait_flag("the pending old-root attach to complete", 20, || {
        native_a.has_handler(FIXTURE_KIND, FIXTURE_VERSION)
    })
    .await?;
    root_a.stop().await?;
    wait_flag("the old root's second generation to retire", 20, || {
        shared.counters.retired.load(Ordering::SeqCst) == 2
    })
    .await?;
    ensure!(
        native_b.has_handler(FIXTURE_KIND, FIXTURE_VERSION),
        "retiring the old root withdrew the restarted root's generation"
    );
    // Receipts survived the restart: this is the same Store read by a new root.
    assert_retained(&store, &first_operation, &first_receipts, &first_artifact)?;

    // ------------------------------------- real call in the restarted root --
    let second = native_b
        .call_tool(&fixture_call("record"), &store.external_task(parent_id)?, &authority)
        .await?;
    ensure!(
        second.outcome == "succeeded",
        "the restarted root could not execute the definition: {second:?}"
    );
    let second_operation = store.external_operation(second.operation_id)?;
    let (second_receipts, second_artifact) = call_record(&store, &second_operation)?;
    ensure!(
        effects(&effect_log)?.len() == 3,
        "the restarted root replayed or dropped the owned effect"
    );

    // ----------------------------- final retirement in the restarted root --
    node.lock().handle_unit_request(UnitRequest::SetEnabled {
        unit_id: FIXTURE_UNIT.into(),
        enabled: false,
    })?;
    wait_flag("the restarted root to withdraw the definition", 20, || {
        !native_b.has_handler(FIXTURE_KIND, FIXTURE_VERSION)
    })
    .await?;
    wait_flag("the restarted generation runtime to retire", 20, || {
        shared.counters.retired.load(Ordering::SeqCst) == 3
    })
    .await?;
    ensure!(
        root_b.runtime.provider(&provider_id()).is_none(),
        "the retired restarted generation stayed published"
    );
    assert_retained(&store, &first_operation, &first_receipts, &first_artifact)?;
    assert_retained(&store, &second_operation, &second_receipts, &second_artifact)?;

    root_b.stop().await?;
    ensure!(
        shared.counters.retired.load(Ordering::SeqCst) == 3,
        "the final stop reported a generation that was still resident"
    );
    ensure!(
        !effect_log.exists() || effects(&effect_log)?.len() == 3,
        "the final stop changed the owned effect log"
    );
    Ok(())
}

struct FixturePieces {
    counters: Arc<Counters>,
    consumer_prepares: Arc<AtomicUsize>,
    generation_prepares: Arc<AtomicUsize>,
    attaches: Arc<AtomicUsize>,
    effect_log: PathBuf,
    hold_release: Arc<Notify>,
}

impl FixturePieces {
    fn plugin(&self, gate: Arc<AttachGate>) -> FixturePlugin {
        FixturePlugin {
            gate,
            counters: Arc::clone(&self.counters),
            consumer_prepares: Arc::clone(&self.consumer_prepares),
            generation_prepares: Arc::clone(&self.generation_prepares),
            attaches: Arc::clone(&self.attaches),
            effect_log: self.effect_log.clone(),
            hold_release: Arc::clone(&self.hold_release),
        }
    }
}
