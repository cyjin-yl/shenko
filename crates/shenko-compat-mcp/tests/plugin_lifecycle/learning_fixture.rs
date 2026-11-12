//! Native consumers of the exact fixture used by compatibility learning.
use super::*;
use rmcp::model::{ClientRequest, GetMeta, ProgressToken};
use sha2::{Digest as _, Sha256};
use shenko_kernel::task::ChildToolOutcome;

const LEARNING_LEDGER: &[u8] = include_bytes!("../support/learning_workspace.py");

// Only transport timing changes: the unmodified learning fixture reads both
// requests before either response reaches the SDK. Both real operations thus
// remain armed while the fixture independently authenticates each request.
const PAIRED_TRANSPORT: &str = r#"import json, pathlib, runpy, sys
root = pathlib.Path(__file__).parent
original_in, original_out = sys.stdin, sys.stdout
held_ids, replies = set(), []

class PairedInput:
    def __iter__(self):
        for line in original_in:
            if json.loads(line).get('method') != 'tools/call':
                yield line
                continue
            (root / 'first-wire').write_bytes(b'arrived')
            other = original_in.readline()
            if not other or json.loads(other).get('method') != 'tools/call':
                raise RuntimeError('paired transport requires two actual calls')
            held_ids.update((json.loads(line)['id'], json.loads(other)['id']))
            yield line
            yield other

class PairedOutput:
    def __init__(self):
        self.buffer = ''
    def write(self, text):
        self.buffer += text
        while '\n' in self.buffer:
            line, self.buffer = self.buffer.split('\n', 1)
            if json.loads(line).get('id') in held_ids:
                replies.append(line)
                if len(replies) == 2:
                    original_out.write('\n'.join(replies) + '\n')
                    original_out.flush()
            else:
                original_out.write(line + '\n')
        return len(text)
    def flush(self):
        original_out.flush()

sys.stdin, sys.stdout = PairedInput(), PairedOutput()
runpy.run_path(str(root / 'original-ledger.py'), run_name='__main__')
"#;

struct Ledger {
    unit: UnitRef,
    script: PathBuf,
    binding: PathBuf,
}

impl Ledger {
    fn action(&self, mode: &str) -> Result<Vec<u8>> {
        let output = std::process::Command::new("/usr/bin/python3")
            .arg("-I").arg(&self.script).arg(&self.binding).arg(mode).output()?;
        ensure!(output.status.success(), "owned ledger {mode} failed: {}",
            String::from_utf8_lossy(&output.stderr));
        Ok(output.stdout)
    }
}

fn learning_root(label: &str) -> Result<PathBuf> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    Ok(root)
}

fn request_call(unit: &UnitRef, method: &str, params: Value) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall { call_id: task_id.to_string(), task_id, kind: "compat.mcp.request".into(), definition_version: 1,
        input: json!({"unit":unit,"request":{"method":method,"params":params},"timeout_secs":30}) }
}

fn install_ledger(store: &Store, node: &Arc<Mutex<Node>>, root: &Path, parent_id: EntityId,
    paired: bool) -> Result<Ledger> {
    let script = root.join("original-ledger.py");
    let binding = root.join("ledger-binding.json");
    std::fs::write(&script, LEARNING_LEDGER)?;
    let executable = if paired {
        let wrapper = root.join("paired-transport.py");
        std::fs::write(&wrapper, PAIRED_TRANSPORT)?;
        wrapper
    } else { script.clone() };
    let profile = McpConnection { description: "Owned ledger, bound to actual armed native producers".into(),
        provenance_refs: vec!["synthetic:learning-producer-binding".into()],
        connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(),
            args: vec!["-I".into(), executable.display().to_string(), binding.display().to_string()],
            cwd: Some(root.to_path_buf()), env_refs: BTreeMap::new() } };
    let export = profile.export("mcp.learning-producer-binding", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(),
        revision: export.root_revision.clone().context("revision")?,
        artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        let package_root = root.join("units").display().to_string();
        node.handle_unit_request(UnitRequest::Import { export: plugin_export(UNIT_ID, true)?,
            package_root: package_root.clone(), enable: Some(true) })?;
        node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
    }
    std::fs::write(&binding, serde_json::to_vec_pretty(&json!({
        "workspace":root.join("business"),"store_db":root.join("shenko.db"),"store_cas":root.join("cas"),
        "agent_id":store.external_task(parent_id)?.agent_id,"unit":&unit,
        "behavior":"applied","initial_balance":71 }))?)?;
    let ledger = Ledger { unit, script, binding };
    ledger.action("--initialize")?;
    Ok(ledger)
}

fn original_evidence(store: &Store, call: &ChildToolOutcome) -> Result<Value> {
    let operation = store.external_operation(call.operation_id)?;
    let attempt = store.external_operation_attempt(call.operation_id)?;
    let receipt_id = attempt.final_receipt_id.context("native attempt has no original E1")?;
    let receipt = store.external_evidence(receipt_id)?;
    ensure!(operation.task_id == call.task_id && receipt.receipt_id == receipt_id
        && receipt.operation_id == Some(operation.operation_id)
        && receipt.attempt_id == Some(attempt.attempt_id)
        && attempt.operation_id == operation.operation_id
        && attempt.armed_commit == operation.prepared_commit,
        "original E1 is not bound to the actual native operation/attempt");
    let normalized_ref = receipt.normalized_ref.as_ref().context("E1 has no normalized result")?;
    let bytes = store.cas.read_object(&normalized_ref.digest)?;
    ensure!(bytes.len() as u64 == normalized_ref.bytes && Digest::blake3(&bytes) == normalized_ref.digest,
        "normalized E1 bytes do not authenticate");
    let normalized: Value = serde_json::from_slice(&bytes)?;
    ensure!(normalized["source_receipt"] == serde_json::to_value(&receipt.raw_ref)?
        && call.result.as_ref() == Some(&normalized), "caller result lost original E1 provenance");
    let raw = store.cas.read_object(&receipt.raw_ref.digest)?;
    ensure!(raw.len() as u64 == receipt.raw_ref.bytes && Digest::blake3(&raw) == receipt.raw_ref.digest,
        "raw E1 bytes do not authenticate");
    ensure!(normalized["observation"] == serde_json::from_slice::<Value>(&raw)?,
        "normalized result changed the original observation");
    Ok(normalized)
}

fn verify_producer(store: &Store, call: &ChildToolOutcome, producer: &Value, params: &Value) -> Result<()> {
    let operation = store.external_operation(call.operation_id)?;
    let attempt = store.external_operation_attempt(call.operation_id)?;
    let reference: ObjectRef = serde_json::from_value(producer["input_ref"].clone())?;
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(producer["task_id"] == call.task_id.to_string()
        && producer["operation_id"] == call.operation_id.to_string()
        && producer["attempt_id"] == attempt.attempt_id.to_string()
        && reference == operation.final_input_ref && reference.digest == operation.input_digest
        && bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest
        && producer["input_sha256"] == format!("{:x}", Sha256::digest(&bytes)),
        "fixture producer is not the authenticated frozen native operation");
    let frozen: Value = serde_json::from_slice(&bytes)?;
    let mut projected: ClientRequest = serde_json::from_value(frozen["request"].clone())?;
    let token: ProgressToken = serde_json::from_value(params["_meta"]["progressToken"].clone())?;
    projected.get_meta_mut().set_progress_token(token);
    ensure!(serde_json::to_value(projected)?["params"] == *params,
        "observed wire differs from pinned SDK projection of original frozen input");
    original_evidence(store, call)?;
    Ok(())
}

fn authored_meta() -> Value {
    json!({"progressToken":"authored-token-overridden-by-sdk", "shenko":{
        "nested":{"keep":[1,2,{"deep":null}],"progressToken":"authored-nested-token"},
        "note":null,"flag":false}})
}

#[tokio::test]
async fn list_and_call_bind_one_armed_producer_despite_generated_progress_token() -> Result<()> {
    let root = learning_root("learning-producer-binding")?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let ledger = install_ledger(&store, &node, &root, parent_id, false)?;
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        let authored = authored_meta();
        let listed = native.call_tool(&request_call(&ledger.unit, "tools/list", json!({"cursor":null})), &parent, &authority).await?;
        let read = native.call_tool(&request_call(&ledger.unit, "tools/call",
            json!({"name":"ledger.read","arguments":null})), &parent, &authority).await?;
        let adjusted = native.call_tool(&request_call(&ledger.unit, "tools/call",
            json!({"name":"ledger.adjust","arguments":{"delta":23,"reference":"authored-metadata-1"},
                "_meta":&authored})), &parent, &authority).await?;
        let calls = [&listed, &read, &adjusted];
        ensure!(calls.iter().all(|call| call.outcome == "succeeded"), "native exchanges failed: {calls:?}");
        let snapshot: Value = serde_json::from_slice(&ledger.action("--snapshot")?)?;
        let exchanges = snapshot["exchanges"].as_array().context("exchanges missing")?;
        let requests: Vec<_> = exchanges.iter().filter(|row| row["phase"] == "request").collect();
        let commits: Vec<_> = exchanges.iter().filter(|row| row["phase"] == "business_commit").collect();
        ensure!(requests.len() == 3 && commits.len() == 1, "request/effect multiplicity changed: {snapshot}");
        for (call, request) in calls.iter().zip(&requests) {
            let producers = request["producers"].as_array().context("producers missing")?;
            ensure!(producers.len() == 1, "request has no unique armed producer: {request}");
            verify_producer(&store, call, &producers[0], &request["params"])?;
        }
        ensure!(requests[2]["params"]["_meta"]["shenko"] == authored["shenko"],
            "authored nested/null/bool metadata changed");
        let observation = original_evidence(&store, &adjusted)?;
        let result: Value = serde_json::from_str(observation["observation"]["protocol"]["result"]
            ["content"][0]["text"].as_str().context("adjustment observation missing")?)?;
        ensure!(result["status"] == "applied" && commits[0]["ticket"] == result["ticket"]
            && commits[0]["wire_request_id"] == requests[2]["wire_request_id"]
            && commits[0]["session_id"] == requests[2]["session_id"]
            && snapshot["balance"] == 94 && snapshot["submissions"].as_array().context("submissions missing")?.len() == 1,
            "ledger effect is not joined to the unique request and its original E1: {snapshot}");
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "listed":listed,"read":read,"adjusted":adjusted,"snapshot":snapshot,
            "scope":"native Store/child/rmcp, exact retained ledger fixture; synthetic business, no model efficacy"}))?)?;
        println!("learning producer receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}

#[tokio::test]
async fn ambiguous_identical_in_flight_producers_write_nothing() -> Result<()> {
    let root = learning_root("learning-ambiguous-producer")?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let ledger = install_ledger(&store, &node, &root, parent_id, true)?;
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let mut pending = tokio::task::JoinSet::new();
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        let params = json!({"name":"ledger.adjust","arguments":{"reference":"ambiguous-1","delta":7},
            "_meta":authored_meta()});
        let first = request_call(&ledger.unit, "tools/call", params.clone());
        let worker_native = native.clone();
        let worker_parent = parent.clone();
        let worker_authority = authority.clone();
        pending.spawn(async move { worker_native.call_tool(&first, &worker_parent, &worker_authority).await });
        tokio::time::timeout(Duration::from_secs(15), async {
            while !root.join("first-wire").exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.context("first actual request did not reach paired transport")?;
        let second = native.call_tool(&request_call(&ledger.unit, "tools/call", params), &parent, &authority).await?;
        let first = tokio::time::timeout(Duration::from_secs(20), pending.join_next()).await
            .context("first native call exceeded its guard")?.context("first call missing")???;
        ensure!(first.outcome == "failed" && second.outcome == "failed",
            "ambiguous requests did not both fail: {first:?} {second:?}");
        let snapshot: Value = serde_json::from_slice(&ledger.action("--snapshot")?)?;
        let exchanges = snapshot["exchanges"].as_array().context("exchanges missing")?;
        let requests: Vec<_> = exchanges.iter().filter(|row| row["phase"] == "request").collect();
        ensure!(requests.len() == 2 && snapshot["balance"] == 71
            && snapshot["submissions"].as_array().context("submissions missing")?.is_empty()
            && !exchanges.iter().any(|row| row["phase"] == "business_commit"),
            "ambiguous real producers changed business state: {snapshot}");
        let calls = [&first, &second];
        for request in &requests {
            let producers = request["producers"].as_array().context("producers missing")?;
            ensure!(producers.len() == 2, "contemporaneous ambiguity was not retained: {request}");
            for call in calls {
                let producer = producers.iter().find(|row| row["operation_id"] == call.operation_id.to_string())
                    .context("actual armed producer missing from original request observation")?;
                verify_producer(&store, call, producer, &request["params"])?;
            }
        }
        for call in calls {
            let evidence = original_evidence(&store, call)?;
            let rejection: Value = serde_json::from_str(evidence["observation"]["protocol"]["result"]
                ["content"][0]["text"].as_str().context("rejection missing")?)?;
            ensure!(rejection["status"] == "rejected" && rejection["reason"] == "no unique matching armed producer",
                "failure did not originate from the fixture's unique producer guard: {rejection}");
        }
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "first":first,"second":second,"snapshot":snapshot,
            "scope":"two real armed native operations; transport barrier only, exact retained fixture; zero business effects; no model efficacy"}))?)?;
        println!("ambiguous producer receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    pending.abort_all();
    while pending.join_next().await.is_some() {}
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}
