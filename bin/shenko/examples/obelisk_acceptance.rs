//! One real-model comparison through the production subject sandbox.
//!
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<base URL without /v1>
//! SHENKO_LOCAL_MODEL=<served model> SHENKO_OBELISK_CONFIG=<compatible config JSON>
//! SHENKO_OBELISK_ACCEPTANCE_CONTEXT_TOKENS=<verified deployment context limit>
//! cargo run -p shenko --example obelisk_acceptance
//!
//! The config supplies only the independently installed, source-pinned CLI.
//! With no argument, every provider root is replaced with the embedded corpus.
//! --local-scenario <owner input JSON> instead captures the explicitly selected
//! complete originals once, then gives both arms independent identical copies.
//! Runs require an explicit private SHENKO_OBELISK_ACCEPTANCE_ROOT in Life.
//! The baseline retrieves authenticated historical observations with history.query;
//! the candidate uses the installed history.external.query capability.
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
use shenko_store::{SessionEventFilter, Store};
use shenko_subject::{NativeTaskHandlerPlugin, SubjectLoopState, input_model_caps};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    task::{NativeTaskDefinition, TaskOutcome, TaskResult},
};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

mod obelisk_local_scenario;

#[cfg(test)]
fn fixture_parent() -> PathBuf {
    std::env::var_os("SHENKO_OBELISK_LOCAL_SCENARIO_TEST_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}
use obelisk_local_scenario::{FrozenScenario, SnapshotManifest};

const SYNTHETIC_SOURCE_INSTANCE: &str = "obelisk-acceptance/synthetic-v1";
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

enum SelectedScenario {
    Synthetic { corpus: Vec<Message>, digest: Digest },
    Local(Box<FrozenScenario>),
}

impl SelectedScenario {
    fn synthetic() -> Result<Self> {
        Self::synthetic_bytes(CORPUS)
    }

    fn synthetic_bytes(bytes: &[u8]) -> Result<Self> {
        let corpus: Vec<Message> = serde_json::from_slice(bytes)?;
        let digest = Digest::blake3(&serde_json::to_vec(&corpus)?);
        Ok(Self::Synthetic { corpus, digest })
    }

    fn local(&self) -> Option<&FrozenScenario> {
        match self { Self::Local(snapshot) => Some(snapshot), Self::Synthetic { .. } => None }
    }

    fn scenario(&self) -> &str {
        self.local().map_or(SCENARIO, |snapshot| &snapshot.manifest().scenario)
    }

    fn question(&self) -> &str {
        self.local().map_or(MESSAGE, |snapshot| &snapshot.manifest().question)
    }

    fn rubric(&self) -> &str {
        self.local().map_or(RUBRIC, |snapshot| &snapshot.manifest().rubric)
    }

    fn corpus_digest(&self) -> &Digest {
        match self {
            Self::Synthetic { digest, .. } => digest,
            Self::Local(snapshot) => &snapshot.manifest().corpus_digest,
        }
    }

    // Identical in the run and both arm bindings; paths are relative to the run.
    fn binding(&self) -> Result<Value> {
        Ok(match self {
            Self::Synthetic { .. } => json!({"kind":"synthetic", "fixture_file":"fixture.json",
                "fixture_digest":Digest::blake3(CORPUS), "fixture_bytes":CORPUS.len(),
                "corpus_digest":self.corpus_digest(),
                "question_digest":Digest::blake3(self.question().as_bytes()),
                "rubric_digest":Digest::blake3(self.rubric().as_bytes())}),
            Self::Local(snapshot) => {
                let manifest = snapshot.manifest();
                json!({"kind":"local", "snapshot_file":"local-scenario/snapshot.json",
                    "snapshot_digest":snapshot.manifest_digest(),
                    "corpus_digest":manifest.corpus_digest, "source_instance":manifest.source_instance,
                    "question_digest":manifest.question_digest, "rubric_digest":manifest.rubric_digest,
                    "selection_digest":manifest.selection_digest, "limits":manifest.limits,
                    "file_count":manifest.file_count, "total_bytes":manifest.total_bytes,
                    "coverage":manifest.coverage})
            }
        })
    }
}

enum Command {
    Run(Option<PathBuf>),
    ReportOnly(PathBuf),
}

fn parse_command(arguments: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let mut arguments = arguments.into_iter();
    let Some(flag) = arguments.next() else { return Ok(Command::Run(None)) };
    ensure!(flag == "--local-scenario" || flag == "--report-only",
        "expected --local-scenario <owner input JSON> or --report-only <existing run directory>");
    let path = arguments.next().context("scenario/report flag requires one path")?;
    ensure!(!path.is_empty() && path != "--local-scenario" && path != "--report-only"
        && arguments.next().is_none(), "expected exactly one path and one mode");
    Ok(if flag == "--report-only" { Command::ReportOnly(PathBuf::from(path)) }
        else { Command::Run(Some(PathBuf::from(path))) })
}

fn open_retained_scenario(root: &Path, run: &Value) -> Result<SelectedScenario> {
    let scenario = match run["scenario_kind"].as_str() {
        Some("local") => {
            let digest: Digest = serde_json::from_value(run["source_selection"]["snapshot_digest"].clone())
                .context("local run lacks its original snapshot digest")?;
            SelectedScenario::Local(Box::new(FrozenScenario::open(&root.join("local-scenario"), &digest)?))
        }
        None | Some("synthetic") => {
            let fixture = std::fs::read(root.join("fixture.json"))?;
            ensure!(run["fixture_digest"] == json!(Digest::blake3(&fixture))
                && run["fixture_bytes"].as_u64() == Some(fixture.len() as u64),
                "retained fixture differs from the original run binding");
            SelectedScenario::synthetic_bytes(&fixture)?
        }
        Some(_) => anyhow::bail!("unsupported retained scenario kind"),
    };
    if scenario.local().is_some() || !run["source_selection"].is_null() {
        ensure!(run["source_selection"] == scenario.binding()?
            && run["scenario"] == scenario.scenario()
            && run["corpus_digest"] == json!(scenario.corpus_digest())
            && run["message_digest"] == json!(Digest::blake3(scenario.question().as_bytes()))
            && run["rubric_digest"] == json!(Digest::blake3(scenario.rubric().as_bytes())),
            "retained scenario differs from the original run binding");
    }
    Ok(scenario)
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
    #[serde(default)]
    source_manifests: Vec<shenko_exec_host::obelisk_runtime::ObeliskSourceManifest>,
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

fn cli_observation(returned: &Value, expected_source_instance: &str) -> Result<Option<CliObservation>> {
    let observation = CliResult::deserialize(returned)?.observation.result;
    let process = &observation.process;
    if observation.schema_version != 1 || observation.status != ObeliskRunStatus::Succeeded
        || observation.source_instance != expected_source_instance
        || !process.started || !process.reaped || !process.readers_joined || !process.readers_drained
        || process.exit_code != Some(0) || process.termination_requested
        || process.stdout_truncated || process.stderr_truncated || process.custom_script_host_authority {
        return Ok(None);
    }
    Ok(Some(observation))
}

fn local_selected<'a>(manifest: &'a SnapshotManifest, source: &Value, digest: &Value)
    -> Option<&'a obelisk_local_scenario::SelectedFile>
{
    manifest.files.iter().find(|file| json!(file.source) == *source && json!(file.digest) == *digest)
}

// Source facts come only from archive_into/read_record. Retained reconstruction
// rechecks their selected versions before trusting the original consumption.
fn local_facts_bound(facts: &[Value], snapshot: &FrozenScenario) -> Result<bool> {
    let manifest = snapshot.manifest();
    let originals: Vec<_> = facts.iter().filter(|fact| fact["kind"] == "source").collect();
    if originals.len() != manifest.files.len()
        || facts.iter().any(|fact| fact["snapshot_digest"] != json!(snapshot.manifest_digest())) {
        return Ok(false);
    }
    for file in &manifest.files {
        let matching: Vec<_> = originals.iter().filter(|fact|
            fact["archive"]["source"] == json!(file.source)
                && fact["archive"]["raw_ref"]["digest"] == json!(file.digest)).collect();
        if matching.len() != 1 { return Ok(false) }
        let archive: shenko_exec_host::external_history::ArchivedSource =
            shenko_exec_host::external_history::ArchivedSource::deserialize(&matching[0]["archive"])?;
        if archive.bytes != file.bytes || archive.raw_ref.bytes != file.bytes {
            return Ok(false);
        }
    }
    for fact in facts.iter().filter(|fact| fact["kind"] != "source") {
        if fact["kind"] != "record" { return Ok(false) }
        let record: shenko_exec_host::external_history::ArchivedRecord =
            shenko_exec_host::external_history::ArchivedRecord::deserialize(&fact["record"])?;
        let Some(file) = local_selected(manifest, &json!(record.source), &json!(record.source_digest))
            else { return Ok(false) };
        if record.byte_start > record.byte_end || record.byte_end > file.bytes
            || !originals.iter().any(|original| original["archive"]["archive_session_id"] == json!(record.archive_session_id)
                && original["archive"]["source"] == json!(record.source)
                && original["archive"]["raw_ref"]["digest"] == json!(record.source_digest)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn local_source_facts(snapshot: &FrozenScenario, mounted_root: &Path,
    prepared: &PreparedSandbox, candidate: bool) -> Result<Vec<Value>>
{
    use shenko_exec_host::external_history;
    let mounted_root = mounted_root.canonicalize().context("mounted local snapshot unavailable")?;
    let store = prepared.store();
    let agent_id = prepared.agent_id();
    let (head, _) = store.load_subject(agent_id)?;
    let mut facts = Vec::new();
    for archive in snapshot.archive_into(store, agent_id)? {
        let selected = snapshot.manifest().files.iter().find(|file| file.source == archive.source
            && file.digest == archive.raw_ref.digest).context("archive not in selected inventory")?;
        facts.push(json!({"kind":"source", "snapshot_digest":snapshot.manifest_digest(),
            "archive":archive, "local_path":mounted_root.join("originals")
                .join(&selected.source.provider).join(&selected.relative_path)}));
        let mut from_seq = 0;
        loop {
            let rows = store.list_session_events(archive.archive_session_id, &SessionEventFilter {
                from_seq, limit:64, event_types:vec!["external_history_record".into()],
                ..Default::default()
            })?;
            if rows.is_empty() { break; }
            for row in rows {
                let record = external_history::read_record(store, agent_id, row.event_id, false)?;
                let mut fact = json!({"kind":"record", "record":record,
                    "snapshot_digest":snapshot.manifest_digest()});
                if !candidate {
                    let identity = serde_json::to_vec(&("shenko.obelisk.local-baseline/1",
                        &record.source, &record.source_digest, record.ordinal))?;
                    let branch = store.append_subject_raw_fact(&head,
                        EntityId::derived("obelisk-local-baseline-record", &identity),
                        "external_observation", &json!({"text":record.text, "source":record.source,
                            "source_digest":record.source_digest, "archive_session_id":record.archive_session_id,
                            "archived_event_id":record.event_id, "logical_event_id":record.logical_event_id,
                            "byte_start":record.byte_start, "byte_end":record.byte_end,
                            "record_blake3_hex":record.record_blake3_hex, "native_fields":record.fields,
                            "parse_error":record.parse_error, "synthetic":false,
                            "authority":"historical observation only"}))?;
                    fact["event_id"] = json!(branch.event_id);
                    fact["session_id"] = json!(branch.session_id);
                    fact["seq"] = json!(branch.seq);
                    fact["event_type"] = json!(branch.event_type);
                    fact["role"] = json!(branch.role);
                    fact["data"] = json!(branch.data);
                }
                facts.push(fact);
                from_seq = row.seq.checked_add(1).context("archive sequence overflow")?;
            }
        }
    }
    ensure!(local_facts_bound(&facts, snapshot)?, "prepared local facts differ from the snapshot");
    Ok(facts)
}

fn input_usize(input: &Value, name: &str, default: usize) -> Result<usize> {
    input.get(name).map_or(Ok(default), |value| {
        usize::try_from(value.as_u64().context("reader range must be an unsigned integer")?)
            .context("reader range exceeds platform size")
    })
}

fn local_archive_evidence(definition: &NativeTaskDefinition, returned: &Value,
    facts: &[Value], manifest: &SnapshotManifest) -> Result<Vec<Value>>
{
    use shenko_exec_host::external_history::{ArchiveSearch, ArchivedRecord, OriginalPage};
    use base64::Engine as _;
    let value = &returned["observation"]["result"];
    match definition.input["op"].as_str() {
        Some("search") => {
            let page = ArchiveSearch::deserialize(value)?;
            let mut admitted = Vec::new();
            for hit in page.hits {
                let source = json!(hit.source);
                let digest = json!(hit.source_digest);
                if !has_text(&hit.preview) || local_selected(manifest, &source, &digest).is_none() {
                    continue;
                }
                if facts.iter().filter(|fact| fact["kind"] == "record").any(|fact| {
                    let record = &fact["record"];
                    record["event_id"] == json!(hit.event_id)
                        && record["logical_event_id"] == json!(hit.logical_event_id)
                        && record["archive_session_id"] == json!(hit.archive_session_id)
                        && record["source"] == source && record["source_digest"] == digest
                        && record["ordinal"] == hit.ordinal && record["fields"] == json!(hit.fields)
                        && record["byte_start"] == hit.byte_start && record["byte_end"] == hit.byte_end
                        && record["text"].as_str().is_some_and(|text| text.chars()
                            .skip(hit.preview_start_char).take(hit.preview.chars().count()).eq(hit.preview.chars()))
                }) && hit.citation == format!("shenko-history://{}/{}?version={}&bytes={}-{}",
                    hit.archive_session_id, hit.event_id, hit.source_digest, hit.byte_start, hit.byte_end) {
                    admitted.push(serde_json::to_value(hit)?);
                }
            }
            Ok(admitted)
        }
        Some("read") => {
            let record = ArchivedRecord::deserialize(value)?;
            let offset = input_usize(&definition.input, "offset_chars", 0)?;
            let limit = input_usize(&definition.input, "limit_chars", 4096)?;
            if definition.input["event_id"] != json!(record.event_id) || !has_text(&record.text)
                || local_selected(manifest, &json!(record.source), &json!(record.source_digest)).is_none() {
                return Ok(Vec::new());
            }
            for fact in facts.iter().filter(|fact| fact["kind"] == "record") {
                let mut expected: ArchivedRecord = shenko_exec_host::external_history::ArchivedRecord::deserialize(&fact["record"])?;
                if expected.event_id != record.event_id { continue; }
                let total = expected.text.chars().count();
                expected.text = expected.text.chars().skip(offset).take(limit).collect();
                let count = expected.text.chars().count();
                if serde_json::to_value(expected)? == serde_json::to_value(&record)?
                    && value["page"] == json!({"offset_chars":offset, "returned_chars":count,
                        "total_chars":total, "next_offset_chars":(offset + count < total).then_some(offset + count)}) {
                    return Ok(vec![value.clone()]);
                }
            }
            Ok(Vec::new())
        }
        Some("original") => {
            let page = OriginalPage::deserialize(value)?;
            let offset = u64::try_from(input_usize(&definition.input, "offset", 0)?)?;
            let limit = input_usize(&definition.input, "limit", 16 * 1024)?;
            let content = base64::engine::general_purpose::STANDARD.decode(&page.content_base64)?;
            if !page.authenticated || content.is_empty() || page.offset != offset
                || definition.input["archive_session_id"] != json!(page.archive_session_id)
                || offset > page.total_bytes { return Ok(Vec::new()); }
            let count = (page.total_bytes - offset).min(u64::try_from(limit)?);
            if content.len() as u64 != count || page.next_offset !=
                (offset + count < page.total_bytes).then_some(offset + count) {
                return Ok(Vec::new());
            }
            for fact in facts.iter().filter(|fact| fact["kind"] == "source") {
                let archive = &fact["archive"];
                if archive["archive_session_id"] == json!(page.archive_session_id)
                    && archive["raw_ref"]["digest"] == json!(page.digest)
                    && archive["bytes"] == page.total_bytes
                    && local_selected(manifest, &archive["source"], &json!(page.digest)).is_some() {
                    return Ok(vec![json!({"source":archive["source"], "source_digest":page.digest,
                        "byte_start":offset, "byte_end":offset + count, "page":page})]);
                }
            }
            Ok(Vec::new())
        }
        // Import, listings and generic artifacts are retained but are not a
        // source-content read in this comparison.
        _ => Ok(Vec::new()),
    }
}

fn local_message_source<'a>(id: &str, facts: &[Value], manifest: &'a SnapshotManifest,
    prior: &[&Value]) -> Option<&'a obelisk_local_scenario::SelectedFile>
{
    use shenko_exec_host::obelisk_runtime::ObeliskSourceAvailability;
    for record in prior {
        if record["authenticated"] != true { continue; }
        let Ok(definition) = NativeTaskDefinition::deserialize(&record["definition"]) else { continue };
        let Ok(result) = TaskResult::deserialize(&record["result"]) else { continue };
        if definition.kind != obelisk::TASK_KIND || definition.definition_version != 1
            || result.schema_version != 1 || definition.input["op"] != "obelisk"
            || !matches!(result.outcome, TaskOutcome::Succeeded) {
            continue;
        }
        let Some(returned) = result.result.as_ref() else { continue };
        match definition.input["request"]["op"].as_str() {
            Some("search" | "messages") => {
                let Ok(evidence) = reader_evidence(&definition, &result.outcome,
                    Some(returned), facts, Some(manifest), &[]) else { continue };
                for evidence in evidence {
                    let item = &evidence["evidence"];
                    if item["uuid"] == id || item["message"]["uuid"] == id {
                        return local_selected(manifest, &evidence["source"], &evidence["source_digest"]);
                    }
                }
            }
            Some("manifest") => {
                let Ok(Some(observation)) = cli_observation(returned, &manifest.source_instance)
                    else { continue };
                for native in observation.source_manifests {
                    if !matches!(native.availability, ObeliskSourceAvailability::Available)
                        || native.error.is_some() { continue; }
                    for file in &manifest.files {
                        let basis = match file.identity_basis {
                            obelisk_local_scenario::IdentityBasis::NativeSession => "native_session",
                            obelisk_local_scenario::IdentityBasis::ConfiguredRootRelativePath => "configured_root_relative_path",
                        };
                        if native.source_instance == file.source.source_instance
                            && native.provider.as_str() == file.source.provider
                            && native.source_key == file.source.source_key && native.session_id == file.source.session_id
                            && native.identity_basis == basis && native.observed_size == Some(file.bytes)
                            && facts.iter().any(|fact| fact["kind"] == "source"
                                && fact["archive"]["source"] == json!(file.source)
                                && fact["archive"]["raw_ref"]["digest"] == json!(file.digest)
                                && fact["local_path"] == json!(native.local_path))
                            && native.records.iter().any(|record| record.id == id) {
                            return Some(file);
                        }
                    }
                }
            }
            _ => {},
        }
    }
    None
}

fn has_text(text: &str) -> bool { !text.trim().is_empty() }

// This checks returned evidence structure and corpus identity, never answer
// words. The independent judge still decides whether the answer used it well.
fn reader_evidence(
    definition: &NativeTaskDefinition,
    outcome: &TaskOutcome,
    returned: Option<&Value>,
    source_facts: &[Value],
    local: Option<&SnapshotManifest>,
    prior: &[&Value],
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
    if definition.kind != obelisk::TASK_KIND {
        return Ok(Vec::new());
    }
    if definition.input["op"] != "obelisk" {
        return match local {
            Some(manifest) => local_archive_evidence(definition, returned, source_facts, manifest),
            None => Ok(Vec::new()),
        };
    }
    let Some(observation) = cli_observation(returned,
        local.map_or(SYNTHETIC_SOURCE_INSTANCE, |manifest| manifest.source_instance.as_str()))?
        else { return Ok(Vec::new()) };
    let known_session = |id: &str| match local {
        Some(manifest) => manifest.files.iter().any(|file|
            file.source.session_id.as_deref() == Some(id)
                && id.strip_prefix(file.source.provider.as_str())
                    .and_then(|suffix| suffix.strip_prefix(':')).is_some_and(has_text)),
        None => source_facts.iter().any(|fact| fact["obelisk_session_id"] == id),
    };
    let request = &definition.input["request"];
    let data = observation.data;
    let evidence: Result<Vec<Value>> = match request["op"].as_str() {
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
                && local.is_none_or(|manifest| local_message_source(&page.message_uuid, source_facts,
                    manifest, prior).is_some())
                && request["message_uuid"] == page.message_uuid
                && page.offset as u64 == request["offset_utf16"].as_u64().unwrap_or(0)
                && page.offset_unit == "utf16_code_units" {
                vec![serde_json::to_value(page)?]
            } else { Vec::new() })
        }
        // Directory metadata, arbitrary script output, errors and outlines are
        // retained, but do not attest that historical message text was read.
        _ => Ok(Vec::new()),
    };
    let evidence = evidence?;
    let Some(manifest) = local else { return Ok(evidence) };
    // A CLI result is an indexed/native observation over the selected copies;
    // it is never relabelled as an authenticated full-original record read.
    Ok(evidence.into_iter().filter_map(|item| {
        let session = item["session"]["id"].as_str().or_else(|| item["session_id"].as_str());
        let selected = session.and_then(|session| manifest.files.iter().find(|file|
            file.source.session_id.as_deref() == Some(session)))
            .or_else(|| item["message_uuid"].as_str().and_then(|id|
                local_message_source(id, source_facts, manifest, prior)));
        selected.map(|file| json!({"source":file.source, "source_digest":file.digest,
            "source_bytes":file.bytes, "evidence":item, "coverage":"upstream projection; original retained separately"}))
    }).collect())
}

fn report_summary(arms: Vec<Value>, cleanup_ok: bool, scenario: &SelectedScenario) -> Result<Value> {
    let complete = arms.len() == 2 && arms.iter().all(|arm| arm["status"] == "scored");
    let passed = complete && cleanup_ok && arms.iter().all(|arm|
        arm["effective"] == true && arm["used_expected_reader"] == true);
    Ok(json!({"schema_version":2,
        "status":if passed { "passed_smoke" } else { "inconclusive_or_failed" },
        "scenario":scenario.scenario(), "planned_arms":2, "complete":complete, "cleanup_ok":cleanup_ok,
        "corpus_digest":scenario.corpus_digest(), "source_selection":scenario.binding()?,
        "evaluation_scope":"one bounded production-path comparison; no efficacy or token-saving claim",
        "arms":arms}))
}

// A retained authentication flag describes the original live observation.
// Admission also needs this arm's committed consumption and prepared sources;
// copied receipts and cached evidence summaries cannot supply either binding.
fn retained_reader_admitted(arm: &Value, retrievals: &[Value], expected_kind: &str,
    scenario: &SelectedScenario) -> Result<bool> {
    let binding = &arm["binding"];
    let installation = &arm["installation_binding"];
    let preparation = &arm["preparation"];
    let committed = &arm["committed_capture"];
    let Some(source_facts) = arm["source_facts"].as_array() else { return Ok(false) };
    let Some(consumed) = committed["consumed_results"].as_array() else { return Ok(false) };
    if !binding["attempt_id"].as_str().is_some_and(has_text)
        || installation["attempt_id"] != binding["attempt_id"]
        || preparation["attempt_id"] != binding["attempt_id"]
        || !installation["installation"].as_str().is_some_and(has_text)
        || committed["installation"] != installation["installation"]
        || source_facts.is_empty()
        || preparation["source_facts_digest"] != json!(Digest::blake3(&serde_json::to_vec(source_facts)?)) {
        return Ok(false);
    }
    if let Some(snapshot) = scenario.local()
        && (binding["source_selection"] != scenario.binding()?
            || preparation["source_selection"] != scenario.binding()?
            || arm["source_integrity"]["snapshot_digest"] != json!(snapshot.manifest_digest())
            || arm["source_integrity"]["validated"] != true
            || arm["source_integrity"]["original_snapshot_validated"] != true
            || arm["source_integrity"]["mounted_snapshot_validated"] != true
            || !local_facts_bound(source_facts, snapshot)?) {
        return Ok(false);
    }
    let owned: Vec<_> = retrievals.iter().filter(|record| record["schema_version"] == 1
        && record["task_kind"] == expected_kind && record["authenticated"] == true
        && consumed.iter().any(|result| result == &record["result"]))
        .collect();
    for record in &owned {
        if record["schema_version"] != 1 || record["task_kind"] != expected_kind
            || record["authenticated"] != true
            || !consumed.iter().any(|result| result == &record["result"]) {
            continue;
        }
        let Ok(definition) = NativeTaskDefinition::deserialize(&record["definition"]) else { continue };
        let Ok(result) = TaskResult::deserialize(&record["result"]) else { continue };
        if definition.kind != expected_kind || result.schema_version != 1 {
            continue;
        }
        if reader_evidence(&definition, &result.outcome, result.result.as_ref(), source_facts,
            scenario.local().map(FrozenScenario::manifest), &owned)
            .is_ok_and(|evidence| !evidence.is_empty()
                && record["retrieved_evidence"].as_array() == Some(&evidence)) {
            return Ok(true);
        }
    }
    Ok(false)
}

// Only reads retained JSON. Missing phases remain unknown, and a judge record
// without a terminal arm/cleanup record cannot promote an interrupted sample.
fn reconstruct_report(root: &Path) -> Result<Value> {
    let run_bytes = std::fs::read(root.join("run-binding.json"))
        .context("existing run has no readable durable binding")?;
    let run: Value = serde_json::from_slice(&run_bytes)?;
    ensure!(run["schema_version"] == 2, "unsupported retained run schema");
    let scenario = open_retained_scenario(root, &run)?;
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
            ("judge_input", "judge-input.json"), ("judgement", "judgement.json"),
            ("source_integrity", "source-integrity.json")] {
            arm[key] = read_optional_json(&arm_root.join(file))?.unwrap_or(Value::Null);
        }
        let mut retrievals = Vec::new();
        match std::fs::read_dir(arm_root.join("native-results")) {
            Ok(entries) => {
                let mut paths = entries.map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()?;
                paths.sort();
                for path in paths {
                    ensure!(std::fs::symlink_metadata(&path)?.is_file(),
                        "unexpected retained native evidence entry");
                    // write_bytes can be interrupted before or after linking
                    // the final name. Only its canonical UUIDv4 temp is ignored.
                    let temporary = path.file_name().and_then(|name| name.to_str())
                        .and_then(|name| name.strip_prefix(".evidence-"))
                        .is_some_and(|name| {
                            let Ok(id) = name.parse::<EntityId>() else { return false };
                            let mut canonical = [0; 36];
                            id.as_uuid().get_version() == Some(uuid::Version::Random)
                                && id.as_uuid().get_variant() == uuid::Variant::RFC4122
                                && id.as_uuid().hyphenated().encode_lower(&mut canonical) == name
                        });
                    if temporary {
                        continue;
                    }
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
                && binding.get("condition").and_then(Value::as_str) == Some(condition_name(candidate))
                && (scenario.local().is_none() || (binding.get("source_selection") == Some(&scenario.binding()?)
                    && binding.get("budget") == run.get("budget"))),
                "retained arm belongs to a different run or condition");
        }
        if arm["status"] == "scored" && (arm["binding"].is_null()
            || arm["preparation"].is_null() || arm["starting_context"].is_null()
            || arm["committed_capture"].is_null() || arm["judgement"].is_null()
            || arm["binding"]["attempt_id"] != arm["attempt_id"]
            || arm["preparation"]["attempt_id"] != arm["attempt_id"]
            || arm["judgement"]["attempt_id"] != arm["attempt_id"]
            || (scenario.local().is_some() && arm["source_integrity"]["validated"] != true)) {
            arm["recorded_status"] = arm["status"].clone();
            arm["status"] = json!("incomplete");
        }
        let expected_kind = if candidate { obelisk::TASK_KIND } else { "history.query" };
        arm["recorded_reader_admission"] = arm["used_expected_reader"].clone();
        let admitted = retained_reader_admitted(&arm, &retrievals, expected_kind, &scenario)?;
        arm["used_expected_reader"] = json!(admitted);
        arm["retained_native_results"] = json!(retrievals);
        arms.push(arm);
    }
    let cleanup = read_optional_json(&root.join("cleanup.json"))?;
    let mut report = report_summary(arms, cleanup.as_ref().is_some_and(|value| value["cleanup_ok"] == true), &scenario)?;
    report["run_binding"] = run;
    report["scenario"] = report["run_binding"]["scenario"].clone();
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

struct Runtime<'a> {
    node: Arc<Mutex<Node>>,
    candidate: bool,
    capabilities: ProviderCapabilities,
    evidence_root: PathBuf,
    source_facts: Vec<Value>,
    local: Option<&'a SnapshotManifest>,
    retrievals: Mutex<Vec<Value>>,
}

impl SandboxRuntime for Runtime<'_> {
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
        let (evidence, rejection) = {
            let retained = self.retrievals.lock();
            let prior: Vec<_> = retained.iter().collect();
            match reader_evidence(definition, &result.outcome,
                result.result.as_ref(), &self.source_facts, self.local, &prior) {
                Ok(evidence) => (evidence, None),
                Err(error) => (Vec::new(), Some(format!("{error:#}"))),
            }
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
    scenario: &SelectedScenario,
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
        "run_binding":"../run-binding.json", "scenario":scenario.scenario(),
        "run_binding_digest":Digest::blake3(&std::fs::read(root.join("run-binding.json"))?),
        "source_selection":scenario.binding()?, "corpus_digest":scenario.corpus_digest(),
        "message_digest":Digest::blake3(scenario.question().as_bytes()),
        "rubric_digest":Digest::blake3(scenario.rubric().as_bytes()),
        "provider":provider_identity(handle)?,
        "input_configuration_digest":Digest::blake3(&serde_json::to_vec(configuration)?),
        "budget":{"arm_deadline_ms":deadline_ms, "context_tokens":context_tokens,
            "judge_max_output_tokens":8192, "sampling":"unchanged production defaults"},
        "originals":"installation Store/CAS and native-results; no automatic replay",
    }))?;
    let result = run_bound_arm(handle, &evidence_root, candidate, configuration, scenario,
        ArmBudget { deadline_ms, context_tokens }, attempt_id).await;
    if let Err(error) = &result {
        let failure = json!({"schema_version":2, "condition":name, "attempt_id":attempt_id,
            "scenario":scenario.scenario(), "source_selection":scenario.binding()?,
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
    scenario: &SelectedScenario,
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
    let mounted_root = installation.join("local-scenario");
    let mounted = match scenario.local() {
        Some(snapshot) => match snapshot.mount(&mounted_root) {
            Ok(mounted) => Some(mounted),
            Err(error) => {
                let retained = write_json(&evidence_root.join("source-integrity.json"), &json!({
                    "snapshot_digest":snapshot.manifest_digest(), "validated":false,
                    "original_snapshot_validated":false, "mounted_snapshot_validated":false,
                    "phase":"mount", "error":format!("{error:#}")}));
                return Err(match retained {
                    Ok(()) => error,
                    Err(recording) => error.context(format!("retain mount integrity failure: {recording:#}")),
                });
            }
        },
        None => None,
    };
    let result: Result<Value> = async {
    let mut source_facts = match &mounted {
        Some(snapshot) => local_source_facts(snapshot, &mounted_root, &prepared, candidate)?,
        None => Vec::new(),
    };
    let mut runtime_configuration_digest = None;
    if candidate {
        let mut config = configuration.clone();
        if let Some(snapshot) = &mounted {
            config = snapshot.configuration(configuration, &installation.join("obelisk-index"))?;
        } else if let SelectedScenario::Synthetic { corpus, .. } = scenario {
        let source_root = installation.join("synthetic-codex");
        source_facts = write_codex_corpus(&source_root, corpus)?;
        config.sources = vec![ObeliskSourceRoot {
            provider: ObeliskProvider::Codex,
            root_label: "synthetic-codex".into(),
            path: source_root,
        }];
        config.source_instance = SYNTHETIC_SOURCE_INSTANCE.into();
        config.index_home = installation.join("obelisk-index");
        } else {
            anyhow::bail!("local candidate has no mounted source snapshot");
        }
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
    } else if let SelectedScenario::Synthetic { corpus, .. } = scenario {
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
    }
    write_json(&evidence_root.join("source-facts.json"), &source_facts)?;
    let node = Arc::new(Mutex::new(Node::open(&installation.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node.clone()));
    let configuration_ref = seal(
        &store,
        &json!({"scenario":scenario.scenario(), "source_selection":scenario.binding()?,
        "corpus_digest":scenario.corpus_digest(), "message":scenario.question(),
        "rubric_digest":Digest::blake3(scenario.rubric().as_bytes()), "model":handle.binding().model(),
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
        "source_facts_file":"source-facts.json", "source_selection":scenario.binding()?,
        "inference_started":false,
    }))?;
    let stop = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
    let began = std::time::Instant::now();
    let runtime = Runtime {
        node, candidate, evidence_root:evidence_root.to_path_buf(), source_facts,
        local:mounted.as_ref().map(FrozenScenario::manifest), retrievals:Mutex::new(Vec::new()),
        capabilities:ProviderCapabilities {
            binding:handle.binding().clone(), max_context_tokens:Some(context_tokens), vision:None,
        },
    };
    let execution = run_prepared_sandbox(
        prepared,
        handle,
        catalog,
        scenario.question(),
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
        &json!({"schema_version":1, "scenario":scenario.scenario(), "source_selection":scenario.binding()?,
        "corpus":match scenario { SelectedScenario::Synthetic { corpus, .. } => Some(corpus), _ => None },
        "corpus_digest":scenario.corpus_digest(), "source_facts":runtime.source_facts,
        "retrievals":retrievals, "synthetic":scenario.local().is_none(),
        "claim_scope":"historical source statements with original-byte references; no independent rerun of their claims"}),
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
            rubric: scenario.rubric(),
            message: scenario.question(),
            output: &execution.committed.output,
            facts_ref: &facts_ref,
        },
        &SandboxJudgeContext {
            configuration_ref: &configuration_ref,
            installation_ref: &installation_ref,
            sample_digest: &sample_digest,
            scenario: scenario.scenario(),
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
        "scenario":scenario.scenario(), "source_selection":scenario.binding()?,
        "binding_file":evidence_root.join("arm-binding.json"),
        "status":if effective.is_some() { "scored" } else { "failed" },
        "effective":effective, "used_expected_reader":used_expected_reader,
        "history_query_reads":history_reads, "native_obelisk_cli_reads":cli_reads,
        "elapsed_ms":began.elapsed().as_millis(), "configuration_ref":configuration_ref,
        "installation_ref":installation_ref, "facts_ref":facts_ref, "sample_ref":sample_ref,
        "judge_receipt":judge_receipt, "judge_text":judge_text, "judge_error":judge_error,
        "committed":execution.committed});
    Ok(report)
    }.await;
    let result = if let Some(snapshot) = &mounted {
        let original = scenario.local().context("mounted local scenario lost its original binding")?;
        let original_error = original.validate().err().map(|error| format!("{error:#}"));
        let mounted_error = snapshot.validate().err().map(|error| format!("{error:#}"));
        let integrity = json!({"snapshot_digest":original.manifest_digest(),
            "original_snapshot_validated":original_error.is_none(),
            "mounted_snapshot_validated":mounted_error.is_none(),
            "validated":original_error.is_none() && mounted_error.is_none(),
            "original_error":original_error, "mounted_error":mounted_error});
        let retained = write_json(&evidence_root.join("source-integrity.json"), &integrity);
        let result = match result {
            Ok(mut report) if integrity["validated"] == true => {
                report["source_integrity"] = integrity.clone();
                Ok(report)
            }
            Ok(_) => Err(anyhow::anyhow!("local source integrity failed: {integrity}")),
            Err(error) if integrity["validated"] != true =>
                Err(error.context(format!("local source integrity also failed: {integrity}"))),
            Err(error) => Err(error),
        };
        match retained {
            Ok(()) => result,
            Err(recording) => match result {
                Ok(_) => Err(recording),
                Err(error) => Err(error.context(format!("retaining source integrity also failed: {recording:#}"))),
            },
        }
    } else { result };
    let report = result?;
    write_json(&evidence_root.join("acceptance.json"), &report)?;
    Ok(report)
}

fn main() -> Result<()> {
    let local_input = match parse_command(std::env::args_os().skip(1))? {
        Command::ReportOnly(root) => {
            println!("{}", serde_json::to_string_pretty(&reconstruct_report(&root)?)?);
            return Ok(());
        }
        Command::Run(input) => input,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(local_input))
}

async fn run(local_input: Option<PathBuf>) -> Result<()> {
    let run_id = EntityId::new();
    let selected_root = PathBuf::from(std::env::var_os("SHENKO_OBELISK_ACCEPTANCE_ROOT")
        .filter(|value| !value.is_empty())
        .context("select a private Life evidence parent with SHENKO_OBELISK_ACCEPTANCE_ROOT")?);
    ensure!(selected_root.is_absolute(), "SHENKO_OBELISK_ACCEPTANCE_ROOT must be absolute");
    let root = selected_root.join(format!("run-{run_id}"));
    let requested = if local_input.is_some() {
        json!({"kind":"local", "scenario":null, "corpus_digest":null, "capture_complete":false})
    } else {
        json!({"kind":"synthetic", "scenario":SCENARIO, "corpus_digest":Digest::blake3(CORPUS)})
    };
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
            "status":"blocked_external", "complete":false, "scenario":requested["scenario"],
            "requested_scenario":requested, "reason":"explicit real local model and CLI configuration are required",
            "missing":missing, "corpus_digest":requested["corpus_digest"], "scored":0}),
        )?;
        anyhow::bail!(
            "blocked_external: set SHENKO_USE_LOCAL_MODEL=1 and {required:?}; report {}",
            root.display()
        );
    }
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
                "status":"blocked_external", "complete":false, "scenario":requested["scenario"],
                "requested_scenario":requested, "reason":"the independently installed CLI failed its preflight",
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
    let scenario = match local_input {
        Some(input) => match FrozenScenario::capture(&input, &root.join("local-scenario")) {
            Ok(snapshot) => SelectedScenario::Local(Box::new(snapshot)),
            Err(error) => {
                let retained = write_json(&root.join("report.json"), &json!({"schema_version":2,
                    "status":"failed_preparation", "complete":false, "requested_scenario":requested,
                    "inference_started":false, "error":format!("{error:#}"), "replay_permitted":false}));
                return Err(match retained {
                    Ok(()) => error,
                    Err(recording) => error.context(format!("retain local preparation failure: {recording:#}")),
                });
            }
        },
        None => SelectedScenario::synthetic()?,
    };
    let (broker, handle) = Broker::new()?;
    if matches!(&scenario, SelectedScenario::Synthetic { .. }) {
        write_bytes(&root.join("fixture.json"), CORPUS)?;
    }
    let mut run_binding = json!({"schema_version":2, "run_id":run_id, "scenario":scenario.scenario(),
        "scenario_kind":if scenario.local().is_some() { "local" } else { "synthetic" },
        "planned_conditions":["baseline","candidate"], "trial":0,
        "source_selection":scenario.binding()?, "corpus_digest":scenario.corpus_digest(),
        "message_digest":Digest::blake3(scenario.question().as_bytes()),
        "rubric_digest":Digest::blake3(scenario.rubric().as_bytes()),
        "runner_source_digest":Digest::blake3(include_bytes!("obelisk_acceptance.rs")),
        "scenario_module_source_digest":Digest::blake3(include_bytes!("obelisk_local_scenario/mod.rs")),
        "executable_digest":shenko_kernel::native_artifact::native_artifact_digest()?,
        "provider":provider_identity(&handle)?,
        "input_configuration_digest":Digest::blake3(&serde_json::to_vec(&configuration)?),
        "budget":{"arm_deadline_ms":deadline_ms,"context_tokens":context_tokens,
            "judge_max_output_tokens":8192,"sampling":"unchanged production defaults"},
        "inference_started":false,
    });
    if scenario.local().is_none() {
        run_binding["fixture_digest"] = json!(Digest::blake3(CORPUS));
        run_binding["fixture_bytes"] = json!(CORPUS.len());
    }
    write_json(&root.join("run-binding.json"), &run_binding)?;
    let broker_task = tokio::spawn(broker.run());
    let mut arms = Vec::new();
    for candidate in [false, true] {
        match run_arm(
            &handle,
            &root,
            candidate,
            &configuration,
            &scenario,
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
                    "scenario":scenario.scenario(), "source_selection":scenario.binding()?,
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
    let mut report = report_summary(arms, cleanup_ok, &scenario)?;
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

    pub(super) fn retained_result(returned: Value) -> Result<TaskResult> {
        use shenko_types::{commit::CommitRef, ids::{LedgerSeq, RecordRevision, TaskRevision}, object::SchemaRef};
        let result_bytes = serde_json::to_vec(&returned)?;
        Ok(TaskResult {
            schema_version:1, result_id:EntityId::new(), task_id:EntityId::new(),
            task_revision:TaskRevision(1), task_record_revision:RecordRevision(1),
            invocation_id:EntityId::new(), operation_id:EntityId::new(), receipt_id:EntityId::new(),
            outcome:TaskOutcome::Succeeded, result:Some(returned),
            evidence_ref:ObjectRef {
                digest:Digest::blake3(&result_bytes), bytes:result_bytes.len() as u64,
                media_type:"application/json".into(), store_namespace:"obelisk-acceptance".into(),
                locator:"retained-native-result".into(), codec:SchemaRef {
                    name:"retained-native-result".into(), major:1,
                    digest:Digest::blake3(b"retained-native-result/v1"),
                },
            },
            commit_ref:CommitRef::new(EntityId::new(), LedgerSeq(1)),
        })
    }

    pub(super) fn cli_result(source_instance: &str, data: Value) -> Value {
        json!({"observation":{"result":{
            "schema_version":1,"status":"succeeded","source_instance":source_instance,
            "process":{"started":true,"reaped":true,"readers_joined":true,"readers_drained":true,
                "exit_code":0,"termination_requested":false,"stdout_truncated":false,
                "stderr_truncated":false,"custom_script_host_authority":false},
            "data":data,
        }}})
    }

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
            Some(&returned), &facts, None, &[])?.is_empty());
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.len(), 1);
        returned["events"] = json!([]);
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.is_empty());
        let report = report_summary(vec![
            json!({"status":"scored","effective":true,"used_expected_reader":false}),
            json!({"status":"scored","effective":true,"used_expected_reader":true}),
        ], true, &SelectedScenario::synthetic()?)?;
        assert_eq!(report["status"], "inconclusive_or_failed");
        assert_eq!(report["arms"][0]["effective"], true, "reader admission must not rewrite the judge score");
        definition.input = json!({"op":"read","event_id":event_id});
        returned = json!({"schema_version":1,"status":"ok","op":"read","event":{
            "event_id":event_id,"session_id":session_id,"seq":3,
            "event_ref":format!("session:{session_id}/event:{event_id}/seq:3"),
            "data":serde_json::to_string(&json!({"text":"arbitrary retained source text"}))?,
        }});
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.len(), 1);
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
        let mut returned = cli_result(SYNTHETIC_SOURCE_INSTANCE,
            json!({"mode":"fts5","hits":[{"message":{"uuid":"source-message","text":"arbitrary source text"},
                "session":{"id":session}}]}));
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.len(), 1);
        returned["observation"]["result"]["data"] = json!({"mode":"fts5","hits":[]});
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.is_empty());
        returned["observation"]["result"]["data"] = json!({"mode":"literal_index_fallback","hits":[{
            "uuid":"source-message","session_id":session,"text":"arbitrary source text"}]});
        assert_eq!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.len(), 1);
        returned["observation"]["result"]["status"] = json!("failed");
        assert!(reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&returned), &facts, None, &[])?.is_empty());
        Ok(())
    }

    struct EvidenceRoot(PathBuf);
    impl Drop for EvidenceRoot {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
    }

    #[test]
    fn interrupted_arm_reconstructs_original_identity_without_replay_or_replacement() -> Result<()> {
        use shenko_types::evidence::{DecodeStatus, EvidenceReceipt};

        let root = EvidenceRoot(fixture_parent()
            .join(format!("obelisk-offline-report-{}", EntityId::new())));
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
        let installation = arm_root.join("original-installation");
        write_json(&arm_root.join("installation.json"), &json!({
            "attempt_id":attempt_id,"installation":installation}))?;
        let event_id = EntityId::new();
        let session_id = EntityId::new();
        let source_facts = vec![json!({"event_id":event_id,"session_id":session_id,"seq":3})];
        write_json(&arm_root.join("source-facts.json"), &source_facts)?;
        write_json(&arm_root.join("prepared.json"), &json!({"attempt_id":attempt_id,
            "source_facts_digest":Digest::blake3(&serde_json::to_vec(&source_facts)?)}))?;
        let definition = NativeTaskDefinition {
            kind:"history.query".into(), definition_version:1, input:json!({"op":"search"}),
        };
        let returned = json!({"schema_version":1,"status":"ok","op":"search","events":[{
            "event_id":event_id,"session_id":session_id,"seq":3,
            "event_ref":format!("session:{session_id}/event:{event_id}/seq:3"),
            "data_preview":"arbitrary retained source text",
        }]});
        let result = retained_result(returned)?;
        let native_attempt_id = EntityId::new();
        let receipt = EvidenceReceipt {
            record_schema_version:1, receipt_id:result.receipt_id,
            operation_id:Some(result.operation_id), attempt_id:Some(native_attempt_id),
            source_id:definition.kind.clone(), source_instance_id:EntityId::new().to_string(),
            source_event_key:None, producer_epoch:Some(0), producer_schema_version:Some("1".into()),
            raw_ref:result.evidence_ref.clone(), decoder_ref:None, decode_status:DecodeStatus::Decoded,
            observation_level:None, normalized_ref:Some(result.evidence_ref.clone()),
            observed_at:None, received_commit:result.commit_ref,
        };
        let record = json!({"schema_version":1,"authenticated":true,
            "task_kind":definition.kind,"definition":definition,"result":result,
            "attempt_id":native_attempt_id,"receipt":receipt,"evidence_rejection":null,
            "retrieved_evidence":reader_evidence(&definition, &result.outcome,
                result.result.as_ref(), &source_facts, None, &[])?});
        let mut foreign = record.clone();
        for key in ["result_id", "task_id", "invocation_id", "operation_id", "receipt_id"] {
            foreign["result"][key] = json!(EntityId::new());
        }
        foreign["result"]["commit_ref"]["ledger_id"] = json!(EntityId::new());
        foreign["attempt_id"] = json!(EntityId::new());
        foreign["receipt"]["receipt_id"] = foreign["result"]["receipt_id"].clone();
        foreign["receipt"]["operation_id"] = foreign["result"]["operation_id"].clone();
        foreign["receipt"]["attempt_id"] = foreign["attempt_id"].clone();
        foreign["receipt"]["received_commit"] = foreign["result"]["commit_ref"].clone();
        foreign["receipt"]["source_instance_id"] = json!(EntityId::new());
        // A copied authenticated receipt can have valid text and source IDs.
        // It is still absent from this installation's committed consumption.
        let native_root = arm_root.join("native-results");
        write_json(&native_root.join("foreign.json"), &foreign)?;
        write_bytes(&native_root.join(format!(".evidence-{}", EntityId::new())), b"{unfinished")?;
        let failure = json!({"condition":"baseline","attempt_id":attempt_id,"status":"failed",
            "effective":null,"used_expected_reader":false,"error":"original subject timeout"});
        write_json(&arm_root.join("failure.json"), &failure)?;
        write_json(&arm_root.join("committed.json"), &json!({"installation":installation,
            "step_id":EntityId::new(),"consumed_results":[result],"output":"retained continuation"}))?;
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
        assert_eq!(first["arms"][0]["used_expected_reader"], false);
        assert_eq!(first["arms"][0]["retained_native_results"], json!([foreign]));
        assert_eq!(first["reconstruction"]["requests_issued"], false);

        let original_result = native_root.join(format!("{}.json", result.result_id));
        write_json(&original_result, &record)?;
        // The final link may already exist when a crash leaves its temp name.
        std::fs::hard_link(&original_result, native_root.join(format!(".evidence-{}", EntityId::new())))?;
        let admitted = reconstruct_report(&root.0)?;
        assert_eq!(admitted["arms"][0]["used_expected_reader"], true);
        assert_eq!(admitted["arms"][0]["retained_native_results"].as_array().map(Vec::len), Some(2));
        assert_eq!(admitted["arms"][0]["status"], "failed");
        assert_eq!(admitted["complete"], false);
        let records = admitted["arms"][0]["retained_native_results"].as_array().unwrap();
        let mut unbound = admitted["arms"][0].clone();
        unbound["committed_capture"]["installation"] = json!("a different installation");
        assert!(!retained_reader_admitted(&unbound, records, "history.query", &SelectedScenario::synthetic()?)?);
        unbound = admitted["arms"][0].clone();
        unbound["source_facts"][0]["session_id"] = json!(EntityId::new());
        assert!(!retained_reader_admitted(&unbound, records, "history.query", &SelectedScenario::synthetic()?)?);
        unbound["preparation"]["source_facts_digest"] =
            json!(Digest::blake3(&serde_json::to_vec(&unbound["source_facts"])?));
        assert!(!retained_reader_admitted(&unbound, records, "history.query", &SelectedScenario::synthetic()?)?,
            "a nonempty copied summary must still pass the original reader's source checks");
        assert!(write_json(&arm_root.join("failure.json"), &json!({"replacement":true})).is_err());
        assert_eq!(std::fs::read(arm_root.join("failure.json"))?, original);
        assert!(!root.0.join("report.json").exists());

        // Similar names are not atomic temps, including an uncommitted JSON
        // file. Malformed retained evidence must keep failing reconstruction.
        for name in [".evidence-not-an-id".into(), format!(".evidence-{}.json", EntityId::new()),
            format!(".evidence-{}", EntityId::nil())] {
            let path = native_root.join(name);
            write_bytes(&path, b"{unfinished")?;
            assert!(reconstruct_report(&root.0).is_err());
            std::fs::remove_file(path)?;
        }
        std::fs::create_dir(native_root.join(format!(".evidence-{}", EntityId::new())))?;
        assert!(reconstruct_report(&root.0).is_err(), "temp-shaped directories are unexpected entries");
        Ok(())
    }
}
