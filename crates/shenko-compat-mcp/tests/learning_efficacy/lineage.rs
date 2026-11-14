use super::*;
use serde::Deserialize;
use shenko_awareness_core::CognitionMetadata;
use shenko_types::{evidence::{DecodeStatus, EvidenceReceipt}, operation::{AttemptRecord, OperationRecord},
    subject::Disposition, task::{TaskRecord, TaskResult}};

#[derive(Serialize)]
pub(super) struct ObservedSource {
    pub event_id: EntityId,
    pub task: TaskRecord,
    pub operation: OperationRecord,
    pub attempt: AttemptRecord,
    pub receipt: EvidenceReceipt,
    pub normalized: Value,
    pub frozen_input: Value,
}

pub(super) fn object_bytes(store: &Store, reference: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store.cas.read_object(&reference.digest)?;
    ensure!(bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "retained object bytes differ from their full reference");
    Ok(bytes)
}

pub(super) fn object_json(store: &Store, reference: &ObjectRef) -> Result<Value> {
    Ok(serde_json::from_slice(&object_bytes(store, reference)?)?)
}

fn original_source(store: &Store, event_id: EntityId, task_id: EntityId,
    operation_id: EntityId, receipt_id: EntityId) -> Result<ObservedSource> {
    let task = store.external_task(task_id)?;
    let operation = store.external_operation(operation_id)?;
    let attempt = store.external_operation_attempt(operation_id)?;
    let receipt = store.external_evidence(receipt_id)?;
    ensure!(operation.task_id == task.task_id && operation.operation_id == operation_id
        && operation.operation_name == task.kind && operation.input_digest == operation.final_input_ref.digest
        && attempt.operation_id == operation_id && attempt.armed_commit == operation.prepared_commit
        && attempt.final_receipt_id == Some(receipt_id) && receipt.operation_id == Some(operation_id)
        && receipt.attempt_id == Some(attempt.attempt_id) && receipt.decode_status == DecodeStatus::Decoded,
        "compatibility observation lost original T1/attempt/E1 ownership");
    let normalized = object_json(store, receipt.normalized_ref.as_ref().context("original E1 has no normalized bytes")?)?;
    let raw = ObjectRef::deserialize(&normalized["source_receipt"])?;
    ensure!(raw == receipt.raw_ref
        && normalized["learning_source"]["kind"] == "cas_object"
        && normalized["learning_source"]["reference"] == format!("operation:{operation_id}")
        && normalized["learning_source"]["digest"].as_str() == Some(receipt.raw_ref.digest.0.as_str()),
        "learning source does not identify its original E1 bytes");
    object_bytes(store, &receipt.raw_ref)?;
    let frozen_input = object_json(store, &operation.final_input_ref)?;
    Ok(ObservedSource { event_id, task, operation, attempt, receipt, normalized, frozen_input })
}

pub(super) fn from_result(store: &Store, result: &TaskResult) -> Result<ObservedSource> {
    ensure!(store.native_task_result(result.task_id)?.as_ref() == Some(result),
        "consumed result differs from durable T2");
    let observed = original_source(store, result.result_id, result.task_id, result.operation_id, result.receipt_id)?;
    ensure!(result.invocation_id == observed.attempt.invocation_id
        && result.evidence_ref == observed.receipt.raw_ref
        && result.result.as_ref() == Some(&observed.normalized), "T2 differs from its original normalized E1");
    Ok(observed)
}

pub(super) fn consumed_sources(store: &Store, output: &CommittedTrialOutput) -> Result<Vec<ObservedSource>> {
    let mut sources = Vec::new();
    for result in &output.consumed_results {
        let task = store.external_task(result.task_id)?;
        if (task.kind == "compat.mcp.request" && task.definition_version == 1)
            || (task.kind == "compat.skill.source" && task.definition_version == 2) {
            sources.push(from_result(store, result)?);
        }
    }
    for observation in &output.consumed_observations {
        let source = original_source(store, observation.receipt_id, observation.task_id,
            observation.operation_id, observation.receipt_id)?;
        ensure!(source.task.kind == "compat.mcp.request" && source.task.definition_version == 1
            && source.attempt.attempt_id == observation.attempt_id
            && source.receipt.raw_ref == observation.raw_ref
            && source.receipt.normalized_ref == observation.normalized_ref
            && source.operation.state == shenko_types::operation::OperationState::Reconciling
            && source.operation.outcome.is_none()
            && source.operation.effect_status == shenko_types::operation::EffectStatus::Unknown
            && store.native_task_result(source.task.task_id)?.is_none(),
            "original unresolved E1 was changed or settled");
        sources.push(source);
    }
    Ok(sources)
}

fn consumption_and_write_steps(store: &Store, output: &CommittedTrialOutput,
    event_id: EntityId, creator: EntityId) -> Result<(u64, u64)> {
    let agent = store.get_existing_agent_id()?.context("trial subject missing")?;
    let mut cursor = (0, None);
    let mut source_step = None;
    let mut writer_step = None;
    let mut reached_output = false;
    loop {
        let decisions = store.committed_decisions_after(agent, cursor.0, cursor.1, 64)?;
        if decisions.is_empty() { break; }
        for (seq, step, decision) in decisions {
            cursor = (seq, Some(step.step_id));
            if source_step.is_none() && decision.event_dispositions.iter().any(|entry|
                entry.event_id == event_id && entry.disposition != Disposition::Deferred) {
                source_step = Some(step.step_no);
            }
            if step.step_id == creator { writer_step = Some(step.step_no); }
            if step.step_id == output.step_id { reached_output = true; break; }
        }
        if reached_output { break; }
    }
    let source_step = source_step.context("learning source lacks committed consumption")?;
    let writer_step = writer_step.context("write was not created by this committed subject history")?;
    ensure!(reached_output, "selected acquisition output is absent from committed history");
    Ok((source_step, writer_step))
}

struct ReadCoverage {
    digest: Digest,
    bytes: u64,
    parts: Vec<(u64, Vec<u8>)>,
}

impl ReadCoverage {
    fn complete(mut self) -> Result<bool> {
        self.parts.sort_by_key(|(offset, _)| *offset);
        let mut bytes = Vec::with_capacity(usize::try_from(self.bytes)?);
        for (offset, part) in self.parts {
            ensure!(offset <= self.bytes && part.len() as u64 <= self.bytes - offset,
                "consumed Skill range exceeds the declared source member");
            let offset = usize::try_from(offset)?;
            if offset > bytes.len() { return Ok(false); }
            let overlap = (bytes.len() - offset).min(part.len());
            ensure!(bytes[offset..offset + overlap] == part[..overlap], "consumed Skill ranges contradict each other");
            bytes.extend_from_slice(&part[overlap..]);
        }
        if bytes.len() as u64 != self.bytes { return Ok(false); }
        ensure!(Digest::blake3(&bytes) == self.digest, "consumed Skill member bytes do not match their digest");
        Ok(true)
    }
}

#[derive(Serialize)]
pub(super) struct Candidate {
    pub identity: UnitRef,
    pub export: UnitExport,
    pub source_proof_ref: ObjectRef,
}

pub(super) fn freeze_candidate(store: &Store, node: &Arc<Mutex<Node>>, output: &CommittedTrialOutput,
    family: Family, baseline_ids: &BTreeSet<String>) -> Result<Candidate> {
    let mut selected = None;
    for result in &output.consumed_results {
        let task = store.external_task(result.task_id)?;
        if task.kind == "cognition.unit.write" && task.definition_version == 1
            && result.outcome == TaskOutcome::Succeeded {
            selected = Some((task, result));
            break;
        }
    }
    let (write_task, result) = selected.context("acquisition produced no consumed successful model-authored write")?;
    let value = result.result.as_ref().context("successful writer has no result")?;
    ensure!(value["schema_version"] == 1 && value["status"] == "ok" && value["op"] == "write"
        && value["judgement"].is_null(), "first successful write did not remain an unadopted candidate");
    let identity = UnitRef { unit_id: value["unit_id"].as_str().context("written id missing")?.into(),
        revision: value["revision"].as_str().context("written revision missing")?.into(),
        artifact_digest: serde_json::from_value(value["artifact_digest"].clone())? };
    ensure!(!baseline_ids.contains(&identity.unit_id), "acquisition replaced an existing baseline identity");
    let (fact, export) = {
        let mut authority = node.lock();
        let fact = authority.installed_unit_fact(&identity.unit_id)?.context("written unit is not current")?;
        ensure!(fact.revision == identity.revision && fact.artifact_digest == identity.artifact_digest
            && !fact.enabled && matches!(fact.descriptor.kind, shenko_node::UnitKind::Awareness | shenko_node::UnitKind::Perception)
            && fact.descriptor.config.pointer("/cognition/adoption").is_none_or(Value::is_null),
            "first successful write no longer names an exact disabled awareness/perception candidate");
        let mut response = authority.handle_unit_request(UnitRequest::Export { unit_id: identity.unit_id.clone() })?;
        let export: UnitExport = serde_json::from_value(response.get_mut("export").context("candidate export missing")?.take())?;
        (fact, export)
    };
    ensure!(export.root_unit_id == identity.unit_id && export.root_revision.as_deref() == Some(identity.revision.as_str())
        && export.root_artifact_digest.as_ref() == Some(&identity.artifact_digest) && !export.enabled,
        "export changed the first successful write identity");
    let root = export.packages.iter().find(|package| package.unit.unit_id == identity.unit_id
        && package.unit.revision == identity.revision && package.unit.artifact_digest == identity.artifact_digest)
        .context("candidate root is absent from export")?;
    ensure!(Digest::blake3(&shenko_node::b64_decode(&root.artifact_base64)?) == identity.artifact_digest,
        "candidate bytes changed during export");
    let metadata: CognitionMetadata = serde_json::from_value(root.unit.config["cognition"].clone())?;
    let learning = metadata.learning.context("model-authored candidate has no learning record")?;
    ensure!(learning.source_kind == "cas_object" && learning.source_verified && learning.state == "candidate",
        "model-authored learning source remains unverified");
    let sources = consumed_sources(store, output)?;
    let mut matching = sources.iter().filter(|source| source.task.kind == family.task_kind()
        && learning.source_ref == format!("operation:{}", source.operation.operation_id)
        && learning.source_digest.as_deref() == Some(source.receipt.raw_ref.digest.0.as_str()));
    let source = matching.next().context("write did not derive from an actually consumed compatibility observation")?;
    ensure!(matching.next().is_none(), "learning source has ambiguous consumed producers");
    let creator = write_task.created_by_step_id.context("write has no subject creator")?;
    let (source_step, writer_step) = consumption_and_write_steps(store, output, source.event_id, creator)?;
    ensure!(source_step <= writer_step, "write preceded consumption of the claimed learning source");
    let operation = store.external_operation(result.operation_id)?;
    ensure!(operation.task_id == write_task.task_id && operation.operation_name == "cognition.unit.write.v1",
        "write result lost its original operation");
    let frozen_write = object_json(store, &operation.final_input_ref)?;
    ensure!(frozen_write["derived_from"]["kind"] == learning.source_kind
        && frozen_write["derived_from"]["reference"] == learning.source_ref
        && frozen_write["derived_from"]["digest"].as_str() == learning.source_digest.as_deref(),
        "writer input and installed learning lineage differ");
    let mut qualifying_mcp_success = false;
    let mut qualifying_mcp_failure = false;
    let mut skill_reads: BTreeMap<String, ReadCoverage> = BTreeMap::new();
    for observed in &sources {
        if observed.task.kind != family.task_kind() { continue; }
        let (consumed_at, created_at) = consumption_and_write_steps(store, output, observed.event_id, creator)?;
        if consumed_at > created_at { continue; }
        match family {
            Family::Mcp if observed.frozen_input["request"]["method"] == "tools/call"
                && observed.frozen_input["request"]["params"]["name"] == "ledger.adjust" => {
                qualifying_mcp_success |= observed.operation.outcome == Some(shenko_types::operation::OperationOutcome::Succeeded);
                qualifying_mcp_failure |= observed.operation.outcome == Some(shenko_types::operation::OperationOutcome::Failed);
            }
            Family::Skill if observed.operation.outcome == Some(shenko_types::operation::OperationOutcome::Succeeded)
                && observed.frozen_input["unit"] == source.frozen_input["unit"] => {
                if let Some(path) = observed.frozen_input["query"]["path"].as_str() {
                    let content = &observed.normalized["observation"]["content"];
                    ensure!(observed.normalized["observation"]["status"] == "received"
                        && content["path"] == path, "successful Skill read lost its source member");
                    let digest: Digest = serde_json::from_value(content["file_digest"].clone())?;
                    let bytes = content["total_bytes"].as_u64().context("Skill member byte length missing")?;
                    let offset = content["offset"].as_u64().context("Skill range byte offset missing")?;
                    let part = shenko_node::b64_decode(content["content_base64"].as_str().context("Skill range content missing")?)?;
                    let coverage = skill_reads.entry(path.into()).or_insert_with(|| ReadCoverage {
                        digest: digest.clone(), bytes, parts: Vec::new(),
                    });
                    ensure!(coverage.digest == digest && coverage.bytes == bytes, "Skill member identity changed during acquisition");
                    coverage.parts.push((offset, part));
                }
            }
            _ => {}
        }
    }
    let mut skill_paths = BTreeSet::new();
    for (path, coverage) in skill_reads {
        if coverage.complete()? { skill_paths.insert(path); }
    }
    match family {
        Family::Mcp => ensure!(qualifying_mcp_success && qualifying_mcp_failure,
            "acquisition did not consume both real business success and business failure before learning"),
        Family::Skill => ensure!(["SKILL.md", "references/method.md", "data/current.json"].iter()
            .all(|path| skill_paths.contains(*path)), "acquisition did not consume the actual Skill entry, method and development data before learning"),
    }
    let proof = json!({"schema_version":1,"written_identity":identity,"written_fact":fact,
        "write_task":write_task,"write_result":result,"write_operation":operation,"frozen_write":frozen_write,
        "source":source,"source_consumed_step":source_step,"write_created_step":writer_step,
        "scope":"identity and consumed-source lineage; semantic usefulness remains unmeasured"});
    let source_proof_ref = shenko_compat::recorded::seal(store, &proof, "compat-learning-acquisition-lineage")?;
    Ok(Candidate { identity, export, source_proof_ref })
}
