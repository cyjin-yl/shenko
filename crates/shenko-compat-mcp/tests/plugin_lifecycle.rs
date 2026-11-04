//! Real stdio effects through the native task/operation ledger, followed by an
//! independently versioned cognition unit. No model efficacy is inferred here.
#![cfg(target_os = "linux")]

use std::{collections::BTreeMap, path::{Path, PathBuf}, sync::Arc, time::Duration};
use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use shenko_compat::installation::{UnitRef, plugin_export};
use shenko_compat::runtime::AdapterHost;
use shenko_compat::usage::NAME as USAGE_DIGEST;
use shenko_compat_mcp::{McpConnection, McpPlugin, UNIT_ID, protocol::ConnectionSpec};
use shenko_kernel::Supervisor;
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_subject::{ChildToolCall, NativeChildPlugin, NativeChildService, NativeTaskHandler, NativeTaskHandlerPlugin};
use shenko_types::{commit::CommitRef, ids::*, object::{Digest, ObjectRef, SchemaRef}, operation::AuthorityRef, subject::*, task::*};

const SERVER: &str = r#"import json, os, pathlib, sqlite3, sys, time
root = pathlib.Path(sys.argv[1])
(root / 'server.pid').write_text(str(os.getpid()))
for line in sys.stdin:
    message = json.loads(line)
    method, ident = message.get('method'), message.get('id')
    if ident is None:
        continue
    if method == 'initialize':
        result = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'native-effect-fixture','version':'1'}}
    elif method == 'tools/call':
        params = message['params']
        task = params['arguments']['task_id']
        db = sqlite3.connect('file:' + str(root / 'shenko.db') + '?mode=ro', uri=True)
        row = db.execute("SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?", [task]).fetchone()
        assert row is not None, 'effect attempted before T1'
        operation = json.loads(row[0])
        attempt = json.loads(db.execute('SELECT data FROM attempts WHERE operation_id=?', [operation['operation_id']]).fetchone()[0])
        assert attempt['dispatch_stage'] == 'armed', attempt
        db.close()
        with (root / 'effects.jsonl').open('a') as out:
            out.write(json.dumps({'task_id':task,'operation_id':operation['operation_id'],'attempt_id':attempt['attempt_id'],'name':params['name']}) + '\n')
            out.flush(); os.fsync(out.fileno())
        if params['name'] == 'drop_after_effect':
            os._exit(0)
        if params['name'] == 'hold':
            time.sleep(60)
        if params['name'] == 'large_observation':
            result = {'content':[{'type':'text','text':'x' * 1360000}],'isError':False}
        else:
            failed = params['name'] == 'reported_error'
            result = {'content':[{'type':'text','text':'effect recorded'}],'isError':failed}
    elif method == 'tools/list':
        result = {'tools':[]}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32601,'message':'unknown method'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#;

fn initialize(store: &Store) -> Result<(EntityId, AuthorityRef)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let zero = CommitRef::new(ledger_id, LedgerSeq::ZERO);
    let object = store.cas.seal_object(b"synthetic compatibility native fixture", "text/plain", "fixture", "raw", 1)?.object;
    let genesis = SubjectGenesis { genesis_id, subject_id: agent, bootstrap_id: EntityId::new(), launch_id: EntityId::new(),
        seed_manifest_ref: object.clone(), initial_profile_ref: object.clone(), home_manifest_ref: object.clone(),
        bootstrap_event_ref: object.clone(), created_commit: zero };
    let head = SubjectBranchHead { agent_id: agent, branch_id: BranchId::new(), subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1), software_generation: RuntimeGeneration("compat-native-fixture".into()),
        genesis_ref: GenesisRef { ledger_id, genesis_id }, committed_step_no: 0, committed_step_id: None, committed_at: zero,
        next_step_id: first_id, mode: SubjectMode::Active, mode_reason_ref: None, record_revision: RecordRevision(0) };
    let first = SubjectStep { step_id: first_id, branch_id: head.branch_id.clone(), step_no: 1, expected_parent_commit: zero,
        controller_epoch: head.controller_epoch, software_generation: head.software_generation.clone(), state: SubjectStepState::Planned,
        input_plan_ref: None, context_revision: None, active_request_id: None, active_attempt_id: None, model_result_ref: None,
        model_outcome_ref: None, decision_ref: None, commit_ref: None, retry_reason_ref: None, record_revision: RecordRevision(0), created_by_step_id: None };
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, zero);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    store.start_subject_dispatcher()?;
    Ok((root.task_id, AuthorityRef { delegation_id: root.task_id, delegation_revision: RecordRevision(0), decision_ref: genesis_id }))
}

fn effects(root: &Path) -> Result<Vec<Value>> {
    match std::fs::read_to_string(root.join("effects.jsonl")) {
        Ok(text) => text.lines().map(|line| serde_json::from_str(line).map_err(Into::into)).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn call(unit: &UnitRef, name: &str) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall { call_id: task_id.to_string(), task_id, kind: "compat.mcp.request".into(), definition_version: 1,
        input: json!({"unit":unit,"request":{"method":"tools/call","params":{"name":name,"arguments":{"task_id":task_id}}},"timeout_secs":10}) }
}

#[tokio::test]
async fn request_deadline_includes_blocked_transport_and_timeout_notification() -> Result<()> {
    use shenko_compat_mcp::protocol::{Response, Session};
    use tokio_util::sync::CancellationToken;
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("blocked-transport-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), r#"import fcntl, json, os, pathlib, sys, time
root = pathlib.Path(sys.argv[1])
fcntl.fcntl(sys.stdin.fileno(), fcntl.F_SETPIPE_SZ, 4096)
(root / 'server.pid').write_text(str(os.getpid()))
for line in sys.stdin:
    message = json.loads(line)
    if message.get('id') is None:
        continue
    if message.get('method') == 'initialize':
        result = {'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'blocked-transport-fixture','version':'1'}}
        print(json.dumps({'jsonrpc':'2.0','id':message['id'],'result':result}), flush=True)
        # The child remains alive but no longer consumes its 4 KiB input pipe.
        # Its own finite lifetime also bounds cleanup of the unfixed client.
        time.sleep(4)
        break
    print(json.dumps({'jsonrpc':'2.0','id':message['id'],'error':{'code':-32601,'message':'unknown method'}}), flush=True)
"#)?;
    let spec = ConnectionSpec::Stdio { program: "/usr/bin/python3".into(),
        args: vec![root.join("server.py").display().to_string(), root.display().to_string()],
        cwd: Some(root.clone()), env_refs: BTreeMap::new() };
    let mut session = Session::connect(&spec, Duration::from_secs(5)).await?;
    let request = serde_json::from_value(json!({"method":"tools/call","params":{"name":"blocked","arguments":{"payload":"x".repeat(60_000)}}}))?;
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(1), session.request(request, Duration::from_millis(200), &CancellationToken::new())).await;
    let elapsed = started.elapsed();
    // Always join before asserting the observed failure; do not abandon the
    // external fixture when exercising the broken deadline implementation.
    let cleanup = tokio::time::timeout(Duration::from_secs(8), session.close()).await;
    let pid = std::fs::read_to_string(root.join("server.pid"))?;
    let joined = !Path::new(&format!("/proc/{}", pid.trim())).exists();
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
        "deadline_millis":200,"observer_guard_millis":1000,"elapsed_millis":elapsed.as_millis(),
        "request":format!("{result:?}"),"cleanup":format!("{cleanup:?}"),"owned_process_joined":joined,
        "scope":"Linux real stdio backpressure; protocol deadline, not native ledger or model efficacy"
    }))?)?;
    println!("blocked transport receipt: {}", root.join("receipt.json").display());
    cleanup.context("blocked transport cleanup exceeded its guard")??;
    ensure!(joined, "blocked transport child was not joined");
    ensure!(matches!(result, Ok(Ok(Response::Unknown { reason: "request_deadline_elapsed" }))),
        "request deadline failed to cover backpressure: {result:?}");
    Ok(())
}

#[tokio::test]
async fn streamable_http_transport_bounds_json_and_sse() -> Result<()> {
    use shenko_compat_mcp::protocol::{Response, Session};
    use tokio_util::sync::CancellationToken;
    const HTTP_SERVER: &str = r#"import json, os, pathlib, sys, time
from http.server import ThreadingHTTPServer, BaseHTTPRequestHandler
root = pathlib.Path(sys.argv[1])
posts = open(root/'posts.jsonl','ab',buffering=0)
class H(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(*a): pass
    def authed(self): return self.headers.get('X-Fixture-Auth')=='secret-token-value'
    def unauth(self):
        self.send_response(401); self.send_header('WWW-Authenticate','Bearer realm="fixture"'); self.send_header('Content-Length','0'); self.end_headers()
    def empty(self,status):
        self.send_response(status); self.send_header('Content-Length','0'); self.end_headers()
    def sse(self,payload):
        body=('data: '+json.dumps(payload)+'\r\n\r\n').encode()
        self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Mcp-Session-Id','fixture-session-1'); self.send_header('Content-Length',str(len(body))); self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        open(root/'server.gets.log','ab').write(b'get\n')
        if not self.authed(): return self.unauth()
        self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Transfer-Encoding','chunked'); self.end_headers()
        self.wfile.write(b'5\r\n: hi\n\r\n'); self.wfile.flush()
        while True:
            time.sleep(30)
            try: self.wfile.write(b'5\r\n: hi\n\r\n'); self.wfile.flush()
            except OSError: return
    def do_DELETE(self): self.empty(200)
    def do_POST(self):
        if not self.authed(): return self.unauth()
        message=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0')) or b'0') or b'{}')
        posts.write((json.dumps({'method':message.get('method'),'id':message.get('id')})+'\n').encode())
        method,ident=message.get('method'),message.get('id')
        if method=='initialize':
            self.sse({'jsonrpc':'2.0','id':ident,'result':{'protocolVersion':'2025-11-25','capabilities':{'tools':{}},'serverInfo':{'name':'http-fixture','version':'1'}}})
        elif ident is None:
            self.empty(202)
        elif method=='tools/call':
            name=message['params']['name']
            if name=='large_json':
                body=json.dumps({'jsonrpc':'2.0','id':ident,'result':{'content':[{'type':'text','text':'x'*200000}],'isError':False}}).encode()
                self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
            elif name=='large_sse':
                body=('data: '+json.dumps({'jsonrpc':'2.0','id':ident,'result':{'big':'x'*(17*1024*1024)}})+'\r\n\r\n').encode()
                self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(body))); self.end_headers()
                try: self.wfile.write(body)
                except OSError: pass
            else:
                self.sse({'jsonrpc':'2.0','id':ident,'result':{'content':[{'type':'text','text':'echo:'+name}],'isError':False}})
        else:
            self.sse({'jsonrpc':'2.0','id':ident,'error':{'code':-32601,'message':'unknown method'}})
port=int(sys.argv[2])
server=ThreadingHTTPServer(('127.0.0.1',port),H)
(root/'server.bound').write_text(str(server.server_address[1]))
(root/'server.pid').write_text(str(os.getpid()))
server.serve_forever()
"#;
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("http-transport-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), HTTP_SERVER)?;
    let stderr = std::fs::File::create(root.join("server.stderr"))?;
    // Bind the port in the test and hand it over, so there is no file-based
    // handshake to lag on NFS; the fixture owns an already-chosen address.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port().to_string();
    drop(listener);
    let mut server = std::process::Command::new("/usr/bin/python3")
        .arg(root.join("server.py")).arg(&root).arg(&port)
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(stderr)
        .spawn()?;
    let started = std::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port.parse::<u16>()?)).is_ok() { break; }
        ensure!(started.elapsed() < Duration::from_secs(15), "HTTP fixture never bound a port");
        ensure!(server.try_wait()?.is_none(), "HTTP fixture exited before binding");
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { std::env::set_var("SHENKO_HTTP_FIXTURE_TOKEN", "secret-token-value"); }
    let spec = ConnectionSpec::StreamableHttp { url: format!("http://127.0.0.1:{}/mcp", port.trim()),
        header_env_refs: BTreeMap::from([("X-Fixture-Auth".into(), "SHENKO_HTTP_FIXTURE_TOKEN".into())]) };
    let outcome: Result<()> = async {
        let mut session = Session::connect(&spec, Duration::from_secs(5)).await
            .with_context(|| format!("HTTP fixture did not negotiate at port {port} (bound={:?} gets={:?} posts={:?})",
                std::fs::read_to_string(root.join("server.bound")).ok(),
                std::fs::read_to_string(root.join("server.gets.log")).ok(),
                std::fs::read_to_string(root.join("posts.jsonl")).ok()))?;
        let info = session.server_info()?;
        ensure!(info["serverInfo"]["name"] == "http-fixture", "legacy HTTP negotiation failed: {info}");
        let request = |name: &str| serde_json::from_value(json!({"method":"tools/call","params":{"name":name,"arguments":{}}})).unwrap();
        let echo = session.request(request("echo"), Duration::from_secs(5), &CancellationToken::new()).await?;
        ensure!(matches!(&echo, Response::Received { result } if result["content"][0]["text"] == "echo:echo"),
            "HTTP tools/call did not observe its result: {echo:?}");
        let large_json = session.request(request("large_json"), Duration::from_secs(5), &CancellationToken::new()).await?;
        ensure!(matches!(&large_json, Response::Unknown { .. }), "oversized JSON body was accepted: {large_json:?}");
        let large_sse = session.request(request("large_sse"), Duration::from_secs(10), &CancellationToken::new()).await?;
        ensure!(matches!(&large_sse, Response::Unknown { .. }), "oversized SSE event was accepted: {large_sse:?}");
        tokio::time::timeout(Duration::from_secs(8), session.close()).await
            .context("HTTP session close exceeded its guard")??;
        Ok(())
    }.await;
    server.kill().ok();
    let _ = server.wait();
    outcome?;
    let mut counts = BTreeMap::<String, usize>::new();
    for line in std::fs::read_to_string(root.join("posts.jsonl"))?.lines() {
        let line: Value = serde_json::from_str(line)?;
        *counts.entry(line["method"].as_str().unwrap_or("null").to_owned()).or_default() += 1;
    }
    // server/discover once is the Auto lifecycle bootstrap (the fixture's
    // sessioned SSE error exercised the legacy-discover fallback); anything
    // duplicated is a replay.
    ensure!(counts == BTreeMap::from([
        ("server/discover".to_owned(), 1), ("initialize".to_owned(), 1),
        ("notifications/initialized".into(), 1),
        ("tools/call".into(), 3)]), "HTTP request was replayed or dropped: {counts:?}");
    let pid = std::fs::read_to_string(root.join("server.pid"))?;
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
        "port":port.trim(),"observed_counts":counts,"joined_pid":pid.trim(),
        "scope":"real HTTP server over TCP; legacy session+SSE, bounded JSON body and SSE event; no model efficacy"}))?)?;
    println!("http transport receipt: {}", root.join("receipt.json").display());
    Ok(())
}

#[tokio::test]
async fn installed_changes_retire_connections_and_independent_plugins() -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("hot-installation-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), SERVER)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let package_root = root.join("units").display().to_string();
    let mut profile = McpConnection { description: "first connection revision".into(), provenance_refs: vec!["synthetic:hot-installation".into()],
        connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(), args: vec![root.join("server.py").display().to_string(), root.display().to_string()], cwd: Some(root.clone()), env_refs: BTreeMap::new() } };
    let export = profile.export("mcp.hot-fixture", true)?;
    let first_unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?, artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    let skill_entry = b"---\nname: retained-source\ndescription: Independent retained source\n---\nOriginal instructions.\n";
    let skill = shenko_compat_skill::SkillSource::capture(&store.cas, vec!["synthetic:hot-installation".into()],
        [Ok(shenko_compat_skill::SourceInput { path: "SKILL.md".into(), executable: false,
            bytes: skill_entry.len() as u64, reader: std::io::Cursor::new(skill_entry) })])?;
    let skill_export = skill.export("skill.hot-fixture", true)?;
    let skill_unit = UnitRef { unit_id: skill_export.root_unit_id.clone(), revision: skill_export.root_revision.clone().context("revision")?, artifact_digest: skill_export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        for export in [plugin_export(UNIT_ID, true)?, plugin_export(shenko_compat_skill::UNIT_ID, true)?, export, skill_export] {
            node.handle_unit_request(UnitRequest::Import { export, package_root: package_root.clone(), enable: Some(true) })?;
        }
        for unit_id in [UNIT_ID, shenko_compat_skill::UNIT_ID] {
            node.handle_unit_request(UnitRequest::SetEnabled { unit_id: unit_id.into(), enabled: false })?;
        }
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        supervisor.spawn(shenko_compat_skill::SkillPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        ensure!(!native.has_handler("compat.mcp.request", 1) && !native.has_handler("compat.skill.source", 2), "disabled startup published native definitions");
        ensure!(!root.join("server.pid").exists(), "installation watcher launched a server before any task");
        // Mutation uses another real Node connection, as the CLI does. The
        // running adapter must not require a shared in-memory notification.
        let mut editor = Node::open(&root.join("node"))?;
        for unit_id in [UNIT_ID, shenko_compat_skill::UNIT_ID] {
            editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: unit_id.into(), enabled: true })?;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !native.has_handler("compat.mcp.request", 1) || !native.has_handler("compat.skill.source", 2) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await?;
        let first = native.call_tool(&call(&first_unit, "record"), &parent, &authority).await?;
        ensure!(first.outcome == "succeeded", "first exchange failed");
        let first_pid = std::fs::read_to_string(root.join("server.pid"))?;
        profile.description = "replacement connection revision".into();
        let replacement = profile.export(&first_unit.unit_id, true)?;
        let second_unit = UnitRef { unit_id: replacement.root_unit_id.clone(), revision: replacement.root_revision.clone().context("revision")?, artifact_digest: replacement.root_artifact_digest.clone().context("digest")? };
        editor.handle_unit_request(UnitRequest::Import { export: replacement, package_root: package_root.clone(), enable: Some(true) })?;
        tokio::time::timeout(Duration::from_secs(8), async {
            while Path::new(&format!("/proc/{}", first_pid.trim())).exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.context("replaced idle connection remained alive")?;
        let second = native.call_tool(&call(&second_unit, "record"), &parent, &authority).await?;
        ensure!(second.outcome == "succeeded", "replacement connection was not usable");
        let pending = call(&second_unit, "hold");
        let pending_id = pending.task_id;
        let worker_native = native.clone();
        let worker_parent = parent.clone();
        let worker_authority = authority.clone();
        let worker = tokio::spawn(async move { worker_native.call_tool(&pending, &worker_parent, &worker_authority).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if effects(&root)?.iter().any(|effect| effect["task_id"] == pending_id.to_string()) { return anyhow::Ok(()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        let second_pid = std::fs::read_to_string(root.join("server.pid"))?;
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: false })?;
        let interrupted = tokio::time::timeout(Duration::from_secs(8), worker).await???;
        ensure!(interrupted.outcome == "unknown", "disable asserted remote completion");
        tokio::time::timeout(Duration::from_secs(8), async {
            while Path::new(&format!("/proc/{}", second_pid.trim())).exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.context("disabled plugin retained its process")?;
        let absent = native.call_tool(&call(&second_unit, "record"), &parent, &authority).await?;
        ensure!(absent.outcome == "unavailable", "disabled plugin retained its native definition: {absent:?}");
        let skill_call = || { let task_id = EntityId::new(); ChildToolCall { call_id: task_id.to_string(), task_id,
            kind: "compat.skill.source".into(), definition_version: 2, input: json!({"unit":skill_unit,"query":{"path":"SKILL.md"}}) } };
        let skill_result = native.call_tool(&skill_call(), &parent, &authority).await?;
        ensure!(skill_result.outcome == "succeeded", "MCP disable withdrew the independent Skill plugin");
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: true })?;
        // Discovery, not repeated external requests, waits for publication.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if native.has_handler("compat.mcp.request", 1) { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await?;
        let resumed = native.call_tool(&call(&second_unit, "record"), &parent, &authority).await?;
        ensure!(resumed.outcome == "succeeded", "re-enabled MCP was not usable");
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: shenko_compat_skill::UNIT_ID.into(), enabled: false })?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while native.has_handler("compat.skill.source", 2) { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await?;
        let skill_absent = native.call_tool(&skill_call(), &parent, &authority).await?;
        ensure!(skill_absent.outcome == "unavailable", "disabled Skill retained its definition");
        let mcp_independent = native.call_tool(&call(&second_unit, "record"), &parent, &authority).await?;
        ensure!(mcp_independent.outcome == "succeeded", "Skill disable withdrew MCP");
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: shenko_compat_skill::UNIT_ID.into(), enabled: true })?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !native.has_handler("compat.skill.source", 2) { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await?;
        let skill_resumed = native.call_tool(&skill_call(), &parent, &authority).await?;
        ensure!(skill_resumed.outcome == "succeeded"
            && skill_resumed.result.as_ref().context("re-enabled Skill body")?["observation"]
                == skill_result.result.as_ref().context("original Skill body")?["observation"],
            "re-enabled Skill changed the retained source observation");
        ensure!(effects(&root)?.len() == 5, "reconciliation replayed an external effect");
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "idle_replacement_pid_joined":first_pid.trim(),"disabled_pid_joined":second_pid.trim(),
            "first":first,"replacement":second,"interrupted":interrupted,"disabled":absent,"independent_skill":skill_result,
            "reenabled":resumed,"disabled_skill":skill_absent,"independent_mcp":mcp_independent,"reenabled_skill":skill_resumed,
            "effects":effects(&root)?,"scope":"real Node cross-connection edits, native plugins and stdio; no model efficacy"}))?)?;
        println!("hot installation receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}

#[tokio::test]
async fn indexed_skill_ranges_retain_bytes_and_reject_corruption() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::io::{Seek as _, Write as _};
    use shenko_compat_skill::{CHUNK_BYTES, MAX_READ_BYTES, SkillSource, SourceInput};
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("indexed-skill-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let entry = b"\xef\xbb\xbf---\r\nname: indexed-source\r\ndescription: Exact large resources\r\n---\r\nOriginal.\r\n";
    let resource = (0..8 * CHUNK_BYTES + 19).map(|index| (index % 251) as u8).collect::<Vec<_>>();
    let files = BTreeMap::from([
        ("SKILL.md".to_owned(), (false, entry.to_vec())),
        ("asset.bin".into(), (false, resource.clone())),
        ("empty.bin".into(), (false, Vec::new())),
        ("script.sh".into(), (true, b"#!/bin/sh\nexit 77\n".to_vec())),
    ]);
    let source = SkillSource::capture(&store.cas, vec!["synthetic:indexed-source".into()], files.into_iter().map(|(path, (executable, bytes))|
        Ok(SourceInput { path, executable, bytes: bytes.len() as u64, reader: std::io::Cursor::new(bytes) })))?;
    let archive_path = store.cas.verified_object_path(&source.archive)?;
    let export = source.export("skill.indexed-fixture", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?,
        artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    for export in [plugin_export(shenko_compat_skill::UNIT_ID, true)?, export] {
        node.lock().handle_unit_request(UnitRequest::Import { export, package_root: root.join("units").display().to_string(), enable: Some(true) })?;
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(shenko_compat_skill::SkillPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        let call = |query: Value| { let task_id = EntityId::new(); ChildToolCall { call_id: task_id.to_string(), task_id,
            kind: "compat.skill.source".into(), definition_version: 2, input: json!({"unit":unit,"query":query}) } };
        let mut observations = Vec::new();
        let mut listing = Vec::<String>::new();
        let mut offset = 0;
        loop {
            let result = native.call_tool(&call(json!({"offset":offset,"limit":18})), &parent, &authority).await?;
            ensure!(result.outcome == "succeeded", "listing failed");
            let body = &result.result.as_ref().context("listing receipt")?["observation"]["content"];
            listing.extend(serde_json::from_value::<Vec<String>>(body["files"].clone())?);
            let next = body["next_offset"].as_u64();
            observations.push(result);
            match next { Some(next) => { ensure!(next > offset, "listing did not advance"); offset = next; }, None => break }
        }
        ensure!(listing == ["SKILL.md", "asset.bin", "empty.bin", "script.sh"], "paged membership changed");
        let offset = CHUNK_BYTES - entry.len() - 7;
        let crossing = native.call_tool(&call(json!({"path":"asset.bin","offset":offset,"limit":40})), &parent, &authority).await?;
        ensure!(crossing.outcome == "succeeded", "cross-chunk native read failed");
        let body = &crossing.result.as_ref().context("range receipt")?["observation"]["content"];
        ensure!(STANDARD.decode(body["content_base64"].as_str().context("range bytes")?)? == resource[offset..offset+40], "cross-chunk bytes changed");
        ensure!(body["next_offset"] == offset + 40 && body["file_digest"] == source.index.files["asset.bin"].digest.0, "range identity/cursor changed");
        observations.push(crossing);
        for (query, expected, next, executable) in [
            (json!({"path":"SKILL.md"}), entry.as_slice(), None, false),
            (json!({"path":"asset.bin","offset":resource.len()-11}), &resource[resource.len()-11..], None, false),
            (json!({"path":"empty.bin"}), b"".as_slice(), None, false),
            (json!({"path":"script.sh","limit":4}), b"#!/b".as_slice(), Some(4u64), true),
        ] {
            let result = native.call_tool(&call(query), &parent, &authority).await?;
            ensure!(result.outcome == "succeeded", "bounded member read failed");
            let body = &result.result.as_ref().context("member receipt")?["observation"]["content"];
            ensure!(STANDARD.decode(body["content_base64"].as_str().context("member bytes")?)? == expected
                && body["next_offset"].as_u64() == next && body["executable"] == executable, "member bytes/mode/EOF changed");
            observations.push(result);
        }
        for query in [json!({"path":"asset.bin","offset":resource.len()+1}), json!({"path":"asset.bin","limit":MAX_READ_BYTES+1})] {
            let result = native.call_tool(&call(query), &parent, &authority).await?;
            ensure!(result.outcome == "failed", "invalid range was reported successful");
            observations.push(result);
        }
        // Corrupt a non-entry chunk in this owned fixture. A separate intact
        // range must remain readable; the touched bad chunk must not leak bytes.
        let damaged = 4 * CHUNK_BYTES;
        let mut archive = std::fs::OpenOptions::new().write(true).open(&archive_path)?;
        archive.seek(std::io::SeekFrom::Start(source.index.files["asset.bin"].offset + damaged as u64))?;
        archive.write_all(&[resource[damaged] ^ 0xff])?;
        archive.sync_all()?;
        let intact = native.call_tool(&call(json!({"path":"asset.bin","limit":32})), &parent, &authority).await?;
        ensure!(intact.outcome == "succeeded", "range read rescanned unrelated corrupt bytes");
        observations.push(intact);
        let rejected = native.call_tool(&call(json!({"path":"asset.bin","offset":damaged,"limit":32})), &parent, &authority).await?;
        ensure!(rejected.outcome == "failed" && rejected.result.as_ref().context("corruption receipt")?["observation"]["content"].is_null(),
            "corrupt range returned source bytes");
        observations.push(rejected);
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"unit":unit,"archive":source.archive,
            "observations":observations,"scope":"real native source reads, CAS chunks and operation outcomes; synthetic sources, no model efficacy"}))?)?;
        println!("indexed source receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}

/// A large tool result is outlined into CAS once; the same native runtime
/// then reads it back through compat.artifact.read in authenticated ranges.
/// The receipt is the manifest digest kept in learning_source.
#[tokio::test]
async fn outlined_observation_is_retrievable_in_authenticated_ranges() -> Result<()> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
    use shenko_compat::artifact::{MAX_RANGE_BYTES, NAME as ARTIFACT_READ};
    use shenko_compat::recorded::{ARTIFACT_CHUNK_BYTES, INLINE_OBSERVATION_BYTES};
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("outlined-artifact-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), SERVER)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let profile = McpConnection { description: "outlined observation fixture".into(), provenance_refs: vec!["synthetic:outlined-artifact".into()],
        connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(), args: vec![root.join("server.py").display().to_string(), root.display().to_string()], cwd: Some(root.clone()), env_refs: BTreeMap::new() } };
    let export = profile.export("mcp.outlined-fixture", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?, artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        let package_root = root.join("units").display().to_string();
        node.handle_unit_request(UnitRequest::Import { export: plugin_export(UNIT_ID, true)?, package_root: package_root.clone(), enable: Some(true) })?;
        node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        ensure!(native.has_handler("compat.mcp.request", 1) && native.has_handler(ARTIFACT_READ, 1),
            "MCP generation did not publish the artifact reader");
        let large = native.call_tool(&call(&unit, "large_observation"), &parent, &authority).await?;
        ensure!(large.outcome == "succeeded", "large exchange failed: {large:?}");
        let result = large.result.as_ref().context("large result body")?;
        let observation = &result["observation"];
        ensure!(observation["status"] == "observation_outlined" && observation.get("protocol").is_none()
            && result["source_receipt"]["digest"].is_string(),
            "large observation was not outlined: {observation}");
        ensure!(serde_json::to_vec(result)?.len() < 8 * 1024, "normalized result kept the outlined bytes inline");
        let artifact_digest: Digest = serde_json::from_value(observation["artifact"]["digest"].clone())?;
        let artifact_bytes = observation["artifact"]["bytes"].as_u64().context("outlined artifact size missing")?;
        ensure!(artifact_bytes > INLINE_OBSERVATION_BYTES as u64 && store.cas.object_size(&artifact_digest) == Some(artifact_bytes),
            "artifact identity/size mismatch");
        let manifest_digest: Digest = serde_json::from_value(result["learning_source"]["digest"].clone())?;
        ensure!(serde_json::from_value::<Digest>(result["source_receipt"]["digest"].clone())? == manifest_digest,
            "source receipt is not the learning_source identity");
        let manifest: Value = serde_json::from_slice(&store.cas.read_object(&manifest_digest)?)?;
        ensure!(manifest["format"] == "compat-indexed-observation" && manifest["schema_version"] == 1
            && serde_json::from_value::<Digest>(manifest["artifact_digest"].clone())? == artifact_digest
            && manifest["bytes"].as_u64() == Some(artifact_bytes)
            && manifest["chunk_bytes"].as_u64() == Some(ARTIFACT_CHUNK_BYTES as u64)
            && manifest["chunks"].as_array().context("manifest chunks")?.len() >= 2,
            "artifact manifest is malformed: {manifest}");
        let chunk_count = manifest["chunks"].as_array().unwrap().len();

        let read_call = |input: Value| { let task_id = EntityId::new(); ChildToolCall { call_id: task_id.to_string(), task_id,
            kind: ARTIFACT_READ.into(), definition_version: 1, input } };
        // Page the whole artifact: every returned range is authenticated
        // against its manifest chunk, so the join is the original bytes.
        let mut decoded = Vec::<u8>::new();
        let mut offset = 0u64;
        let mut pages = 0u64;
        loop {
            pages += 1;
            ensure!(pages <= 64, "artifact paging did not terminate");
            let page = native.call_tool(&read_call(json!({"digest":manifest_digest,"offset":offset,"limit":MAX_RANGE_BYTES})), &parent, &authority).await?;
            ensure!(page.outcome == "succeeded", "paged artifact read failed: {page:?}");
            let body = &page.result.as_ref().context("artifact page body")?["observation"];
            ensure!(body["format"] == "compat-indexed-observation"
                && serde_json::from_value::<Digest>(body["artifact_digest"].clone())? == artifact_digest
                && body["total_bytes"].as_u64() == Some(artifact_bytes)
                && body["offset"].as_u64() == Some(offset),
                "artifact page identity/cursor mismatch: {body}");
            decoded.extend(STANDARD.decode(body["content_base64"].as_str().context("artifact page bytes")?)?);
            match body["next_offset"].as_u64() {
                Some(next) => { ensure!(next > offset && next <= artifact_bytes, "artifact paging did not advance"); offset = next; }
                None => break,
            }
        }
        ensure!(decoded.len() as u64 == artifact_bytes && Digest::blake3(&decoded) == artifact_digest
            && decoded == store.cas.read_object(&artifact_digest)?,
            "paged artifact bytes diverge from the sealed original");
        let original: Value = serde_json::from_slice(&decoded)?;
        ensure!(original["protocol"]["status"] == "received"
            && original["protocol"]["result"]["content"][0]["text"].as_str().context("original tool text")?.len() == 1_360_000,
            "original observation was not the real MCP result");
        // A range crossing the chunk boundary is reassembled across
        // independently authenticated chunks.
        let crossing_offset = (ARTIFACT_CHUNK_BYTES - 8) as u64;
        let crossing = native.call_tool(&read_call(json!({"digest":manifest_digest,"offset":crossing_offset,"limit":16})), &parent, &authority).await?;
        ensure!(crossing.outcome == "succeeded", "cross-chunk artifact read failed");
        let body = &crossing.result.as_ref().context("crossing body")?["observation"];
        ensure!(STANDARD.decode(body["content_base64"].as_str().context("crossing bytes")?)? == decoded[crossing_offset as usize..crossing_offset as usize + 16]
            && body["next_offset"].as_u64() == Some(crossing_offset + 16), "cross-chunk bytes/cursor changed");
        // A plain small object is returned raw; limit 0 means the default
        // whole-object range (and EOF reports no further offset).
        let plain = store.cas.seal_object(br#"{"tiny":true}"#, "application/json", "ecosystem-compat", "plain-fixture", 1)?.object;
        let raw = native.call_tool(&read_call(json!({"digest":plain.digest})), &parent, &authority).await?;
        ensure!(raw.outcome == "succeeded", "plain object read failed");
        let body = &raw.result.as_ref().context("raw body")?["observation"];
        ensure!(body["format"] == "raw" && body["total_bytes"].as_u64() == Some(plain.bytes)
            && body["next_offset"].is_null()
            && STANDARD.decode(body["content_base64"].as_str().context("raw bytes")?)? == br#"{"tiny":true}"#,
            "plain object range changed: {body}");
        let mut corruptions = Vec::new();
        // Malformed digest, absent object and an over-bound limit settle
        // failed without panicking or becoming unknown.
        for input in [json!({"digest":{"unexpected":1}}), json!({"digest":"not-a-digest"}),
            json!({"digest":Digest::blake3(b"definitely absent object")}),
            json!({"digest":manifest_digest,"limit":MAX_RANGE_BYTES + 1}),
            json!({"digest":manifest_digest,"offset":artifact_bytes + 1,"limit":8})] {
            let rejected = native.call_tool(&read_call(input), &parent, &authority).await?;
            ensure!(rejected.outcome == "failed", "invalid artifact read succeeded: {rejected:?}");
            ensure!(rejected.result.as_ref().is_none_or(|value| value["observation"]["content_base64"].is_null()),
                "failed artifact read returned bytes: {rejected:?}");
            corruptions.push(json!({"case":rejected.result.as_ref().and_then(|v| v["observation"]["status"].as_str()).unwrap_or("handler_rejected"),"outcome":rejected.outcome}));
        }
        // Corrupt one byte of the second chunk in this owned fixture copy.
        // Reads outside that chunk stay readable; every read covering it
        // fails authentication and returns no bytes.
        let damaged = ARTIFACT_CHUNK_BYTES + 100;
        let artifact_ref = ObjectRef { digest: artifact_digest.clone(), bytes: artifact_bytes, media_type: "application/json".into(),
            store_namespace: "ecosystem-compat".into(), locator: String::new(),
            codec: SchemaRef { name: "compat-observation-bytes".into(), major: 1, digest: Digest::blake3(b"compat-observation-bytes") } };
        let artifact_path = store.cas.verified_object_path(&artifact_ref)?;
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&artifact_path)?;
        file.seek(SeekFrom::Start(damaged as u64))?;
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte)?;
        file.seek(SeekFrom::Start(damaged as u64))?;
        file.write_all(&[byte[0] ^ 0xff])?;
        file.sync_all()?;
        drop(file);
        let intact = native.call_tool(&read_call(json!({"digest":manifest_digest,"offset":0,"limit":32768})), &parent, &authority).await?;
        ensure!(intact.outcome == "succeeded"
            && STANDARD.decode(intact.result.as_ref().context("intact body")?["observation"]["content_base64"].as_str().context("intact bytes")?)? == decoded[..32768],
            "intact range was rejected after unrelated corruption");
        for input in [json!({"digest":manifest_digest,"offset":ARTIFACT_CHUNK_BYTES,"limit":32768}),
            json!({"digest":manifest_digest,"offset":crossing_offset,"limit":16})] {
            let rejected = native.call_tool(&read_call(input), &parent, &authority).await?;
            ensure!(rejected.outcome == "failed"
                && rejected.result.as_ref().context("corrupt body")?["observation"]["content_base64"].is_null(),
                "corrupt range returned bytes: {rejected:?}");
            corruptions.push(json!({"case":"damaged_chunk_range","outcome":rejected.outcome}));
        }
        ensure!(effects(&root)?.len() == 1, "artifact reads produced external effects");
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "unit":unit,"manifest_digest":manifest_digest,"artifact_digest":artifact_digest,"artifact_bytes":artifact_bytes,
            "chunk_bytes":ARTIFACT_CHUNK_BYTES,"chunk_count":chunk_count,"pages_read":pages,"page_bound_bytes":MAX_RANGE_BYTES,
            "paged_digest_verified":Digest::blake3(&decoded),"crossing_offset":crossing_offset,
            "damaged_chunk_byte":damaged,"verdicts":corruptions,"effects":effects(&root)?,
            "scope":"real stdio MCP result outlined into CAS, real compat.artifact.read calls over per-chunk authenticated ranges; synthetic fixture, no model efficacy"}))?)?;
        println!("outlined artifact receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}

/// Real stdio exchanges land as durable observations; compat.usage.digest
/// groups them deterministically, authenticates every exemplar digest through
/// CAS, and one verified exemplar feeds a cognition.unit.write candidate.
/// The digest itself records observations only — it never adopts a unit.
#[tokio::test]
async fn usage_digest_groups_durable_observations_and_feeds_candidate_learning() -> Result<()> {
    use shenko_compat::artifact::{MAX_RANGE_BYTES, NAME as ARTIFACT_READ};
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("usage-digest-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), SERVER)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let profile = McpConnection { description: "usage digest fixture".into(), provenance_refs: vec!["synthetic:usage-digest".into()],
        connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(), args: vec![root.join("server.py").display().to_string(), root.display().to_string()], cwd: Some(root.clone()), env_refs: BTreeMap::new() } };
    let export = profile.export("mcp.usage-fixture", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?, artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        let package_root = root.join("units").display().to_string();
        node.handle_unit_request(UnitRequest::Import { export: plugin_export(UNIT_ID, true)?, package_root: package_root.clone(), enable: Some(true) })?;
        node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        let parent = store.external_task(parent_id)?;
        ensure!(native.has_handler("compat.mcp.request", 1) && native.has_handler(ARTIFACT_READ, 1)
            && native.has_handler(USAGE_DIGEST, 1), "MCP generation did not publish the usage digest");
        // Two methods, success + isError failure: three method+status groups.
        let success = native.call_tool(&call(&unit, "record"), &parent, &authority).await?;
        ensure!(success.outcome == "succeeded", "seed exchange failed: {success:?}");
        let failure = native.call_tool(&call(&unit, "reported_error"), &parent, &authority).await?;
        ensure!(failure.outcome == "failed", "isError did not settle failed");
        let list_id = EntityId::new();
        let listed = native.call_tool(&ChildToolCall { call_id: list_id.to_string(), task_id: list_id,
            kind: "compat.mcp.request".into(), definition_version: 1,
            input: json!({"unit":unit,"request":{"method":"tools/list","params":{}},"timeout_secs":10}) }, &parent, &authority).await?;
        ensure!(listed.outcome == "succeeded", "tools/list exchange failed: {listed:?}");

        let digest_call = |input: Value| { let task_id = EntityId::new(); ChildToolCall { call_id: task_id.to_string(), task_id,
            kind: USAGE_DIGEST.into(), definition_version: 1, input } };
        let first = native.call_tool(&digest_call(json!({"grouping":"method+status"})), &parent, &authority).await?;
        ensure!(first.outcome == "succeeded", "usage digest failed: {first:?}");
        let digest = &first.result.as_ref().context("digest body")?["observation"];
        ensure!(digest["status"] == "digest_ready" && digest["observations_only"] == true
            && digest["matched_records"].as_u64() == Some(3)
            && digest["scanned_operations"].as_u64().context("scan count")? >= 3
            && digest["scan_truncated"] == false, "digest window/counts wrong: {digest}");
        let groups = digest["groups"].as_array().context("digest groups")?;
        ensure!(groups.len() == 3, "method+status grouping wrong: {groups:?}");
        let group = |method: &str, status: &str| groups.iter().find(|group| group["method"] == method
            && group["status"] == status).cloned().context(format!("missing group {method}/{status}"));
        let call_ok = group("tools/call", "succeeded")?;
        let call_failed = group("tools/call", "failed")?;
        let list_ok = group("tools/list", "succeeded")?;
        ensure!(call_ok["count"] == 1 && call_failed["count"] == 1 && list_ok["count"] == 1
            && call_ok["latest_occurrence_utc"].as_str() == call_ok["first_seen_utc"].as_str()
            && call_ok["latest_occurrence_utc"].is_string(),
            "group counts/timestamps wrong: {groups:?}");
        let exemplar = call_ok["exemplar_digests"].as_array().context("exemplars")?.first()
            .and_then(Value::as_str).context("succeeded group has no verified exemplar")?.to_owned();
        ensure!(call_failed["exemplar_digests"].as_array().unwrap().len() == 1
            && list_ok["exemplar_digests"].as_array().unwrap().len() == 1, "exemplar bounds wrong");
        // A second identical call recomputes the same observation — no new
        // effect, no drift.
        let second = native.call_tool(&digest_call(json!({"grouping":"method+status"})), &parent, &authority).await?;
        ensure!(second.outcome == "succeeded"
            && second.result.as_ref().context("second body")?["observation"] == *digest,
            "usage digest was not deterministic: {second:?}");
        // Grouping by method alone merges the outcomes deterministically.
        let by_method = native.call_tool(&digest_call(json!({"grouping":"method"})), &parent, &authority).await?;
        let merged = by_method.result.as_ref().context("method grouping body")?["observation"]["groups"].as_array()
            .context("method groups")?.iter().find(|group| group["method"] == "tools/call").cloned()
            .context("tools/call method group")?;
        ensure!(merged["count"] == 2 && merged["status"].is_null(), "method grouping did not merge outcomes");
        // The window keeps the latest records only: the tools/list exchange.
        let latest = native.call_tool(&digest_call(json!({"grouping":"method+status","window":{"max_records":1}})), &parent, &authority).await?;
        let windowed = &latest.result.as_ref().context("windowed body")?["observation"];
        ensure!(windowed["matched_records"] == 3 && windowed["windowed_records"] == 1
            && windowed["groups"].as_array().unwrap().as_slice() == std::slice::from_ref(&list_ok),
            "window bound did not keep the latest record: {windowed}");
        // Filters: wrong unit matches nothing; the owning unit matches all;
        // a future since matches nothing.
        let foreign = native.call_tool(&digest_call(json!({"grouping":"method+status","unit":{"unit_id":"mcp.other"}})), &parent, &authority).await?;
        ensure!(foreign.result.as_ref().context("foreign body")?["observation"]["matched_records"] == 0,
            "unit filter matched another adapter's observations");
        let owned = native.call_tool(&digest_call(json!({"grouping":"method+status","unit":unit})), &parent, &authority).await?;
        ensure!(owned.result.as_ref().context("owned body")?["observation"]["matched_records"] == 3,
            "unit filter dropped its own observations");
        let future = native.call_tool(&digest_call(json!({"grouping":"method+status","window":{"since_rfc3339":"2999-01-01T00:00:00Z"}})), &parent, &authority).await?;
        ensure!(future.result.as_ref().context("future body")?["observation"]["matched_records"] == 0,
            "since bound admitted older observations");
        let oversized = native.call_tool(&digest_call(json!({"grouping":"method+status","window":{"max_records":201}})), &parent, &authority).await?;
        ensure!(oversized.outcome == "failed", "over-bound window succeeded: {oversized:?}");

        // Every returned exemplar round-trips through compat.artifact.read:
        // the same bytes, the same digest, locally authenticated.
        let read_call = |input: Value| { let task_id = EntityId::new(); ChildToolCall { call_id: task_id.to_string(), task_id,
            kind: ARTIFACT_READ.into(), definition_version: 1, input } };
        let exemplar_digest = Digest(exemplar.clone());
        let read = native.call_tool(&read_call(json!({"digest":exemplar_digest,"limit":MAX_RANGE_BYTES})), &parent, &authority).await?;
        ensure!(read.outcome == "succeeded", "exemplar artifact read failed: {read:?}");
        let body = &read.result.as_ref().context("exemplar read body")?["observation"];
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD.decode(body["content_base64"].as_str().context("exemplar bytes")?)?;
        ensure!(Digest::blake3(&bytes) == exemplar_digest, "exemplar bytes do not authenticate");
        let normalized: Value = serde_json::from_slice(&bytes)?;
        ensure!(normalized["observation"]["protocol"]["status"] == "received"
            && normalized["observation"]["protocol"]["result"]["content"][0]["text"] == "effect recorded",
            "exemplar is not the real MCP observation: {normalized}");
        ensure!(effects(&root)?.len() == 2, "digest/artifact reads produced external effects");

        // The learning path stays a claim until the unit write verifies this
        // exact exemplar against the installation CAS: candidate, not adopted.
        let catalog = Arc::new(shenko_awareness_core::CognitionCatalog::new(node.clone()));
        let writer: Arc<dyn NativeTaskHandler> = Arc::new(shenko_exec_host::CognitionWriteHandler::new(store.clone(), catalog.clone(), root.join("cognition"), None));
        supervisor.spawn(NativeTaskHandlerPlugin, writer).await?;
        let task_id = EntityId::new();
        let write = ChildToolCall { call_id: task_id.to_string(), task_id, kind: "cognition.unit.write".into(), definition_version: 1,
            input: json!({"unit_id":"awareness.usage-exemplar","revision":"1","kind":"awareness","title":"Compat usage exemplar",
                "capability_card":"Durable adapter observations group into verifiable usage evidence",
                "body":"Repeated failed outcomes under one method are evidence to correct the call, not to retry blindly.",
                "provenance_refs":[format!("unit:{}@{}#{}",unit.unit_id,unit.revision,unit.artifact_digest.0)],
                "derived_from":{"kind":"cas_object","reference":format!("compat.usage.digest exemplar:{exemplar}"),"digest":exemplar},
                "enabled":false}) };
        let written = native.call_tool(&write, &parent, &authority).await?;
        ensure!(written.outcome == "succeeded", "exemplar learning write failed: {written:?}");
        let learned = serde_json::to_value(catalog.read("awareness.usage-exemplar", "1")?)?;
        ensure!(learned["learning"]["source_verified"] == true && learned["learning"]["state"] == "candidate"
            && learned["enabled"] == false, "exemplar learning was not a verified candidate: {learned}");
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "unit":unit,"seeded":{"succeeded":success.operation_id,"failed":failure.operation_id,"listed":listed.operation_id},
            "digest":digest,"deterministic_second_call":second.result,"method_grouping":merged,"windowed":windowed,
            "unit_filter":{"foreign":foreign.result,"owned":owned.result},"since_bound":future.result,
            "over_bound_rejected":oversized.outcome,"exemplar_roundtrip":{"digest":exemplar,"bytes":bytes.len()},
            "learning_candidate":learned["learning"],"effects":effects(&root)?,
            "scope":"real stdio MCP observations, real usage digest + artifact reads + cognition write on the native ledger; synthetic fixture, no model efficacy"}))?)?;
        println!("usage digest receipt: {}", root.join("receipt.json").display());
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
    }
}

#[tokio::test]
async fn effects_are_armed_once_and_learning_keeps_original_evidence() -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("native-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), SERVER)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let profile = McpConnection { description: "owned native protocol fixture".into(), provenance_refs: vec!["synthetic:native-effect-fixture@1".into()],
        connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(), args: vec![root.join("server.py").display().to_string(), root.display().to_string()], cwd: Some(root.clone()), env_refs: BTreeMap::new() } };
    let export = profile.export("mcp.native-fixture", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?, artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        let package_root = root.join("units").display().to_string();
        node.handle_unit_request(UnitRequest::Import { export: plugin_export(UNIT_ID, true)?, package_root: package_root.clone(), enable: Some(true) })?;
        node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let outcome: Result<()> = async {
    supervisor.publish_services().await?;
    supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    let adapter = supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
    let parent = store.external_task(parent_id)?;
    let first = call(&unit, "record");
    let result = native.call_tool(&first, &parent, &authority).await?;
    ensure!(result.outcome == "succeeded", "native exchange failed: {result:?}");
    let value = result.result.as_ref().context("native result body")?;
    ensure!(value["observation"]["protocol"]["result"]["isError"] == false, "original result lost");
    let observed = effects(&root)?;
    ensure!(observed.len() == 1 && observed[0]["task_id"] == first.task_id.to_string(), "wrong external effect multiplicity");
    ensure!(observed[0]["operation_id"] == result.operation_id.to_string(), "server observed a different T1");
    let attempt = store.external_operation_attempt(result.operation_id)?;
    ensure!(observed[0]["attempt_id"] == attempt.attempt_id.to_string(), "server observed a different attempt");
    let repeat = native.call_tool(&first, &parent, &authority).await?;
    ensure!(repeat.operation_id == result.operation_id && effects(&root)?.len() == 1, "same native task re-executed its effect");
    let failure = native.call_tool(&call(&unit, "reported_error"), &parent, &authority).await?;
    ensure!(failure.outcome == "failed", "MCP isError was normalized into success");

    // Actual adapter E1 -> CAS -> cognition.unit.write, not a duplicated
    // predicate or a fabricated observation object.
    let source = value["learning_source"].clone();
    let source_digest: Digest = serde_json::from_value(source["digest"].clone())?;
    let original = store.cas.read_object(&source_digest)?;
    let original_value: Value = serde_json::from_slice(&original)?;
    ensure!(original_value["source"]["artifact_digest"] == serde_json::to_value(&unit.artifact_digest)?, "observation lost the source version");
    let catalog = Arc::new(shenko_awareness_core::CognitionCatalog::new(node.clone()));
    let writer: Arc<dyn NativeTaskHandler> = Arc::new(shenko_exec_host::CognitionWriteHandler::new(store.clone(), catalog.clone(), root.join("cognition"), None));
    supervisor.spawn(NativeTaskHandlerPlugin, writer).await?;
    let mut learned = None;
    for revision in ["1", "2"] {
        let task_id = EntityId::new();
        let supersedes = learned.as_ref().map(|digest: &Digest| vec![json!({"revision":"1","artifact_digest":digest,"reason":"refined scope, source retained"})]).unwrap_or_default();
        let write = ChildToolCall { call_id: task_id.to_string(), task_id, kind: "cognition.unit.write".into(), definition_version: 1,
            input: json!({"unit_id":"awareness.fixture-observation","revision":revision,"kind":"awareness","title":"Protocol evidence scope",
                "capability_card":"An observed protocol exchange is not evidence of every downstream effect","body":format!("Revision {revision}: this source-bound exchange has an observation; downstream effects are separate facts."),
                "provenance_refs":[format!("unit:{}@{}#{}",unit.unit_id,unit.revision,unit.artifact_digest.0)],"derived_from":source,"enabled":false,"supersedes":supersedes}) };
        let written = native.call_tool(&write, &store.external_task(parent_id)?, &authority).await?;
        ensure!(written.outcome == "succeeded", "cognition write failed: {written:?}");
        let read = serde_json::to_value(catalog.read("awareness.fixture-observation", revision)?)?;
        ensure!(read["learning"]["source_verified"] == true && read["learning"]["state"] == "candidate", "learning source was unverified or automatically adopted: {read}");
        ensure!(read["enabled"] == false, "learning forced itself into active guidance");
        learned = Some(node.lock().installed_unit_fact("awareness.fixture-observation")?.context("learned unit")?.artifact_digest);
    }
    ensure!(serde_json::to_value(catalog.read("awareness.fixture-observation", "1")?)?["body"].as_str().context("body")?.contains("Revision 1"), "correction overwrote original learning");
    ensure!(store.cas.read_object(&source_digest)? == original, "learning overwrote source evidence");

    let interrupted = call(&unit, "drop_after_effect");
    let unknown = native.call_tool(&interrupted, &store.external_task(parent_id)?, &authority).await?;
    ensure!(unknown.outcome == "unknown", "lost completion became success: {unknown:?}");
    let before = effects(&root)?;
    let repeated_unknown = native.call_tool(&interrupted, &store.external_task(parent_id)?, &authority).await?;
    ensure!(repeated_unknown.outcome == "unknown" && effects(&root)? == before, "unknown attempt was replayed");
    ensure!(repeated_unknown.operation_id == unknown.operation_id && repeated_unknown.evidence_ref == unknown.evidence_ref
        && repeated_unknown.result == unknown.result, "reading an unresolved task lost its original operation/evidence");

    // Keep a real server effect in flight while the actual plugin is removed.
    let pending = call(&unit, "hold");
    let pending_id = pending.task_id;
    let task_native = native.clone();
    let task_parent = store.external_task(parent_id)?;
    let task_authority = authority.clone();
    let pending_worker = tokio::spawn(async move { task_native.call_tool(&pending, &task_parent, &task_authority).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if effects(&root)?.iter().any(|effect| effect["task_id"] == pending_id.to_string()) { return anyhow::Ok(()); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await??;
    let pid = std::fs::read_to_string(root.join("server.pid"))?;
    ensure!(Path::new(&format!("/proc/{}",pid.trim())).exists(), "fixture process missing before retirement");
    tokio::time::timeout(Duration::from_secs(15), supervisor.uninstall(&adapter)).await??;
    let pending_result = pending_worker.await??;
    ensure!(pending_result.outcome == "unknown", "retirement attested remote completion");
    ensure!(!Path::new(&format!("/proc/{}",pid.trim())).exists(), "plugin retirement returned before its process exited");
    let after = native.call_tool(&call(&unit, "record"), &store.external_task(parent_id)?, &authority).await?;
    ensure!(after.outcome == "unavailable" && effects(&root)?.len() == before.len()+1, "retired adapter still dispatched");
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"native_t1_observed_by_server":true,"native_call_result":result,
        "unknown_call_result":unknown,"learning_source":source,"corrected_learning_versions":["1","2"],"effects":effects(&root)?,"owned_process_joined":true,
        "scope":"Linux synthetic real stdio server, real Node/Store/native task/E1/cognition; no model efficacy or arbitrary remote-effect guarantee"}))?)?;
    println!("native compatibility receipt: {}", root.join("receipt.json").display());
    Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    match (outcome, cleanup) {
        (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}
