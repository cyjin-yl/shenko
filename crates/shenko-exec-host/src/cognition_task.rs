//! Native children for installable cognition units — SPEC §45.1–45.7.
//!
//! `cognition.unit.read:1` reads one unit's content at one exact version.
//! `cognition.unit.write:1` writes a unit down from what the subject actually
//! observed and, when the subject states it, records how it judged that
//! version.
//!
//! The kernel stores what the subject decided and which facts it cited. It
//! never decides for itself which unit is relevant to a message, and it never
//! concludes from a unit's text that the unit is good: the judgement and its
//! evidence come from the subject, and the source reference of a learned unit
//! keeps its own "verified" or "claimed" state.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::{
    CognitionCatalog, CognitionPackage, CognitionUnit, LearningRecord, SupersedesRef,
};
use shenko_node::{AdoptionDecision, AdoptionEvidence, AdoptionMethod, UnitKind};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::path::PathBuf;
use std::sync::Arc;

const READ_CONTRACT: &[u8] =
    b"shenko.cognition.unit.read/v1: bounded read of one installed unit version; no external effects";
const WRITE_CONTRACT: &[u8] =
    b"shenko.cognition.unit.write/v1: installs one cognition unit version into the local installation authority";
const MAX_INPUT_BYTES: usize = 32 * 1024;
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_LIST: usize = 32;

/// One unit version the subject wants to read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    unit_id: String,
    revision: String,
}

/// The source a learned unit was derived from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DerivedFrom {
    /// The kind of the observation: `inbox_event`, `session_event`,
    /// `tool_result`, `cas_object`, `external_text`.
    kind: String,
    /// The reference of that observation.
    reference: String,
    /// The digest of the observed bytes, when the source has one.
    #[serde(default)]
    digest: Option<String>,
}

/// How the subject judged the version it wrote.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
struct Judgement {
    /// `adopt`, `reject` or `retire`.
    decision: Option<String>,
    /// How the judgement was reached, e.g. `sandbox_comparison`.
    method: Option<String>,
    /// The facts the judgement rests on. A trial attempt is a record of a
    /// trial this installation ran; an observation is bytes this
    /// installation holds.
    evidence: Vec<JudgementEvidence>,
    /// How confident the subject is, when it states one. Absent records that
    /// it has not judged yet; no default number is filled in.
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    notes: Option<String>,
    /// Whether the unit is offered after this call.
    #[serde(default)]
    enable: Option<bool>,
}

/// One piece of evidence a judgement cites.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum JudgementEvidence {
    /// A trial attempt of this installation, bound to this unit version.
    TrialAttempt { attempt_id: String },
    /// Bytes this installation registered for this unit.
    Observation { digest: String },
}

impl Default for Judgement {
    fn default() -> Self {
        Self {
            decision: None,
            method: None,
            evidence: Vec::new(),
            confidence: None,
            notes: None,
            enable: None,
        }
    }
}

/// Map the wire decision of a judgement onto the installation authority's own.
///
/// Both children that record a judgement — `cognition.unit.write:1` for the
/// version it installs and `cognition.unit.judge:1` for a version already
/// installed — share this mapping, so the accepted spellings and the refusal
/// of an unnamed decision cannot drift apart.
pub(crate) fn adoption_decision(value: &str) -> Result<AdoptionDecision> {
    Ok(match value.trim() {
        "adopt" | "adopted" => AdoptionDecision::Adopted,
        "reject" | "rejected" => AdoptionDecision::Rejected,
        "retire" | "retired" => AdoptionDecision::Retired,
        "" => anyhow::bail!("a judgement must state its decision: adopt, reject or retire"),
        other => anyhow::bail!("unknown judgement decision '{other}'; use adopt, reject or retire"),
    })
}

/// Map the wire method of a judgement. An unnamed method records what was
/// said — a statement, never a measurement.
pub(crate) fn adoption_method(value: &str) -> Result<AdoptionMethod> {
    Ok(match value.trim() {
        "" => AdoptionMethod::OperatorStatement,
        "sandbox_comparison" => AdoptionMethod::SandboxComparison,
        "local_trial" => AdoptionMethod::LocalTrial,
        "operator_statement" => AdoptionMethod::OperatorStatement,
        other => anyhow::bail!("unknown judgement method '{other}'"),
    })
}

/// Map the cited evidence of a judgement.
///
/// The installation authority verifies every item against its own durable
/// state; this only refuses a reference that can never be one of its own
/// facts.
pub(crate) fn adoption_evidence(items: &[JudgementEvidence]) -> Result<Vec<AdoptionEvidence>> {
    items
        .iter()
        .map(|item| {
            Ok(match item {
                JudgementEvidence::TrialAttempt { attempt_id } => AdoptionEvidence::TrialAttempt {
                    attempt_id: attempt_id.trim().to_string(),
                },
                JudgementEvidence::Observation { digest } => {
                    let text = digest.trim();
                    let parsed = Digest(text.to_string());
                    ensure!(
                        parsed.canonical_blake3_hex().is_some(),
                        "the observation digest '{text}' is not a canonical blake3 digest"
                    );
                    AdoptionEvidence::StoredObservation { digest: parsed }
                }
            })
        })
        .collect()
}

/// Whether the judgement may switch the unit on.
///
/// Enabling is an installation fact the caller states; only an adopted
/// decision can enable, because a rejected or retired version is kept out of
/// use. None of this makes the unit measured: that stays the evidence's fact.
pub(crate) fn judgement_enable(
    decision: AdoptionDecision,
    enable: Option<bool>,
) -> Result<Option<bool>> {
    match enable {
        Some(enable) if decision == AdoptionDecision::Adopted => Ok(Some(enable)),
        Some(true) => anyhow::bail!("only an adopted unit can be enabled"),
        other => Ok(other),
    }
}

/// The result code a refused judgement evidence reports.
///
/// The installation authority refuses one evidence item with a typed reason.
/// Both children that record judgements share this mapping, so a caller reads
/// a stable code instead of matching this node's prose.
pub(crate) fn evidence_refusal_code(error: &anyhow::Error) -> &'static str {
    error
        .downcast_ref::<shenko_node::AdoptionEvidenceRejected>()
        .map_or("judgement_not_recorded", |rejected| rejected.reason.code())
}

/// One unit the subject wants to write down.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    unit_id: String,
    revision: String,
    /// `awareness`, `perception` or `skill`.
    kind: String,
    title: String,
    capability_card: String,
    #[serde(default)]
    why: Option<String>,
    #[serde(default)]
    cues: Vec<String>,
    #[serde(default)]
    negative_cues: Vec<String>,
    #[serde(default)]
    applicability: Option<String>,
    #[serde(default)]
    knowledge: Vec<String>,
    #[serde(default)]
    provenance_refs: Vec<String>,
    /// Where this insight came from. Without it the unit is an authored
    /// material, not a lesson.
    #[serde(default)]
    derived_from: Option<DerivedFrom>,
    /// The body: the material the model reads.
    body: String,
    /// Whether the unit is offered right away. A unit derived from an
    /// observation starts as a candidate.
    #[serde(default)]
    enabled: Option<bool>,
    /// Versions this one replaces.
    #[serde(default)]
    supersedes: Vec<SupersedesInput>,
    /// The subject's judgement of this version, when it has one.
    #[serde(default)]
    judgement: Option<Judgement>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SupersedesInput {
    revision: String,
    artifact_digest: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenRead {
    schema_version: u32,
    unit_id: String,
    revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenWrite {
    schema_version: u32,
    unit_id: String,
    revision: String,
    kind: String,
    title: String,
    capability_card: String,
    why: Option<String>,
    cues: Vec<String>,
    negative_cues: Vec<String>,
    applicability: Option<String>,
    knowledge: Vec<String>,
    provenance_refs: Vec<String>,
    derived_from: Option<DerivedFrom>,
    body: String,
    enabled: Option<bool>,
    supersedes: Vec<SupersedesInput>,
    judgement: Option<Judgement>,
}

/// Read one installed unit version.
pub struct CognitionReadHandler {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
}

impl CognitionReadHandler {
    pub fn new(store: Arc<Store>, catalog: Arc<CognitionCatalog>) -> Self {
        Self { store, catalog }
    }

    fn plugin_id() -> EntityId {
        "9c1f4a70-52d8-4c6b-9d2e-71b4a5f8e013"
            .parse()
            .expect("fixed cognition-read plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-cognition",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize + ?Sized>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenRead> {
        let input: ReadInput = serde_json::from_value(value)?;
        let unit_id = input.unit_id.trim().to_string();
        let revision = input.revision.trim().to_string();
        ensure!(!unit_id.is_empty() && unit_id.len() <= 128, "unit_id must be 1..=128 characters");
        ensure!(!revision.is_empty() && revision.len() <= 64, "revision must be 1..=64 characters");
        Ok(FrozenRead {
            schema_version: 1,
            unit_id,
            revision,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenRead> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "cognition_read_frozen_input_mismatch"
        );
        let frozen: FrozenRead = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_cognition_read_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "cognition.unit.read.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_cognition_units".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "cognition-read-binding")?;
        let acceptance = self.seal(
            "the content of the named unit version, or an explicit read failure",
            "cognition-read-acceptance",
        )?;
        let retry = self.seal(
            "re-read only the frozen unit version under the original armed attempt; never arm again",
            "cognition-read-retry",
        )?;
        let contract_digest = Digest::blake3(READ_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:cognition-unit-read-v1".into(),
            },
            symbol: "cognition.unit.read.execute".into(),
            schema: SchemaRef {
                name: "cognition-read-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "cognition.unit.read.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenRead,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = match self.catalog.read(&frozen.unit_id, &frozen.revision) {
            Ok(read) => {
                let body = if read.body.len() > MAX_BODY_BYTES {
                    let mut end = MAX_BODY_BYTES;
                    while end > 0 && !read.body.is_char_boundary(end) {
                        end -= 1;
                    }
                    read.body[..end].to_string()
                } else {
                    read.body.clone()
                };
                (
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "read",
                        "unit": {
                            "unit_id": read.unit_id,
                            "revision": read.revision,
                            "artifact_digest": read.artifact_digest,
                            "kind": read.kind.as_str(),
                            "enabled": read.enabled,
                            "current": read.current,
                            "title": read.title,
                            "capability_card": read.capability_card,
                            "body": body,
                            "body_truncated": read.body_truncated || read.body.len() > MAX_BODY_BYTES,
                            "provenance_refs": read.provenance_refs,
                            "learning": read.learning,
                            "adoption": read.adoption,
                            "supersedes": read.supersedes,
                            "superseded_by": read.superseded_by,
                            "dependencies": read.dependencies,
                        },
                    }),
                    OperationOutcome::Succeeded,
                )
            }
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "read",
                    "requested": {"unit_id": frozen.unit_id, "revision": frozen.revision},
                    "error": {"code": "unit_not_readable", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "cognition-read-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            outcome = OperationOutcome::Failed;
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; read a narrower unit instead"
                    }
                }),
                "cognition-read-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "cognition.unit.read.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("cognition-read:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            EffectStatus::None,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "cognition.unit.read"
                && invocation.task.definition_version == 1,
            "cognition_read_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "cognition-read-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        self.observe(&operation, &attempt, &frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for CognitionReadHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "cognition.unit.read".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(READ_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-cognition-reader/1".into(),
            authority_refs: vec!["cognition_unit.read/v1".into()],
            prompt_section: Some(
                "cognition.unit.read/v1 读取某个已安装认知单元在某个确切版本的正文：[delegate:cognition.unit.read:1] {\"unit_id\":\"...\",\"revision\":\"...\"}。目录里只给短能力卡，正文要按需读；读到的是该版本原文，附来源与判断记录。读不到就说读不到，不要凭印象补写单元内容。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == "cognition.unit.read.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}

/// Write one cognition unit down and, when the subject states it, record how
/// it judged that version.
pub struct CognitionWriteHandler {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    package_root: PathBuf,
    /// The self-knowledge view this installation answers from. A unit that
    /// was just installed changes what the instance offers, so the view is
    /// rebuilt from the installation authority again.
    self_knowledge: Option<Arc<shenko_awareness_core::SelfKnowledgeProvider>>,
}

impl CognitionWriteHandler {
    pub fn new(
        store: Arc<Store>,
        catalog: Arc<CognitionCatalog>,
        package_root: PathBuf,
        self_knowledge: Option<Arc<shenko_awareness_core::SelfKnowledgeProvider>>,
    ) -> Self {
        Self {
            store,
            catalog,
            package_root,
            self_knowledge,
        }
    }

    fn plugin_id() -> EntityId {
        "5b7e2d14-8a3c-4f61-b0d5-27c9e4a6f832"
            .parse()
            .expect("fixed cognition-write plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-cognition",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize + ?Sized>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenWrite> {
        let input: WriteInput = serde_json::from_value(value)?;
        let unit_id = input.unit_id.trim().to_string();
        let revision = input.revision.trim().to_string();
        ensure!(!unit_id.is_empty() && unit_id.len() <= 128, "unit_id must be 1..=128 characters");
        ensure!(!revision.is_empty() && revision.len() <= 64, "revision must be 1..=64 characters");
        let kind = match input.kind.trim() {
            "awareness" => "awareness",
            "perception" => "perception",
            "skill" => "skill",
            "code" => "code",
            other => anyhow::bail!("unknown unit kind '{other}'; use awareness, perception, skill or code"),
        };
        let body = input.body.trim().to_string();
        ensure!(!body.is_empty(), "the unit body must not be empty");
        ensure!(body.len() <= MAX_BODY_BYTES, "the unit body exceeds the size bound");
        ensure!(
            input.cues.len() <= MAX_LIST && input.negative_cues.len() <= MAX_LIST,
            "too many cues"
        );
        Ok(FrozenWrite {
            schema_version: 1,
            unit_id,
            revision,
            kind: kind.to_string(),
            title: input.title,
            capability_card: input.capability_card,
            why: input.why,
            cues: input.cues,
            negative_cues: input.negative_cues,
            applicability: input.applicability,
            knowledge: input.knowledge,
            provenance_refs: input.provenance_refs,
            derived_from: input.derived_from,
            body,
            enabled: input.enabled,
            supersedes: input.supersedes,
            judgement: input.judgement,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenWrite> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "cognition_write_frozen_input_mismatch"
        );
        let frozen: FrozenWrite = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_cognition_write_input_schema");
        Ok(frozen)
    }

    /// The learning record of a written unit.
    ///
    /// The reference is kept as the subject stated it. It is marked verified
    /// only when this installation can confirm it against a real fact: a
    /// sealed object in this store, or a measurement this installation
    /// registered. An unconfirmed reference stays a claim, and a unit learned
    /// from a claim stays a candidate.
    fn learning_record(&self, unit_id: &str, derived: &Option<DerivedFrom>) -> Option<LearningRecord> {
        let derived = derived.as_ref()?;
        let digest = derived
            .digest
            .as_ref()
            .map(|digest| Digest(digest.clone()))
            .filter(|digest| digest.canonical_blake3_hex().is_some());
        let (verified, verified_by) = match derived.kind.trim() {
            "cas_object" => (
                digest
                    .as_ref()
                    .is_some_and(|digest| self.store.cas.object_exists(digest)),
                "store_cas",
            ),
            "observation" => (
                digest.as_ref().is_some_and(|digest| {
                    self.catalog
                        .observations(unit_id)
                        .map(|observations| {
                            observations
                                .iter()
                                .any(|observation| observation.digest == digest.0)
                        })
                        .unwrap_or(false)
                }),
                "node_observation",
            ),
            _ => (false, "claimed"),
        };
        Some(LearningRecord {
            source_kind: derived.kind.clone(),
            source_ref: derived.reference.clone(),
            source_digest: derived.digest.clone(),
            source_verified: verified,
            verified_by: verified_by.to_string(),
            learned_at: chrono::Utc::now().to_rfc3339(),
            state: "candidate".to_string(),
        })
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "cognition.unit.write.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "write_local_cognition_units".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "cognition-write-binding")?;
        let acceptance = self.seal(
            "the unit version is installed in this installation's unit authority, or an explicit write failure",
            "cognition-write-acceptance",
        )?;
        let retry = self.seal(
            "re-observe the installation state of the frozen unit version under the original armed attempt; never write a second version",
            "cognition-write-retry",
        )?;
        let contract_digest = Digest::blake3(WRITE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:cognition-unit-write-v1".into(),
            },
            symbol: "cognition.unit.write.execute".into(),
            schema: SchemaRef {
                name: "cognition-write-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "cognition.unit.write.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    /// Build the authored package of a frozen write.
    fn package(&self, frozen: &FrozenWrite) -> Result<(CognitionPackage, Digest)> {
        let kind = match frozen.kind.as_str() {
            "perception" => UnitKind::Perception,
            "skill" => UnitKind::Skill,
            "code" => UnitKind::Code,
            _ => UnitKind::Awareness,
        };
        let entry_text = format!(
            "---\nid: {}\nversion: {}\nkind: {}\ntitle: {}\ncapability_card: {}\n---\n\n{}\n",
            frozen.unit_id,
            frozen.revision,
            frozen.kind,
            frozen.title,
            frozen.capability_card,
            frozen.body
        );
        let package = CognitionPackage {
            unit_id: frozen.unit_id.clone(),
            kind,
            revision: frozen.revision.clone(),
            title: frozen.title.clone(),
            capability_card: frozen.capability_card.clone(),
            why: frozen.why.clone(),
            cues: frozen.cues.clone(),
            negative_cues: frozen.negative_cues.clone(),
            applicability: frozen.applicability.clone(),
            knowledge: frozen.knowledge.clone(),
            provenance_refs: frozen.provenance_refs.clone(),
            learning: self.learning_record(&frozen.unit_id, &frozen.derived_from),
            supersedes: frozen
                .supersedes
                .iter()
                .map(|relation| SupersedesRef {
                    revision: relation.revision.clone(),
                    artifact_digest: relation.artifact_digest.clone(),
                    reason: relation.reason.clone(),
                })
                .collect(),
            provides: Vec::new(),
            dependencies: Vec::new(),
            body: frozen.body.clone(),
            entry_text,
            enabled: frozen.enabled.unwrap_or(false),
        };
        let (_, digest) = package.export()?;
        Ok((package, digest))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenWrite,
    ) -> Result<OperationRecord> {
        // Installing the version is a local durable effect; a refused
        // judgement after it leaves that effect standing and visible.
        let (result, outcome, effect) = match self.write(frozen) {
            Ok((result, OperationOutcome::Succeeded)) => {
                (result, OperationOutcome::Succeeded, EffectStatus::Complete)
            }
            Ok((result, outcome)) => (result, outcome, EffectStatus::Partial),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "write",
                    "requested": {"unit_id": frozen.unit_id, "revision": frozen.revision},
                    "error": {"code": "unit_not_written", "message": format!("{error:#}")},
                }),
                OperationOutcome::Failed,
                EffectStatus::None,
            ),
        };
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "cognition-write-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence"
                    }
                }),
                "cognition-write-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "cognition.unit.write.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("cognition-write:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            effect,
        )
    }

    /// Install the frozen unit and record the judgement it carries.
    ///
    /// Installing and judging are two facts. When the version was installed
    /// but the installation authority refused the judgement, the answer says
    /// exactly that: the version is there, the judgement is not, and the
    /// operation did not succeed. Reporting the whole call as "not written"
    /// would hide a real installation the subject would otherwise not know it
    /// has.
    fn write(&self, frozen: &FrozenWrite) -> Result<(serde_json::Value, OperationOutcome)> {
        let (package, digest) = self.package(frozen)?;
        let installed = self.catalog.install(&package, &self.package_root)?;
        let mut recorded_judgement = None;
        if let Some(judgement) = frozen.judgement.as_ref() {
            let decision = adoption_decision(judgement.decision.as_deref().unwrap_or(""))?;
            let method = adoption_method(judgement.method.as_deref().unwrap_or(""))?;
            let evidence = adoption_evidence(&judgement.evidence)?;
            let enable = judgement_enable(decision, judgement.enable)?;
            let recorded = self.catalog.record_adoption(
                &frozen.unit_id,
                &frozen.revision,
                &digest,
                decision,
                method,
                &evidence,
                judgement.confidence,
                judgement.notes.as_deref(),
                enable,
            );
            match recorded {
                Ok(recorded) => recorded_judgement = Some(recorded),
                Err(error) => {
                    let units: Vec<CognitionUnit> = self.catalog.units()?;
                    if let Some(self_knowledge) = self.self_knowledge.as_ref() {
                        self_knowledge.refresh_cognition_units(&units);
                    }
                    return Ok((
                        serde_json::json!({
                            "schema_version": 1,
                            "status": "failed",
                            "op": "write",
                            "unit_id": frozen.unit_id,
                            "revision": frozen.revision,
                            "artifact_digest": digest,
                            "installed": installed,
                            "judgement": serde_json::Value::Null,
                            "units": units,
                            "error": {
                                "code": evidence_refusal_code(&error),
                                "message": format!("the version is installed, but {error:#}"),
                            },
                        }),
                        OperationOutcome::Failed,
                    ));
                }
            }
        }
        let units: Vec<CognitionUnit> = self.catalog.units()?;
        if let Some(self_knowledge) = self.self_knowledge.as_ref() {
            self_knowledge.refresh_cognition_units(&units);
        }
        Ok((
            serde_json::json!({
                "schema_version": 1,
                "status": "ok",
                "op": "write",
                "unit_id": frozen.unit_id,
                "revision": frozen.revision,
                "artifact_digest": digest,
                "installed": installed,
                "judgement": recorded_judgement,
                "units": units,
            }),
            OperationOutcome::Succeeded,
        ))
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "cognition.unit.write"
                && invocation.task.definition_version == 1,
            "cognition_write_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "cognition-write-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        self.observe(&operation, &attempt, &frozen)
    }
}

#[async_trait]
impl NativeTaskHandler for CognitionWriteHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "cognition.unit.write".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(WRITE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-cognition-writer/1".into(),
            authority_refs: vec!["cognition_unit.write/v1".into()],
            prompt_section: Some(
                "cognition.unit.write/v1 把一条你确实学到的东西写成一个可独立安装的单元。未采纳候选的写法：[delegate:cognition.unit.write:1] {\"unit_id\":\"...\",\"revision\":\"...\",\"kind\":\"awareness\",\"title\":\"...\",\"capability_card\":\"...\",\"body\":\"...\",\"derived_from\":{\"kind\":\"cas_object\",\"reference\":\"operation:...\",\"digest\":\"blake3:...\"},\"enabled\":false}。回执提供 learning_source 时，derived_from 使用它的完整对象：kind、reference、digest 的值各自原样保留；reference 是来源标识，digest 是内容摘要。仅保存未采纳候选时省略 judgement 或设为 null；candidate 是学习状态，不是 judgement.decision。只有作出采纳、拒绝或退役判断时才提供 judgement，decision 只接受 adopt、reject、retire；例如有真实沙箱比较证据时可附加 \"judgement\":{\"decision\":\"adopt\",\"method\":\"sandbox_comparison\",\"evidence\":[{\"kind\":\"trial_attempt\",\"attempt_id\":\"...\"}]}。正文写原则与理由，不写具体动作脚本；来源要指向真实发生过的观察（derived_from.kind 写 cas_object 时给 digest，写 observation 时给本机登记的观测 digest；无法核实的来源会保持“声称”，单元也就只是候选）。判断与证据由你给出：evidence 只能引用本机真实存在的记录——cognition.unit.trial 跑出来的 attempt_id，或 cognition.unit.evidence 读到的观测 digest；内核只记录并核对，不会替你编造证据。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == "cognition.unit.write.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}