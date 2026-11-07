use super::*;
use shenko_types::operation::{AttemptDispatchStage, EffectStatus, OperationState};

fn admission_trigger(root: &Path, install: bool) -> Result<()> {
    let statement = if install {
        "CREATE TRIGGER reject_native_observation BEFORE INSERT ON inbox_events WHEN NEW.source = 'native-observation' BEGIN SELECT RAISE(FAIL, 'owned admission interruption'); END"
    } else {
        "DROP TRIGGER reject_native_observation"
    };
    let status = std::process::Command::new("/usr/bin/python3").args(["-I", "-c",
        "import sqlite3, sys; db=sqlite3.connect(sys.argv[1]); db.execute(sys.argv[2]); db.commit(); db.close()"])
        .arg(root.join("shenko.db")).arg(statement).status()?;
    ensure!(status.success(), "owned admission trigger failed: {status}");
    Ok(())
}

#[tokio::test]
async fn interrupted_unknown_admission_recovers_original_e1_without_replay() -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let root = base.join(format!("unknown-admission-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.py"), SERVER)?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent_id, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let profile = McpConnection {
        description: "owned original-evidence admission interruption".into(),
        provenance_refs: vec!["synthetic:unknown-admission@1".into()],
        connection: ConnectionSpec::Stdio {
            program: "/usr/bin/python3".into(),
            args: vec!["-I".into(), root.join("server.py").display().to_string(), root.display().to_string()],
            cwd: Some(root.clone()), env_refs: BTreeMap::new(),
        },
    };
    let export = profile.export("mcp.unknown-admission", true)?;
    let unit = UnitRef { unit_id: export.root_unit_id.clone(),
        revision: export.root_revision.clone().context("revision")?,
        artifact_digest: export.root_artifact_digest.clone().context("digest")? };
    {
        let mut node = node.lock();
        let package_root = root.join("units").display().to_string();
        node.handle_unit_request(UnitRequest::Import {
            export: plugin_export(UNIT_ID, true)?, package_root: package_root.clone(), enable: Some(true),
        })?;
        node.handle_unit_request(UnitRequest::Import { export, package_root, enable: Some(true) })?;
    }
    let supervisor = Supervisor::new(RuntimeGeneration::default());
    let attempted: Result<Value> = async {
        supervisor.publish_services().await?;
        supervisor.spawn(NativeChildPlugin, (Arc::clone(&store), RuntimeGeneration::default())).await?;
        supervisor.spawn(McpPlugin, AdapterHost::new(&supervisor, Arc::clone(&store), node)).await?;
        let native = supervisor.context().try_service::<NativeChildService>()?;
        admission_trigger(&root, true)?;
        let interrupted = call(&unit, "drop_after_effect");
        let task_id = interrupted.task_id;
        let returned = native.call_tool(&interrupted, &store.external_task(parent_id)?, &authority).await;
        // Handler admission failure may be a transient direct-call failure;
        // durable operation truth, rather than that wrapper, is authoritative.
        let returned = match returned {
            Ok(value) => json!({"outcome":value.outcome,"operation_id":value.operation_id}),
            Err(error) => json!({"error":format!("{error:#}")}),
        };
        let mut operations = store.external_task_operations(task_id, "compat.mcp.request")?;
        ensure!(operations.len() == 1, "interruption changed operation multiplicity");
        let operation = operations.remove(0);
        let attempt = store.external_operation_attempt(operation.operation_id)?;
        let evidence = store.external_evidence(attempt.final_receipt_id.context("original E1 missing")?)?;
        ensure!(operation.state == OperationState::Reconciling && operation.outcome.is_none()
            && operation.effect_status == EffectStatus::Unknown && attempt.dispatch_stage == AttemptDispatchStage::Unknown,
            "admission failure rewrote original unknown");
        ensure!(store.native_task_result(task_id)?.is_none(), "admission failure fabricated T2");
        let observed = effects(&root)?;
        ensure!(observed.len() == 1 && observed[0]["task_id"] == task_id.to_string(), "effect was lost or replayed");
        let task = store.external_task(task_id)?;
        ensure!(store.pending_inbox_events(task.agent_id, 64)?.is_empty(), "rejected admission unexpectedly queued input");
        let raw = store.cas.read_object(&evidence.raw_ref.digest)?;
        let normalized = evidence.normalized_ref.as_ref().map(|reference| store.cas.read_object(&reference.digest)).transpose()?;
        let mut snapshot = json!({
            "task_id":task_id,"operation":operation,"attempt":attempt,"evidence":evidence,
            "raw_bytes":raw,"normalized_bytes":normalized,"observed_requests":observed,
            "attempts":store.external_operation_attempts(operation.operation_id)?.len(),
            "receipts":store.external_operation_evidence(operation.operation_id)?.len(),
            "direct_return":returned,
        });
        admission_trigger(&root, false)?;
        let Err(error) = shenko_compat::recorded::reconcile_original(
            &store, &task, operation.operation_id, "compat.mcp.request") else {
            anyhow::bail!("original writer remained open after rejected native E1 admission");
        };
        snapshot["original_writer_recovery_error"] = json!(format!("{error:#}"));
        Ok(snapshot)
    }.await;
    let cleanup = supervisor.stop().await;
    let before = attempted.context("original E1 interruption fixture failed")?;
    cleanup?;
    drop(supervisor);
    drop(store);

    // The rejected SQL insert fenced the original writer. Reopen and repair
    // only admission, preserving every original operation and receipt field.
    let store = Store::open(root.join("shenko.db"), root.join("cas"))?;
    let task_id: EntityId = serde_json::from_value(before["task_id"].clone())?;
    let operation_id: EntityId = serde_json::from_value(before["operation"]["operation_id"].clone())?;
    let task = store.external_task(task_id)?;
    for _ in 0..2 {
        shenko_compat::recorded::reconcile_original(&store, &task, operation_id, "compat.mcp.request")?;
    }
    let operation = store.external_operation(operation_id)?;
    let attempt = store.external_operation_attempt(operation_id)?;
    let evidence = store.external_evidence(attempt.final_receipt_id.context("recovered E1 missing")?)?;
    ensure!(serde_json::to_value(&operation)? == before["operation"], "repair changed operation");
    ensure!(serde_json::to_value(&attempt)? == before["attempt"], "repair changed attempt");
    ensure!(serde_json::to_value(&evidence)? == before["evidence"], "repair replaced original E1");
    ensure!(serde_json::to_value(store.cas.read_object(&evidence.raw_ref.digest)?)? == before["raw_bytes"], "repair changed raw evidence");
    let normalized = evidence.normalized_ref.as_ref().map(|reference| store.cas.read_object(&reference.digest)).transpose()?;
    ensure!(serde_json::to_value(normalized)? == before["normalized_bytes"], "repair changed normalized evidence");
    ensure!(store.external_operation_attempts(operation_id)?.len() == 1
        && store.external_operation_evidence(operation_id)?.len() == 1,
        "repair appended an attempt or E1");
    ensure!(serde_json::to_value(effects(&root)?)? == before["observed_requests"], "repair repeated the external request");
    ensure!(store.native_task_result(task_id)?.is_none(), "repair fabricated T2");
    let pending = store.pending_inbox_events(task.agent_id, 64)?;
    ensure!(pending.len() == 1, "original unknown observation was not admitted exactly once: {}", pending.len());
    let event: Value = serde_json::from_str(&pending[0])?;
    let expected_id = serde_json::to_value(evidence.receipt_id)?;
    ensure!(event["event_id"] == expected_id && event["kind"] == "system_event"
        && event["source"] == "native-observation" && event["created_at"] == serde_json::to_value(evidence.observed_at)?
        && event["payload"]["receipt_id"] == expected_id
        && event["payload"]["received_commit"] == serde_json::to_value(evidence.received_commit)?
        && event["payload"]["raw_ref"] == serde_json::to_value(&evidence.raw_ref)?
        && event["payload"]["normalized_ref"] == serde_json::to_value(&evidence.normalized_ref)?,
        "repaired input differs from its original E1");
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
        "before":before,"recovered_event":event,"scope":"real stdio effect and interrupted E1 admission; no model efficacy",
    }))?)?;
    eprintln!("unknown admission fixture: {}", root.display());
    Ok(())
}
