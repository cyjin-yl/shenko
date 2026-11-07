//! The **old** public PMA / native-handler producer.
//!
//! What this program does
//! ----------------------
//! It builds one private installation with two genuine subjects (an *owner*
//! agent A and a *foreign* agent B), genuine research epochs owned by A's
//! subject-root task, genuine native `pma.evaluate` tasks, the real native-child
//! runtime, the real broker, the real `ModelMemoryInterventionPolicy` answer
//! parser, the real contribution registry and the **pre-fix** public
//! `pending_intervention_reminders` / `pending_interventions_for_scope`
//! readers.
//!
//! It then lets that old code commit a reminder whose decision was authored by
//! B's operation while the epoch belongs to A, and records what the old
//! preparation boundary published for A.
//!
//! What is scripted, and what is not
//! ---------------------------------
//! * The **provider answer body** is a fixed string served by a loopback HTTP
//!   server. This is the only scripted surface: a deterministic answer is what
//!   makes the run reproducible, and the receipt labels it as scripted.
//! * The **policy** is the shipped `ModelMemoryInterventionPolicy` and it parses
//!   the answer for real; the reminder's content, scope, provenance and expiry
//!   are the parser's own reading of the served text.
//! * The **store, dispatcher, native task admission, native handler, operation,
//!   armed T1 attempt, sealed input, sealed provider result, E1 evidence receipt
//!   and settlement** are the product's own code paths, unpatched.
//!
//! What it deliberately never does
//! -------------------------------
//! * it never inserts a reminder row, an evidence receipt, an operation, an
//!   attempt or a task by hand: every one of them comes out of the product's own
//!   settlement path;
//! * it does not use raw SQL to read back what it claims;
//! * it does not edit, copy or relocate the source tree it is linked against.
//!
//! Output
//! ------
//! One JSON receipt written to `--out`; it is the reader's input contract (see
//! `scripts/pma_legacy_reminder_consumer.py`). Exit code 3 is reserved for
//! `BEFORE_PATH_NOT_VULNERABLE`: the old producer ran to completion but its own
//! preparation boundary did not publish or drain the foreign reminder, so the
//! premise of the comparison does not hold and no receipt is claimed.
//!
//! Contract position (SPEC §51.1–§51.10, decisions 0002/0003, PR180 merge
//! 2cb17172)
//! ---------------------------------------------------------------------
//! This program links the **pre-fix** tree, so it produces the old contract's
//! mechanism records: a reminder row, an operation, an armed attempt, an
//! accepted settlement receipt and an immutable sealed decision object. It is
//! recorded as old-contract code by construction and no §51 acceptance is
//! claimed from it — its only job is to leave a genuine installation for the
//! candidate to refuse. The rows it writes are exactly the ones the canonical
//! atoms contract keeps as mechanism records and never rewrites; the reader
//! asserts they survive byte-for-byte.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, ensure};
use serde_json::{Map, Value, json};
use shenko_broker::{
    broker::{Broker, BrokerHandle},
    local_provider::LocalModelProvider,
};
use shenko_context_policy::{ContributionHost, ContributionPayload, ContributionRegistry, scope_wide};
use shenko_kernel::Supervisor;
use shenko_memory::{
    MemoryInterventionRegistry, ModelMemoryInterventionPolicy,
    intervention_runtime::{MemoryInterventionConfig, MemoryInterventionPlugin},
};
use shenko_store::{
    InterventionDecisionKind, InterventionReminder, ResearchBudget, ResearchClosure,
    ResearchEnvironment, Store,
};
use shenko_subject::{
    InboxEvent, InboxEventKind, NativeChildPlugin, NativeChildService, NativeTaskHandler,
    NativeTaskHandlerPlugin, PmaEvaluateHandler,
};
use shenko_types::commit::CommitRef;
use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};
use shenko_types::ids::*;
use shenko_types::object::{Digest, ObjectRef};
use shenko_types::operation::{AttemptRecord, OperationRecord};
use shenko_types::subject::*;
use shenko_types::task::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The bounded decision the scripted provider serves for the foreign author.
/// The text is scripted; its acceptance is the real policy parser's decision.
const FOREIGN_CONTENT: &str =
    "scripted foreign-author reminder: keep the sealed decision identity intact";
/// The bounded decision the scripted provider serves for the owning author.
const OWN_CONTENT: &str = "scripted owner reminder: the epoch owner may prepare this";
/// The provenance reference of both scripted answers. The epochs carry no bank
/// entries, so the parser does not bind provenance to a bank row; the receipt
/// states that as a declared limitation, not as an accepted fact.
const PROVENANCE: &str = "scripted:consumer-fixture";

fn answer(content: &str) -> String {
    serde_json::to_string(&json!({
        "decision": "remind",
        "content": content,
        "provenance": [PROVENANCE],
        "relevance": "the current research",
        "scope": "subject",
        "expires_in_secs": 600,
    }))
    .expect("static scripted answer")
}

struct Args {
    installation: PathBuf,
    out: PathBuf,
    deadline_secs: u64,
}

fn parse_args() -> Result<Args> {
    let mut installation = None;
    let mut out = None;
    let mut deadline_secs = 180u64;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--installation" => {
                installation = Some(PathBuf::from(
                    args.next().context("--installation needs a directory")?,
                ))
            }
            "--out" => out = Some(PathBuf::from(args.next().context("--out needs a file")?)),
            "--deadline-secs" => {
                deadline_secs = args
                    .next()
                    .context("--deadline-secs needs a number")?
                    .parse()
                    .context("--deadline-secs is not a number")?
            }
            "--help" | "-h" => {
                println!(
                    "pma-legacy-reminder-producer --installation <fresh dir> --out <receipt json> \
                     [--deadline-secs 180]"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(Args {
        installation: installation.context("missing --installation")?,
        out: out.context("missing --out")?,
        deadline_secs,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// The canonical bytes this program observed, so the reader can compare bytes
/// instead of trusting a digest it computed itself.
fn entry<T: serde::Serialize + ?Sized>(value: &T) -> Result<Value> {
    let bytes = serde_json::to_vec(value)?;
    Ok(json!({
        "json": serde_json::from_slice::<Value>(&bytes)?,
        "canonical_len": bytes.len(),
        "canonical_sha256": sha256_hex(&bytes),
    }))
}

fn read_sealed(store: &Store, object: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "sealed object closure mismatch for {}",
        object.digest
    );
    Ok(bytes)
}

fn generation() -> RuntimeGeneration {
    RuntimeGeneration("pma-legacy-reminder-consumer".into())
}

/// One genuine subject: genesis, head, first planned step and root task.
fn initialize(store: &Store) -> Result<(EntityId, SubjectBranchHead)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(
            b"synthetic consumer fixture subject",
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
        created_commit: precommit,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: generation(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
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
    Ok((agent, head))
}

fn closure(label: &str) -> ResearchClosure {
    ResearchClosure {
        constraints: vec![],
        budget: ResearchBudget {
            max_tokens: Some(1000),
            max_duration_secs: None,
            max_cost_units: None,
            notes: None,
        },
        effects: vec![],
        environment: ResearchEnvironment {
            environment_id: format!("pma-legacy-reminder-{label}"),
            facts: vec!["synthetic consumer observation".into()],
            toolchain: vec![],
            dataset_versions: vec![],
            reproducible: true,
        },
    }
}

/// Admit one real native `pma.evaluate` task through the product's own
/// admission path. `agent` is the task's own agent; the epoch it names is a
/// separate fact, which is exactly the link this producer studies.
fn admit_pma_task(
    store: &Store,
    agent: EntityId,
    head: &SubjectBranchHead,
    implementation_ref: ImplementationRef,
    epoch: EntityId,
    lineage: EntityId,
    summary: &str,
) -> Result<EntityId> {
    let task_id = EntityId::new();
    let input_ref = store
        .cas
        .seal_object(
            &serde_json::to_vec(&json!({
                "lineage_id": lineage,
                "epoch_id": epoch,
                "max_tokens": 96,
                "trajectory": [{"kind": "message", "summary": summary, "at_ms": 1}],
            }))?,
            "application/json",
            "fixture",
            "pma-request",
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
            id: head.subject_root_task_id,
        },
        background: false,
        kind: "pma.evaluate".into(),
        definition_version: 1,
        implementation_ref,
        input_ref,
        state: TaskState::Pending {
            checkpoint_ref: None,
        },
        abort_request: None,
        memos: Map::from_iter([
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
    let event = InboxEvent {
        event_id: EntityId::new(),
        kind: InboxEventKind::ArtifactChanged,
        created_at: chrono::Utc::now(),
        source: "pma-legacy-reminder-consumer".into(),
        payload: json!({"task_id": task_id}),
    };
    let raw = shenko_types::store::RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "artifact_changed".into(),
        payload_json: serde_json::to_string(&event)?,
        received_at_ms: event.created_at.timestamp_millis(),
    };
    ensure!(
        store.admit_native_task(&task, &raw)?.is_none(),
        "a fresh native admission must insert, not replay"
    );
    Ok(task_id)
}

fn await_settlement(
    store: &Store,
    native: &NativeChildService,
    task: EntityId,
    deadline_secs: u64,
) -> Result<()> {
    let start = Instant::now();
    loop {
        native.run_once().await?;
        if let Some(result) = store.native_task_result(task)? {
            ensure!(
                matches!(result.outcome, TaskOutcome::Succeeded),
                "the bounded memory-intervention task did not succeed: {result:?}"
            );
            return Ok(());
        }
        let current = store.external_task(task)?;
        ensure!(
            start.elapsed() < Duration::from_secs(deadline_secs),
            "the memory-intervention task did not settle within {deadline_secs}s: state={:?} park={:?}",
            current.state,
            shenko_store::child::park_reason(&current),
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Everything the durable settlement of one bounded call left behind, read
/// through the product's public readers.
struct Call {
    operation: OperationRecord,
    attempt: AttemptRecord,
    receipt: EvidenceReceipt,
    normalized: Value,
    normalized_sha256: String,
    decision: ObjectRef,
    decision_sha256: String,
    reminder: InterventionReminder,
}

fn capture_call(store: &Store, epoch: EntityId, task: EntityId) -> Result<Call> {
    let mut operations = store.external_task_operations(task, "pma.evaluate.v1")?;
    ensure!(
        operations.len() == 1,
        "expected exactly one bounded intervention operation, found {}",
        operations.len()
    );
    let operation = operations.pop().expect("checked length");
    let attempt = store.external_operation_attempt(operation.operation_id)?;
    let mut receipts = store.external_operation_evidence(operation.operation_id)?;
    ensure!(
        receipts.len() == 1,
        "expected exactly one settlement receipt, found {}",
        receipts.len()
    );
    let receipt = receipts.pop().expect("checked length");
    let reminder = store
        .latest_intervention(epoch)?
        .context("the bounded call committed no decision")?;
    ensure!(
        reminder.decision == InterventionDecisionKind::Remind,
        "the scripted answer must have been parsed as a reminder"
    );
    ensure!(
        matches!(receipt.decode_status, DecodeStatus::Decoded),
        "the settlement receipt is not a decoded observation"
    );
    ensure!(
        receipt.operation_id == Some(operation.operation_id)
            && receipt.attempt_id == Some(attempt.attempt_id),
        "the settlement receipt does not name its own operation and attempt"
    );
    ensure!(
        receipt.source_event_key.as_deref()
            == Some(format!("pma-decision:{}", reminder.reminder_id).as_str()),
        "the settlement receipt does not name the committed decision"
    );
    let normalized_ref = receipt
        .normalized_ref
        .as_ref()
        .context("the settlement receipt carries no normalized decision")?;
    let normalized_bytes = read_sealed(store, normalized_ref)?;
    let normalized: Value = serde_json::from_slice(&normalized_bytes)?;
    let decision: ObjectRef = serde_json::from_value(normalized["decision_ref"].clone())
        .context("the normalized decision names no decision object")?;
    let decision_bytes = read_sealed(store, &decision)?;
    let sealed: InterventionReminder = serde_json::from_slice(&decision_bytes)?;
    ensure!(
        sealed == reminder,
        "the sealed decision differs from the durable reminder row"
    );
    Ok(Call {
        operation,
        attempt,
        receipt,
        normalized_sha256: sha256_hex(&normalized_bytes),
        normalized,
        decision_sha256: sha256_hex(&decision_bytes),
        decision,
        reminder,
    })
}

/// Whole-installation file inventory. `durable` covers the ledger database and
/// the content store; `auxiliary` covers SQLite's own side files, which are
/// recorded but never asserted (they are SQLite bookkeeping, not product
/// facts).
fn inventory(root: &Path) -> Result<Value> {
    fn walk(root: &Path, dir: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_dir() {
                walk(root, &path, files)?;
            } else if meta.file_type().is_file() {
                let relative = path
                    .strip_prefix(root)
                    .expect("walked path is under the root")
                    .to_string_lossy()
                    .into_owned();
                files.insert(relative, sha256_hex(&std::fs::read(&path)?));
            } else {
                anyhow::bail!(
                    "unexpected non-file entry in the installation: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files)?;
    let mut durable = String::new();
    let mut durable_files = Map::new();
    let mut auxiliary = Map::new();
    for (relative, digest) in &files {
        let product_owned = relative.as_str() == "store.db"
            || relative.starts_with("store.db-wal")
            || relative.starts_with("cas/");
        if product_owned {
            durable.push_str(relative);
            durable.push('\0');
            durable.push_str(digest);
            durable.push('\n');
            durable_files.insert(relative.clone(), json!(digest));
        } else {
            auxiliary.insert(relative.clone(), json!(digest));
        }
    }
    Ok(json!({
        "root": root.display().to_string(),
        "durable_sha256": sha256_hex(durable.as_bytes()),
        "durable_files": Value::Object(durable_files),
        "auxiliary_files": Value::Object(auxiliary),
        "wal_present": files.contains_key("store.db-wal"),
    }))
}

async fn run() -> Result<()> {
    let args = parse_args()?;
    ensure!(
        !args.installation.join("store.db").exists(),
        "--installation must be fresh: {} already holds a ledger",
        args.installation.display()
    );
    std::fs::create_dir_all(&args.installation)?;
    let db_path = args.installation.join("store.db");
    let cas_path = args.installation.join("cas");

    let store = Arc::new(Store::open(db_path.clone(), cas_path.clone())?);
    let (owner, owner_head) = initialize(&store)?;
    let (author, author_head) = initialize(&store)?;
    ensure!(
        owner != author && owner_head.branch_id != author_head.branch_id,
        "the two subjects must be distinct"
    );
    store.start_subject_dispatcher()?;

    // Both epochs belong to the OWNER's committed subject-root task. The first
    // decision is authored by a task of the OTHER agent.
    let foreign_epoch = store.open_research_lineage(
        EntityId::new(),
        EntityId::new(),
        Some(owner_head.subject_root_task_id),
        &closure("foreign"),
    )?;
    let own_epoch = store.open_research_lineage(
        EntityId::new(),
        EntityId::new(),
        Some(owner_head.subject_root_task_id),
        &closure("own"),
    )?;
    ensure!(
        foreign_epoch.task_id == Some(owner_head.subject_root_task_id)
            && own_epoch.task_id == Some(owner_head.subject_root_task_id),
        "both epochs must be owned by the owner's subject-root task"
    );

    // Scripted provider: a real loopback HTTP server answering the real broker.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let served = Arc::new(AtomicUsize::new(0));
    let served_server = served.clone();
    let answers = [answer(FOREIGN_CONTENT), answer(OWN_CONTENT)];
    let server = tokio::spawn(async move {
        for answer in answers {
            let (mut socket, _) = listener.accept().await?;
            read_wire(&mut socket).await?;
            served_server.fetch_add(1, Ordering::SeqCst);
            let raw = json!({
                "id": "scripted-pma-legacy-answer",
                "choices": [{
                    "message": {"role": "assistant", "content": answer},
                    "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 12, "completion_tokens": 7},
            })
            .to_string();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}",
                        raw.len()
                    )
                    .as_bytes(),
                )
                .await?;
            socket.shutdown().await?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle): (Broker, BrokerHandle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint.clone(), "scripted-pma-legacy-provider".into()),
        1,
    )?;
    let broker_run = tokio::spawn(broker.run());

    let supervisor = Supervisor::new(generation());
    supervisor
        .spawn(NativeChildPlugin, (store.clone(), generation()))
        .await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let registry = Arc::new(ContributionRegistry::new());
    supervisor.spawn(ContributionHost, registry.clone()).await?;
    let failure_cancel = tokio_util::sync::CancellationToken::new();
    let policy = Arc::new(MemoryInterventionRegistry::new(Arc::new(
        ModelMemoryInterventionPolicy,
    )));
    supervisor
        .spawn(
            MemoryInterventionPlugin,
            MemoryInterventionConfig {
                store: store.clone(),
                agent_id: owner,
                policy: policy.clone(),
                failure_cancel: failure_cancel.clone(),
            },
        )
        .await?;
    let handler: Arc<dyn NativeTaskHandler> = Arc::new(PmaEvaluateHandler::new(
        store.clone(),
        handle.clone(),
        policy,
    ));
    let implementation_ref = handler.descriptor().implementation_ref;
    supervisor
        .spawn(NativeTaskHandlerPlugin, handler)
        .await?;
    supervisor
        .spawn(
            shenko_subject::intervention::PmaEvaluatePlugin,
            (store.clone(), handle.clone()),
        )
        .await?;
    // Recovery runs after both the handler and the publisher seam exist, so it
    // observes exactly the registration a live subject would (the same order
    // the product's own pma settlement regression uses).
    native.recover()?;

    // 1. The foreign call: agent B's task commits a decision into agent A's epoch.
    let author_task = admit_pma_task(
        &store,
        author,
        &author_head,
        implementation_ref.clone(),
        foreign_epoch.epoch_id,
        foreign_epoch.lineage_id,
        "scripted foreign-author bounded decision",
    )?;
    await_settlement(&store, &native, author_task, args.deadline_secs).await?;
    let foreign = capture_call(&store, foreign_epoch.epoch_id, author_task)?;

    // 2. The own call: agent A's task commits a decision into A's own epoch.
    let owner_task = admit_pma_task(
        &store,
        owner,
        &owner_head,
        implementation_ref,
        own_epoch.epoch_id,
        own_epoch.lineage_id,
        "scripted owner bounded decision",
    )?;
    await_settlement(&store, &native, owner_task, args.deadline_secs).await?;
    let own = capture_call(&store, own_epoch.epoch_id, owner_task)?;

    ensure!(
        foreign.reminder.reminder_id != own.reminder.reminder_id,
        "the two bounded calls produced the same decision identity"
    );
    ensure!(
        foreign.reminder.content.as_deref() == Some(FOREIGN_CONTENT)
            && own.reminder.content.as_deref() == Some(OWN_CONTENT),
        "the parser did not read the served reminder texts"
    );
    let author_record = store.external_task(author_task)?;
    let owner_record = store.external_task(owner_task)?;
    let epoch_owner_task = foreign_epoch
        .task_id
        .context("the epoch must name its owning task")?;
    let epoch_owner_record = store.external_task(epoch_owner_task)?;
    ensure!(
        author_record.agent_id == author
            && owner_record.agent_id == owner
            && epoch_owner_record.agent_id == owner,
        "the admitted tasks do not carry the agents this producer intended"
    );

    // 3. What the OLD preparation boundary does with that state.
    let now = chrono::Utc::now().timestamp_millis();
    let owner_drain: Vec<InterventionReminder> = store.pending_intervention_reminders(owner, now, 32)?;
    let author_drain: Vec<InterventionReminder> = store.pending_intervention_reminders(author, now, 32)?;
    let scopes = store.pending_intervention_scopes(owner, now)?;
    let mut scope_readers = Vec::new();
    let mut scope_ids: Vec<String> = Vec::new();
    for scope in &scopes {
        let ids: Vec<EntityId> = store
            .pending_interventions_for_scope(scope, now)?
            .into_iter()
            .map(|reminder| reminder.reminder_id)
            .collect();
        let rendered: Vec<String> = ids.iter().map(ToString::to_string).collect();
        scope_ids.extend(rendered.iter().cloned());
        scope_readers.push(json!({
            "scope": serde_json::to_value(scope)?,
            "reminder_ids": rendered,
        }));
    }
    let foreign_delivery = store
        .intervention_delivery_scope(&foreign.reminder)?
        .context("the foreign decision has no delivery scope")?;
    let own_delivery = store
        .intervention_delivery_scope(&own.reminder)?
        .context("the own decision has no delivery scope")?;
    ensure!(
        foreign_delivery == own_delivery,
        "the two decisions must resolve to the same owner-side delivery scope"
    );

    // The real published batch of the old projection is what a request would
    // have collected.
    let collect = |registry: &ContributionRegistry| {
        registry.collect(
            &scope_wide(foreign_delivery.scope_id),
            chrono::Utc::now().timestamp_millis(),
        )
    };
    let foreign_key = format!("pma.reminder.{}", foreign.reminder.reminder_id);
    let own_key = format!("pma.reminder.{}", own.reminder.reminder_id);
    let mut published_keys: Vec<String> = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(args.deadline_secs) {
        let mut keys: Vec<String> = collect(&registry)
            .contributions
            .into_iter()
            .map(|item| item.key)
            .collect();
        keys.sort();
        published_keys = keys;
        if published_keys.contains(&foreign_key) && published_keys.contains(&own_key) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let foreign_text_published = collect(&registry)
        .contributions
        .iter()
        .filter(|item| item.key == foreign_key)
        .any(|item| {
            store
                .cas
                .read_object(&item.data.digest)
                .ok()
                .and_then(|bytes| ContributionPayload::read(&bytes).ok())
                .as_deref()
                == Some(FOREIGN_CONTENT)
        });

    let owner_drain_ids: Vec<String> = owner_drain.iter().map(|r| r.reminder_id.to_string()).collect();
    let author_drain_ids: Vec<String> = author_drain.iter().map(|r| r.reminder_id.to_string()).collect();
    let foreign_id = foreign.reminder.reminder_id.to_string();
    let own_id = own.reminder.reminder_id.to_string();
    let foreign_event_key = format!("pma-decision:{foreign_id}");
    let own_event_key = format!("pma-decision:{own_id}");
    let old_path_vulnerable = owner_drain_ids.contains(&foreign_id)
        && scope_ids.contains(&foreign_id)
        && published_keys.contains(&foreign_key)
        && foreign_text_published;

    // Everything the reader will be compared against, captured while the store
    // is still open and through public readers only.
    let provider_requests = served.load(Ordering::SeqCst);
    let owner_commit_seq = store.current_commit_seq()?;
    let foreign_lineage = store.research_lineage(foreign_epoch.lineage_id)?;
    let own_lineage = store.research_lineage(own_epoch.lineage_id)?;

    // Close and join the old process's runtime before anything is hashed.
    tokio::time::timeout(Duration::from_secs(args.deadline_secs), supervisor.stop())
        .await
        .context("the old supervisor did not stop in time")??;
    drop(native);
    drop(supervisor);
    drop(handle);
    broker_run.abort();
    let _ = broker_run.await;
    server.abort();
    match server.await {
        Err(error) if error.is_cancelled() => {}
        Ok(result) => result?,
        Err(error) => return Err(error.into()),
    }
    drop(store);

    let mut receipt = Map::new();
    receipt.insert("schema_version".into(), json!(1));
    receipt.insert("phase".into(), json!("before"));
    receipt.insert(
        "producer".into(),
        json!("shenko-pma-legacy-reminder-producer"),
    );
    receipt.insert("consumer".into(), json!("legacy_foreign_author_reminder"));
    receipt.insert(
        "contract".into(),
        json!({
            "reading": "SPEC §51.1–§51.10 with docs/decisions/0002 and 0003 (PR180 merge 2cb17172)",
            "position": "old-contract code by construction: this binary links the pre-fix tree and \
                emits that tree's mechanism records (reminder, operation, armed attempt, accepted \
                settlement receipt, immutable sealed decision). It claims no §51 acceptance",
            "reason_terminal": "the accepted settlement receipt (source_event_key \
                pma-decision:<request_id>) is the reason terminal of the committed decision",
            "confidence": "mechanism records carry no confidence field; this producer does not \
                invent one and writes no default number",
            "no_new_gate": "the foreign decision is admitted by the shipped native handler; no \
                keyword filter, allowlist, manual approval or static deny is involved",
        }),
    );
    receipt.insert(
        "scripted".into(),
        json!({
            "provider": "loopback HTTP server returning fixed OpenAI-chat bodies",
            "endpoint": endpoint,
            "model": "scripted-pma-legacy-provider",
            "answers": {"foreign": answer(FOREIGN_CONTENT), "own": answer(OWN_CONTENT)},
            "policy": "shenko_memory::ModelMemoryInterventionPolicy (shipped parser, unscripted)",
            "note": "only the provider body is scripted; store, dispatcher, native task admission, \
                     the pma.evaluate handler, broker, parser, armed T1 attempt, E1 settlement receipt \
                     and the public preparation readers are the product's own code",
            "bank_entries": 0,
            "declared_limitation": "the epochs carry no bank entries, so the parser does not bind \
                reminder provenance to a bank row; this run describes author admission, not \
                relevance quality",
        }),
    );
    receipt.insert(
        "installation".into(),
        json!({
            "root": args.installation.display().to_string(),
            "db": db_path.display().to_string(),
            "cas": cas_path.display().to_string(),
        }),
    );
    receipt.insert(
        "agents".into(),
        json!({"owner": owner.to_string(), "author": author.to_string()}),
    );
    receipt.insert(
        "heads".into(),
        json!({
            "owner": entry(&owner_head)?,
            "author": entry(&author_head)?,
            "owner_commit_seq": owner_commit_seq,
        }),
    );
    receipt.insert(
        "epochs".into(),
        json!({
            "foreign": entry(&foreign_epoch)?,
            "own": entry(&own_epoch)?,
            "epoch_owner_task": epoch_owner_task.to_string(),
            "foreign_lineage": entry(&foreign_lineage)?,
            "own_lineage": entry(&own_lineage)?,
        }),
    );
    receipt.insert(
        "tasks".into(),
        json!({
            "author_task": entry(&author_record)?,
            "owner_task": entry(&owner_record)?,
            "epoch_owner_task": entry(&epoch_owner_record)?,
        }),
    );
    receipt.insert(
        "calls".into(),
        json!({
            "foreign": {
                "operation": entry(&foreign.operation)?,
                "attempt": entry(&foreign.attempt)?,
                "receipt": entry(&foreign.receipt)?,
                "normalized_sha256": foreign.normalized_sha256,
                "normalized": foreign.normalized,
                "decision_object": foreign.decision,
                "decision_sha256": foreign.decision_sha256,
                "request_id": foreign_id.clone(),
                "source_event_key": foreign_event_key,
            },
            "own": {
                "operation": entry(&own.operation)?,
                "attempt": entry(&own.attempt)?,
                "receipt": entry(&own.receipt)?,
                "normalized_sha256": own.normalized_sha256,
                "normalized": own.normalized,
                "decision_object": own.decision,
                "decision_sha256": own.decision_sha256,
                "request_id": own_id.clone(),
                "source_event_key": own_event_key,
            },
        }),
    );
    receipt.insert(
        "reminders".into(),
        json!({
            "foreign": entry(&foreign.reminder)?,
            "own": entry(&own.reminder)?,
        }),
    );
    receipt.insert(
        "old_path".into(),
        json!({
            "vulnerable": old_path_vulnerable,
            "owner_agent_drain_ids": owner_drain_ids,
            "author_agent_drain_ids": author_drain_ids,
            "owner_pending_scopes": scopes
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()?,
            "scope_reminder_ids": scope_readers,
            "published_batch_keys": published_keys,
            "foreign_key": foreign_key,
            "own_key": own_key,
            "foreign_body_text_published": foreign_text_published,
            "delivery_scope": serde_json::to_value(&foreign_delivery)?,
        }),
    );
    receipt.insert("provider_requests".into(), json!(provider_requests));
    receipt.insert(
        "closed".into(),
        json!({
            "supervisor_stopped": true,
            "broker_joined": true,
            "provider_joined": true,
            "store_dropped": true,
        }),
    );
    receipt.insert("files".into(), inventory(&args.installation)?);

    if !old_path_vulnerable {
        anyhow::bail!(
            "BEFORE_PATH_NOT_VULNERABLE: the old preparation boundary did not publish the \
             foreign-authored decision (drain={owner_drain_ids:?}, scopes={scope_ids:?}, \
             batch={published_keys:?}, body={foreign_text_published})"
        );
    }
    std::fs::write(&args.out, serde_json::to_vec_pretty(&Value::Object(receipt))?)?;
    println!("{}", args.out.display());
    Ok(())
}

async fn read_wire(socket: &mut tokio::net::TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count != 0, "the provider request ended before its body");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let len: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .context("the provider request carries no content length")?
                .parse()?;
            if bytes.len() >= end + 4 + len {
                return Ok(serde_json::from_slice(&bytes[end + 4..end + 4 + len])?);
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        let text = format!("{error:#}");
        eprintln!("pma-legacy-reminder-producer failed: {text}");
        std::process::exit(if text.contains("BEFORE_PATH_NOT_VULNERABLE") {
            3
        } else {
            2
        });
    }
}
