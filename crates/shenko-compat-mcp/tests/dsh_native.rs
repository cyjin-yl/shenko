//! One real DSH -> MCP -> native-child -> Store journey. This exercises the
//! installed upstream DSH runtime, not a protocol stand-in or a model provider.
//!
//! Linux, the repository Rust toolchain and Node >=24.9 are required:
//! npm --prefix adapters/dsh ci --ignore-scripts
//! cargo test -p shenko-compat-mcp --test dsh_native --locked -- --ignored --nocapture
//!
//! SHENKO_DSH_NODE selects Node (default: node on PATH).
//! SHENKO_DSH_EVIDENCE selects the JSON receipt file. The real Store/CAS remain
//! under SHENKO_COMPAT_EVIDENCE_ROOT or target/compat-native-fixtures.
#![cfg(target_os = "linux")]

use std::{collections::BTreeMap, path::{Path, PathBuf}, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use shenko_compat::{installation::UnitRef, runtime::AdapterHost};
use shenko_compat_mcp::{McpConnection, McpPlugin, UNIT_ID};
use shenko_kernel::{Supervisor, task::{ChildToolCall, ChildToolOutcome}};
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService};
use shenko_types::{commit::CommitRef, ids::*, operation::{AuthorityRef, EffectStatus, OperationState}, subject::*, task::*};

const CONNECTION: &str = "dsh.native-journey";
const REQUEST: &str = "compat.mcp.request";

fn initialize(store: &Store) -> Result<(EntityId, AuthorityRef)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let zero = CommitRef::new(ledger_id, LedgerSeq::ZERO);
    let object = store.cas.seal_object(b"DSH native integration fixture; no model", "text/plain", "fixture", "raw", 1)?.object;
    let genesis = SubjectGenesis { genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(), initial_profile_ref: object.clone(), home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(), created_commit: zero };
    let head = SubjectBranchHead { branch_origin: None, agent_id: agent, branch_id: BranchId::new(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef { ledger_id, genesis_id }, committed_step_no: 0, committed_step_id: None, committed_at: zero,
        next_step_id: first_id, mode: SubjectMode::Active, mode_reason_ref: None, record_revision: RecordRevision(0) };
    let first = SubjectStep { step_id: first_id, branch_id: head.branch_id.clone(), step_no: 1, expected_parent_commit: zero,
        controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
        input_plan_ref: None, context_revision: None, active_request_id: None, active_attempt_id: None, model_result_ref: None,
        model_outcome_ref: None, decision_ref: None, commit_ref: None, retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None };
    let parent = TaskRecord::subject_root(head.subject_root_task_id, agent, object, zero);
    store.initialize_genesis(&genesis, &head, &first, &parent)?;
    store.start_subject_dispatcher()?;
    Ok((parent.task_id, AuthorityRef { delegation_id: parent.task_id, delegation_revision: RecordRevision(0), decision_ref: genesis_id }))
}

fn request(unit: &UnitRef, method: &str, params: Value) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall { call_id: task_id.to_string(), task_id, kind: REQUEST.into(), definition_version: 1,
        input: json!({"unit":unit,"request":{"method":method,"params":params},"timeout_secs":15}) }
}

async fn command(command: &mut tokio::process::Command) -> Result<std::process::Output> {
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(15), command.output()).await
        .context("DSH prerequisite/profile command exceeded 15 seconds")??;
    ensure!(output.status.success(), "DSH prerequisite/profile command failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(output)
}

fn sha256(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

fn witness(root: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(root.join("effects.txt"))?)
}

fn pids(root: &Path) -> Result<Vec<u32>> {
    std::fs::read_to_string(root.join("pids.txt"))?.lines()
        .map(|line| line.parse().context("invalid DSH process witness")).collect()
}

fn joined(pid: u32) -> bool { !Path::new(&format!("/proc/{pid}")).exists() }

fn durable(store: &Store, outcome: &ChildToolOutcome) -> Result<Value> {
    let operation = store.external_operation(outcome.operation_id)?;
    let attempts = store.external_operation_attempts(outcome.operation_id)?;
    let receipts = store.external_operation_evidence(outcome.operation_id)?;
    ensure!(attempts.len() == 1 && receipts.len() == 1, "one DSH call produced multiple attempts or receipts");
    let raw = store.cas.read_object(&receipts[0].raw_ref.digest)?;
    let normalized = receipts[0].normalized_ref.as_ref()
        .map(|reference| store.cas.read_object(&reference.digest)).transpose()?;
    Ok(json!({"operation":operation,"attempts":attempts,"receipts":receipts,
        "raw":serde_json::from_slice::<Value>(&raw)?,
        "normalized":normalized.as_deref().map(serde_json::from_slice::<Value>).transpose()?,
        "task_result":store.native_task_result(outcome.task_id)?}))
}

async fn mount(supervisor: &Supervisor, store: &Arc<Store>, node: &Arc<Mutex<Node>>) -> Result<Arc<NativeChildService>> {
    supervisor.publish_services().await?;
    supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
    supervisor.spawn(McpPlugin, AdapterHost::new(supervisor, store.clone(), node.clone())).await?;
    Ok(supervisor.context().try_service::<NativeChildService>()?)
}

async fn wait_handler(native: &NativeChildService, enabled: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        while native.has_handler(REQUEST, 1) != enabled {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.context("installation observer did not publish/withdraw MCP")?;
    Ok(())
}

fn stopped<T>(outcome: Result<T>, cleanup: Result<()>) -> Result<T> {
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("DSH fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(value), Ok(())) => Ok(value), (Ok(_), Err(error)) => Err(error),
    }
}

#[tokio::test]
#[ignore = "requires Node >=24.9 and installed adapters/dsh dependencies; run explicitly with --ignored"]
async fn dsh_tools_keep_native_receipts_across_restart_and_disable() -> Result<()> {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    let adapter = repo.join("adapters/dsh");
    let node_bin = std::env::var_os("SHENKO_DSH_NODE").unwrap_or_else(|| "node".into());
    let runtime: Value = serde_json::from_slice(&command(tokio::process::Command::new(node_bin)
        .args(["-p", "JSON.stringify({version:process.version,execPath:process.execPath})"])).await
        .context("install Node >=24.9 or set SHENKO_DSH_NODE to its executable")?.stdout)?;
    let version = runtime["version"].as_str().context("Node version missing")?;
    let mut components = version.trim_start_matches('v').split('.');
    let major: u32 = components.next().context("Node major")?.parse()?;
    let minor: u32 = components.next().context("Node minor")?.parse()?;
    ensure!((major, minor) >= (24, 9), "DSH integration needs Node >=24.9, found {version}");
    let executable = runtime["execPath"].as_str().context("Node executable missing")?;
    let manifest: Value = serde_json::from_slice(&std::fs::read(adapter.join("package.json"))?)?;
    let mut dependencies = BTreeMap::new();
    for (name, expected) in manifest["dependencies"].as_object().context("DSH dependency manifest")? {
        let path = adapter.join("node_modules").join(name).join("package.json");
        let installed: Value = serde_json::from_slice(&std::fs::read(&path)
            .with_context(|| format!("missing DSH dependency {name}; run npm --prefix adapters/dsh ci --ignore-scripts"))?)?;
        ensure!(installed["version"] == *expected, "{name} differs from the pinned DSH manifest; run npm ci");
        dependencies.insert(name.clone(), installed["version"].clone());
    }
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| repo.join("target/compat-native-fixtures"));
    let root = base.join(format!("dsh-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let receipt_path = std::env::var_os("SHENKO_DSH_EVIDENCE").map(PathBuf::from).unwrap_or_else(|| root.join("receipt.json"));
    std::fs::write(root.join("effects.txt"), "")?;
    std::fs::write(root.join("pids.txt"), "")?;
    // Observe the owned OS process while preserving the existing DSH fixture
    // byte-for-byte. All tools and execution still come from the real module.
    let fixture = adapter.join("test/fixtures/protocol-plugin.mjs");
    let wrapper = format!("import {{ appendFileSync }} from 'node:fs';\nimport {{ pathToFileURL }} from 'node:url';\nconst plugin = await import(pathToFileURL({}).href);\nexport const name = plugin.name;\nexport const inject = plugin.inject;\nexport function apply(ctx, config) {{\n  appendFileSync(config.pidWitness, String(process.pid) + '\\n');\n  return plugin.apply(ctx, config);\n}}\n", serde_json::to_string(&fixture)?);
    std::fs::write(root.join("plugin.mjs"), wrapper)?;
    let config = root.join("bridge.json");
    std::fs::write(&config, serde_json::to_vec_pretty(&json!({"schemaVersion":1,
        "plugins":[{"id":"native-witness","entry":root.join("plugin.mjs"),
            "config":{"witness":root.join("effects.txt"),"pidWitness":root.join("pids.txt")}}],
        "callTimeoutMs":3000,"shutdownGraceMs":1000}))?)?;
    let profile_bytes = command(tokio::process::Command::new(executable)
        .arg(adapter.join("src/cli.mjs")).arg("profile").arg(&config)).await?.stdout;
    std::fs::write(root.join("connection.json"), &profile_bytes)?;
    let profile: McpConnection = serde_json::from_slice(&profile_bytes)?;
    let export = profile.export(CONNECTION, false)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("connection revision")?,
        artifact_digest: export.root_artifact_digest.clone().context("connection digest")? };
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    node.lock().handle_unit_request(UnitRequest::Import { export, package_root: root.join("units").display().to_string(), enable: Some(false) })?;
    let add = request(&unit, "tools/call", json!({"name":"add","arguments":{"a":17,"b":25}}));
    let uncertain = request(&unit, "tools/call", json!({"name":"uncertain","arguments":{}}));

    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let first: Result<Value> = async {
        let native = mount(&supervisor, &store, &node).await?;
        ensure!(!native.has_handler(REQUEST, 1) && pids(&root)?.is_empty() && witness(&root)?.is_empty(),
            "profile/import or disabled installation executed a DSH plugin");
        let mut editor = Node::open(&root.join("node"))?;
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: true })?;
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: CONNECTION.into(), enabled: true })?;
        wait_handler(&native, true).await?;
        ensure!(pids(&root)?.is_empty(), "enabling an installation launched its external process before a native request");
        let parent = store.external_task(parent_id)?;
        let listed = native.call_tool(&request(&unit, "tools/list", json!({})), &parent, &authority).await?;
        ensure!(listed.outcome == "succeeded", "DSH tools/list failed: {listed:?}");
        let tools = listed.result.as_ref().context("tools/list receipt missing")?["observation"]["protocol"]["result"]["tools"]
            .as_array().context("DSH tools/list omitted its tools")?;
        ensure!(tools.len() == 2 && tools.iter().any(|tool| tool["name"] == "add" && tool["inputSchema"]["properties"]["a"]["type"] == "integer")
            && tools.iter().any(|tool| tool["name"] == "uncertain"), "DSH discovery did not expose the registered tools");
        let added = native.call_tool(&add, &parent, &authority).await?;
        ensure!(added.outcome == "succeeded" && added.result.as_ref().context("addition receipt missing")?
            ["observation"]["protocol"]["result"]["structuredContent"]["value"] == 42, "DSH native result differs: {added:?}");
        ensure!(store.native_task_result(add.task_id)?.is_some(), "successful DSH result did not settle on Store");
        let unknown = native.call_tool(&uncertain, &parent, &authority).await?;
        let operation = store.external_operation(unknown.operation_id)?;
        ensure!(unknown.outcome == "unknown" && operation.state == OperationState::Reconciling
            && operation.effect_status == EffectStatus::Unknown && operation.outcome.is_none()
            && store.native_task_result(uncertain.task_id)?.is_none(), "post-effect DSH timeout manufactured completion: {unknown:?}");
        ensure!(witness(&root)? == "add\neffect-before-timeout\ndisposed\n", "effect/cleanup multiplicity changed");
        let observed_pids = pids(&root)?;
        ensure!(observed_pids.len() == 1 && joined(observed_pids[0]), "unknown session retained its owned DSH process");
        Ok(json!({"list":listed,"add":added,"unknown":unknown,
            "success_durable":durable(&store, &added)?,"unknown_durable":durable(&store, &unknown)?}))
    }.await;
    let cleanup = supervisor.stop().await;
    let first = stopped(first, cleanup)?;
    drop(supervisor);
    drop(node);
    drop(store);

    // Reopen the actual database and installation; no result/operation is
    // copied into a substitute store and no model response is fabricated.
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    store.start_subject_dispatcher()?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let resumed: Result<Value> = async {
        let native = mount(&supervisor, &store, &node).await?;
        native.recover()?;
        // Give the ordinary dispatcher its real pending Unknown operation.
        // Its one-second retry eligibility is local scheduling, not permission
        // to execute that operation again. The closing barrier joins this work.
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if native.run_once().await? > 0 { return anyhow::Ok(()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.context("native recovery never admitted the original Unknown for reconciliation")??;
        let parent = store.external_task(parent_id)?;
        let added = native.call_tool(&add, &parent, &authority).await?;
        let unknown = native.call_tool(&uncertain, &parent, &authority).await?;
        ensure!(serde_json::to_value(&added)? == first["add"] && serde_json::to_value(&unknown)? == first["unknown"],
            "restart changed original DSH call identity or observation");
        ensure!(durable(&store, &added)? == first["success_durable"] && durable(&store, &unknown)? == first["unknown_durable"],
            "restart changed original operation, attempt, E1 or T2");
        ensure!(pids(&root)?.len() == 1 && witness(&root)? == "add\neffect-before-timeout\ndisposed\n",
            "recovery or repeated child call replayed the DSH plugin");
        let listed = native.call_tool(&request(&unit, "tools/list", json!({})), &parent, &authority).await?;
        ensure!(listed.outcome == "succeeded", "explicit discovery after reopen failed: {listed:?}");
        let observed_pids = pids(&root)?;
        ensure!(observed_pids.len() == 2 && !joined(observed_pids[1]), "explicit request did not own a fresh DSH process");
        let mut editor = Node::open(&root.join("node"))?;
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: CONNECTION.into(), enabled: false })?;
        tokio::time::timeout(Duration::from_secs(8), async {
            while !joined(observed_pids[1]) { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.context("disabled DSH connection retained its process")?;
        ensure!(unit.installed(&node.lock(), UNIT_ID).is_err() && native.has_handler(REQUEST, 1),
            "connection disable did not preserve the independently enabled MCP adapter");
        let denied_call = request(&unit, "tools/call", json!({"name":"add","arguments":{"a":1,"b":2}}));
        let denied = native.call_tool(&denied_call, &parent, &authority).await?;
        ensure!(denied.outcome == "failed" && store.external_task_operations(denied_call.task_id, REQUEST)?.is_empty(),
            "disabled connection acquired an external attempt: {denied:?}");
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: false })?;
        wait_handler(&native, false).await?;
        let unavailable = native.call_tool(&request(&unit, "tools/list", json!({})), &parent, &authority).await?;
        ensure!(unavailable.outcome == "unavailable", "disabled MCP retained its native capability");
        Ok(json!({"add":added,"unknown":unknown,"explicit_list":listed,"connection_disabled":denied,
            "adapter_disabled":unavailable,"connection_fact":node.lock().installed_unit_fact(CONNECTION)?,
            "success_durable":durable(&store, &added)?,"unknown_durable":durable(&store, &unknown)?}))
    }.await;
    let cleanup = supervisor.stop().await;
    let mut resumed = stopped(resumed, cleanup)?;
    // Read again after the actual native/adapter closing barriers. A snapshot
    // taken while reconciliation was still live cannot attest final stability.
    let added: ChildToolOutcome = serde_json::from_value(resumed["add"].clone())?;
    let unknown: ChildToolOutcome = serde_json::from_value(resumed["unknown"].clone())?;
    resumed["success_durable"] = durable(&store, &added)?;
    resumed["unknown_durable"] = durable(&store, &unknown)?;
    ensure!(resumed["success_durable"] == first["success_durable"] && resumed["unknown_durable"] == first["unknown_durable"],
        "disable/reconciliation changed the retained DSH evidence");
    let recovery_task = store.external_task(uncertain.task_id)?;
    ensure!(matches!(&recovery_task.state, TaskState::Pending { .. })
        && matches!(&recovery_task.dispatch_eligibility, DispatchEligibility::Parked { .. })
        && recovery_task.memos.get(shenko_store::child::NATIVE_PARK).and_then(Value::as_str) == Some("reconciling")
        && recovery_task.memos.get(shenko_store::child::NATIVE_RECOVERY).and_then(Value::as_bool) == Some(true),
        "original Unknown did not remain available for reconciliation: {recovery_task:?}");
    resumed["recovery_task_after_shutdown"] = serde_json::to_value(recovery_task)?;
    let observed_pids = pids(&root)?;
    ensure!(observed_pids.len() == 2 && observed_pids.iter().copied().all(joined)
        && witness(&root)? == "add\neffect-before-timeout\ndisposed\ndisposed\n", "shutdown left a process or replayed an effect");
    let mut sources = BTreeMap::new();
    for relative in ["package.json", "package-lock.json", "src/config.mjs", "src/cli.mjs", "src/host.mjs", "src/server.mjs", "test/fixtures/protocol-plugin.mjs"] {
        sources.insert(relative, sha256(&adapter.join(relative))?);
    }
    let receipt = json!({"schema_version":1,"scope":"real DSH upstream runtime, generated MCP profile, Node installation, native-child, Store/CAS, shutdown/reopen, Unknown reconciliation and disable; no model efficacy or arbitrary plugin compatibility",
        "node":runtime,"dependencies":dependencies,"source_sha256":sources,
        "source_revision":std::env::var("SHENKO_SOURCE_REVISION").ok(),
        "test_sha256":sha256(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/dsh_native.rs"))?,
        "process_witness_wrapper_sha256":sha256(&root.join("plugin.mjs"))?,"config_sha256":sha256(&config)?,
        "native_artifact_digest":shenko_kernel::native_artifact::native_artifact_digest()?,
        "unit":unit,"profile":profile,"store_root":root,"first_generation":first,"reopened_generation":resumed,
        "effect_witness":witness(&root)?,"owned_processes_joined":observed_pids});
    if let Some(parent) = receipt_path.parent().filter(|parent| !parent.as_os_str().is_empty()) { std::fs::create_dir_all(parent)?; }
    std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
    println!("DSH native receipt: {}", receipt_path.display());
    Ok(())
}
