use super::*;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use shenko_compat::runtime::AdapterHost;
use shenko_compat_mcp::{McpConnection, McpPlugin, protocol::ConnectionSpec};
use shenko_compat_skill::{SkillPlugin, SkillSource, SourceInput};
use shenko_exec_host::{CognitionQueryHandler, CognitionReadHandler, CognitionWriteHandler,
    SandboxRuntime, UnresolvedSandboxObservation};
use shenko_kernel::Supervisor;
use shenko_subject::{NativeChildService, NativeTaskHandlerPlugin};
use shenko_kernel::task::NativeTaskHandler;
use shenko_types::{subject::SubjectStep, task::{NativeTaskDefinition, TaskResult}};
use std::{io::{Cursor, Write}, process::Command};

const LEDGER_SCRIPT: &[u8] = include_bytes!("../support/learning_workspace.py");
const SKILL_ENTRY: &str = "---\nname: depot-stock\ndescription: Read the source's current method and dataset to determine usable stock\n---\n\nFor stock questions, use references/method.md and data/current.json from this source. The method belongs to the source revision; declarations do not execute scripts.\n";
const METHOD_ONE: &str = "Usable stock is accepted incoming quantity minus shipped quantity minus active reserved quantity. Pending incoming quantity and cancelled reservations are excluded.\n";
const METHOD_TWO: &str = "Usable stock is accepted incoming quantity minus shipped quantity minus active reserved quantity minus quarantined quantity. Pending incoming quantity and cancelled reservations are excluded.\n";

#[derive(Clone)]
pub(super) struct WorldSpec {
    pub behavior: &'static str,
    pub initial_balance: i64,
    pub method_version: u32,
    pub method_missing: bool,
    pub dataset: Value,
}

impl WorldSpec {
    pub fn new(case: Case, repetition: usize) -> Self {
        let n = repetition as i64;
        Self { behavior: match case { Case::McpLost => "drop_after_effect", Case::McpPending => "accepted_only", _ => "applied" },
            initial_balance: 71 + n * 11,
            method_version: if case == Case::SkillChanged { 2 } else { 1 },
            method_missing: case == Case::SkillMissing,
            dataset: json!({"incoming":[{"status":"accepted","quantity":83+n*13},
                {"status":"pending","quantity":29+n*3},{"status":"accepted","quantity":17+n*2}],
                "shipped":21+n*2,"reservations":[{"status":"active","quantity":9+n},
                {"status":"cancelled","quantity":23+n*2}],"quarantined":6+n}) }
    }

    fn stock_facts(&self) -> Result<Value> {
        let method = if self.method_missing { None } else {
            Some(if self.method_version == 2 { METHOD_TWO } else { METHOD_ONE })
        };
        let quantity = |value: &Value| value["quantity"].as_i64().context("fixture quantity is not integral");
        let incoming = self.dataset["incoming"].as_array().context("fixture incoming missing")?.iter()
            .filter(|entry| entry["status"] == "accepted").try_fold(0i64, |sum, entry| Ok::<_, anyhow::Error>(sum + quantity(entry)?))?;
        let reserved = self.dataset["reservations"].as_array().context("fixture reservations missing")?.iter()
            .filter(|entry| entry["status"] == "active").try_fold(0i64, |sum, entry| Ok::<_, anyhow::Error>(sum + quantity(entry)?))?;
        let net = incoming - self.dataset["shipped"].as_i64().context("fixture shipped missing")? - reserved
            - if self.method_version == 2 { self.dataset["quarantined"].as_i64().context("fixture quarantine missing")? } else { 0 };
        Ok(json!({"dataset":self.dataset,"method_available":method.is_some(),"method":method,
            "expected_usable_stock":method.map(|_| net),"scripts_executed_by_adapter":false}))
    }
}

pub(super) struct Fixture {
    pub script: PathBuf,
    pub binding: PathBuf,
    pub script_ref: ObjectRef,
    pub profile: UnitExport,
    pub unit: UnitRef,
}

impl Fixture {
    pub fn new(store: &Store, root: &Path) -> Result<Self> {
        let script = root.join("ledger-fixture.py");
        std::fs::OpenOptions::new().write(true).create_new(true).open(&script)?.write_all(LEDGER_SCRIPT)?;
        let binding = root.join("ledger-binding.json");
        let script_ref = store.cas.seal_object(LEDGER_SCRIPT, "text/x-python", "ecosystem-compat", "learning-fixture-source", 1)?.object;
        let profile = McpConnection { description: "Owned local ledger: discover its real tools and report their observations".into(),
            provenance_refs: vec![format!("synthetic:ledger-fixture:{}", script_ref.digest.0)],
            connection: ConnectionSpec::Stdio { program: "/usr/bin/python3".into(),
                args: vec!["-I".into(), script.display().to_string(), binding.display().to_string()],
                cwd: Some(root.to_path_buf()), env_refs: BTreeMap::new() } }.export("mcp.learning-ledger", true)?;
        let unit = export_identity(&profile)?;
        Ok(Self { script, binding, script_ref, profile, unit })
    }

    fn verify_script(&self) -> Result<()> {
        ensure!(std::fs::read(&self.script)? == LEDGER_SCRIPT, "fixed fixture executable changed during measurement");
        Ok(())
    }

    fn action(&self, mode: &str) -> Result<Vec<u8>> {
        self.verify_script()?;
        let result = Command::new("/usr/bin/python3").arg("-I").arg(&self.script).arg(&self.binding).arg(mode).output()?;
        ensure!(result.status.success(), "fixture {mode} failed: {}", String::from_utf8_lossy(&result.stderr));
        Ok(result.stdout)
    }
}

pub(super) struct FrozenSkill {
    pub export: UnitExport,
    pub unit: UnitRef,
    pub archive: ObjectRef,
    pub archive_bytes: Vec<u8>,
    pub spec: WorldSpec,
}

impl FrozenSkill {
    pub fn capture(store: &Store, spec: WorldSpec) -> Result<Self> {
        let mut files = BTreeMap::from([
            ("SKILL.md".to_owned(), SKILL_ENTRY.as_bytes().to_vec()),
            ("data/current.json".to_owned(), serde_json::to_vec(&spec.dataset)?),
        ]);
        if !spec.method_missing {
            files.insert("references/method.md".into(), if spec.method_version == 2 { METHOD_TWO } else { METHOD_ONE }.as_bytes().to_vec());
        }
        let inputs = files.iter().map(|(path, bytes)| Ok(SourceInput { path: path.clone(), executable: false,
            bytes: bytes.len() as u64, reader: Cursor::new(bytes.as_slice()) }));
        let source = SkillSource::capture(&store.cas, vec!["synthetic:depot-stock-source".into()], inputs)?;
        let export = source.export("skill.depot-stock", true)?;
        let unit = export_identity(&export)?;
        let archive_bytes = lineage::object_bytes(store, &source.archive)?;
        Ok(Self { export, unit, archive: source.archive, archive_bytes, spec })
    }
}

pub(super) fn export_identity(export: &UnitExport) -> Result<UnitRef> {
    Ok(UnitRef { unit_id: export.root_unit_id.clone(), revision: export.root_revision.clone().context("export revision missing")?,
        artifact_digest: export.root_artifact_digest.clone().context("export artifact missing")? })
}

fn without_unit(source: &UnitExport, excluded: &str) -> Option<UnitExport> {
    let mut export = source.clone();
    export.packages.retain(|package| package.unit.unit_id != excluded);
    if export.root_unit_id == excluded {
        let root = export.packages.first()?;
        export.root_unit_id = root.unit.unit_id.clone();
        export.root_revision = Some(root.unit.revision.clone());
        export.root_artifact_digest = Some(root.unit.artifact_digest.clone());
        export.enabled = false;
    }
    (!export.packages.is_empty()).then_some(export)
}

pub(super) struct World {
    pub prepared: PreparedSandbox,
    pub store: Arc<Store>,
    pub node: Arc<Mutex<Node>>,
    pub catalog: Arc<CognitionCatalog>,
    pub installation_ref: ObjectRef,
    pub runtime: Runtime,
    pub selectors: String,
}

pub(super) fn prepare(measurement: &Store, root: &Path, baseline: &[UnitExport], fixture: Arc<Fixture>,
    skill: &FrozenSkill, candidate: Option<&lineage::Candidate>, condition: Option<Condition>, acquisition: bool) -> Result<World> {
    let prepared = PreparedSandbox::fresh("compat-learning", root)?;
    let installation = prepared.installation().to_path_buf();
    let store = Arc::clone(prepared.store());
    let archive = store.cas.seal_object(&skill.archive_bytes, "application/octet-stream", "ecosystem-compat", "skill-source-bytes", 2)?.object;
    ensure!(archive.digest == skill.archive.digest && archive.bytes == skill.archive.bytes, "source archive copy changed bytes");
    let node = Arc::new(Mutex::new(Node::open(&installation.join("node"))?));
    let mut outcomes = Vec::new();
    {
        let mut authority = node.lock();
        let package_root = installation.join("units").display().to_string();
        let mut root_states = BTreeMap::new();
        for export in baseline.iter().chain([&fixture.profile, &skill.export]) {
            let export = match candidate.filter(|_| condition == Some(Condition::Control)) {
                Some(candidate) => without_unit(export, &candidate.identity.unit_id),
                None => Some(export.clone()),
            };
            if let Some(export) = export {
                let enable = export.enabled;
                root_states.insert(export.root_unit_id.clone(), (export_identity(&export)?, enable));
                outcomes.push(authority.handle_unit_request(UnitRequest::Import { export,
                    package_root: package_root.clone(), enable: Some(enable) })?);
            }
        }
        if let Some(candidate) = candidate {
            // Import all other dependency members in both conditions first.
            // Preserve an already installed noncandidate root's enabled state.
            if let Some(dependencies) = without_unit(&candidate.export, &candidate.identity.unit_id) {
                let current = authority.installed_unit_fact(&dependencies.root_unit_id)?;
                let enable = current.as_ref().is_some_and(|fact| fact.enabled);
                let identity = match current {
                    Some(fact) => UnitRef { unit_id: fact.unit_id, revision: fact.revision, artifact_digest: fact.artifact_digest },
                    None => export_identity(&dependencies)?,
                };
                root_states.entry(dependencies.root_unit_id.clone()).or_insert((identity, enable));
                outcomes.push(authority.handle_unit_request(UnitRequest::Import { export: dependencies,
                    package_root: package_root.clone(), enable: Some(enable) })?);
            }
            if condition == Some(Condition::Candidate) {
                root_states.insert(candidate.identity.unit_id.clone(), (candidate.identity.clone(), true));
                outcomes.push(authority.handle_unit_request(UnitRequest::Import { export: candidate.export.clone(),
                    package_root, enable: Some(true) })?);
            } else {
                ensure!(authority.installed_unit_fact(&candidate.identity.unit_id)?.is_none(),
                    "candidate identity is present in the control installation");
                ensure!(authority.retained_unit_facts(&candidate.identity.unit_id)?.is_empty(),
                    "an older candidate version survived control preparation");
            }
        }
        // Reimports retain the previous enable state, including a root first
        // encountered as another export's disabled dependency.
        for (unit_id, (identity, enabled)) in root_states {
            let current = authority.installed_unit_fact(&unit_id)?.context("captured root disappeared during import")?;
            ensure!(current.revision == identity.revision && current.artifact_digest == identity.artifact_digest,
                "overlapping imports changed a captured root's current identity");
            outcomes.push(authority.handle_unit_request(UnitRequest::SetEnabled { unit_id: unit_id.clone(), enabled })?);
            ensure!(authority.installed_unit_fact(&unit_id)?.is_some_and(|fact| fact.enabled == enabled),
                "captured root enable state was not restored");
        }
    }
    let binding = serde_json::to_vec(&json!({"workspace":installation.join("business"),
        "store_db":installation.join("shenko.db"),"store_cas":installation.join("cas"),
        "agent_id":prepared.agent_id(),"unit":fixture.unit,"behavior":skill.spec.behavior,
        "initial_balance":skill.spec.initial_balance}))?;
    // The caller reaches this only after the preceding owned runtime joined.
    std::fs::write(&fixture.binding, &binding)?;
    std::fs::write(installation.join("fixture-binding.json"), &binding)?;
    fixture.action("--initialize")?;
    let binding_ref = measurement.cas.seal_object(&binding, "application/json", "ecosystem-compat", "learning-fixture-binding", 1)?.object;
    let installation_ref = shenko_compat::recorded::seal(measurement, &json!({"schema_version":1,
        "installation":installation,"condition":condition,"acquisition":acquisition,
        "script_ref":fixture.script_ref,"binding_ref":binding_ref,"fixture_profile":fixture.profile,
        "skill_export":skill.export,"skill_archive_ref":archive,"import_outcomes":outcomes}), "compat-learning-installation")?;
    let selectors = format!("当前真实连接 unit={}；当前 Skill source unit={}。通过已安装的 native delegate 查询它们。",
        serde_json::to_string(&fixture.unit)?, serde_json::to_string(&skill.unit)?);
    let catalog = Arc::new(CognitionCatalog::new(Arc::clone(&node)));
    let runtime = Runtime { node: Arc::clone(&node), fixture, binding, spec: skill.spec.clone(), acquisition };
    Ok(World { prepared, store, node, catalog, installation_ref, runtime, selectors })
}

pub(super) struct Runtime {
    node: Arc<Mutex<Node>>,
    fixture: Arc<Fixture>,
    binding: Vec<u8>,
    spec: WorldSpec,
    acquisition: bool,
}

impl Runtime {
    pub fn after_pending(&self, root: &Path) -> Result<Option<Value>> {
        if self.spec.behavior != "accepted_only" { return Ok(None); }
        self.verify_binding()?;
        self.fixture.action("--apply-pending")?;
        let bytes = self.fixture.action("--snapshot")?;
        std::fs::write(root.join("late-application-observation.json"), &bytes)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    fn verify_binding(&self) -> Result<()> {
        self.fixture.verify_script()?;
        ensure!(std::fs::read(&self.fixture.binding)? == self.binding, "fixture binding changed before runtime retirement");
        Ok(())
    }

    fn snapshot(&self, sandbox: &PreparedSandbox, output: &CommittedTrialOutput) -> Result<Value> {
        self.verify_binding()?;
        let bytes = self.fixture.action("--snapshot")?;
        std::fs::write(sandbox.installation().join("world-observation-at-output.json"), &bytes)?;
        let snapshot: Value = serde_json::from_slice(&bytes)?;
        let exchanges = snapshot["exchanges"].as_array().context("fixture exchanges missing")?;
        let binding_sha = format!("{:x}", Sha256::digest(&self.binding));
        let mut requests = Vec::new();
        for exchange in exchanges {
            ensure!(exchange["binding_sha256"] == binding_sha, "fixture exchange used a different binding");
            if exchange["phase"] != "request" { continue; }
            let producers = exchange["producers"].as_array().context("fixture producer set missing")?;
            ensure!(producers.len() == 1, "fixture request has {} possible armed producers; original association is ambiguous", producers.len());
            let producer = &producers[0];
            let task_id = EntityId::deserialize(&producer["task_id"])?;
            let operation_id = EntityId::deserialize(&producer["operation_id"])?;
            let attempt_id = EntityId::deserialize(&producer["attempt_id"])?;
            let operation = sandbox.store().external_operation(operation_id)?;
            let attempt = sandbox.store().external_operation_attempt(operation_id)?;
            let task = sandbox.store().external_task(task_id)?;
            let reference = ObjectRef::deserialize(&producer["input_ref"])?;
            let input_bytes = lineage::object_bytes(sandbox.store(), &reference)?;
            let input: Value = serde_json::from_slice(&input_bytes)?;
            // Independently reproduce the pinned SDK's serialization and its
            // generated progress token; all other authored metadata survives.
            use rmcp::model::{ClientRequest, GetMeta, ProgressToken};
            let mut projected: ClientRequest = serde_json::from_value(input["request"].clone())?;
            let token: ProgressToken = serde_json::from_value(exchange.pointer("/params/_meta/progressToken")
                .context("fixture request has no SDK progress token")?.clone())?;
            projected.get_meta_mut().set_progress_token(token);
            let projected = serde_json::to_value(projected)?;
            ensure!(operation.task_id == task_id && operation.operation_name == "compat.mcp.request"
                && operation.final_input_ref == reference && operation.input_digest == reference.digest
                && attempt.attempt_id == attempt_id && attempt.operation_id == operation_id
                && attempt.armed_commit == operation.prepared_commit && task.agent_id == sandbox.agent_id()
                && task.kind == "compat.mcp.request" && task.definition_version == 1
                && producer["input_sha256"] == format!("{:x}", Sha256::digest(&input_bytes))
                && input["unit"] == serde_json::to_value(&self.fixture.unit)?
                && projected["method"] == exchange["method"]
                && projected["params"] == exchange["params"],
                "fixture exchange is not bound to its actual native T1/attempt/request");
            requests.push(exchange);
        }
        let mut commits = BTreeMap::new();
        let mut tickets = BTreeSet::new();
        let mut committed_requests = BTreeSet::new();
        for exchange in exchanges.iter().filter(|exchange| exchange["phase"] == "business_commit") {
            let matching: Vec<_> = requests.iter().filter(|request| request["session_id"] == exchange["session_id"]
                && request["wire_request_id"] == exchange["wire_request_id"]).collect();
            ensure!(matching.len() == 1 && matching[0]["method"] == "tools/call"
                && matching[0]["params"]["name"] == "ledger.adjust"
                && matching[0]["params"]["arguments"]["reference"] == exchange["reference"]
                && matching[0]["params"]["arguments"]["delta"] == exchange["delta"],
                "business effect lost its original native producer and wire request");
            let reference = exchange["reference"].as_str().context("business reference is missing")?;
            let ticket = exchange["ticket"].as_str().context("business ticket is missing")?;
            let delta = exchange["delta"].as_i64().context("business delta is not integral")?;
            let expected_status = if self.spec.behavior == "accepted_only" { "accepted" } else { "applied" };
            ensure!(exchange["status"] == expected_status && !reference.is_empty() && !ticket.is_empty()
                && i32::try_from(delta).is_ok(), "business commit has an invalid ticket, delta or status");
            ensure!(committed_requests.insert((exchange["session_id"].as_str().context("business session missing")?,
                    serde_json::to_string(&exchange["wire_request_id"])?)),
                "one authenticated request produced multiple business commits");
            ensure!(tickets.insert(ticket) && commits.insert(reference, exchange).is_none(),
                "business log repeats a submission ticket or reference");
        }
        let submissions = snapshot["submissions"].as_array().context("ledger submissions missing")?;
        ensure!(submissions.len() == commits.len(), "ledger and authenticated business log have different membership");
        let mut seen_references = BTreeSet::new();
        let mut expected_balance = self.spec.initial_balance;
        for submission in submissions {
            let reference = submission["reference"].as_str().context("ledger reference missing")?;
            ensure!(seen_references.insert(reference), "ledger repeats a submission reference");
            let commit = commits.get(reference).context("ledger effect has no authenticated business commit")?;
            ensure!(submission["ticket"] == commit["ticket"] && submission["delta"] == commit["delta"]
                && submission["status"] == commit["status"], "ledger submission differs from its authenticated commit");
            if submission["status"] == "applied" {
                expected_balance = expected_balance.checked_add(submission["delta"].as_i64()
                    .context("ledger delta is not integral")?).context("ledger balance overflows")?;
            }
        }
        ensure!(snapshot["balance"].as_i64() == Some(expected_balance),
            "ledger balance does not equal the initial balance plus authenticated applied deltas");
        let sources = lineage::consumed_sources(sandbox.store(), output)?;
        std::fs::write(sandbox.installation().join("consumed-compatibility-sources.json"), serde_json::to_vec_pretty(&sources)?)?;
        let observed = sources.iter().map(|source| {
            let observation = &source.normalized["observation"];
            let protocol = &observation["protocol"];
            let mcp = source.task.kind == "compat.mcp.request";
            json!({"task_kind":source.task.kind,
                "operation_state":source.operation.state,"operation_outcome":source.operation.outcome,
                "effect_status":source.operation.effect_status,"method":source.frozen_input["request"]["method"],
                "params":source.frozen_input["request"]["params"],"resource_path":source.frozen_input["query"]["path"],
                "protocol_status":protocol["status"],"protocol_result":protocol["result"],
                "protocol_error":protocol["error"],"protocol_reason":protocol["reason"],
                "observation_status":if mcp { &protocol["status"] } else { &observation["status"] },
                "source_error":if mcp { &Value::Null } else { &observation["error"] },
                "source_reason":if mcp { &Value::Null } else { &observation["reason"] }})
        }).collect::<Vec<_>>();
        Ok(json!({"schema_version":1,"observed_ns":snapshot["observed_ns"],
            "ledger":{"balance":snapshot["balance"],"submissions":snapshot["submissions"]},
            "stock":self.spec.stock_facts()?,"consumed_protocol_observations":observed}))
    }
}

impl SandboxRuntime for Runtime {
    fn install(&self, supervisor: &Supervisor, sandbox: &PreparedSandbox, catalog: &Arc<CognitionCatalog>)
        -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            // Both installations may be prepared before the trial is opened.
            // The caller serializes runtimes and stops after any cleanup error.
            self.fixture.verify_script()?;
            std::fs::write(&self.fixture.binding, &self.binding)?;
            self.verify_binding()?;
            supervisor.spawn(McpPlugin, AdapterHost::new(supervisor, Arc::clone(sandbox.store()), Arc::clone(&self.node))).await?;
            supervisor.spawn(SkillPlugin, AdapterHost::new(supervisor, Arc::clone(sandbox.store()), Arc::clone(&self.node))).await?;
            let mut handlers: Vec<Arc<dyn NativeTaskHandler>> = vec![
                Arc::new(CognitionReadHandler::new(Arc::clone(sandbox.store()), Arc::clone(catalog))),
                Arc::new(CognitionQueryHandler::list(Arc::clone(sandbox.store()), Arc::clone(catalog))),
                Arc::new(CognitionQueryHandler::evidence(Arc::clone(sandbox.store()), Arc::clone(catalog))),
            ];
            if self.acquisition {
                handlers.push(Arc::new(CognitionWriteHandler::new(Arc::clone(sandbox.store()), Arc::clone(catalog),
                    sandbox.installation().join("authored-units"), None)));
            }
            for handler in handlers { supervisor.spawn(NativeTaskHandlerPlugin, handler).await?; }
            let native = supervisor.context().try_service::<NativeChildService>()?;
            ensure!(native.has_handler("compat.mcp.request", 1) && native.has_handler("compat.skill.source", 2),
                "compatibility runtime was not published before capability snapshot");
            Ok(())
        }
    }

    fn accept_result(&self, store: &Store, definition: &NativeTaskDefinition, result: &TaskResult) -> Result<()> {
        if result.outcome == TaskOutcome::Succeeded { return Ok(()); }
        ensure!(matches!(result.outcome, TaskOutcome::Failed { .. })
            && ((definition.kind == "compat.mcp.request" && definition.definition_version == 1)
                || (definition.kind == "compat.skill.source" && definition.definition_version == 2)),
            "nonprotocol delegated failure is not a business observation");
        lineage::from_result(store, result)?;
        Ok(())
    }

    fn accept_unresolved(&self, definition: &NativeTaskDefinition, observation: UnresolvedSandboxObservation<'_>) -> Result<()> {
        ensure!(definition.kind == "compat.mcp.request" && definition.definition_version == 1,
            "unrecognized unresolved observation provider");
        let normalized = observation.normalized.context("unknown MCP E1 has no normalized original")?;
        let source = ObjectRef::deserialize(&normalized["source_receipt"])?;
        ensure!(source == observation.receipt.raw_ref, "unknown MCP source no longer identifies original E1");
        Ok(())
    }

    fn capture(&self, sandbox: &PreparedSandbox, _step: &SubjectStep, output: &CommittedTrialOutput)
        -> impl std::future::Future<Output = Result<Option<ObjectRef>>> + Send {
        async move {
            // This synchronous snapshot does not yield the current-thread
            // subject runtime into another decision before the observation.
            let facts = self.snapshot(sandbox, output)?;
            Ok(Some(shenko_compat::recorded::seal(sandbox.store(), &facts, "compat-learning-world-facts")?))
        }
    }
}
