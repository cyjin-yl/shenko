//! Runtime-error awareness natives — SPEC §45.14.
//!
//! `errors.read` is a read-only view over the committed `runtime_error` raw
//! facts of the caller's own subject session. `issue.submit` is an armed
//! external effect the subject may author: it writes one bounded draft JSON
//! into the local issues outbox and records its CAS-sealed bytes as evidence.
//! Neither handler decides when the subject should act; the prompt sections
//! only state what exists.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::{RUNTIME_ERROR_EVENT_TYPE, SessionEventFilter, Store};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::history_task::{
    MAX_NORMALIZED_BYTES, MAX_QUERY_BYTES, collection_result, read_result,
};

const ERRORS_CONTRACT: &[u8] = b"shenko.errors.read/v1: immutable own-subject runtime_error raw reads; no external effects; bounded match-centred previews";
const ISSUE_CONTRACT: &[u8] = b"shenko.issue.submit/v1: subject-authored issue draft into the local issues outbox; one digest-deduped artifact; no network submission";
const MAX_LIMIT: usize = 50;
const MAX_TITLE_CHARS: usize = 200;
const MAX_SUMMARY_CHARS: usize = 8000;
const MAX_ERROR_REFS: usize = 32;
const MAX_ERROR_REF_CHARS: usize = 512;
// Six bytes cover the largest JSON escape of one Unicode scalar; the rest
// covers the fixed envelope and pretty-printed array punctuation.
const MAX_DRAFT_BYTES: usize =
    (MAX_TITLE_CHARS + MAX_SUMMARY_CHARS + MAX_ERROR_REFS * MAX_ERROR_REF_CHARS) * 6 + 4096;

// ---------------------------------------------------------------------------
// errors.read — read-only journal queries over runtime_error facts.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ErrorsOp {
    Search,
    List,
    Read,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorsInput {
    op: ErrorsOp,
    query: Option<String>,
    event_id: Option<EntityId>,
    seq: Option<i64>,
    #[serde(default)]
    from_seq: i64,
    to_seq: Option<i64>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenErrorsQuery {
    Search { query: String, filter: SessionEventFilter },
    List { filter: SessionEventFilter },
    ReadEvent { event_id: EntityId },
    ReadSeq { seq: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenErrorsInput {
    schema_version: u32,
    session_id: EntityId,
    query: FrozenErrorsQuery,
}

pub struct ErrorsReadHandler {
    store: Arc<Store>,
}

impl ErrorsReadHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "3f7a9c2e-4d61-4b08-9e5f-2a1c8d6b3e47"
            .parse()
            .expect("fixed errors.read plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-errors-read", schema, 1)
        {
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

    /// Freeze the model input. The journal scope is fixed to `runtime_error`
    /// facts: a caller cannot widen or relabel the view, and a stored event
    /// of any other type is never returned.
    fn freeze(&self, value: serde_json::Value, session_id: EntityId) -> Result<FrozenErrorsInput> {
        let ErrorsInput { op, query, event_id, seq, from_seq, to_seq, limit } =
            serde_json::from_value(value)?;
        ensure!(from_seq >= 0, "from_seq must be non-negative");
        if let Some(to_seq) = to_seq {
            ensure!(to_seq >= from_seq, "to_seq must be at least from_seq");
        }
        let limit_was_specified = limit.is_some();
        let limit = limit.unwrap_or(20);
        ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1..=50");
        let filter = SessionEventFilter {
            from_seq,
            to_seq: to_seq.unwrap_or(i64::MAX),
            limit,
            event_types: vec![RUNTIME_ERROR_EVENT_TYPE.to_owned()],
            roles: vec![],
        };
        let query = match op {
            ErrorsOp::Search => {
                ensure!(
                    event_id.is_none() && seq.is_none(),
                    "search does not accept an event pointer"
                );
                let query = query.context("search requires query")?;
                let query = query.trim();
                ensure!(!query.is_empty(), "search query must not be empty");
                ensure!(query.len() <= MAX_QUERY_BYTES, "search query exceeds 4096 bytes");
                FrozenErrorsQuery::Search { query: query.into(), filter }
            }
            ErrorsOp::List => {
                ensure!(
                    query.is_none() && event_id.is_none() && seq.is_none(),
                    "list accepts only range fields"
                );
                FrozenErrorsQuery::List { filter }
            }
            ErrorsOp::Read => {
                ensure!(
                    query.is_none()
                        && from_seq == 0
                        && to_seq.is_none()
                        && !limit_was_specified,
                    "read accepts exactly one event_id or seq, without search filters"
                );
                match (event_id, seq) {
                    (Some(event_id), None) => FrozenErrorsQuery::ReadEvent { event_id },
                    (None, Some(seq)) => {
                        ensure!(seq >= 0, "seq must be non-negative");
                        FrozenErrorsQuery::ReadSeq { seq }
                    }
                    _ => anyhow::bail!("read requires exactly one of event_id or seq"),
                }
            }
        };
        Ok(FrozenErrorsInput { schema_version: 1, session_id, query })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenErrorsInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "errors_read_frozen_input_mismatch"
        );
        let frozen: FrozenErrorsInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_errors_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        session_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.runtime_errors.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_own_subject_errors".into(),
            extra: serde_json::json!({"session_id": session_id}),
        };
        let resource = self.seal(&bindings, "errors-read-binding")?;
        let acceptance = self.seal(
            &"a bounded chronological read of immutable runtime_error rows, or an explicit read failure",
            "errors-read-acceptance",
        )?;
        let retry = self.seal(
            &"re-read only the frozen immutable range under the original armed attempt; never arm again",
            "errors-read-retry",
        )?;
        let contract_digest = Digest::blake3(ERRORS_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:errors-read-v1".into(),
            },
            symbol: "errors.read.execute".into(),
            schema: SchemaRef {
                name: "errors-read-observation-v1".into(),
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
            operation_name: "errors.read.v1".into(),
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

    fn query(&self, frozen: &FrozenErrorsInput) -> Result<(serde_json::Value, OperationOutcome)> {
        match &frozen.query {
            FrozenErrorsQuery::Search { query, filter } => {
                let events =
                    self.store.search_session_events(frozen.session_id, query, filter)?;
                Ok((
                    collection_result("search", frozen.session_id, filter, events, Some(query)),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenErrorsQuery::List { filter } => {
                let events = self.store.list_session_events(frozen.session_id, filter)?;
                Ok((
                    collection_result("list", frozen.session_id, filter, events, None),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenErrorsQuery::ReadEvent { event_id } => {
                let event = self
                    .store
                    .session_event(*event_id)?
                    .filter(|event| {
                        event.session_id == frozen.session_id
                            && event.event_type == RUNTIME_ERROR_EVENT_TYPE
                    });
                Ok(read_result(frozen.session_id, *event_id, None, event))
            }
            FrozenErrorsQuery::ReadSeq { seq } => {
                let event = self
                    .store
                    .session_events_range(frozen.session_id, *seq, *seq)?
                    .into_iter()
                    .find(|event| event.event_type == RUNTIME_ERROR_EVENT_TYPE);
                Ok(read_result(frozen.session_id, EntityId::nil(), Some(*seq), event))
            }
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenErrorsInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "errors-read-observation")?;
        let normalized = if bytes.len() <= MAX_NORMALIZED_BYTES {
            raw.clone()
        } else {
            outcome = OperationOutcome::Failed;
            self.seal(
                &serde_json::json!({
                    "schema_version": 1,
                    "status": "failed",
                    "session_id": frozen.session_id,
                    "error": {
                        "code": "normalized_result_too_large",
                        "message": "the exact observation is sealed in raw evidence; increase the bounded native result contract before delivery"
                    }
                }),
                "errors-read-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "errors.read.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("raw-errors:{}", attempt.attempt_id)),
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
            invocation.task.kind == "errors.read" && invocation.task.definition_version == 1,
            "errors_read_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("errors_read_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("errors_read_task_origin_is_outside_current_subject_branch")?;
        let frozen =
            self.freeze(std::mem::take(&mut invocation.input), EntityId(head.branch_id.0))?;
        let input_ref = self.seal(&frozen, "errors-read-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, frozen.session_id, input_ref)?;
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
impl NativeTaskHandler for ErrorsReadHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "errors.read".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(ERRORS_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-errors-reader/1".into(),
            authority_refs: vec!["store.runtime_errors.read.own_subject/v1".into()],
            prompt_section: Some(
                "errors.read/v1 是已安装的只读运行时错误 child。每个 engine/runtime 错误在被停止或降级时已作为 runtime_error 事实提交进你自己的原始历史。列出错误：\
                 [delegate:errors.read:1] {\"op\":\"list\",\"limit\":20}；op=search 按字面词检索并给出以命中为中心的有界预览，op=read 重读一条完整 data（给 event_id 或 seq）。\
                 只读 runtime_error 事件；search/list 只含 data_preview，截断标志与起始 Unicode 标量位置标明预览不是全文；预览没显示某内容不证明原文没有，需要完整正文用 read。\
                 journal_dropped_total 非零表示有错误日志在提交前丢失。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "errors.read.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative. A later
            // successful read must not erase it or create another attempt.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}

// ---------------------------------------------------------------------------
// issue.submit — subject-authored issue draft into the local outbox.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueInput {
    title: String,
    summary: String,
    #[serde(default)]
    error_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenIssueInput {
    schema_version: u32,
    agent_id: EntityId,
    title: String,
    summary: String,
    error_refs: Vec<String>,
    digest: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedIssueBody {
    title: String,
    summary: String,
    error_refs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedIssueDraft {
    v: u32,
    digest: String,
    created_at: String,
    source: String,
    audience: String,
    issue: PublishedIssueBody,
}

// Same no-replace rename primitive and target support as shenko-update.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_issue_draft(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn rename_issue_draft(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;
    let mut from: Vec<u16> = from.as_os_str().encode_wide().collect();
    let mut to: Vec<u16> = to.as_os_str().encode_wide().collect();
    if from.contains(&0) || to.contains(&0) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"));
    }
    from.push(0);
    to.push(0);
    // SAFETY: both NUL-terminated strings outlive the call. MoveFileW refuses
    // an existing destination instead of replacing its bytes.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn rename_issue_draft(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive issue draft publication is not supported on this platform",
    ))
}

pub struct IssueSubmitHandler {
    store: Arc<Store>,
    outbox_dir: PathBuf,
}

impl IssueSubmitHandler {
    pub fn new(store: Arc<Store>, outbox_dir: PathBuf) -> Self {
        Self { store, outbox_dir }
    }

    fn plugin_id() -> EntityId {
        "7d2e6b4a-1f9c-4e38-a570-8b6d3f21c9e5"
            .parse()
            .expect("fixed issue.submit plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self
            .store
            .cas
            .seal_object(bytes, "application/json", "native-issue-submit", schema, 1)
        {
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

    fn freeze(&self, value: serde_json::Value, agent_id: EntityId) -> Result<FrozenIssueInput> {
        let IssueInput { title, summary, error_refs } = serde_json::from_value(value)?;
        let title = title.trim().to_owned();
        let summary = summary.trim().to_owned();
        ensure!(
            !title.is_empty() && title.chars().count() <= MAX_TITLE_CHARS,
            "title must be 1..=200 characters"
        );
        ensure!(
            !summary.is_empty() && summary.chars().count() <= MAX_SUMMARY_CHARS,
            "summary must be 1..=8000 characters"
        );
        ensure!(error_refs.len() <= MAX_ERROR_REFS, "at most 32 error_refs");
        for reference in &error_refs {
            ensure!(
                !reference.trim().is_empty() && reference.chars().count() <= MAX_ERROR_REF_CHARS,
                "each error_ref must be 1..=512 characters"
            );
        }
        let digest = Digest::blake3(
            serde_json::to_vec(&serde_json::json!({
                "v": 1,
                "agent_id": agent_id,
                "title": title,
                "summary": summary,
                "error_refs": error_refs,
            }))?
            .as_slice(),
        );
        Ok(FrozenIssueInput {
            schema_version: 1,
            agent_id,
            title,
            summary,
            error_refs,
            digest: digest.to_string(),
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenIssueInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "issue_frozen_input_mismatch"
        );
        let frozen: FrozenIssueInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_issue_input_schema");
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        agent_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "fs.issues_outbox.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "write_issue_drafts".into(),
            extra: serde_json::json!({
                "agent_id": agent_id,
                "outbox_dir": self.outbox_dir.to_string_lossy(),
            }),
        };
        let resource = self.seal(&bindings, "issue-submit-binding")?;
        let acceptance = self.seal(
            &"one digest-deduped draft JSON exists in the local issues outbox and its bytes are sealed as evidence, or an explicit failure",
            "issue-submit-acceptance",
        )?;
        let retry = self.seal(
            &"re-observe the original armed draft under the original attempt; the same digest never writes a second file",
            "issue-submit-retry",
        )?;
        let contract_digest = Digest::blake3(ISSUE_CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:issue-submit-v1".into(),
            },
            symbol: "issue.submit.execute".into(),
            schema: SchemaRef {
                name: "issue-submit-observation-v1".into(),
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
            operation_name: "issue.submit.v1".into(),
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

    fn existing_draft(path: &Path, frozen: &FrozenIssueInput) -> Result<Option<Vec<u8>>> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
        }
        let mut file = match options.open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("issue draft cannot be opened"),
        };
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "issue draft is not a regular file");
        ensure!(metadata.len() <= MAX_DRAFT_BYTES as u64, "issue draft exceeds its byte bound");
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&mut file).take(MAX_DRAFT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_DRAFT_BYTES, "issue draft exceeds its byte bound");
        let draft: PublishedIssueDraft = serde_json::from_slice(&bytes)
            .context("existing issue draft is not a valid v1 artifact")?;
        ensure!(
            draft.v == 1
                && draft.digest == frozen.digest
                && draft.source == "shenko.issue.submit.v1"
                && draft.audience == "local_outbox"
                && draft.issue.title == frozen.title
                && draft.issue.summary == frozen.summary
                && draft.issue.error_refs == frozen.error_refs,
            "existing issue draft does not match its frozen submission"
        );
        chrono::DateTime::parse_from_rfc3339(&draft.created_at)
            .context("existing issue draft has an invalid creation time")?;
        file.sync_all()?;
        Ok(Some(bytes))
    }

    fn sync_outbox_path(&self) -> Result<()> {
        for directory in self.outbox_dir.ancestors().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::File::open(directory)?.sync_all()?;
        }
        Ok(())
    }

    /// An existing name is not evidence. Only a bounded, valid v1 artifact
    /// matching this frozen submission can be attested; it is never replaced.
    fn publish(&self, frozen: &FrozenIssueInput) -> Result<(Vec<u8>, PathBuf, bool)> {
        std::fs::create_dir_all(&self.outbox_dir).map_err(|error| {
            anyhow::anyhow!("issue outbox {} unavailable: {error}", self.outbox_dir.display())
        })?;
        let file_name = format!("issue-{}.json", frozen.digest.replace(':', "-"));
        let path = self.outbox_dir.join(&file_name);
        if let Some(existing) = Self::existing_draft(&path, frozen)? {
            self.sync_outbox_path()?;
            return Ok((existing, path, false));
        }
        let draft = serde_json::json!({
            "v": 1,
            "digest": frozen.digest,
            "created_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "source": "shenko.issue.submit.v1",
            "audience": "local_outbox",
            "issue": {
                "title": frozen.title,
                "summary": frozen.summary,
                "error_refs": frozen.error_refs,
            },
        });
        let bytes = serde_json::to_vec_pretty(&draft)?;
        let tmp = self.outbox_dir.join(format!(".{file_name}.tmp-{}", EntityId::new()));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        let publication: Result<bool> = (|| {
            file.write_all(&bytes)?;
            file.sync_all()?;
            match rename_issue_draft(&tmp, &path) {
                Ok(()) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                Err(error) => Err(error).context("issue draft atomic publication failed"),
            }
        })();
        drop(file);
        if !matches!(&publication, Ok(true)) {
            if let Err(cleanup) = std::fs::remove_file(&tmp) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    if publication.is_ok() {
                        return Err(cleanup).context("issue draft staging cleanup failed");
                    }
                    tracing::warn!(error=%cleanup, "issue draft staging cleanup failed after publication failure");
                }
            }
        }
        let written = publication?;
        self.sync_outbox_path()?;
        let published = Self::existing_draft(&path, frozen)?
            .context("published issue draft disappeared before observation")?;
        Ok((published, path, written))
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenIssueInput,
    ) -> Result<OperationRecord> {
        let (bytes, path, written) = self.publish(frozen)?;
        let artifact = self.seal_bytes(&bytes, "issue-draft-artifact")?;
        let result = serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "digest": frozen.digest,
            "artifact": {
                "path": path,
                "digest": artifact.digest,
                "bytes": artifact.bytes,
                "written_now": written,
            },
            "submission": {
                "delivered_to": "local_outbox",
                "network_sent": false,
            },
        });
        let raw = self.seal_bytes(&serde_json::to_vec(&result)?, "issue-submit-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "issue.submit.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("issue-draft:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(artifact),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(OperationOutcome::Succeeded),
            EffectStatus::Complete,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "issue.submit" && invocation.task.definition_version == 1,
            "issue_submit_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("issue_submit_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("issue_submit_task_origin_is_outside_current_subject_branch")?;
        let frozen =
            self.freeze(std::mem::take(&mut invocation.input), invocation.task.agent_id)?;
        let input_ref = self.seal(&frozen, "issue-submit-frozen-input")?;
        let (mut operation, mut attempt) =
            self.operation_parts(&invocation, frozen.agent_id, input_ref)?;
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
impl NativeTaskHandler for IssueSubmitHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "issue.submit".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(ISSUE_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-issue-submit/1".into(),
            authority_refs: vec!["outbox.issue_drafts.write/v1".into()],
            prompt_section: Some(
                "issue.submit/v1 是已安装的本地问题草案 child。提交：\
                 [delegate:issue.submit:1] {\"title\":\"……\",\"summary\":\"……\",\"error_refs\":[\"<errors.read 的 event_ref 或自由文本>\"]}。\
                 title≤200 字符、summary≤8000 字符、error_refs≤32 条每条≤512 字符。\
                 效果是把一份有界草案 JSON 写入本地 issues outbox（digest 去重，同一内容不会产生第二份文件），不做网络提交；\
                 回执的 normalized_ref 指向该草案字节并给出路径。是否提交以及内容是你的自主决定。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "issue.submit.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Unknown/conflict evidence stays authoritative; the original
            // effect is never replayed as a second write.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        self.observe(&operation, &attempt, &frozen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_store::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};
    use shenko_types::subject::{SubjectBranchHead, SubjectGenesis, SubjectStep, SubjectStepState, SubjectMode, GenesisRef};
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};

    fn open_store(tag: &str) -> (PathBuf, Arc<Store>) {
        let dir = std::env::temp_dir().join(format!("shenko-{tag}-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        (dir, store)
    }

    /// A real subject head plus first step through the genesis transaction, so
    /// native tasks can carry a true created_by_step_id.
    fn subject_fixture(store: &Store) -> (EntityId, EntityId) {
        let agent = EntityId::new();
        let ledger_id = store.ledger.ledger_id();
        let precommit = CommitRef::new(ledger_id, LedgerSeq(0));
        let object = store
            .cas
            .seal_object(b"seed", "text/plain", "test", "raw", 1)
            .unwrap()
            .object;
        let first_id = EntityId::new();
        let genesis = SubjectGenesis {
            genesis_id: EntityId::new(),
            subject_id: agent,
            bootstrap_id: EntityId::new(),
            launch_id: EntityId::new(),
            seed_manifest_ref: object.clone(),
            initial_profile_ref: object.clone(),
            home_manifest_ref: object.clone(),
            bootstrap_event_ref: object.clone(),
            created_commit: precommit,
        };
        let head = SubjectBranchHead {
            agent_id: agent,
            branch_id: BranchId::new(),
            subject_root_task_id: EntityId::new(),
            controller_epoch: ControllerEpoch(7),
            software_generation: RuntimeGeneration("fixture-generation".into()),
            genesis_ref: GenesisRef { ledger_id, genesis_id: genesis.genesis_id },
            committed_step_no: 0,
            committed_step_id: None,
            committed_at: precommit,
            next_step_id: first_id,
            mode: SubjectMode::Active,
            mode_reason_ref: None,
            record_revision: RecordRevision(0),
        };
        let first = SubjectStep {
            step_id: first_id,
            branch_id: head.branch_id.clone(),
            step_no: 1,
            expected_parent_commit: precommit,
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
        let root_task_record = shenko_types::task::TaskRecord::subject_root(
            head.subject_root_task_id,
            agent,
            object,
            precommit,
        );
        store
            .initialize_genesis(&genesis, &head, &first, &root_task_record)
            .unwrap();
        (agent, first_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn running_task(
        store: &Store,
        implementation_ref: ImplementationRef,
        agent_id: EntityId,
        step_id: EntityId,
        kind: &str,
        input_ref: ObjectRef,
    ) -> (TaskRecord, TaskInvocation) {
        let task_id = EntityId::new();
        let invocation_id = EntityId::new();
        let epoch = ControllerEpoch(7);
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: EntityId::new() },
            background: false,
            kind: kind.into(),
            definition_version: 1,
            implementation_ref,
            input_ref,
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::Map::new(),
            residual_case_ids: vec![],
            created_by_step_id: Some(step_id),
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration("fixture-generation".into()),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        (task, invocation)
    }

    fn native_invocation(
        task: TaskRecord,
        task_invocation: TaskInvocation,
        input: serde_json::Value,
    ) -> NativeTaskInvocation {
        let operation_id = EntityId::new();
        let mut context = CallContext::new_system(task_invocation.software_generation.clone());
        context.task_id = Some(task.task_id);
        context.invocation_id = Some(task_invocation.invocation_id);
        context.controller_epoch = task_invocation.epoch;
        NativeTaskInvocation {
            context,
            task_invocation,
            authority: AuthorityRef {
                delegation_id: task.task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            task,
            operation_id,
            input,
        }
    }

    #[test]
    fn errors_read_only_exposes_runtime_error_facts_with_bounded_preview() {
        let (dir, store) = open_store("errors-read");
        let handler = ErrorsReadHandler::new(store.clone());
        let (session_id, agent_id) = (EntityId::new(), EntityId::new());
        let error_data = serde_json::json!({
            "v":1, "kind":"subject_loop", "source_module":"shenko_subject::loop_runner",
            "message": format!("{}配额请求失败 timeout {}", "前文".repeat(300), "后文".repeat(200)),
            "message_truncated": false, "step_no": 41, "task_id": null,
            "error_digest": "blake3:abc", "occurred_at": "2026-11-01T00:00:00Z",
            "journal_dropped_total": 0, "extra": {}
        })
        .to_string();
        let mut batch = WriteBatch::new();
        batch.raw_session_upserts.push(RawSession {
            session_id,
            agent_id,
            started_at: "2026-11-01T00:00:00Z".into(),
            data: "{}".into(),
        });
        batch.session_event_upserts.push(RawSessionEvent {
            event_id: EntityId::new(),
            session_id,
            seq: 1,
            event_type: "user".into(),
            role: "user".into(),
            data: serde_json::json!({"v":1,"text":"配额请求失败 timeout 普通对话"}).to_string(),
        });
        batch.session_event_upserts.push(RawSessionEvent {
            event_id: EntityId::new(),
            session_id,
            seq: 2,
            event_type: RUNTIME_ERROR_EVENT_TYPE.into(),
            role: RUNTIME_ERROR_EVENT_TYPE.into(),
            data: error_data.clone(),
        });
        store.commit(batch).unwrap();
        let query = |input| handler.query(&handler.freeze(input, session_id).unwrap()).unwrap().0;

        let list = query(serde_json::json!({"op":"list"}));
        assert_eq!(list["events"].as_array().unwrap().len(), 1);
        assert_eq!(list["events"][0]["seq"], 2);
        assert_eq!(list["events"][0]["event_type"], RUNTIME_ERROR_EVENT_TYPE);

        let found = query(serde_json::json!({"op":"search","query":"timeout"}));
        assert_eq!(found["events"].as_array().unwrap().len(), 1);
        let event = &found["events"][0];
        assert_eq!(event["seq"], 2);
        assert!(event["data_preview"].as_str().unwrap().contains("配额请求失败 timeout"));
        assert_eq!(event["data_preview_truncated_before"], true);
        assert_eq!(event["data_preview_match_visible"], true);

        // A non-runtime_error event at seq 1 is invisible even to a direct read.
        let hidden = query(serde_json::json!({"op":"read","seq":1}));
        assert_eq!(hidden["status"], "failed");
        assert_eq!(hidden["error"]["code"], "event_not_found");
        let read = query(serde_json::json!({"op":"read","seq":2}));
        assert_eq!(read["status"], "ok");
        assert_eq!(read["event"]["data"], error_data);
        drop(handler);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn issue_submit_publishes_digest_deduped_draft_and_reconciles() {
        let (dir, store) = open_store("issue-submit");
        let outbox = dir.join("issues");
        let (agent, step_id) = subject_fixture(&store);
        let session_id = EntityId(store.load_subject(agent).unwrap().0.branch_id.0);
        let handler = IssueSubmitHandler::new(store.clone(), outbox.clone());
        let input = serde_json::json!({
            "title": "quota probe timed out under load",
            "summary": "主体观察到多次配额请求超时。password=hunter2 不应出现在草案之外任何地方造成的问题。",
            "error_refs": [format!("session:{session_id}/event:{}/seq:3", EntityId::new())]
        });
        let frozen = handler.freeze(input.clone(), agent).unwrap();
        let input_ref = handler.seal(&frozen, "issue-submit-frozen-input").unwrap();
        let (task, task_invocation) = running_task(
            &store,
            handler.descriptor().implementation_ref,
            agent,
            step_id,
            "issue.submit",
            input_ref.clone(),
        );
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();

        let task_id = task.task_id;
        let invocation = native_invocation(task, task_invocation.clone(), input.clone());
        let operation = handler.execute(invocation).await.unwrap();
        assert_eq!(operation.state, OperationState::Resolved);
        assert_eq!(operation.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(operation.effect_status, EffectStatus::Complete);

        // The real outbox artifact: exactly one digest-named draft file.
        let drafts: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(drafts.len(), 1);
        let draft: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&drafts[0]).unwrap()).unwrap();
        assert_eq!(draft["digest"], frozen.digest);
        assert_eq!(draft["issue"]["title"], "quota probe timed out under load");
        assert_eq!(draft["audience"], "local_outbox");

        let evidence = store.external_operation_evidence(operation.operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let normalized = evidence[0].normalized_ref.as_ref().unwrap();
        let artifact_bytes = store.cas.read_object(&normalized.digest).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&artifact_bytes).unwrap(), draft);

        // Reconciliation of the resolved record is a replay of durable truth,
        // not another write; an in-flight re-observe also dedupes by digest.
        let task = store.external_task(task_id).unwrap();
        let again = handler.reconcile(&task, operation.operation_id).await.unwrap();
        assert_eq!(again.state, OperationState::Resolved);
        assert_eq!(store.external_operation_evidence(operation.operation_id).unwrap().len(), 1);
        let drafts_after: Vec<_> = std::fs::read_dir(&outbox)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(drafts_after.len(), 1);

        // A new operation must not attest valid JSON whose content was changed
        // under the old digest-named path. Historical receipts remain history.
        let mut tampered = draft;
        tampered["issue"]["title"] = serde_json::json!("a different submission");
        let tampered_bytes = serde_json::to_vec(&tampered).unwrap();
        std::fs::write(&drafts_after[0], &tampered_bytes).unwrap();
        let (new_task, new_invocation) = running_task(
            &store,
            handler.descriptor().implementation_ref,
            agent,
            step_id,
            "issue.submit",
            input_ref,
        );
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(new_task.clone());
        store.commit(batch).unwrap();
        let invocation = native_invocation(new_task, new_invocation, input);
        let rejected_id = invocation.operation_id;
        assert!(handler.execute(invocation).await.is_err());
        let rejected = store.external_operation(rejected_id).unwrap();
        assert_ne!(rejected.outcome, Some(OperationOutcome::Succeeded));
        assert_ne!(rejected.effect_status, EffectStatus::Complete);
        assert!(store.external_operation_evidence(rejected_id).unwrap().is_empty());
        assert_eq!(std::fs::read(&drafts_after[0]).unwrap(), tampered_bytes);
        drop(handler);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn errors_read_armed_recovery_observes_original_attempt() {
        let (dir, store) = open_store("errors-read-recovery");
        let (agent, step_id) = subject_fixture(&store);
        let session_id = EntityId(store.load_subject(agent).unwrap().0.branch_id.0);
        let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
        let handler = ErrorsReadHandler::new(store.clone());
        let frozen = handler.freeze(serde_json::json!({"op":"list","limit":5}), session_id).unwrap();
        let input_ref = handler.seal(&frozen, "errors-read-frozen-input").unwrap();
        let (task, task_invocation) = running_task(
            &store,
            handler.descriptor().implementation_ref,
            agent,
            step_id,
            "errors.read",
            input_ref.clone(),
        );
        // The real journal write creates the raw session parent and the fact.
        let event_id = store
            .record_runtime_error(
                agent,
                shenko_store::RuntimeErrorContext {
                    kind: "subject_loop".into(),
                    source_module: "shenko_subject::loop_runner".into(),
                    message: "提交失败".into(),
                    step_no: Some(1),
                    task_id: None,
                    extra: serde_json::json!({}),
                },
            )
            .unwrap()
            .event_id;
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let invocation = native_invocation(task, task_invocation.clone(), serde_json::Value::Null);
        let (mut operation, mut attempt) = handler
            .operation_parts(&invocation, session_id, input_ref)
            .unwrap();
        assert!(
            store
                .arm_external_operation(&task_invocation, &mut operation, &mut attempt)
                .unwrap()
                .is_none()
        );
        let attempt_id = attempt.attempt_id;
        drop(handler);
        drop(store);

        // Crash between arm and observation: recovery re-observes the SAME
        // armed attempt and resolves once.
        let reopened = Arc::new(Store::open(db, cas).unwrap());
        let recovered = ErrorsReadHandler::new(reopened.clone());
        let task = reopened.external_task(task_invocation.task_id).unwrap();
        let resolved = recovered.reconcile(&task, operation.operation_id).await.unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(
            reopened
                .external_operation_attempt(operation.operation_id)
                .unwrap()
                .attempt_id,
            attempt_id
        );
        let evidence = reopened.external_operation_evidence(operation.operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let normalized = evidence[0].normalized_ref.as_ref().unwrap();
        let result: serde_json::Value =
            serde_json::from_slice(&reopened.cas.read_object(&normalized.digest).unwrap()).unwrap();
        assert_eq!(result["events"][0]["event_id"], event_id.to_string());
        let settled = recovered.reconcile(&task, operation.operation_id).await.unwrap();
        assert_eq!(settled.state, OperationState::Resolved);
        assert_eq!(reopened.external_operation_evidence(operation.operation_id).unwrap().len(), 1);
        drop(recovered);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }
}
