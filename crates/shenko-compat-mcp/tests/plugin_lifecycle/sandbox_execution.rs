use super::*;
use serde::Deserialize;
use shenko_awareness_core::CognitionCatalog;
use shenko_broker::{broker::Broker, local_provider::LocalModelProvider};
use shenko_exec_host::{CommittedTrialOutput, PreparedSandbox, SandboxRuntime,
    UnresolvedSandboxObservation, run_prepared_sandbox};
use shenko_types::operation::{EffectStatus, OperationState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct ProtocolRuntime {
    node: Arc<Mutex<Node>>,
}

impl SandboxRuntime for ProtocolRuntime {
    fn install(
        &self,
        supervisor: &Supervisor,
        sandbox: &PreparedSandbox,
        catalog: &Arc<CognitionCatalog>,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            supervisor.spawn(McpPlugin,
                AdapterHost::new(supervisor, Arc::clone(sandbox.store()), Arc::clone(&self.node))).await?;
            let reader: Arc<dyn NativeTaskHandler> = Arc::new(shenko_exec_host::CognitionReadHandler::new(
                Arc::clone(sandbox.store()), Arc::clone(catalog)));
            supervisor.spawn(NativeTaskHandlerPlugin, reader).await?;
            let native = supervisor.context().try_service::<NativeChildService>()?;
            ensure!(native.has_handler("compat.mcp.request", 1), "initial MCP runtime was not published");
            Ok(())
        }
    }

    fn accept_result(&self, store: &Store, definition: &NativeTaskDefinition, result: &TaskResult) -> Result<()> {
        if result.outcome == TaskOutcome::Succeeded { return Ok(()); }
        ensure!(definition.kind == "compat.mcp.request" && definition.definition_version == 1
            && matches!(result.outcome, TaskOutcome::Failed { .. }), "nonprotocol failure is not a scenario observation");
        let receipt = store.external_evidence(result.receipt_id)?;
        let value = result.result.as_ref().context("failed MCP result has no sealed observation")?;
        let source = ObjectRef::deserialize(&value["source_receipt"])?;
        ensure!(receipt.operation_id == Some(result.operation_id) && receipt.raw_ref == source,
            "failed MCP result lost its original E1");
        Ok(())
    }

    fn accept_unresolved(&self, definition: &NativeTaskDefinition,
        observation: UnresolvedSandboxObservation<'_>) -> Result<()> {
        ensure!(definition.kind == "compat.mcp.request" && definition.definition_version == 1,
            "unrecognized unresolved provider");
        let normalized = observation.normalized.context("unknown MCP observation has no normalized original")?;
        let source = ObjectRef::deserialize(&normalized["source_receipt"])?;
        ensure!(source == observation.receipt.raw_ref, "unknown normalized result points at another E1");
        Ok(())
    }

    fn capture(&self, sandbox: &PreparedSandbox, step: &SubjectStep,
        _output: &CommittedTrialOutput) -> impl std::future::Future<Output = Result<Option<ObjectRef>>> + Send {
        async move {
            Ok(Some(shenko_compat::recorded::seal(sandbox.store(), &json!({
                "step_id":step.step_id,"commit_ref":step.commit_ref,
                "observed_requests":effects(sandbox.installation())?,
            }), "sandbox-native-facts")?))
        }
    }
}

async fn request(socket: &mut tokio::net::TcpStream) -> Result<Value> {
    let mut bytes = Vec::new();
    let (start, length) = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count > 0, "provider fixture closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = std::str::from_utf8(&bytes[..end])?.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>())
            }).context("provider fixture has no body length")??;
            break (end + 4, length);
        }
        ensure!(bytes.len() <= 64 * 1024, "provider headers exceed bound");
    };
    ensure!(length <= 512 * 1024, "provider body exceeds bound");
    while bytes.len() < start + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count > 0, "provider fixture closed before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(serde_json::from_slice(&bytes[start..start + length])?)
}

#[tokio::test]
async fn failed_mcp_t2_reaches_a_committed_continuation() -> Result<()> {
    continuation(false, false, false).await
}

#[tokio::test]
async fn unknown_mcp_e1_reaches_a_committed_continuation_without_t2() -> Result<()> {
    continuation(true, false, false).await
}

#[tokio::test]
async fn deferred_unknown_e1_is_reselected_and_consumed_without_settlement() -> Result<()> {
    continuation(true, true, false).await
}

#[tokio::test]
async fn a_joined_subject_failure_retires_its_owned_sandbox() -> Result<()> {
    continuation(false, false, true).await
}

async fn continuation(unknown: bool, defer_first: bool, provider_rejected: bool) -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/compat-native-fixtures"));
    let prepared = PreparedSandbox::fresh("protocol-subject", &base)?;
    let root = prepared.installation().to_owned();
    let store = Arc::clone(prepared.store());
    let agent = prepared.agent_id();
    std::fs::write(root.join("server.py"), SERVER)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    let profile = McpConnection {
        description: "owned subject protocol observation fixture".into(),
        provenance_refs: vec!["synthetic:subject-observation@1".into()],
        connection: ConnectionSpec::Stdio {
            program: "/usr/bin/python3".into(),
            args: vec!["-I".into(), root.join("server.py").display().to_string(), root.display().to_string()],
            cwd: Some(root.clone()), env_refs: BTreeMap::new(),
        },
    };
    let export = profile.export("mcp.subject-observation", true)?;
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
    let catalog = Arc::new(CognitionCatalog::new(Arc::clone(&node)));
    let name = if unknown { "drop_after_effect" } else { "reported_error" };
    let delegate_input = json!({
        "unit":unit,"request":{"method":"tools/call","params":{"name":name,"arguments":{}}},"timeout_secs":5,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let observed_store = Arc::clone(&store);
    let observed_root = root.clone();
    let server = tokio::spawn(async move {
        let mut delegated = false;
        let mut deferred = false;
        let mut rejections = 0;
        loop {
            let (mut socket, _) = listener.accept().await?;
            let request = request(&mut socket).await?;
            if provider_rejected {
                rejections += 1;
                std::fs::write(observed_root.join(format!("provider-rejection-{rejections}.json")),
                    serde_json::to_vec_pretty(&request)?)?;
                let body = br#"{"error":{"message":"synthetic invalid request","type":"invalid_request_error"}}"#;
                socket.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
                socket.write_all(body).await?;
                continue;
            }
            let observed = effects(&observed_root)?;
            let arrival = if let Some(effect) = observed.first() {
                let task_id: EntityId = serde_json::from_value(effect["task_id"].clone())?;
                if unknown {
                    let operation_id: EntityId = serde_json::from_value(effect["operation_id"].clone())?;
                    observed_store.external_operation_attempt(operation_id)?.final_receipt_id
                } else {
                    observed_store.native_task_result(task_id)?.map(|result| result.result_id)
                }
            } else { None };
            let identity = arrival.map(|id| id.to_string());
            let (_, step) = observed_store.load_subject(agent)?;
            let input = step.input_plan_ref.as_ref().map(|reference| -> Result<Value> {
                Ok(serde_json::from_slice(&observed_store.cas.read_object(&reference.digest)?)?)
            }).transpose()?;
            let selected = identity.as_ref().is_some_and(|identity| input.as_ref().is_some_and(|input|
                input["events"].as_array().is_some_and(|events| events.iter()
                    .any(|event| event["event_id"].as_str() == Some(identity.as_str())))));
            let consumed = selected && identity.as_ref().is_some_and(|identity|
                request["messages"].as_array().is_some_and(|messages| messages.iter()
                    .any(|message| message["content"].as_str().is_some_and(|text| text.contains(identity)))));
            let deferred_output;
            let delegated_output;
            let content: &str = if consumed && defer_first && !deferred {
                let effect = observed.first().context("original effect missing")?;
                let operation_id: EntityId = serde_json::from_value(effect["operation_id"].clone())?;
                let operation = observed_store.external_operation(operation_id)?;
                let attempt = observed_store.external_operation_attempt(operation_id)?;
                let evidence = observed_store.external_evidence(attempt.final_receipt_id.context("original E1 missing")?)?;
                std::fs::write(observed_root.join("deferred-original.json"), serde_json::to_vec_pretty(&json!({
                    "step_id":step.step_id,"step_no":step.step_no,"input_plan_ref":step.input_plan_ref,
                    "operation":operation,"attempt":attempt,"evidence":evidence,
                }))?)?;
                deferred = true;
                deferred_output = format!("Deferring this original observation for one step.\n[dispose] {}", json!({
                    "event_id":arrival.context("selected event has no identity")?,
                    "disposition":"deferred","reason":"Observe this input on the next step",
                }));
                &deferred_output
            } else if consumed { "original protocol observation reached this continuation" }
                else if !delegated {
                    let selector = request["messages"].as_array().context("provider messages missing")?
                        .iter().filter(|message| message["role"] == "system")
                        .filter_map(|message| message["content"].as_str())
                        .flat_map(str::lines)
                        .filter_map(|line| line.strip_prefix("[delegate:")?.split_once(']').map(|(selector, _)| selector))
                        .find(|selector| selector.rsplit_once(':').is_some_and(|(kind, _)| kind == "compat.mcp.request"))
                        .context("the actual provider request omitted the registered MCP invocation selector")?;
                    delegated_output = format!("[delegate:{selector}] {delegate_input}");
                    std::fs::write(observed_root.join("advertised-native-call.json"), serde_json::to_vec_pretty(&json!({
                        "selector":selector,"provider_request":request,"output":delegated_output,
                    }))?)?;
                    delegated = true;
                    &delegated_output
                }
                else { "intervening thought while the native observation arrives" };
            let body = serde_json::to_vec(&json!({
                "choices":[{"message":{"content":content},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":1,"completion_tokens":1},
            }))?;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "protocol-subject-fixture".into()), 1)?;
    let broker_task = tokio::spawn(broker.run());
    let measured = run_prepared_sandbox(prepared, &handle, catalog,
        "Perform the requested protocol operation and report its actual observation.",
        tokio::time::Instant::now() + Duration::from_secs(90), &ProtocolRuntime { node }).await;
    let shutdown = tokio::time::timeout(Duration::from_secs(10), handle.quiesce_for_handoff()).await;
    broker_task.abort();
    let broker_join = broker_task.await;
    server.abort();
    let server_join = server.await;
    ensure!(broker_join.is_err_and(|error| error.is_cancelled()), "provider broker did not join");
    ensure!(matches!(&server_join, Err(error) if error.is_cancelled()), "provider fixture did not join: {server_join:?}");
    let shutdown = shutdown.context("fixture broker drain exceeded deadline")??;
    ensure!(shutdown["local_provider_readers_drained"] == true, "fixture reader did not drain");
    if provider_rejected {
        let error = match measured {
            Err(error) => error,
            Ok(_) => anyhow::bail!("a rejected provider request produced a sandbox decision"),
        };
        ensure!(error.downcast_ref::<shenko_exec_host::SandboxCleanupFailure>().is_none(),
            "a joined subject error was misclassified as an unretired sandbox: {error:#}");
        let (head, step) = store.load_subject(agent)?;
        ensure!(head.committed_step_no == 0 && effects(&root)?.is_empty(),
            "a rejected model request committed an answer or dispatched the operation");
        ensure!(root.join("provider-rejection-1.json").is_file()
            && !root.join("provider-rejection-2.json").exists(),
            "the failed sandbox replayed its original request");
        std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
            "execution_error":format!("{error:#}"),"head":head,"step":step,
            "broker_shutdown":shutdown,"owned_provider_and_broker_joined":true,
            "scope":"Real subject and HTTP provider rejection; the original failed request remains failed while owned runtime retirement completes."
        }))?)?;
        return Ok(());
    }
    let result = measured?;
    ensure!(result.committed.output.text == "original protocol observation reached this continuation",
        "selected a decision before protocol observation consumption");
    let facts_ref = result.facts_ref.context("independent facts missing")?;
    let facts: Value = serde_json::from_slice(&store.cas.read_object(&facts_ref.digest)?)?;
    let observed = effects(&root)?;
    ensure!(observed.len() == 1 && facts["observed_requests"] == serde_json::to_value(&observed)?,
        "capture did not observe the original request exactly once");
    let task_id: EntityId = serde_json::from_value(observed[0]["task_id"].clone())?;
    if unknown {
        ensure!(result.committed.consumed_results.is_empty(), "unknown completion fabricated T2");
        ensure!(result.committed.consumed_observations.len() == 1, "original unknown input was not consumed once");
        let original = &result.committed.consumed_observations[0];
        ensure!(original.task_id == task_id && store.native_task_result(task_id)?.is_none(), "unknown task was settled");
        let operation = store.external_operation(original.operation_id)?;
        ensure!(operation.state == OperationState::Reconciling && operation.outcome.is_none()
            && operation.effect_status == EffectStatus::Unknown && !store.external_task(task_id)?.is_terminal(),
            "observing unknown resolved the original effect or task");
        ensure!(store.event_disposition_commit(agent, original.receipt_id)?.is_some(), "E1 lacks committed consumption");
        ensure!(store.pending_inbox_events(agent, 64)?.is_empty(), "staged E1 remained queued");
        if defer_first {
            let before: Value = serde_json::from_slice(&std::fs::read(root.join("deferred-original.json"))?)?;
            let (head, _) = store.load_subject(agent)?;
            let deferred_id: EntityId = before["step_id"].as_str().context("deferred step identity missing")?.parse()?;
            let deferred = store.get_step(&head.branch_id, deferred_id)?
                .context("deferred step missing")?;
            ensure!(deferred.step_id == deferred_id && Some(deferred.step_no) == before["step_no"].as_u64()
                && deferred.state == SubjectStepState::Committed && deferred.step_id != result.committed.step_id,
                "observation was not deferred in a distinct committed step");
            let decision_ref = deferred.decision_ref.context("deferred decision missing")?;
            let decision: StepDecision = serde_json::from_slice(&store.cas.read_object(&decision_ref.digest)?)?;
            ensure!(decision.event_dispositions.iter().any(|disposition|
                disposition.event_id == original.receipt_id && disposition.disposition == Disposition::Deferred),
                "earlier committed decision did not defer original E1");
            let attempt = store.external_operation_attempt(original.operation_id)?;
            let evidence = store.external_evidence(original.receipt_id)?;
            ensure!(serde_json::to_value(&operation)? == before["operation"]
                && serde_json::to_value(&attempt)? == before["attempt"]
                && serde_json::to_value(&evidence)? == before["evidence"],
                "reselection changed original operation, attempt or E1");
            ensure!(store.external_operation_attempts(original.operation_id)?.len() == 1
                && store.external_operation_evidence(original.operation_id)?.len() == 1,
                "deferred observation repeated original execution");
        }
    } else {
        ensure!(result.committed.consumed_observations.is_empty(), "failed T2 fabricated an unknown event");
        ensure!(result.committed.consumed_results.len() == 1, "failed protocol result not consumed");
        let original = &result.committed.consumed_results[0];
        ensure!(original.task_id == task_id && matches!(original.outcome, TaskOutcome::Failed { .. }),
            "real business error became success");
        ensure!(original.result.as_ref().context("failed result body missing")?["observation"]["protocol"]["result"]["isError"] == true,
            "original MCP isError was lost");
        ensure!(store.event_disposition_commit(agent, original.result_id)?.is_some(), "failed T2 lacks committed consumption");
    }
    std::fs::write(root.join("receipt.json"), serde_json::to_vec_pretty(&json!({
        "committed":result.committed,"facts_ref":facts_ref,"facts":facts,"broker_shutdown":shutdown,
        "scope":"synthetic provider; real subject, MCP stdio, Store and committed input; no model efficacy",
    }))?)?;
    eprintln!("protocol subject fixture: {}", root.display());
    Ok(())
}
