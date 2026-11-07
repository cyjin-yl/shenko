// Shared real native runtime for the receipt-boundary regressions below.
struct BoundaryFixture {
    root: PathBuf,
    store: Arc<Store>,
    supervisor: Supervisor,
    native: Arc<NativeChildService>,
    parent: TaskRecord,
    authority: AuthorityRef,
    unit: UnitRef,
}

impl BoundaryFixture {
    async fn open(label: &str, hold_initialize: bool) -> Result<Self> {
        let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
        let root = base.join(format!("{label}-{}", EntityId::new()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("server.py"), SERVER)?;
        if hold_initialize { std::fs::write(root.join("hold-initialize"), b"hold")?; }
        let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
        let (parent_id, authority) = initialize(&store)?;
        let parent = store.external_task(parent_id)?;
        let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
        let profile = McpConnection { description: "receipt boundary fixture".into(), provenance_refs: vec!["synthetic:receipt-boundary".into()],
            connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(), args: vec![root.join("server.py").display().to_string(), root.display().to_string()],
                cwd: Some(root.clone()), env_refs: BTreeMap::new() } };
        let export = profile.export("mcp.receipt-boundary", true)?;
        let unit = UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("revision")?,
            artifact_digest: export.root_artifact_digest.clone().context("digest")? };
        {
            let mut node = node.lock();
            let package_root = root.join("units").display().to_string();
            node.handle_unit_request(UnitRequest::Import { export: plugin_export(UNIT_ID, true)?, package_root: package_root.clone(), enable: Some(true) })?;
            node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
        }
        let supervisor = Supervisor::new(RuntimeGeneration::default());
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, store.clone(), node)).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        Ok(Self { root, store, supervisor, native, parent, authority, unit })
    }

    async fn finish(&self, outcome: Result<()>) -> Result<()> {
        match (outcome, self.supervisor.stop().await) {
            (Err(error), Err(cleanup)) => Err(error.context(format!("fixture teardown also failed: {cleanup}"))),
            (Err(error), _) => Err(error), (Ok(()), cleanup) => cleanup,
        }
    }

    fn tool(kind: &str, input: Value) -> ChildToolCall {
        let task_id = EntityId::new();
        ChildToolCall { call_id: task_id.to_string(), task_id, kind: kind.into(), definition_version: 1, input }
    }
}

#[tokio::test]
async fn receipt_boundary_artifact_roots_require_authentication() -> Result<()> {
    let fixture = BoundaryFixture::open("root-authentication", false).await?;
    let outcome = async {
        let store = &fixture.store;
        let data = store.cas.seal_object(b"chunk", "text/plain", "fixture", "raw", 1)?.object;
        let manifest = json!({"format":"compat-indexed-observation","schema_version":1,"artifact_digest":data.digest,
            "bytes":data.bytes,"chunk_bytes":1048576,"chunks":[data.digest],"tag":"a"});
        let manifest = store.cas.seal_object(&serde_json::to_vec(&manifest)?, "application/json", "fixture", "manifest", 1)?.object;
        let plain = store.cas.seal_object(b"original", "text/plain", "fixture", "plain", 1)?.object;
        let mut observations = Vec::new();
        for object in [plain, manifest] {
            let path = store.cas.verified_object_path(&object)?;
            let mut bytes = store.cas.read_object(&object.digest)?;
            if object.media_type == "application/json" {
                let at = bytes.windows(9).position(|part| part == b"\"tag\":\"a\"").context("tag")?;
                bytes[at + 7] = b'b';
            } else { bytes[0] ^= 1; }
            std::fs::write(path, bytes)?;
            let result = fixture.native.call_tool(&BoundaryFixture::tool(shenko_compat::artifact::NAME,
                json!({"digest":object.digest})), &fixture.parent, &fixture.authority).await?;
            observations.push(result);
        }
        std::fs::write(fixture.root.join("receipt.json"), serde_json::to_vec_pretty(&observations)?)?;
        ensure!(observations.iter().all(|result| result.outcome == "failed"
            && result.result.as_ref().is_some_and(|body| body["observation"]["content_base64"].is_null())),
            "same-size corrupted root returned authenticated-looking bytes: {observations:?}");
        Ok(())
    }.await;
    fixture.finish(outcome).await
}

#[tokio::test]
async fn receipt_boundary_malformed_manifest_records_failed_evidence() -> Result<()> {
    let fixture = BoundaryFixture::open("manifest-validation", false).await?;
    let outcome = async {
        let mut results = Vec::new();
        for (chunk_bytes, artifact_digest) in [(0u64, Digest::blake3(b"empty")), (1, Digest::blake3(b"empty")),
            (u64::MAX, Digest::blake3(b"empty")), (1048576, Digest("invalid".into()))] {
            let manifest = json!({"format":"compat-indexed-observation","schema_version":1,"artifact_digest":artifact_digest,
                "bytes":0,"chunk_bytes":chunk_bytes,"chunks":[]});
            let object = fixture.store.cas.seal_object(&serde_json::to_vec(&manifest)?, "application/json", "fixture", "manifest", 1)?.object;
            let call = BoundaryFixture::tool(shenko_compat::artifact::NAME, json!({"digest":object.digest}));
            let result = fixture.native.call_tool(&call, &fixture.parent, &fixture.authority).await?;
            let operations = fixture.store.external_task_operations(call.task_id, shenko_compat::artifact::NAME)?;
            let operation = operations.first().context("armed artifact operation")?;
            let evidence = fixture.store.external_operation_evidence(operation.operation_id)?;
            results.push(json!({"chunk_bytes":chunk_bytes,"result":result,"operation":operation,"evidence_count":evidence.len()}));
        }
        std::fs::write(fixture.root.join("receipt.json"), serde_json::to_vec_pretty(&results)?)?;
        ensure!(results.iter().all(|row| row["result"]["outcome"] == "failed" && row["evidence_count"] == 1
            && row["result"]["result"]["observation"]["status"] == "artifact_read_failed"
            && row["operation"]["effect_status"] == "none"), "invalid manifest escaped the failed-observation boundary: {results:?}");
        Ok(())
    }.await;
    fixture.finish(outcome).await
}

#[tokio::test]
async fn receipt_boundary_abort_during_initialize_prevents_dispatch() -> Result<()> {
    let fixture = BoundaryFixture::open("cancel-before-dispatch", true).await?;
    let outcome = async {
        let call = call(&fixture.unit, "record");
        let task_id = call.task_id;
        let native = fixture.native.clone();
        let parent = fixture.parent.clone();
        let authority = fixture.authority.clone();
        let worker = tokio::spawn(async move { native.call_tool(&call, &parent, &authority).await });
        let cancellation = async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !fixture.root.join("initialize.started").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
            }).await.context("initialize did not start")?;
            fixture.native.cancel(task_id, "fixture cancellation committed during handshake", AbortScope::Task)?;
            ensure!(fixture.store.external_task(task_id)?.abort_request.is_some(), "abort was not durable");
            anyhow::Ok(())
        }.await;
        // Release and join even when the observation above failed.
        std::fs::write(fixture.root.join("release-initialize"), b"release")?;
        let result = worker.await??;
        cancellation?;
        let operation = fixture.store.external_task_operations(task_id, "compat.mcp.request")?.into_iter().next().context("operation")?;
        let effects = effects(&fixture.root)?;
        std::fs::write(fixture.root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"result":result,"operation":operation,"effects":effects}))?)?;
        ensure!(effects.is_empty() && operation.outcome == Some(shenko_types::operation::OperationOutcome::Cancelled)
            && operation.effect_status == shenko_types::operation::EffectStatus::None,
            "cancelled handshake still dispatched or did not settle cancelled/no-effect: {operation:?}; {effects:?}");
        Ok(())
    }.await;
    fixture.finish(outcome).await
}

#[tokio::test]
async fn receipt_boundary_usage_window_reaches_rare_units_and_latest_evidence() -> Result<()> {
    let fixture = BoundaryFixture::open("usage-window", false).await?;
    let outcome = async {
        // Seed one real native exchange, then import typed synthetic history
        // rows in a single durable batch. This measures the reader, not 3073
        // remote effects or model efficacy.
        let seed = fixture.native.call_tool(&call(&fixture.unit, "record"), &fixture.parent, &fixture.authority).await?;
        let seed_operation = fixture.store.external_operation(seed.operation_id)?;
        let seed_attempt = fixture.store.external_operation_attempt(seed.operation_id)?;
        let seed_evidence = fixture.store.external_operation_evidence(seed.operation_id)?.pop().context("seed evidence")?;
        let mut batch = shenko_types::store::WriteBatch::new();
        for index in 0..3075u64 {
            let rare = index >= 3073;
            let latest = index == 0;
            let method = if rare { if index == 3073 { "skill/older" } else { "skill/latest" } }
                else if latest { "fixture/latest" } else { "fixture/older" };
            let observed = if rare { if index == 3073 { "2029-01-01T00:00:00.123456788Z" } else { "2029-01-01T00:00:00.123456789Z" } }
                else if latest { "2030-01-01T00:00:00.999999999Z" } else { "2030-01-01T00:00:00Z" };
            let frozen = if rare { json!({"unit":{"unit_id":"skill.rare","revision":"1","artifact_digest":fixture.unit.artifact_digest}}) }
                else { json!({"unit":fixture.unit,"request":{"method":method}}) };
            let input = fixture.store.cas.seal_object(&serde_json::to_vec(&frozen)?, "application/json", "fixture", "frozen", 1)?.object;
            let normalized = fixture.store.cas.seal_object(&serde_json::to_vec(&json!({"observation":{"protocol":{"method":method}}}))?,
                "application/json", "fixture", "normalized", 1)?.object;
            let mut operation = seed_operation.clone();
            // The latest record sorts BEFORE the UUID prefix the old scanner
            // admits; chronological order is deliberately unrelated to UUID.
            operation.operation_id = format!("00000000-0000-4000-8000-{index:012x}").parse()?;
            operation.logical_key = format!("synthetic-history:{index}");
            operation.record_revision = RecordRevision(0);
            operation.operation_name = if rare { "compat.skill.source".into() } else { "compat.mcp.request".into() };
            operation.input_digest = input.digest.clone();
            operation.final_input_ref = input;
            let mut attempt = seed_attempt.clone();
            attempt.attempt_id = EntityId::new();
            attempt.operation_id = operation.operation_id;
            let mut evidence = seed_evidence.clone();
            evidence.receipt_id = format!("00000000-0000-4000-9000-{index:012x}").parse()?;
            evidence.operation_id = Some(operation.operation_id);
            evidence.attempt_id = Some(attempt.attempt_id);
            evidence.observed_at = Some(serde_json::from_value(json!(observed))?);
            evidence.normalized_ref = Some(normalized);
            attempt.final_receipt_id = Some(evidence.receipt_id);
            batch.operation_upserts.push(operation);
            batch.attempt_upserts.push(attempt);
            batch.evidence_upserts.push(evidence);
        }
        fixture.store.commit(batch)?;
        let digest = |input| BoundaryFixture::tool(USAGE_DIGEST, input);
        let latest = fixture.native.call_tool(&digest(json!({"grouping":"method","window":{"max_records":1}})),
            &fixture.parent, &fixture.authority).await?;
        let latest_body = &latest.result.as_ref().context("latest result")?["observation"];
        ensure!(latest_body["groups"][0]["method"] == "fixture/latest", "latest window followed UUID order: {latest_body}");
        let mut cursor = Value::Null;
        let mut pages = Vec::new();
        let mut selected = Vec::new();
        for _ in 0..4 {
            let mut input = json!({"grouping":"method","unit":{"unit_id":"skill.rare"},"window":{"max_records":1}});
            if !cursor.is_null() { input["cursor"] = cursor; }
            let result = fixture.native.call_tool(&digest(input), &fixture.parent, &fixture.authority).await?;
            ensure!(result.outcome == "succeeded", "usage page failed: {result:?}");
            let body = result.result.as_ref().context("page result")?["observation"].clone();
            ensure!(body["scanned_operations"].as_u64().context("scan count")? <= 3072, "scan exceeded bound");
            cursor = body["next_cursor"].clone();
            if body["windowed_records"].as_u64() == Some(1) {
                selected.push(body["groups"][0]["method"].as_str().context("selected method")?.to_string());
            }
            pages.push(body);
            if cursor.is_null() { break; }
        }
        std::fs::write(fixture.root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"latest":latest_body,"rare_pages":pages,"selected":selected}))?)?;
        ensure!(selected == ["skill/latest", "skill/older"] && cursor.is_null(),
            "bounded pagination omitted, repeated, reordered, or failed to finish rare observations: {pages:?}");
        ensure!(pages[0]["scanned_operations"] == 3072 && pages[0]["windowed_records"] == 0
            && pages[0]["scan_truncated"] == true && !pages[0]["next_cursor"].is_null(),
            "excluded traffic escaped the scan budget or an empty page lost continuation: {pages:?}");
        let empty = fixture.native.call_tool(&digest(json!({"grouping":"method","window":{"max_records":0}})),
            &fixture.parent, &fixture.authority).await?;
        let empty = &empty.result.as_ref().context("empty result")?["observation"];
        ensure!(empty["groups"] == json!([]) && empty["scanned_operations"] == 0 && empty["next_cursor"].is_null(),
            "zero window scanned or consumed observations: {empty}");
        Ok(())
    }.await;
    fixture.finish(outcome).await
}

#[tokio::test]
async fn receipt_boundary_usage_cursor_orders_ties_missing_times_and_latest_appended_evidence() -> Result<()> {
    let fixture = BoundaryFixture::open("usage-cursor-order", false).await?;
    let outcome = async {
        let seed = fixture.native.call_tool(&call(&fixture.unit, "record"), &fixture.parent, &fixture.authority).await?;
        let seed_operation = fixture.store.external_operation(seed.operation_id)?;
        let seed_attempt = fixture.store.external_operation_attempt(seed.operation_id)?;
        let seed_evidence = fixture.store.external_operation_evidence(seed.operation_id)?.pop().context("seed evidence")?;
        let rows = [
            ("missing-low", None),
            ("missing-high", None),
            ("millisecond", Some("2029-01-01T00:00:00.123Z")),
            ("nano-low", Some("2029-01-01T00:00:00.123000001Z")),
            ("nano-high", Some("2029-01-01T00:00:00.123000001Z")),
            ("latest", Some("2029-01-01T00:00:00.124Z")),
        ];
        let mut batch = shenko_types::store::WriteBatch::new();
        for (index, (method, observed)) in rows.into_iter().enumerate() {
            let input = fixture.store.cas.seal_object(&serde_json::to_vec(&json!({"unit":{"unit_id":"fixture.order"}}))?,
                "application/json", "fixture", "frozen", 1)?.object;
            let normalized = fixture.store.cas.seal_object(&serde_json::to_vec(&json!({"observation":{"protocol":{"method":method}}}))?,
                "application/json", "fixture", "normalized", 1)?.object;
            let mut operation = seed_operation.clone();
            operation.operation_id = format!("00000000-0000-4000-8001-{index:012x}").parse()?;
            operation.logical_key = format!("cursor-order:{index}");
            operation.record_revision = RecordRevision(0);
            operation.operation_name = ["compat.mcp.request", "compat.skill.source", shenko_compat::artifact::NAME][index % 3].into();
            operation.input_digest = input.digest.clone();
            operation.final_input_ref = input;
            let mut attempt = seed_attempt.clone();
            attempt.attempt_id = EntityId::new();
            attempt.operation_id = operation.operation_id;
            let mut evidence = seed_evidence.clone();
            evidence.receipt_id = format!("00000000-0000-4000-9001-{index:012x}").parse()?;
            evidence.operation_id = Some(operation.operation_id);
            evidence.attempt_id = Some(attempt.attempt_id);
            evidence.observed_at = observed.map(|raw| serde_json::from_value(json!(raw))).transpose()?;
            evidence.normalized_ref = Some(normalized);
            attempt.final_receipt_id = Some(evidence.receipt_id);
            if method == "latest" {
                // An earlier receipt with a later observation time must not
                // replace the operation's last appended receipt.
                let mut earlier = evidence.clone();
                earlier.receipt_id = EntityId::new();
                earlier.observed_at = Some(serde_json::from_value(json!("2040-01-01T00:00:00Z"))?);
                batch.evidence_upserts.push(earlier);
            }
            batch.operation_upserts.push(operation);
            batch.attempt_upserts.push(attempt);
            batch.evidence_upserts.push(evidence);
        }
        fixture.store.commit(batch)?;
        let mut cursor = Value::Null;
        let mut selected = Vec::new();
        let mut pages = Vec::new();
        for _ in 0..8 {
            let mut input = json!({"grouping":"method","unit":{"unit_id":"fixture.order"},"window":{"max_records":1}});
            if !cursor.is_null() { input["cursor"] = cursor; }
            let result = fixture.native.call_tool(&BoundaryFixture::tool(USAGE_DIGEST, input), &fixture.parent, &fixture.authority).await?;
            ensure!(result.outcome == "succeeded", "cursor page failed: {result:?}");
            let body = result.result.context("cursor page result")?["observation"].clone();
            if body["windowed_records"] == 1 {
                selected.push(body["groups"][0]["method"].as_str().context("selected method")?.to_string());
            }
            cursor = body["next_cursor"].clone();
            pages.push(body);
            if cursor.is_null() { break; }
        }
        ensure!(selected == ["latest", "nano-high", "nano-low", "millisecond", "missing-high", "missing-low"]
            && cursor.is_null(), "timestamp, receipt tie, latest-append, or missing-time pagination failed: {pages:?}");
        let filtered = fixture.native.call_tool(&BoundaryFixture::tool(USAGE_DIGEST,
            json!({"grouping":"method","unit":{"unit_id":"fixture.order"},
                "window":{"since_rfc3339":"2029-01-01T00:00:00.123000001Z"}})),
            &fixture.parent, &fixture.authority).await?;
        let filtered = &filtered.result.as_ref().context("since result")?["observation"];
        ensure!(filtered["windowed_records"] == 3 && filtered["scanned_operations"] == 3
            && filtered["next_cursor"].is_null(), "since filter lost nanoseconds or admitted missing timestamps: {filtered}");
        std::fs::write(fixture.root.join("receipt.json"), serde_json::to_vec_pretty(&json!({"selected":selected,"pages":pages,"since":filtered}))?)?;
        Ok(())
    }.await;
    fixture.finish(outcome).await
}
