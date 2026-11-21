// Controlled HTTP peers plus genuine Store/S1/native handlers. These are not
// OXR recognition, model-behavior, physical-power-loss or remote-cancel proofs.
mod document_recovery {
    use super::*;
    use base64::Engine as _;
    use shenko_document::{CredentialHandle, OxrEngineBinding, OxrTransport};
    use shenko_exec_host::{DocumentParseHandler, DOCUMENT_OXR_UNIT, document_task::OxrInstallation};
    use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
    use shenko_node::{UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA};
    use shenko_types::{agent::CallContext, object::{Digest, ObjectRef}, operation::AuthorityRef};
    use std::{sync::atomic::{AtomicUsize, Ordering}, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct PeerTrace {
        accepted: std::collections::BTreeSet<String>,
        checks: Vec<Option<String>>,
    }
    #[derive(Default)]
    struct PeerState { uploads: AtomicUsize, checks: AtomicUsize, mode: AtomicUsize,
        trace: std::sync::Mutex<PeerTrace>,
        task_modes: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
        late_waiting: tokio::sync::Notify,
        release_late: tokio::sync::Notify,
        last_observation: std::sync::Mutex<Option<Vec<u8>>> }
    struct Peer { endpoint: String, state: Arc<PeerState>, stop: CancellationToken, worker: tokio::task::JoinHandle<()> }
    impl Peer {
        async fn new(mode: usize) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
            let state = Arc::new(PeerState::default()); state.mode.store(mode, Ordering::SeqCst);
            let stop = CancellationToken::new(); let worker_stop = stop.clone(); let observations = state.clone();
            let worker = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        _ = worker_stop.cancelled() => break,
                        _accepted = listener.accept() => {
                            let (stream, _) = _accepted.unwrap(); let state = observations.clone();
                            connections.spawn(async move { serve(stream, state).await });
                        }
                        _result = connections.join_next(), if !connections.is_empty() => { _result.unwrap().unwrap(); }
                    }
                }
                connections.abort_all();
                while let Some(result) = connections.join_next().await {
                    if let Err(error) = result { assert!(error.is_cancelled(), "owned HTTP peer panicked: {error}"); }
                }
            });
            Self { endpoint, state, stop, worker }
        }
        fn checked_tasks(&self) -> Vec<String> {
            self.state.trace.lock().unwrap().checks.iter().map(|task| {
                task.clone().expect("each check must carry exactly one task_id field")
            }).collect()
        }
        fn task_mode(&self, task_id: &str, mode: usize) {
            assert!(self.state.trace.lock().unwrap().accepted.contains(task_id));
            self.state.task_modes.lock().unwrap().insert(task_id.to_owned(), mode);
        }
        async fn finish(self) { self.stop.cancel(); self.worker.await.unwrap(); }
    }

    fn multipart_field<'a>(header: &str, body: &'a [u8], name: &str) -> Option<&'a [u8]> {
        let content_type = header.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-type").then_some(value.trim())
        })?;
        let boundary = content_type.split(';').skip(1).find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == "boundary").then_some(value.trim().trim_matches('"'))
        })?;
        if boundary.is_empty() { return None; }
        let delimiter = format!("--{boundary}");
        let separator = format!("\r\n{delimiter}");
        let mut remaining = body.strip_prefix(delimiter.as_bytes())?;
        let mut found = None;
        while !remaining.starts_with(b"--") {
            remaining = remaining.strip_prefix(b"\r\n")?;
            let header_end = remaining.windows(4).position(|part| part == b"\r\n\r\n")?;
            let part_headers = std::str::from_utf8(&remaining[..header_end]).ok()?;
            let disposition = part_headers.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-disposition").then_some(value)
            })?;
            let part_name = disposition.split(';').skip(1).find_map(|part| {
                let (key, value) = part.trim().split_once('=')?;
                if key != "name" { return None; }
                value.trim().strip_prefix('"')?.strip_suffix('"')
            })?;
            let data = &remaining[header_end + 4..];
            let end = data.windows(separator.len()).position(|part| part == separator.as_bytes())?;
            if part_name == name && found.replace(&data[..end]).is_some() { return None; }
            remaining = &data[end + separator.len()..];
        }
        (remaining == b"--" || remaining == b"--\r\n").then_some(found).flatten()
    }
    async fn serve(mut stream: tokio::net::TcpStream, state: Arc<PeerState>) {
        let mut header = Vec::new(); let mut byte = [0];
        while !header.ends_with(b"\r\n\r\n") {
            if stream.read_exact(&mut byte).await.is_err() { return; }
            header.push(byte[0]); assert!(header.len() <= 64 * 1024);
        }
        let header = String::from_utf8(header).unwrap();
        let target = header.lines().next().unwrap().split_whitespace().nth(1).unwrap();
        let len: usize = header.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
        }).unwrap_or(0);
        assert!(len < 64 * 1024);
        let mut body = vec![0; len]; stream.read_exact(&mut body).await.unwrap();
        let mut declared_length = None;
        let mut interrupted = false;
        let mut malformed = false;
        let (status, result) = if target.ends_with("/parse/async") {
            assert!(multipart_field(&header, &body, "file").is_some(), "upload must carry an exact file field");
            let id = state.uploads.fetch_add(1, Ordering::SeqCst) + 1;
            let task_id = format!("original-{id}");
            assert!(state.trace.lock().unwrap().accepted.insert(task_id.clone()));
            (200, serde_json::json!({"code":0,"data":{"task_id":task_id}}))
        } else if target.ends_with("/parse/async_check") {
            assert!(multipart_field(&header, &body, "file").is_none(), "a check cannot upload a source");
            let task_id = multipart_field(&header, &body, "task_id").and_then(|value| std::str::from_utf8(value).ok());
            state.checks.fetch_add(1, Ordering::SeqCst);
            let accepted = {
                let mut trace = state.trace.lock().unwrap();
                trace.checks.push(task_id.map(str::to_owned));
                task_id.is_some_and(|task_id| trace.accepted.contains(task_id))
            };
            if !accepted {
                (404, serde_json::json!({"code":1120,"message":"task_id was not submitted to this peer"}))
            } else {
            let task_id = task_id.unwrap();
            let mode = state.task_modes.lock().unwrap().get(task_id).copied().unwrap_or_else(|| state.mode.load(Ordering::SeqCst));
            match mode {
                0 => (503, serde_json::json!({"code":1120,"message":"original task observation temporarily unavailable"})),
                1 => (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"success","metadata":{"page_num":2},
                    "result":{"outputs":{"json":{"pages":[
                        {"page_num":0,"page_size":{"w":1e100,"h":200},"blocks":[{"idx":0,"type":"Text","content":"受控页零","bbox":[1,2,30,40]}]},
                        {"page_num":1,"page_size":{"w":100,"h":200},"blocks":[{"idx":0,"type":"Table","format":"html","content":"<table><tr><td>受控页一</td></tr></table>"},
                            {"idx":1,"type":"Picture","format":"image","content":"","bbox":[10,20,30,40],"img_url":"https://example.invalid/transient-owned-figure.png"},
                            {"idx":2,"type":"Text","content":"finite f64 outside f32 range","bbox":[1e100,0,1e100,1]},
                            {"idx":3,"type":"Text","content":"representable f32 edge","bbox":[0,0,f32::MAX as f64,1]}]}
                    ]}}}}})),
                2 => (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"processing"}})),
                3 => (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"success","metadata":{"page_num":2},
                    "result":{"outputs":{"json":{"pages":[{"page_num":0,"blocks":[{"content":"incomplete original inventory"}]}]}}}}})),
                4 => (401, serde_json::json!({"code":1133,"message":"check access refused; original computation was not observed"})),
                5 => (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"failed","error":"controlled original-task terminal failure"}})),
                6 => {
                    declared_length = Some(64 * 1024 * 1024 + 1usize);
                    (200, serde_json::json!({"code":1133,"message":"response exceeds the local capture budget"}))
                }
                7 => {
                    interrupted = true;
                    (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"processing"}}))
                }
                8 => {
                    state.late_waiting.notify_one();
                    state.release_late.notified().await;
                    (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"success","metadata":{"page_num":1},
                        "result":{"outputs":{"json":{"pages":[{"page_num":0,"blocks":[{"content":"contradictory late original result"}]}]}}}}}))
                }
                9 => (200, serde_json::json!({"code":0,"data":{"task_id":task_id,"status":"success","metadata":{"page_num":1},
                    "result":{"outputs":{"json":{"pages":[{"page_num":0,"page_size":{"w":100,"h":200},"blocks":[{"content":"original main mapper page","bbox":[1,2,3,4]}]}]}}}}})),
                10 => { malformed = true; (200, serde_json::json!({})) },
                11 => (200, serde_json::json!({"code":0,"data":{"status":"success","result":{"outputs":{"json":{"pages":[]}}}}})),
                12 => (200, serde_json::json!({"code":0,"data":{"task_id":"unrelated-task","status":"success","result":{"outputs":{"json":{"pages":[]}}}}})),
                _ => unreachable!(),
            }
            }
        } else { (404, serde_json::json!({})) };
        let mut body = serde_json::to_vec(&result).unwrap();
        if malformed { body = b"{owned original malformed JSON response".to_vec(); }
        // These modes describe the actual wire observation, never recognition.
        if status == 401 { body.clear(); }
        if interrupted { body.truncate(body.len() / 2); }
        if target.ends_with("/parse/async_check") { *state.last_observation.lock().unwrap() = Some(body.clone()); }
        let header = if interrupted {
            format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        } else {
            format!("HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", declared_length.unwrap_or(body.len()))
        };
        if stream.write_all(header.as_bytes()).await.is_ok() {
            if interrupted {
                let _ = stream.write_all(format!("{:x}\r\n", body.len()).as_bytes()).await;
                let _ = stream.write_all(&body).await;
                let _ = stream.write_all(b"\r\n").await;
                // Deliberately no terminating zero chunk: the received prefix
                // is real, but a complete response was never delivered.
            } else { let _ = stream.write_all(&body).await; }
        }
        let _ = stream.shutdown().await;
    }

    // Each fixture exclusively owns a fresh persistent installation and CAS.
    // Reopened handlers share only that owned installation; these selectors
    // never start a collector or claim shared-installation GC exclusion.
    struct Fixture {
        root: PathBuf, store: Arc<shenko_store::Store>, handler: DocumentParseHandler,
        supervisor: shenko_kernel::Supervisor, service: Arc<shenko_subject::NativeChildService>, source: ObjectRef,
    }
    impl Fixture {
        async fn fresh(peer: &Peer, scope: &str) -> Self {
            Self::fresh_with_legacy(peer, scope, None).await
        }
        async fn fresh_with_legacy(peer: &Peer, scope: &str, legacy_main: Option<shenko_exec_host::document_task::LegacyMainAdmission>) -> Self {
            let base = std::env::var_os("SHENKO_DOCUMENT_EVIDENCE_ROOT").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/document-recovery-fixtures"));
            let root = base.join(EntityId::new().to_string()); std::fs::create_dir_all(&root).unwrap();
            let store = Arc::new(open_native_fixture(&root));
            {
                let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig { data_dir: root.clone(), ..Default::default() })
                    .prepare(store.ledger.ledger_id()).unwrap();
                bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap(); drop(home);
            }
            store.start_subject_dispatcher().unwrap();
            let mut node = node_sync::open_node(&root, &store).unwrap();
            let profile = OxrInstallation { schema_version: 1, endpoint: peer.endpoint.clone(), model_identity: "owned-http-fixture/no-real-OXR".into(),
                scope: scope.to_owned(), credential_env: "OWNED_DOCUMENT_FIXTURE_HANDLE".into(), engine: OxrEngineBinding::Remote {},
                transport: OxrTransport::Async, max_documents: 1, max_source_bytes: 1024 * 1024,
                request_timeout_secs: 2, poll_timeout_secs: 1, poll_interval_millis: 10, legacy_main };
            Self::install_profile(&mut node, &root, &profile, "owned-fixture-1");
            let source = store.cas.seal_object(b"%PDF-1.7\nowned HTTP protocol fixture; no recognition engine runs\n",
                "application/pdf", "owned-fixture", "source", 1).unwrap().object;
            Self::open(root, store, node, source).await
        }
        fn install_profile(node: &mut shenko_node::Node, root: &std::path::Path, profile: &OxrInstallation, revision: &str) {
            let bytes = serde_json::to_vec(profile).unwrap(); let digest = Digest::blake3(&bytes);
            let unit = UnitDescriptor { unit_id: DOCUMENT_OXR_UNIT.into(), kind: UnitKind::Code, revision: revision.into(),
                artifact_digest: digest.clone(), provides: vec!["cap.document.parse/1".into()], dependencies: vec![], applicability: None,
                config: serde_json::json!({"format":"document-oxr-http","schema_version":1}) };
            node.handle_unit_request(UnitRequest::Import { export: UnitExport { schema: UNIT_SCHEMA, root_unit_id: DOCUMENT_OXR_UNIT.into(),
                root_revision: Some(unit.revision.clone()), root_artifact_digest: Some(digest), origin_node: None, enabled: true,
                packages: vec![UnitPackage { unit, artifact_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                    evidence: vec![], resolved_dependencies: None }] }, package_root: root.join("units").to_string_lossy().into_owned(), enable: Some(true) }).unwrap();
        }
        async fn open(root: PathBuf, store: Arc<shenko_store::Store>, node: shenko_node::Node, source: ObjectRef) -> Self {
            Self::open_with_credentials(root, store, node, source, true).await
        }
        async fn open_with_credentials(root: PathBuf, store: Arc<shenko_store::Store>, node: shenko_node::Node,
            source: ObjectRef, credential_available: bool) -> Self {
            let handler = DocumentParseHandler::installed_with_credentials(store.clone(), Arc::new(parking_lot::Mutex::new(node)), |handle| {
                assert_eq!(handle, "OWNED_DOCUMENT_FIXTURE_HANDLE");
                Ok(credential_available.then(|| CredentialHandle::new("owned-http-fixture-only")))
            }).unwrap().unwrap();
            let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
            supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
            let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
            supervisor.spawn(crate::document::Generation, handler.clone()).await.unwrap();
            Self { root, store, handler, supervisor, service, source }
        }
        fn input(&self, source: &ObjectRef, pages: &[u32]) -> serde_json::Value {
            serde_json::json!({"source":source,"source_name":"owned.pdf","pages":pages.iter().map(|index|serde_json::json!({"page_index":index})).collect::<Vec<_>>(),
                "closure_id":self.handler.closure_id()})
        }
        fn request_snapshot(&self, operation_id: EntityId) -> serde_json::Value {
            let attempt = self.store.external_operation_attempt(operation_id).unwrap();
            let evidence = self.store.external_evidence(attempt.final_receipt_id.unwrap()).unwrap();
            serde_json::from_slice(&self.store.cas.read_object(&evidence.raw_ref.digest).unwrap()).unwrap()
        }
        fn claim(&self, id: EntityId, input: serde_json::Value) -> NativeTaskInvocation {
            self.claim_for(id, input, self.handler.descriptor().unwrap())
        }
        fn claim_for(&self, id: EntityId, input: serde_json::Value, descriptor: NativeTaskDescriptor) -> NativeTaskInvocation {
            let claim = self.store.claim_native_task(id, descriptor.implementation_ref.clone(), false).unwrap().unwrap();
            NativeTaskInvocation { context: CallContext { caller_instance_id: EntityId::new(), role_revision: descriptor.role_revision,
                task_id: Some(id), expected_task_revision: Some(claim.invocation.task_revision.0), invocation_id: Some(claim.invocation.invocation_id),
                controller_epoch: claim.invocation.epoch, software_generation: claim.software_generation, authority_refs: descriptor.authority_refs, trace_id: EntityId::new() },
                task_invocation: claim.invocation.clone(), task: claim.task.clone(), operation_id: claim.operation_id,
                cancellation: tokio_util::sync::CancellationToken::new(),
                authority: AuthorityRef { delegation_id: id, delegation_revision: RecordRevision(claim.invocation.task_revision.0),
                    decision_ref: claim.task.created_by_step_id.unwrap_or(claim.invocation.invocation_id) }, input, child_tools: None }
        }
        async fn close(self) -> (PathBuf, ObjectRef) {
            self.supervisor.stop().await.unwrap();
            assert!(self.service.installed_prompt_sections().is_empty());
            (self.root, self.source)
        }
    }

    #[tokio::test]
    async fn original_async_handle_survives_store_reopen_and_cached_page_read() {
        let peer = Peer::new(0).await;
        let fixture = Fixture::fresh(&peer, "owned-fixture").await;
        let task_id = EntityId::new(); let input = fixture.input(&fixture.source, &[]);
        {
            commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[task_id], vec![]);
        }
        let operation = fixture.handler.execute(fixture.claim(task_id, input)).await.unwrap();
        assert_eq!(operation.effect_status, EffectStatus::Unknown);
        assert_eq!(operation.outcome, None);
        let original_attempt = fixture.store.external_operation_attempt(operation.operation_id).unwrap().attempt_id;
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        assert_eq!(peer.checked_tasks(), vec!["original-1".to_owned()]);
        let accepted = fixture.store.external_operation_evidence(operation.operation_id).unwrap().into_iter().find_map(|evidence| {
            let value: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&evidence.raw_ref.digest).unwrap()).unwrap();
            (value["raw_observation"]["source"] == "async_submit").then_some(value)
        }).expect("the original async acceptance must be durable before the first check");
        assert_eq!(accepted["raw_observation"]["attempt_id"], original_attempt.to_string());
        assert_eq!(accepted["raw_observation"]["capture_complete"], true);
        let accepted_body: ObjectRef = serde_json::from_value(accepted["raw_observation"]["body"].clone()).unwrap();
        assert_eq!(fixture.store.cas.read_object(&accepted_body.digest).unwrap(),
            serde_json::to_vec(&serde_json::json!({"code":0,"data":{"task_id":"original-1"}})).unwrap());
        let accepted_at = accepted["accepted_at"].clone();
        let (root, source) = fixture.close().await;
        let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
        let node = node_sync::open_node(&root, &store).unwrap();
        let fixture = Fixture::open(root, store, node, source).await;
        peer.state.mode.store(1, Ordering::SeqCst);
        let task = fixture.store.external_task(task_id).unwrap();
        let resolved = fixture.handler.reconcile(&task, operation.operation_id).await.unwrap();
        assert_eq!(peer.checked_tasks(), vec!["original-1".to_owned(), "original-1".to_owned()]);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(fixture.store.external_operation_attempt(operation.operation_id).unwrap().attempt_id, original_attempt);
        assert_eq!(fixture.store.external_operation_attempts(operation.operation_id).unwrap().len(), 1);
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        let saved = fixture.store.external_operation_attempt(operation.operation_id).unwrap();
        let evidence = fixture.store.external_evidence(saved.final_receipt_id.unwrap()).unwrap();
        let snapshot: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&evidence.raw_ref.digest).unwrap()).unwrap();
        assert_eq!(snapshot["provider_task_id"], "original-1"); assert_eq!(snapshot["pages"].as_object().unwrap().len(), 2);
        assert_eq!(snapshot["accepted_at"], accepted_at);
        assert_eq!(fixture.store.cas.read_object(&accepted_body.digest).unwrap(),
            serde_json::to_vec(&serde_json::json!({"code":0,"data":{"task_id":"original-1"}})).unwrap());
        let frozen: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&resolved.final_input_ref.digest).unwrap()).unwrap();
        assert_eq!(frozen["schema_version"], 2);
        assert_eq!(frozen["request"]["schema_version"], 2);
        assert_eq!(frozen["request"]["deployment_scope"], "owned-fixture");
        assert_eq!(frozen["provider"]["scope"], frozen["request"]["deployment_scope"]);
        assert_eq!(frozen["request"]["cache_scope"], format!("{}:owned-fixture", task.agent_id));
        let page: ObjectRef = serde_json::from_value(snapshot["pages"]["1"].clone()).unwrap();
        let page: shenko_document::PageCheckpoint = serde_json::from_slice(&fixture.store.cas.read_object(&page.digest).unwrap()).unwrap();
        let page = page.result.unwrap();
        let zero: ObjectRef = serde_json::from_value(snapshot["pages"]["0"].clone()).unwrap();
        let zero: shenko_document::PageCheckpoint = serde_json::from_slice(&fixture.store.cas.read_object(&zero.digest).unwrap()).unwrap();
        let zero = zero.result.unwrap();
        assert!(zero.page_size.is_none(), "unrepresentable dimensions remain unknown after typed CAS readback");
        assert_eq!(zero.blocks[0].bbox, Some([1.0,2.0,30.0,40.0]));
        assert!(!page.coordinates_restored);
        assert_eq!(page.page_size, Some(shenko_document::PageSize { width:100.0, height:200.0 }));
        assert!(page.blocks[0].bbox.is_none(), "page size never invents every block's bbox");
        assert_eq!(page.blocks[1].bbox, Some([10.0,20.0,30.0,40.0]));
        assert!(page.blocks[2].bbox.is_none(), "finite f64 overflow cannot become serialized null coordinates");
        assert_eq!(page.blocks[3].bbox, Some([0.0,0.0,f32::MAX,1.0]));
        assert_eq!(page.blocks[1].image_resource, Some(shenko_document::PageImageResource::TransientUnretained {
            provider_url: "https://example.invalid/transient-owned-figure.png".into(),
        }));
        assert_eq!(page.provenance.raw_output_ref.as_deref(), snapshot["raw_output"]["digest"].as_str());
        let cached_task = EntityId::new(); let input = fixture.input(&fixture.source, &[1]);
        {
            commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[cached_task], vec![]);
        }
        let cached = fixture.handler.execute(fixture.claim(cached_task, input)).await.unwrap();
        assert_eq!(cached.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        // This real alias has completed E1 through execute(), but the native
        // service has not yet been driven to T2. Replay must preserve that
        // original result rather than append evidence before settlement.
        assert!(fixture.store.native_task_result(cached_task).unwrap().is_none());
        let cached_owner = fixture.store.external_task(cached_task).unwrap();
        let before_t2 = [resolved.operation_id, cached.operation_id].into_iter().map(|operation_id| (
            operation_id,
            serde_json::to_value(fixture.store.external_operation(operation_id).unwrap()).unwrap(),
            serde_json::to_value(fixture.store.external_operation_attempts(operation_id).unwrap()).unwrap(),
            fixture.store.external_operation_evidence(operation_id).unwrap(),
        )).collect::<Vec<_>>();
        assert_eq!(before_t2[0].1, serde_json::to_value(&resolved).unwrap());
        assert_eq!(before_t2[1].1, serde_json::to_value(&cached).unwrap());
        let uploads_before_t2 = peer.state.uploads.load(Ordering::SeqCst);
        let checks_before_t2 = peer.state.checks.load(Ordering::SeqCst);
        let handles_before_t2 = peer.checked_tasks();
        let replayed_alias = fixture.handler.reconcile(&cached_owner, cached.operation_id).await.unwrap();
        assert_eq!(serde_json::to_value(&replayed_alias).unwrap(), before_t2[1].1);
        for (operation_id, operation, attempts, receipts) in &before_t2 {
            assert_eq!(serde_json::to_value(fixture.store.external_operation(*operation_id).unwrap()).unwrap(), *operation);
            assert_eq!(serde_json::to_value(fixture.store.external_operation_attempts(*operation_id).unwrap()).unwrap(), *attempts);
            assert_eq!(fixture.store.external_operation_evidence(*operation_id).unwrap(), *receipts);
        }
        assert!(fixture.store.native_task_result(cached_task).unwrap().is_none(), "reconciliation must not invent T2");
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), uploads_before_t2);
        assert_eq!(peer.state.checks.load(Ordering::SeqCst), checks_before_t2);
        assert_eq!(peer.checked_tasks(), handles_before_t2);
        let read_task = EntityId::new();
        {
            commit_native_fixture_step(&fixture.store, &format!("[delegate:document.page.read:1] {}",
                serde_json::json!({"operation_id":cached.operation_id,"page_index":1})), &[read_task], vec![]);
        }
        drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(read_task)?.is_some())).await.unwrap();
        let read = fixture.store.native_task_result(read_task).unwrap().unwrap();
        assert_eq!(read.outcome, TaskOutcome::Succeeded);
        let value = read.result.unwrap();
        assert!(value["content"].as_str().unwrap().contains("受控页一"));
        assert!(value["content"].as_str().unwrap().contains(&fixture.source.digest.0));
        assert!(value["content"].as_str().unwrap().contains("transient_unretained"));
        assert_eq!(peer.checked_tasks(), vec!["original-1".to_owned(), "original-1".to_owned()], "cached reads never query another handle");
        let read_operation = fixture.store.external_operations_after("document.page.read", None, 256).unwrap()
            .into_iter().find(|operation| operation.task_id == read_task).unwrap();
        assert_eq!(read_operation.outcome, Some(OperationOutcome::Succeeded));
        let read_owner = fixture.store.external_task(read_task).unwrap();
        // Capture the original A, cached alias B and settled page C before
        // either replay. Compare every serialized operation/attempt field;
        // these records deliberately do not require a PartialEq impl.
        let retained = [resolved.operation_id, cached.operation_id, read_operation.operation_id]
            .into_iter().map(|operation_id| (
                operation_id,
                serde_json::to_value(fixture.store.external_operation(operation_id).unwrap()).unwrap(),
                serde_json::to_value(fixture.store.external_operation_attempts(operation_id).unwrap()).unwrap(),
                fixture.store.external_operation_evidence(operation_id).unwrap(),
            )).collect::<Vec<_>>();
        assert_eq!(retained[0].1, serde_json::to_value(&resolved).unwrap());
        assert_eq!(retained[1].1, serde_json::to_value(&cached).unwrap());
        assert_eq!(retained[2].1, serde_json::to_value(&read_operation).unwrap());
        let uploads_before_replay = peer.state.uploads.load(Ordering::SeqCst);
        let checks_before_replay = peer.state.checks.load(Ordering::SeqCst);
        let handles_before_replay = peer.checked_tasks();
        // Use the same installed handler factories as document::Generation;
        // resolved recovery must validate custody without publishing a fact,
        // including when this Store can no longer accept any E1 writes.
        let reader = fixture.handler.page_reader();
        for writer_closed in [false, true] {
            if writer_closed { fixture.store.close_external_writer(); }
            let replayed_alias = fixture.handler.reconcile(&cached_owner, cached.operation_id).await.unwrap();
            assert_eq!(serde_json::to_value(&replayed_alias).unwrap(), retained[1].1,
                "alias recovery must return the exact settled operation");
            let replayed_page = reader.reconcile(&read_owner, read_operation.operation_id).await.unwrap();
            assert_eq!(serde_json::to_value(&replayed_page).unwrap(), retained[2].1,
                "page recovery must return the exact settled operation");
            for (operation_id, operation, attempts, receipts) in &retained {
                assert_eq!(serde_json::to_value(fixture.store.external_operation(*operation_id).unwrap()).unwrap(), *operation,
                    "resolved recovery must preserve every original operation field");
                assert_eq!(serde_json::to_value(fixture.store.external_operation_attempts(*operation_id).unwrap()).unwrap(), *attempts,
                    "resolved recovery must preserve every original attempt field");
                assert_eq!(fixture.store.external_operation_evidence(*operation_id).unwrap(), *receipts,
                    "resolved recovery must preserve the complete original receipt history");
            }
            assert_eq!(peer.state.uploads.load(Ordering::SeqCst), uploads_before_replay);
            assert_eq!(peer.state.checks.load(Ordering::SeqCst), checks_before_replay);
            assert_eq!(peer.checked_tasks(), handles_before_replay);
        }
        fixture.close().await; peer.finish().await;
    }

    #[tokio::test]
    async fn accepted_task_keeps_original_deadline_and_late_terminal_conflict() {
        let peer = Peer::new(0).await;
        let fixture = Fixture::fresh(&peer, "owned-fixture").await;
        let task_id = EntityId::new(); let input = fixture.input(&fixture.source, &[]);
        {
            commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[task_id], vec![]);
        }
        let operation = fixture.handler.execute(fixture.claim(task_id, input)).await.unwrap();
        let original_attempt = fixture.store.external_operation_attempt(operation.operation_id).unwrap().attempt_id;
        let accepted_at: chrono::DateTime<chrono::Utc> = serde_json::from_value(fixture.request_snapshot(operation.operation_id)["accepted_at"].clone()).unwrap();
        let task = fixture.store.external_task(task_id).unwrap();
        fixture.store.request_native_cancellation(task_id, EntityId::new(), task.created_by_step_id.unwrap(),
            "owned original-task cancellation", AbortScope::Task).unwrap();
        let (root, source) = fixture.close().await;
        let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
        let node = node_sync::open_node(&root, &store).unwrap();
        let fixture = Fixture::open(root, store, node, source).await;
        let until_expiry = (accepted_at + chrono::Duration::seconds(1) - chrono::Utc::now()).to_std().unwrap_or_default();
        tokio::time::sleep(until_expiry).await;
        peer.state.mode.store(2, Ordering::SeqCst);
        let before = peer.state.checks.load(Ordering::SeqCst);
        let task = fixture.store.external_task(task_id).unwrap();
        assert!(task.abort_request.is_some(), "reopen preserves the original local cancellation");
        let unknown = fixture.handler.reconcile(&task, operation.operation_id).await.unwrap();
        assert_eq!(unknown.effect_status, EffectStatus::Unknown);
        assert_eq!(unknown.outcome, None);
        assert_eq!(peer.state.checks.load(Ordering::SeqCst), before + 1, "expired original deadline permits exactly one bounded query");
        assert_eq!(fixture.request_snapshot(operation.operation_id)["accepted_at"], serde_json::to_value(accepted_at).unwrap());
        // Two independently reconstructed owners observe the same attempt.
        // Deliver the late HTTP response within the original timeout, then
        // pause only its independent connection's first publication-head read.
        // Neither a server write nor entry into observe proves HTTP delivery:
        // require the exact, newly sealed terminal bytes while this gate holds.
        struct PublicationGate {
            armed: std::sync::atomic::AtomicBool,
            state: std::sync::Mutex<(bool, bool)>, // released, timed out
            release: std::sync::Condvar,
            reached: tokio::sync::Notify,
        }
        impl PublicationGate {
            fn release(&self) {
                self.state.lock().unwrap().0 = true;
                self.release.notify_all();
            }
        }
        struct LateOwner {
            gate: Arc<PublicationGate>,
            peer: Arc<PeerState>,
            stop: CancellationToken,
            worker: Option<std::thread::JoinHandle<anyhow::Result<shenko_types::operation::OperationRecord>>>,
        }
        impl Drop for LateOwner {
            fn drop(&mut self) {
                // Also runs on every failed assertion/wait after spawning.
                // Release both waits before joining; the worker has its own
                // runtime deadline and the SQLite gate has a bounded wait.
                self.peer.release_late.notify_one();
                self.gate.release();
                self.stop.cancel();
                if let Some(worker) = self.worker.take() {
                    let result = worker.join();
                    if std::thread::panicking() {
                        eprintln!("late OXR owner cleanup: {result:?}");
                    } else {
                        result.unwrap().unwrap();
                    }
                }
            }
        }
        let gate = Arc::new(PublicationGate {
            armed: std::sync::atomic::AtomicBool::new(false),
            state: std::sync::Mutex::new((false, false)),
            release: std::sync::Condvar::new(), reached: tokio::sync::Notify::new(),
        });
        let late_store = Arc::new(open_native_fixture(&fixture.root));
        assert_eq!(late_store.ledger.ledger_id(), fixture.store.ledger.ledger_id());
        let other_node = node_sync::open_node(&fixture.root, &late_store).unwrap();
        let late_handler = DocumentParseHandler::installed_with_credentials(late_store.clone(),
            Arc::new(parking_lot::Mutex::new(other_node)), |_| Ok(Some(CredentialHandle::new("owned-http-fixture-only"))))
            .unwrap().unwrap();
        shenko_store::test_support::set_sqlite_authorizer(&late_store, Some({
            let gate = gate.clone();
            move |context: rusqlite::hooks::AuthContext<'_>| {
                if matches!(context.action, rusqlite::hooks::AuthAction::Read { table_name: "operations", .. })
                    && gate.armed.swap(false, Ordering::SeqCst)
                {
                    gate.reached.notify_one();
                    let (mut state, timed) = gate.release.wait_timeout_while(
                        gate.state.lock().unwrap(), Duration::from_secs(20), |state| !state.0).unwrap();
                    state.1 = timed.timed_out() && !state.0;
                }
                rusqlite::hooks::Authorization::Allow
            }
        }));
        let late_bytes = serde_json::to_vec(&serde_json::json!({"code":0,"data":{"task_id":"original-1","status":"success","metadata":{"page_num":1},
            "result":{"outputs":{"json":{"pages":[{"page_num":0,"blocks":[{"content":"contradictory late original result"}]}]}}}}})).unwrap();
        let late_digest = Digest::blake3(&late_bytes);
        assert!(!fixture.store.cas.object_exists(&late_digest),
            "the fixture must not pre-seal its contradictory terminal response");
        peer.state.mode.store(8, Ordering::SeqCst);
        let late_task = task.clone(); let late_owner = late_handler.clone(); let operation_id = operation.operation_id;
        let stop = CancellationToken::new();
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || -> anyhow::Result<_> {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let result = runtime.block_on(async {
                tokio::select! {
                    biased;
                    _ = worker_stop.cancelled() => Err(anyhow::anyhow!("owned late reconciliation cancelled during fixture cleanup")),
                    _result = tokio::time::timeout(Duration::from_secs(20), late_owner.reconcile(&late_task, operation_id)) =>
                        _result.map_err(|_| anyhow::anyhow!("owned late reconciliation exceeded fixture deadline"))?,
                }
            });
            runtime.shutdown_timeout(Duration::from_secs(2));
            result
        });
        let mut late = LateOwner { gate: gate.clone(), peer: peer.state.clone(), stop, worker: Some(worker) };
        tokio::time::timeout(Duration::from_secs(2), peer.state.late_waiting.notified()).await.unwrap();
        gate.armed.store(true, Ordering::SeqCst);
        peer.state.release_late.notify_one();
        tokio::time::timeout(Duration::from_secs(5), gate.reached.notified()).await.unwrap();
        assert!(!gate.state.lock().unwrap().0);
        assert_eq!(fixture.store.cas.read_object(&late_digest).unwrap(), late_bytes,
            "the real client received, decoded and sealed the entire contradictory terminal response before publication");
        let before_terminal_checks = peer.state.checks.load(Ordering::SeqCst);
        peer.state.mode.store(1, Ordering::SeqCst);
        let completed = fixture.handler.reconcile(&task, operation_id).await.unwrap();
        // Recovery may observe the original remote completion after local
        // cancellation. result() retains that response before its page-work
        // cancellation check; it must not resume the cancelled local work.
        assert_eq!(completed.outcome, Some(OperationOutcome::Cancelled));
        assert_eq!(completed.effect_status, EffectStatus::Complete);
        assert!(!completed.outcome_conflict);
        let before_conflict = fixture.store.external_operation_evidence(operation_id).unwrap();
        let completed_snapshot = fixture.request_snapshot(operation_id);
        assert_eq!(completed_snapshot["remote_finished"], true);
        assert_eq!(completed_snapshot["remote_may_be_running"], false);
        assert!(completed_snapshot["pages"].as_object().unwrap().is_empty(), "cancelled local work materializes no new pages");
        let original_body: ObjectRef = serde_json::from_value(completed_snapshot["raw_output"].clone()).unwrap();
        let original_bytes = fixture.store.cas.read_object(&original_body.digest).unwrap();
        let original_response: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
        assert_eq!(original_response["data"]["result"]["outputs"]["json"]["pages"].as_array().unwrap().len(), 2);
        assert!(!gate.state.lock().unwrap().1, "late publication gate must remain held through original settlement");
        gate.release();
        // Joining on this thread cannot starve the HTTP peer: the complete
        // response was already consumed and sealed before the gate was reached.
        let conflict = late.worker.take().unwrap().join().unwrap().unwrap();
        assert!(!gate.state.lock().unwrap().1, "the publication barrier must not expire");
        assert_eq!(peer.state.checks.load(Ordering::SeqCst), before_terminal_checks + 1,
            "only the original owner's one terminal query follows the already delivered late response");
        assert!(conflict.outcome_conflict);
        assert_eq!(conflict.outcome, None);
        assert_eq!(conflict.effect_status, EffectStatus::Unknown);
        let retained = fixture.store.external_operation_evidence(operation_id).unwrap();
        assert!(retained.len() > before_conflict.len());
        for original in before_conflict { assert!(retained.iter().any(|receipt| receipt == &original)); }
        let late_snapshot = fixture.request_snapshot(operation_id);
        let late_body: ObjectRef = serde_json::from_value(late_snapshot["raw_output"].clone()).unwrap();
        assert_eq!(late_body.digest, late_digest);
        assert_eq!(fixture.store.cas.read_object(&late_body.digest).unwrap(), late_bytes);
        assert!(String::from_utf8(fixture.store.cas.read_object(&late_body.digest).unwrap()).unwrap().contains("contradictory late original result"));
        assert_eq!(fixture.store.cas.read_object(&original_body.digest).unwrap(), original_bytes,
            "the authentic original completion remains readable alongside the contradictory late response");
        assert_eq!(late_snapshot["pages"], completed_snapshot["pages"]);
        assert_eq!(fixture.store.external_task(task_id).unwrap().abort_request.as_ref().unwrap().cancel_id,
            task.abort_request.as_ref().unwrap().cancel_id, "late evidence cannot undo local cancellation");
        assert_eq!(fixture.store.external_operation_attempt(operation_id).unwrap().attempt_id, original_attempt);
        assert_eq!(fixture.store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        assert!(peer.checked_tasks().iter().all(|handle| handle == "original-1"));
        assert!(fixture.handler.reconcile(&task, operation_id).await.unwrap().outcome_conflict);

        let consumer = EntityId::new();
        let input = fixture.input(&fixture.source, &[]);
        commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[consumer], vec![]);
        let before_cache_checks = peer.state.checks.load(Ordering::SeqCst);
        drive_native_until(&fixture.service, || {
            Ok(fixture.store.external_operations_after("document.parse", None, 256)?
                .iter().any(|entry| entry.task_id == consumer && entry.outcome_conflict))
        }).await.unwrap();
        let consumer_op = fixture.store.external_operations_after("document.parse", None, 256).unwrap()
            .into_iter().find(|entry| entry.task_id == consumer).unwrap();
        assert_eq!(consumer_op.outcome, None);
        assert!(fixture.store.native_task_result(consumer).unwrap().is_none());
        assert_eq!(fixture.request_snapshot(consumer_op.operation_id)["source_operation"], serde_json::json!(operation_id));
        assert_eq!(peer.state.checks.load(Ordering::SeqCst), before_cache_checks);
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        late_handler.shutdown().await;
        fixture.close().await; peer.finish().await;
    }

    #[tokio::test]
    async fn original_main_handle_dispatches_and_settles_through_installed_reader() {
        historical_main_recovery_case(HistoricalMainCut::Accepted).await;
    }

    #[tokio::test]
    async fn mismatched_main_installation_never_queries_and_keeps_debt() {
        historical_main_recovery_case(HistoricalMainCut::MismatchedAccount).await;
    }

    #[tokio::test]
    async fn main_t1_without_receipt_retains_debt_without_any_wire_request() {
        historical_main_recovery_case(HistoricalMainCut::NoReceipt).await;
    }

    #[tokio::test]
    async fn historical_unavailable_without_dispatch_does_not_charge_capacity() {
        historical_main_recovery_case(HistoricalMainCut::UnavailableWithoutDispatch).await;
    }

    enum HistoricalMainCut { Accepted, MismatchedAccount, NoReceipt, UnavailableWithoutDispatch }

    async fn historical_main_recovery_case(cut: HistoricalMainCut) {
        use shenko_document::legacy_main::{AttemptBinding, OxrTaskHandle};
        use shenko_types::operation::*;
        use shenko_types::task::ImplementationRef;
        const ORIGINAL_CONTRACT: &[u8] = b"shenko.document.parse/v1: one original armed OXR upload; immutable accepted-task and page receipts; unknown reconciles without upload replay";
        let no_send = matches!(cut, HistoricalMainCut::UnavailableWithoutDispatch);
        let accepted = matches!(cut, HistoricalMainCut::Accepted | HistoricalMainCut::MismatchedAccount);
        let matched = !matches!(cut, HistoricalMainCut::MismatchedAccount);
        let peer = Peer::new(9).await;
        let provider = |account: &str| serde_json::json!({"endpoint":peer.endpoint,
            "model":"owned-http-fixture/no-real-OXR","account":account,"scope":"original-main-scope",
            "transport":"Async","engine":"Remote","request_timeout":Duration::from_secs(2),
            "poll_timeout":Duration::from_millis(20),"poll_interval":Duration::from_millis(10)});
        {
            let admitted_provider = provider("independent-original-account");
            let admission = shenko_exec_host::document_task::LegacyMainAdmission {
                account_identity:"independent-original-account".into(), scope:Some("original-main-scope".into()),
                provider_binding_digest:Digest::blake3(&serde_json::to_vec(&admitted_provider).unwrap()),
                mapper_digest:shenko_document::legacy_main::mapper_digest(),
            };
            let fixture = Fixture::fresh_with_legacy(&peer, "installed-deployment-permission", Some(admission)).await;
            let original_provider = if matched { admitted_provider } else { provider("different-original-account") };
            let task_id = EntityId::new();
            let request = serde_json::json!({"schema_version":1,"source_digest":fixture.source.digest,
                "source_name":"original.pdf","pages":[],"closure_id":"oxr/1.0","scope":"original-main-scope"});
            let input = serde_json::json!({"source_ref":fixture.source,"request":request});
            {
                commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[task_id], vec![]);
            }
            let descriptor = NativeTaskDescriptor {
                kind: "document.parse".into(), definition_version: 1,
                implementation_ref: ImplementationRef {
                    plugin_id: EntityId::derived("native-document-plugin/v1", ORIGINAL_CONTRACT),
                    artifact_digest: Digest::blake3(ORIGINAL_CONTRACT), registry_revision: 1,
                },
                role_revision: "native-document-parse/1".into(),
                authority_refs: vec!["document.parse/v1".into()], prompt_section: None,
            };
            let invocation = fixture.claim_for(task_id, input.clone(), descriptor.clone());
            let zero = shenko_types::commit::CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
            let attempt_id = EntityId::derived("document.parse/attempt/v1", invocation.operation_id.to_string().as_bytes());
            let params = serde_json::json!({"schema":"document.parse/oxr-params/v2","transport":"Async","output_formats":"json",
                "element_formats":"upstream-default","feature_config":"upstream-default","engine":"Remote",
                "request_timeout":Duration::from_secs(2),"poll_timeout":Duration::from_millis(20),"poll_interval":Duration::from_millis(10)});
            let binding = AttemptBinding { operation_id:invocation.operation_id, attempt_id,
                account_identity:original_provider["account"].as_str().unwrap().into(), source_digest:fixture.source.digest.0.clone(),
                source_bytes_digest:fixture.source.digest.0.clone(), source_name:"original.pdf".into(), request_schema_version:1,
                closure_id:"oxr/1.0".into(),scope:Some("original-main-scope".into()), endpoint:peer.endpoint.clone(),
                model_identity:"owned-http-fixture/no-real-OXR".into(), params_digest:Digest::blake3(params.to_string().as_bytes()).0 };
            let mut operation;
            let mut attempt;
            {
                let seal = |value: &serde_json::Value, schema: &str| fixture.store.cas.seal_object(&serde_json::to_vec(value).unwrap(),
                    "application/json","document-parse",schema,1).unwrap().object;
                let frozen = seal(&serde_json::json!({"schema_version":1,"input":input,"provider":original_provider}),"document-frozen-input");
                operation = OperationRecord { record_schema_version:1,operation_id:invocation.operation_id,record_revision:RecordRevision(0),
                    task_id,logical_key:"native-handler".into(),operation_name:"document.parse.v1".into(),input_schema_version:1,output_schema_version:1,
                    input_digest:frozen.digest.clone(),final_input_ref:frozen,contract_ref:OperationContractRef {
                        plugin_id:descriptor.implementation_ref.plugin_id,artifact_digest:descriptor.implementation_ref.artifact_digest.clone(),
                        contract_digest:descriptor.implementation_ref.artifact_digest.clone(),receipt_reader_ref:descriptor.implementation() },
                    resource_bindings_ref:seal(&serde_json::json!({"provider":"oxr/1.0","account_identity":binding.account_identity,
                        "runtime_node":invocation.task_invocation.node_id.to_string(),"mount_generation":0,
                        "capability_level":"document_upload_and_task_query","extra":original_provider}),"document-resource-binding"),
                    authority_ref:invocation.authority,acceptance_predicate_ref:seal(&serde_json::json!("valid mapped requested pages or explicit failure/unavailable; unknown is never success"),"document-acceptance"),
                    retry_contract_ref:seal(&serde_json::json!("one original upload; accepted task may only be queried; an unknown upload may not be replayed"),"document-retry"),
                    checkpoint_scope_ref:Some(seal(&serde_json::to_value(&binding).unwrap(),"document-attempt-binding")),
                    state:OperationState::InFlight,outcome:None,outcome_conflict:false,effect_status:EffectStatus::None,retry_of:None,compensates:None,
                    possible_duplicate_of:vec![],prepared_commit:zero,evidence_through:None };
                attempt = AttemptRecord { record_schema_version:1,attempt_id,operation_id:operation.operation_id,attempt_no:1,
                    invocation_id:invocation.task_invocation.invocation_id,worker_id:invocation.task_invocation.worker_id,
                    node_id:invocation.task_invocation.node_id,epoch:invocation.task_invocation.epoch,implementation_ref:descriptor.implementation(),
                    dispatch_stage:AttemptDispatchStage::Armed,armed_commit:zero,provider_handle:None,provider_idempotency:None,
                    transport_ref:None,started_at:None,last_observed_at:None,final_receipt_id:None };
                let existing = if no_send {
                    fixture.store.arm_external_operation(&invocation.task_invocation,&mut operation,&mut attempt)
                } else {
                    fixture.store.arm_native_operation_for_dispatch(&invocation.task_invocation,&mut operation,&mut attempt)
                };
                assert!(existing.unwrap().is_none());
            }
            assert_eq!(operation.effect_status, EffectStatus::None);
            assert_eq!(attempt.dispatch_stage, AttemptDispatchStage::Armed);
            // Create the genuine acceptance receipt only for the cases where
            // the original HTTP upload crossed that durable boundary.
            let handle = if accepted {
                let client = shenko_document::OxrClient::new(shenko_document::OxrClientConfig::new(peer.endpoint.clone(),
                    CredentialHandle::new("owned-http-fixture-only"))).unwrap();
                let source_bytes = fixture.store.cas.read_object(&fixture.source.digest).unwrap();
                let shenko_document::OxrSubmission::Accepted { task_id: handle, response } = client.submit_document("original.pdf",&source_bytes).await.unwrap() else { panic!("expected real acceptance"); };
                let accepted_at = chrono::Utc::now();
                let task = OxrTaskHandle { task_id:handle.clone(),accepted_at,poll_deadline:accepted_at+chrono::Duration::milliseconds(20) };
                let raw = fixture.store.cas.seal_object(response.body.as_deref().unwrap(),"application/octet-stream","document-parse","oxr-http-response-body",1).unwrap().object;
                let normalized = fixture.store.cas.seal_object(&serde_json::to_vec(&serde_json::json!({"schema_version":2,
                    "binding_digest":binding.digest().unwrap(),"receipt":{"task":task,"outcome":null,"raw_output_ref":raw.digest},
                    "response":{"path":"/parse/async","status":200,"declared_length":raw.bytes,"body_state":"complete","body":raw}})).unwrap(),
                    "application/json","document-parse","oxr-transport-observation",1).unwrap().object;
                operation = fixture.store.record_external_evidence(operation.operation_id,attempt_id,shenko_types::evidence::EvidenceReceipt {
                    record_schema_version:1,receipt_id:EntityId::new(),operation_id:Some(operation.operation_id),attempt_id:Some(attempt_id),
                    source_id:"document.oxr.transport.v1".into(),source_instance_id:attempt.node_id.to_string(),source_event_key:None,
                    producer_epoch:Some(attempt.epoch.0),producer_schema_version:Some("2".into()),raw_ref:raw,decoder_ref:Some(descriptor.implementation()),
                    decode_status:shenko_types::evidence::DecodeStatus::Decoded,observation_level:None,normalized_ref:Some(normalized),observed_at:Some(accepted_at),
                    received_commit:operation.prepared_commit },None,EffectStatus::Unknown).unwrap();
                Some(task)
            } else if no_send {
                let seal = |value: &serde_json::Value, schema: &str| fixture.store.cas.seal_object(
                    &serde_json::to_vec(value).unwrap(), "application/json", "document-parse", schema, 1).unwrap().object;
                let run = serde_json::json!({"checkpoints":[],"state":"provider_unavailable",
                    "document_failure":"original fixture refused before admission"});
                let manifest = seal(&serde_json::json!({"schema_version":1,"operation_id":operation.operation_id,
                    "attempt_id":attempt_id,"state":"provider_unavailable","pages":[]}), "document-checkpoint-manifest");
                let raw = seal(&run, "document-parse-run");
                let normalized = seal(&serde_json::json!({"schema_version":1,"status":"unavailable",
                    "run":run,"checkpoint_manifest_ref":manifest}), "document-parse-result");
                operation = fixture.store.record_external_evidence(operation.operation_id, attempt_id,
                    shenko_types::evidence::EvidenceReceipt {
                        record_schema_version:1,receipt_id:EntityId::new(),operation_id:Some(operation.operation_id),
                        attempt_id:Some(attempt_id),source_id:"document.parse.result.v1".into(),
                        source_instance_id:attempt.node_id.to_string(),source_event_key:None,
                        producer_epoch:Some(attempt.epoch.0),producer_schema_version:Some("1".into()),raw_ref:raw,
                        decoder_ref:Some(descriptor.implementation()),decode_status:shenko_types::evidence::DecodeStatus::Decoded,
                        observation_level:None,normalized_ref:Some(normalized),observed_at:Some(chrono::Utc::now()),
                        received_commit:operation.prepared_commit,
                    }, Some(OperationOutcome::Failed), EffectStatus::None).unwrap();
                None
            } else { None };
            let original_contract = operation.contract_ref.clone();
            let original_resources = operation.resource_bindings_ref.clone();
            let original_checkpoint_scope = operation.checkpoint_scope_ref.clone();
            let original_input = operation.final_input_ref.clone();
            let original_source_bytes = fixture.store.cas.read_object(&fixture.source.digest).unwrap();
            let original_implementation = fixture.store.external_task(task_id).unwrap().implementation_ref;
            assert_eq!(original_implementation, descriptor.implementation_ref);
            let original_evidence = fixture.store.external_operation_evidence(operation.operation_id).unwrap();
            let (root,source) = fixture.close().await;
            let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
            let node = node_sync::open_node(&root,&store).unwrap();
            let mut fixture = Fixture::open(root,store,node,source).await;
            tokio::time::sleep(Duration::from_millis(25)).await;
            if accepted && matched {
                let owner = fixture.store.external_task(task_id).unwrap();
                let reader = fixture.handler.legacy_recovery_handler();
                for mode in [11,12,10] {
                    peer.state.mode.store(mode,Ordering::SeqCst);
                    let unknown = reader.reconcile(&owner,operation.operation_id).await.unwrap();
                    assert_eq!(unknown.outcome,None);
                    assert_eq!(unknown.effect_status,EffectStatus::Unknown);
                    assert!(!unknown.outcome_conflict,"unbound terminal-looking responses cannot quarantine the original handle");
                    assert_eq!(fixture.store.external_operation_attempt(operation.operation_id).unwrap().attempt_id,attempt_id);
                }
                let malformed = fixture.store.external_operation_evidence(operation.operation_id).unwrap().into_iter()
                    .rfind(|evidence| evidence.source_id == "document.oxr.transport.v1").unwrap();
                let original_body = fixture.store.cas.read_object(&malformed.raw_ref.digest).unwrap();
                assert_eq!(original_body,b"{owned original malformed JSON response");
                let (root,source) = fixture.close().await;
                let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
                let node = node_sync::open_node(&root,&store).unwrap();
                fixture = Fixture::open(root,store,node,source).await;
                assert_eq!(fixture.store.cas.read_object(&malformed.raw_ref.digest).unwrap(),original_body);
                peer.state.mode.store(9,Ordering::SeqCst);
                let prior_results: Vec<_> = fixture.store.external_operation_evidence(operation.operation_id).unwrap()
                    .into_iter().filter(|evidence| evidence.source_id == "document.parse.result.v1").collect();
                // The original admitted reader obtains and seals the real
                // terminal observation, then stops before local page projection.
                let client = shenko_document::legacy_main::OxrClient::new(
                    shenko_document::legacy_main::OxrClientConfig::new(peer.endpoint.clone(),
                        CredentialHandle::new("owned-http-fixture-only"))).unwrap();
                let observed = client.check_document_observed(handle.as_ref().unwrap()).await;
                let document = observed.outcome.unwrap().expect("the original task reached success");
                let response = observed.response.expect("terminal receipt retains the original wire body");
                assert_eq!(response.body_state, shenko_document::legacy_main::OxrBodyState::Complete);
                let raw = fixture.store.cas.seal_object(response.body(), "application/octet-stream", "document-parse",
                    "oxr-http-response-body", 1).unwrap().object;
                let receipt = shenko_document::legacy_main::AttemptReceipt { task: handle.clone(),
                    outcome: Some(Ok(document)), raw_output_ref: Some(raw.digest.0.clone()) };
                assert!(receipt.terminal());
                let normalized = fixture.store.cas.seal_object(&serde_json::to_vec(&serde_json::json!({"schema_version":2,
                    "binding_digest":binding.digest().unwrap(),"receipt":receipt,
                    "response":{"path":response.path,"status":response.status,"declared_length":response.declared_length,
                        "body_state":response.body_state,"body":raw}})).unwrap(),
                    "application/json", "document-parse", "oxr-transport-observation", 1).unwrap().object;
                operation = fixture.store.external_operation(operation.operation_id).unwrap();
                operation = fixture.store.record_external_evidence(operation.operation_id,attempt_id,shenko_types::evidence::EvidenceReceipt {
                    record_schema_version:1,receipt_id:EntityId::new(),operation_id:Some(operation.operation_id),attempt_id:Some(attempt_id),
                    source_id:"document.oxr.transport.v1".into(),source_instance_id:attempt.node_id.to_string(),source_event_key:None,
                    producer_epoch:Some(attempt.epoch.0),producer_schema_version:Some("2".into()),raw_ref:raw.clone(),
                    decoder_ref:Some(descriptor.implementation()),decode_status:shenko_types::evidence::DecodeStatus::Decoded,
                    observation_level:None,normalized_ref:Some(normalized),observed_at:Some(chrono::Utc::now()),
                    received_commit:operation.prepared_commit },None,EffectStatus::Unknown).unwrap();
                assert_eq!(operation.outcome,None);
                let retained_results: Vec<_> = fixture.store.external_operation_evidence(operation.operation_id).unwrap()
                    .into_iter().filter(|evidence| evidence.source_id == "document.parse.result.v1").collect();
                assert_eq!(retained_results, prior_results, "terminal transport is durable before its result projection");
                assert!(fixture.store.native_task_result(task_id).unwrap().is_none());
                let (root,source) = fixture.close().await;
                let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
                let mut node = node_sync::open_node(&root,&store).unwrap();
                let original_unit = node.installed_unit_fact(DOCUMENT_OXR_UNIT).unwrap().unwrap();
                let mut profile: OxrInstallation = serde_json::from_slice(
                    &node.unit_object_bytes(&original_unit.artifact_digest).unwrap()).unwrap();
                assert!(profile.legacy_main.take().is_some());
                Fixture::install_profile(&mut node, &root, &profile, "owned-fixture-terminal-offline");
                let replacement = node.installed_unit_fact(DOCUMENT_OXR_UNIT).unwrap().unwrap();
                assert!(replacement.enabled);
                assert_ne!(replacement.artifact_digest,original_unit.artifact_digest);
                assert_eq!(replacement.revision,"owned-fixture-terminal-offline");
                fixture = Fixture::open_with_credentials(root,store,node,source,false).await;
                assert_eq!(fixture.store.cas.read_object(&malformed.raw_ref.digest).unwrap(),original_body);
                assert_eq!(fixture.store.cas.read_object(&raw.digest).unwrap(),response.body());
            }
            assert_eq!(fixture.service.installed_definitions().iter()
                .filter(|(kind, _)| kind == "document.parse").count(), 1);
            let before_checks = peer.state.checks.load(Ordering::SeqCst);
            let before_uploads = peer.state.uploads.load(Ordering::SeqCst);
            fixture.service.recover().unwrap();
            if no_send {
                assert_eq!(operation.state, OperationState::Resolved);
                assert_eq!(operation.effect_status, EffectStatus::None);
                assert_eq!(before_checks, 0);
                assert_eq!(before_uploads, 0);
                peer.state.mode.store(9, Ordering::SeqCst);
                let fresh = EntityId::new();
                let input = fixture.input(&fixture.source, &[]);
                commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {input}"), &[fresh], vec![]);
                drive_native_until(&fixture.service, || {
                    Ok(peer.state.uploads.load(Ordering::SeqCst) == 1
                        && fixture.store.external_operations_after("document.parse", None, 256)?
                            .iter().any(|entry| entry.task_id == fresh))
                }).await.expect("fresh T1 and original upload were not observed");
                drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(fresh)?.is_some()))
                    .await.expect("fresh original upload did not settle");
                let fresh = fixture.store.external_operations_after("document.parse", None, 256).unwrap()
                    .into_iter().find(|entry| entry.task_id == fresh).unwrap();
                assert_eq!(fresh.outcome, Some(OperationOutcome::Succeeded));
                assert_eq!(fixture.store.external_operation_evidence(operation.operation_id).unwrap(), original_evidence);
            } else if accepted && matched {
                drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(task_id)?.is_some())).await.unwrap();
                let recovered = fixture.store.external_operation(operation.operation_id).unwrap();
                assert_eq!(recovered.outcome,Some(OperationOutcome::Succeeded));
                assert_eq!(peer.state.checks.load(Ordering::SeqCst),before_checks,"sealed terminal recovery must be offline");
                assert_eq!(peer.checked_tasks().last(),handle.as_ref().map(|task| &task.task_id));
                let retained = fixture.store.external_operation_evidence(operation.operation_id).unwrap();
                assert!(original_evidence.iter().all(|evidence| retained.contains(evidence)));
                let terminal = retained.iter().rfind(|evidence| evidence.source_id == "document.oxr.transport.v1").unwrap();
                let attempt = fixture.store.external_operation_attempt(operation.operation_id).unwrap();
                let evidence = fixture.store.external_evidence(attempt.final_receipt_id.unwrap()).unwrap();
                let run: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&evidence.raw_ref.digest).unwrap()).unwrap();
                assert_eq!(run["checkpoints"][0]["result"]["provenance"]["code_version"],"oxr/1.0");
                assert_eq!(run["checkpoints"][0]["result"]["provenance"]["model_version"],binding.model_identity);
                assert_eq!(run["checkpoints"][0]["result"]["coordinates_restored"],true,"retain the original mapper's historical assertion");
                assert_eq!(run["checkpoints"][0]["result"]["provenance"]["raw_output_ref"],terminal.raw_ref.digest.0);
                assert_eq!(run["checkpoints"][0]["result"]["provenance"]["params_digest"],binding.params_digest);
                let read = EntityId::new();
                let input = serde_json::json!({"operation_id":operation.operation_id,"page_index":0});
                commit_native_fixture_step(&fixture.store,&format!("[delegate:document.page.read:1] {input}"),&[read],vec![]);
                drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(read)?.is_some())).await.unwrap();
                let read_op = fixture.store.external_operations_after("document.page.read", None, 256).unwrap()
                    .into_iter().find(|item| item.task_id == read).unwrap();
                assert_eq!(read_op.outcome, Some(OperationOutcome::Succeeded));
                let read_result = fixture.store.native_task_result(read).unwrap().unwrap();
                assert_eq!(read_result.outcome, TaskOutcome::Succeeded);
                let page = read_result.result.unwrap();
                assert!(page["content"].as_str().unwrap().contains("original main mapper page"));
                assert!(page["content"].as_str().unwrap().contains(&fixture.source.digest.0));
                assert_eq!(peer.state.checks.load(Ordering::SeqCst),before_checks,"saved page reads must also stay offline");
            } else {
                if accepted {
                    let owner = fixture.store.external_task(task_id).unwrap();
                    let error = fixture.handler.legacy_recovery_handler().reconcile(&owner,operation.operation_id).await.unwrap_err();
                    assert!(error.to_string().contains("attestation mismatch"));
                    assert_eq!(fixture.store.external_operation_evidence(operation.operation_id).unwrap(),original_evidence);
                } else {
                    drive_native_until(&fixture.service, || Ok(fixture.store.external_operation(operation.operation_id)?.effect_status == EffectStatus::Unknown)).await.unwrap();
                    let retained = fixture.store.external_operation_evidence(operation.operation_id).unwrap();
                    assert!(retained.iter().all(|item| item.source_id == "document.parse.result.v1"));
                    assert!(!retained.is_empty());
                }
                let original = fixture.store.external_operation(operation.operation_id).unwrap();
                assert_eq!(original.effect_status,EffectStatus::Unknown);
                assert_eq!(original.outcome,None);
                assert!(fixture.store.native_task_result(task_id).unwrap().is_none());
                let waiting = EntityId::new(); let input = fixture.input(&fixture.source,&[]);
                {
                    commit_native_fixture_step(&fixture.store,&format!("[delegate:document.parse:1] {input}"),&[waiting],vec![]);
                }
                drive_native_until(&fixture.service, || {
                    let task = fixture.store.external_task(waiting)?;
                    Ok(shenko_store::child::park_reason(&task) == Some("reconciling")
                        && matches!(task.dispatch_eligibility,
                            shenko_types::task::DispatchEligibility::Parked { until_utc: Some(_), .. }))
                }).await.unwrap();
                assert!(!fixture.store.external_operations_after("document.parse",None,256).unwrap().iter().any(|operation| operation.task_id == waiting));
                assert!(fixture.store.native_task_result(waiting).unwrap().is_none(),"unattested original debt remains charged");
                assert_eq!(peer.state.checks.load(Ordering::SeqCst),before_checks,"original operation lacks a permitted query");
            }
            assert_eq!(peer.state.uploads.load(Ordering::SeqCst), before_uploads + usize::from(no_send),
                "only the new operation in the proven no-dispatch case may upload");
            let retained_operation = fixture.store.external_operation(operation.operation_id).unwrap();
            assert_eq!(serde_json::to_value(&retained_operation.contract_ref).unwrap(),
                serde_json::to_value(&original_contract).unwrap());
            assert_eq!(retained_operation.resource_bindings_ref,original_resources);
            assert_eq!(retained_operation.checkpoint_scope_ref,original_checkpoint_scope);
            assert_eq!(retained_operation.final_input_ref,original_input);
            assert_eq!(fixture.store.cas.read_object(&fixture.source.digest).unwrap(), original_source_bytes);
            assert_eq!(fixture.store.external_task(task_id).unwrap().implementation_ref,original_implementation);
            assert_eq!(fixture.store.external_operation_attempt(operation.operation_id).unwrap().attempt_id,attempt_id);
            assert_eq!(fixture.store.external_operation_attempts(operation.operation_id).unwrap().len(),1);
            fixture.close().await;
        }
        peer.finish().await;
    }

    #[test]
    fn cancellation_retains_remote_capacity_while_unrelated_child_progresses() {
        // Keep synchronous fixture publications off the native I/O workers.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2).thread_name("oxr-capacity-native")
            .enable_all().build().unwrap();
        runtime.block_on(capacity_fixture_with_owned_native_runtime());
    }

    async fn capacity_fixture_with_owned_native_runtime() {
        // Keep the actual peer off the client/Store executor: synchronous
        // publications must not prevent its accept loop or connections running.
        struct IsolatedPeer {
            peer: Option<Peer>,
            retire: Option<tokio::sync::oneshot::Sender<Peer>>,
            worker: Option<std::thread::JoinHandle<()>>,
        }
        impl IsolatedPeer {
            fn new(mode: usize) -> Self {
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let (retire_tx, retire_rx) = tokio::sync::oneshot::channel::<Peer>();
                let worker = std::thread::Builder::new().name("oxr-capacity-peer".into()).spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                    runtime.block_on(async move {
                        let peer = Peer::new(mode).await;
                        if ready_tx.send(peer).is_err() { return; }
                        if let Ok(peer) = retire_rx.await {
                            // Cancel, abort and drain every owned connection
                            // before shutting down this dedicated runtime.
                            peer.finish().await;
                        }
                    });
                }).unwrap();
                // Install the owner before waiting, so startup failure also
                // closes the handoff and joins the dedicated runtime thread.
                let mut owner = Self { peer: None, retire: Some(retire_tx), worker: Some(worker) };
                owner.peer = Some(ready_rx.recv_timeout(Duration::from_secs(10))
                    .expect("isolated HTTP peer must start within 10 seconds"));
                owner
            }
            fn shutdown(&mut self) -> std::thread::Result<()> {
                if let Some(peer) = self.peer.take() {
                    peer.stop.cancel();
                    if let Some(retire) = self.retire.take() {
                        // A failed send means the peer thread already exited;
                        // joining below retains its original panic on finish.
                        let _ = retire.send(peer);
                    }
                }
                drop(self.retire.take());
                match self.worker.take() {
                    Some(worker) => worker.join(),
                    None => Ok(()),
                }
            }
            async fn finish(mut self) {
                self.shutdown().expect("isolated HTTP peer thread panicked");
            }
        }
        impl std::ops::Deref for IsolatedPeer {
            type Target = Peer;
            fn deref(&self) -> &Peer { self.peer.as_ref().unwrap() }
        }
        impl Drop for IsolatedPeer {
            fn drop(&mut self) {
                // Always cancel and join, including assertion unwinding; do
                // not turn a peer panic into a second panic during teardown.
                let _ = self.shutdown();
            }
        }
        let peer = IsolatedPeer::new(0); let fixture = Fixture::fresh(&peer, "owned-fixture").await;
        fixture.supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin,
            Arc::new(shenko_exec_host::HostPrintfHandler::new(fixture.store.clone())) as Arc<dyn NativeTaskHandler>).await.unwrap();
        let first = EntityId::new(); let one = fixture.input(&fixture.source, &[]);
        commit_native_fixture_step(&fixture.store, &format!("[delegate:document.parse:1] {one}"), &[first], vec![]);
        drive_native_until(&fixture.service, || {
            let task = fixture.store.external_task(first)?;
            Ok(shenko_store::child::park_reason(&task) == Some("reconciling")
                && fixture.store.external_operations_after("document.parse", None, 256)?.len() == 1)
        }).await.unwrap();
        let original = fixture.store.external_operations_after("document.parse", None, 256).unwrap().pop().unwrap();
        assert_eq!(original.task_id, first);
        assert_eq!(original.effect_status, EffectStatus::Unknown);
        assert_eq!(original.outcome, None);
        let operation_id = original.operation_id;
        let attempt_id = fixture.store.external_operation_attempt(operation_id).unwrap().attempt_id;
        let original_handle = fixture.request_snapshot(operation_id)["provider_task_id"].as_str().unwrap().to_owned();
        let acceptance = fixture.store.external_operation_evidence(operation_id).unwrap().into_iter().find(|receipt| {
            let value: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&receipt.raw_ref.digest).unwrap()).unwrap();
            value["raw_observation"]["source"] == "async_submit"
        }).expect("the original accepted handle must have immutable evidence before the queue arrives");
        let acceptance_bytes = fixture.store.cas.read_object(&acceptance.raw_ref.digest).unwrap();
        let original_receipts = fixture.store.external_operation_evidence(operation_id).unwrap().len();
        let original_checks = peer.state.checks.load(Ordering::SeqCst);

        // More than the eight managed workers, each with an actually admitted,
        // distinct source. A wave of workers sleeping on remote capacity would
        // prevent both the original Settle job and this short task progressing.
        let mut waiting = Vec::new(); let mut sources = Vec::new(); let mut output = String::new();
        for index in 0..12 {
            let source = fixture.store.cas.seal_object(format!("%PDF-1.7\nqueued distinct document {index}\n").as_bytes(),
                "application/pdf", "owned-fixture", "source", 1).unwrap().object;
            let id = EntityId::new(); let input = fixture.input(&source, &[]);
            output.push_str(&format!("[delegate:document.parse:1] {input}\n"));
            waiting.push(id); sources.push(source);
        }
        assert_eq!(sources.iter().map(|source| source.digest.0.as_str()).collect::<std::collections::BTreeSet<_>>().len(), waiting.len());
        assert!(sources.iter().all(|source| source.digest != fixture.source.digest));
        let short = EntityId::new();
        output.push_str("[delegate:host.printf:1] {\"text\":\"short-progress-under-full-document-queue\"}");
        let mut admitted = waiting.clone(); admitted.push(short);
        commit_native_fixture_step(&fixture.store, &output, &admitted, vec![]);
        for id in &admitted { assert_eq!(fixture.store.external_task(*id).unwrap().task_id, *id); }
        // Sample each actual capacity park before the next dispatcher pass
        // can reclaim it. Retry deadlines may expire while another child's
        // synchronous Store publication runs; all children need not be Pending
        // at one instant. Each as-yet-unobserved child has its own 10s phase.
        async fn observe_capacity_parks(
            fixture: &Fixture,
            tasks: &[EntityId],
            minimum_revisions: &std::collections::BTreeMap<EntityId, u64>,
        ) -> std::collections::BTreeMap<EntityId, EntityId> {
            let mut observed = std::collections::BTreeMap::new();
            for target in tasks {
                if observed.contains_key(target) { continue; }
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        for id in tasks {
                            let task = fixture.store.external_task(*id)?;
                            if !matches!(task.state, shenko_types::task::TaskState::Pending { .. })
                                || shenko_store::child::park_reason(&task) != Some("reconciling")
                                || task.memos.get("native_diagnostic").and_then(|value| value.get("detail"))
                                    .and_then(serde_json::Value::as_str)
                                    != Some("document remote capacity is occupied; retry before admission")
                                || !matches!(task.dispatch_eligibility,
                                    shenko_types::task::DispatchEligibility::Parked { until_utc: Some(_), .. })
                                || !shenko_store::child::memo_flag(&task, shenko_store::child::NATIVE_CHILD)
                                || !shenko_store::child::memo_flag(&task, shenko_store::child::NATIVE_RECOVERY)
                                || task.first_run_at.is_none()
                                || task.record_revision.0 <= minimum_revisions.get(id).copied().unwrap_or(0)
                            { continue; }
                            let operation = shenko_store::child::memo_uuid(&task, shenko_store::child::NATIVE_OPERATION_ID)
                                .expect("an actual deferred claim retains its stable operation ID");
                            assert!(fixture.store.get_operation(operation)?.is_none(), "capacity park precedes T1");
                            assert!(fixture.store.external_operation_attempts(operation)?.is_empty());
                            assert!(fixture.store.external_operation_evidence(operation)?.is_empty());
                            assert!(fixture.store.native_task_result(*id)?.is_none());
                            if let Some(previous) = observed.insert(*id, operation) { assert_eq!(previous, operation); }
                        }
                        if observed.contains_key(target) { return anyhow::Ok(()); }
                        fixture.service.run_once().await?;
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.unwrap().unwrap();
            }
            observed
        }
        // Change the original observation only after the full queue is durable.
        // A 401 snapshot proves a new original-handle Settle, independently of
        // whether identical 503 observations are folded by the receipt reader.
        peer.task_mode(&original_handle, 4);
        let observed = observe_capacity_parks(&fixture, &waiting, &std::collections::BTreeMap::new()).await;
        assert_eq!(observed.len(), waiting.len());
        drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(short)?.is_some()
            && peer.state.checks.load(Ordering::SeqCst) > original_checks
            && fixture.store.external_operation_evidence(operation_id)?.len() > original_receipts
            && fixture.request_snapshot(operation_id)["raw_observation"]["http_status"] == 401)).await.unwrap_or_else(|error| {
                panic!("full-queue progress conjunct missing: {error}; short_outcome={:?}; checks={}/>{original_checks}; receipts={:?}/>{original_receipts}; last_http_status={}",
                    fixture.store.native_task_result(short).map(|result| result.map(|result| result.outcome)),
                    peer.state.checks.load(Ordering::SeqCst),
                    fixture.store.external_operation_evidence(operation_id).map(|receipts| receipts.len()),
                    fixture.request_snapshot(operation_id)["raw_observation"]["http_status"]);
            });
        assert_eq!(fixture.store.native_task_result(short).unwrap().unwrap().outcome, TaskOutcome::Succeeded);
        let queued_operations: Vec<_> = waiting.iter().map(|id| {
            let task = fixture.store.external_task(*id).unwrap();
            let operation = shenko_store::child::memo_uuid(&task, shenko_store::child::NATIVE_OPERATION_ID).unwrap();
            assert_eq!(observed.get(id), Some(&operation));
            operation
        }).collect();
        let queued_are_unarmed = |store: &shenko_store::Store| -> Result<()> {
            for (id, operation_id) in waiting.iter().zip(&queued_operations) {
                let task = store.external_task(*id)?;
                assert_eq!(shenko_store::child::memo_uuid(&task, shenko_store::child::NATIVE_OPERATION_ID), Some(*operation_id));
                assert!(store.get_operation(*operation_id)?.is_none(), "capacity deferral must precede T1");
                assert!(store.external_operation_attempts(*operation_id)?.is_empty(), "a deferred child has no attempt");
                assert!(store.external_operation_evidence(*operation_id)?.is_empty(), "no invented scheduling receipt");
                assert!(store.native_task_result(*id)?.is_none(), "no invented Failed/Unknown terminal result");
            }
            Ok(())
        };
        queued_are_unarmed(&fixture.store).unwrap();
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1);
        assert!(peer.checked_tasks().iter().all(|handle| handle == &original_handle));
        assert_eq!(fixture.store.external_operation_attempt(operation_id).unwrap().attempt_id, attempt_id);
        assert_eq!(fixture.store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(fixture.store.external_evidence(acceptance.receipt_id).unwrap().raw_ref, acceptance.raw_ref);
        assert_eq!(fixture.store.cas.read_object(&acceptance.raw_ref.digest).unwrap(), acceptance_bytes);

        let task = fixture.store.external_task(first).unwrap();
        fixture.store.request_native_cancellation(first, EntityId::new(), task.created_by_step_id.unwrap(),
            "owned cancellation fixture", AbortScope::Task).unwrap();
        assert_eq!(fixture.store.external_operation(operation_id).unwrap().effect_status, EffectStatus::Unknown);
        assert_eq!(fixture.store.external_operation(operation_id).unwrap().outcome, None);
        assert!(fixture.store.external_task(first).unwrap().abort_request.is_some());
        queued_are_unarmed(&fixture.store).unwrap();
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1, "local cancellation cannot release remote capacity");

        // The same durable queued children must retry after reopen, while the
        // cancelled original still contributes its authenticated capacity debt.
        let (root, source) = fixture.close().await;
        let store = Arc::new(open_native_fixture(&root)); store.start_subject_dispatcher().unwrap();
        let node = node_sync::open_node(&root, &store).unwrap();
        let fixture = Fixture::open(root, store, node, source).await;
        fixture.supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin,
            Arc::new(shenko_exec_host::HostPrintfHandler::new(fixture.store.clone())) as Arc<dyn NativeTaskHandler>).await.unwrap();
        let revisions = waiting.iter().map(|id| (*id, fixture.store.external_task(*id).unwrap().record_revision.0))
            .collect::<std::collections::BTreeMap<_, _>>();
        let reopened_short = EntityId::new();
        commit_native_fixture_step(&fixture.store, "[delegate:host.printf:1] {\"text\":\"short-progress-after-reopen\"}", &[reopened_short], vec![]);
        let reopened = observe_capacity_parks(&fixture, &waiting, &revisions).await;
        assert_eq!(reopened, observed, "reopened retries retain every original stable operation ID");
        drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(reopened_short)?.is_some())).await.unwrap();
        assert_eq!(fixture.store.native_task_result(reopened_short).unwrap().unwrap().outcome, TaskOutcome::Succeeded);
        queued_are_unarmed(&fixture.store).unwrap();
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 1, "reopen cannot forgive the cancelled original's remote debt");
        assert_eq!(fixture.store.external_operation_attempt(operation_id).unwrap().attempt_id, attempt_id);
        assert_eq!(fixture.store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(fixture.store.cas.read_object(&acceptance.raw_ref.digest).unwrap(), acceptance_bytes);

        // The dispatcher deliberately skips aborted tasks. Observe only that
        // original handle explicitly, as before; the earlier full-queue receipt
        // advancement proved dispatcher Settle independently of cancellation.
        peer.task_mode(&original_handle, 5);
        peer.state.mode.store(2, Ordering::SeqCst);
        let task = fixture.store.external_task(first).unwrap();
        let resolved = fixture.handler.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(resolved.outcome, Some(OperationOutcome::Failed));
        assert_eq!(resolved.effect_status, EffectStatus::Complete);
        drive_native_until(&fixture.service, || Ok(peer.state.uploads.load(Ordering::SeqCst) == 2
            && fixture.store.external_operations_after("document.parse", None, 256)?.len() == 2)).await.unwrap();
        let replacement = fixture.store.external_operations_after("document.parse", None, 256).unwrap().into_iter()
            .find(|operation| operation.operation_id != operation_id).unwrap();
        assert!(waiting.contains(&replacement.task_id));
        assert_eq!(fixture.store.external_operation_attempts(replacement.operation_id).unwrap().len(), 1);
        let still_waiting: Vec<_> = waiting.iter().copied().filter(|id| *id != replacement.task_id).collect();
        let replacement_parks = observe_capacity_parks(&fixture, &still_waiting, &std::collections::BTreeMap::new()).await;
        for (id, operation) in &replacement_parks { assert_eq!(observed.get(id), Some(operation)); }
        drive_native_until(&fixture.service, || Ok(fixture.store.external_operation_attempt(replacement.operation_id)?.final_receipt_id.is_some())).await.unwrap();
        assert_eq!(peer.state.uploads.load(Ordering::SeqCst), 2, "max_documents=1 admits one replacement while it is Unknown");
        for (id, queued_operation) in waiting.iter().zip(&queued_operations) {
            if *id != replacement.task_id {
                assert!(fixture.store.get_operation(*queued_operation).unwrap().is_none());
                assert!(fixture.store.native_task_result(*id).unwrap().is_none());
            }
        }
        peer.state.mode.store(1, Ordering::SeqCst);
        drive_native_until(&fixture.service, || Ok(fixture.store.native_task_result(replacement.task_id)?.is_some()
            && peer.state.uploads.load(Ordering::SeqCst) > 2)).await.unwrap();
        assert_eq!(fixture.store.native_task_result(replacement.task_id).unwrap().unwrap().outcome, TaskOutcome::Succeeded);
        assert!(peer.state.uploads.load(Ordering::SeqCst) <= 1 + waiting.len());
        for (id, queued_operation) in waiting.iter().zip(&queued_operations) {
            let task = fixture.store.external_task(*id).unwrap();
            assert_eq!(shenko_store::child::memo_uuid(&task, shenko_store::child::NATIVE_OPERATION_ID), Some(*queued_operation));
            assert!(fixture.store.external_operation_attempts(*queued_operation).unwrap().len() <= 1);
        }
        assert_eq!(fixture.store.external_operation_attempts(operation_id).unwrap().len(), 1);
        assert_eq!(fixture.store.external_operation_attempt(operation_id).unwrap().attempt_id, attempt_id);
        assert_eq!(fixture.store.external_evidence(acceptance.receipt_id).unwrap().raw_ref, acceptance.raw_ref);
        assert_eq!(fixture.store.cas.read_object(&acceptance.raw_ref.digest).unwrap(), acceptance_bytes);
        assert!(fixture.store.external_task(first).unwrap().abort_request.is_some());
        assert!(fixture.store.native_task_result(first).unwrap().is_none(), "late original evidence cannot complete an aborted task");
        fixture.close().await; peer.finish().await;
    }
}
