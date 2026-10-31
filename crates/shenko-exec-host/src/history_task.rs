//! Read-only native history queries over the caller's committed raw session.
//! The session is derived from the subject task, never accepted as model input.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::{SessionEventFilter, Store, StoredSessionEvent};
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

const CONTRACT: &[u8] = b"shenko.history.query/v1: immutable own-subject raw session reads; no external effects; bounded linear literal-match previews";
const MAX_QUERY_BYTES: usize = 4096;
const MAX_FILTER_VALUES: usize = 8;
const MAX_FILTER_VALUE_BYTES: usize = 64;
const MAX_LIMIT: usize = 50;
const MAX_PREVIEW_CHARS: usize = 400;
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoryOp {
    Search,
    List,
    Read,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryInput {
    op: HistoryOp,
    query: Option<String>,
    event_id: Option<EntityId>,
    seq: Option<i64>,
    #[serde(default)]
    from_seq: i64,
    to_seq: Option<i64>,
    limit: Option<usize>,
    #[serde(default)]
    event_types: Vec<String>,
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum FrozenQuery {
    Search {
        query: String,
        filter: SessionEventFilter,
    },
    List {
        filter: SessionEventFilter,
    },
    ReadEvent {
        event_id: EntityId,
    },
    ReadSeq {
        seq: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    schema_version: u32,
    session_id: EntityId,
    query: FrozenQuery,
}

pub struct HistoryHandler {
    store: Arc<Store>,
}

impl HistoryHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "8f0dc86b-19b4-46f7-a3a5-cb7235bdcf2a"
            .parse()
            .expect("fixed history plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-history-reads",
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

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, value: serde_json::Value, session_id: EntityId) -> Result<FrozenInput> {
        let HistoryInput {
            op,
            query,
            event_id,
            seq,
            from_seq,
            to_seq,
            limit,
            mut event_types,
            mut roles,
        } = serde_json::from_value(value)?;
        ensure!(from_seq >= 0, "from_seq must be non-negative");
        if let Some(to_seq) = to_seq {
            ensure!(to_seq >= from_seq, "to_seq must be at least from_seq");
        }
        let limit_was_specified = limit.is_some();
        let limit = limit.unwrap_or(20);
        ensure!((1..=MAX_LIMIT).contains(&limit), "limit must be 1..=50");
        for values in [&event_types, &roles] {
            ensure!(values.len() <= MAX_FILTER_VALUES, "too many filter values");
            ensure!(
                values.iter().all(|value| {
                    !value.trim().is_empty() && value.len() <= MAX_FILTER_VALUE_BYTES
                }),
                "filter values must be non-empty and at most 64 bytes"
            );
        }
        event_types.sort();
        event_types.dedup();
        roles.sort();
        roles.dedup();
        let filter = SessionEventFilter {
            from_seq,
            to_seq: to_seq.unwrap_or(i64::MAX),
            limit,
            event_types,
            roles,
        };
        let query = match op {
            HistoryOp::Search => {
                ensure!(
                    event_id.is_none() && seq.is_none(),
                    "search does not accept an event pointer"
                );
                let query = query.context("search requires query")?;
                let query = query.trim();
                ensure!(!query.is_empty(), "search query must not be empty");
                ensure!(
                    query.len() <= MAX_QUERY_BYTES,
                    "search query exceeds 4096 bytes"
                );
                FrozenQuery::Search {
                    query: query.into(),
                    filter,
                }
            }
            HistoryOp::List => {
                ensure!(
                    query.is_none() && event_id.is_none() && seq.is_none(),
                    "list accepts only range and filter fields"
                );
                FrozenQuery::List { filter }
            }
            HistoryOp::Read => {
                ensure!(
                    query.is_none()
                        && from_seq == 0
                        && to_seq.is_none()
                        && !limit_was_specified
                        && filter.event_types.is_empty()
                        && filter.roles.is_empty(),
                    "read accepts exactly one event_id or seq, without search filters"
                );
                match (event_id, seq) {
                    (Some(event_id), None) => FrozenQuery::ReadEvent { event_id },
                    (None, Some(seq)) => {
                        ensure!(seq >= 0, "seq must be non-negative");
                        FrozenQuery::ReadSeq { seq }
                    }
                    _ => anyhow::bail!("read requires exactly one of event_id or seq"),
                }
            }
        };
        Ok(FrozenInput {
            schema_version: 1,
            session_id,
            query,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "history_frozen_input_mismatch"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        ensure!(
            frozen.schema_version == 1,
            "unsupported_history_input_schema"
        );
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        session_id: EntityId,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.session_events.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "read_only_own_subject_history".into(),
            extra: serde_json::json!({"session_id": session_id}),
        };
        let resource = self.seal(&bindings, "history-query-binding")?;
        let acceptance = self.seal(
            &"a bounded chronological read of immutable raw rows, or an explicit read failure",
            "history-query-acceptance",
        )?;
        let retry = self.seal(
            &"re-read only the frozen immutable range under the original armed attempt; never arm again",
            "history-query-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:history-query-v1".into(),
            },
            symbol: "history.query.execute".into(),
            schema: SchemaRef {
                name: "history-query-observation-v1".into(),
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
            operation_name: "history.query.v1".into(),
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

    fn query(&self, frozen: &FrozenInput) -> Result<(serde_json::Value, OperationOutcome)> {
        match &frozen.query {
            FrozenQuery::Search { query, filter } => {
                let events = self
                    .store
                    .search_session_events(frozen.session_id, query, filter)?;
                Ok((
                    collection_result("search", frozen.session_id, filter, events, Some(query)),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::List { filter } => {
                let events = self.store.list_session_events(frozen.session_id, filter)?;
                Ok((
                    collection_result("list", frozen.session_id, filter, events, None),
                    OperationOutcome::Succeeded,
                ))
            }
            FrozenQuery::ReadEvent { event_id } => {
                let event = self
                    .store
                    .session_event(*event_id)?
                    .filter(|event| event.session_id == frozen.session_id);
                Ok(read_result(frozen.session_id, *event_id, None, event))
            }
            FrozenQuery::ReadSeq { seq } => {
                let event = self
                    .store
                    .session_events_range(frozen.session_id, *seq, *seq)?
                    .into_iter()
                    .next();
                Ok(read_result(
                    frozen.session_id,
                    EntityId::nil(),
                    Some(*seq),
                    event,
                ))
            }
        }
    }

    fn observe(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        frozen: &FrozenInput,
    ) -> Result<OperationRecord> {
        let (result, mut outcome) = self.query(frozen)?;
        let bytes = serde_json::to_vec(&result)?;
        let raw = self.seal_bytes(&bytes, "history-query-observation")?;
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
                "history-query-result",
            )?
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "history.query.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("raw-history:{}", attempt.attempt_id)),
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
            invocation.task.kind == "history.query" && invocation.task.definition_version == 1,
            "history_task_identity_mismatch"
        );
        let (head, _) = self.store.load_subject(invocation.task.agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("history_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("history_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(
            std::mem::take(&mut invocation.input),
            EntityId(head.branch_id.0),
        )?;
        let input_ref = self.seal(&frozen, "history-query-frozen-input")?;
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
impl NativeTaskHandler for HistoryHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "history.query".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-history-reader/1".into(),
            authority_refs: vec!["store.session_events.read.own_subject/v1".into()],
            prompt_section: Some(
                "history.query/v1 是已安装的只读原始历史 child。搜索：[delegate:history.query:1] {\"op\":\"search\",\"query\":\"深刻\",\"from_seq\":0,\"limit\":20}；列事件用 op=list；按搜索结果重读用 op=read 且只给一个 event_id 或 seq。可选 event_types/roles 数组过滤。含中文的 query（包括中英混合）按完整字面子串匹配，不是多词语义搜索；没有命中只说明该字串未匹配，可分别搜索更短的原文字面词，不证明相关事实不存在。结果按 seq 升序，经后续真实 task_result 返回；search/list 只有有界 data_preview，search 尽量展示匹配位置，截断标志和起始位置说明预览并非全文；起点是原始 data 的 Unicode 标量位置，不是字节或 UTF-16 偏移。压缩画面、当前片段或预览没显示某内容不证明原文没有；需要核实而当前证据不足时可先查询，需要完整正文时使用 read，仍不能确认就如实说明而不是断言原文不存在。seq 和 event_id 是历史行定位，不是材料自身的标题或编号；不得把 seq、event_id 或查询结果序号当作“第N份”“第N条”等作者编号，材料编号只能来自正文；未看到来源标注就重读核实或不提，不得推断。回复是否已送达提问者、是否出现在时间线、控制台或界面，只能依据真实投递/可见性回执；没有此类回执时如实说明尚未确认送达，不得声称对方已经看到。不得把尚未返回的回忆当成已核实事实。"
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
            operation.task_id == task.task_id && operation.operation_name == "history.query.v1",
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

fn event_ref(event: &StoredSessionEvent) -> String {
    format!(
        "session:{}/event:{}/seq:{}",
        event.session_id, event.event_id, event.seq
    )
}

// FTS/LIKE can match ASCII case-insensitively. KMP keeps this linear even for
// long repeated inputs; the bounded prefix table never allocates on the heap.
fn ascii_match_offset(text: &str, query: &str) -> Option<usize> {
    let pattern = query.as_bytes();
    if pattern.is_empty() || pattern.len() > MAX_QUERY_BYTES || !query.is_ascii() {
        return None;
    }
    let mut prefix = [0_u16; MAX_QUERY_BYTES];
    for i in 1..pattern.len() {
        let mut matched = usize::from(prefix[i - 1]);
        while matched > 0 && !pattern[i].eq_ignore_ascii_case(&pattern[matched]) {
            matched = usize::from(prefix[matched - 1]);
        }
        if pattern[i].eq_ignore_ascii_case(&pattern[matched]) {
            matched += 1;
        }
        prefix[i] = matched as u16; // at most MAX_QUERY_BYTES - 1
    }
    let mut matched = 0;
    for (index, byte) in text.bytes().enumerate() {
        while matched > 0 && !byte.eq_ignore_ascii_case(&pattern[matched]) {
            matched = usize::from(prefix[matched - 1]);
        }
        if byte.eq_ignore_ascii_case(&pattern[matched]) {
            matched += 1;
        }
        if matched == pattern.len() {
            return Some(index + 1 - pattern.len());
        }
    }
    None
}

fn event_summary(event: &StoredSessionEvent, query: Option<&str>) -> serde_json::Value {
    // The store can return FTS matches without a byte-identical substring.
    // Center only a literal (or ASCII case-insensitive) match we can locate;
    // otherwise retain the prefix and explicitly report that no match is shown.
    let matched_byte = query.filter(|query| !query.is_empty()).and_then(|query| {
        event.data.find(query).or_else(|| ascii_match_offset(&event.data, query))
    });
    let matched_char = matched_byte.map(|offset| event.data[..offset].chars().count());
    let query_chars = query.map_or(0, |query| query.chars().count());
    let left_context = MAX_PREVIEW_CHARS.saturating_sub(query_chars).min(100);
    let start = matched_char.map_or(0, |offset| offset.saturating_sub(left_context));
    let mut chars = event.data.chars().skip(start);
    let preview: String = chars.by_ref().take(MAX_PREVIEW_CHARS).collect();
    let truncated_before = start > 0;
    let truncated_after = chars.next().is_some();
    let match_visible = matched_char.is_some_and(|offset| {
        offset - start + query_chars <= preview.chars().count()
    });
    serde_json::json!({
        "event_ref": event_ref(event),
        "event_id": event.event_id,
        "session_id": event.session_id,
        "seq": event.seq,
        "event_type": event.event_type,
        "role": event.role,
        "data_preview": preview,
        "data_preview_start_char": start,
        "data_preview_truncated": truncated_before || truncated_after,
        "data_preview_truncated_before": truncated_before,
        "data_preview_truncated_after": truncated_after,
        "data_preview_match_visible": match_visible,
    })
}

fn collection_result(
    op: &str,
    session_id: EntityId,
    filter: &SessionEventFilter,
    events: Vec<StoredSessionEvent>,
    query: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "status": "ok",
        "op": op,
        "session_id": session_id,
        "range": {"from_seq": filter.from_seq, "to_seq": filter.to_seq},
        "events": events.iter().map(|event| event_summary(event, query)).collect::<Vec<_>>(),
    })
}

fn read_result(
    session_id: EntityId,
    event_id: EntityId,
    seq: Option<i64>,
    event: Option<StoredSessionEvent>,
) -> (serde_json::Value, OperationOutcome) {
    let Some(event) = event else {
        return (
            serde_json::json!({
                "schema_version": 1,
                "status": "failed",
                "op": "read",
                "session_id": session_id,
                "requested": {"event_id": (event_id != EntityId::nil()).then_some(event_id), "seq": seq},
                "error": {"code": "event_not_found", "message": "no such event in this subject's raw session"}
            }),
            OperationOutcome::Failed,
        );
    };
    (
        serde_json::json!({
            "schema_version": 1,
            "status": "ok",
            "op": "read",
            "event": {
                "event_ref": event_ref(&event),
                "event_id": event.event_id,
                "session_id": event.session_id,
                "seq": event.seq,
                "event_type": event.event_type,
                "role": event.role,
                "data": event.data,
            }
        }),
        OperationOutcome::Succeeded,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_store::TaskInvocation;
    use shenko_types::agent::CallContext;
    use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};
    use shenko_types::task::{DispatchEligibility, RunMode, TaskOwner, TaskState};

    #[test]
    fn search_preview_exposes_late_match_and_preserves_raw_read() {
        let dir = std::env::temp_dir().join(format!("shenko-history-preview-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let handler = HistoryHandler::new(store.clone());
        let (session_id, agent_id) = (EntityId::new(), EntityId::new());
        let data = serde_json::json!({"text": format!("{}青铜仪器编号 P6O0-l1I-283{}", "前".repeat(600), "后".repeat(500))}).to_string();
        let ascii = serde_json::json!({"text": format!("{} NEEdLe {}", "before ".repeat(100), "after ".repeat(100))}).to_string();
        let normalized = serde_json::json!({"text": format!("{} Café {}", "before ".repeat(100), "after ".repeat(100))}).to_string();
        let long_match = serde_json::json!({"text": format!("{} {} {}", "before ".repeat(100), "A".repeat(600), "after ".repeat(100))}).to_string();
        let mut batch = WriteBatch::new();
        batch.raw_session_upserts.push(RawSession {
            session_id, agent_id, started_at: "2026-10-26T00:00:00Z".into(), data: "{}".into(),
        });
        for (seq, body) in [(1, data.clone()), (2, ascii), (3, normalized), (4, long_match)] {
            batch.session_event_upserts.push(RawSessionEvent {
                event_id: EntityId::new(), session_id, seq, event_type: "user".into(), role: "user".into(), data: body,
            });
        }
        store.commit(batch).unwrap();
        let query = |input| handler.query(&handler.freeze(input, session_id).unwrap()).unwrap().0;
        let found = query(serde_json::json!({"op":"search", "query":"青铜仪器"}));
        let event = &found["events"][0];
        let preview = event["data_preview"].as_str().unwrap();
        let start = event["data_preview_start_char"].as_u64().unwrap() as usize;
        assert_eq!(event["seq"], 1);
        assert!(preview.contains("青铜仪器编号 P6O0-l1I-283"));
        assert!(preview.chars().count() <= MAX_PREVIEW_CHARS);
        assert_eq!(preview, data.chars().skip(start).take(MAX_PREVIEW_CHARS).collect::<String>());
        assert_eq!(event["data_preview_match_visible"], true);
        assert_eq!(event["data_preview_truncated_before"], true);
        assert_eq!(event["data_preview_truncated_after"], true);

        let list = query(serde_json::json!({"op":"list", "limit":1}));
        assert_eq!(list["events"][0]["data_preview_start_char"], 0);
        assert!(!list["events"][0]["data_preview"].as_str().unwrap().contains("青铜仪器"));
        let read = query(serde_json::json!({"op":"read", "seq":1}));
        assert_eq!(read["event"]["data"], data);

        let ascii_match = query(serde_json::json!({"op":"search", "query":"needle"}));
        assert!(ascii_match["events"][0]["data_preview"].as_str().unwrap().contains("NEEdLe"));
        assert_eq!(ascii_match["events"][0]["data_preview_match_visible"], true);
        // FTS can match after accent normalization without any literal span.
        let folded = query(serde_json::json!({"op":"search", "query":"cafe"}));
        assert_eq!(folded["events"][0]["data_preview_match_visible"], false);
        assert_eq!(folded["events"][0]["data_preview_start_char"], 0);
        let long = query(serde_json::json!({"op":"search", "query":"a".repeat(600)}));
        assert_eq!(long["events"][0]["seq"], 4);
        assert_eq!(long["events"][0]["data_preview_match_visible"], false);
        assert_eq!(long["events"][0]["data_preview"].as_str().unwrap().chars().count(), MAX_PREVIEW_CHARS);
        drop(handler);
        drop(store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn armed_history_query_recovers_on_original_attempt_without_duplicate_evidence() {
        let dir = std::env::temp_dir().join(format!("shenko-history-recovery-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
        let (session_id, agent_id, event_id) = (EntityId::new(), EntityId::new(), EntityId::new());
        let store = Arc::new(Store::open(db.clone(), cas.clone()).unwrap());
        let handler = HistoryHandler::new(store.clone());
        let frozen = handler
            .freeze(
                serde_json::json!({"op":"search","query":"深刻"}),
                session_id,
            )
            .unwrap();
        let input_ref = handler.seal(&frozen, "history-query-frozen-input").unwrap();
        let (task_id, operation_id, invocation_id) =
            (EntityId::new(), EntityId::new(), EntityId::new());
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task {
                id: EntityId::new(),
            },
            background: false,
            kind: "history.query".into(),
            definition_version: 1,
            implementation_ref: handler.descriptor().implementation_ref,
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
        batch.raw_session_upserts.push(RawSession {
            session_id,
            agent_id,
            started_at: "2026-10-26T00:00:00Z".into(),
            data: "{}".into(),
        });
        batch.session_event_upserts.push(RawSessionEvent {
            event_id,
            session_id,
            seq: 1,
            event_type: "user".into(),
            role: "user".into(),
            data: serde_json::json!({"v":1,"text":"深刻原文"}).to_string(),
        });
        batch.task_upserts.push(task.clone());
        store.commit(batch).unwrap();
        let task_invocation = TaskInvocation {
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        let mut context = CallContext::new_system(RuntimeGeneration::default());
        context.task_id = Some(task_id);
        context.invocation_id = Some(invocation_id);
        context.controller_epoch = epoch;
        let invocation = NativeTaskInvocation {
            context,
            task_invocation: task_invocation.clone(),
            task,
            operation_id,
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: EntityId::new(),
            },
            input: serde_json::Value::Null,
        };
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
        assert_eq!(operation.state, OperationState::InFlight);
        assert!(
            store
                .external_operation_evidence(operation_id)
                .unwrap()
                .is_empty()
        );

        // Deterministic crash point: T1 is durable, no observation/E1 exists.
        drop(handler);
        drop(store);
        let reopened = Arc::new(Store::open(db, cas).unwrap());
        let recovered = HistoryHandler::new(reopened.clone());
        let task = reopened.external_task(task_id).unwrap();
        let resolved = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert_eq!(resolved.outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(resolved.effect_status, EffectStatus::None);
        assert_eq!(
            reopened
                .external_operation_attempts(operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_attempt(operation_id)
                .unwrap()
                .attempt_id,
            attempt_id
        );
        let evidence = reopened.external_operation_evidence(operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let normalized = evidence[0].normalized_ref.as_ref().unwrap();
        let result: serde_json::Value =
            serde_json::from_slice(&reopened.cas.read_object(&normalized.digest).unwrap()).unwrap();
        assert_eq!(
            result
                .pointer("/events/0/event_id")
                .and_then(serde_json::Value::as_str),
            Some(&*event_id.to_string())
        );
        assert!(
            result
                .pointer("/events/0/data_preview")
                .and_then(serde_json::Value::as_str)
                .unwrap()
                .contains("深刻原文")
        );

        // Settlement replay reuses E1 instead of adding another observation.
        let settled = recovered.reconcile(&task, operation_id).await.unwrap();
        assert_eq!(settled.state, OperationState::Resolved);
        assert_eq!(
            reopened
                .external_operation_attempts(operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_evidence(operation_id)
                .unwrap()
                .len(),
            1
        );
        drop(recovered);
        drop(reopened);
        let _ = std::fs::remove_dir_all(dir);
    }
}
