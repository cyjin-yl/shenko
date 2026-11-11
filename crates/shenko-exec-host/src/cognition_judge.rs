//! The subject's judgement of one installed cognition unit — docs/specs/cognition-units.md#spec-45-7.
//!
//! `cognition.unit.judge:1` records how the subject judged **one exact
//! installed version**, together with the facts it cites. It never authors,
//! re-imports or rewrites a unit: the authored bytes, the learning record and
//! the retained versions stay exactly as `cognition.unit.write:1` left them.
//!
//! That is what closes the self-test loop. A subject writes a candidate, then
//! measures it in the sandbox with `cognition.unit.trial:1`, then decides.
//! Adopting through `write` again would force it to re-emit the whole body:
//! the entry the write child builds is the artifact, so any drift in title,
//! card or body installs a *second* artifact under the same revision, moves
//! the current version onto those unmeasured bytes, and leaves the trial
//! attempt bound to the digest that is no longer current. The judgement then
//! cannot be recorded against the version it measured. Naming the version
//! here instead keeps the measurement, the bytes and the decision on one
//! identity.
//!
//! The kernel still decides nothing about the unit: the decision, the method
//! and the evidence come from the subject, and the installation authority
//! verifies every cited item against its own durable state. A trial attempt
//! makes the judgement empirical; an observation proves bytes exist and never
//! does. An unmeasured judgement leaves the unit exactly as tentative as it
//! was.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_awareness_core::{AdoptionRecord, CognitionCatalog, CognitionUnit};
use shenko_node::{AdoptionDecision, AdoptionEvidence, AdoptionMethod};
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
use std::sync::Arc;

use crate::cognition_task::{
    JudgementEvidence, adoption_decision, adoption_evidence, adoption_method,
    evidence_refusal_code, judgement_enable,
};

const JUDGE_CONTRACT: &[u8] = b"shenko.cognition.unit.judge/v1: records how one exact installed cognition unit version was judged and which of this installation's own trial attempts or observations the judgement cites; it never installs, rewrites, supersedes or re-enables a version by itself";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;
const MAX_LIST: usize = 32;
const MAX_NOTES_BYTES: usize = 4096;

/// One judgement the subject states about an installed version.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeInput {
    unit_id: String,
    revision: String,
    /// The exact artifact the subject judged. When it is absent the named
    /// revision must name exactly one current version; a revision retained
    /// with several artifacts must be named by digest rather than guessed.
    #[serde(default)]
    artifact_digest: Option<String>,
    /// `adopt`, `reject` or `retire`.
    decision: String,
    /// How the judgement was reached, e.g. `sandbox_comparison`.
    #[serde(default)]
    method: Option<String>,
    /// The facts the judgement rests on: a trial attempt this installation
    /// ran, or bytes this installation holds for the version.
    #[serde(default)]
    evidence: Vec<JudgementEvidence>,
    /// How confident the subject is, when it states one. Absent records that
    /// it has not judged yet; this child never fills a default number.
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    notes: Option<String>,
    /// Whether the version is offered after this call.
    #[serde(default)]
    enable: Option<bool>,
}

/// The frozen judgement: the exact identity, decision, method and evidence
/// the original attempt is bound to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenJudge {
    schema_version: u32,
    unit_id: String,
    revision: String,
    artifact_digest: Option<String>,
    decision: AdoptionDecision,
    method: AdoptionMethod,
    evidence: Vec<AdoptionEvidence>,
    /// Frozen with the rest of the judgement: a replay records the same
    /// confidence, and an absent one stays explicitly unknown.
    #[serde(default)]
    confidence: Option<f64>,
    notes: Option<String>,
    enable: Option<bool>,
}

/// Record how the subject judged an installed cognition unit version.
pub struct CognitionJudgeHandler {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    /// The self-knowledge view this installation answers from. A recorded
    /// judgement can change what the instance offers and whether a unit is
    /// on, so the view is rebuilt from the installation authority again.
    self_knowledge: Option<Arc<shenko_awareness_core::SelfKnowledgeProvider>>,
}

impl CognitionJudgeHandler {
    pub fn new(
        store: Arc<Store>,
        catalog: Arc<CognitionCatalog>,
        self_knowledge: Option<Arc<shenko_awareness_core::SelfKnowledgeProvider>>,
    ) -> Self {
        Self {
            store,
            catalog,
            self_knowledge,
        }
    }

    fn plugin_id() -> EntityId {
        "1e6c9b52-3f47-4d0a-8ac5-6b71d2f9043e"
            .parse()
            .expect("fixed cognition-judge plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-cognition", schema, 1)
        {
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

    fn freeze(&self, value: serde_json::Value) -> Result<FrozenJudge> {
        let input: JudgeInput = serde_json::from_value(value)?;
        let unit_id = input.unit_id.trim().to_string();
        let revision = input.revision.trim().to_string();
        ensure!(!unit_id.is_empty() && unit_id.len() <= 128, "unit_id must be 1..=128 characters");
        ensure!(!revision.is_empty() && revision.len() <= 64, "revision must be 1..=64 characters");
        let artifact_digest = match input.artifact_digest.as_deref().map(str::trim) {
            Some(digest) if !digest.is_empty() => {
                ensure!(
                    Digest(digest.to_string()).canonical_blake3_hex().is_some(),
                    "the artifact digest '{digest}' is not a canonical blake3 digest"
                );
                Some(digest.to_string())
            }
            _ => None,
        };
        let decision = adoption_decision(&input.decision)?;
        let method = adoption_method(input.method.as_deref().unwrap_or(""))?;
        ensure!(input.evidence.len() <= MAX_LIST, "too many evidence items");
        let evidence = adoption_evidence(&input.evidence)?;
        if let Some(confidence) = input.confidence {
            // The subject's own number travels unchanged. This child adds no
            // scale and no range: it only refuses a value that is not a
            // number at all, and records nothing when the subject states
            // nothing.
            ensure!(
                confidence.is_finite(),
                "the stated confidence must be a finite number; state nothing when you have not judged"
            );
        }
        if let Some(notes) = input.notes.as_ref() {
            ensure!(
                notes.len() <= MAX_NOTES_BYTES,
                "the judgement notes exceed the text bound"
            );
        }
        let enable = judgement_enable(decision, input.enable)?;
        Ok(FrozenJudge {
            schema_version: 1,
            unit_id,
            revision,
            artifact_digest,
            decision,
            method,
            evidence,
            confidence: input.confidence,
            notes: input.notes,
            enable,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenJudge> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "cognition_judge_frozen_input_mismatch"
        );
        let frozen: FrozenJudge = serde_json::from_slice(&bytes)?;
        ensure!(
            frozen.schema_version == 1,
            "unsupported_cognition_judge_input_schema"
        );
        Ok(frozen)
    }

    /// The installed version this judgement names, read from the authority.
    ///
    /// The identity is never taken from the caller on trust: the version must
    /// be retained here, and when the caller names no artifact the revision
    /// must name exactly one current version. Guessing between two artifacts
    /// of one revision would record a judgement on content the subject may
    /// never have seen.
    fn target(&self, frozen: &FrozenJudge) -> Result<CognitionUnit> {
        let versions: Vec<CognitionUnit> = self
            .catalog
            .versions(&frozen.unit_id)?
            .into_iter()
            .filter(|unit| unit.revision == frozen.revision)
            .collect();
        ensure!(
            !versions.is_empty(),
            "unit '{}' has no retained revision '{}'",
            frozen.unit_id,
            frozen.revision
        );
        match frozen.artifact_digest.as_deref() {
            Some(digest) => versions
                .into_iter()
                .find(|unit| unit.artifact_digest == digest)
                .with_context(|| {
                    format!(
                        "unit '{}' has no retained revision '{}' with artifact {digest}",
                        frozen.unit_id, frozen.revision
                    )
                }),
            None => {
                let mut current = versions.into_iter().filter(|unit| unit.current);
                let Some(unit) = current.next() else {
                    anyhow::bail!(
                        "revision '{}' of unit '{}' is retained but not current; read the catalog again and pass the exact artifact_digest",
                        frozen.revision,
                        frozen.unit_id
                    );
                };
                ensure!(
                    current.next().is_none(),
                    "revision '{}' of unit '{}' names several current versions; pass the exact artifact_digest",
                    frozen.revision,
                    frozen.unit_id
                );
                Ok(unit)
            }
        }
    }

    /// The fresh facts of the judged version, read back from the authority.
    fn read_back(&self, frozen: &FrozenJudge, artifact_digest: &str) -> Result<CognitionUnit> {
        self.catalog
            .versions(&frozen.unit_id)?
            .into_iter()
            .find(|unit| unit.revision == frozen.revision && unit.artifact_digest == artifact_digest)
            .context("the judged version left the installation before it could be read back")
    }

    /// Record the frozen judgement and answer with the authority's own facts.
    fn judge(&self, frozen: &FrozenJudge) -> Result<serde_json::Value> {
        let target = self.target(frozen)?;
        let artifact = Digest(target.artifact_digest.clone());
        self.catalog.record_adoption(
            &frozen.unit_id,
            &frozen.revision,
            &artifact,
            frozen.decision,
            frozen.method,
            &frozen.evidence,
            frozen.confidence,
            frozen.notes.as_deref(),
            frozen.enable,
        )?;
        let facts = self.read_back(frozen, &artifact.0)?;
        let units = self.catalog.units()?;
        if let Some(self_knowledge) = self.self_knowledge.as_ref() {
            self_knowledge.refresh_cognition_units(&units);
        }
        Ok(verdict(frozen, &facts, true, None))
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "cognition.unit.judge.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "record_local_cognition_judgements".into(),
            extra: serde_json::json!({}),
        };
        let resource = self.seal(&bindings, "cognition-judge-binding")?;
        let acceptance = self.seal(
            "the judgement of the exact named unit version is recorded in this installation's unit authority, or an explicit failure",
            "cognition-judge-acceptance",
        )?;
        let retry = self.seal(
            "re-read the judgement this installation holds for the frozen unit version under the original armed attempt; never re-record a judgement that would replace a different one",
            "cognition-judge-retry",
        )?;
        let contract_digest = Digest::blake3(JUDGE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:cognition-unit-judge-v1".into(),
            },
            symbol: "cognition.unit.judge.execute".into(),
            schema: SchemaRef {
                name: "cognition-judge-observation-v1".into(),
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
            operation_name: "cognition.unit.judge.v1".into(),
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

    /// Seal the result as this attempt's own evidence.
    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        result: serde_json::Value,
        outcome: OperationOutcome,
        effect: EffectStatus,
    ) -> Result<OperationRecord> {
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "cognition-judge-observation")?;
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
                "cognition-judge-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "cognition.unit.judge.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("cognition-judge:{}", attempt.attempt_id)),
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

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenJudge,
    ) -> Result<OperationRecord> {
        match self.judge(frozen) {
            Ok(result) => self.record(operation, attempt, result, OperationOutcome::Succeeded, EffectStatus::Complete),
            Err(error) => {
                let result = serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "judge",
                    "requested": {
                        "unit_id": frozen.unit_id,
                        "revision": frozen.revision,
                        "artifact_digest": frozen.artifact_digest,
                        "decision": frozen.decision.as_str(),
                        "method": frozen.method.as_str(),
                    },
                    "error": {"code": evidence_refusal_code(&error), "message": format!("{error:#}")},
                });
                // The installation authority rejected the judgement, so this
                // attempt applied nothing: the failure and the absent effect
                // are reported together instead of implying a partial write.
                self.record(operation, attempt, result, OperationOutcome::Failed, EffectStatus::None)
            }
        }
    }

    /// Record the judgement once, on the original armed attempt.
    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "cognition.unit.judge"
                && invocation.task.definition_version == 1,
            "cognition_judge_task_identity_mismatch"
        );
        let frozen = self.freeze(std::mem::take(&mut invocation.input))?;
        let input_ref = self.seal(&frozen, "cognition-judge-frozen-input")?;
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

/// Whether the installation's recorded judgement is the one this attempt
/// asked for.
///
/// Only the facts the node itself verified are compared: the decision, the
/// method, the cited attempt ids and observation digests, and the notes. The
/// record's own timestamp and enable effects are not part of the identity, so
/// a replay does not have to rewrite either.
fn same_judgement(recorded: &AdoptionRecord, frozen: &FrozenJudge) -> bool {
    recorded.decision == frozen.decision.as_str()
        && recorded.method == frozen.method.as_str()
        && recorded.notes == frozen.notes
        // The comparison is over the number the subject stated, not over how
        // a record encodes "no number": an older record without the field and
        // an explicit unknown are the same judgement fact.
        && recorded.stated_confidence() == frozen.confidence
        && recorded.evidence.len() == frozen.evidence.len()
        && frozen.evidence.iter().all(|cited| match cited {
            AdoptionEvidence::TrialAttempt { attempt_id } => recorded
                .evidence
                .iter()
                .any(|fact| fact.kind == "trial_attempt" && fact.attempt_id.as_deref() == Some(attempt_id.as_str())),
            AdoptionEvidence::StoredObservation { digest } => recorded
                .evidence
                .iter()
                .any(|fact| fact.kind == "stored_observation" && fact.digest.as_deref() == Some(digest.0.as_str())),
        })
}

/// The result body of one judgement, read from the installation's own facts.
fn verdict(
    frozen: &FrozenJudge,
    facts: &CognitionUnit,
    recorded_now: bool,
    reason: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "status": "ok",
        "op": "judge",
        "unit_id": frozen.unit_id,
        "revision": frozen.revision,
        "artifact_digest": facts.artifact_digest,
        "current": facts.current,
        "decision": frozen.decision.as_str(),
        "method": frozen.method.as_str(),
        "enable_requested": frozen.enable,
        "enabled": facts.enabled,
        "state": facts.state.as_str(),
        "adoption": facts.adoption,
        "learning": facts.learning,
        "empirical": facts.adoption.as_ref().is_some_and(|adoption| adoption.empirical),
        // What the cited trials recorded, so the answer distinguishes a
        // measured judgement from one recorded on an attempt that measured
        // nothing. Absent on a record written before the node classified
        // evidence: that record's verdict is kept, not re-derived.
        "measurement": facts
            .adoption
            .as_ref()
            .and_then(|adoption| adoption.measurement.clone()),
        "evidence_verified": facts
            .adoption
            .as_ref()
            .is_some_and(|adoption| adoption.evidence_verified),
        "recorded_now": recorded_now,
        "reason": reason,
    })
}

#[async_trait]
impl NativeTaskHandler for CognitionJudgeHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "cognition.unit.judge".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(JUDGE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-cognition-judge/1".into(),
            authority_refs: vec!["cognition_unit.judge/v1".into()],
            prompt_section: Some(
                "cognition.unit.judge/v1 为一个已经装在本机上的单元版本登记你的判断，不改写单元正文、不新增版本：[delegate:cognition.unit.judge:1] {\"unit_id\":\"...\",\"revision\":\"...\",\"artifact_digest\":\"...\",\"decision\":\"adopt\",\"method\":\"sandbox_comparison\",\"evidence\":[{\"kind\":\"trial_attempt\",\"attempt_id\":\"...\"}],\"enable\":true}。写完候选之后先用 cognition.unit.trial:1 在沙箱里自测，再在这里引用那次 attempt_id；不要为了登记判断而重写同样的正文——那会以同一版本号装出另一份未被测量的工件。证据只能引用本机真实存在的记录：trial 跑出来的 attempt_id，或 cognition.unit.evidence 读到的本机观测 digest。本机按记录本身分类这次试验：measured（计划的每个样本都给出了评审结论）、partial（只测到一部分）、inconclusive（已关闭但没有一条测量）、unexecuted（开了但从未关闭）。开了从未关闭的 attempt 会被拒绝登记（error.code=evidence_unexecuted）；不属于本机或不属于这个版本的引用是 evidence_not_admissible。已关闭但没有测量的判断可以登记，但结果里 measurement=inconclusive、empirical=false，单元仍保持候选/tentative；只有核对通过、且确实来自本机测量的证据（measured 或 partial，包括失败/超时的真实运行）才让它成为 adopted。artifact_digest 可省略（此时该版本必须是唯一 current）；同一版本存有多份工件时必须点名。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    /// Re-read the judgement this installation holds for the frozen version.
    ///
    /// A judgement is the subject's fact, not the result of a rerun: a replay
    /// must never replace a different decision the subject recorded later.
    /// The judgement is therefore applied on reconcile only when the version
    /// carries none at all.
    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id
                && operation.operation_name == "cognition.unit.judge.v1",
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
        let target = match self.target(&frozen) {
            Ok(target) => target,
            Err(error) => {
                let result = serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "judge",
                    "reconciled": true,
                    "requested": {"unit_id": frozen.unit_id, "revision": frozen.revision},
                    "error": {"code": "judged_version_not_retained", "message": format!("{error:#}")},
                });
                return self.record(
                    &operation,
                    &attempt,
                    result,
                    OperationOutcome::Failed,
                    EffectStatus::None,
                );
            }
        };
        match target.adoption.as_ref() {
            Some(recorded) if same_judgement(recorded, &frozen) => {
                let result = verdict(&frozen, &target, false, Some("the installation already holds this judgement"));
                self.record(&operation, &attempt, result, OperationOutcome::Succeeded, EffectStatus::Complete)
            }
            None => {
                // Nothing was recorded for this version: applying the frozen
                // judgement now adds the one fact the original attempt asked
                // for and replaces nobody's decision.
                let result = match self.judge(&frozen) {
                    Ok(result) => result,
                    Err(error) => serde_json::json!({
                        "schema_version": 1,
                        "status": "failed",
                        "op": "judge",
                        "reconciled": true,
                        "requested": {"unit_id": frozen.unit_id, "revision": frozen.revision},
                        "error": {"code": evidence_refusal_code(&error), "message": format!("{error:#}")},
                    }),
                };
                let outcome = if result.get("status").and_then(|value| value.as_str()) == Some("ok") {
                    OperationOutcome::Succeeded
                } else {
                    OperationOutcome::Failed
                };
                let effect = if outcome == OperationOutcome::Succeeded { EffectStatus::Complete } else { EffectStatus::None };
                self.record(&operation, &attempt, result, outcome, effect)
            }
            Some(_) => {
                let result = serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "op": "judge",
                    "reconciled": true,
                    "recorded_now": false,
                    "requested": {"unit_id": frozen.unit_id, "revision": frozen.revision},
                    "reason": "the installation holds a different judgement for this version; the older operation is not re-applied over it",
                });
                self.record(&operation, &attempt, result, OperationOutcome::Failed, EffectStatus::None)
            }
        }
    }
}

#[cfg(test)]
mod tests;
