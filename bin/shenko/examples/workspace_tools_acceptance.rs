//! One real-model workspace task under the two installed editor profiles.
//!
//! cargo run -p shenko --example workspace_tools_acceptance -- \
//!   --base-url <OpenAI-compatible base URL> --model <served model> \
//!   --context-tokens <verified deployment limit> --output <new directory>
//!
//! This uses the production Store, subject, parser, finite model child and
//! native tool runtime. It never supplies a model response or a replacement
//! editor. One fixture is repeated with alternating condition order. Raw
//! installations and receipts stay outside the public summary under raw/.
//! A small-model diagnostic cannot establish the 27B reference acceptance or
//! a default winner. The execution-only condition is explicitly not covered.

use anyhow::{Context as _, Result, ensure};
use clap::Parser;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use shenko::{cli::ToolProfileCommand, workspace_tools};
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::{
    broker::{Broker, BrokerHandle},
    local_provider::LocalModelProvider,
    request::{InferenceResult, ProviderCapabilities},
};
use shenko_exec_host::{
    PreparedSandbox, SandboxCleanupFailure, SandboxRuntime, run_prepared_sandbox,
};
use shenko_kernel::{Supervisor, task::NativeTaskHandler};
use shenko_node::Node;
use shenko_store::{
    Store,
    external::{MODEL_CHILD_KIND, MODEL_CHILD_RESULT_OPERATION, MODEL_CHILD_STEP_OPERATION},
};
use shenko_subject::{
    ModelChildHandler, NativeChildService, NativeTaskHandlerPlugin, SubjectLoopState,
};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    operation::{EffectStatus, OperationOutcome},
    task::{TaskOutcome, TaskOwner},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, Instant},
};

const SCENARIO: &str = "active_shipping_policy_with_duplicate_thresholds";
const ACTIVE: &str = "services/checkout/shipping.json";
const EXAMPLE: &str = "examples/checkout/shipping.json";
const README: &str = "# Checkout service\n\nProduction checkout loads services/checkout/shipping.json.\nexamples/checkout/shipping.json is a documentation fixture, not the active policy.\nAmounts are integer CNY cents. Rules are selected by service.\n";
const CHILD_STEPS: u32 = 8;
const CHILD_TOOL_CALLS: u32 = 12;

#[derive(Parser, Serialize)]
#[command(about = "Bounded real-model workspace profile diagnostic; no mock fallback")]
struct Args {
    /// OpenAI-compatible base, with or without a trailing /v1. No credentials.
    #[arg(long, env = "SHENKO_LOCAL_MODEL_URL")]
    base_url: Option<String>,
    #[arg(long, env = "SHENKO_LOCAL_MODEL")]
    model: Option<String>,
    /// New output directory; an existing directory is never overwritten.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Independent repetitions of this single task, per editor.
    #[arg(long, default_value_t = 1)]
    trials: usize,
    /// Deadline for each complete subject/child run; cleanup is measured too.
    #[arg(long, default_value_t = 180)]
    deadline_secs: u64,
    /// Actual deployment context limit, never inferred from the model name.
    #[arg(long)]
    context_tokens: Option<u32>,
    /// Subject per-step cap and requested child per-step output reservation.
    #[arg(long, default_value_t = 2048)]
    max_output_tokens: u64,
    /// Controls fixture data only. Provider sampling/seed are not overridden.
    #[arg(long, default_value_t = 20261117)]
    fixture_seed: u64,
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", EntityId::new()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path).with_context(|| format!("publish {}", path.display()))
}

fn read_json(store: &Store, reference: &ObjectRef) -> Result<Value> {
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "retained CAS object identity mismatch"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn executable_sha256() -> Result<String> {
    let mut file = fs::File::open(std::env::current_exe()?)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    loop {
        let count = file.read(&mut bytes)?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn checkout_fact(args: &[&str]) -> Value {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    match Command::new("git").current_dir(repo).args(args).output() {
        Ok(output) => json!({"exit_code":output.status.code(),
            "stdout":String::from_utf8_lossy(&output.stdout),
            "stderr":String::from_utf8_lossy(&output.stderr)}),
        Err(error) => json!({"error":error.to_string()}),
    }
}

fn initial_policy(seed: u64) -> Value {
    let threshold = original_threshold(seed);
    json!({"currency":"CNY","rules":[
        {"service":"standard","free_shipping_min_cents":threshold,"fee_cents":600},
        {"service":"express","free_shipping_min_cents":threshold,"fee_cents":1200}
    ]})
}

fn original_threshold(seed: u64) -> u64 {
    8000 + (seed % 3) * 1000
}

fn prepare_fixture(root: &Path, seed: u64) -> Result<Value> {
    fs::create_dir_all(root.join("services/checkout"))?;
    fs::create_dir_all(root.join("examples/checkout"))?;
    fs::write(root.join("README.md"), README)?;
    let policy = initial_policy(seed);
    write_json(&root.join(ACTIVE), &policy)?;
    write_json(&root.join(EXAMPLE), &policy)?;
    fixture_snapshot(root)
}

fn fixture_snapshot(root: &Path) -> Result<Value> {
    let mut files = BTreeMap::new();
    for path in ["README.md", ACTIVE, EXAMPLE] {
        let bytes = fs::read(root.join(path))?;
        files.insert(
            path,
            json!({"digest":Digest::blake3(&bytes), "bytes":bytes.len(),
            "text":String::from_utf8_lossy(&bytes)}),
        );
    }
    Ok(json!(files))
}

fn shipping_fee(policy: &Value, service: &str, subtotal: u64) -> Result<u64> {
    ensure!(policy["currency"] == "CNY", "policy currency changed");
    let rules = policy["rules"].as_array().context("policy has no rules")?;
    let selected: Vec<_> = rules
        .iter()
        .filter(|rule| rule["service"] == service)
        .collect();
    ensure!(selected.len() == 1, "service must have exactly one policy");
    let rule = selected[0];
    let minimum = rule["free_shipping_min_cents"]
        .as_u64()
        .context("invalid threshold")?;
    let fee = rule["fee_cents"].as_u64().context("invalid fee")?;
    Ok(if subtotal >= minimum { 0 } else { fee })
}

// The oracle is withheld from the model. It evaluates the resulting policy's
// business behavior, not the model's prose, tool spelling or an expected diff.
fn inspect_outcome(root: &Path, seed: u64, before: &Value) -> Result<Value> {
    let after = fixture_snapshot(root)?;
    let policy: Value = serde_json::from_slice(&fs::read(root.join(ACTIVE))?)?;
    let old = original_threshold(seed);
    let new = old + 2000;
    let actual = [
        shipping_fee(&policy, "standard", old)?,
        shipping_fee(&policy, "standard", new - 1)?,
        shipping_fee(&policy, "standard", new)?,
        shipping_fee(&policy, "express", old - 1)?,
        shipping_fee(&policy, "express", old)?,
    ];
    let preserved = after["README.md"]["digest"] == before["README.md"]["digest"]
        && after[EXAMPLE]["digest"] == before[EXAMPLE]["digest"];
    Ok(
        json!({"passed":actual == [600, 600, 0, 1200, 0] && preserved,
        "observed_fees":actual, "readme_and_example_preserved":preserved,
        "after":after, "oracle":"shipping behavior and unrelated source preservation"}),
    )
}

fn message(args: &Args, model: &str) -> String {
    let old = original_threshold(args.fixture_seed) / 100;
    let new = old + 20;
    format!(
        "请实际修改这个临时项目当前启用的订单配送配置：普通配送的免运费门槛从 {old} 元提高到 {new} 元；\
         普通配送运费保持 6 元，加急配送的免运费门槛仍为 {old} 元、运费仍为 12 元。\
         项目说明标识了实际启用的配置，示例配置和项目说明均应保留。金额在文件中以整数分记录。\
         请将文件工作交给一个有限执行 child，给它授予本次目录中实际可用的工作区能力，由它自行查找、读取和修改，完成后回报实际结果。\
         child 使用当前实际模型 {model}，每步输出预算 {tokens}，总输出预留 {total}，最多 {CHILD_STEPS} 步、{CHILD_TOOL_CALLS} 次工具调用。\
         不必请求外发权限；若工具或预算不足，明确保留失败情况。",
        tokens = args.max_output_tokens,
        total = args.max_output_tokens * u64::from(CHILD_STEPS),
    )
}

struct Runtime {
    broker: BrokerHandle,
    editor: &'static str,
    max_output_tokens: u64,
    context_tokens: Option<u32>,
}

impl SandboxRuntime for Runtime {
    async fn install(
        &self,
        supervisor: &Supervisor,
        sandbox: &PreparedSandbox,
        _catalog: &Arc<CognitionCatalog>,
    ) -> Result<()> {
        // Exactly the CLI's persisted Node record and production startup
        // composition. The explicit root needs no guessed home mount.
        workspace_tools::install(supervisor, sandbox.store(), sandbox.installation(), &[]).await?;
        let child: Arc<dyn NativeTaskHandler> = Arc::new(ModelChildHandler::new(
            sandbox.store().clone(),
            self.broker.clone(),
            supervisor.context().clone(),
        )?);
        supervisor.spawn(NativeTaskHandlerPlugin, child).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let selected = format!("workspace.edit.{}", self.editor);
        let other = if self.editor == "replace" {
            "workspace.edit.hashline"
        } else {
            "workspace.edit.replace"
        };
        ensure!(
            native.has_handler(&selected, 1) && !native.has_handler(other, 1),
            "installed editor differs from the persisted condition"
        );
        write_json(
            &sandbox.installation().join("installed-tools.json"),
            &json!({
                "definitions":native.installed_definitions(),
                "prompt_sections":native.installed_prompt_sections(),
            }),
        )?;
        Ok(())
    }

    async fn prepare_subject(
        &self,
        _supervisor: &Supervisor,
        _sandbox: &PreparedSandbox,
        mut state: SubjectLoopState,
    ) -> Result<SubjectLoopState> {
        state.max_tokens = self.max_output_tokens;
        if let Some(context_tokens) = self.context_tokens {
            state = state.with_model_capabilities(Some(ProviderCapabilities {
                binding: self.broker.binding().clone(),
                max_context_tokens: Some(context_tokens),
                vision: None,
            }));
        }
        Ok(state)
    }
}

struct Arm {
    store: Arc<Store>,
    agent: EntityId,
    installation: PathBuf,
    editor: &'static str,
    trial: usize,
    consumed_children: BTreeSet<EntityId>,
    requested_child: Value,
    outcome: Value,
    cleanup_failed: bool,
}

async fn run_arm(
    handle: &BrokerHandle,
    raw: &Path,
    args: &Args,
    editor: &'static str,
    trial: usize,
) -> Result<Arm> {
    let prepared = PreparedSandbox::fresh(&format!("{editor}-{trial}"), raw)?;
    let installation = prepared.installation().to_path_buf();
    let store = prepared.store().clone();
    let agent = prepared.agent_id();
    let workspace = installation.join("workspace");
    let before = prepare_fixture(&workspace, args.fixture_seed)?;
    let message = message(args, handle.binding().model());
    let requested_child = json!({"model":handle.binding().model(),
        "max_tokens":args.max_output_tokens, "budget":{
            "max_total_tokens":args.max_output_tokens * u64::from(CHILD_STEPS),
            "max_steps":CHILD_STEPS, "max_tool_calls":CHILD_TOOL_CALLS}});
    write_json(&installation.join("before.json"), &before)?;
    write_json(
        &installation.join("task.json"),
        &json!({"message":message, "fixture_seed":args.fixture_seed,
        "provider_seed":null, "deadline_secs":args.deadline_secs,
        "subject_max_output_tokens":args.max_output_tokens,
        "requested_child_budget":{"per_step":args.max_output_tokens,
            "max_total_tokens":args.max_output_tokens * u64::from(CHILD_STEPS),
            "max_steps":CHILD_STEPS, "max_tool_calls":CHILD_TOOL_CALLS},
        "budget_basis":"output reservation, not total input/output usage; actual authored contract is retained"}),
    )?;
    let began = Instant::now();
    let result = async {
        workspace_tools::command(
            &installation,
            ToolProfileCommand::Configure {
                editor: editor.into(),
                root: Some(workspace.clone()),
            },
        )?;
        let node = Node::open(&installation.join("node"))?;
        write_json(
            &installation.join("profile.json"),
            &node.installed_unit_fact(workspace_tools::UNIT_ID)?,
        )?;
        let catalog = Arc::new(CognitionCatalog::new(Arc::new(Mutex::new(node))));
        run_prepared_sandbox(
            prepared,
            handle,
            catalog,
            &message,
            tokio::time::Instant::now() + Duration::from_secs(args.deadline_secs),
            &Runtime {
                broker: handle.clone(),
                editor,
                max_output_tokens: args.max_output_tokens,
                context_tokens: args.context_tokens,
            },
        )
        .await
    }
    .await;
    let elapsed_ms = began.elapsed().as_millis();
    let cleanup_failed = result
        .as_ref()
        .err()
        .is_some_and(|error| error.downcast_ref::<SandboxCleanupFailure>().is_some());
    let mut consumed_children = BTreeSet::new();
    if let Ok(execution) = &result {
        write_json(&installation.join("committed.json"), &execution.committed)?;
        for result in &execution.committed.consumed_results {
            if result.outcome == TaskOutcome::Succeeded
                && store.external_task(result.task_id)?.kind == MODEL_CHILD_KIND
            {
                consumed_children.insert(result.task_id);
            }
        }
    }
    let artifact = match inspect_outcome(&workspace, args.fixture_seed, &before) {
        Ok(oracle) => oracle,
        Err(error) => json!({"passed":false, "error":format!("{error:#}")}),
    };
    let outcome = json!({"editor":editor, "trial":trial, "installation":installation,
        "run_completed":result.is_ok(), "elapsed_ms_including_cleanup":elapsed_ms,
        "cleanup_failed":cleanup_failed, "artifact":artifact,
        "error":result.err().map(|error| format!("{error:#}")),
        "consumed_model_children":consumed_children});
    write_json(&installation.join("outcome.json"), &outcome)?;
    Ok(Arm {
        store,
        agent,
        installation,
        editor,
        trial,
        consumed_children,
        requested_child,
        outcome,
        cleanup_failed,
    })
}

fn usage(requests: &BTreeMap<EntityId, Option<InferenceResult>>) -> Value {
    let mut counts = serde_json::Map::new();
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        let mut known = 0u64;
        let mut unknown = 0usize;
        for result in requests.values() {
            let amount = result
                .as_ref()
                .and_then(|r| r.response.as_ref())
                .and_then(|r| r.usage.as_ref())
                .and_then(|usage| match field {
                    "input_tokens" => usage.input_tokens,
                    "output_tokens" => usage.output_tokens,
                    "cache_read_tokens" => usage.cache_read_tokens,
                    _ => usage.cache_write_tokens,
                });
            match amount {
                Some(value) => known = known.saturating_add(value),
                None => unknown += 1,
            }
        }
        counts.insert(
            field.into(),
            json!({"known_total":known, "unknown_requests":unknown,
            "total":if unknown == 0 { Some(known) } else { None }}),
        );
    }
    json!({"admitted_requests":requests.len(), "tokens":counts, "monetary_cost":null,
        "basis":"original provider usage, deduplicated by request_id; missing is not zero"})
}

// Read original receipts after the broker drain, including late/failed E1.
// No direct SQL, transcript heuristics, or replay is needed for measurement.
fn retain_evidence(arm: &Arm) -> Result<Value> {
    let store = &arm.store;
    let (head, _) = store.load_subject(arm.agent)?;
    let steps = store.sealed_subject_steps(&head.branch_id)?;
    let mut requests = BTreeMap::new();
    let mut subject = Vec::new();
    let mut child_contracts = BTreeMap::new();
    for step in &steps {
        let attempts = store.subject_request_attempts_for_step(arm.agent, step.step_id)?;
        for attempt in &attempts {
            requests.entry(attempt.request_id).or_insert(None);
            if let Some(reference) = attempt
                .terminal
                .as_ref()
                .and_then(|terminal| terminal.result_ref.as_ref())
            {
                let result: InferenceResult = serde_json::from_value(read_json(store, reference)?)?;
                ensure!(
                    result.request_id == attempt.request_id,
                    "subject request receipt identity mismatch"
                );
                requests.insert(result.request_id, Some(result));
            }
        }
        let frozen = step
            .input_plan_ref
            .as_ref()
            .map(|reference| read_json(store, reference))
            .transpose()?;
        let decision = step
            .decision_ref
            .as_ref()
            .map(|reference| read_json(store, reference))
            .transpose()?;
        if let Some(intents) = decision
            .as_ref()
            .and_then(|d| d["delegated_task_intents"].as_array())
        {
            for intent in intents {
                if intent["definition"]["kind"] == MODEL_CHILD_KIND {
                    child_contracts.insert(
                        intent["intent_id"]
                            .as_str()
                            .context("child intent has no identity")?
                            .to_string(),
                        intent["definition"]["input"].clone(),
                    );
                }
            }
        }
        subject
            .push(json!({"step":step, "frozen":frozen, "decision":decision, "attempts":attempts}));
    }
    let mut operations = Vec::new();
    let mut tasks = BTreeMap::new();
    let mut successful_edit_parents = BTreeSet::new();
    let mut tool_operations = 0usize;
    let mut unknown_operations = 0usize;
    let mut child_requests = BTreeSet::new();
    let mut emitted = BTreeMap::new();
    for name in [
        MODEL_CHILD_STEP_OPERATION,
        MODEL_CHILD_RESULT_OPERATION,
        "workspace.read.v1",
        "workspace.search.v1",
        "workspace.create.v1",
        "workspace.edit.replace.v1",
        "workspace.edit.hashline.v1",
    ] {
        let mut after = None;
        loop {
            let page = store.external_operations_after(name, after, 128)?;
            if page.is_empty() {
                break;
            }
            for operation in &page {
                let task = store.external_task(operation.task_id)?;
                let frozen = read_json(store, &operation.final_input_ref)?;
                let receipts = store.external_operation_evidence(operation.operation_id)?;
                if name == MODEL_CHILD_STEP_OPERATION {
                    let id: EntityId = frozen["request_id"]
                        .as_str()
                        .context("child step lacks request_id")?
                        .parse()?;
                    child_requests.insert(id);
                    requests.entry(id).or_insert(None);
                    for receipt in &receipts {
                        let raw = read_json(store, &receipt.raw_ref)?;
                        if let Ok(result) = serde_json::from_value::<InferenceResult>(raw) {
                            ensure!(
                                result.request_id == id,
                                "child request receipt identity mismatch"
                            );
                            requests.insert(id, Some(result));
                        }
                        if let Some(reference) = &receipt.normalized_ref {
                            let normalized = read_json(store, reference)?;
                            if normalized["request_id"].as_str() == Some(id.to_string().as_str())
                                && let Some(calls) = normalized["tool_calls"].as_array()
                            {
                                emitted.insert(id, calls.len());
                            }
                        }
                    }
                }
                if name == MODEL_CHILD_RESULT_OPERATION && frozen["spec"].is_object() {
                    child_contracts.insert(operation.task_id.to_string(), frozen.clone());
                }
                if name.starts_with("workspace.") {
                    tool_operations += 1;
                    if name == format!("workspace.edit.{}.v1", arm.editor)
                        && operation.outcome == Some(OperationOutcome::Succeeded)
                        && operation.effect_status == EffectStatus::Complete
                        && let TaskOwner::Task { id } = &task.owner
                    {
                        successful_edit_parents.insert(*id);
                    }
                }
                if operation.outcome.is_none() || operation.effect_status == EffectStatus::Unknown {
                    unknown_operations += 1;
                }
                operations.push(json!({"operation":operation, "frozen":frozen,
                    "attempts":store.external_operation_attempts(operation.operation_id)?, "receipts":receipts}));
                tasks.insert(
                    task.task_id,
                    json!({"task":task, "frozen":read_json(store, &task.input_ref)?}),
                );
                after = Some(operation.operation_id);
            }
        }
    }
    let consumed_edit_child = successful_edit_parents
        .iter()
        .any(|id| arm.consumed_children.contains(id));
    let complete_chain = consumed_edit_child && unknown_operations == 0;
    let budgets: Vec<_> = child_contracts.iter().map(|(id, contract)| json!({
        "task_id":id, "model":contract["spec"]["model"], "max_tokens":contract["max_tokens"],
        "budget":contract["budget"], "tools":contract["spec"]["tools"],
    })).collect();
    let budget_matches = !budgets.is_empty()
        && budgets.iter().all(|budget| {
            budget["model"] == arm.requested_child["model"]
                && budget["max_tokens"] == arm.requested_child["max_tokens"]
                && budget["budget"] == arm.requested_child["budget"]
        });
    let unobserved_child_steps = child_requests.len().saturating_sub(emitted.len());
    let emitted_count: usize = emitted.values().sum();
    let cost = usage(&requests);
    write_json(
        &arm.installation.join("evidence.json"),
        &json!({
            "subject":subject, "tasks":tasks, "operations":operations, "requests":requests,
            "child_contracts":child_contracts,
            "retained_store":"shenko.db", "retained_objects":"cas",
        }),
    )?;
    let success = arm.outcome["run_completed"] == true
        && arm.outcome["artifact"]["passed"] == true
        && complete_chain
        && !arm.cleanup_failed;
    let summary = json!({"editor":arm.editor, "trial":arm.trial, "success":success,
        "run_completed":arm.outcome["run_completed"], "artifact_passed":arm.outcome["artifact"]["passed"],
        "complete_subject_child_edit_chain":complete_chain, "subject_steps":steps.len(),
        "native_tool_operations":tool_operations, "unknown_operations":unknown_operations,
        "emitted_tool_calls":{"known_count":emitted_count, "unobserved_child_steps":unobserved_child_steps,
            "total":if unobserved_child_steps == 0 { Some(emitted_count) } else { None },
            "basis":"controls recognized by the actual child parser; malformed attempts are not counted"},
        "requested_child":arm.requested_child, "actual_children":budgets,
        "child_budget_matches_request":budget_matches,
        "elapsed_ms_including_cleanup":arm.outcome["elapsed_ms_including_cleanup"],
        "cost":cost, "raw_directory":arm.installation, "cleanup_failed":arm.cleanup_failed});
    write_json(&arm.installation.join("summary.json"), &summary)?;
    Ok(summary)
}

fn totals(samples: &[Value], editor: &str, planned: usize) -> Value {
    let samples: Vec<_> = samples
        .iter()
        .filter(|sample| sample["editor"] == editor)
        .collect();
    let successes = samples
        .iter()
        .filter(|sample| sample["success"] == true)
        .count();
    let elapsed: u64 = samples
        .iter()
        .filter_map(|sample| sample["elapsed_ms_including_cleanup"].as_u64())
        .sum();
    let elapsed_complete = samples
        .iter()
        .all(|sample| sample["elapsed_ms_including_cleanup"].is_u64());
    let mut tokens = serde_json::Map::new();
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        let values: Vec<_> = samples
            .iter()
            .map(|sample| &sample["cost"]["tokens"][field])
            .collect();
        let known: u64 = values
            .iter()
            .filter_map(|value| value["known_total"].as_u64())
            .sum();
        let complete = values.iter().all(|value| value["total"].is_u64());
        tokens.insert(field.into(), json!({"known_total":known,
            "total":if complete { Some(known) } else { None },
            "all_runs_per_success":if complete && successes > 0 { Some(known as f64 / successes as f64) } else { None }}));
    }
    json!({"planned":planned, "observed":samples.len(), "unstarted":planned.saturating_sub(samples.len()),
        "successes":successes, "success_rate_over_planned":successes as f64 / planned as f64,
        "known_elapsed_ms":elapsed,
        "all_runs_elapsed_ms":if elapsed_complete { Some(elapsed) } else { None },
        "all_runs_ms_per_success":if elapsed_complete && successes > 0 { Some(elapsed as f64 / successes as f64) } else { None },
        "child_budgets_comparable":!samples.is_empty() && samples.iter().all(|sample| sample["child_budget_matches_request"] == true),
        "tokens":tokens})
}

async fn run(args: Args) -> Result<()> {
    ensure!(
        (1..=16).contains(&args.trials) && (1..=900).contains(&args.deadline_secs),
        "trials must be 1..=16 and deadline-secs 1..=900"
    );
    ensure!(
        (1..=32768).contains(&args.max_output_tokens),
        "max-output-tokens must be 1..=32768"
    );
    ensure!(
        args.context_tokens
            .is_none_or(|limit| u64::from(limit) > args.max_output_tokens),
        "explicit context limit must exceed the output reservation"
    );
    let root = args.output.clone().unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../build/workspace-tools-acceptance")
            .join(format!("run-{}", EntityId::new()))
    });
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&root).context("output must be a new directory; preserve earlier evidence")?;
    let root = root.canonicalize()?;
    let raw = root.join("raw");
    fs::create_dir(&raw)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    write_json(
        &root.join("report.json"),
        &json!({"status":"starting", "scenario":SCENARIO,
        "evaluation_scope":"bounded_real_model_diagnostic", "provider_seed":null}),
    )?;
    let configured = || -> Result<_> {
        let base = args
            .base_url
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("--base-url or SHENKO_LOCAL_MODEL_URL is required")?
            .trim_end_matches('/');
        let base = base.strip_suffix("/v1").unwrap_or(base).to_string();
        let model = args
            .model
            .clone()
            .filter(|value| !value.trim().is_empty())
            .context("--model or SHENKO_LOCAL_MODEL is required")?;
        let provider = LocalModelProvider::new(base, model)
            .with_request_timeout_ms(args.deadline_secs * 1000)?;
        Broker::with_local_provider(provider, 1)
    };
    let (broker, handle) = match configured() {
        Ok(binding) => binding,
        Err(error) => {
            write_json(
                &raw.join("configuration-error.json"),
                &json!({"error":format!("{error:#}")}),
            )?;
            write_json(
                &root.join("report.json"),
                &json!({"status":"not_run", "scenario":SCENARIO,
                "reason":"explicit real provider configuration is unavailable", "scored":0,
                "details":"raw/configuration-error.json"}),
            )?;
            return Err(error);
        }
    };
    let binding_digest = Digest::blake3(&serde_json::to_vec(handle.binding())?);
    write_json(
        &raw.join("run.json"),
        &json!({"schema_version":1, "started_at":chrono::Utc::now(),
        "args":args, "binding":handle.binding(), "binding_digest":binding_digest,
        "provider_seed":null, "sampling":"unchanged provider defaults; exact payloads retained in CAS",
        "context_limit_basis":if args.context_tokens.is_some() { "explicit deployment input" } else { "unknown" },
        "scenario":SCENARIO, "source":{
            "checkout_head_at_run":checkout_fact(&["rev-parse","HEAD"]),
            "checkout_status_at_run":checkout_fact(&["status","--porcelain=v1","--untracked-files=normal"]),
            "binary_sha256":executable_sha256()?,
            "compiled_runner_digest":Digest::blake3(include_bytes!("workspace_tools_acceptance.rs")),
            "workspace_implementation_digest":shenko_exec_host::workspace_tools::implementation_digest(),
            "compiled_subject_prompt_source_digest":Digest::blake3(include_bytes!("../../../crates/shenko-subject/src/lib.rs")),
            "compiled_subject_parser_digest":Digest::blake3(include_bytes!("../../../crates/shenko-subject/src/output.rs")),
            "compiled_child_digest":Digest::blake3(include_bytes!("../../../crates/shenko-subject/src/model_child.rs")),
        },
        "uncovered":["execution-only disabled baseline","27B reference acceptance","stale snapshot","cross-file behavioral repair"],
        "default_selection":"reasoned provisional; this runner never changes the default"}),
    )?;
    let broker_task = tokio::spawn(broker.run());
    let mut arms = Vec::new();
    let mut samples = Vec::new();
    let mut stop_reason = None;
    'trials: for trial in 0..args.trials {
        let order = if trial % 2 == 0 {
            ["replace", "hashline"]
        } else {
            ["hashline", "replace"]
        };
        for editor in order {
            match run_arm(&handle, &raw, &args, editor, trial).await {
                Ok(mut arm) => {
                    // A cancelled caller is not proof that its provider call
                    // ended. Drain original readers before the next condition.
                    let began = Instant::now();
                    let drained = handle.quiesce_for_handoff().await;
                    let drain_ms = u64::try_from(began.elapsed().as_millis())?;
                    let elapsed = arm.outcome["elapsed_ms_including_cleanup"]
                        .as_u64()
                        .unwrap_or(0);
                    arm.outcome["elapsed_ms_including_cleanup"] =
                        json!(elapsed.saturating_add(drain_ms));
                    arm.outcome["broker_drain_ms"] = json!(drain_ms);
                    let unsettled = drained.as_ref().map_or(true, |value| {
                        value["requests"]
                            .as_array()
                            .is_none_or(|requests| !requests.is_empty())
                    });
                    write_json(
                        &arm.installation.join("broker-drain.json"),
                        &match &drained {
                            Ok(value) => value.clone(),
                            Err(error) => {
                                json!({"error":format!("{error:#}"),"remote_termination_claimed":false})
                            }
                        },
                    )?;
                    write_json(&arm.installation.join("outcome.json"), &arm.outcome)?;
                    let stopped = arm.cleanup_failed || unsettled;
                    arms.push(arm);
                    if stopped {
                        stop_reason = Some(
                            "original provider debt or runtime cleanup remains unresolved; later conditions were not started",
                        );
                        break 'trials;
                    }
                    handle.resume_after_handoff_abort()?;
                }
                Err(error) => {
                    write_json(
                        &raw.join(format!("setup-error-{editor}-{trial}.json")),
                        &json!({"error":format!("{error:#}")}),
                    )?;
                    samples.push(json!({"editor":editor,"trial":trial,"success":false,"status":"sample_unavailable"}));
                    stop_reason =
                        Some("sample collection failed; original sandbox data is retained");
                    break 'trials;
                }
            }
        }
    }
    let shutdown = handle.quiesce_for_handoff().await;
    write_json(
        &raw.join("broker-shutdown.json"),
        &match &shutdown {
            Ok(value) => value.clone(),
            Err(error) => json!({"error":format!("{error:#}"),"remote_termination_claimed":false}),
        },
    )?;
    broker_task.abort();
    let joined = broker_task.await;
    let cleanup_ok = shutdown.is_ok()
        && match joined {
            Ok(_) => true,
            Err(error) => error.is_cancelled(),
        };
    for arm in &arms {
        match retain_evidence(arm) {
            Ok(sample) => samples.push(sample),
            Err(error) => {
                write_json(
                    &arm.installation.join("evidence-error.json"),
                    &json!({"error":format!("{error:#}")}),
                )?;
                samples.push(
                    json!({"editor":arm.editor,"trial":arm.trial,"success":false,
                    "status":"evidence_incomplete","raw_directory":arm.installation}),
                );
            }
        }
    }
    let complete = samples.len() == args.trials * 2;
    let passed = complete && cleanup_ok && samples.iter().all(|sample| sample["success"] == true);
    write_json(
        &root.join("report.json"),
        &json!({"schema_version":1,
        "status":if passed { "passed_diagnostic" } else { "inconclusive_or_failed" },
        "evaluation_scope":"bounded_real_model_diagnostic", "scenario":SCENARIO,
        "model":handle.binding().model(), "binding_digest":binding_digest,
        "fixture_seed":args.fixture_seed, "provider_seed":null,
        "complete":complete, "cleanup_ok":cleanup_ok, "stop_reason":stop_reason,
        "finished_at":chrono::Utc::now(),
        "conditions":{"replace":totals(&samples,"replace",args.trials),
            "hashline":totals(&samples,"hashline",args.trials)},
        "samples":samples, "raw_metadata":"raw/run.json",
        "uncovered":["execution-only disabled baseline","27B reference acceptance","stale snapshot","cross-file behavioral repair"],
        "default_selection":"reasoned provisional; no winner selected"}),
    )?;
    eprintln!(
        "Workspace real-model diagnostic: {}",
        root.join("report.json").display()
    );
    ensure!(
        passed,
        "diagnostic did not complete successfully; original failures and Store/CAS are retained at {}",
        root.display()
    );
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(args))
}
