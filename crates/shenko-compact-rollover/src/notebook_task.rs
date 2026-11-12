//! The installed notebook definition — `context.notebook` (docs/specs/context-policies.md#spec-12-3-1).
//!
//! One native definition, four operations, all branch-scoped to the caller's
//! own subject:
//!
//! * `read` — the newest journaled revision and the visible notebook;
//! * `replace` — append a new note revision, compare-and-append on the
//!   revision the caller read, then request a rollover at the next safe
//!   boundary so the next prompt carries the saved working state;
//! * `clear` — end the visible notebook with a clearing revision (a reset),
//!   then request the same rollover;
//! * `rollover` — request a new window at the next safe boundary without
//!   changing the notebook.
//!
//! The definition is installed by the notes-rollover policy unit, so
//! disabling or uninstalling that unit withdraws it. Nothing here calls a
//! model, and no operation rewrites the raw history.
use std::sync::Arc;

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_context_policy::{
    MAX_NOTE_BYTES, NoteRevision, NoteSource, NotesError, NotesJournal, PolicyRegistry,
    RolloverRequest,
};
use shenko_store::Store;
use shenko_kernel::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    effect::{
        CheckpointContract, EffectContract, EffectContractBuilder, GuaranteeBasis,
        IdempotencyGuarantee, LifecycleContract, ResourceAccess,
    },
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskRecord},
};

const CONTRACT: &[u8] = b"shenko.context.notebook/v1: branch-journaled working notes and safe-boundary rollover requests; no summarization model; raw history unchanged";

pub const DEFINITION_KIND: &str = "context.notebook";
pub const DEFINITION_VERSION: u32 = 1;
const OPERATION_NAME: &str = "context.notebook.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum NotebookOp {
    Read,
    Replace,
    Clear,
    Rollover,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NotebookInput {
    op: NotebookOp,
    /// The revision the caller last read; 0 means "no note yet".
    expected_revision: Option<u64>,
    text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenOp {
    Read,
    Replace { expected_revision: u64, text: String },
    Clear { expected_revision: u64 },
    Rollover,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    schema_version: u32,
    scope_id: EntityId,
    op: FrozenOp,
}

pub struct ContextNotebookHandler {
    store: Arc<Store>,
    notes: Arc<dyn NotesJournal>,
    registry: Arc<PolicyRegistry>,
}

impl ContextNotebookHandler {
    pub fn new(
        store: Arc<Store>,
        notes: Arc<dyn NotesJournal>,
        registry: Arc<PolicyRegistry>,
    ) -> Self {
        Self { store, notes, registry }
    }

    fn plugin_id() -> EntityId {
        "3f5a2a58-6a54-4c1b-9d55-2b8f0c2f5d31"
            .parse()
            .expect("fixed context notebook plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(bytes, "application/json", "native-context-notebook", schema, 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value, scope_id: EntityId) -> Result<FrozenInput> {
        let NotebookInput { op, expected_revision, text } = serde_json::from_value(value)?;
        let op = match op {
            NotebookOp::Read => {
                ensure!(
                    expected_revision.is_none() && text.is_none(),
                    "read accepts no expected_revision or text"
                );
                FrozenOp::Read
            }
            NotebookOp::Replace => {
                let text = text.context("replace requires text")?;
                ensure!(!text.trim().is_empty(), "replace requires non-empty text; use op=clear to end the notebook");
                ensure!(
                    text.len() <= MAX_NOTE_BYTES,
                    "note text of {} UTF-8 bytes exceeds the {MAX_NOTE_BYTES}-byte limit; the previous note is unchanged",
                    text.len()
                );
                FrozenOp::Replace {
                    expected_revision: expected_revision
                        .context("replace requires the expected_revision the caller read (0 when there is none)")?,
                    text,
                }
            }
            NotebookOp::Clear => {
                ensure!(text.is_none(), "clear accepts no text");
                FrozenOp::Clear {
                    expected_revision: expected_revision
                        .context("clear requires the expected_revision the caller read (0 when there is none)")?,
                }
            }
            NotebookOp::Rollover => {
                ensure!(
                    expected_revision.is_none() && text.is_none(),
                    "rollover accepts no expected_revision or text"
                );
                FrozenOp::Rollover
            }
        };
        Ok(FrozenInput { schema_version: 1, scope_id, op })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "notebook_frozen_input_mismatch"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_notebook_input_schema");
        Ok(frozen)
    }

    fn check_scope(&self, frozen: &FrozenInput) -> Result<()> {
        ensure!(
            self.notes.scope_id() == frozen.scope_id,
            "notebook journal belongs to scope {}, not {}",
            self.notes.scope_id(),
            frozen.scope_id
        );
        ensure!(
            self.registry.ledger().scope_id() == frozen.scope_id,
            "context chain belongs to scope {}, not {}",
            self.registry.ledger().scope_id(),
            frozen.scope_id
        );
        Ok(())
    }

    fn note_view(note: &NoteRevision) -> serde_json::Value {
        serde_json::json!({
            "revision": note.revision,
            "visible": note.visible,
            "text": note.text,
            "source": note.source.label(),
            "written_at_ms": note.written_at_ms,
        })
    }

    fn read(&self, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome)> {
        self.check_scope(frozen)?;
        // One journal read answers both "what is the head" and "what page did
        // I see": reading the head and then the page separately could report a
        // revision the caller never received, and the revision it names is the
        // one a later write is compared against.
        let page = self.notes.revisions(1)?;
        let head = page.first().cloned();
        let result = serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "op": "read",
            "scope": frozen.scope_id,
            "revision": head.as_ref().map(|note| note.revision).unwrap_or(0),
            "head": head.as_ref().map(Self::note_view),
            "visible": head.as_ref().filter(|note| note.visible).map(Self::note_view),
            "journal_page": page.iter().map(Self::note_view).collect::<Vec<_>>(),
            "raw_history": "original raw events of this branch stay readable through history.query by seq or event_id",
        });
        Ok((result, OperationOutcome::Succeeded))
    }

    fn armed(&self, frozen: &FrozenInput) -> Result<serde_json::Value> {
        let request: RolloverRequest = self
            .registry
            .request_rollover(frozen.scope_id)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        Ok(serde_json::json!({
            "armed": true,
            "scope": request.scope_id,
            "requested_at_ms": request.requested_at_ms,
        }))
    }

    fn write_note(&self, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome)> {
        self.check_scope(frozen)?;
        let (note, op) = match &frozen.op {
            FrozenOp::Replace { expected_revision, text } => (
                self.notes.replace(*expected_revision, text, NoteSource::Tool)?,
                "replace",
            ),
            FrozenOp::Clear { expected_revision } => (self.notes.clear(*expected_revision)?, "clear"),
            _ => anyhow::bail!("write_note called for a non-writing operation"),
        };
        let armed = self.armed(frozen)?;
        Ok((
            serde_json::json!({
                "schema_version": 1,
                "status": "ok",
                "op": op,
                "scope": frozen.scope_id,
                "note": Self::note_view(&note),
                "rollover": armed,
            }),
            OperationOutcome::Succeeded,
        ))
    }

    /// A refused write is reported with its exact cause and the journal stays
    /// untouched; the caller re-reads the head and tries again.
    fn refused(&self, error: &NotesError) -> serde_json::Value {
        let (code, message) = match error {
            NotesError::Oversize { .. } => ("note_oversize", error.to_string()),
            NotesError::StaleRevision { current, .. } => {
                let current = current.map(|revision| json_number(revision));
                return serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "error": {"code": "stale_revision", "message": error.to_string(), "current_revision": current},
                });
            }
            NotesError::Empty => ("note_empty", error.to_string()),
            NotesError::ScopeMismatch { .. } => ("scope_mismatch", error.to_string()),
            NotesError::AlreadyJournaled { .. } => ("already_journaled", error.to_string()),
            NotesError::Storage(_) => ("storage_failed", error.to_string()),
        };
        serde_json::json!({
            "schema_version": 1,
            "status": "failed",
            "error": {"code": code, "message": message},
        })
    }

    fn query(&self, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome, EffectStatus)> {
        match &frozen.op {
            FrozenOp::Read => {
                let (result, outcome) = self.read(frozen)?;
                Ok((result, outcome, EffectStatus::None))
            }
            FrozenOp::Rollover => {
                self.check_scope(frozen)?;
                let armed = self.armed(frozen)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "rollover",
                        "scope": frozen.scope_id,
                        "rollover": armed,
                    }),
                    OperationOutcome::Succeeded,
                    EffectStatus::None,
                ))
            }
            FrozenOp::Replace { .. } | FrozenOp::Clear { .. } => match self.write_note(frozen) {
                Ok((result, outcome)) => Ok((result, outcome, EffectStatus::Complete)),
                Err(error) => {
                    if let Some(notes) = error.downcast_ref::<NotesError>() {
                        return Ok((self.refused(notes), OperationOutcome::Failed, EffectStatus::None));
                    }
                    Err(error)
                }
            },
        }
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        frozen: &FrozenInput,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let writes = matches!(frozen.op, FrozenOp::Replace { .. } | FrozenOp::Clear { .. });
        let scope_id = frozen.scope_id;
        let bindings = ResourceBindings {
            provider: "store.context_notes.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: if writes {
                "read_write_own_subject_notes".into()
            } else {
                "read_only_own_subject_notes".into()
            },
            extra: serde_json::json!({"scope_id": scope_id}),
        };
        let resource = self.seal(&bindings, "notebook-query-binding")?;
        let acceptance = self.seal(
            &"the branch journal head after this operation names the exact applied revision, or the operation reports its refusal with the unchanged head",
            "notebook-acceptance",
        )?;
        let retry = self.seal(
            &"reads re-read the same immutable journal; a write is never appended twice — recovery compares the journal head with the frozen expected revision",
            "notebook-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: format!("builtin:{DEFINITION_KIND}-v1"),
            },
            symbol: "context.notebook.execute".into(),
            schema: SchemaRef {
                name: "context-notebook-observation-v1".into(),
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
            operation_name: OPERATION_NAME.into(),
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
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let (result, outcome, effect) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "notebook-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: OPERATION_NAME.into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("context-notebook:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw.clone(),
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(raw),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store
            .record_external_evidence(operation.operation_id, attempt.attempt_id, evidence, Some(outcome), effect)
    }

    /// Recovery of a write settles from evidence; it never appends a second
    /// revision. A journal that still shows the frozen expected revision did
    /// not apply; a journal that moved past it is reported as not applied by
    /// this attempt, so a later note write cannot be mistaken for a replay.
    fn settle_write_after_crash(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let head = self.notes.head()?;
        let expected = match &frozen.op {
            FrozenOp::Replace { expected_revision, .. } | FrozenOp::Clear { expected_revision } => {
                *expected_revision
            }
            _ => 0,
        };
        let applied = head.as_ref().is_some_and(|note| note.revision > expected);
        let result = serde_json::json!({
            "schema_version": 1,
            "status": if applied { "unknown" } else { "failed" },
            "op": match &frozen.op { FrozenOp::Replace { .. } => "replace", FrozenOp::Clear { .. } => "clear", _ => "unknown" },
            "scope": frozen.scope_id,
            "recovery": {
                "expected_revision": expected,
                "current_revision": head.as_ref().map(|note| note.revision),
                "reapplied": false,
                "detail": if applied {
                    "the journal advanced past the frozen expectation; whether this attempt or another writer advanced it is unknown, so nothing is appended again"
                } else {
                    "the journal still shows the frozen expectation; this attempt never applied and nothing is appended again"
                },
            },
        });
        let raw = self.seal_bytes(&serde_json::to_vec(&result)?, "notebook-recovery-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: OPERATION_NAME.into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("context-notebook-recovery:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw.clone(),
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(raw),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(OperationOutcome::Failed),
            EffectStatus::Unknown,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == DEFINITION_KIND
                && invocation.task.definition_version == DEFINITION_VERSION,
            "notebook_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("notebook_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("notebook_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(std::mem::take(&mut invocation.input), EntityId(head.branch_id.0))?;
        let input_ref = self.seal(&frozen, "notebook-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, &frozen, input_ref)?;
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

fn json_number(revision: u64) -> serde_json::Value {
    serde_json::Value::Number(revision.into())
}

#[async_trait]
impl NativeTaskHandler for ContextNotebookHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: DEFINITION_KIND.into(),
            definition_version: DEFINITION_VERSION,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-context-notebook/1".into(),
            authority_refs: vec!["store.context_notes.read_write.own_subject/v1".into()],
            prompt_section: Some(
                "context.notebook/v1 是本分支的工作笔记与安全边界切窗工具（不调用摘要模型，不改写原始历史）。读：[delegate:context.notebook:1] {\"op\":\"read\"}。写：[delegate:context.notebook:1] {\"op\":\"replace\",\"expected_revision\":<刚读到的 revision，0 表示还没有>,\"text\":\"...\"}；写入后会在下一个安全边界请求新窗口，笔记最新版本进入新窗口。结束可见笔记（reset）：{\"op\":\"clear\",\"expected_revision\":<revision>}。只要在合适边界切窗而不改笔记：{\"op\":\"rollover\"}。笔记上限 16384 UTF-8 字节，超限或 revision 过期会被拒绝且旧笔记不变。原始事件始终可经 history.query 按 seq/event_id 读取，窗口化不删除经历。"
                    .into(),
            ),
        }
    }

    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        vec![EffectContractBuilder::new(
            descriptor.implementation_ref.plugin_id,
            descriptor.implementation(),
            OPERATION_NAME,
            descriptor.definition_version,
        )
        .resource("store_context_notes", "own_subject", ResourceAccess::Write)
        // A repeat under the same expected revision is refused by the
        // compare-and-append, so the key makes a retry safe rather than
        // pretending the write is naturally idempotent.
        .idempotency(IdempotencyGuarantee::Strict {
            key_namespace: "store.context_notes.expected_revision".into(),
            max_age_secs: 3600,
        })
        .checkpoint(CheckpointContract {
            capturable: Vec::new(),
            excluded: vec!["journaled note revisions".into()],
            coordinated: false,
            consistency: "compare-and-append on the expected revision".into(),
        })
        .lifecycle(LifecycleContract {
            on_upgrade: "an in-flight note write settles by reading the same journal; it is never appended twice".into(),
            on_uninstall: "the definition is withdrawn; journaled notes and raw history stay readable".into(),
            on_crash: "recovery compares the journal head with the frozen expected revision and appends nothing".into(),
            receipt_readable_after_removal: true,
        })
        .basis(GuaranteeBasis::ClaimOnly)
        .build()]
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration {
            capability_id: "native.context.notebook".into(),
            version: "1".into(),
            description: "Read, replace or clear this branch's journaled working notes and request a new context window at the next safe boundary; no summarization model is called".into(),
            usage: Some("delegate:context.notebook:1 {\"op\":\"read\"|\"replace\"|\"clear\"|\"rollover\"}".into()),
        }]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == OPERATION_NAME,
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        match frozen.op {
            FrozenOp::Read | FrozenOp::Rollover => self.observe(&operation, &attempt, &frozen),
            FrozenOp::Replace { .. } | FrozenOp::Clear { .. } => {
                self.settle_write_after_crash(&operation, &attempt, &frozen)
            }
        }
    }
}
