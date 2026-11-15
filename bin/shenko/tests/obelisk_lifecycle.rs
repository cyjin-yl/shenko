//! One real Store/Node/native-task lifecycle. This is mechanism evidence, not
//! model efficacy or an Obelisk CLI compatibility substitute.
use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde_json::json;
use shenko::obelisk::{self, ObeliskPlugin, TASK_KIND, UNIT_ID};
use shenko_compat::{installation::plugin_export, runtime::AdapterHost};
use shenko_kernel::{
    Supervisor,
    task::{ChildToolCall, ChildToolRuntime},
};
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_subject::{NativeChildPlugin, NativeChildService};
use shenko_types::{commit::CommitRef, ids::*, operation::AuthorityRef, subject::*, task::*};
use std::{path::PathBuf, sync::Arc, time::Duration};

fn initialize(store: &Store) -> Result<(TaskRecord, AuthorityRef)> {
    let agent = EntityId::new();
    let genesis_id = EntityId::new();
    let first_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();
    let zero = CommitRef::new(ledger_id, LedgerSeq::ZERO);
    let object = store
        .cas
        .seal_object(
            b"external history lifecycle fixture",
            "text/plain",
            "fixture",
            "raw",
            1,
        )?
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
        created_commit: zero,
    };
    let head = SubjectBranchHead {
        agent_id: agent,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration("external-history-fixture".into()),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: zero,
        next_step_id: first_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };
    let first = SubjectStep {
        step_id: first_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: zero,
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
    let root = TaskRecord::subject_root(head.subject_root_task_id, agent, object, zero);
    store.initialize_genesis(&genesis, &head, &first, &root)?;
    store.start_subject_dispatcher()?;
    Ok((
        root.clone(),
        AuthorityRef {
            delegation_id: root.task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: genesis_id,
        },
    ))
}

async fn runtime(
    store: Arc<Store>,
    node: Arc<Mutex<Node>>,
    root: &std::path::Path,
) -> Result<(Supervisor, Arc<NativeChildService>)> {
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    supervisor.publish_services().await?;
    supervisor
        .spawn(
            NativeChildPlugin,
            (store.clone(), RuntimeGeneration::default()),
        )
        .await?;
    supervisor
        .spawn(
            ObeliskPlugin {
                config_path: obelisk::config_path(root),
            },
            AdapterHost::new(&supervisor, store, node),
        )
        .await?;
    let native = supervisor.context().try_service::<NativeChildService>()?;
    Ok((supervisor, native))
}

async fn wait_definition(native: &NativeChildService, enabled: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        while native.has_handler(TASK_KIND, 1) != enabled {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("Node change did not update the native definition")
}

fn tool(input: serde_json::Value) -> ChildToolCall {
    let task_id = EntityId::new();
    ChildToolCall {
        call_id: task_id.to_string(),
        task_id,
        kind: TASK_KIND.into(),
        definition_version: 1,
        input,
    }
}

#[tokio::test]
async fn optional_archive_queries_survive_source_loss_disable_and_restart() -> Result<()> {
    let root: PathBuf =
        std::env::temp_dir().join(format!("shenko-obelisk-lifecycle-{}", EntityId::new()));
    std::fs::create_dir_all(root.join("node"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    let file = root.join("sources/session.jsonl");
    std::fs::write(
        &file,
        format!(
            "{}\n",
            json!({"id":"m1","role":"assistant","timestamp":"2026-11-01T12:00:00Z","content":format!("{}权限认证故障最终修复 auth.rs", "padding ".repeat(1600))})
        ),
    )?;
    // Missing executable deliberately demonstrates that CAS reads and imports
    // do not require a live external CLI on the receiving node.
    std::fs::write(
        obelisk::config_path(&root),
        serde_json::to_vec(&json!({
            "executable":root.join("no-cli"),"source_checkout":root.join("offline-upstream"),
            "arguments":[root.join("offline-upstream/packages/cli/src/obelisk.ts")],"index_home":root.join("index"),
            "source_instance":"portable-origin","sources":[{"provider":"pi","root_label":"personal-pi","path":root.join("sources")}]
        }))?,
    )?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    node.lock().handle_unit_request(UnitRequest::Import {
        export: plugin_export(UNIT_ID, false)?,
        package_root: root.join("units").display().to_string(),
        enable: Some(false),
    })?;
    let (supervisor, native) = runtime(store.clone(), node.clone(), &root).await?;
    let result: Result<()> = async {
        ensure!(!native.has_handler(TASK_KIND, 1), "disabled installation published history capability");
        let mut editor = Node::open(&root.join("node"))?;
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: true })?;
        wait_definition(&native, true).await?;
        let archive_call = tool(json!({"op":"archive","source":{"provider":"pi","source_instance":"portable-origin","source_key":"session:portable","session_id":"portable"},"file":file}));
        let archived = native.call_tool(&archive_call, &parent, &authority).await?;
        ensure!(archived.outcome == "succeeded", "archive failed: {archived:?}");
        let original = archived.result.as_ref().context("archive result")?["observation"]["result"].clone();
        ensure!(original["record_count"] == 1, "missing archived record: {original}");
        let operation_id = archived.operation_id;
        let receipt_id = store.external_operation_attempt(operation_id)?.final_receipt_id.context("archive has no durable E1")?;
        std::fs::remove_file(&file)?;
        std::fs::remove_file(obelisk::config_path(&root))?;
        let searched = native.call_tool(&tool(json!({"op":"search","query":"权限认证","limit":2})), &parent, &authority).await?;
        ensure!(searched.outcome == "succeeded", "archive search required original/CLI: {searched:?}");
        let body = &searched.result.as_ref().context("search result")?["observation"]["result"];
        let event_id = body["hits"][0]["event_id"].clone();
        ensure!(body["hits"][0]["preview"].as_str().is_some_and(|s|s.contains("auth.rs")), "long-tail CJK evidence missing: {body}");
        let read = native.call_tool(&tool(json!({"op":"read","event_id":event_id,"limit_chars":32})), &parent, &authority).await?;
        ensure!(read.outcome == "succeeded", "read failed: {read:?}");
        ensure!(read.result.as_ref().context("read result")?["observation"]["result"]["page"]["returned_chars"] == 32, "record output was not bounded");
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: false })?;
        wait_definition(&native, false).await?;
        let unavailable = native.call_tool(&tool(json!({"op":"overview"})), &parent, &authority).await?;
        ensure!(unavailable.outcome == "unavailable", "disabled plugin still dispatched");
        supervisor.stop().await?;
        // A new runtime generation observes the same Node enable fact; the
        // original completed task is returned without reading the lost source.
        editor.handle_unit_request(UnitRequest::SetEnabled { unit_id: UNIT_ID.into(), enabled: true })?;
        let (restarted, native) = runtime(store.clone(), node.clone(), &root).await?;
        let recovered = native.call_tool(&archive_call, &parent, &authority).await;
        let cleanup = restarted.stop().await;
        let recovered = recovered?;
        cleanup?;
        ensure!(recovered.outcome == "succeeded" && recovered.operation_id == operation_id, "restart repeated or lost original archive: {recovered:?}");
        ensure!(store.external_operation_attempts(operation_id)?.len() == 1 && store.external_operation_attempt(operation_id)?.final_receipt_id == Some(receipt_id), "recovery invented another attempt or E1");
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    if result.is_ok() && cleanup.is_ok() {
        let _ = std::fs::remove_dir_all(&root);
    }
    result.and(cleanup)
}

#[cfg(feature = "compat-mcp")]
#[tokio::test]
#[ignore = "requires the independently installed audited Obelisk source and real Node executable"]
async fn real_script_receipt_remains_unknown_and_readable_after_mcp_retirement() -> Result<()> {
    use shenko_types::operation::EffectStatus;
    let executable =
        std::env::var_os("SHENKO_OBELISK_EXECUTABLE").context("real Node executable required")?;
    let checkout = PathBuf::from(
        std::env::var_os("SHENKO_OBELISK_SOURCE_CHECKOUT")
            .context("audited Obelisk source required")?,
    );
    let root = std::env::temp_dir().join(format!("shenko-obelisk-native-cli-{}", EntityId::new()));
    std::fs::create_dir_all(root.join("node"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(
        obelisk::config_path(&root),
        serde_json::to_vec(&json!({
            "executable":PathBuf::from(executable), "source_checkout":checkout,
            "arguments":[checkout.join("packages/cli/src/obelisk.ts")],"index_home":root.join("index"),
            "source_instance":"synthetic-native-proof","sources":[{"provider":"pi","root_label":"proof","path":root.join("sources")}]
        }))?,
    )?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    for id in [UNIT_ID, shenko_compat_mcp::UNIT_ID] {
        node.lock().handle_unit_request(UnitRequest::Import {
            export: plugin_export(id, true)?,
            package_root: root.join("units").display().to_string(),
            enable: Some(true),
        })?;
    }
    let (supervisor, native) = runtime(store.clone(), node.clone(), &root).await?;
    let result: Result<()> = async {
        supervisor.spawn(shenko_compat_mcp::McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        ensure!(native.has_handler("compat.artifact.read", 1) && native.has_handler(TASK_KIND, 1), "independent plugins cannot coexist");
        let query = native.call_tool(&tool(json!({"op":"obelisk","request":{"op":"sessions","limit":1}})), &parent, &authority).await?;
        ensure!(query.outcome == "succeeded", "pinned standard query did not complete: {query:?}");
        let scripted = native.call_tool(&tool(json!({"op":"obelisk","request":{"op":"script","script":"return {proof:'observed', payload:'x'.repeat(300000)};"}})), &parent, &authority).await?;
        ensure!(scripted.outcome == "unknown", "arbitrary JS exit was promoted to complete external effects");
        let operation = store.external_operation(scripted.operation_id)?;
        ensure!(operation.effect_status == EffectStatus::Unknown && operation.outcome.is_none(), "unknown effects were lost");
        let evidence = store.external_operation_evidence(scripted.operation_id)?;
        ensure!(evidence.len() == 1, "script did not preserve exactly one original E1");
        let normalized_ref = evidence[0].normalized_ref.as_ref().context("normalized script E1")?;
        let normalized: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&normalized_ref.digest)?)?;
        ensure!(normalized["observation"]["status"] == "observation_outlined", "large observation was not outlined");
        ensure!(normalized["observation"]["read"]["handler"] == TASK_KIND && normalized["observation"]["read"]["input"]["op"] == "artifact", "outline points to another plugin's reader");
        node.lock().handle_unit_request(UnitRequest::SetEnabled { unit_id: shenko_compat_mcp::UNIT_ID.into(), enabled: false })?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while native.has_handler("compat.artifact.read", 1) { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await?;
        let read = native.call_tool(&tool(json!({"op":"artifact","digest":normalized["source_receipt"]["digest"],"limit":128})), &parent, &authority).await?;
        ensure!(read.outcome == "succeeded", "MCP retirement removed history receipt reader: {read:?}");
        let payload = &read.result.as_ref().context("artifact result")?["observation"]["result"];
        ensure!(payload["total_bytes"].as_u64().is_some_and(|n|n > 300000) && payload["next_offset"] == 128, "original outlined observation is not range-readable: {payload}");
        let original = obelisk::handler(store.clone(), node.clone(), obelisk::config_path(&root))
            .reconcile(&store.external_task(scripted.task_id)?, scripted.operation_id).await?;
        ensure!(original.effect_status == EffectStatus::Unknown && store.external_operation_evidence(scripted.operation_id)?.len() == 1
            && store.external_operation_attempts(scripted.operation_id)?.len() == 1, "reconciliation replayed JS or rewrote E1");
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    if result.is_ok() && cleanup.is_ok() {
        let _ = std::fs::remove_dir_all(&root);
    }
    result.and(cleanup)
}
