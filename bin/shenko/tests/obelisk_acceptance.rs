//! One opt-in, real-model comparison through the production subject sandbox.
//!
//! SHENKO_USE_LOCAL_MODEL=1 SHENKO_LOCAL_MODEL_URL=<base URL without /v1>
//! SHENKO_LOCAL_MODEL=<served model> SHENKO_OBELISK_CONFIG=<compatible config JSON>
//! cargo test -p shenko --test obelisk_acceptance -- --ignored --nocapture
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

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko::obelisk::{self, HistoryCommand, ObeliskCommand};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::broker::{Broker, BrokerHandle};
use shenko_exec_host::{
    Condition, HistoryHandler, JudgeReceiptIdentity, JudgeVerdict, PreparedSandbox,
    SandboxJudgeContext, SandboxJudgement, SandboxRuntime, judge_sandbox_output,
    obelisk_runtime::{ObeliskProvider, ObeliskRuntime, ObeliskSourceRoot},
    run_prepared_sandbox,
};
use shenko_kernel::{Supervisor, task::NativeTaskHandler};
use shenko_node::Node;
use shenko_store::Store;
use shenko_subject::NativeTaskHandlerPlugin;
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const CORPUS: &[u8] = include_bytes!("fixtures/obelisk_auth_history.json");
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
    std::fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
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
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "acceptance evidence CAS mismatch"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_codex_corpus(root: &Path, corpus: &[Message]) -> Result<Vec<Value>> {
    let sessions = root.join("sessions/2026/11");
    std::fs::create_dir_all(&sessions)?;
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
        std::fs::write(&path, &bytes)?;
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
}

async fn run_arm(
    handle: &BrokerHandle,
    root: &Path,
    candidate: bool,
    configuration: &Path,
    corpus: &[Message],
    deadline_ms: u64,
) -> Result<Value> {
    let name = if candidate { "candidate" } else { "baseline" };
    let prepared = PreparedSandbox::fresh(name, root)?;
    let installation = prepared.installation().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&installation, std::fs::Permissions::from_mode(0o700))?;
    }
    let store = prepared.store().clone();
    let mut source_facts = Vec::new();
    if candidate {
        let mut config = obelisk::load_config(configuration)?;
        let source_root = installation.join("synthetic-codex");
        source_facts = write_codex_corpus(&source_root, corpus)?;
        config.sources = vec![ObeliskSourceRoot {
            provider: ObeliskProvider::Codex,
            root_label: "synthetic-codex".into(),
            path: source_root,
        }];
        config.source_instance = "obelisk-acceptance/synthetic-v1".into();
        config.index_home = installation.join("obelisk-index");
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
            source_facts.push(serde_json::to_value(fact)?);
        }
    }
    let node = Arc::new(Mutex::new(Node::open(&installation.join("node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node.clone()));
    let configuration_ref = seal(
        &store,
        &json!({"scenario":SCENARIO,
        "corpus_digest":Digest::blake3(CORPUS), "message":MESSAGE, "model":handle.binding().model(),
        "deadline_ms":deadline_ms, "evaluation_scope":"bounded_production_smoke",
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
    let stop = tokio::time::Instant::now() + Duration::from_millis(deadline_ms);
    let began = std::time::Instant::now();
    let execution = run_prepared_sandbox(
        prepared,
        handle,
        catalog,
        MESSAGE,
        stop,
        &Runtime { node, candidate },
    )
    .await?;
    let mut retrievals = Vec::new();
    let mut history_reads = 0;
    let mut cli_reads = 0;
    for result in &execution.committed.consumed_results {
        let task = store.external_task(result.task_id)?;
        let input = read_json(&store, &task.input_ref)?;
        let definition_input = input.get("input").unwrap_or(&input);
        if task.kind == "history.query" {
            history_reads += 1;
        }
        if task.kind == obelisk::TASK_KIND && definition_input["op"] == "obelisk" {
            cli_reads += 1;
        }
        retrievals.push(json!({"task_kind":task.kind, "input_ref":task.input_ref,
            "input":input, "result":result}));
    }
    let facts_ref = seal(
        &store,
        &json!({"schema_version":1, "corpus":corpus,
        "corpus_digest":Digest::blake3(CORPUS), "source_facts":source_facts,
        "retrievals":retrievals, "synthetic":true,
        "claim_scope":"historical statements in a controlled fixture, not independent verification of code changes"}),
        "observed-facts",
    )?;
    let sample_ref = seal(&store, &execution.committed, "committed-output")?;
    let sample_digest = sample_ref.digest.to_string();
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
    let used_expected_reader = if candidate {
        cli_reads > 0
    } else {
        history_reads > 0
    };
    let report = json!({"condition":name, "installation":installation,
        "status":if effective.is_some() { "scored" } else { "failed" },
        "effective":effective, "used_expected_reader":used_expected_reader,
        "history_query_reads":history_reads, "native_obelisk_cli_reads":cli_reads,
        "elapsed_ms":began.elapsed().as_millis(), "configuration_ref":configuration_ref,
        "installation_ref":installation_ref, "facts_ref":facts_ref, "sample_ref":sample_ref,
        "judge_receipt":judge_receipt, "judge_text":judge_text, "judge_error":judge_error,
        "committed":execution.committed});
    write_json(&installation.join("acceptance.json"), &report)?;
    Ok(report)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a reachable real local model and independently installed pinned Obelisk source; never mocked"]
async fn same_corpus_history_research_through_real_model_and_cli() -> Result<()> {
    let root = std::env::var_os("SHENKO_OBELISK_ACCEPTANCE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/obelisk-acceptance")
        })
        .join(format!("run-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let required = [
        "SHENKO_LOCAL_MODEL_URL",
        "SHENKO_LOCAL_MODEL",
        "SHENKO_OBELISK_CONFIG",
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
    let configuration =
        PathBuf::from(std::env::var_os("SHENKO_OBELISK_CONFIG").context("config path")?);
    let installation_proof = obelisk::load_config(&configuration)
        .and_then(ObeliskRuntime::new)
        .and_then(|runtime| runtime.verify_installation());
    match installation_proof {
        Ok(proof) => write_json(&root.join("upstream-installation.json"), &proof)?,
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
    }
    let deadline_ms = std::env::var("SHENKO_OBELISK_ACCEPTANCE_DEADLINE_MS")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(180_000);
    ensure!(
        (30_000..=900_000).contains(&deadline_ms),
        "arm deadline must be 30..=900 seconds"
    );
    let (broker, handle) = Broker::new()?;
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
        )
        .await
        {
            Ok(report) => arms.push(report),
            Err(error) => {
                let cleanup_failed = error
                    .downcast_ref::<shenko_exec_host::SandboxCleanupFailure>()
                    .is_some();
                arms.push(json!({"condition":if candidate { "candidate" } else { "baseline" },
                    "status":"failed", "error":format!("{error:#}"), "cleanup_failed":cleanup_failed}));
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
    let complete = arms.len() == 2 && arms.iter().all(|arm| arm["status"] == "scored");
    let passed = complete
        && cleanup_ok
        && arms
            .iter()
            .all(|arm| arm["effective"] == true && arm["used_expected_reader"] == true);
    write_json(
        &root.join("report.json"),
        &json!({"schema_version":1,
        "status":if passed { "passed_smoke" } else { "inconclusive_or_failed" },
        "scenario":SCENARIO, "planned_arms":2, "complete":complete,
        "cleanup_ok":cleanup_ok, "corpus_digest":Digest::blake3(CORPUS),
        "shutdown_artifact_error":shutdown_artifact.err().map(|error| format!("{error:#}")),
        "evaluation_scope":"one bounded production-path comparison; no efficacy or token-saving claim",
        "arms":arms}),
    )?;
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
