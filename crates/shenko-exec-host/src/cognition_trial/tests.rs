use super::*;
use shenko_awareness_core::parse_cognition_entry;
use shenko_broker::{broker::Broker, local_provider::LocalModelProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BODY_MARKER: &str = "cognition-native-body-73c90b";
const FOLLOWUP_MARKER: &str = "cognition-followup-body-4ab628";

async fn fixture_request(socket: &mut tokio::net::TcpStream) -> Result<serde_json::Value> {
    let mut bytes = Vec::new();
    let (header_end, length) = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count > 0, "fixture request closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&bytes[..end])?;
            let length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>())
            }).context("fixture request has no length")??;
            break (end + 4, length);
        }
        ensure!(bytes.len() <= 64 * 1024, "fixture headers exceed bound");
    };
    ensure!(length <= 512 * 1024, "fixture body exceeds bound");
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await?;
        ensure!(count > 0, "fixture request closed before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(serde_json::from_slice(&bytes[header_end..header_end + length])?)
}

#[tokio::test]
async fn native_read_reaches_the_committed_continuation() {
    native_read_fixture(false, false).await;
}

#[tokio::test]
async fn a_consuming_continuation_can_delegate_a_followup_read() {
    native_read_fixture(true, false).await;
}

#[tokio::test]
async fn a_failed_native_read_remains_an_attempt_failure_after_cleanup() {
    native_read_fixture(false, true).await;
}

async fn native_read_fixture(followup: bool, missing: bool) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/cognition-trial-regression")
        .join(EntityId::new().to_string());
    std::fs::create_dir_all(&root).unwrap();
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node")).unwrap()));
    let catalog = Arc::new(CognitionCatalog::new(node));
    let seed = shenko_awareness_core::seed_packages().unwrap().remove(0);
    let package = parse_cognition_entry(
        &format!("{}\n{BODY_MARKER}\n", seed.entry_text), Path::new(&seed.unit_id),
    ).unwrap();
    catalog.install(&package, &root.join("cognition")).unwrap();
    let seed = shenko_awareness_core::seed_packages().unwrap().into_iter()
        .find(|seed| seed.unit_id != package.unit_id).unwrap();
    let second = parse_cognition_entry(
        &format!("{}\n{FOLLOWUP_MARKER}\n", seed.entry_text), Path::new(&seed.unit_id),
    ).unwrap();
    if followup { catalog.install(&second, &root.join("cognition")).unwrap(); }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let requested = if missing { "uninstalled.fixture.unit" } else { &package.unit_id };
    let delegate = format!("[delegate:cognition.unit.read:1] {{\"unit_id\":\"{requested}\",\"revision\":\"{}\"}}", package.revision);
    // The reader accepts surrounding whitespace; observation must validate
    // the same normalized identity when this later read is consumed.
    let second_delegate = format!("[delegate:cognition.unit.read:1] {{\"unit_id\":\" {} \",\"revision\":\" {} \"}}", second.unit_id, second.revision);
    let server = tokio::spawn(async move {
        let mut delegated = false;
        let mut delegated_second = false;
        loop {
            let (mut socket, _) = listener.accept().await?;
            let request = fixture_request(&mut socket).await?;
            let has_body = request["messages"].as_array().context("messages missing")?.iter()
                .any(|message| message["content"].as_str().is_some_and(|text| text.contains(BODY_MARKER)));
            let has_second = request["messages"].as_array().context("messages missing")?.iter()
                .any(|message| message["content"].as_str().is_some_and(|text| text.contains(FOLLOWUP_MARKER)));
            let content = if followup && has_body && !delegated_second {
                delegated_second = true;
                &second_delegate
            } else if (followup && has_second) || (!followup && has_body) {
                "native body reached this continuation"
            } else if !delegated {
                delegated = true;
                &delegate
            } else { "intervening thought while the native read runs" };
            let body = serde_json::to_vec(&serde_json::json!({
                "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            }))?;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(LocalModelProvider::new(endpoint, "native-trial-fixture".into()), 1).unwrap();
    let broker_task = tokio::spawn(broker.run());
    let result = committed_output("native-read", &root, &handle, catalog,
        "Read the installed cognition unit before answering.", tokio::time::Instant::now() + Duration::from_secs(60)).await;
    broker_task.abort();
    assert!(broker_task.await.unwrap_err().is_cancelled());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    if missing {
        assert!(result.is_err(), "a failed real T2 read must fail the attempt");
        let installation = std::fs::read_dir(&root).unwrap().map(|entry| entry.unwrap())
            .find(|entry| entry.file_name().to_string_lossy().starts_with("native-read-")).unwrap().path();
        let store = Store::open(installation.join("shenko.db"), installation.join("cas")).unwrap();
        let agent = store.get_existing_agent_id().unwrap().unwrap();
        let decisions = store.committed_decisions_after(agent, 0, None, 64).unwrap();
        let intent = decisions.iter().flat_map(|(_, _, decision)| &decision.delegated_task_intents)
            .find(|intent| intent.definition.as_ref().is_some_and(|definition|
                definition.input["unit_id"] == "uninstalled.fixture.unit")).unwrap();
        let result = store.native_task_result(intent.intent_id).unwrap().unwrap();
        assert!(matches!(result.outcome, TaskOutcome::Failed { .. }));
        assert_eq!(result.result.as_ref().unwrap()["error"]["code"], "unit_not_readable");
        let consuming = decisions.iter().find(|(_, _, decision)| decision.event_dispositions.iter()
            .any(|disposition| disposition.event_id == result.result_id
                && disposition.disposition != Disposition::Deferred)).unwrap();
        let task = store.external_task(result.task_id).unwrap();
        assert_eq!(store.event_disposition_commit(task.agent_id, result.result_id).unwrap(), consuming.1.commit_ref);
        return;
    }
    let output = result.expect("fresh production assembly must consume the real native read");
    assert_eq!(output.output.text, "native body reached this continuation");
    assert!(!output.output.controls.iter().any(|control| matches!(control.action, OutputAction::Delegate { .. })));
    let result = output.consumed_results.iter().find(|result|
        result.result.as_ref().is_some_and(|value| value["unit"]["unit_id"] == package.unit_id))
        .expect("the measured continuation must consume the installed unit's real T2 result");
    let unit = &result.result.as_ref().unwrap()["unit"];
    assert_eq!(unit["revision"], package.revision);
    assert_eq!(unit["artifact_digest"], Digest::blake3(package.entry_text.as_bytes()).0);
    assert_eq!(result.outcome, TaskOutcome::Succeeded);
    if followup {
        let second_result = output.consumed_results.iter().find(|result|
            result.result.as_ref().is_some_and(|value| value["unit"]["unit_id"] == second.unit_id))
            .expect("the later delegated read must settle and be consumed before scoring");
        assert_ne!(result.task_id, second_result.task_id);
        assert_eq!(second_result.result.as_ref().unwrap()["unit"]["artifact_digest"],
            Digest::blake3(second.entry_text.as_bytes()).0);
        assert_eq!(second_result.outcome, TaskOutcome::Succeeded);
    }
}

enum FixtureEntry {
    PublicRunner,
    NativeTrial,
}

#[derive(Serialize)]
struct NativeTrialFixture {
    source_export: shenko_node::UnitExport,
    attempt_id: String,
    attempt: serde_json::Value,
    evidence: serde_json::Value,
}

struct PreparedFixture {
    root: PathBuf,
    store: Arc<Store>,
    source_read: serde_json::Value,
    source_metadata: serde_json::Value,
    report: TrialReport,
    native_trial: Option<NativeTrialFixture>,
}

async fn prepared_fixture(learned: bool, empty_judge: bool, entry: FixtureEntry) -> Result<PreparedFixture> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../build/cognition-trial-regression"));
    let root = base.join(format!("prepared-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let (_, store, agent) = sandbox("source", &root)?;
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("source-node"))?));
    let catalog = Arc::new(CognitionCatalog::new(node));
    let (unit_id, revision) = if learned {
        let source = store.cas.seal_object(b"an observed native fixture read", "text/plain",
            "fixture", "cognition-learning-source", 1)?.object;
        let (head, _) = store.load_subject(agent)?;
        let parent = store.external_task(head.subject_root_task_id)?;
        let authority = AuthorityRef {
            delegation_id: parent.task_id, delegation_revision: RecordRevision(0),
            decision_ref: head.genesis_ref.genesis_id,
        };
        store.start_subject_dispatcher()?;
        let supervisor = Supervisor::new(RuntimeGeneration::default());
        let written: Result<()> = async {
            supervisor.publish_services().await?;
            supervisor.spawn(NativeChildPlugin,
                (Arc::clone(&store), RuntimeGeneration::default())).await?;
            let writer: Arc<dyn NativeTaskHandler> = Arc::new(crate::CognitionWriteHandler::new(
                Arc::clone(&store), Arc::clone(&catalog), root.join("source-cognition"), None));
            supervisor.spawn(NativeTaskHandlerPlugin, writer).await?;
            let native = supervisor.context().try_service::<NativeChildService>()?;
            let task_id = EntityId::new();
            let result = native.call_tool(&shenko_kernel::task::ChildToolCall {
                call_id: task_id.to_string(), task_id, kind: "cognition.unit.write".into(),
                definition_version: 1,
                input: serde_json::json!({
                    "unit_id": "awareness.preparation-fidelity", "revision": "1",
                    "kind": "awareness", "title": "Observed content scope",
                    "capability_card": "A read establishes the addressed content's scope",
                    "body": format!("{BODY_MARKER}\nA read establishes the addressed content; other effects need their own observations."),
                    "why": "Observations have a source and a scope",
                    "cues": ["source-fidelity-cue"],
                    "negative_cues": ["source-fidelity-negative"],
                    "applicability": "source-fidelity-applicability",
                    "knowledge": ["Only the observed scope is established"],
                    "provenance_refs": [format!("native-fixture:{}", source.digest.0)],
                    "derived_from": {"kind": "cas_object", "reference": "native preparation observation",
                        "digest": source.digest.0},
                    "enabled": false,
                }),
            }, &parent, &authority).await?;
            ensure!(result.outcome == "succeeded", "source writer failed: {result:?}");
            Ok(())
        }.await;
        let cleanup = supervisor.stop().await;
        written?;
        cleanup?;
        ("awareness.preparation-fidelity".to_string(), "1".to_string())
    } else {
        let seed = shenko_awareness_core::seed_packages()?.remove(0);
        let package = parse_cognition_entry(&format!("{}\n{BODY_MARKER}\n", seed.entry_text),
            Path::new(&seed.unit_id))?;
        catalog.install(&package, &root.join("source-cognition"))?;
        (package.unit_id, package.revision)
    };
    let source_read = serde_json::to_value(catalog.read(&unit_id, &revision)?)?;
    let source_metadata = serde_json::to_value(catalog.units()?.into_iter()
        .find(|unit| unit.unit_id == unit_id).context("source candidate missing")?)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let delegate = format!("[delegate:cognition.unit.read:1] {{\"unit_id\":\"{unit_id}\",\"revision\":\"{revision}\"}}");
    let server = tokio::spawn(async move {
        let mut delegated = false;
        loop {
            let (mut socket, _) = listener.accept().await?;
            let request = fixture_request(&mut socket).await?;
            let messages = request["messages"].as_array().context("messages missing")?;
            let is_judge = messages.iter().any(|message| message["role"] == "system"
                && message["content"].as_str().is_some_and(|text| text.contains("你只做评审")));
            let body_or_failure = messages.iter().any(|message| message["content"].as_str()
                .is_some_and(|text| text.contains(BODY_MARKER) || text.contains("unit_not_readable")));
            let content = if is_judge {
                // Subject cleanup completed before this request. The next
                // subject belongs to the other fresh condition.
                delegated = false;
                if empty_judge { "" } else { "EFFECTIVE the native read reached this decision" }
            } else if body_or_failure {
                "native body reached this continuation"
            } else if !delegated {
                delegated = true;
                &delegate
            } else {
                "intervening thought while the native read runs"
            };
            let body = serde_json::to_vec(&serde_json::json!({
                "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 37, "completion_tokens": 3},
            }))?;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await?;
            socket.write_all(&body).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "native-preparation-fixture".into()), 1)?;
    let broker_task = tokio::spawn(broker.run());
    let spec = TrialSpec {
        unit_id, revision, trials_per_condition: 1, deadline_ms: 90_000,
        scenarios: vec![TrialScenario { name: "fidelity".into(),
            message: "Read the installed unit at the exact requested identity before answering.".into(),
            rubric: "The decision is based on the actual native read.".into() }],
    };
    let report = match entry {
        FixtureEntry::PublicRunner => run_sandbox_trial(Arc::clone(&store),
            Arc::clone(&catalog), handle.clone(), root.join("trial"), &spec).await
            .map(|report| (report, None)),
        FixtureEntry::NativeTrial => {
            let supervisor = Supervisor::new(RuntimeGeneration::default());
            let measured: Result<(TrialReport, NativeTrialFixture)> = async {
                if !learned { store.start_subject_dispatcher()?; }
                let source_export = catalog.export(&spec.unit_id)?;
                let (head, _) = store.load_subject(agent)?;
                let parent = store.external_task(head.subject_root_task_id)?;
                let authority = AuthorityRef {
                    delegation_id: parent.task_id, delegation_revision: RecordRevision(0),
                    decision_ref: head.genesis_ref.genesis_id,
                };
                supervisor.publish_services().await?;
                supervisor.spawn(NativeChildPlugin,
                    (Arc::clone(&store), RuntimeGeneration::default())).await?;
                let handler: Arc<dyn NativeTaskHandler> = Arc::new(CognitionTrialHandler::new(
                    Arc::clone(&store), Arc::clone(&catalog), handle.clone(), root.join("trial")));
                supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
                let native = supervisor.context().try_service::<NativeChildService>()?;
                let task_id = EntityId::new();
                let result = native.call_tool(&shenko_kernel::task::ChildToolCall {
                    call_id: task_id.to_string(), task_id, kind: "cognition.unit.trial".into(),
                    definition_version: 1, input: serde_json::to_value(&spec)?,
                }, &parent, &authority).await?;
                ensure!(result.outcome == "succeeded", "native trial failed: {result:?}");
                let value = result.result.context("native trial returned no result")?;
                let report = serde_json::from_value(value.get("report")
                    .context("native trial result has no report")?.clone())?;
                let attempt_id = value.get("attempt_id").and_then(serde_json::Value::as_str)
                    .context("native trial attempt ID missing")?.to_owned();
                let evidence = catalog.trial_evidence(&spec.unit_id, "0", 128)?;
                let connection = rusqlite::Connection::open_with_flags(
                    root.join("source-node/node.sqlite3"),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                        | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW)?;
                let data: String = connection.query_row(
                    "SELECT data FROM unit_attempts WHERE attempt_id=?1 AND state='finished'",
                    [attempt_id.as_str()], |row| row.get(0))?;
                let attempt = serde_json::from_str(&data)?;
                Ok((report, NativeTrialFixture { source_export, attempt_id, attempt, evidence }))
            }.await;
            let cleanup = supervisor.stop().await;
            match (measured, cleanup) {
                (Ok((report, native)), Ok(())) => Ok((report, Some(native))),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(error.context(
                    format!("native trial fixture cleanup also failed: {cleanup:#}"))),
            }
        }
    };
    let drained = handle.quiesce_for_handoff().await;
    broker_task.abort();
    assert!(broker_task.await.unwrap_err().is_cancelled());
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let ((report, native_trial), shutdown) = match (report, drained) {
        (Ok(report), Ok(shutdown)) => (report, shutdown),
        (Err(error), Ok(_)) => return Err(error),
        (Ok(_), Err(error)) => return Err(error.context("fixture broker drain failed")),
        (Err(error), Err(drain)) => return Err(error.context(
            format!("fixture broker drain also failed: {drain:#}"))),
    };
    std::fs::write(root.join("broker-shutdown.json"), serde_json::to_vec_pretty(&shutdown)?)?;
    std::fs::write(root.join("report.json"), serde_json::to_vec_pretty(&report)?)?;
    if let Some(native) = &native_trial {
        std::fs::write(root.join("native-trial.json"), serde_json::to_vec_pretty(native)?)?;
    }
    eprintln!("prepared cognition fixture: {}", root.display());
    Ok(PreparedFixture { root, store, source_read, source_metadata, report, native_trial })
}

fn condition_root(fixture: &PreparedFixture, condition: Condition) -> Result<PathBuf> {
    let prefix = format!("fidelity-0-{}", condition.as_str());
    for entry in std::fs::read_dir(fixture.root.join("trial"))? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(&prefix) { return Ok(entry.path()); }
    }
    bail!("condition installation missing")
}

#[tokio::test]
async fn native_preparation_preserves_written_candidate_metadata() -> Result<()> {
    let fixture = prepared_fixture(true, false, FixtureEntry::PublicRunner).await?;
    assert_written_candidate_fidelity(&fixture)
}

fn assert_written_candidate_fidelity(fixture: &PreparedFixture) -> Result<()> {
    assert_eq!(fixture.source_read["learning"]["source_verified"], true);
    let sample = fixture.report.samples.iter().find(|sample| sample.condition == Condition::Candidate)
        .context("candidate sample missing")?;
    assert_eq!(sample.verdict, "effective", "{:?}", sample.failure);
    let bytes = fixture.store.cas.read_object(&Digest(sample.receipt_digest.clone().context("sample receipt missing")?))?;
    let observation: serde_json::Value = serde_json::from_slice(&bytes)?;
    let result = observation["observation"]["consumed_results"].as_array().context("consumed results missing")?
        .iter().find(|result| result["result"]["unit"]["unit_id"] == fixture.source_read["unit_id"])
        .context("written candidate was not read by the native child")?;
    for field in ["unit_id", "revision", "artifact_digest", "body", "learning", "provenance_refs"] {
        assert_eq!(result["result"]["unit"][field], fixture.source_read[field], "native read lost {field}");
    }
    let root = condition_root(fixture, Condition::Candidate)?;
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
    let catalog = CognitionCatalog::new(node);
    let read = catalog.read(fixture.source_read["unit_id"].as_str().context("source identity missing")?,
        fixture.source_read["revision"].as_str().context("source revision missing")?)?;
    assert_eq!(serde_json::to_value(read.entry_text)?, fixture.source_read["entry_text"]);
    let actual = serde_json::to_value(catalog.units()?.into_iter()
        .find(|unit| unit.unit_id == fixture.source_metadata["unit_id"]).context("sandbox candidate missing")?)?;
    for field in ["why", "cues", "negative_cues", "applicability", "knowledge", "learning", "provenance_refs"] {
        assert_eq!(actual[field], fixture.source_metadata[field], "candidate descriptor lost {field}");
    }
    let disclosure = catalog.disclosure()?;
    for cue in ["source-fidelity-cue", "source-fidelity-negative", "source-fidelity-applicability"] {
        assert!(disclosure.contains(cue), "authored applicability was lost: {cue}");
    }
    Ok(())
}

#[tokio::test]
async fn native_trial_binds_the_executed_configuration_to_the_opened_plan() -> Result<()> {
    let fixture = prepared_fixture(true, false, FixtureEntry::NativeTrial).await?;
    assert_written_candidate_fidelity(&fixture)?;
    assert_control_cannot_read_candidate(&fixture)?;
    let native = fixture.native_trial.as_ref().context("native trial evidence missing")?;
    let rows = native.evidence["evidence"].as_array().context("trial rows missing")?;
    let finished = rows.iter().find(|row| row["attempt_id"] == native.attempt_id)
        .context("finished native trial missing")?;
    let plan = native.attempt.get("plan").filter(|plan| plan.is_object())
        .context("persisted trial plan missing")?;
    assert_eq!(plan["configuration_ref"], serde_json::to_value(&fixture.report.configuration_ref)?);
    assert_eq!(plan["artifact_digest"], fixture.source_read["artifact_digest"]);
    let plan_digest = Digest::blake3(&serde_json::to_vec(plan)?).0;
    assert_eq!(plan_digest, native.attempt["plan_digest"]);
    assert_eq!(plan_digest, finished["environment_digest"]);
    assert_eq!(native.attempt["plan_digest"], finished["evidence"]["plan_digest"]);
    let reference = &fixture.report.configuration_ref;
    let bytes = fixture.store.cas.read_object(&reference.digest)?;
    assert_eq!(bytes.len() as u64, reference.bytes);
    assert_eq!(Digest::blake3(&bytes), reference.digest);
    let configuration: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(configuration["source_export"], serde_json::to_value(&native.source_export)?);
    assert_eq!(finished["outcome"], "unknown");
    let receipts = finished["evidence"]["receipts"].as_array().context("trial receipts missing")?;
    for sample in &fixture.report.samples {
        let receipt = receipts.iter().find(|receipt| receipt["condition"] == sample.condition.as_str())
            .context("condition fact missing from native trial")?;
        let expected = sample.receipt_digest.as_deref().or_else(|| sample.installation_ref.as_ref()
            .map(|reference| reference.digest.0.as_str())).context("sample has no retained fact")?;
        assert_eq!(receipt["digest"], expected);
        assert_eq!(receipt["installation_ref"], serde_json::to_value(&sample.installation_ref)?);
    }
    Ok(())
}

async fn control_cannot_read_candidate(learned: bool) -> Result<()> {
    let fixture = prepared_fixture(learned, false, FixtureEntry::PublicRunner).await?;
    assert_control_cannot_read_candidate(&fixture)
}

fn assert_control_cannot_read_candidate(fixture: &PreparedFixture) -> Result<()> {
    let root = condition_root(fixture, Condition::Control)?;
    let installation = std::fs::read_dir(&root)?.collect::<std::io::Result<Vec<_>>>()?.into_iter()
        .map(|entry| entry.path()).find(|path| path.join("shenko.db").is_file())
        .context("control subject installation missing")?;
    let store = Store::open(installation.join("shenko.db"), installation.join("cas"))?;
    let agent = store.get_existing_agent_id()?.context("control subject missing")?;
    let decisions = store.committed_decisions_after(agent, 0, None, 64)?;
    let intent = decisions.iter().flat_map(|(_, _, decision)| &decision.delegated_task_intents)
        .find(|intent| intent.definition.as_ref().is_some_and(|definition|
            definition.input["unit_id"] == fixture.source_read["unit_id"]))
        .context("control did not attempt the known candidate read")?;
    assert_eq!(intent.definition.as_ref().context("control read definition missing")?.input["revision"],
        fixture.source_read["revision"]);
    let result = store.native_task_result(intent.intent_id)?.context("control native result missing")?;
    assert!(matches!(result.outcome, TaskOutcome::Failed { .. }), "control read reached the candidate: {result:?}");
    assert_eq!(result.result.as_ref().context("native failure observation missing")?["error"]["code"], "unit_not_readable");
    let sample = fixture.report.samples.iter().find(|sample| sample.condition == Condition::Control)
        .context("control sample missing")?;
    assert_eq!(sample.verdict, "failed");
    let failure = sample.failure.as_deref().context("control failure missing")?;
    assert!(sample.receipt_digest.is_none());
    assert!(sample.judge_receipt.is_none());
    let totals = &fixture.report.scenarios.iter().find(|scenario| scenario.name == sample.scenario)
        .context("control scenario totals missing")?.control;
    assert_eq!((totals.attempted, totals.scored, totals.failed, totals.effective), (1, 0, 1, 0));
    let task = store.external_task(result.task_id)?;
    if let Some(consuming) = decisions.iter().find(|(_, _, decision)| decision.event_dispositions.iter()
        .any(|disposition| disposition.event_id == result.result_id
            && disposition.disposition != Disposition::Deferred)) {
        assert_eq!(store.event_disposition_commit(task.agent_id, result.result_id)?, consuming.1.commit_ref);
        let reference = &consuming.2.input_plan_ref;
        assert_eq!(consuming.1.input_plan_ref.as_ref(), Some(reference));
        let bytes = store.cas.read_object(&reference.digest)?;
        assert_eq!(bytes.len() as u64, reference.bytes);
        assert_eq!(Digest::blake3(&bytes), reference.digest);
        #[derive(Deserialize)]
        struct InputEvents { events: Vec<InboxEvent> }
        let input: InputEvents = serde_json::from_slice(&bytes)?;
        let event = input.events.iter().find(|event| event.event_id == result.result_id)
            .context("consuming S1 omitted the original failed T2")?;
        assert_eq!(event.kind, InboxEventKind::TaskResult);
        assert_eq!(event.source, "native-child");
        assert_eq!(TaskResult::deserialize(&event.payload)?, result);
    } else {
        // A deadline may stop observation before cleanup finishes the read.
        // Its original failure must remain ready for a later continuation.
        assert!(failure.starts_with("sandbox deadline:"), "unconsumed control read: {failure}");
        assert_eq!(store.event_disposition_commit(task.agent_id, result.result_id)?, None);
        let connection = rusqlite::Connection::open_with_flags(installation.join("shenko.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let pending: bool = connection.query_row(
            "SELECT consumed = 0 AND kind = 'task_result' AND source = 'native-child' FROM inbox_events WHERE event_id = ?1 AND agent_id = ?2",
            rusqlite::params![result.result_id.to_string(), task.agent_id.to_string()], |row| row.get(0))?;
        assert!(pending, "the original failed T2 is not pending for delivery");
    }
    let node = Arc::new(parking_lot::Mutex::new(Node::open(&root.join("node"))?));
    let catalog = CognitionCatalog::new(node);
    assert!(catalog.versions(fixture.source_read["unit_id"].as_str()
        .context("candidate identity missing")?)?.is_empty());
    Ok(())
}

#[tokio::test]
async fn a_control_known_id_read_cannot_reach_the_seed_candidate() -> Result<()> {
    control_cannot_read_candidate(false).await
}

#[tokio::test]
async fn a_control_known_id_read_cannot_reach_the_learned_candidate() -> Result<()> {
    control_cannot_read_candidate(true).await
}

#[tokio::test]
async fn an_empty_judge_verdict_retains_the_original_broker_result() -> Result<()> {
    let fixture = prepared_fixture(false, true, FixtureEntry::PublicRunner).await?;
    let sample = fixture.report.samples.iter().find(|sample| sample.condition == Condition::Candidate)
        .context("candidate sample missing")?;
    assert_eq!(sample.verdict, "failed");
    assert!(sample.failure.as_ref().is_some_and(|failure| failure.contains("empty verdict")));
    let sample = serde_json::to_value(sample)?;
    let receipt = &sample["judge_receipt"];
    let result_ref: ObjectRef = serde_json::from_value(receipt["result_ref"].clone())
        .context("empty judge result was discarded")?;
    let bytes = fixture.store.cas.read_object(&result_ref.digest)?;
    assert_eq!(bytes.len() as u64, result_ref.bytes);
    assert_eq!(Digest::blake3(&bytes), result_ref.digest);
    let result: shenko_broker::request::InferenceResult = serde_json::from_slice(&bytes)?;
    assert_eq!(serde_json::to_value(result.request_id)?, receipt["request_id"]);
    assert_eq!(result.status, InferenceStatus::Success);
    let response = result.response.context("original judge response missing")?;
    assert_eq!(response.content, "");
    assert_eq!(response.stop_reason, "stop");
    let raw: serde_json::Value = serde_json::from_str(&response.raw_response)?;
    assert_eq!(raw["choices"][0]["message"]["content"], "");
    assert_eq!(raw["usage"]["prompt_tokens"], 37);
    let usage = response.usage.context("original judge usage missing")?;
    assert_eq!(usage.input_tokens, Some(37));
    assert_eq!(usage.output_tokens, Some(3));
    let request_ref: ObjectRef = serde_json::from_value(receipt["request_ref"].clone())?;
    let request: serde_json::Value = serde_json::from_slice(&fixture.store.cas.read_object(&request_ref.digest)?)?;
    assert_eq!(request["request_id"], receipt["request_id"]);
    assert_eq!(request["payload"]["max_tokens"], 512);
    let journal = fixture.store.content_stream_units(receipt["journal_id"].as_str().context("judge journal missing")?, 0)?;
    let terminal = journal.iter().find(|unit| unit.terminal).context("judge result was not journaled")?;
    let recorded: serde_json::Value = serde_json::from_str(&terminal.data)?;
    assert_eq!(recorded["result_ref"], receipt["result_ref"]);
    Ok(())
}

#[tokio::test]
async fn a_timed_out_judge_retains_its_original_late_result_after_drain() -> Result<()> {
    let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../build/cognition-trial-regression"));
    let root = base.join(format!("late-judge-{}", EntityId::new()));
    std::fs::create_dir_all(&root)?;
    let store = Arc::new(Store::open(root.join("measurement.db"), root.join("cas"))?);
    let catalog = CognitionCatalog::new(Arc::new(parking_lot::Mutex::new(
        Node::open(&root.join("source-node"))?)));
    let seed = shenko_awareness_core::seed_packages()?.remove(0);
    catalog.install(&seed, &root.join("source-cognition"))?;
    let spec = TrialSpec {
        unit_id: seed.unit_id, revision: seed.revision, trials_per_condition: 1, deadline_ms: 2_000,
        scenarios: vec![TrialScenario { name: "late-judge".into(),
            message: "Has the requested action actually been verified?".into(),
            rubric: "The answer distinguishes an observation from an assumption.".into() }],
    };
    let prepared = prepare_trial(&store, &catalog, &spec)?;
    let (_candidate, installation_ref) = prepare_condition(&store, &prepared,
        Condition::Candidate, &root.join("candidate"))?;
    let text = "This action has not been verified.";
    let raw_ref = store.cas.seal_object(text.as_bytes(), "text/plain", "fixture",
        "cognition-late-judge-raw-output", 1)?.object;
    let output = ParsedOutput { schema_version: 1, step_id: EntityId::new(), raw_ref,
        text: text.into(), controls: Vec::new() };
    let sample_ref = seal_trial_object(&store, &serde_json::json!({
        "scope": "synthetic parsed output for broker-lifetime proof; no subject trial",
        "output": output,
    }), "cognition-late-judge-probe")?;
    let context = SandboxJudgeContext { configuration_ref: &prepared.configuration_ref,
        installation_ref: &installation_ref, sample_digest: &sample_ref.digest.0,
        scenario: "late-judge", condition: Condition::Candidate, trial: 0 };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let marker = format!("EFFECTIVE original-late-result-{}", EntityId::new());
    let expected_response = serde_json::json!({
        "choices": [{"message": {"content": marker}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 37, "completion_tokens": 3},
    });
    let response_bytes = serde_json::to_vec(&expected_response)?;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let _request = fixture_request(&mut socket).await?;
        accepted_tx.send(()).map_err(|_| anyhow!("judge acceptance observer dropped"))?;
        release_rx.await.context("judge response gate dropped")?;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response_bytes.len()).as_bytes()).await?;
        socket.write_all(&response_bytes).await?;
        Ok::<(), anyhow::Error>(())
    });
    let (broker, handle) = Broker::with_local_provider(
        LocalModelProvider::new(endpoint, "late-judge-fixture".into()), 1)?;
    let broker_task = tokio::spawn(broker.run());
    let outer = tokio::time::Instant::now() + Duration::from_secs(10);
    let stop = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut receipt = None;
    let (accepted, judged) = tokio::join!(
        tokio::time::timeout_at(outer, accepted_rx),
        tokio::time::timeout_at(outer, judge(Arc::clone(&store), &handle,
            &spec.scenarios[0].rubric, &spec.scenarios[0].message, &output,
            &context, &mut receipt, stop)),
    );
    accepted.context("provider did not receive the original judge request")??;
    let failure = match judged.context("judge waiter did not terminate")? {
        Err(error) => error,
        Ok(_) => bail!("judge returned before the gated provider response"),
    };
    assert!(failure.to_string().contains("judge exceeded the trial deadline"));
    let identity = receipt.context("timed-out judge lost its original identity")?;
    assert!(identity.result_ref.is_none());
    let before = store.content_stream_units(&identity.journal_id, 0)?;
    assert_eq!(before.iter().map(|unit| unit.seq).collect::<Vec<_>>(), vec![0, 1]);
    assert!(before.iter().all(|unit| !unit.terminal));

    // The provider cannot finish before the original waiter has timed out.
    // Drain the same admitted request; never submit a replacement or rescore.
    release_tx.send(()).map_err(|_| anyhow!("provider response gate closed"))?;
    let shutdown = tokio::time::timeout_at(outer, handle.quiesce_for_handoff()).await
        .context("late judge did not drain within the bounded observation")??;
    assert_eq!(shutdown["local_provider_readers_drained"], true);
    assert_eq!(shutdown["remote_termination_claimed"], false);
    assert_eq!(shutdown["requests"], serde_json::json!([]));
    for domain in shutdown["domains"].as_array().context("domain snapshot missing")? {
        assert_eq!(domain["active_or_unknown_charges"], 0);
    }
    let journal = store.content_stream_units(&identity.journal_id, 0)?;
    let terminal = journal.iter().find(|unit| unit.terminal).context("late result was not journaled")?;
    assert_eq!(terminal.seq, 2);
    let recorded: serde_json::Value = serde_json::from_str(&terminal.data)?;
    assert_eq!(recorded["request_id"], serde_json::to_value(identity.request_id)?);
    assert_eq!(recorded["request_ref"], serde_json::to_value(&identity.request_ref)?);
    let reference: ObjectRef = serde_json::from_value(recorded["result_ref"].clone())?;
    let bytes = store.cas.read_object(&reference.digest)?;
    assert_eq!(bytes.len() as u64, reference.bytes);
    assert_eq!(Digest::blake3(&bytes), reference.digest);
    let result: shenko_broker::request::InferenceResult = serde_json::from_slice(&bytes)?;
    assert_eq!(result.request_id, identity.request_id);
    assert_eq!(result.status, InferenceStatus::Success);
    assert_eq!(result.termination.kind, shenko_broker::request::TerminationKind::ProviderTerminal);
    let response = result.response.context("late original response missing")?;
    assert_eq!(response.content, marker);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&response.raw_response)?, expected_response);
    let usage = response.usage.context("late original usage missing")?;
    assert_eq!(usage.input_tokens, Some(37));
    assert_eq!(usage.output_tokens, Some(3));
    tokio::time::timeout_at(outer, server).await.context("provider fixture did not finish")???;
    broker_task.abort();
    assert!(broker_task.await.unwrap_err().is_cancelled());
    std::fs::write(root.join("late-judge.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "waiter_error": format!("{failure:#}"), "waiter_receipt": identity,
        "terminal": recorded, "broker_shutdown": shutdown,
    }))?)?;
    eprintln!("late judge fixture: {}", root.display());
    Ok(())
}

/// A planned attempt slot for the bookkeeping regressions. It never touches a
/// broker: the point is exactly that the totals and the verdict are facts
/// about the plan and its slots, not about what an attempt happened to say.
fn booked_sample(condition: Condition, started: bool, verdict: &str) -> TrialSample {
    TrialSample {
        scenario: "bookkeeping".into(), condition, trial: 0, started, deadline_ms: 45_000,
        verdict: verdict.into(), judge_text: String::new(), failure: None,
        receipt_digest: None, installation_ref: None, judge_receipt: None, output_excerpt: None,
    }
}

#[test]
fn unstarted_slots_are_not_started_failures() {
    let samples = vec![
        booked_sample(Condition::Candidate, true, "effective"),
        booked_sample(Condition::Control, false, "failed"),
    ];
    let candidate = totals_of(&samples, Condition::Candidate);
    assert_eq!(
        (candidate.attempted, candidate.unstarted, candidate.scored, candidate.failed, candidate.effective),
        (1, 0, 1, 0, 1));
    let control = totals_of(&samples, Condition::Control);
    assert_eq!((control.attempted, control.unstarted, control.scored, control.failed), (1, 1, 0, 0));
    assert_eq!(control.rate_over_scored, 0.0);
    let (verdict, reason) = verdict_of(&candidate, &control);
    assert_eq!(verdict, "inconclusive");
    assert!(reason.contains("started failures: candidate 0, control 0"), "{reason}");
    assert!(reason.contains("unstarted slots: candidate 0, control 1"), "{reason}");
    // A started failure is still a failure, not an unstarted slot.
    let failed = totals_of(&[booked_sample(Condition::Control, true, "failed")], Condition::Control);
    assert_eq!((failed.attempted, failed.unstarted, failed.scored, failed.failed), (1, 0, 0, 1));
}

#[test]
fn every_planned_slot_of_a_pair_gets_the_same_window() {
    assert_eq!(sample_window(900_000, 2), Duration::from_millis(450_000));
    assert_eq!(sample_window(90_000, 4), Duration::from_millis(22_500));
    assert_eq!(sample_window(1_000, 2), Duration::from_millis(MIN_SAMPLE_WINDOW_MS));
}

#[test]
fn the_judge_verdict_is_read_from_the_judges_own_words() {
    for text in ["EFFECTIVE 理由是它先做了观察", "判定：EFFECTIVE，因为它先核实了", "EFFECTIVE"] {
        assert!(matches!(judge_verdict(text).unwrap(), JudgeVerdict::Effective(_)), "{text}");
    }
    for text in ["NOT_EFFECTIVE 理由是直接下结论", "**not_effective** 没有核实",
        "Not Effective: it asserted", "INEFFECTIVE 直接下结论没有核实",
        "判定：INEFFECTIVE，因为它直接断言"] {
        assert!(matches!(judge_verdict(text).unwrap(), JudgeVerdict::Ineffective(_)), "{text}");
    }
    // The first verdict token wins, so a prefix cannot flip a stated negative.
    assert!(matches!(judge_verdict("NOT_EFFECTIVE；它不是 EFFECTIVE 的核实").unwrap(),
        JudgeVerdict::Ineffective(_)));
    // `EFFECTIVE` inside a longer word is prose, not the verdict token.
    let prose = judge_verdict("这回答的 effectiveness 很高，它确实核实了").unwrap_err().to_string();
    assert!(prose.contains("neither EFFECTIVE nor NOT_EFFECTIVE"), "{prose}");
    assert!(matches!(judge_verdict("not_effective：直接下结论").unwrap(),
        JudgeVerdict::Ineffective(_)));
    let empty = judge_verdict("   ").unwrap_err().to_string();
    assert!(empty.contains("empty verdict"), "{empty}");
}

#[tokio::test]
async fn a_prepared_pair_starts_both_conditions_under_one_window() -> Result<()> {
    let fixture = prepared_fixture(false, false, FixtureEntry::PublicRunner).await?;
    assert_eq!(fixture.report.samples.len(), 2, "{:?}", fixture.report.samples);
    assert!(fixture.report.samples.iter().all(|sample| sample.started), "{:?}", fixture.report.samples);
    let windows: Vec<u64> = fixture.report.samples.iter().map(|sample| sample.deadline_ms).collect();
    assert!(windows.windows(2).all(|pair| pair[0] == pair[1]), "{windows:?}");
    assert_eq!(fixture.report.deadline_ms, 90_000);
    assert_eq!(fixture.report.sample_deadline_ms, 45_000);
    assert_eq!(windows, vec![45_000, 45_000]);
    for totals in fixture.report.scenarios.iter().flat_map(|scenario| [&scenario.candidate, &scenario.control]) {
        assert_eq!(totals.unstarted, 0);
        assert_eq!(totals.scored + totals.failed + totals.unstarted, totals.attempted);
    }
    let report = std::fs::read(fixture.root.join("report.json"))?;
    let report: serde_json::Value = serde_json::from_slice(&report)?;
    assert_eq!(report["sample_deadline_ms"], serde_json::json!(45_000));
    assert_eq!(report["samples"][0]["started"], serde_json::json!(true));
    Ok(())
}
