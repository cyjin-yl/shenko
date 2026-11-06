//! OS-process partition settlement proof for durable obligations.
//!
//! One genuine committed obligation origin is forked into two peer stores by
//! copying the quiesced origin store, and two *independent OS processes* each
//! commit their own real S0/S1 terminal settlement in their own peer (A:
//! `completed`, B: `cancelled`). Both children wait on a rendezvous barrier
//! before settling, so the run proves the two settlements happened in
//! overlapping process lifetimes rather than one after the other. The peers
//! then exchange their real archives (A into B, B into A, B into a store that
//! already carries A's replicated facts), and a converged store attempts a
//! competing settlement, exports, and relays to a fourth store.
//!
//! The proof is source-only evidence: it uses the real parser, Store, CAS and
//! archive code, but no model, no provider and no network. It proves nothing
//! about physical power loss, real installations with distinct ledgers, or
//! model behaviour; those limits are stated in the emitted receipt.
//!
//! Run it explicitly, with a fresh durable root (never `/tmp`):
//!
//! ```text
//! SHENKO_OBLIGATION_PARTITION_ROOT=<fresh durable root> \
//!   cargo test -p shenko-subject --lib \
//!     recovery_tests::obligation_partition_tests::partition_orchestrator \
//!     -- --exact --ignored --nocapture
//! ```
//!
//! The test is `#[ignore]`d so an ordinary `cargo test` never runs it against
//! an accidental root. Every child writes `<root>/receipts/<name>.json`, the
//! orchestrator writes `<root>/receipt.json`, and a failed run keeps both its
//! receipt and the child output that produced it.
use super::*;
use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use shenko_types::subject::{
    ObligationTerminalOutcome, ObligationTerminalReceipt, ObligationTerminalState,
};
use std::io::Read;
use std::path::{Path, PathBuf};

/// The sealed policy contract a terminal settlement is stored under. It is a
/// wire constant, not a private crate item.
const TERMINAL_CONTRACT: &str = "subject/obligation-terminal/1";
/// The refusal a required obligation unit produces when a member diverges.
const UNIT_REFUSAL: &str = "required obligation conflict unit diverges";

const ROOT_ENV: &str = "SHENKO_OBLIGATION_PARTITION_ROOT";
const MODE_ENV: &str = "SHENKO_OBLIGATION_PARTITION_MODE";
const PEER_ENV: &str = "SHENKO_OBLIGATION_PARTITION_PEER";
const STORE_ENV: &str = "SHENKO_OBLIGATION_PARTITION_STORE";
const AGENT_ENV: &str = "SHENKO_OBLIGATION_PARTITION_AGENT";
const OBLIGATION_ENV: &str = "SHENKO_OBLIGATION_PARTITION_OBLIGATION";
const STATE_ENV: &str = "SHENKO_OBLIGATION_PARTITION_STATE";
const ARCHIVE_ENV: &str = "SHENKO_OBLIGATION_PARTITION_ARCHIVE";
const ORIGIN_ARCHIVE_ENV: &str = "SHENKO_OBLIGATION_PARTITION_ORIGIN_ARCHIVE";
const EXPECT_FINGERPRINT_ENV: &str = "SHENKO_OBLIGATION_PARTITION_EXPECT_FINGERPRINT";
const EXPECT_BYTES_ENV: &str = "SHENKO_OBLIGATION_PARTITION_EXPECT_BYTES";
const RENDEZVOUS_ENV: &str = "SHENKO_OBLIGATION_PARTITION_RENDEZVOUS";
const RECEIPT_ENV: &str = "SHENKO_OBLIGATION_PARTITION_RECEIPT";

const CHILD_SETTLE_A: &str = "partition_child_settle_peer_a";
const CHILD_SETTLE_B: &str = "partition_child_settle_peer_b";
const CHILD_REFUSE_B: &str = "partition_child_refuse_b_into_a";
const CHILD_CONVERGE: &str = "partition_child_converge_store_d";
const CHILD_REFUSE_D: &str = "partition_child_refuse_b_into_d";
const CHILD_RELAY: &str = "partition_child_relay_store_e";

fn partition_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn partition_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
}

fn partition_id(name: &str) -> EntityId {
    partition_env(name)
        .parse()
        .unwrap_or_else(|error| panic!("{name} is not an entity id: {error}"))
}

fn partition_receipt_path() -> PathBuf {
    PathBuf::from(partition_env(RECEIPT_ENV))
}

fn partition_write_receipt(path: &Path, value: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("receipt directory");
    }
    let temporary = PathBuf::from(format!("{}.tmp", path.display()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(value).expect("receipt json"))
        .expect("write receipt");
    std::fs::rename(&temporary, path).expect("publish receipt");
}

fn partition_executable() -> serde_json::Value {
    let path = std::env::current_exe().expect("current executable");
    let bytes = std::fs::read(&path).expect("read current executable");
    serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": bytes.len(),
        "blake3": shenko_types::object::Digest::blake3(&bytes).to_string(),
    })
}

fn partition_snapshot_options() -> CognitionSnapshotOptions {
    CognitionSnapshotOptions { include_git_history: false, ..Default::default() }
}

fn partition_import_options() -> CognitionImportOptions {
    CognitionImportOptions { peer_id: Some(EntityId::new()), ..Default::default() }
}

fn partition_open(dir: &Path) -> Arc<Store> {
    std::fs::create_dir_all(dir).expect("store root");
    Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).expect("open store"))
}

/// A real genesis for one agent, so every subsequent commit is the ordinary
/// S0/S1 path rather than a hand-written row.
fn partition_genesis(dir: &Path, agent: EntityId) -> Arc<Store> {
    let store = partition_open(dir);
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
    let object = store
        .cas
        .seal_object(b"obligation-partition-seed", "text/plain", "fixture", "raw", 1)
        .unwrap()
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
        controller_epoch: ControllerEpoch(7),
        software_generation: RuntimeGeneration("obligation-partition".into()),
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
    let root_task =
        TaskRecord::subject_root(head.subject_root_task_id, agent, object, precommit);
    store.initialize_genesis(&genesis, &head, &first, &root_task).unwrap();
    store
}

/// Admit one durable input delivery the way the store really receives it.
fn partition_admit(store: &Store, agent: EntityId, event: &crate::inbox::InboxEvent) {
    let mut batch = shenko_types::store::WriteBatch::new();
    batch.inbox_event_upserts.push(shenko_types::store::RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "message".into(),
        payload_json: serde_json::to_string(&event.payload).unwrap(),
        received_at_ms: event.created_at.timestamp_millis(),
    });
    store.commit(batch).unwrap();
}

fn partition_terminal(id: EntityId, state: &str) -> String {
    format!(
        "[obligation:terminal] {}",
        serde_json::json!({
            "obligation_id": id,
            "state": state,
            "reason": "the authored terminal judgment of this peer",
        })
    )
}

/// The settlement intents and their typed receipts, in durable intent order.
fn partition_settlements(store: &Store) -> Vec<(serde_json::Value, ObligationTerminalReceipt)> {
    store
        .committed_intents(Some("policy"), None, usize::MAX / 2)
        .unwrap()
        .into_iter()
        .filter(|intent| intent.contract == TERMINAL_CONTRACT)
        .map(|intent| {
            assert_eq!(intent.state, "settled", "a terminal settlement intent is not settled");
            let receipt: ObligationTerminalReceipt = serde_json::from_str(
                intent.result_ref.as_deref().expect("a settled settlement carries its receipt"),
            )
            .expect("typed settlement receipt");
            (
                serde_json::json!({
                    "intent_id": intent.intent_id.to_string(),
                    "step_id": intent.step_id.to_string(),
                    "kind": intent.kind,
                    "contract": intent.contract,
                    "state": intent.state,
                    "revision": intent.revision,
                }),
                receipt,
            )
        })
        .collect()
}

fn partition_settlement_json(
    settlements: &[(serde_json::Value, ObligationTerminalReceipt)],
) -> serde_json::Value {
    serde_json::Value::Array(
        settlements
            .iter()
            .map(|(intent, receipt)| {
                serde_json::json!({
                    "intent": intent,
                    "receipt": serde_json::to_value(receipt).unwrap(),
                })
            })
            .collect(),
    )
}

/// The obligation projection exactly as stored, with the same text the
/// archive carries so a row can be compared across stores and processes.
fn partition_obligation(dir: &Path, id: EntityId) -> serde_json::Value {
    let connection = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    connection
        .query_row(
            "SELECT obligation_id,agent_id,step_id,COALESCE(origin_event_id,''),description,
                    COALESCE(depends_on_task_id,''),resume_condition,COALESCE(due_at,''),decision_ref,
                    created_seq,state,updated_seq
             FROM subject_obligations WHERE obligation_id=?1",
            [id.to_string()],
            |row| {
                Ok(serde_json::json!({
                    "obligation_id": row.get::<_, String>(0)?,
                    "agent_id": row.get::<_, String>(1)?,
                    "step_id": row.get::<_, String>(2)?,
                    "origin_event_id": row.get::<_, String>(3)?,
                    "description": row.get::<_, String>(4)?,
                    "depends_on_task_id": row.get::<_, String>(5)?,
                    "resume_condition": row.get::<_, String>(6)?,
                    "due_at": row.get::<_, String>(7)?,
                    "decision_ref": row.get::<_, String>(8)?,
                    "created_seq": row.get::<_, i64>(9)?.to_string(),
                    "state": row.get::<_, String>(10)?,
                    "updated_seq": row.get::<_, i64>(11)?.to_string(),
                }))
            },
        )
        .expect("obligation projection")
}

/// Counts of every local table the proof compares, plus the ledger high-water.
fn partition_footprint(dir: &Path) -> serde_json::Value {
    let connection = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    let mut counts = serde_json::Map::new();
    for table in [
        "channel_record_migrations",
        "cognition_baselines",
        "cognition_export_order",
        "cognition_import_order",
        "commit_log",
        "inbox_events",
        "subject_branch_heads",
        "subject_event_dispositions",
        "subject_intent_steps",
        "subject_intents",
        "subject_obligations",
        "subject_steps",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
            .unwrap();
        counts.insert(table.to_string(), serde_json::json!(count));
    }
    let max_seq: i64 = connection
        .query_row("SELECT COALESCE(MAX(seq),-1) FROM commit_log", [], |row| row.get(0))
        .unwrap();
    serde_json::json!({ "counts": counts, "ledger_max_seq": max_seq })
}

fn partition_import_order(dir: &Path) -> serde_json::Value {
    let connection = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT source_ledger_id,peer_id,snapshot_seq,high_water,fingerprint
             FROM cognition_import_order ORDER BY source_ledger_id",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(serde_json::json!({
                "source_ledger_id": row.get::<_, String>(0)?,
                "peer_id": row.get::<_, String>(1)?,
                "snapshot_seq": row.get::<_, i64>(2)?,
                "high_water": row.get::<_, i64>(3)?,
                "fingerprint": row.get::<_, String>(4)?,
            }))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    serde_json::Value::Array(rows)
}

fn partition_cas(dir: &Path) -> serde_json::Value {
    fn walk(directory: &Path, objects: &mut u64, bytes: &mut u64) {
        let Ok(entries) = std::fs::read_dir(directory) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&path, objects, bytes),
                Ok(kind) if kind.is_file() => {
                    *objects += 1;
                    *bytes += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    let (mut objects, mut bytes) = (0u64, 0u64);
    walk(&dir.join("cas"), &mut objects, &mut bytes);
    serde_json::json!({ "objects": objects, "bytes": bytes })
}

fn partition_copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let target = to.join(entry.file_name());
        match entry.file_type()? {
            kind if kind.is_dir() => partition_copy_tree(&path, &target)?,
            kind if kind.is_file() => {
                std::fs::copy(&path, &target)?;
            }
            _ => anyhow::bail!("unexpected file kind in a store root: {}", path.display()),
        }
    }
    Ok(())
}

/// Read an archive's own manifest frame. This never rewrites the bytes and
/// never reinterprets the schema: it reports exactly what the archive claims.
fn partition_manifest(path: &Path) -> anyhow::Result<serde_json::Value> {
    let mut file = std::fs::File::open(path)?;
    let mut line = String::new();
    let mut byte = [0u8; 1];
    let mut read_line = |file: &mut std::fs::File, line: &mut String| -> anyhow::Result<()> {
        line.clear();
        loop {
            anyhow::ensure!(line.len() < 8192, "archive frame header exceeds its bound");
            match file.read(&mut byte)? {
                0 => anyhow::bail!("archive ended inside a frame header"),
                _ if byte[0] == b'\n' => return Ok(()),
                _ => line.push(byte[0] as char),
            }
        }
    };
    read_line(&mut file, &mut line)?;
    anyhow::ensure!(line == "shenko-cognition-archive/1", "not a cognition archive: {line}");
    read_line(&mut file, &mut line)?;
    let mut parts = line.split(' ').filter(|part| !part.is_empty());
    anyhow::ensure!(parts.next() == Some("manifest"), "archive does not start with its manifest");
    let length: usize = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("manifest frame has no length"))?
        .parse()?;
    let mut bytes = vec![0u8; length];
    file.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn partition_archive_identity(path: &Path) -> anyhow::Result<serde_json::Value> {
    let manifest = partition_manifest(path)?;
    let mut counts = serde_json::Map::new();
    for row in manifest["rows"].as_array().into_iter().flatten() {
        let table = row["table"].as_str().unwrap_or("<unnamed>").to_string();
        let entry = counts.entry(table).or_insert(serde_json::json!(0));
        *entry = serde_json::json!(entry.as_u64().unwrap_or(0) + 1);
    }
    Ok(serde_json::json!({
        "path": path.to_string_lossy(),
        "bytes": std::fs::metadata(path)?.len(),
        "schema": manifest["schema"],
        "fingerprint": manifest["fingerprint"],
        "fact_classes": manifest["fact_classes"],
        "row_counts": counts,
        "gaps": manifest["gaps"],
        "high_water": manifest["high_water"],
        "source_snapshot_seq": manifest["source_snapshot_seq"],
        "source_ledger_id": manifest["source_ledger_id"],
    }))
}

/// One manifest row, by table and key, exactly as the archive stores it.
fn partition_row(
    manifest: &serde_json::Value,
    table: &str,
    key: &str,
) -> Option<serde_json::Value> {
    manifest["rows"]
        .as_array()?
        .iter()
        .find(|row| row["table"] == serde_json::json!(table) && row["key"] == serde_json::json!(key))
        .cloned()
}

/// The first manifest row of a class. A subject has one local branch, so the
/// head, genesis and scope classes are single-row sets in this proof.
fn partition_first_row(manifest: &serde_json::Value, table: &str) -> Option<serde_json::Value> {
    manifest["rows"]
        .as_array()?
        .iter()
        .find(|row| row["table"] == serde_json::json!(table))
        .cloned()
}

fn partition_store_manifest(store: &Store) -> serde_json::Value {
    let export = store.export_cognition(&partition_snapshot_options()).unwrap();
    serde_json::to_value(&export.manifest).unwrap()
}

fn partition_export(store: &Store, archive: &Path) -> serde_json::Value {
    if let Some(parent) = archive.parent() {
        std::fs::create_dir_all(parent).expect("archive directory");
    }
    let receipt = store
        .write_cognition_archive(
            &partition_snapshot_options(),
            None,
            std::fs::File::create(archive).expect("create archive"),
        )
        .expect("write archive");
    assert!(receipt.complete, "the exported archive is incomplete: {:?}", receipt.gaps);
    serde_json::json!({
        "identity": partition_archive_identity(archive).expect("archive identity"),
        "receipt": {
            "fingerprint": receipt.fingerprint.to_string(),
            "high_water": receipt.high_water.0,
            "rows": receipt.rows,
            "objects": receipt.objects,
            "files": receipt.files,
            "complete": receipt.complete,
            "gaps": receipt.gaps,
        },
    })
}

/// A step that may legitimately be refused at S0 (a converged store has to
/// re-derive its whole frozen context), so the caller can record an honest
/// outcome instead of panicking. Same real path as the retained fixture:
/// S0 → recorded result → S1.
async fn partition_try_commit_output(
    store: &Arc<Store>,
    subject: &mut SubjectLoopState,
    seq: u64,
    events: &[crate::inbox::InboxEvent],
    raw: &str,
) -> Result<EntityId, ShenkoError> {
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    execute_step(seq, events, subject, &cancelled).await?;
    let step_id = partition_record_result(store, subject, raw)?;
    execute_step(seq, &[], subject, &tokio_util::sync::CancellationToken::new())
        .await?
        .map(|_| step_id)
        .ok_or_else(|| ShenkoError::conflict("S1 did not commit"))
}

fn partition_record_result(
    store: &Arc<Store>,
    subject: &mut SubjectLoopState,
    raw: &str,
) -> Result<EntityId, ShenkoError> {
    let head = subject.head.clone();
    let mut step = subject.step.clone();
    let step_id = step.step_id;
    step.state = SubjectStepState::AwaitingModel;
    step.active_request_id = Some(EntityId::new());
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &head, revision)?;
    let raw_ref = store
        .cas
        .seal_object(raw.as_bytes(), "text/plain", "model_results", "text", 1)?
        .object;
    step.state = SubjectStepState::ResultRecorded;
    step.model_result_ref = Some(raw_ref.clone());
    step.model_outcome_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&SubjectModelOutcome {
                    schema_version: 2,
                    request_id: step.active_request_id.unwrap(),
                    result_ref: raw_ref,
                    stop_reason: "end_turn".into(),
                    raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                    usage: None,
                })
                .map_err(|error| ShenkoError::other(error.to_string()))?,
                "application/json",
                "model_outcomes",
                "SubjectModelOutcome",
                2,
            )?
            .object,
    );
    let revision = step.record_revision;
    step.record_revision = RecordRevision(revision.0 + 1);
    store.upsert_step(&step, &head, revision)?;
    subject.step = step;
    Ok(step_id)
}

/// Wait until every counterpart process is alive, so the settlements really
/// overlap instead of running one after the other.
fn partition_rendezvous(root: &Path, peer: &str, counterparts: &str) -> anyhow::Result<serde_json::Value> {
    let directory = root.join("rendezvous");
    std::fs::create_dir_all(&directory)?;
    let arrived = partition_now_ms();
    std::fs::write(directory.join(format!("{peer}.ready")), arrived.to_string())?;
    let mut waited = Vec::new();
    for counterpart in counterparts.split(',').filter(|name| !name.is_empty()) {
        let path = directory.join(format!("{counterpart}.ready"));
        let deadline = partition_now_ms() + 30_000;
        while !path.exists() {
            anyhow::ensure!(
                partition_now_ms() < deadline,
                "counterpart peer {counterpart} never reached the partition barrier"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        waited.push(counterpart.to_string());
    }
    Ok(serde_json::json!({ "arrived_at_ms": arrived, "waited_for": waited }))
}

/// Start one child of this same test binary. The child is a separate OS
/// process with its own store handle; it reports through its receipt file and
/// through its exit status.
fn partition_start(
    filter: &str,
    environment: Vec<(&'static str, String)>,
) -> anyhow::Result<(std::process::Child, u64)> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command
        .arg(filter)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1");
    for (name, value) in environment {
        command.env(name, value);
    }
    Ok((command.spawn()?, partition_now_ms()))
}

fn partition_collect(
    name: &str,
    mut child: std::process::Child,
    started_at_ms: u64,
    receipt_path: &Path,
) -> anyhow::Result<serde_json::Value> {
    let output = child.wait_with_output()?;
    let finished_at_ms = partition_now_ms();
    let receipt = if receipt_path.exists() {
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(receipt_path)?)?
    } else {
        serde_json::Value::Null
    };
    Ok(serde_json::json!({
        "name": name,
        "receipt_path": receipt_path.to_string_lossy(),
        "status": output.status.code(),
        "success": output.status.success(),
        "started_at_ms": started_at_ms,
        "finished_at_ms": finished_at_ms,
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "receipt": receipt,
    }))
}

// ---------------------------------------------------------------------------
// Children: each is a separate process, selected by an argv filter.
// ---------------------------------------------------------------------------

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_settle_peer_a() {
    partition_run_settle("a", "completed", "b").await.unwrap();
}

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_settle_peer_b() {
    partition_run_settle("b", "cancelled", "a").await.unwrap();
}

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_refuse_b_into_a() {
    partition_run_refuse("peer-a", "required unit refusal onto a partitioned peer").await.unwrap();
}

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_converge_store_d() {
    partition_run_converge().await.unwrap();
}

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_refuse_b_into_d() {
    partition_run_refuse("store-d", "required unit refusal onto a store holding replicated facts")
        .await
        .unwrap();
}

#[ignore = "OS-process obligation partition proof: run through partition_orchestrator"]
#[tokio::test]
async fn partition_child_relay_store_e() {
    partition_run_relay().await.unwrap();
}

/// One partitioned peer commits its own terminal settlement for the shared
/// obligation, then exports what it really holds.
async fn partition_run_settle(peer: &str, target_state: &str, counterpart: &str) -> anyhow::Result<()> {
    let started_at_ms = partition_now_ms();
    let receipt_path = partition_receipt_path();
    let result = partition_settle_body(peer, target_state, counterpart, started_at_ms).await;
    match result {
        Ok(value) => {
            partition_write_receipt(&receipt_path, &value);
            Ok(())
        }
        Err(error) => {
            partition_write_receipt(
                &receipt_path,
                &serde_json::json!({
                    "mode": "settle",
                    "peer": peer,
                    "failed": true,
                    "error": format!("{error:#}"),
                    "started_at_ms": started_at_ms,
                    "finished_at_ms": partition_now_ms(),
                }),
            );
            Err(error)
        }
    }
}

async fn partition_settle_body(
    peer: &str,
    target_state: &str,
    counterpart: &str,
    started_at_ms: u64,
) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(partition_env(MODE_ENV) == "settle", "wrong child mode");
    anyhow::ensure!(partition_env(PEER_ENV) == peer, "this binary was started for another peer");
    let root = PathBuf::from(partition_env(ROOT_ENV));
    let store_dir = PathBuf::from(partition_env(STORE_ENV));
    let agent = partition_id(AGENT_ENV);
    let obligation = partition_id(OBLIGATION_ENV);
    let archive = PathBuf::from(partition_env(ARCHIVE_ENV));
    let origin_archive = PathBuf::from(partition_env(ORIGIN_ARCHIVE_ENV));
    let expected_state: ObligationTerminalState = match target_state {
        "completed" => ObligationTerminalState::Completed,
        "cancelled" => ObligationTerminalState::Cancelled,
        other => anyhow::bail!("unknown settlement state {other}"),
    };
    // The archive this child settles from is authenticated against the values
    // the orchestrator recorded, so a replaced archive cannot pass silently.
    let origin = partition_archive_identity(&origin_archive)?;
    let expected_fingerprint = partition_env(EXPECT_FINGERPRINT_ENV);
    anyhow::ensure!(
        origin["fingerprint"].as_str() == Some(expected_fingerprint.as_str()),
        "the origin archive fingerprint is not the one the orchestrator recorded"
    );
    anyhow::ensure!(
        origin["bytes"].as_u64() == Some(partition_env(EXPECT_BYTES_ENV).parse()?),
        "the origin archive size is not the one the orchestrator recorded"
    );
    anyhow::ensure!(origin["schema"] == serde_json::json!(6), "the origin archive is not schema 6");
    let store = partition_open(&store_dir);
    // The fork must hold the committed origin obligation verbatim: compare the
    // local row with the row the origin archive declares.
    let local = partition_store_manifest(&store);
    let local_row = partition_row(&local, "subject_obligations", &obligation.to_string())
        .ok_or_else(|| anyhow::anyhow!("this peer holds no such obligation"))?;
    let origin_manifest = partition_manifest(&origin_archive)?;
    let origin_row = partition_row(&origin_manifest, "subject_obligations", &obligation.to_string())
        .ok_or_else(|| anyhow::anyhow!("the origin archive declares no such obligation"))?;
    anyhow::ensure!(
        local_row == origin_row,
        "the forked peer does not hold the committed origin obligation verbatim"
    );
    anyhow::ensure!(
        partition_obligation(&store_dir, obligation)["state"] == serde_json::json!("open"),
        "the partitioned peer must start from an open obligation"
    );
    anyhow::ensure!(
        partition_settlements(&store).is_empty(),
        "the partitioned peer already carries a settlement"
    );
    // Both peers are alive before either settles: the run is a partition, not a
    // sequential rehearsal.
    let barrier = partition_rendezvous(&root, peer, counterpart)?;
    let counts_before = partition_footprint(&store_dir);
    let cas_before = partition_cas(&store_dir);
    let head_before = store.load_subject(agent).map(|(head, step)| {
        serde_json::json!({
            "committed_step_no": head.committed_step_no,
            "next_step_no": step.step_no,
            "next_step_state": format!("{:?}", step.state),
        })
    })?;
    anyhow::ensure!(
        head_before["committed_step_no"] == serde_json::json!(1),
        "a partitioned peer must start at the origin commit"
    );
    let mut subject = state(store.clone(), agent).map_err(|error| anyhow::anyhow!("{error:#}"))?;
    // The real S0/S1 path: sealed input, recorded result, atomic settlement.
    let step_id = commit_output(&store, &mut subject, 2, &[], &partition_terminal(obligation, target_state)).await;
    let settlements = partition_settlements(&store);
    anyhow::ensure!(settlements.len() == 1, "the peer must hold exactly one settlement");
    let (intent, receipt) = &settlements[0];
    anyhow::ensure!(
        matches!(&receipt.outcome, ObligationTerminalOutcome::Applied),
        "the first settlement of an open obligation must be applied"
    );
    anyhow::ensure!(receipt.change.obligation_id == obligation, "the settlement names another obligation");
    anyhow::ensure!(receipt.change.state == expected_state, "the settlement applied another state");
    anyhow::ensure!(receipt.step_id == step_id, "the settlement belongs to another step");
    let settlement_step = store
        .get_step(&subject.head.branch_id, step_id)?
        .ok_or_else(|| anyhow::anyhow!("the settlement step is missing"))?;
    anyhow::ensure!(
        settlement_step.state == SubjectStepState::Committed,
        "the settlement step is not committed"
    );
    anyhow::ensure!(
        Some(&receipt.input_plan_ref) == settlement_step.input_plan_ref.as_ref(),
        "the settlement is not bound to its own sealed S0"
    );
    let obligation_after = partition_obligation(&store_dir, obligation);
    anyhow::ensure!(
        obligation_after["state"] == serde_json::json!(target_state),
        "the projection does not show the settled state"
    );
    // Creation provenance is immutable: the settled projection differs from
    // the committed origin projection only in its state and update sequence.
    let origin_projection = archive_row_as_projection(&origin_row);
    for field in [
        "obligation_id",
        "agent_id",
        "step_id",
        "origin_event_id",
        "description",
        "depends_on_task_id",
        "resume_condition",
        "due_at",
        "decision_ref",
        "created_seq",
    ] {
        anyhow::ensure!(
            obligation_after[field] == origin_projection[field],
            "settlement changed the immutable creation field {field}"
        );
    }
    let counts_after = partition_footprint(&store_dir);
    let cas_after = partition_cas(&store_dir);
    let exported = partition_export(&store, &archive);
    drop(subject);
    drop(store);
    // Reopen: the durable state, not the process, is what this receipt claims.
    let reopened = partition_open(&store_dir);
    reopened.start_subject_dispatcher().map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let reopened_settlements = partition_settlement_json(&partition_settlements(&reopened));
    anyhow::ensure!(
        reopened_settlements == partition_settlement_json(&settlements),
        "reopening changed the typed settlement"
    );
    let reopened_row = partition_obligation(&store_dir, obligation);
    anyhow::ensure!(reopened_row == obligation_after, "reopening changed the obligation projection");
    Ok(serde_json::json!({
        "mode": "settle",
        "peer": peer,
        "state": target_state,
        "started_at_ms": started_at_ms,
        "finished_at_ms": partition_now_ms(),
        "barrier": barrier,
        "executable": partition_executable(),
        "store": store_dir.to_string_lossy(),
        "agent_id": agent.to_string(),
        "obligation_id": obligation.to_string(),
        "origin_archive": origin,
        "origin_row": origin_row.clone(),
        "head_before": head_before,
        "settlement_step": {
            "step_id": step_id.to_string(),
            "step_no": settlement_step.step_no,
            "state": format!("{:?}", settlement_step.state),
            "input_plan_ref": settlement_step.input_plan_ref.as_ref().map(|object| object.digest.to_string()),
            "commit_seq": settlement_step.commit_ref.map(|commit| commit.seq.0),
        },
        "intent": intent,
        "receipt": serde_json::to_value(receipt)?,
        "settlements": reopened_settlements,
        "obligation_before": origin_row,
        "obligation_after": reopened_row,
        "counts_before": counts_before,
        "counts_after": counts_after,
        "cas_before": cas_before,
        "cas_after": cas_after,
        "archive": exported,
    }))
}

/// Import the competing peer's archive into a store that must refuse it as one
/// required unit, publishing nothing while refusing.
async fn partition_run_refuse(store_name: &str, scope: &str) -> anyhow::Result<()> {
    let started_at_ms = partition_now_ms();
    let receipt_path = partition_receipt_path();
    let result = partition_refuse_body(store_name, scope, started_at_ms);
    match result {
        Ok(value) => {
            partition_write_receipt(&receipt_path, &value);
            Ok(())
        }
        Err(error) => {
            partition_write_receipt(
                &receipt_path,
                &serde_json::json!({
                    "mode": "refuse",
                    "store": store_name,
                    "failed": true,
                    "error": format!("{error:#}"),
                    "started_at_ms": started_at_ms,
                    "finished_at_ms": partition_now_ms(),
                }),
            );
            Err(error)
        }
    }
}

fn partition_refuse_body(
    store_name: &str,
    scope: &str,
    started_at_ms: u64,
) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(partition_env(MODE_ENV) == "refuse", "wrong child mode");
    let root = PathBuf::from(partition_env(ROOT_ENV));
    let store_dir = root.join(store_name);
    let agent = partition_id(AGENT_ENV);
    let obligation = partition_id(OBLIGATION_ENV);
    let archive = PathBuf::from(partition_env(ARCHIVE_ENV));
    let incoming = partition_archive_identity(&archive)?;
    let expected_fingerprint = partition_env(EXPECT_FINGERPRINT_ENV);
    anyhow::ensure!(
        incoming["fingerprint"].as_str() == Some(expected_fingerprint.as_str()),
        "the incoming archive is not the one the orchestrator recorded"
    );
    let incoming_manifest = partition_manifest(&archive)?;
    let store = partition_open(&store_dir);
    let local_row_before = partition_obligation(&store_dir, obligation);
    let local_settlements_before = partition_settlement_json(&partition_settlements(&store));
    let footprint_before = partition_footprint(&store_dir);
    let cas_before = partition_cas(&store_dir);
    let error = match store.import_cognition_archive(
        std::fs::File::open(&archive)?,
        &partition_import_options(),
    ) {
        Ok(outcome) => anyhow::bail!(
            "a competing settlement was accepted: inserted={} updated={} conflicts={:?}",
            outcome.inserted,
            outcome.updated,
            outcome.conflicts
        ),
        Err(error) => format!("{error:#}"),
    };
    anyhow::ensure!(error.contains(UNIT_REFUSAL), "refused for another reason: {error}");
    let footprint_after = partition_footprint(&store_dir);
    let cas_after = partition_cas(&store_dir);
    anyhow::ensure!(
        footprint_before == footprint_after,
        "a refused archive changed local rows, baselines or the ledger"
    );
    anyhow::ensure!(
        partition_obligation(&store_dir, obligation) == local_row_before,
        "a refused archive changed the local obligation projection"
    );
    anyhow::ensure!(
        partition_settlement_json(&partition_settlements(&store))
            == local_settlements_before,
        "a refused archive changed the local typed settlement"
    );
    anyhow::ensure!(
        cas_after["objects"].as_u64() >= cas_before["objects"].as_u64(),
        "the content store lost objects during a refusal"
    );
    drop(store);
    let reopened = partition_open(&store_dir);
    reopened.start_subject_dispatcher().map_err(|error| anyhow::anyhow!("{error:#}"))?;
    anyhow::ensure!(
        partition_obligation(&store_dir, obligation) == local_row_before,
        "reopening the refused store changed the obligation projection"
    );
    anyhow::ensure!(
        partition_settlement_json(&partition_settlements(&reopened))
            == local_settlements_before,
        "reopening the refused store changed the typed settlement"
    );
    let local_row = partition_obligation(&store_dir, obligation);
    // The head rows are the other member of the unit rule that diverges here:
    // each peer advanced its own branch, so the rows name different successors.
    let local_manifest = partition_store_manifest(&reopened);
    let local_head = partition_first_row(&local_manifest, "subject_branch_heads");
    let incoming_head = partition_first_row(&incoming_manifest, "subject_branch_heads");
    Ok(serde_json::json!({
        "mode": "refuse",
        "store": store_name,
        "scope": scope,
        "started_at_ms": started_at_ms,
        "finished_at_ms": partition_now_ms(),
        "executable": partition_executable(),
        "store_dir": store_dir.to_string_lossy(),
        "agent_id": agent.to_string(),
        "obligation_id": obligation.to_string(),
        "archive": incoming,
        // Divergence evidence: the two rows the unit rule sees.
        "local_obligation": local_row,
        "incoming_obligation": partition_row(&incoming_manifest, "subject_obligations", &obligation.to_string()),
        "local_head": local_head,
        "incoming_head": incoming_head,
        "error": error,
        "expected_error": UNIT_REFUSAL,
        "footprint_before": footprint_before,
        "footprint_after": footprint_after,
        "cas_before": cas_before,
        "cas_after": cas_after,
        "import_order": partition_import_order(&store_dir),
        "local_settlements": local_settlements_before,
        "reopen_valid": true,
    }))
}

/// Receive the winning settlement into a fresh store, then attempt the
/// competing settlement locally. The attempt may legitimately be refused at S0
/// (a converged store re-derives its context); whatever happens, the applied
/// settlement and the projection must not change.
async fn partition_run_converge() -> anyhow::Result<()> {
    let started_at_ms = partition_now_ms();
    let receipt_path = partition_receipt_path();
    let result = partition_converge_body(started_at_ms).await;
    match result {
        Ok(value) => {
            partition_write_receipt(&receipt_path, &value);
            Ok(())
        }
        Err(error) => {
            partition_write_receipt(
                &receipt_path,
                &serde_json::json!({
                    "mode": "converge",
                    "failed": true,
                    "error": format!("{error:#}"),
                    "started_at_ms": started_at_ms,
                    "finished_at_ms": partition_now_ms(),
                }),
            );
            Err(error)
        }
    }
}

async fn partition_converge_body(started_at_ms: u64) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(partition_env(MODE_ENV) == "converge", "wrong child mode");
    let root = PathBuf::from(partition_env(ROOT_ENV));
    let store_dir = root.join("store-d");
    let agent = partition_id(AGENT_ENV);
    let obligation = partition_id(OBLIGATION_ENV);
    let archive = PathBuf::from(partition_env(ARCHIVE_ENV));
    let incoming = partition_archive_identity(&archive)?;
    let expected_fingerprint = partition_env(EXPECT_FINGERPRINT_ENV);
    anyhow::ensure!(
        incoming["fingerprint"].as_str() == Some(expected_fingerprint.as_str()),
        "the accepted archive is not the one the orchestrator recorded"
    );
    let incoming_manifest = partition_manifest(&archive)?;
    let store = partition_open(&store_dir);
    anyhow::ensure!(
        partition_footprint(&store_dir)["counts"]["subject_obligations"] == serde_json::json!(0),
        "the convergence store must start empty"
    );
    let outcome = store.import_cognition_archive(
        std::fs::File::open(&archive)?,
        &partition_import_options(),
    )?;
    anyhow::ensure!(
        outcome.ack(EntityId::new()).verified,
        "the settlement archive was not verified: conflicts={:?}",
        outcome.conflicts
    );
    anyhow::ensure!(outcome.conflicts.is_empty(), "a convergent import reported conflicts: {:?}", outcome.conflicts);
    let obligation_after_import = partition_obligation(&store_dir, obligation);
    anyhow::ensure!(
        obligation_after_import
            == partition_row(&incoming_manifest, "subject_obligations", &obligation.to_string())
                .map(|row| archive_row_as_projection(&row))
                .ok_or_else(|| anyhow::anyhow!("the archive declares no such obligation"))?,
        "the imported obligation projection differs from the archive"
    );
    let settlements_after_import = partition_settlement_json(&partition_settlements(&store));
    anyhow::ensure!(
        settlements_after_import == archive_settlements(&incoming_manifest),
        "the imported typed settlement differs from the archive"
    );
    let import_order = partition_import_order(&store_dir);
    anyhow::ensure!(
        import_order.as_array().map(Vec::len) == Some(1),
        "the accepted import recorded no durable order"
    );
    anyhow::ensure!(
        import_order[0]["fingerprint"] == incoming["fingerprint"],
        "the durable import order does not name the accepted archive"
    );
    drop(store);
    // The converged store now attempts the competing settlement for real.
    let store = partition_open(&store_dir);
    let attempt = {
        let mut subject = state(store.clone(), agent).map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let head = subject.head.committed_step_no;
        let next = subject.step.step_no;
        let next_state = format!("{:?}", subject.step.state);
        let outcome = partition_try_commit_output(
            &store,
            &mut subject,
            3,
            &[],
            &partition_terminal(obligation, "cancelled"),
        )
        .await;
        let recorded = match outcome {
            Ok(step_id) => serde_json::json!({
                "reached_s1": true,
                "step_id": step_id.to_string(),
                "error": serde_json::Value::Null,
            }),
            Err(error) => serde_json::json!({
                "reached_s1": false,
                "step_id": serde_json::Value::Null,
                "error": format!("{error:#}"),
            }),
        };
        // If the attempt was corrected instead of committed, the loop left a
        // sealed rejection bound to the retired step. Report it rather than
        // hiding why the competing settlement did not reach S1.
        let after = store.load_subject(agent).ok().map(|(head, step)| {
            let reason = step
                .retry_reason_ref
                .as_ref()
                .and_then(|object| store.cas.read_object(&object.digest).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            serde_json::json!({
                "committed_step_no": head.committed_step_no,
                "step_no": step.step_no,
                "step_id": step.step_id.to_string(),
                "state": format!("{:?}", step.state),
                "rejection": reason,
            })
        });
        serde_json::json!({
            "head_committed_step_no": head,
            "next_step_no": next,
            "next_step_state": next_state,
            "recorded": recorded,
            "after": after,
        })
    };
    let settlements_after_attempt = partition_settlements(&store);
    let obligation_after_attempt = partition_obligation(&store_dir, obligation);
    let typed_after_attempt = partition_settlement_json(&settlements_after_attempt);
    // Whatever the attempt did, the applied settlement is the imported one and
    // the projection is the state it applied.
    let applied: Vec<&(serde_json::Value, ObligationTerminalReceipt)> = settlements_after_attempt
        .iter()
        .filter(|(_, receipt)| matches!(&receipt.outcome, ObligationTerminalOutcome::Applied))
        .collect();
    anyhow::ensure!(applied.len() == 1, "the converged store must hold exactly one applied settlement");
    anyhow::ensure!(
        applied[0].0["intent_id"] == archive_settlements(&incoming_manifest)[0]["intent"]["intent_id"],
        "the applied settlement is not the one the archive delivered"
    );
    anyhow::ensure!(
        obligation_after_attempt == obligation_after_import,
        "a competing settlement changed the settled projection"
    );
    anyhow::ensure!(
        typed_after_attempt[0] == settlements_after_import[0],
        "a competing settlement replaced the applied typed receipt"
    );
    for (_, receipt) in &settlements_after_attempt {
        if matches!(&receipt.outcome, ObligationTerminalOutcome::Applied) {
            continue;
        }
        match &receipt.outcome {
            ObligationTerminalOutcome::Rejected { current_state, reason } => {
                anyhow::ensure!(
                    *current_state == ObligationTerminalState::Completed && !reason.trim().is_empty(),
                    "a rejected settlement must name the applied terminal state"
                );
            }
            ObligationTerminalOutcome::Applied => unreachable!(),
        }
    }
    let exported = partition_export(&store, &root.join("artifacts/store-d.archive"));
    drop(store);
    let reopened = partition_open(&store_dir);
    reopened.start_subject_dispatcher().map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let reopened_settlements = partition_settlement_json(&partition_settlements(&reopened));
    anyhow::ensure!(
        reopened_settlements == typed_after_attempt,
        "reopening the converged store changed the settlement set"
    );
    anyhow::ensure!(
        partition_obligation(&store_dir, obligation) == obligation_after_import,
        "reopening the converged store changed the settled projection"
    );
    Ok(serde_json::json!({
        "mode": "converge",
        "started_at_ms": started_at_ms,
        "finished_at_ms": partition_now_ms(),
        "executable": partition_executable(),
        "store_dir": store_dir.to_string_lossy(),
        "agent_id": agent.to_string(),
        "obligation_id": obligation.to_string(),
        "archive": incoming,
        "imported": {
            "inserted": outcome.inserted,
            "updated": outcome.updated,
            "unchanged": outcome.unchanged,
            "verified_objects": outcome.verified_objects,
            "fingerprint": outcome.fingerprint.to_string(),
        },
        "import_order": import_order,
        "settlements_after_import": settlements_after_import,
        "attempt": attempt,
        "settlements_after_attempt": reopened_settlements,
        "obligation": partition_obligation(&store_dir, obligation),
        "archive_out": exported,
        "reopen_valid": true,
    }))
}

/// Import the converged store's archive into a fresh store and check the typed
/// facts arrived unchanged after a real reopen.
async fn partition_run_relay() -> anyhow::Result<()> {
    let started_at_ms = partition_now_ms();
    let receipt_path = partition_receipt_path();
    let result = partition_relay_body(started_at_ms);
    match result {
        Ok(value) => {
            partition_write_receipt(&receipt_path, &value);
            Ok(())
        }
        Err(error) => {
            partition_write_receipt(
                &receipt_path,
                &serde_json::json!({
                    "mode": "relay",
                    "failed": true,
                    "error": format!("{error:#}"),
                    "started_at_ms": started_at_ms,
                    "finished_at_ms": partition_now_ms(),
                }),
            );
            Err(error)
        }
    }
}

fn partition_relay_body(started_at_ms: u64) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(partition_env(MODE_ENV) == "relay", "wrong child mode");
    let root = PathBuf::from(partition_env(ROOT_ENV));
    let store_dir = root.join("store-e");
    let obligation = partition_id(OBLIGATION_ENV);
    let archive = PathBuf::from(partition_env(ARCHIVE_ENV));
    let incoming = partition_archive_identity(&archive)?;
    let expected_fingerprint = partition_env(EXPECT_FINGERPRINT_ENV);
    anyhow::ensure!(
        incoming["fingerprint"].as_str() == Some(expected_fingerprint.as_str()),
        "the relayed archive is not the one the orchestrator recorded"
    );
    let incoming_manifest = partition_manifest(&archive)?;
    let store = partition_open(&store_dir);
    let outcome = store.import_cognition_archive(
        std::fs::File::open(&archive)?,
        &partition_import_options(),
    )?;
    anyhow::ensure!(
        outcome.ack(EntityId::new()).verified,
        "the relayed archive was not verified: conflicts={:?}",
        outcome.conflicts
    );
    let settlements = partition_settlement_json(&partition_settlements(&store));
    anyhow::ensure!(
        settlements == archive_settlements(&incoming_manifest),
        "the relayed typed settlement differs from the archive"
    );
    anyhow::ensure!(
        partition_obligation(&store_dir, obligation)
            == partition_row(&incoming_manifest, "subject_obligations", &obligation.to_string())
                .map(|row| archive_row_as_projection(&row))
                .ok_or_else(|| anyhow::anyhow!("the archive declares no such obligation"))?,
        "the relayed obligation projection differs from the archive"
    );
    drop(store);
    let reopened = partition_open(&store_dir);
    reopened.start_subject_dispatcher().map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let reopened_settlements = partition_settlement_json(&partition_settlements(&reopened));
    anyhow::ensure!(
        reopened_settlements == settlements,
        "reopening the relayed store changed the typed settlement"
    );
    Ok(serde_json::json!({
        "mode": "relay",
        "started_at_ms": started_at_ms,
        "finished_at_ms": partition_now_ms(),
        "executable": partition_executable(),
        "store_dir": store_dir.to_string_lossy(),
        "obligation_id": obligation.to_string(),
        "archive": incoming,
        "imported": {
            "inserted": outcome.inserted,
            "updated": outcome.updated,
            "unchanged": outcome.unchanged,
            "fingerprint": outcome.fingerprint.to_string(),
        },
        "settlements": reopened_settlements,
        "obligation": partition_obligation(&store_dir, obligation),
        "import_order": partition_import_order(&store_dir),
        "reopen_valid": true,
    }))
}

/// The settlement tuples an archive declares, in the same shape the store
/// reports, so a store and an archive can be compared directly.
fn archive_settlements(manifest: &serde_json::Value) -> serde_json::Value {
    let mut out = Vec::new();
    for row in manifest["rows"].as_array().into_iter().flatten() {
        if row["table"] != serde_json::json!("subject_intents") {
            continue;
        }
        let values = row["values"].as_array().cloned().unwrap_or_default();
        let field = |name: &str| {
            values
                .iter()
                .find(|value| value[0] == serde_json::json!(name))
                .and_then(|value| value[1].clone())
        };
        if field("contract") != Some(serde_json::json!(TERMINAL_CONTRACT)) {
            continue;
        }
        let receipt = field("result_ref")
            .and_then(|text| text.as_str().and_then(|text| serde_json::from_str(text).ok()));
        out.push(serde_json::json!({
            "intent": {
                "intent_id": row["key"],
                "step_id": field("step_id"),
                "kind": field("kind"),
                "contract": field("contract"),
                "state": field("state"),
                "revision": field("revision").and_then(|value| value.as_u64().or_else(|| value.as_str().and_then(|text| text.parse().ok()))),
            },
            "receipt": receipt,
        }));
    }
    serde_json::Value::Array(out)
}

/// Turn one archive projection row into the projection shape this module
/// reads from SQLite, so a store row and an archive row are comparable.
fn archive_row_as_projection(row: &serde_json::Value) -> serde_json::Value {
    let values = row["values"].as_array().cloned().unwrap_or_default();
    let field = |name: &str| {
        values
            .iter()
            .find(|value| value[0] == serde_json::json!(name))
            .and_then(|value| value[1].clone())
            .unwrap_or(serde_json::json!(""))
    };
    let text = |name: &str| field(name).as_str().map(str::to_owned).unwrap_or_default();
    let seq = |name: &str| {
        field(name)
            .as_i64()
            .map(|number| number.to_string())
            .or_else(|| field(name).as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    serde_json::json!({
        "obligation_id": row["key"],
        "agent_id": text("agent_id"),
        "step_id": text("step_id"),
        "origin_event_id": text("origin_event_id"),
        "description": text("description"),
        "depends_on_task_id": text("depends_on_task_id"),
        "resume_condition": text("resume_condition"),
        "due_at": text("due_at"),
        "decision_ref": text("decision_ref"),
        "created_seq": seq("created_seq"),
        "state": text("state"),
        "updated_seq": seq("updated_seq"),
    })
}

// ---------------------------------------------------------------------------
// Orchestrator
// ---------------------------------------------------------------------------

struct PartitionRun {
    root: PathBuf,
    started_at_ms: u64,
    checks: Vec<serde_json::Value>,
    children: Vec<serde_json::Value>,
    evidence: serde_json::Map<String, serde_json::Value>,
}

impl PartitionRun {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            started_at_ms: partition_now_ms(),
            checks: Vec::new(),
            children: Vec::new(),
            evidence: serde_json::Map::new(),
        }
    }

    fn check(&mut self, name: &str, passed: bool, detail: serde_json::Value) {
        self.checks.push(serde_json::json!({ "check": name, "passed": passed, "detail": detail }));
    }

    fn child(&self, name: &str) -> PathBuf {
        self.root.join("receipts").join(format!("{name}.json"))
    }

    fn child_receipt(&self, name: &str) -> serde_json::Value {
        let path = self.child(name);
        std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or(serde_json::Value::Null)
    }

    fn finish(self) {
        let failed: Vec<String> = self
            .checks
            .iter()
            .filter(|check| check["passed"] != serde_json::json!(true))
            .map(|check| check["check"].as_str().unwrap_or("<unnamed>").to_owned())
            .collect();
        let receipt = serde_json::json!({
            "state": if failed.is_empty() { "passed" } else { "failed" },
            "scope": concat!(
                "Source-level obligation partition settlement proof: real parser, Store, CAS and archive code, ",
                "two independent OS processes settling from one committed origin with a rendezvous barrier, ",
                "archive exchange, unit refusal, convergence, sticky terminal state, relay and reopen. ",
                "Not a power-loss, distributed-installation, network, or model-efficacy proof; no provider request is made."
            ),
            "root": self.root.to_string_lossy(),
            "started_at_ms": self.started_at_ms,
            "finished_at_ms": partition_now_ms(),
            "executable": partition_executable(),
            "checks": self.checks,
            "failed_checks": failed,
            "children": self.children,
            "evidence": self.evidence,
            "limits": [
                "peers are byte copies of one quiesced store, so they share the origin ledger identity and incarnation",
                "the content store may gain unreferenced sealed objects during a refused import; no fact is published",
                "no model, provider or network is involved; the authored control text is parsed by the real parser",
            ],
        });
        let path = self.root.join("receipt.json");
        partition_write_receipt(&path, &receipt);
        println!("{}", serde_json::to_string(&receipt).unwrap_or_else(|_| "{}".into()));
        assert!(
            receipt["failed_checks"].as_array().map(Vec::len) == Some(0),
            "obligation partition proof failed: {}",
            receipt["failed_checks"]
        );
    }
}

#[ignore = "requires SHENKO_OBLIGATION_PARTITION_ROOT: real OS-process obligation partition proof"]
#[tokio::test]
async fn partition_orchestrator() {
    let root = PathBuf::from(partition_env(ROOT_ENV));
    let mut run = PartitionRun::new(root);
    if let Err(error) = partition_prepare_root(&mut run) {
        run.check("fresh_root", false, serde_json::json!({ "error": format!("{error:#}") }));
        run.finish();
        return;
    }
    if let Err(error) = partition_orchestrate(&mut run).await {
        run.check("orchestration", false, serde_json::json!({ "error": format!("{error:#}") }));
    }
    run.finish();
}

fn partition_prepare_root(run: &mut PartitionRun) -> anyhow::Result<()> {
    if run.root.exists() {
        let mut entries = std::fs::read_dir(&run.root)?;
        anyhow::ensure!(
            entries.next().is_none(),
            "{} is not a fresh root: it already contains entries; use a new durable root per run",
            run.root.display()
        );
    } else {
        std::fs::create_dir_all(&run.root)?;
    }
    run.check(
        "fresh_root",
        true,
        serde_json::json!({ "root": run.root.to_string_lossy() }),
    );
    Ok(())
}

async fn partition_orchestrate(run: &mut PartitionRun) -> anyhow::Result<()> {
    let origin_dir = run.root.join("origin");
    let agent = EntityId::new();
    // 1. The genuine committed origin: admissible input, real S0/S1 promise.
    let store = partition_genesis(&origin_dir, agent);
    let origin_event = message("the input this promise answers");
    partition_admit(&store, agent, &origin_event);
    let mut subject = state(store.clone(), agent).map_err(|error| anyhow::anyhow!("{error:#}"))?;
    let control = format!(
        "[obligation] {}",
        serde_json::json!({
            "description": "a promise two partitioned peers will settle",
            "origin_event_id": origin_event.event_id,
            "resume_condition": "the peer partition heals",
        })
    );
    commit_output(&store, &mut subject, 1, &[origin_event.clone()], &control).await;
    let open = store.open_subject_obligations(agent).unwrap();
    anyhow::ensure!(open.len() == 1, "the origin must hold exactly one open obligation");
    let obligation = open[0].obligation_id;
    let origin_row = partition_obligation(&origin_dir, obligation);
    let origin_projection = origin_row.clone();
    anyhow::ensure!(
        origin_row["state"] == serde_json::json!("open"),
        "the origin obligation is not open"
    );
    anyhow::ensure!(
        partition_settlements(&store).is_empty(),
        "the origin already carries a settlement"
    );
    let origin_archive = run.root.join("artifacts/origin.archive");
    let origin_export = partition_export(&store, &origin_archive);
    let origin_counts = partition_footprint(&origin_dir);
    drop(subject);
    drop(store);
    run.evidence.insert("origin".into(), serde_json::json!({
        "store": origin_dir.to_string_lossy(),
        "agent_id": agent.to_string(),
        "obligation_id": obligation.to_string(),
        "origin_event_id": origin_event.event_id.to_string(),
        "obligation": origin_row.clone(),
        "archive": origin_export.clone(),
        "counts": origin_counts,
    }));
    run.check(
        "origin_commits_one_open_obligation",
        origin_projection["state"] == serde_json::json!("open")
            && origin_export["identity"]["schema"] == serde_json::json!(6)
            && origin_export["receipt"]["complete"] == serde_json::json!(true),
        serde_json::json!({ "obligation": origin_projection, "archive": origin_export["identity"] }),
    );
    let origin_identity = origin_export["identity"].clone();
    let origin_fingerprint = origin_identity["fingerprint"].as_str().unwrap_or_default().to_owned();
    let origin_bytes = origin_identity["bytes"].as_u64().unwrap_or_default().to_string();

    // 2. Fork the quiesced origin into two partitioned peers.
    for peer in ["peer-a", "peer-b"] {
        partition_copy_tree(&origin_dir, &run.root.join(peer))?;
    }
    run.check(
        "peers_fork_one_origin",
        partition_obligation(&run.root.join("peer-a"), obligation) == origin_projection
            && partition_obligation(&run.root.join("peer-b"), obligation) == origin_projection,
        serde_json::json!({ "obligation": origin_projection }),
    );

    // 3. Two independent OS processes settle in overlapping lifetimes.
    let (child_a, start_a) = partition_start(CHILD_SETTLE_A, vec![
        (MODE_ENV, "settle".to_string()),
        (PEER_ENV, "a".to_string()),
        (STORE_ENV, run.root.join("peer-a").to_string_lossy().to_string()),
        (AGENT_ENV, agent.to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (STATE_ENV, "completed".to_string()),
        (ARCHIVE_ENV, run.root.join("artifacts/peer-a.archive").to_string_lossy().to_string()),
        (ORIGIN_ARCHIVE_ENV, origin_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, origin_fingerprint.clone()),
        (EXPECT_BYTES_ENV, origin_bytes.clone()),
        (RENDEZVOUS_ENV, "b".to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("settle-a").to_string_lossy().to_string()),
    ])?;
    let (child_b, start_b) = partition_start(CHILD_SETTLE_B, vec![
        (MODE_ENV, "settle".to_string()),
        (PEER_ENV, "b".to_string()),
        (STORE_ENV, run.root.join("peer-b").to_string_lossy().to_string()),
        (AGENT_ENV, agent.to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (STATE_ENV, "cancelled".to_string()),
        (ARCHIVE_ENV, run.root.join("artifacts/peer-b.archive").to_string_lossy().to_string()),
        (ORIGIN_ARCHIVE_ENV, origin_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, origin_fingerprint.clone()),
        (EXPECT_BYTES_ENV, origin_bytes.clone()),
        (RENDEZVOUS_ENV, "a".to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("settle-b").to_string_lossy().to_string()),
    ])?;
    let settle_a = partition_collect("settle_a", child_a, start_a, &run.child("settle-a"))?;
    let settle_b = partition_collect("settle_b", child_b, start_b, &run.child("settle-b"))?;
    run.children.push(settle_a.clone());
    run.children.push(settle_b.clone());
    let a = run.child_receipt("settle-a");
    let b = run.child_receipt("settle-b");
    run.check(
        "peer_a_settles_completed",
        settle_a["success"] == serde_json::json!(true)
            && a["state"] == serde_json::json!("completed")
            && a["obligation_after"]["state"] == serde_json::json!("completed")
            && a["receipt"]["outcome"]["outcome"] == serde_json::json!("applied"),
        serde_json::json!({ "child": settle_a, "receipt": a["settlement_step"] }),
    );
    run.check(
        "peer_b_settles_cancelled",
        settle_b["success"] == serde_json::json!(true)
            && b["state"] == serde_json::json!("cancelled")
            && b["obligation_after"]["state"] == serde_json::json!("cancelled")
            && b["receipt"]["outcome"]["outcome"] == serde_json::json!("applied"),
        serde_json::json!({ "child": settle_b, "receipt": b["settlement_step"] }),
    );
    let a_started = a["started_at_ms"].as_u64().unwrap_or_default();
    let a_finished = a["finished_at_ms"].as_u64().unwrap_or_default();
    let b_started = b["started_at_ms"].as_u64().unwrap_or_default();
    let b_finished = b["finished_at_ms"].as_u64().unwrap_or_default();
    run.check(
        "settlements_overlap_in_process_lifetimes",
        a_started < b_finished && b_started < a_finished,
        serde_json::json!({
            "peer_a": [a_started, a_finished],
            "peer_b": [b_started, b_finished],
            "barriers": [a["barrier"], b["barrier"]],
        }),
    );
    run.check(
        "peers_compete_on_one_obligation",
        a["obligation_id"] == b["obligation_id"]
            && a["obligation_after"]["state"] != b["obligation_after"]["state"]
            && a["settlements"].as_array().map(Vec::len) == Some(1)
            && b["settlements"].as_array().map(Vec::len) == Some(1)
            && a["counts_after"]["counts"]["cognition_import_order"] == serde_json::json!(0)
            && b["counts_after"]["counts"]["cognition_import_order"] == serde_json::json!(0),
        serde_json::json!({
            "peer_a": a["obligation_after"],
            "peer_b": b["obligation_after"],
        }),
    );
    for (name, receipt) in [("peer-a", &a), ("peer-b", &b)] {
        run.check(
            &format!("{name}_archive_is_genuine_schema6"),
            receipt["archive"]["identity"]["schema"] == serde_json::json!(6)
                && receipt["archive"]["receipt"]["complete"] == serde_json::json!(true)
                && receipt["archive"]["identity"]["row_counts"]["subject_obligations"] == serde_json::json!(1),
            receipt["archive"]["identity"].clone(),
        );
    }

    // 4. Both exchanges refuse the competing settlement as one required unit.
    let a_archive = run.root.join("artifacts/peer-a.archive");
    let b_archive = run.root.join("artifacts/peer-b.archive");
    let a_identity = partition_archive_identity(&a_archive)?;
    let b_identity = partition_archive_identity(&b_archive)?;
    let (refuse_child, refuse_start) = partition_start(CHILD_REFUSE_B, vec![
        (MODE_ENV, "refuse".to_string()),
        (STORE_ENV, run.root.join("peer-a").to_string_lossy().to_string()),
        (AGENT_ENV, agent.to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (ARCHIVE_ENV, b_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, b_identity["fingerprint"].as_str().unwrap_or_default().to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("refuse-b-into-a").to_string_lossy().to_string()),
    ])?;
    let refused_into_a =
        partition_collect("refuse_b_into_a", refuse_child, refuse_start, &run.child("refuse-b-into-a"))?;
    run.children.push(refused_into_a.clone());
    let into_a = run.child_receipt("refuse-b-into-a");
    run.check(
        "competing_settlement_refused_as_one_unit",
        refused_into_a["success"] == serde_json::json!(true)
            && into_a["error"].as_str().is_some_and(|error| error.contains(UNIT_REFUSAL))
            && into_a["local_obligation"] == a["obligation_after"]
            && into_a["incoming_obligation"]
                .as_object()
                .is_some_and(|row| row.get("values").is_some())
            && into_a["local_head"] != into_a["incoming_head"],
        serde_json::json!({
            "child": refused_into_a,
            "local": into_a["local_obligation"],
            "incoming": into_a["incoming_obligation"],
        }),
    );
    run.check(
        "refusal_publishes_no_rows_in_a",
        into_a["footprint_before"] == into_a["footprint_after"]
            && into_a["import_order"].as_array().map(Vec::len) == Some(0)
            && into_a["local_settlements"].as_array().map(Vec::len) == Some(1),
        serde_json::json!({
            "footprint": into_a["footprint_before"],
            "cas_before": into_a["cas_before"],
            "cas_after": into_a["cas_after"],
        }),
    );

    // 5. A fresh store accepts the winning settlement and then tries the
    //    competing one for real.
    let (converge_child, converge_start) = partition_start(CHILD_CONVERGE, vec![
        (MODE_ENV, "converge".to_string()),
        (AGENT_ENV, agent.to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (ARCHIVE_ENV, a_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, a_identity["fingerprint"].as_str().unwrap_or_default().to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("converge-store-d").to_string_lossy().to_string()),
    ])?;
    let converged = partition_collect("converge_store_d", converge_child, converge_start, &run.child("converge-store-d"))?;
    run.children.push(converged.clone());
    let d = run.child_receipt("converge-store-d");
    run.check(
        "converged_store_accepts_the_winning_settlement",
        converged["success"] == serde_json::json!(true)
            && d["settlements_after_import"][0]["receipt"] == a["receipt"]
            && d["import_order"].as_array().map(Vec::len) == Some(1)
            && d["obligation"]["state"] == serde_json::json!("completed"),
        serde_json::json!({
            "child": converged,
            "imported": d["imported"],
            "import_order": d["import_order"],
            "settlements": d["settlements_after_import"],
        }),
    );
    run.check(
        "competing_settlement_after_convergence_never_replaces",
        d["obligation"]["state"] == serde_json::json!("completed")
            && d["settlements_after_attempt"]
                .as_array()
                .is_some_and(|settlements| {
                    settlements
                        .iter()
                        .filter(|settlement| settlement["receipt"]["outcome"]["outcome"] == serde_json::json!("applied"))
                        .count()
                        == 1
                        && settlements[0]["receipt"] == a["receipt"]
                }),
        serde_json::json!({
            "attempt": d["attempt"],
            "settlements": d["settlements_after_attempt"],
            "obligation": d["obligation"],
        }),
    );

    // 6. A store that already holds replicated facts refuses the competing
    //    settlement without advancing its baselines or import order.
    let d_archive = run.root.join("artifacts/store-d.archive");
    let d_identity = partition_archive_identity(&d_archive)?;
    let (refuse_d_child, refuse_d_start) = partition_start(CHILD_REFUSE_D, vec![
        (MODE_ENV, "refuse".to_string()),
        (STORE_ENV, run.root.join("store-d").to_string_lossy().to_string()),
        (AGENT_ENV, agent.to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (ARCHIVE_ENV, b_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, b_identity["fingerprint"].as_str().unwrap_or_default().to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("refuse-b-into-d").to_string_lossy().to_string()),
    ])?;
    let refused_into_d =
        partition_collect("refuse_b_into_d", refuse_d_child, refuse_d_start, &run.child("refuse-b-into-d"))?;
    run.children.push(refused_into_d.clone());
    let into_d = run.child_receipt("refuse-b-into-d");
    run.check(
        "competing_settlement_refused_over_replicated_facts",
        refused_into_d["success"] == serde_json::json!(true)
            && into_d["error"].as_str().is_some_and(|error| error.contains(UNIT_REFUSAL))
            && into_d["footprint_before"] == into_d["footprint_after"]
            && into_d["footprint_before"]["counts"]["cognition_baselines"].as_i64().unwrap_or(0) > 0
            && into_d["import_order"].as_array().map(Vec::len) == Some(1)
            && into_d["local_obligation"]["state"] == serde_json::json!("completed"),
        serde_json::json!({
            "child": refused_into_d,
            "footprint": into_d["footprint_before"],
            "import_order": into_d["import_order"],
        }),
    );

    // 7. Relay the converged store's archive into a fresh store: the typed
    //    facts survive a real reopen of every hop.
    let (relay_child, relay_start) = partition_start(CHILD_RELAY, vec![
        (MODE_ENV, "relay".to_string()),
        (OBLIGATION_ENV, obligation.to_string()),
        (ARCHIVE_ENV, d_archive.to_string_lossy().to_string()),
        (EXPECT_FINGERPRINT_ENV, d_identity["fingerprint"].as_str().unwrap_or_default().to_string()),
        (ROOT_ENV, run.root.to_string_lossy().to_string()),
        (RECEIPT_ENV, run.child("relay-store-e").to_string_lossy().to_string()),
    ])?;
    let relayed = partition_collect("relay_store_e", relay_child, relay_start, &run.child("relay-store-e"))?;
    run.children.push(relayed.clone());
    let e = run.child_receipt("relay-store-e");
    run.check(
        "relay_preserves_typed_settlements",
        relayed["success"] == serde_json::json!(true)
            && e["settlements"] == d["settlements_after_attempt"]
            && e["obligation"] == d["obligation"]
            && e["import_order"].as_array().map(Vec::len) == Some(1),
        serde_json::json!({
            "child": relayed,
            "settlements": e["settlements"],
            "import_order": e["import_order"],
        }),
    );

    // 8. The archives this run produced were never rewritten: the identity the
    //    orchestrator recorded is the identity every consumer observed.
    run.check(
        "archives_are_byte_stable",
        partition_archive_identity(&origin_archive)?["bytes"] == origin_identity["bytes"]
            && partition_archive_identity(&a_archive)?["fingerprint"] == a_identity["fingerprint"]
            && partition_archive_identity(&b_archive)?["fingerprint"] == b_identity["fingerprint"]
            && partition_archive_identity(&d_archive)?["fingerprint"] == d_identity["fingerprint"],
        serde_json::json!({
            "origin": origin_identity["bytes"],
            "peer_a": a_identity["fingerprint"],
            "peer_b": b_identity["fingerprint"],
            "store_d": d_identity["fingerprint"],
        }),
    );

    // 9. Every store still reopens and still validates its obligations.
    let mut reopened = Vec::new();
    for name in ["origin", "peer-a", "peer-b", "store-d", "store-e"] {
        let dir = run.root.join(name);
        let store = partition_open(&dir);
        store.start_subject_dispatcher().map_err(|error| anyhow::anyhow!("{error:#}"))?;
        reopened.push(serde_json::json!({
            "store": name,
            "obligation": partition_obligation(&dir, obligation),
            "settlements": partition_settlement_json(&partition_settlements(&store)),
            "counts": partition_footprint(&dir)["counts"],
        }));
    }
    let distinct_states: Vec<&str> = reopened
        .iter()
        .filter_map(|store| store["obligation"]["state"].as_str())
        .collect();
    run.check(
        "every_store_reopens_and_validates",
        reopened.len() == 5
            && distinct_states.iter().filter(|state| **state == "open").count() == 1
            && distinct_states.iter().filter(|state| **state == "completed").count() == 3
            && distinct_states.iter().filter(|state| **state == "cancelled").count() == 1,
        serde_json::json!({ "stores": reopened.clone() }),
    );
    run.evidence.insert("reopened".into(), serde_json::json!(reopened));
    run.evidence.insert("peer_a".into(), a.clone());
    run.evidence.insert("peer_b".into(), b.clone());
    run.evidence.insert("store_d".into(), d.clone());
    run.evidence.insert("store_e".into(), e.clone());
    Ok(())
}
