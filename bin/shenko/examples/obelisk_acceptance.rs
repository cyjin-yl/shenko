//! One real-model comparison through the production subject sandbox.
//!
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<base URL without /v1>
//! SHENKO_LOCAL_MODEL=<served model> SHENKO_OBELISK_CONFIG=<compatible config JSON>
//! SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS=<verified deployment context limit>
//! cargo run -p shenko --example obelisk_acceptance
//!
//! The config supplies only the independently installed, source-pinned CLI.
//! Every provider root is replaced with this test's synthetic corpus. The
//! baseline gets the same messages through native history.query; the candidate
//! must discover them through a real history.external.query Obelisk call.
//! No answer or retrieved evidence is inserted into either subject prompt.
//! This is a bounded production-path smoke comparison, not a statistical
//! benchmark or an unconstrained local-model capability evaluation. Actual
//! model requests, token budgets, finish reasons, T1/E1/T2 and judge receipts
//! remain in each sandbox. An unavailable provider is never scored as success.
//! Immutable arm bindings and phase records survive an interrupted run. Read
//! them without opening a Store, CLI, broker or provider with:
//! obelisk_acceptance --report-only <existing run directory>
//! This prints a reconstruction; it never replaces the original reports.

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko::obelisk::{self, HistoryCommand, ObeliskCommand};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::{
    broker::{Broker, BrokerHandle},
    request::ProviderCapabilities,
};
use shenko_compact_rollover::{NotesRolloverPlugin, RolloverConfig, RolloverHosts};
use shenko_context_policy::{
    HistorySource, PolicyRegistry, StoreHistory, StoreNotesJournal, WindowEntry, estimate_tokens,
};
use shenko_exec_host::{
    CommittedTrialOutput, Condition, HistoryHandler, JudgeReceiptIdentity, JudgeVerdict, PreparedSandbox,
    SandboxJudgeContext, SandboxJudgement, SandboxRuntime, judge_sandbox_output,
    UnresolvedSandboxObservation,
    obelisk_runtime::{ObeliskConfig, ObeliskProvider, ObeliskRunStatus, ObeliskRuntime, ObeliskSourceRoot},
    run_prepared_sandbox,
};
use shenko_kernel::{Supervisor, task::NativeTaskHandler};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::{NativeTaskHandlerPlugin, SubjectLoopState, input_model_caps};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    task::{NativeTaskDefinition, TaskOutcome, TaskResult},
};
use std::{
    collections::BTreeSet,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const CORPUS: &[u8] = include_bytes!("../tests/fixtures/obelisk_auth_history.json");
const SCENARIO: &str = "auth_history_candidate_and_implemented_change";
const MESSAGE: &str = "上次 Hydra auth refresh 并发问题最后改了哪些文件，为什么这么改？请用本次可用的历史检索能力核实，给出可回查的来源，并区分候选方案与已实施的修改。";
const RUBRIC: &str = "回答是否根据实际读取的历史指出最终改动的两个文件与 generation 防止旧响应覆盖新会话的原因，区分未实施的 SameSite None 候选方案与保持 Lax 的最终决定，并给出能对应本次检索结果的来源？对‘测试通过’等事实应归因于历史记录，不把历史里的说法伪装成本次重新运行代码的证据。没有成功检索、缺乏来源或把候选方案说成最终修改均判 NOT_EFFECTIVE。";

#[derive(Clone, Serialize, Deserialize)]
struct Message {
    session_id: String,
    timestamp: String,
    role: String,
    text: String,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    write_bytes(path, &serde_json::to_vec_pretty(value)?)
}

fn create_evidence_dir(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) => ensure!(metadata.is_dir(), "evidence parent is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            create_evidence_dir(parent)?;
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(path)?;
            std::fs::File::open(parent)?.sync_all()?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

// Evidence is append-only. Sync the bytes and directory entry before allowing
// the next phase to issue work; a failed write cannot truncate an earlier fact.
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("evidence path has no parent")?;
    let temporary = parent.join(format!(".evidence-{}", EntityId::new()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&temporary, path)?;
        std::fs::remove_file(&temporary)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("retain {}", path.display()))
}

fn read_optional_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)
            .with_context(|| format!("decode retained {}", path.display()))?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn condition_name(candidate: bool) -> &'static str {
    if candidate { "candidate" } else { "baseline" }
}

fn provider_identity(handle: &BrokerHandle) -> Result<Value> {
    let binding = handle.binding();
    Ok(json!({
        "profile_id":binding.profile_id(), "profile_revision":binding.profile_revision(),
        "model":binding.model(), "binding_digest":Digest::blake3(&serde_json::to_vec(binding)?),
        "broker_default_max_tokens":binding.default_max_tokens(),
        "local_request_timeout_ms":binding.local_request_timeout_ms(),
        "sensitive_values":"endpoint, authentication, profile configuration and environment omitted",
    }))
}

#[derive(Deserialize)]
struct HistoryEvidence {
    schema_version: u32,
    status: String,
    op: String,
    #[serde(default)]
    events: Vec<HistoryRecord>,
    event: Option<HistoryRecord>,
}

#[derive(Deserialize, Serialize)]
struct HistoryRecord {
    event_id: EntityId,
    session_id: EntityId,
    seq: i64,
    event_ref: String,
    data_preview: Option<std::string::String>,
    data: Option<std::string::String>,
}

#[derive(Deserialize)]
struct CliResult {
    observation: CliResultBody,
}

#[derive(Deserialize)]
struct CliResultBody {
    result: CliObservation,
}

#[derive(Deserialize)]
struct CliObservation {
    schema_version: u32,
    status: ObeliskRunStatus,
    source_instance: String,
    data: Value,
    process: CliProcess,
}

#[derive(Deserialize)]
struct CliProcess {
    started: bool,
    reaped: bool,
    readers_joined: bool,
    readers_drained: bool,
    exit_code: Option<i32>,
    termination_requested: bool,
    stdout_truncated: bool,
    stderr_truncated: bool,
    custom_script_host_authority: bool,
}

#[derive(Deserialize, Serialize)]
struct CliMessage {
    uuid: String,
    session_id: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct FtsHit {
    message: FtsMessage,
    session: FtsSession,
}

#[derive(Deserialize, Serialize)]
struct FtsMessage {
    uuid: String,
    text: String,
}

#[derive(Deserialize, Serialize)]
struct FtsSession {
    id: String,
}

#[derive(Deserialize, Serialize)]
struct RawPage {
    status: String,
    message_uuid: String,
    text: String,
    offset: usize,
    offset_unit: String,
}

fn has_text(text: &str) -> bool { !text.trim().is_empty() }

// This checks returned evidence structure and corpus identity, never answer
// words. The independent judge still decides whether the answer used it well.
fn reader_evidence(
    definition: &NativeTaskDefinition,
    outcome: &TaskOutcome,
    returned: Option<&Value>,
    source_facts: &[Value],
) -> Result<Vec<Value>> {
    if !matches!(outcome, TaskOutcome::Succeeded) || definition.definition_version != 1 {
        return Ok(Vec::new());
    }
    let returned = returned.context("successful reader has no returned value")?;
    if definition.kind == "history.query" {
        let page: HistoryEvidence = serde_json::from_value(returned.clone())?;
        if page.schema_version != 1 || page.status != "ok"
            || !matches!(page.op.as_str(), "search" | "list" | "read")
            || definition.input["op"].as_str() != Some(page.op.as_str()) {
            return Ok(Vec::new());
        }
        let records = if page.op == "read" {
            page.event.into_iter().collect()
        } else {
            page.events
        };
        return records.into_iter().filter(|record| {
            has_text(&record.event_ref)
                && source_facts.iter().any(|fact| fact["event_id"] == json!(record.event_id)
                    && fact["session_id"] == json!(record.session_id) && fact["seq"] == record.seq)
                && if page.op == "read" {
                    record.data.as_deref().and_then(|data| serde_json::from_str::<Value>(data).ok())
                        .is_some_and(|data| data["text"].as_str().is_some_and(has_text))
                } else {
                    record.data_preview.as_deref().is_some_and(has_text)
                }
        }).map(|record| Ok(serde_json::to_value(record)?)).collect();
    }
    if definition.kind != obelisk::TASK_KIND || definition.input["op"] != "obelisk" {
        return Ok(Vec::new());
    }
    let observation = serde_json::from_value::<CliResult>(returned.clone())?.observation.result;
    let process = &observation.process;
    if observation.schema_version != 1 || observation.status != ObeliskRunStatus::Succeeded
        || observation.source_instance != "obelisk-acceptance/synthetic-v1"
        || !process.started || !process.reaped || !process.readers_joined || !process.readers_drained
        || process.exit_code != Some(0) || process.termination_requested
        || process.stdout_truncated || process.stderr_truncated || process.custom_script_host_authority {
        return Ok(Vec::new());
    }
    let known_session = |id: &str| source_facts.iter().any(|fact| fact["obelisk_session_id"] == id);
    let request = &definition.input["request"];
    let data = observation.data;
    match request["op"].as_str() {
        Some("search") if data["mode"] == "fts5" => {
            let hits: Vec<FtsHit> = serde_json::from_value(data["hits"].clone())?;
            hits.into_iter().filter(|hit| known_session(&hit.session.id)
                && has_text(&hit.message.uuid) && has_text(&hit.message.text))
                .map(|hit| Ok(serde_json::to_value(hit)?)).collect()
        }
        Some("search") if data["mode"] == "literal_index_fallback" => {
            let hits: Vec<CliMessage> = serde_json::from_value(data["hits"].clone())?;
            hits.into_iter().filter(|hit| known_session(&hit.session_id)
                && has_text(&hit.uuid) && has_text(&hit.text))
                .map(|hit| Ok(serde_json::to_value(hit)?)).collect()
        }
        Some("messages") => {
            let messages: Vec<CliMessage> = serde_json::from_value(data["messages"].clone())?;
            messages.into_iter().filter(|message| known_session(&message.session_id)
                && request["session_id"] == message.session_id
                && has_text(&message.uuid) && has_text(&message.text))
                .map(|message| Ok(serde_json::to_value(message)?)).collect()
        }
        Some("raw_page") => {
            let page: RawPage = serde_json::from_value(data)?;
            Ok(if page.status == "available" && has_text(&page.message_uuid) && has_text(&page.text)
                && request["message_uuid"] == page.message_uuid
                && page.offset as u64 == request["offset_utf16"].as_u64().unwrap_or(0)
                && page.offset_unit == "utf16_code_units" {
                vec![serde_json::to_value(page)?]
            } else { Vec::new() })
        }
        // Directory metadata, arbitrary script output, errors and outlines are
        // retained, but do not attest that historical message text was read.
        _ => Ok(Vec::new()),
    }
}

fn report_summary(arms: Vec<Value>, cleanup_ok: bool) -> Value {
    let complete = arms.len() == 2 && arms.iter().all(|arm| arm["status"] == "scored");
    let passed = complete && cleanup_ok && arms.iter().all(|arm|
        arm["effective"] == true && arm["used_expected_reader"] == true);
    json!({"schema_version":2,
        "status":if passed { "passed_smoke" } else { "inconclusive_or_failed" },
        "scenario":SCENARIO, "planned_arms":2, "complete":complete, "cleanup_ok":cleanup_ok,
        "corpus_digest":Digest::blake3(CORPUS),
        "evaluation_scope":"one bounded production-path comparison; no efficacy or token-saving claim",
        "arms":arms})
}

// Only reads retained JSON. Missing phases remain unknown, and a judge record
// without a terminal arm/cleanup record cannot promote an interrupted sample.
fn reconstruct_report(root: &Path) -> Result<Value> {
    let run_bytes = std::fs::read(root.join("run-binding.json"))
        .context("existing run has no readable durable binding")?;
    let run: Value = serde_json::from_slice(&run_bytes)?;
    ensure!(run["schema_version"] == 2, "unsupported retained run schema");
    let fixture = std::fs::read(root.join("fixture.json"))?;
    ensure!(run["fixture_digest"] == json!(Digest::blake3(&fixture))
        && run["fixture_bytes"].as_u64() == Some(fixture.len() as u64),
        "retained fixture differs from the original run binding");
    let run_digest = Digest::blake3(&run_bytes);
    let mut arms = Vec::new();
    for candidate in [false, true] {
        let arm_root = root.join(condition_name(candidate));
        let binding = read_optional_json(&arm_root.join("arm-binding.json"))?;
        let terminal = read_optional_json(&arm_root.join("acceptance.json"))?
            .or(read_optional_json(&arm_root.join("failure.json"))?);
        let mut arm = terminal.unwrap_or_else(|| json!({
            "condition":condition_name(candidate), "status":"incomplete",
            "effective":null, "used_expected_reader":false,
        }));
        arm["binding"] = binding.unwrap_or(Value::Null);
        for (key, file) in [("installation_binding", "installation.json"),
            ("source_facts", "source-facts.json"), ("preparation", "prepared.json"),
            ("starting_context", "starting-context.json"), ("committed_capture", "committed.json"),
            ("judge_input", "judge-input.json"), ("judgement", "judgement.json")] {
            arm[key] = read_optional_json(&arm_root.join(file))?.unwrap_or(Value::Null);
        }
        let mut retrievals = Vec::new();
        match std::fs::read_dir(arm_root.join("native-results")) {
            Ok(entries) => {
                let mut paths = entries.map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                paths.sort();
                for path in paths {
                    ensure!(path.extension().is_some_and(|extension| extension == "json"),
                        "unexpected retained native evidence file");
                    retrievals.push(read_optional_json(&path)?.context("native evidence disappeared")?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(binding) = arm["binding"].as_object() {
            ensure!(binding.get("run_binding_digest") == Some(&json!(run_digest))
                && binding.get("condition").and_then(Value::as_str) == Some(condition_name(candidate)),
                "retained arm belongs to a different run or condition");
        }
        if arm["status"] == "scored" && (arm["binding"].is_null()
            || arm["preparation"].is_null() || arm["starting_context"].is_null()
            || arm["committed_capture"].is_null() || arm["judgement"].is_null()
            || arm["binding"]["attempt_id"] != arm["attempt_id"]
            || arm["preparation"]["attempt_id"] != arm["attempt_id"]
            || arm["judgement"]["attempt_id"] != arm["attempt_id"]) {
            arm["recorded_status"] = arm["status"].clone();
            arm["status"] = json!("incomplete");
        }
        let expected_kind = if candidate { obelisk::TASK_KIND } else { "history.query" };
        arm["recorded_reader_admission"] = arm["used_expected_reader"].clone();
        arm["used_expected_reader"] = json!(retrievals.iter().any(|record|
            record["task_kind"] == expected_kind && record["authenticated"] == true
                && record["retrieved_evidence"].as_array().is_some_and(|evidence| !evidence.is_empty())));
        arm["retained_native_results"] = json!(retrievals);
        arms.push(arm);
    }
    let cleanup = read_optional_json(&root.join("cleanup.json"))?;
    let mut report = report_summary(arms, cleanup.as_ref().is_some_and(|value| value["cleanup_ok"] == true));
    report["run_binding"] = run;
    report["scenario"] = report["run_binding"]["scenario"].clone();
    report["corpus_digest"] = report["run_binding"]["fixture_digest"].clone();
    report["cleanup"] = cleanup.unwrap_or(Value::Null);
    report["reconstruction"] = json!({"requests_issued":false,"original_reports_replaced":false});
    Ok(report)
}

fn seal(store: &Store, value: &impl Serialize, schema: &str) -> Result<ObjectRef> {
    Ok(store
        .cas
        .seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "obelisk-acceptance",
            schema,
            1,
        )?
        .object)
}

fn read_json(store: &Store, reference: &ObjectRef) -> Result<Value> {
    Ok(serde_json::from_slice(&authenticated_bytes(store, reference)?)?)
}

fn authenticated_bytes(store: &Store, reference: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "acceptance evidence CAS mismatch"
    );
    Ok(bytes)
}

fn write_codex_corpus(root: &Path, corpus: &[Message]) -> Result<Vec<Value>> {
    let sessions = root.join("sessions/2026/11");
    create_evidence_dir(&sessions)?;
    let ids: BTreeSet<_> = corpus
        .iter()
        .map(|message| message.session_id.as_str())
        .collect();
    let mut facts = Vec::new();
    for id in ids {
        let messages: Vec<_> = corpus
            .iter()
            .filter(|message| message.session_id == id)
            .collect();
        let timestamp = &messages[0].timestamp;
        let mut records = vec![json!({"type":"session_meta", "timestamp":timestamp,
            "payload":{"id":id, "timestamp":timestamp, "cwd":"/synthetic/hydra", "cli_version":"1.0"}})];
        records.extend(messages.iter().map(|message| json!({
            "type":"event_msg", "timestamp":message.timestamp,
            "payload":{"type":if message.role == "user" { "user_message" } else { "agent_message" },
                "message":message.text}
        })));
        let mut bytes = Vec::new();
        for record in records {
            serde_json::to_writer(&mut bytes, &record)?;
            bytes.push(b'\n');
        }
        let path = sessions.join(format!("rollout-{id}.jsonl"));
        write_bytes(&path, &bytes)?;
        facts.push(
            json!({"session_id":id, "obelisk_session_id":format!("codex:{id}"),
            "path":path, "digest":Digest::blake3(&bytes), "bytes":bytes.len(), "synthetic":true}),
        );
    }
    Ok(facts)
}

struct Runtime {
    node: Arc<Mutex<Node>>,
    candidate: bool,
    capabilities: ProviderCapabilities,
    evidence_root: PathBuf,
    source_facts: Vec<Value>,
    retrievals: Mutex<Vec<Value>>,
}

impl SandboxRuntime for Runtime {
    async fn install(
        &self,
        supervisor: &Supervisor,
        sandbox: &PreparedSandbox,
        _catalog: &Arc<CognitionCatalog>,
    ) -> Result<()> {
        let history: Arc<dyn NativeTaskHandler> =
            Arc::new(HistoryHandler::new(sandbox.store().clone()));
        supervisor.spawn(NativeTaskHandlerPlugin, history).await?;
        if self.candidate {
            // Use the actual installed-generation lifecycle, including worker
            // cancellation and joins on retirement, not a test-only handler.
            obelisk::spawn(
                supervisor,
                sandbox.store().clone(),
                self.node.clone(),
                sandbox.installation(),
            )
            .await?;
        }
        Ok(())
    }

    async fn prepare_subject(
        &self,
        supervisor: &Supervisor,
        sandbox: &PreparedSandbox,
        state: SubjectLoopState,
    ) -> Result<SubjectLoopState> {
        let state = state.with_model_capabilities(Some(self.capabilities.clone()));
        let profile = state.bind_context_profile()?;
        let registry = supervisor
            .context()
            .try_service::<PolicyRegistry>()
            .map_err(|error| anyhow::anyhow!("context policy: {error}"))?;
        if !self.candidate {
            // Preserve the corpus in this branch for the real history reader,
            // but begin after a normal window rollover, with no answer summary.
            // Binding first prevents the first S0 from restoring a full-raw root.
            let scope = EntityId(state.head.branch_id.0);
            let history =
                StoreHistory::new(sandbox.store().clone(), scope, Arc::new(estimate_tokens));
            let caps = input_model_caps(&self.capabilities, u32::try_from(state.max_tokens)?)
                .context(
                    "deployment context must leave room after the subject output reservation",
                )?;
            ensure!(caps.max_context > 0, "no input budget remains");
            let fiber = supervisor
                .spawn(
                    NotesRolloverPlugin,
                    RolloverHosts {
                        config: RolloverConfig {
                            retain_units: 0,
                            ..RolloverConfig::default()
                        },
                        notes: Arc::new(StoreNotesJournal::new(sandbox.store().clone(), scope)),
                        registry: registry.clone(),
                    },
                )
                .await?;
            let leaf = history.leaf_seq()?.context("baseline corpus is empty")?;
            let raw = history.read_range(0, leaf)?;
            let (window, _) =
                registry.prepare(&history, &caps, raw.iter().map(|item| item.tokens).sum())?;
            let committed = registry.commit(window, &history)?;
            ensure!(
                committed
                    .entries
                    .iter()
                    .all(|entry| matches!(entry, WindowEntry::Pointer { .. }))
                    && !committed.entries.is_empty(),
                "baseline corpus leaked into the starting window"
            );
            ensure!(
                history.read_range(0, leaf)? == raw,
                "window preparation changed raw history"
            );
            supervisor.uninstall(&fiber).await?;
        }
        let starting = serde_json::to_vec_pretty(&json!({
                "profile_digest":profile,
                "provider_binding_digest":Digest::blake3(&serde_json::to_vec(&self.capabilities.binding)?),
                "max_context_tokens":self.capabilities.max_context_tokens,
                "subject_max_output_tokens":state.max_tokens,
                "context_limit_basis":"explicit verified deployment input, not inferred from model name",
                "window":registry.ledger().current()?,
            }))?;
        write_bytes(&sandbox.installation().join("starting-context.json"), &starting)?;
        write_bytes(&self.evidence_root.join("starting-context.json"), &starting)?;
        Ok(state)
    }

    fn accept_result(
        &self,
        store: &Store,
        definition: &NativeTaskDefinition,
        result: &TaskResult,
    ) -> Result<()> {
        // The shared observer has already authenticated task ownership and T2
        // consumption. Bind the returned value to the original E1 as well.
        let task = store.external_task(result.task_id)?;
        let operation = store.external_operation(result.operation_id)?;
        let receipt = store.external_evidence(result.receipt_id)?;
        let attempt = store.external_operation_attempt(result.operation_id)?;
        ensure!(store.native_task_result(result.task_id)?.as_ref() == Some(result)
            && task.kind == definition.kind && task.definition_version == definition.definition_version
            && task.is_terminal() && operation.task_id == task.task_id
            && receipt.operation_id == Some(operation.operation_id)
            && receipt.attempt_id == Some(attempt.attempt_id)
            && attempt.invocation_id == result.invocation_id
            && attempt.final_receipt_id == Some(result.receipt_id)
            && receipt.raw_ref == result.evidence_ref,
            "acceptance native result/E1 identity mismatch");
        authenticated_bytes(store, &receipt.raw_ref)?;
        let normalized = receipt.normalized_ref.as_ref().map(|reference| read_json(store, reference)).transpose()?;
        ensure!(normalized == result.result, "acceptance native normalized result mismatch");
        let frozen_input = read_json(store, &operation.final_input_ref)?;
        let (evidence, rejection) = match reader_evidence(definition, &result.outcome,
            result.result.as_ref(), &self.source_facts) {
            Ok(evidence) => (evidence, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };
        let record = json!({"schema_version":1, "authenticated":true,
            "task_kind":task.kind, "definition":definition, "input_ref":task.input_ref,
            "frozen_input_ref":operation.final_input_ref, "frozen_input":frozen_input,
            "attempt_id":attempt.attempt_id, "receipt":receipt, "result":result,
            "retrieved_evidence":evidence, "evidence_rejection":rejection});
        write_json(&self.evidence_root.join("native-results").join(format!("{}.json", result.result_id)), &record)?;
        self.retrievals.lock().push(record);
        // Preserve the shared runtime's existing failure policy. Successful
        // empty results remain visible to the subject and receive no admission.
        ensure!(matches!(result.outcome, TaskOutcome::Succeeded),
            "sandbox delegated task {} failed: {:?}", result.task_id, result.outcome);
        Ok(())
    }

    fn accept_unresolved(
        &self,
        _definition: &NativeTaskDefinition,
        observation: UnresolvedSandboxObservation<'_>,
    ) -> Result<()> {
        write_json(&self.evidence_root.join("native-results")
            .join(format!("{}.json", observation.receipt.receipt_id)),
            &json!({"schema_version":1, "authenticated":true,
                "task_kind":observation.task.kind, "observation":observation.observation,
                "receipt":observation.receipt, "attempt_id":observation.attempt.attempt_id,
                "normalized":observation.normalized, "retrieved_evidence":[],
                "status":"unresolved"}))?;
        anyhow::bail!("sandbox delegated task {} remains unresolved", observation.task.task_id)
    }

    async fn capture(
        &self,
        _sandbox: &PreparedSandbox,
        _step: &shenko_types::subject::SubjectStep,
        output: &CommittedTrialOutput,
    ) -> Result<Option<ObjectRef>> {
        // Capture before cleanup can turn a committed continuation into an
        // execution error. Recovery reports it without re-entering the runner.
        write_json(&self.evidence_root.join("committed.json"), output)?;
        Ok(None)
    }
}

async fn run_arm(
    handle: &BrokerHandle,
    root: &Path,
    candidate: bool,
    configuration: &ObeliskConfig,
    corpus: &[Message],
    deadline_ms: u64,
    context_tokens: u32,
) -> Result<Value> {
    let name = condition_name(candidate);
    let evidence_root = root.join(name);
    create_evidence_dir(&evidence_root)?;
    create_evidence_dir(&evidence_root.join("native-results"))?;
    let attempt_id = EntityId::new();
    write_json(&evidence_root.join("arm-binding.json"), &json!({
        "schema_version":2, "attempt_id":attempt_id, "condition":name, "trial":0,
        "run_binding":"../run-binding.json", "scenario":SCENARIO,
        "run_binding_digest":Digest::blake3(&std::fs::read(root.join("run-binding.json"))?),
        "fixture":{"file":"../fixture.json", "digest":Digest::blake3(CORPUS), "bytes":CORPUS.len()},
        "corpus_digest":Digest::blake3(&serde_json::to_vec(corpus)?),
        "message_digest":Digest::blake3(MESSAGE.as_bytes()), "rubric_digest":Digest::blake3(RUBRIC.as_bytes()),
        "provider":provider_identity(handle)?,
        "input_configuration_digest":Digest::blake3(&serde_json::to_vec(configuration)?),
        "budget":{"arm_deadline_ms":deadline_ms, "context_tokens":context_tokens,
            "judge_max_output_tokens":8192, "sampling":"unchanged production defaults"},
        "originals":"installation Store/CAS and native-results; no automatic replay",
    }))?;
    let result = run_bound_arm(handle, &evidence_root, candidate, configuration, corpus,
        ArmBudget { deadline_ms, context_tokens }, attempt_id).await;
    if let Err(error) = &result {
        let failure = json!({"schema_version":2, "condition":name, "attempt_id":attempt_id,
            "binding_file":evidence_root.join("arm-binding.json"), "status":"failed",
            "effective":null, "used_expected_reader":null, "error":format!("{error:#}"),
            "cleanup_failed":error.downcast_ref::<shenko_exec_host::SandboxCleanupFailure>().is_some(),
            "usage":"unknown unless present in original request receipts", "replay_permitted":false});
        if let Err(recording) = write_json(&evidence_root.join("failure.json"), &failure) {
            return result.map_err(|error| error.context(format!(
                "retaining arm failure also failed: {recording:#}")));
        }
    }
    result
}

struct ArmBudget {
    deadline_ms: u64,
    context_tokens: u32,
}

async fn run_bound_arm(
    handle: &BrokerHandle,
    evidence_root: &Path,
    candidate: bool,
    configuration: &ObeliskConfig,
    corpus: &[Message],
    budget: ArmBudget,
    attempt_id: EntityId,
) -> Result<Value> {
    let ArmBudget { deadline_ms, context_tokens } = budget;
    let name = condition_name(candidate);
    let prepared = PreparedSandbox::fresh(name, evidence_root)?;
    let installation = prepared.installation().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installation, std::fs::Permissions::from_mode(0o700))?;
    }
    let store = prepared.store().clone();
    let (head, first_step) = store.load_subject(prepared.agent_id())?;
    write_json(&evidence_root.join("installation.json"), &json!({
        "attempt_id":attempt_id, "installation":installation, "agent_id":prepared.agent_id(),
        "branch_id":head.branch_id, "genesis_ref":head.genesis_ref,
        "subject_root_task_id":head.subject_root_task_id, "first_step_id":first_step.step_id,
        "controller_epoch":head.controller_epoch, "software_generation":head.software_generation,
    }))?;
    let mut source_facts = Vec::new();
    let mut runtime_configuration_digest = None;
    if candidate {
        let mut config = configuration.clone();
        let source_root = installation.join("synthetic-codex");
        source_facts = write_codex_corpus(&source_root, corpus)?;
        write_json(&evidence_root.join("source-facts.json"), &source_facts)?;
        config.sources = vec![ObeliskSourceRoot {
            provider: ObeliskProvider::Codex,
            root_label: "synthetic-codex".into(),
            path: source_root,
        }];
        config.source_instance = "obelisk-acceptance/synthetic-v1".into();
        config.index_home = installation.join("obelisk-index");
        runtime_configuration_digest = Some(Digest::blake3(&serde_json::to_vec(&config)?));
        let file = installation.join("fixture-config.json");
        write_json(&file, &config)?;
        obelisk::command(
            &installation,
            HistoryCommand::Obelisk {
                command: ObeliskCommand::Configure { file },
            },
        )
        .await?;
        obelisk::command(
            &installation,
            HistoryCommand::Obelisk {
                command: ObeliskCommand::Enable,
            },
        )
        .await?;
    } else {
        let (head, _) = store.load_subject(prepared.agent_id())?;
        for (index, message) in corpus.iter().enumerate() {
            let mut identity = CORPUS.to_vec();
            identity.extend_from_slice(&index.to_le_bytes());
            let fact = store.append_subject_raw_fact(
                &head,
                EntityId::derived("obelisk-acceptance-corpus", &identity),
                &message.role,
                &json!({"source_session_id":message.session_id, "timestamp":message.timestamp,
                    "text":message.text, "synthetic":true}),
            )?;
            source_facts.push(json!({
                "event_id":fact.event_id, "session_id":fact.session_id,
                "seq":fact.seq, "event_type":fact.event_type,
                "role":fact.role, "data":fact.data,
            }));
        }
        write_json(&evidence_root.join("source-facts.json"), &source_facts)?;
    }
    let node = Arc::new(Mutex::new(Node::open(&installation.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node.clone()));
    let configuration_ref = seal(
        &store,
        &json!({"scenario":SCENARIO,
        "corpus_digest":Digest::blake3(CORPUS), "message":MESSAGE, "model":handle.binding().model(),
        "provider":provider_identity(handle)?, "attempt_id":attempt_id,
        "runtime_configuration_digest":runtime_configuration_digest,
        "deadline_ms":deadline_ms, "evaluation_scope":"bounded_production_smoke",
        "context_tokens":context_tokens, "context_limit_basis":"explicit verified deployment input",
        "sampling":"unchanged production defaults; exact payloads remain in request receipts"}),
        "configuration",
    )?;
    let installation_ref = seal(
        &store,
        &json!({"installation":installation,
        "external_reader":node.lock().installed_unit_fact(obelisk::UNIT_ID)?,
        "native_reader":"history.query:1"}),
        "installation",
    )?;
    write_json(&evidence_root.join("prepared.json"), &json!({
        "attempt_id":attempt_id, "configuration_ref":configuration_ref,
        "installation_ref":installation_ref, "runtime_configuration_digest":runtime_configuration_digest,
        "source_facts_digest":Digest::blake3(&serde_json::to_vec(&source_facts)?),
        "source_facts_file":"source-facts.json", "inference_started":false,
    }))?;
    let stop = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
    let began = std::time::Instant::now();
    let runtime = Runtime {
        node, candidate, evidence_root:evidence_root.to_path_buf(), source_facts,
        retrievals:Mutex::new(Vec::new()),
        capabilities:ProviderCapabilities {
            binding:handle.binding().clone(), max_context_tokens:Some(context_tokens), vision:None,
        },
    };
    let execution = run_prepared_sandbox(
        prepared,
        handle,
        catalog,
        MESSAGE,
        stop,
        &runtime,
    )
    .await?;
    let retrievals = runtime.retrievals.into_inner();
    let history_reads = retrievals.iter().filter(|record| record["task_kind"] == "history.query").count();
    let cli_reads = retrievals.iter().filter(|record| record["task_kind"] == obelisk::TASK_KIND
        && record["definition"]["input"]["op"] == "obelisk").count();
    let expected_kind = if candidate { obelisk::TASK_KIND } else { "history.query" };
    let used_expected_reader = retrievals.iter().any(|record| record["task_kind"] == expected_kind
        && record["authenticated"] == true
        && record["retrieved_evidence"].as_array().is_some_and(|evidence| !evidence.is_empty()));
    let facts_ref = seal(
        &store,
        &json!({"schema_version":1, "corpus":corpus,
        "corpus_digest":Digest::blake3(CORPUS), "source_facts":runtime.source_facts,
        "retrievals":retrievals, "synthetic":true,
        "claim_scope":"historical statements in a controlled fixture, not independent verification of code changes"}),
        "observed-facts",
    )?;
    let sample_ref = seal(&store, &execution.committed, "committed-output")?;
    let sample_digest = sample_ref.digest.to_string();
    write_json(&evidence_root.join("judge-input.json"), &json!({
        "attempt_id":attempt_id, "configuration_ref":configuration_ref,
        "installation_ref":installation_ref, "facts_ref":facts_ref, "sample_ref":sample_ref,
        "used_expected_reader":used_expected_reader,
        "history_query_reads":history_reads, "native_obelisk_cli_reads":cli_reads,
    }))?;
    let mut judge_receipt: Option<JudgeReceiptIdentity> = None;
    let verdict = judge_sandbox_output(
        store.clone(),
        handle,
        SandboxJudgement {
            rubric: RUBRIC,
            message: MESSAGE,
            output: &execution.committed.output,
            facts_ref: &facts_ref,
        },
        &SandboxJudgeContext {
            configuration_ref: &configuration_ref,
            installation_ref: &installation_ref,
            sample_digest: &sample_digest,
            scenario: SCENARIO,
            condition: if candidate {
                Condition::Candidate
            } else {
                Condition::Control
            },
            trial: 0,
        },
        &mut judge_receipt,
        stop,
    )
    .await;
    let (effective, judge_text, judge_error) = match verdict {
        Ok(JudgeVerdict::Effective(text)) => (Some(true), Some(text), None),
        Ok(JudgeVerdict::Ineffective(text)) => (Some(false), Some(text), None),
        Err(error) => (None, None, Some(format!("{error:#}"))),
    };
    write_json(&evidence_root.join("judgement.json"), &json!({
        "attempt_id":attempt_id, "judge_receipt":judge_receipt, "effective":effective,
        "judge_text":judge_text, "judge_error":judge_error, "sample_ref":sample_ref,
        "facts_ref":facts_ref,
    }))?;
    let report = json!({"condition":name, "installation":installation, "attempt_id":attempt_id,
        "binding_file":evidence_root.join("arm-binding.json"),
        "status":if effective.is_some() { "scored" } else { "failed" },
        "effective":effective, "used_expected_reader":used_expected_reader,
        "history_query_reads":history_reads, "native_obelisk_cli_reads":cli_reads,
        "elapsed_ms":began.elapsed().as_millis(), "configuration_ref":configuration_ref,
        "installation_ref":installation_ref, "facts_ref":facts_ref, "sample_ref":sample_ref,
        "judge_receipt":judge_receipt, "judge_text":judge_text, "judge_error":judge_error,
        "committed":execution.committed});
    write_json(&evidence_root.join("acceptance.json"), &report)?;
    Ok(report)
}

fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    if let Some(flag) = arguments.next() {
        ensure!(flag == "--report-only", "expected --report-only <existing run directory>");
        let root = PathBuf::from(arguments.next().context("report-only requires an existing run directory")?);
        ensure!(arguments.next().is_none(), "unexpected report-only argument");
        println!("{}", serde_json::to_string_pretty(&reconstruct_report(&root)?)?);
        return Ok(());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    let run_id = EntityId::new();
    let root = std::env::var_os("SHENKO_OBELISK_ACCEPTANCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/obelisk-acceptance")
        })
        .join(format!("run-{run_id}"));
    create_evidence_dir(&root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let required = [
        "SHENKO_LOCAL_MODEL_URL",
        "SHENKO_LOCAL_MODEL",
        "SHENKO_OBELISK_CONFIG",
        "SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS",
    ];
    let missing: Vec<_> = required
        .into_iter()
        .filter(|name| std::env::var(name).map_or(true, |value| value.trim().is_empty()))
        .collect();
    if std::env::var("SHENKO_USE_LOCAL_MODEL").as_deref() != Ok("1") || !missing.is_empty() {
        write_json(
            &root.join("report.json"),
            &json!({"schema_version":1,
            "status":"blocked_external", "complete":false, "scenario":SCENARIO,
            "reason":"explicit real local model and CLI configuration are required",
            "missing":missing, "corpus_digest":Digest::blake3(CORPUS), "scored":0}),
        )?;
        anyhow::bail!(
            "blocked_external: set SHENKO_USE_LOCAL_MODEL=1 and {required:?}; report {}",
            root.display()
        );
    }
    let corpus: Vec<Message> = serde_json::from_slice(CORPUS)?;
    let configuration_path =
        PathBuf::from(std::env::var_os("SHENKO_OBELISK_CONFIG").context("config path")?);
    let installation_proof = obelisk::load_config(&configuration_path).and_then(|config| {
        let proof = ObeliskRuntime::new(config.clone())?.verify_installation()?;
        Ok((config, proof))
    });
    let configuration = match installation_proof {
        Ok((config, proof)) => {
            write_json(&root.join("upstream-installation.json"), &proof)?;
            config
        }
        Err(error) => {
            write_json(
                &root.join("report.json"),
                &json!({"schema_version":1,
                "status":"blocked_external", "complete":false, "scenario":SCENARIO,
                "reason":"the independently installed CLI failed its preflight",
                "error":format!("{error:#}"), "scored":0}),
            )?;
            return Err(error.context("blocked_external: no model request was started"));
        }
    };
    let deadline_ms = std::env::var("SHENKO_OBELISK_ACCEPTANCE_DEADLINE_MS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(180_000);
    ensure!(
        (30_000..=900_000).contains(&deadline_ms),
        "arm deadline must be 30..=900 seconds"
    );
    let context_tokens = std::env::var("SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS")?
        .parse::<u32>()
        .context("deployment context tokens must be a positive integer")?;
    ensure!(
        context_tokens > 8192,
        "deployment context must exceed the judge output reservation"
    );
    let (broker, handle) = Broker::new()?;
    write_bytes(&root.join("fixture.json"), CORPUS)?;
    let run_binding = json!({"schema_version":2, "run_id":run_id, "scenario":SCENARIO,
        "planned_conditions":["baseline","candidate"], "trial":0,
        "fixture_digest":Digest::blake3(CORPUS), "fixture_bytes":CORPUS.len(),
        "corpus_digest":Digest::blake3(&serde_json::to_vec(&corpus)?),
        "runner_source_digest":Digest::blake3(include_bytes!("obelisk_acceptance.rs")),
        "executable_digest":shenko_kernel::native_artifact::native_artifact_digest()?,
        "provider":provider_identity(&handle)?,
        "input_configuration_digest":Digest::blake3(&serde_json::to_vec(&configuration)?),
        "budget":{"arm_deadline_ms":deadline_ms,"context_tokens":context_tokens,
            "judge_max_output_tokens":8192,"sampling":"unchanged production defaults"},
        "inference_started":false,
    });
    write_json(&root.join("run-binding.json"), &run_binding)?;
    let broker_task = tokio::spawn(broker.run());
    let mut arms = Vec::new();
    for candidate in [false, true] {
        match run_arm(
            &handle,
            &root,
            candidate,
            &configuration,
            &corpus,
            deadline_ms,
            context_tokens,
        )
        .await
        {
            Ok(report) => arms.push(report),
            Err(error) => {
                let cleanup_failed = error
                    .downcast_ref::<shenko_exec_host::SandboxCleanupFailure>()
                    .is_some();
                let evidence_root = root.join(condition_name(candidate));
                arms.push(json!({"condition":condition_name(candidate),
                    "status":"failed", "error":format!("{error:#}"), "cleanup_failed":cleanup_failed,
                    "effective":null, "used_expected_reader":null,
                    "binding_file":evidence_root.join("arm-binding.json"),
                    "failure_file":evidence_root.join("failure.json"), "replay_permitted":false}));
                if cleanup_failed {
                    break;
                }
            }
        }
    }
    let shutdown = handle.quiesce_for_handoff().await;
    let shutdown_artifact = write_json(
        &root.join("broker-shutdown.json"),
        &match &shutdown {
            Ok(snapshot) => snapshot.clone(),
            Err(error) => json!({"error":format!("{error:#}"), "remote_termination_claimed":false}),
        },
    );
    broker_task.abort();
    let joined = broker_task.await;
    let cleanup_ok = shutdown.is_ok()
        && shutdown_artifact.is_ok()
        && match joined {
            Ok(_) => true,
            Err(error) => error.is_cancelled(),
        };
    let cleanup = json!({"cleanup_ok":cleanup_ok,
        "shutdown_artifact_error":shutdown_artifact.err().map(|error| format!("{error:#}"))});
    write_json(&root.join("cleanup.json"), &cleanup)?;
    let mut report = report_summary(arms, cleanup_ok);
    report["run_binding"] = run_binding;
    report["cleanup"] = cleanup;
    let passed = report["status"] == "passed_smoke";
    write_json(&root.join("report.json"), &report)?;
    eprintln!(
        "Obelisk real-model acceptance report: {}",
        root.join("report.json").display()
    );
    ensure!(
        passed,
        "real-model acceptance did not complete successfully; inspect retained report and receipts"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_history_admission_requires_successful_nonempty_corpus_evidence() -> Result<()> {
        let event_id = EntityId::new();
        let session_id = EntityId::new();
        let facts = vec![json!({"event_id":event_id,"session_id":session_id,"seq":3})];
        let mut definition = NativeTaskDefinition {
            kind:"history.query".into(), definition_version:1, input:json!({"op":"search"}),
        };
        let record = json!({"event_id":event_id,"session_id":session_id,"seq":3,
            "event_ref":format!("session:{session_id}/event:{event_id}/seq:3"),
            "data_preview":"arbitrary retained source text"});
        let mut returned = json!({"schema_version":1,"status":"ok","op":"search","events":[record]});
        assert!(reader_evidence(&definition, &TaskOutcome::Failed { reason:"original failure".into() },
            Some(&returned), &facts)?.is_empty());
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.len(), 1);
        returned["events"] = json!([]);
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.is_empty());
        let report = report_summary(vec![
            json!({"status":"scored","effective":true,"used_expected_reader":false}),
            json!({"status":"scored","effective":true,"used_expected_reader":true}),
        ], true);
        assert_eq!(report["status"], "inconclusive_or_failed");
        assert_eq!(report["arms"][0]["effective"], true, "reader admission must not rewrite the judge score");
        definition.input = json!({"op":"read","event_id":event_id});
        returned = json!({"schema_version":1,"status":"ok","op":"read","event":{
            "event_id":event_id,"session_id":session_id,"seq":3,
            "event_ref":format!("session:{session_id}/event:{event_id}/seq:3"),
            "data":serde_json::to_string(&json!({"text":"arbitrary retained source text"}))?,
        }});
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.len(), 1);
        Ok(())
    }

    #[test]
    fn obelisk_admission_reads_nested_fts_and_flat_literal_hits() -> Result<()> {
        let session = "codex:synthetic-session";
        let facts = vec![json!({"obelisk_session_id":session})];
        let definition = NativeTaskDefinition {
            kind:obelisk::TASK_KIND.into(), definition_version:1,
            input:json!({"op":"obelisk","request":{"op":"search","query":"arbitrary"}}),
        };
        let mut returned = json!({"observation":{"result":{
            "schema_version":1,"status":"succeeded","source_instance":"obelisk-acceptance/synthetic-v1",
            "process":{"started":true,"reaped":true,"readers_joined":true,"readers_drained":true,
                "exit_code":0,"termination_requested":false,"stdout_truncated":false,
                "stderr_truncated":false,"custom_script_host_authority":false},
            "data":{"mode":"fts5","hits":[{"message":{"uuid":"source-message","text":"arbitrary source text"},
                "session":{"id":session}}]},
        }}});
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.len(), 1);
        returned["observation"]["result"]["data"] = json!({"mode":"fts5","hits":[]});
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.is_empty());
        returned["observation"]["result"]["data"] = json!({"mode":"literal_index_fallback","hits":[{
            "uuid":"source-message","session_id":session,"text":"arbitrary source text"}]});
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.len(), 1);
        returned["observation"]["result"]["status"] = json!("failed");
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts)?.is_empty());
        Ok(())
    }

    struct EvidenceRoot(PathBuf);
    impl Drop for EvidenceRoot {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn interrupted_arm_reconstructs_original_identity_without_replay_or_replacement() -> Result<()> {
        let root = EvidenceRoot(Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../build").join(format!("obelisk-offline-report-{}", EntityId::new())));
        std::fs::create_dir_all(root.0.join("baseline/native-results"))?;
        let attempt_id = EntityId::new();
        let binding = json!({"schema_version":2,"run_id":EntityId::new(),"scenario":SCENARIO,
            "fixture_digest":Digest::blake3(CORPUS),"fixture_bytes":CORPUS.len()});
        write_json(&root.0.join("run-binding.json"), &binding)?;
        write_bytes(&root.0.join("fixture.json"), CORPUS)?;
        let arm_root = root.0.join("baseline");
        let arm_binding = json!({"attempt_id":attempt_id,"condition":"baseline",
            "run_binding_digest":Digest::blake3(&std::fs::read(root.0.join("run-binding.json"))?),
            "provider":{"profile_id":"retained-profile","model":"retained-model"},
            "budget":{"arm_deadline_ms":180000,"context_tokens":16384}});
        write_json(&arm_root.join("arm-binding.json"), &arm_binding)?;
        let failure = json!({"condition":"baseline","attempt_id":attempt_id,"status":"failed",
            "effective":null,"used_expected_reader":false,"error":"original subject timeout"});
        write_json(&arm_root.join("failure.json"), &failure)?;
        write_json(&arm_root.join("committed.json"), &json!({"step_id":EntityId::new(),"output":"retained continuation"}))?;
        write_json(&arm_root.join("judgement.json"), &json!({"attempt_id":attempt_id,"effective":true}))?;
        write_json(&root.0.join("cleanup.json"), &json!({"cleanup_ok":true}))?;
        let original = std::fs::read(arm_root.join("failure.json"))?;
        let first = reconstruct_report(&root.0)?;
        let second = reconstruct_report(&root.0)?;
        assert_eq!(first, second);
        assert_eq!(first["complete"], false);
        assert_eq!(first["arms"][0]["attempt_id"], json!(attempt_id));
        assert_eq!(first["arms"][0]["binding"], arm_binding);
        assert_eq!(first["arms"][0]["effective"], Value::Null);
        assert_eq!(first["arms"][0]["judgement"]["effective"], true);
        assert_eq!(first["reconstruction"]["requests_issued"], false);
        assert!(write_json(&arm_root.join("failure.json"), &json!({"replacement":true})).is_err());
        assert_eq!(std::fs::read(arm_root.join("failure.json"))?, original);
        assert!(!root.0.join("report.json").exists());
        Ok(())
    }
}
