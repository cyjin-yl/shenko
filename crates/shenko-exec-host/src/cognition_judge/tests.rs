//! Consumer-boundary regressions for the judgement child.
//!
//! These run the real installation authority (a real `Node` over its own
//! SQLite store), the real write child that authors the candidate, and the
//! real judgement child. The sandbox comparison itself needs a served model
//! and is measured by the efficacy consumer; what is exercised here is the
//! contract that turns a finished comparison into an adoption — and refuses
//! everything that is not one. A trial attempt opened and closed here is a
//! runner-opened attempt of this installation, not a claim about model
//! behaviour or unit efficacy.

use super::*;
use parking_lot::Mutex;
use shenko_node::{Node, TrialOutcome};
use shenko_types::task::TaskInvocation;
use shenko_types::agent::CallContext;
use shenko_types::ids::RuntimeGeneration;
use shenko_types::store::WriteBatch;
use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};
use crate::CognitionWriteHandler;

/// One throwaway installation with the two children the self-test loop uses.
struct Fixture {
    store: Arc<Store>,
    catalog: Arc<CognitionCatalog>,
    write: CognitionWriteHandler,
    judge: CognitionJudgeHandler,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("shenko-cognition-judge-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).expect("fixture directory");
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).expect("store"));
        let node = Arc::new(Mutex::new(Node::open(&dir.join("node")).expect("node")));
        let catalog = Arc::new(CognitionCatalog::new(node));
        let write = CognitionWriteHandler::new(
            Arc::clone(&store),
            Arc::clone(&catalog),
            dir.join("cognition"),
            None,
        );
        let judge = CognitionJudgeHandler::new(Arc::clone(&store), Arc::clone(&catalog), None);
        Self { store, catalog, write, judge }
    }

    fn task(
        &self,
        descriptor: &NativeTaskDescriptor,
        input_ref: &ObjectRef,
    ) -> (TaskRecord, TaskInvocation) {
        let task_id = EntityId::new();
        let invocation_id = EntityId::new();
        let epoch = ControllerEpoch(1);
        let agent_id = EntityId::new();
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: EntityId::new() },
            background: false,
            kind: descriptor.kind.clone(),
            definition_version: descriptor.definition_version,
            implementation_ref: descriptor.implementation_ref.clone(),
            input_ref: input_ref.clone(),
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        self.store.commit(batch).expect("task commit");
        let invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        (task, invocation)
    }

    fn native(
        &self,
        task: TaskRecord,
        invocation: TaskInvocation,
        input: serde_json::Value,
    ) -> NativeTaskInvocation {
        let task_id = task.task_id;
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation.invocation_id);
        context.controller_epoch = invocation.epoch;
        let authority = AuthorityRef {
            delegation_id: task_id,
            delegation_revision: RecordRevision(0),
            decision_ref: EntityId::new(),
        };
        NativeTaskInvocation {
            context,
            task_invocation: invocation,
            task,
            operation_id: EntityId::new(),
            authority,
            input,
            child_tools: None,
        }
    }

    /// The installation's own facts of one retained version.
    fn version(&self, unit_id: &str, revision: &str, artifact: &str) -> CognitionUnit {
        self.catalog
            .versions(unit_id)
            .expect("versions")
            .into_iter()
            .find(|unit| unit.revision == revision && unit.artifact_digest == artifact)
            .expect("retained version")
    }

    /// The sealed bytes one admitted task names as its frozen input.
    fn input_ref(&self, value: &serde_json::Value) -> ObjectRef {
        self.store
            .cas
            .seal_object(
                &serde_json::to_vec(value).expect("input json"),
                "application/json",
                "native-cognition",
                "fixture-input",
                1,
            )
            .expect("sealed input")
            .object
    }

    fn result_of(&self, operation_id: EntityId) -> serde_json::Value {
        let evidence = self
            .store
            .external_operation_evidence(operation_id)
            .expect("evidence");
        let receipt = evidence.last().expect("one result receipt");
        let object = receipt.normalized_ref.as_ref().expect("normalized result");
        serde_json::from_slice(&self.store.cas.read_object(&object.digest).expect("result bytes"))
            .expect("result json")
    }

    /// Author a candidate through the write child and return its artifact.
    async fn write_unit(
        &self,
        unit_id: &str,
        revision: &str,
        kind: &str,
        body: &str,
    ) -> String {
        let observed = self
            .store
            .cas
            .seal_object(b"durable observation bytes", "text/plain", "fixture", "observation", 1)
            .expect("sealed observation")
            .object
            .digest
            .0
            .clone();
        let input = serde_json::json!({
            "unit_id": unit_id,
            "revision": revision,
            "kind": kind,
            "title": "Self-tested unit",
            "capability_card": "measure before believing",
            "body": body,
            "provenance_refs": ["fixture:self-test-loop"],
            "derived_from": {
                "kind": "cas_object",
                "reference": "fixture observation",
                "digest": observed,
            },
        });
        // The write child freezes its own input; the task record only names
        // the bytes this attempt was admitted for.
        let result = self.write_request(input).await;
        assert_eq!(result["status"], "ok", "write failed: {result}");
        result["artifact_digest"]
            .as_str()
            .expect("artifact digest")
            .to_string()
    }

    /// Run one authored write through the write child and return its result.
    async fn write_request(&self, input: serde_json::Value) -> serde_json::Value {
        let input_ref = self.input_ref(&input);
        let descriptor = self.write.descriptor();
        let (task, invocation) = self.task(&descriptor, &input_ref);
        let operation = self
            .write
            .execute(self.native(task, invocation, input))
            .await
            .expect("write operation");
        self.result_of(operation.operation_id)
    }

    /// The plan a sandbox comparison opens its attempt with.
    ///
    /// One scenario and one repetition per condition: the plan names two
    /// sample slots, exactly as the real trial child's does.
    fn sandbox_plan(unit_id: &str, revision: &str, artifact: &str) -> serde_json::Value {
        serde_json::json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact,
            "candidate": {"enabled": true, "unit_id": unit_id, "revision": revision},
            "control": {"excluded_unit_id": unit_id, "exclude_all_versions": true, "enable_root": false},
            "scenarios": [{"name": "self-test", "message_digest": "fixture", "rubric_digest": "fixture"}],
            "trials_per_condition": 1,
            "scope": "adoption contract regression; no model efficacy claim",
        })
    }

    /// One sample receipt of the shape the real trial child writes.
    fn sample(artifact: &str, condition: &str, verdict: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "sample",
            "scenario": "self-test",
            "condition": condition,
            "trial": 0,
            "verdict": verdict,
            "digest": artifact,
        })
    }

    /// Open and close one comparison attempt of an exact version.
    ///
    /// The receipts are the per-slot facts the run produced. A run that was
    /// closed before it measured anything passes none.
    fn closed_comparison(
        &self,
        unit_id: &str,
        revision: &str,
        artifact: &str,
        outcome: TrialOutcome,
        receipts: &[serde_json::Value],
    ) -> String {
        let plan = Self::sandbox_plan(unit_id, revision, artifact);
        let begun = self
            .catalog
            .begin_trial(unit_id, revision, &Digest(artifact.to_string()), "sandbox_comparison", &plan)
            .expect("trial opened");
        let attempt = begun["attempt_id"].as_str().expect("attempt id").to_string();
        self.catalog
            .finish_trial(&attempt, outcome, receipts)
            .expect("trial closed");
        attempt
    }

    /// A bound comparison whose candidate and control slots were both
    /// judged: the one attempt shape that carries a measurement.
    fn sandbox_comparison(
        &self,
        unit_id: &str,
        revision: &str,
        artifact: &str,
        outcome: TrialOutcome,
    ) -> String {
        self.closed_comparison(
            unit_id,
            revision,
            artifact,
            outcome,
            &[
                Self::sample(artifact, "candidate", "effective"),
                Self::sample(artifact, "control", "ineffective"),
            ],
        )
    }

    async fn judge_request(
        &self,
        unit_id: &str,
        revision: &str,
        artifact: &str,
        decision: &str,
        method: &str,
        evidence: Vec<serde_json::Value>,
        enable: Option<bool>,
    ) -> (serde_json::Value, TaskRecord, EntityId) {
        let mut input = serde_json::json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact,
            "decision": decision,
            "method": method,
            "evidence": evidence,
        });
        if let Some(enable) = enable {
            input["enable"] = serde_json::json!(enable);
        }
        let frozen = self.judge.freeze(input.clone()).expect("judge freeze");
        let input_ref = self
            .judge
            .seal(&frozen, "cognition-judge-frozen-input")
            .expect("judge seal");
        let descriptor = self.judge.descriptor();
        let (task, invocation) = self.task(&descriptor, &input_ref);
        let operation = self
            .judge
            .execute(self.native(task.clone(), invocation, input))
            .await
            .expect("judge operation");
        let result = self.result_of(operation.operation_id);
        (result, task, operation.operation_id)
    }

    /// Arm a judgement without running it: the durable state a crash leaves.
    fn arm_judge(
        &self,
        unit_id: &str,
        revision: &str,
        artifact: &str,
        decision: &str,
        evidence: Vec<serde_json::Value>,
    ) -> (TaskRecord, EntityId) {
        let input = serde_json::json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact,
            "decision": decision,
            "method": "sandbox_comparison",
            "evidence": evidence,
        });
        let frozen = self.judge.freeze(input.clone()).expect("judge freeze");
        let input_ref = self
            .judge
            .seal(&frozen, "cognition-judge-frozen-input")
            .expect("judge seal");
        let descriptor = self.judge.descriptor();
        let (task, invocation) = self.task(&descriptor, &input_ref);
        let native = self.native(task.clone(), invocation, input);
        let (mut operation, mut attempt) = self
            .judge
            .operation_parts(&native, input_ref)
            .expect("operation parts");
        assert!(
            self.store
                .arm_external_operation(&native.task_invocation, &mut operation, &mut attempt)
                .expect("arm")
                .is_none(),
            "the fixture armed an operation twice"
        );
        (task, operation.operation_id)
    }
}

/// A subject-written candidate becomes an adoption only through a comparison
/// this installation ran, and never by rewriting the authored bytes.
#[tokio::test]
async fn subject_authored_candidate_is_self_tested_and_adopted_without_rewriting_it() {
    let fixture = Fixture::new();
    let unit = "awareness.self-test-loop";
    let body = "先把能核实的事实摆出来，再决定要不要相信自己记得的那一套；没核实过就保持不确定。";
    let artifact = fixture.write_unit(unit, "1", "awareness", body).await;

    let candidate = fixture.version(unit, "1", &artifact);
    assert_eq!(candidate.state.as_str(), "candidate");
    assert!(!candidate.enabled, "a written candidate offered itself before any measurement");
    let learning = candidate.learning.as_ref().expect("learning record");
    assert!(learning.source_verified, "the sealed source was not verified");
    let learned_at = learning.learned_at.clone();
    let authored_entry = fixture.catalog.read(unit, "1").expect("read").entry_text;

    // (a) An attempt this installation never opened cannot back a judgement.
    let (unknown, _, _) = fixture
        .judge_request(
            unit, "1", &artifact, "adopt", "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": EntityId::new().to_string()})],
            Some(true),
        )
        .await;
    assert_eq!(unknown["status"], "failed", "an invented attempt was accepted: {unknown}");
    assert_eq!(unknown["error"]["code"], "evidence_not_admissible");
    assert_eq!(fixture.version(unit, "1", &artifact).state.as_str(), "candidate");

    // (b) A real attempt of another unit version is refused.
    let other = "perception.self-test-other";
    let other_artifact = fixture.write_unit(other, "1", "perception", "另一条观察。").await;
    assert_eq!(
        fixture.version(other, "1", &other_artifact).kind.as_str(),
        "perception",
        "the authored perception lost its kind"
    );
    let other_attempt = fixture.sandbox_comparison(other, "1", &other_artifact, TrialOutcome::Succeeded);
    let (foreign, _, _) = fixture
        .judge_request(
            unit, "1", &artifact, "adopt", "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": other_attempt})],
            None,
        )
        .await;
    assert_eq!(foreign["status"], "failed", "a trial of another version certified this one: {foreign}");
    assert_eq!(foreign["error"]["code"], "evidence_not_admissible");
    assert_eq!(fixture.version(unit, "1", &artifact).state.as_str(), "candidate");

    // (c) An attempt that was opened and never closed records no terminal
    // fact, so it is refused as evidence rather than recorded as a trial.
    let opened = fixture
        .catalog
        .begin_trial(
            unit, "1", &Digest(artifact.clone()), "sandbox_comparison",
            &serde_json::json!({"unit_id": unit, "scope": "opened and never closed"}),
        )
        .expect("open attempt");
    let open_attempt = opened["attempt_id"].as_str().expect("attempt id").to_string();
    let (unfinished, _, _) = fixture
        .judge_request(
            unit, "1", &artifact, "adopt", "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": open_attempt})],
            None,
        )
        .await;
    assert_eq!(unfinished["status"], "failed", "an unfinished attempt certified a unit: {unfinished}");
    assert_eq!(
        unfinished["error"]["code"], "evidence_unexecuted",
        "an attempt with no terminal trial fact was not named as unexecuted: {unfinished}"
    );

    // (d) The finished comparison of this exact version, whose receipts name
    // the sample slots its own plan asked for, is the one judgement that can
    // carry empirical acceptance.
    let attempt = fixture.sandbox_comparison(unit, "1", &artifact, TrialOutcome::Succeeded);
    let (adopted, task, operation) = fixture
        .judge_request(
            unit, "1", &artifact, "adopt", "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": attempt})],
            Some(true),
        )
        .await;
    assert_eq!(adopted["status"], "ok", "the bound judgement failed: {adopted}");
    assert_eq!(adopted["state"], "adopted");
    assert_eq!(adopted["empirical"], true);
    assert_eq!(adopted["measurement"], "measured");
    assert_eq!(adopted["evidence_verified"], true);
    assert_eq!(adopted["enabled"], true);
    assert_eq!(adopted["artifact_digest"], artifact);
    let facts = adopted["adoption"].as_object().expect("adoption record");
    assert_eq!(facts["measurement"], "measured");
    assert_eq!(facts["evidence"][0]["measurement"], "measured");
    assert_eq!(facts["evidence"][0]["scored"], serde_json::json!(2));
    assert_eq!(facts["evidence"][0]["planned_slots"], serde_json::json!(2));

    // The authored version is still the only version, with the same bytes and
    // the same source record: judging never re-authors.
    let versions = fixture.catalog.versions(unit).expect("versions");
    assert_eq!(versions.len(), 1, "judging installed a second artifact: {versions:?}");
    let read = fixture.catalog.read(unit, "1").expect("read");
    assert_eq!(read.body, body);
    assert_eq!(read.entry_text, authored_entry, "judging rewrote the authored entry");
    assert_eq!(read.artifact_digest, artifact);
    assert_eq!(
        read.learning.as_ref().expect("learning").learned_at,
        learned_at,
        "the judged version's source record was rewritten"
    );

    // Replaying the recorded judgement re-reads it; it never records a second
    // judgement over the first.
    let recorded_at = fixture
        .version(unit, "1", &artifact)
        .adoption
        .expect("adoption")
        .recorded_at;
    let reconciled = fixture.judge.reconcile(&task, operation).await.expect("reconcile");
    assert_eq!(reconciled.state, OperationState::Resolved);
    assert_eq!(reconciled.outcome, Some(OperationOutcome::Succeeded));
    assert_eq!(
        fixture.version(unit, "1", &artifact).adoption.expect("adoption").recorded_at,
        recorded_at,
        "reconcile recorded a second judgement"
    );

    // (e) An observation proves the bytes exist; it never makes a judgement
    // empirical, so the unit stays tentative.
    let observation = fixture
        .catalog
        .register_observation(other, "1", &Digest(other_artifact.clone()), "report", b"caller-written report")
        .expect("observation")["digest"]
        .as_str()
        .expect("observation digest")
        .to_string();
    let (observed, _, _) = fixture
        .judge_request(
            other, "1", &other_artifact, "adopt", "sandbox_comparison",
            vec![serde_json::json!({"kind": "observation", "digest": observation})],
            None,
        )
        .await;
    assert_eq!(observed["status"], "ok", "an observation-backed judgement was refused: {observed}");
    assert_eq!(observed["empirical"], false, "an observation claimed a measurement");
    assert_eq!(observed["evidence_verified"], true);
    assert_eq!(observed["state"], "candidate", "an unmeasured judgement left the tentative state");

    // A rejected version keeps its judgement and its evidence, and leaves the
    // disclosure index instead of being offered as usable material.
    let (rejected, _, _) = fixture
        .judge_request(
            other, "1", &other_artifact, "reject", "sandbox_comparison",
            vec![serde_json::json!({"kind": "observation", "digest": observation})],
            None,
        )
        .await;
    assert_eq!(rejected["status"], "ok", "the rejection failed: {rejected}");
    assert_eq!(rejected["state"], "rejected");
    let offered: Vec<String> = fixture
        .catalog
        .cards()
        .expect("cards")
        .into_iter()
        .map(|card| card.unit_id)
        .collect();
    assert!(
        !offered.contains(&other.to_string()),
        "a rejected unit stayed in the disclosure index: {offered:?}"
    );
}

/// A trial closed without measuring anything records the subject's judgement
/// and never turns the written candidate into an adoption.
#[tokio::test]
async fn a_closed_trial_without_a_measured_sample_never_adopts_the_unit() {
    let fixture = Fixture::new();
    let unit = "awareness.unmeasured-close";
    let artifact = fixture
        .write_unit(unit, "1", "awareness", "没测过就先别当作已经验证。")
        .await;
    assert_eq!(fixture.version(unit, "1", &artifact).state.as_str(), "candidate");

    // The trial child takes this path when the run failed before any sample
    // was judged: the attempt is closed as unknown with no receipts.
    let closed = fixture.closed_comparison(unit, "1", &artifact, TrialOutcome::Unknown, &[]);
    let (recorded, _, _) = fixture
        .judge_request(
            unit,
            "1",
            &artifact,
            "adopt",
            "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": closed})],
            None,
        )
        .await;
    assert_eq!(
        recorded["status"], "ok",
        "a closed trial is a real fact and stays citable: {recorded}"
    );
    assert_eq!(recorded["measurement"], "inconclusive");
    assert_eq!(
        recorded["empirical"], false,
        "a trial that measured nothing certified adoption: {recorded}"
    );
    assert_eq!(
        recorded["state"], "candidate",
        "an unmeasured judgement left the tentative state: {recorded}"
    );
    let adoption = recorded["adoption"].as_object().expect("adoption record");
    assert_eq!(adoption["measurement"], "inconclusive");
    assert_eq!(adoption["evidence"][0]["outcome"], "unknown");
    assert_eq!(adoption["evidence"][0]["scored"], serde_json::json!(0));

    // A receipt naming a sample slot the attempt's plan never asked for was
    // not produced by this attempt's run, so the node refuses to close with
    // it instead of trusting a caller-written measurement.
    let stray = fixture
        .catalog
        .begin_trial(
            unit,
            "1",
            &Digest(artifact.clone()),
            "sandbox_comparison",
            &Fixture::sandbox_plan(unit, "1", &artifact),
        )
        .expect("open attempt");
    let stray_attempt = stray["attempt_id"].as_str().expect("attempt id").to_string();
    let refused = fixture.catalog.finish_trial(
        &stray_attempt,
        TrialOutcome::Succeeded,
        &[serde_json::json!({
            "kind": "sample",
            "scenario": "never-planned",
            "condition": "candidate",
            "trial": 0,
            "verdict": "effective",
            "digest": artifact,
        })],
    );
    assert!(
        refused.is_err(),
        "a receipt outside the attempt's plan was accepted: {refused:?}"
    );

    // The slots the plan did name are the measurement, including a negative
    // one: a measured failure is still a measurement.
    let measured = fixture.sandbox_comparison(unit, "1", &artifact, TrialOutcome::Failed);
    let (adopted, _, _) = fixture
        .judge_request(
            unit,
            "1",
            &artifact,
            "adopt",
            "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": measured})],
            Some(true),
        )
        .await;
    assert_eq!(adopted["status"], "ok", "the measured judgement failed: {adopted}");
    assert_eq!(adopted["measurement"], "measured");
    assert_eq!(adopted["empirical"], true);
    assert_eq!(adopted["state"], "adopted");
}

/// Reconciling reads the judgement this installation holds and never replaces
/// a different decision the subject recorded later.
#[tokio::test]
async fn reconcile_reads_the_judgement_and_never_replaces_a_different_one() {
    let fixture = Fixture::new();
    let unit = "awareness.judged-after-crash";
    let artifact = fixture.write_unit(unit, "1", "awareness", "先核实，再决定。").await;
    let attempt = fixture.sandbox_comparison(unit, "1", &artifact, TrialOutcome::Succeeded);

    // A judgement armed before the node call and then interrupted: nothing is
    // recorded yet, so reconciling completes that one judgement.
    let (task, operation) = fixture.arm_judge(
        unit, "1", &artifact, "adopt",
        vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": attempt.clone()})],
    );
    let settled = fixture.judge.reconcile(&task, operation).await.expect("reconcile");
    assert_eq!(settled.state, OperationState::Resolved);
    assert_eq!(settled.outcome, Some(OperationOutcome::Succeeded));
    let recorded = fixture.version(unit, "1", &artifact).adoption.expect("adoption");
    assert_eq!(recorded.decision, "adopted");
    assert!(recorded.empirical);

    // The subject later retires it through a normal call.
    let (retired, _, _) = fixture
        .judge_request(
            unit, "1", &artifact, "retire", "sandbox_comparison",
            vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": attempt})],
            None,
        )
        .await;
    assert_eq!(retired["status"], "ok", "the later judgement failed: {retired}");
    assert_eq!(retired["state"], "retired");

    // A second interrupted judgement that asked for adoption must not undo the
    // retirement the subject recorded afterwards.
    let (stale_task, stale_operation) = fixture.arm_judge(
        unit, "1", &artifact, "adopt",
        vec![serde_json::json!({"kind": "trial_attempt", "attempt_id": attempt})],
    );
    let replayed = fixture.judge.reconcile(&stale_task, stale_operation).await.expect("reconcile");
    assert_eq!(replayed.state, OperationState::Resolved);
    assert_eq!(replayed.outcome, Some(OperationOutcome::Failed));
    let held = fixture.version(unit, "1", &artifact).adoption.expect("adoption");
    assert_eq!(held.decision, "retired", "the replay replaced the subject's later decision");
    let result = fixture.result_of(stale_operation);
    assert_eq!(result["recorded_now"], false);
}

/// A refused judgement on a first write does not pretend the version was
/// never installed: the installation and the missing judgement are reported
/// as the two separate facts they are.
#[tokio::test]
async fn a_refused_judgement_leaves_the_installed_version_visible() {
    let fixture = Fixture::new();
    let unit = "awareness.refused-judgement";
    let observed = fixture
        .store
        .cas
        .seal_object(b"durable observation bytes", "text/plain", "fixture", "observation", 1)
        .expect("sealed observation")
        .object
        .digest
        .0
        .clone();
    let result = fixture
        .write_request(serde_json::json!({
            "unit_id": unit,
            "revision": "1",
            "kind": "awareness",
            "title": "Self-tested unit",
            "capability_card": "measure before believing",
            "body": "没有测量就不算采用。",
            "provenance_refs": ["fixture:self-test-loop"],
            "derived_from": {"kind": "cas_object", "reference": "fixture observation", "digest": observed},
            "judgement": {
                "decision": "adopt",
                "method": "sandbox_comparison",
                "evidence": [{"kind": "trial_attempt", "attempt_id": EntityId::new().to_string()}],
                "enable": true,
            },
        }))
        .await;
    assert_eq!(result["status"], "failed", "a refused judgement was reported as a success: {result}");
    assert_eq!(result["error"]["code"], "evidence_not_admissible");
    assert!(
        result["installed"].is_object(),
        "the answer hid the installation it did make: {result}"
    );
    let artifact = result["artifact_digest"].as_str().expect("artifact digest").to_string();
    let installed = fixture.version(unit, "1", &artifact);
    assert_eq!(installed.state.as_str(), "candidate", "an unjudged version was not left tentative");
    assert!(!installed.enabled, "the refused judgement switched the unit on");
    assert!(installed.adoption.is_none(), "a refused judgement was recorded anyway");

    // The version is real and readable, so the subject can judge it through
    // the judgement child once it has a measurement.
    let read = fixture.catalog.read(unit, "1").expect("read");
    assert_eq!(read.body, "没有测量就不算采用。");
}
