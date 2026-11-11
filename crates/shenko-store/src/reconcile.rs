//! 效果未知的对账案例 — docs/specs/durable-state.md#spec-41-4 X11（X04/X06/U/X07 的交叉义务）。
//!
//! 一笔 operation 一旦进入 `reconciling`，它的外部效果就可能已经发生、
//! 也可能没有发生；恢复器从不猜测，也从不自动重发。真正需要一条持久
//! 记录的是**这份不确定性本身**：它由哪几份原始证据支持、谁来处理、
//! 以及主体是否已经明确决定“带着这份不确定性继续”。
//!
//! 这里只有一份表示：规范记录 `ReconciliationCase` 存在自己的表里，
//! case 的 `decision_ref` 直接指向那个 sealed `StepDecision` 对象（与
//! `subject_obligations` 完全相同的引用方式），因此没有第二条决定日志，
//! 也没有另一个“重试队列”。
//!
//! 三条边界：
//!
//! * **不改写成功**：`acknowledged_unknown` 只改 case 状态；operation 的
//!   `state`/`effect_status`/`outcome` 和所有原始 receipt 一律不动。
//! * **不改写历史**：已经 sealed 的 receipt、已经提交的 attempt 阶段
//!   都保留原样；后来出现的确定证据把 case 推进到 `resolved`，并追加
//!   它引用的那份 receipt，旧 case 行本身不被删除。
//! * **导入的 peer provenance 不授予权威**：case 是本地的协调记录，
//!   带本地 `CommitRef`，不进入 cognition archive 的权威集合；对端可以
//!   从同一份不可变证据在本地重新开启自己的 case，但不能替本地主体
//!   接受未知。

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::evidence::{ReconciliationCase, ReconciliationState};
use shenko_types::ids::EntityId;
use shenko_types::operation::{EffectStatus, OperationOutcome, OperationRecord, OperationState};

use crate::store::Store;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS reconciliation_cases(
 case_id TEXT PRIMARY KEY,
 operation_id TEXT NOT NULL,
 agent_id TEXT,
 supervisor_task_id TEXT NOT NULL,
 state TEXT NOT NULL,
 data TEXT NOT NULL,
 created_commit_seq INTEGER NOT NULL,
 updated_commit_seq INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS reconciliation_cases_operation
 ON reconciliation_cases(operation_id, created_commit_seq);
CREATE INDEX IF NOT EXISTS reconciliation_cases_docket
 ON reconciliation_cases(agent_id, state, created_commit_seq, case_id);
"#;

fn storage(error: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn parse_entity_id(value: &str) -> Result<EntityId, ShenkoError> {
    value.parse().map_err(storage)
}

/// 一份待主体判断的对账案例，连同判断所需的 operation 事实。
///
/// 这里刻意不带 operation 的 input/参数正文：那是一段可能含私人内容的
/// 载荷，判读只需要知道是哪个 operation、效果处于什么状态、证据有几份。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationDocketEntry {
    pub case: ReconciliationCase,
    pub operation_name: String,
    pub operation_state: OperationState,
    pub effect_status: EffectStatus,
    pub outcome: Option<OperationOutcome>,
}

fn case_state(value: ReconciliationState) -> &'static str {
    match value {
        ReconciliationState::Open => "open",
        ReconciliationState::Working => "working",
        ReconciliationState::Waiting => "waiting",
        ReconciliationState::Resolved => "resolved",
        ReconciliationState::AcknowledgedUnknown => "acknowledged_unknown",
    }
}

/// The states a case can still be decided in.
fn undecided(state: &str) -> bool {
    matches!(state, "open" | "working" | "waiting")
}

impl Store {
    /// 用同一笔事务里刚刚变成 `reconciling` 的 operation 建立/更新案例。
    ///
    /// 调用者已经知道 effect 现在未知；这里只把它写成可判读的事实：
    /// 哪个 operation、受哪个 durable supervisor task 约束、由哪份原始
    /// receipt 支持。同一 operation 已有一个未决 case 时复用并追加证据
    /// （重复的 receipt 不重复追加）；已经 acknowledged 或 resolved 的
    /// case 是历史事实，不再改写——新证据开出新的一行。
    pub(crate) fn ensure_unknown_effect_case(
        &self,
        tx: &Transaction<'_>,
        operation: &OperationRecord,
        receipt_id: EntityId,
        reason: &str,
        commit: CommitRef,
    ) -> Result<EntityId, ShenkoError> {
        let latest: Option<(String, String, String)> = tx
            .query_row(
                "SELECT case_id,state,data FROM reconciliation_cases
                 WHERE operation_id=?1 ORDER BY created_commit_seq DESC, rowid DESC LIMIT 1",
                params![operation.operation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage)?;
        match latest {
            Some((case_id, state, data)) if undecided(&state) => {
                let mut case: ReconciliationCase = serde_json::from_str(&data)?;
                if !case.evidence_refs.contains(&receipt_id) {
                    case.evidence_refs.push(receipt_id);
                }
                case.updated_commit = commit;
                tx.execute(
                    "UPDATE reconciliation_cases SET state=?2,data=?3,updated_commit_seq=?4
                     WHERE case_id=?1",
                    params![
                        case_id,
                        case_state(case.state),
                        serde_json::to_string(&case)?,
                        commit.seq.0 as i64
                    ],
                )
                .map_err(storage)?;
                Ok(parse_entity_id(&case_id)?)
            }
            _ => {
                let case = ReconciliationCase {
                    case_id: EntityId::new(),
                    operation_id: operation.operation_id,
                    supervisor_task_id: operation.task_id,
                    reason: reason.to_owned(),
                    state: ReconciliationState::Open,
                    evidence_refs: vec![receipt_id],
                    next_action_ref: None,
                    decision_ref: None,
                    created_commit: commit,
                    updated_commit: commit,
                };
                // The agent is a projection of the operation's durable owner
                // task, not a caller claim. An operation without a readable
                // owner task keeps an unbound case: no subject docket will
                // show it, and nobody can acknowledge it on that subject's
                // behalf.
                let agent_id: Option<String> = tx
                    .query_row(
                        "SELECT json_extract(data,'$.agent_id') FROM tasks WHERE task_id=?1",
                        params![operation.task_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage)?
                    .flatten();
                tx.execute(
                    "INSERT INTO reconciliation_cases
                        (case_id,operation_id,agent_id,supervisor_task_id,state,data,
                         created_commit_seq,updated_commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
                    params![
                        case.case_id.to_string(),
                        case.operation_id.to_string(),
                        agent_id,
                        case.supervisor_task_id.to_string(),
                        case_state(case.state),
                        serde_json::to_string(&case)?,
                        commit.seq.0 as i64
                    ],
                )
                .map_err(storage)?;
                Ok(case.case_id)
            }
        }
    }

    /// 确定证据落下时给该 operation 的对账收尾。
    ///
    /// `resolved` 只在结果和实际效果按合同已有明确证据时成立：调用者
    /// 只在 operation 真的变成 `Resolved` 且 effect 已知时才调用本函数。
    ///
    /// 尚未被主体判断过的案例（open/working/waiting）就在本行上走到
    /// `resolved`：它没有可供保留的决定，只是得到了答案。已经被主体
    /// 显式接受为未知、或已经 resolved 的案例**不被改写**（docs/specs/cognition-units.md#spec-51-1
    /// 旧读法不覆盖）；确定证据为它们追加一行新的 resolved 案例，
    /// 说明“答案后来到了”。原始 receipt 一行都不删。
    pub(crate) fn resolve_unknown_effect_cases(
        &self,
        tx: &Transaction<'_>,
        operation_id: EntityId,
        receipt_id: EntityId,
        commit: CommitRef,
    ) -> Result<usize, ShenkoError> {
        let rows: Vec<(String, String, String)> = {
            let mut statement = tx
                .prepare(
                    "SELECT case_id,state,data FROM reconciliation_cases WHERE operation_id=?1
                     ORDER BY created_commit_seq DESC, rowid DESC LIMIT 1",
                )
                .map_err(storage)?;
            statement
                .query_map(params![operation_id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(storage)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(storage)?
        };
        let Some((case_id, state, data)) = rows.into_iter().next() else {
            // Definite evidence for an operation that never had a case: the
            // effect was never unknown, so there is nothing to close.
            return Ok(0);
        };
        if undecided(&state) {
            let mut case: ReconciliationCase = serde_json::from_str(&data)?;
            if !case.evidence_refs.contains(&receipt_id) {
                case.evidence_refs.push(receipt_id);
            }
            case.state = ReconciliationState::Resolved;
            case.updated_commit = commit;
            tx.execute(
                "UPDATE reconciliation_cases SET state='resolved',data=?2,updated_commit_seq=?3
                 WHERE case_id=?1",
                params![
                    case_id,
                    serde_json::to_string(&case)?,
                    commit.seq.0 as i64
                ],
            )
            .map_err(storage)?;
            return Ok(1);
        }
        if state == "acknowledged_unknown" {
            // The subject's acceptance stays exactly as committed. A new row
            // records that definite evidence later answered the same
            // operation, so both readings remain readable in order.
            let previous: ReconciliationCase = serde_json::from_str(&data)?;
            let resolved = ReconciliationCase {
                case_id: EntityId::new(),
                operation_id,
                supervisor_task_id: previous.supervisor_task_id,
                reason: REASON_DEFINITE_EVIDENCE.to_owned(),
                state: ReconciliationState::Resolved,
                evidence_refs: vec![receipt_id],
                next_action_ref: None,
                decision_ref: None,
                created_commit: commit,
                updated_commit: commit,
            };
            let agent_id: Option<String> = tx
                .query_row(
                    "SELECT agent_id FROM reconciliation_cases WHERE case_id=?1",
                    params![case_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(storage)?
                .flatten();
            tx.execute(
                "INSERT INTO reconciliation_cases
                    (case_id,operation_id,agent_id,supervisor_task_id,state,data,
                     created_commit_seq,updated_commit_seq)
                 VALUES(?1,?2,?3,?4,'resolved',?5,?6,?6)",
                params![
                    resolved.case_id.to_string(),
                    resolved.operation_id.to_string(),
                    agent_id,
                    resolved.supervisor_task_id.to_string(),
                    serde_json::to_string(&resolved)?,
                    commit.seq.0 as i64
                ],
            )
            .map_err(storage)?;
            return Ok(1);
        }
        // Already resolved: the answer is in; repeated evidence adds nothing.
        Ok(0)
    }

    /// 主体显式接受一份未知效果 — docs/specs/durable-state.md#spec-41-4 X11。
    ///
    /// 决定是一次已提交的 S1：`decision_ref` 就是那个 sealed 决定对象，
    /// 其中 `unknown_acknowledgements` 的那一项带着 `acknowledgement_id`
    /// 与剩余影响。这里只做 case 状态与引用；operation 仍是
    /// `reconciling`/`unknown`，所以没有把任何结果改写成成功。
    ///
    /// 已 resolved 的 case 拒绝（确定证据已经给出答案）；已 acknowledged
    /// 的 case 也拒绝（同一案例的第二次接受不是同一条事实，新的未决
    /// 证据会开出新的 case）。
    pub(crate) fn acknowledge_unknown_effect(
        &self,
        tx: &Transaction<'_>,
        agent_id: EntityId,
        acknowledgement: &shenko_types::evidence::ReconciliationAcknowledgement,
        decision_ref: &shenko_types::object::ObjectRef,
        commit: CommitRef,
    ) -> Result<(), ShenkoError> {
        let case_id = acknowledgement.case_id;
        let (owner, state, data): (Option<String>, String, String) = tx
            .query_row(
                "SELECT agent_id,state,data FROM reconciliation_cases WHERE case_id=?1",
                params![case_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(storage)?
            .ok_or_else(|| ShenkoError::conflict("reconciliation case not found"))?;
        let expected_owner = agent_id.to_string();
        if owner.as_deref() != Some(expected_owner.as_str()) {
            return Err(ShenkoError::conflict(
                "reconciliation case belongs to another subject",
            ));
        }
        if !undecided(&state) {
            return Err(ShenkoError::conflict(
                "reconciliation case is already decided",
            ));
        }
        let mut case: ReconciliationCase = serde_json::from_str(&data)?;
        if case.decision_ref.is_some() {
            return Err(ShenkoError::conflict(
                "reconciliation case already names a decision",
            ));
        }
        case.state = ReconciliationState::AcknowledgedUnknown;
        case.decision_ref = Some(decision_ref.clone());
        case.updated_commit = commit;
        tx.execute(
            "UPDATE reconciliation_cases SET state='acknowledged_unknown',data=?2,
                updated_commit_seq=?3 WHERE case_id=?1",
            params![
                case_id.to_string(),
                serde_json::to_string(&case)?,
                commit.seq.0 as i64
            ],
        )
        .map_err(storage)?;
        // The outbox projection stops recycling this intent as fresh
        // unknown work, but it is NOT settled: the effect is still unknown
        // and the receipt set stays exactly what it was.
        tx.execute(
            "UPDATE subject_intents SET state='acknowledged',revision=revision+1,updated_seq=?2
             WHERE intent_id=?1 AND kind='publication' AND state IN ('claimed','unknown')",
            params![case.operation_id.to_string(), commit.seq.0 as i64],
        )
        .map_err(storage)?;
        record_acknowledgement_attitude_tx(
            self,
            tx,
            commit,
            agent_id,
            decision_ref,
            acknowledgement,
        )?;
        Ok(())
    }

    /// 一个案例的规范记录。
    pub fn reconciliation_case(
        &self,
        case_id: EntityId,
    ) -> Result<Option<ReconciliationCase>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM reconciliation_cases WHERE case_id=?1",
                params![case_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        data.map(|data| serde_json::from_str(&data).map_err(ShenkoError::from))
            .transpose()
    }

    /// 全部案例，按创建顺序分页（观测/导出投影只读）。
    pub fn reconciliation_cases(
        &self,
        limit: usize,
    ) -> Result<Vec<ReconciliationCase>, ShenkoError> {
        let limit = i64::try_from(limit).map_err(storage)?;
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT data FROM reconciliation_cases
                 ORDER BY created_commit_seq, case_id LIMIT ?1",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![limit], |row| row.get::<_, String>(0))
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(ShenkoError::from))
            .collect()
    }

    /// 一个 operation 的案例历史，最早的在前。acknowledged/resolved 的
    /// 旧行保留，因此“先接受未知、后来收到确定证据”两件事都能读到。
    pub fn reconciliation_cases_for_operation(
        &self,
        operation_id: EntityId,
    ) -> Result<Vec<ReconciliationCase>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT data FROM reconciliation_cases WHERE operation_id=?1
                 ORDER BY created_commit_seq, case_id",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![operation_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        rows.into_iter()
            .map(|data| serde_json::from_str(&data).map_err(ShenkoError::from))
            .collect()
    }

    /// 该主体当前需要判断的对账案例。
    ///
    /// 只有仍未决的案例进入这份清单：已接受或已解决的案例是历史，不会
    /// 反复出现在主体的思考里。清单只读，不写、不唤醒、不重发。
    pub fn open_reconciliation_docket(
        &self,
        agent_id: EntityId,
        limit: usize,
    ) -> Result<Vec<ReconciliationDocketEntry>, ShenkoError> {
        let limit = i64::try_from(limit).map_err(storage)?;
        let conn = self.ledger.conn.lock();
        let mut statement = conn
            .prepare(
                "SELECT c.data, o.data FROM reconciliation_cases c
                 JOIN operations o ON o.operation_id=c.operation_id
                 WHERE c.agent_id=?1 AND c.state IN ('open','working','waiting')
                 ORDER BY c.created_commit_seq, c.case_id LIMIT ?2",
            )
            .map_err(storage)?;
        let rows = statement
            .query_map(params![agent_id.to_string(), limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        rows.into_iter()
            .map(|(case, operation)| {
                let case: ReconciliationCase =
                    serde_json::from_str(&case).map_err(ShenkoError::from)?;
                let operation: OperationRecord =
                    serde_json::from_str(&operation).map_err(ShenkoError::from)?;
                Ok(ReconciliationDocketEntry {
                    case,
                    operation_name: operation.operation_name,
                    operation_state: operation.state,
                    effect_status: operation.effect_status,
                    outcome: operation.outcome,
                })
            })
            .collect()
    }
}

/// The reasons a case can be opened for. They are fixed kinds, never a copy
/// of transport text: the raw error stays in the receipt it came with.
pub const REASON_EFFECT_UNKNOWN: &str = "external_effect_unknown";
pub const REASON_CONTRADICTORY_EVIDENCE: &str = "contradictory_terminal_evidence";
pub const REASON_DEFINITE_EVIDENCE: &str = "definite_evidence_after_unknown";

/// 把主体的接受写成规范账本里的 attitude（PR180 docs/specs/cognition-units.md#spec-51-1）。
///
/// 这是本片唯一的 canonical 写入点，也是与 Core 的唯一接缝：
///
/// * role 是 `EffectUnknown`（效应仍未知），不是伪造成功的 Assertion；
/// * reasons 停在**同一份冻结可见**的 case 与它当时携带的原始回执上
///   （`Reconciliation{case_id}` 加每份 receipt 的 `Receipt` 终止），
///   绝不引用这一步没看到的证据；
/// * confidence 对新写入显式给出 `Unknown`：主体没有给出数字，也不填
///   默认数字（旧行的 `None` 保持未知，不被改写）；
/// * 写入发生在 S1 自己的事务里，并使用那一笔的 sealed 决定 ObjectRef
///   （含长度与 codec），所以 case 状态、决定与 attitude 是同一笔提交，
///   不存在“先算 digest 再补一次 mutation”的第二步。
///
/// Core（AlignAtoms180）的事务内写入口按已公布契约：
/// `record_attitude_for_digest_tx(tx, agent_id, object: &ObjectRef,
/// draft: AttitudeDraft) -> Result<ContentAttitude>`。若最终命名不同，
/// 只需改这一处调用，不必碰 case 语义。
fn record_acknowledgement_attitude_tx(
    store: &Store,
    tx: &Transaction<'_>,
    commit: CommitRef,
    agent_id: EntityId,
    decision_ref: &shenko_types::object::ObjectRef,
    acknowledgement: &shenko_types::evidence::ReconciliationAcknowledgement,
) -> Result<(), ShenkoError> {
    use crate::atoms::AttitudeDraft;
    use shenko_types::cognition::{AttitudeConfidence, ReasonPointer};

    let mut reasons = vec![ReasonPointer::reconciliation(acknowledgement.case_id)];
    for receipt_id in &acknowledgement.reason_refs {
        // The observation level is read from the receipt itself, never
        // guessed: a reason claiming a stronger observation than the receipt
        // recorded would be a fabricated basis.
        let level: Option<shenko_types::evidence::ObservationLevel> = tx
            .query_row(
                "SELECT json_extract(data,'$.observation_level') FROM evidence_receipts
                 WHERE receipt_id=?1",
                params![receipt_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(storage)?
            .flatten()
            .and_then(|value| serde_json::from_str(&value).ok());
        reasons.push(ReasonPointer::receipt(*receipt_id, level));
    }
    let draft = AttitudeDraft::effect_unknown(reasons, AttitudeConfidence::Unknown);
    store.record_attitude_for_digest_tx(tx, commit, agent_id, decision_ref.clone(), draft)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::cognition::{
        AttitudeConfidence, AttitudeRole, ConfidenceState, ReasonReading,
    };
    use shenko_types::commit::CommitRef;
    use shenko_types::evidence::{ReconciliationAcknowledgement, ReconciliationState};
    use shenko_types::ids::*;
    use shenko_types::operation::*;

    fn test_store(name: &str) -> (std::path::PathBuf, Store) {
        let root =
            std::env::temp_dir().join(format!("shenko-reconcile-{name}-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(root.join("store.db"), root.join("cas")).unwrap();
        (root, store)
    }

    /// Seed a minimal task row so `ensure_unknown_effect_case` can look up the agent.
    fn seed_task(store: &Store, task_id: EntityId, agent_id: EntityId, commit: CommitRef) {
        let conn = store.ledger.conn.lock();
        let task_data = serde_json::json!({
            "agent_id": agent_id.to_string(),
        });
        conn.execute(
            "INSERT OR IGNORE INTO tasks(task_id,agent_id,kind,state,data,\
             created_commit_seq,updated_commit_seq,record_revision)
             VALUES(?1,?2,'publication','active',?3,?4,?4,0)",
            params![
                task_id.to_string(),
                agent_id.to_string(),
                task_data.to_string(),
                commit.seq.0 as i64,
            ],
        )
        .unwrap();
    }

    /// Seed a minimal operation row for the docket join.
    fn seed_operation(
        store: &Store,
        operation_id: EntityId,
        task_id: EntityId,
        dummy_ref: &shenko_types::object::ObjectRef,
        commit: CommitRef,
    ) {
        let op_data = serde_json::json!({
            "record_schema_version":1, "operation_id": operation_id.to_string(),
            "record_revision":0, "task_id": task_id.to_string(),
            "logical_key":"test/op", "operation_name":"test.send",
            "input_schema_version":1, "output_schema_version":1,
            "final_input_ref": dummy_ref, "input_digest": dummy_ref.digest.0,
            "contract_ref":{
                "plugin_id":task_id.to_string(),
                "artifact_digest":dummy_ref.digest.0,
                "contract_digest":dummy_ref.digest.0,
                "receipt_reader_ref":{
                    "artifact":{"digest":dummy_ref.digest.0,"locator":&dummy_ref.locator},
                    "symbol":"r","schema":&dummy_ref.codec
                }
            },
            "resource_bindings_ref": dummy_ref,
            "authority_ref":{
                "delegation_id":task_id.to_string(),
                "delegation_revision":0,
                "decision_ref":task_id.to_string()
            },
            "acceptance_predicate_ref": dummy_ref,
            "retry_contract_ref": dummy_ref,
            "checkpoint_scope_ref":null,
            "state":"reconciling", "outcome":null, "outcome_conflict":false,
            "effect_status":"unknown", "retry_of":null, "compensates":null,
            "possible_duplicate_of":[], "prepared_commit": commit,
            "evidence_through":null
        });
        let conn = store.ledger.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO operations(operation_id,task_id,state,data,\
             created_commit_seq,updated_commit_seq)
             VALUES(?1,?2,'reconciling',?3,?4,?4)",
            params![
                operation_id.to_string(),
                task_id.to_string(),
                op_data.to_string(),
                commit.seq.0 as i64,
            ],
        )
        .unwrap();
    }

    /// Seed an evidence receipt row.
    fn seed_receipt(store: &Store, receipt_id: EntityId, operation_id: EntityId) {
        let data = serde_json::json!({
            "receipt_id": receipt_id.to_string(),
            "operation_id": operation_id.to_string(),
            "observation_level": "direct"
        });
        let conn = store.ledger.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO evidence_receipts(receipt_id,operation_id,data,created_commit_seq)
             VALUES(?1,?2,?3,1)",
            params![receipt_id.to_string(), operation_id.to_string(), data.to_string()],
        )
        .unwrap();
    }

    fn dummy_operation(
        operation_id: EntityId,
        task_id: EntityId,
        dummy_ref: &shenko_types::object::ObjectRef,
        commit: CommitRef,
    ) -> OperationRecord {
        OperationRecord {
            record_schema_version: 1,
            operation_id,
            record_revision: RecordRevision(0),
            task_id,
            logical_key: "test/op".into(),
            operation_name: "test.send".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: dummy_ref.clone(),
            input_digest: dummy_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: task_id,
                artifact_digest: dummy_ref.digest.clone(),
                contract_digest: dummy_ref.digest.clone(),
                receipt_reader_ref: shenko_types::object::VersionRef {
                    artifact: shenko_types::object::ArtifactRef {
                        digest: dummy_ref.digest.clone(),
                        locator: dummy_ref.locator.clone(),
                    },
                    symbol: "r".into(),
                    schema: dummy_ref.codec.clone(),
                },
            },
            resource_bindings_ref: dummy_ref.clone(),
            authority_ref: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(0),
                decision_ref: task_id,
            },
            acceptance_predicate_ref: dummy_ref.clone(),
            retry_contract_ref: dummy_ref.clone(),
            checkpoint_scope_ref: None,
            state: OperationState::Reconciling,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: commit,
            evidence_through: None,
        }
    }

    /// Full lifecycle: open case → acknowledge → attitude written → definite evidence resolves.
    ///
    /// Exercises the docs/specs/durable-state.md#spec-41-4 X11 + docs/specs/cognition-units.md#spec-51-1 attitude seam:
    /// - Case opens via `ensure_unknown_effect_case`.
    /// - Docket shows the open case.
    /// - `acknowledge_unknown_effect` writes case + attitude atomically.
    /// - Attitude uses `EffectUnknown` role, `Reconciliation` + `Receipt` reasons.
    /// - Confidence is `Some(Unknown)` (explicitly unjudged).
    /// - Definite evidence appends a new resolved case (old acknowledged row intact).
    #[test]
    fn reconciliation_case_lifecycle_with_attitude_seam() {
        let (_root, store) = test_store("lifecycle");
        let agent = EntityId::new();
        let task_id = EntityId::new();
        let operation_id = EntityId::new();
        let receipt_id_1 = EntityId::new();
        let receipt_id_2 = EntityId::new();
        let ledger_id = store.ledger.ledger_id();
        let commit_1 = CommitRef::new(ledger_id, LedgerSeq(1));

        let dummy_ref = store
            .cas
            .seal_object(b"task-data", "application/json", "test", "raw", 1)
            .unwrap()
            .object;

        seed_task(&store, task_id, agent, commit_1);
        seed_operation(&store, operation_id, task_id, &dummy_ref, commit_1);
        seed_receipt(&store, receipt_id_1, operation_id);
        seed_receipt(&store, receipt_id_2, operation_id);

        // 1. Open a reconciliation case.
        let op_record = dummy_operation(operation_id, task_id, &dummy_ref, commit_1);
        let commit_2 = CommitRef::new(ledger_id, LedgerSeq(2));
        let case_id = {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let id = store
                .ensure_unknown_effect_case(
                    &tx,
                    &op_record,
                    receipt_id_1,
                    REASON_EFFECT_UNKNOWN,
                    commit_2,
                )
                .unwrap();
            tx.commit().unwrap();
            id
        };

        let case = store.reconciliation_case(case_id).unwrap().unwrap();
        assert_eq!(case.state, ReconciliationState::Open);
        assert_eq!(case.operation_id, operation_id);
        assert!(case.evidence_refs.contains(&receipt_id_1));
        assert!(case.decision_ref.is_none());

        // Docket shows the open case.
        let docket = store.open_reconciliation_docket(agent, 10).unwrap();
        assert_eq!(docket.len(), 1);
        assert_eq!(docket[0].case.case_id, case_id);
        assert_eq!(docket[0].operation_name, "test.send");

        // 2. Acknowledge the unknown effect.
        let decision_object = store
            .cas
            .seal_object(
                b"{\"decision\":\"acknowledge\"}",
                "application/json",
                "cognition",
                "shenko.cognition.content",
                1,
            )
            .unwrap()
            .object;
        let ack = ReconciliationAcknowledgement {
            case_id,
            acknowledgement_id: EntityId::new(),
            reason_refs: vec![receipt_id_1, receipt_id_2],
            remaining_impact: "effect uncertain, continuing anyway".into(),
        };
        let commit_3 = CommitRef::new(ledger_id, LedgerSeq(3));
        {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            store
                .acknowledge_unknown_effect(&tx, agent, &ack, &decision_object, commit_3)
                .unwrap();
            tx.commit().unwrap();
        }

        // Case is now acknowledged.
        let case = store.reconciliation_case(case_id).unwrap().unwrap();
        assert_eq!(case.state, ReconciliationState::AcknowledgedUnknown);
        assert!(case.decision_ref.is_some());
        assert_eq!(case.decision_ref.unwrap().digest, decision_object.digest);

        // Docket is empty (acknowledged = history, not fresh work).
        let docket = store.open_reconciliation_docket(agent, 10).unwrap();
        assert!(docket.is_empty());

        // 3. Verify the attitude was written.
        let attitudes = store.attitudes_for(agent, &decision_object.digest).unwrap();
        assert_eq!(attitudes.len(), 1, "exactly one attitude for the acknowledgement");
        let attitude = &attitudes[0];
        assert!(
            matches!(attitude.role, AttitudeRole::EffectUnknown),
            "role must be EffectUnknown, not a fake Assertion"
        );
        assert_eq!(
            attitude.confidence,
            Some(AttitudeConfidence::Unknown),
            "new writes carry explicit Unknown confidence"
        );
        assert_eq!(attitude.confidence_state(), ConfidenceState::Unknown);
        // Reasons: Reconciliation(case_id) + Receipt(receipt_id_1) + Receipt(receipt_id_2).
        assert_eq!(attitude.reasons.len(), 3);
        assert_eq!(
            attitude.reading(),
            ReasonReading::Fact,
            "all reason terminals are fact-side"
        );
        assert_eq!(attitude.agent_id, agent);

        // 4. Double acknowledgement is refused (append-only).
        {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let result = store.acknowledge_unknown_effect(
                &tx,
                agent,
                &ack,
                &decision_object,
                CommitRef::new(ledger_id, LedgerSeq(4)),
            );
            assert!(result.is_err(), "double acknowledgement must be refused");
        }

        // 5. Definite evidence opens a NEW resolved case; old acknowledged stays.
        let definite_receipt = EntityId::new();
        let commit_5 = CommitRef::new(ledger_id, LedgerSeq(5));
        {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let resolved = store
                .resolve_unknown_effect_cases(&tx, operation_id, definite_receipt, commit_5)
                .unwrap();
            assert_eq!(resolved, 1);
            tx.commit().unwrap();
        }

        let history = store.reconciliation_cases_for_operation(operation_id).unwrap();
        assert_eq!(history.len(), 2, "acknowledged + resolved are both history");
        assert_eq!(history[0].state, ReconciliationState::AcknowledgedUnknown);
        assert_eq!(history[1].state, ReconciliationState::Resolved);
        assert_eq!(history[1].reason, REASON_DEFINITE_EVIDENCE);
        assert!(history[1].evidence_refs.contains(&definite_receipt));
    }

    /// Ensure undecided case reuses and appends evidence (no duplicates).
    #[test]
    fn undecided_case_reuses_with_additional_evidence() {
        let (_root, store) = test_store("reuse");
        let agent = EntityId::new();
        let task_id = EntityId::new();
        let operation_id = EntityId::new();
        let receipt_1 = EntityId::new();
        let receipt_2 = EntityId::new();
        let ledger_id = store.ledger.ledger_id();
        let commit = CommitRef::new(ledger_id, LedgerSeq(1));

        let dummy_ref = store
            .cas
            .seal_object(b"op", "application/json", "test", "raw", 1)
            .unwrap()
            .object;

        seed_task(&store, task_id, agent, commit);
        seed_operation(&store, operation_id, task_id, &dummy_ref, commit);

        let op_record = dummy_operation(operation_id, task_id, &dummy_ref, commit);
        let commit_2 = CommitRef::new(ledger_id, LedgerSeq(2));
        let case_id_1 = {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let id = store
                .ensure_unknown_effect_case(&tx, &op_record, receipt_1, REASON_EFFECT_UNKNOWN, commit_2)
                .unwrap();
            tx.commit().unwrap();
            id
        };

        // Same operation, different receipt: reuses the undecided case.
        let commit_3 = CommitRef::new(ledger_id, LedgerSeq(3));
        let case_id_2 = {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let id = store
                .ensure_unknown_effect_case(&tx, &op_record, receipt_2, REASON_EFFECT_UNKNOWN, commit_3)
                .unwrap();
            tx.commit().unwrap();
            id
        };

        assert_eq!(case_id_1, case_id_2, "same undecided case reused");
        let case = store.reconciliation_case(case_id_1).unwrap().unwrap();
        assert_eq!(case.evidence_refs.len(), 2);
        assert!(case.evidence_refs.contains(&receipt_1));
        assert!(case.evidence_refs.contains(&receipt_2));

        // Duplicate receipt does not duplicate evidence.
        let commit_4 = CommitRef::new(ledger_id, LedgerSeq(4));
        {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            store
                .ensure_unknown_effect_case(&tx, &op_record, receipt_1, REASON_EFFECT_UNKNOWN, commit_4)
                .unwrap();
            tx.commit().unwrap();
        }
        let case = store.reconciliation_case(case_id_1).unwrap().unwrap();
        assert_eq!(case.evidence_refs.len(), 2, "no duplicate receipt");
    }

    /// Acknowledgement from wrong agent is refused.
    #[test]
    fn acknowledgement_from_wrong_agent_is_refused() {
        let (_root, store) = test_store("wrong-agent");
        let agent = EntityId::new();
        let other_agent = EntityId::new();
        let task_id = EntityId::new();
        let operation_id = EntityId::new();
        let receipt_id = EntityId::new();
        let ledger_id = store.ledger.ledger_id();
        let commit = CommitRef::new(ledger_id, LedgerSeq(1));

        let dummy_ref = store
            .cas
            .seal_object(b"x", "application/json", "test", "raw", 1)
            .unwrap()
            .object;

        seed_task(&store, task_id, agent, commit);
        seed_operation(&store, operation_id, task_id, &dummy_ref, commit);

        let op_record = dummy_operation(operation_id, task_id, &dummy_ref, commit);
        let commit_2 = CommitRef::new(ledger_id, LedgerSeq(2));
        let case_id = {
            let mut conn = store.ledger.conn.lock();
            let tx = conn.transaction().unwrap();
            let id = store
                .ensure_unknown_effect_case(&tx, &op_record, receipt_id, REASON_EFFECT_UNKNOWN, commit_2)
                .unwrap();
            tx.commit().unwrap();
            id
        };

        let decision_object = store
            .cas
            .seal_object(b"d", "application/json", "cognition", "shenko.cognition.content", 1)
            .unwrap()
            .object;
        let ack = ReconciliationAcknowledgement {
            case_id,
            acknowledgement_id: EntityId::new(),
            reason_refs: vec![receipt_id],
            remaining_impact: "n/a".into(),
        };

        let mut conn = store.ledger.conn.lock();
        let tx = conn.transaction().unwrap();
        let result = store.acknowledge_unknown_effect(
            &tx,
            other_agent,
            &ack,
            &decision_object,
            CommitRef::new(ledger_id, LedgerSeq(3)),
        );
        assert!(result.is_err(), "wrong agent must be refused");
    }

    /// Resolve without prior case is a no-op.
    #[test]
    fn resolve_without_case_is_noop() {
        let (_root, store) = test_store("no-case");
        let ledger_id = store.ledger.ledger_id();
        let commit = CommitRef::new(ledger_id, LedgerSeq(1));
        let mut conn = store.ledger.conn.lock();
        let tx = conn.transaction().unwrap();
        let resolved = store
            .resolve_unknown_effect_cases(&tx, EntityId::new(), EntityId::new(), commit)
            .unwrap();
        assert_eq!(resolved, 0);
    }
}
