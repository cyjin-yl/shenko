//! Read-only task trajectories for inspection surfaces
//! (docs/specs/web-interface.md#spec-21-1 "理解或调整一项工作"; the subagent
//! trajectory view in docs/specs/web-interface.md#5-2).
//!
//! A trajectory is assembled from facts the ledger already holds and nothing
//! else: the task row, the tasks it owns, its operations in ledger order, and
//! for each settled operation the normalized record its final evidence
//! receipt sealed in CAS. No plugin type is named here; a record is returned
//! as the JSON it was sealed as, so a model child's step, a native tool's
//! result or a future plugin's record all read the same way. Missing or
//! unreadable evidence is reported as such, never filled in.
use crate::Store;
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::Value;
use shenko_types::{ids::EntityId, operation::OperationRecord};

/// Records above this size are summarized by reference instead of inlined.
pub const TRAJECTORY_RECORD_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub kind: String,
    pub state: String,
    pub parent_task_id: Option<String>,
    pub created_by_step_id: Option<String>,
    pub created_at: String,
    pub terminal_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryOperation {
    pub operation_id: String,
    pub operation_name: String,
    pub logical_key: String,
    pub state: Value,
    pub outcome: Value,
    pub attempts: usize,
    /// The sealed normalized record of the final evidence, when it is JSON
    /// and within [`TRAJECTORY_RECORD_LIMIT`].
    pub record: Option<Value>,
    /// Why `record` is absent: `unsettled`, `no_receipt`, `no_normalized`,
    /// `too_large:<bytes>`, `not_json` or `unreadable:<reason>`.
    pub record_status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskTrajectory {
    pub task: TaskSummary,
    /// The task's sealed input when it is JSON within the record limit: for
    /// a delegation, the brief the delegator wrote.
    pub input: Option<Value>,
    pub children: Vec<TaskSummary>,
    pub operations: Vec<TrajectoryOperation>,
}

fn summary(task_id: String, data: &str, created_at: String) -> TaskSummary {
    let v: Value = serde_json::from_str(data).unwrap_or_default();
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let owner = v.get("owner");
    // The subject root owns itself; a self-owner is not a parent.
    let parent_task_id = (owner.and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("task"))
        .then(|| text(owner.and_then(|o| o.get("id"))))
        .flatten()
        .filter(|parent| *parent != task_id);
    TaskSummary {
        task_id,
        kind: text(v.get("kind")).unwrap_or_else(|| "unknown".into()),
        state: text(v.get("state").and_then(|s| s.get("status"))).unwrap_or_else(|| "unknown".into()),
        parent_task_id,
        created_by_step_id: text(v.get("created_by_step_id")),
        created_at,
        terminal_at: text(v.get("terminal_at")),
    }
}

impl Store {
    /// Recent tasks with their parent and creating step, newest first.
    pub fn list_task_summaries(&self, limit: usize) -> Result<Vec<TaskSummary>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT task_id, data, created_at FROM tasks ORDER BY created_at DESC, task_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.clamp(1, 500) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data, created_at) = row?;
            out.push(summary(id, &data, created_at));
        }
        Ok(out)
    }

    /// The trajectory of one task, or `None` when the task does not exist.
    pub fn task_trajectory(&self, task_id: EntityId) -> Result<Option<TaskTrajectory>> {
        let (task, children, operations) = {
            let conn = self.ledger.conn.lock();
            let Some(task) = conn
                .query_row(
                    "SELECT task_id, data, created_at FROM tasks WHERE task_id=?1",
                    [task_id.to_string()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let input_ref = serde_json::from_str::<Value>(&task.1).ok()
                .and_then(|v| v.get("input_ref").cloned())
                .and_then(|r| serde_json::from_value::<shenko_types::object::ObjectRef>(r).ok());
            let task = (summary(task.0, &task.1, task.2), input_ref);
            let mut stmt = conn.prepare(
                "SELECT task_id, data, created_at FROM tasks \
                 WHERE json_extract(data,'$.owner.kind')='task' AND json_extract(data,'$.owner.id')=?1 AND task_id<>?1 \
                 ORDER BY created_at, rowid",
            )?;
            let children = stmt
                .query_map([task_id.to_string()], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .map(|row| row.map(|(id, data, at)| summary(id, &data, at)))
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt = conn.prepare(
                "SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?1 ORDER BY rowid",
            )?;
            let operations = stmt
                .query_map([task_id.to_string()], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            (task, children, operations)
        };
        let (task, input_ref) = task;
        let input = input_ref
            .filter(|object| object.bytes <= TRAJECTORY_RECORD_LIMIT)
            .and_then(|object| self.cas.read_object(&object.digest).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let operations = operations
            .iter()
            .filter_map(|data| serde_json::from_str::<OperationRecord>(data).ok())
            .map(|operation| self.trajectory_operation(operation))
            .collect();
        Ok(Some(TaskTrajectory { task, input, children, operations }))
    }

    fn trajectory_operation(&self, operation: OperationRecord) -> TrajectoryOperation {
        let attempts = self.external_operation_attempts(operation.operation_id).unwrap_or_default();
        let (record, record_status) = self.sealed_record(&operation, attempts.last());
        TrajectoryOperation {
            operation_id: operation.operation_id.to_string(),
            operation_name: operation.operation_name.clone(),
            logical_key: operation.logical_key.clone(),
            state: serde_json::to_value(operation.state).unwrap_or(Value::Null),
            outcome: serde_json::to_value(operation.outcome).unwrap_or(Value::Null),
            attempts: attempts.len(),
            record,
            record_status,
        }
    }

    fn sealed_record(
        &self,
        operation: &OperationRecord,
        attempt: Option<&shenko_types::operation::AttemptRecord>,
    ) -> (Option<Value>, String) {
        use shenko_types::operation::OperationState;
        if operation.state != OperationState::Resolved {
            return (None, "unsettled".into());
        }
        let Some(receipt_id) = attempt.and_then(|a| a.final_receipt_id) else {
            return (None, "no_receipt".into());
        };
        let evidence = match self.external_evidence(receipt_id) {
            Ok(evidence) => evidence,
            Err(error) => return (None, format!("unreadable:{error}")),
        };
        let Some(normalized) = evidence.normalized_ref else {
            return (None, "no_normalized".into());
        };
        if normalized.bytes > TRAJECTORY_RECORD_LIMIT {
            return (None, format!("too_large:{}", normalized.bytes));
        }
        match self.cas.read_object(&normalized.digest) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(value) => (Some(value), "sealed".into()),
                Err(_) => (None, "not_json".into()),
            },
            Err(error) => (None, format!("unreadable:{error}")),
        }
    }
}

/// One message of a channel conversation, from the subject's own records:
/// an inbound event whose payload is a classified [`ChannelEnvelope`], or a
/// committed publication the subject addressed to that audience. Threads are
/// keyed by the reply audience (`account|kind|id`), the same identity a
/// publication target carries, so both directions meet in one thread.
///
/// [`ChannelEnvelope`]: shenko_types::channel::ChannelEnvelope
#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub thread_key: String,
    /// The plugin's human label for the audience, when it gave one.
    pub thread_label: Option<String>,
    pub thread_kind: String,
    pub channel: String,
    pub direction: &'static str,
    pub message_id: String,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    pub text: String,
    pub at: Option<String>,
    /// The publication ledger's actual state for outbound messages.
    pub state: Option<String>,
}

impl Store {
    /// Recent messages, up to `limit` in each direction, in timestamp order.
    /// Publication time comes from the task created with the intent; missing
    /// historical task metadata stays unknown instead of inventing a time.
    pub fn channel_conversation_messages(&self, limit: usize) -> Result<Vec<ConversationMessage>> {
        use chrono::SecondsFormat;
        use shenko_types::channel::ChannelEnvelope;
        let limit = limit.clamp(1, 2000) as i64;
        let inbound = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT event_id, data FROM inbox_events \
                 WHERE kind='message' ORDER BY created_at DESC, rowid DESC LIMIT ?1",
            )?;
            stmt.query_map([limit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let publications = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT i.intent_id, i.payload, i.state, t.created_at FROM subject_intents i \
                 LEFT JOIN tasks t ON t.task_id=i.intent_id \
                 WHERE i.kind='publication' ORDER BY i.created_seq DESC, i.intent_id DESC LIMIT ?1",
            )?;
            stmt.query_map([limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut out = Vec::with_capacity(inbound.len() + publications.len());
        for (event_id, data) in inbound {
            // Only classified envelopes carry a reply audience; other inputs
            // (the owner's web input, legacy frames) are not channel threads.
            let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&data) else { continue };
            let Some(target) = envelope.reply_target else { continue };
            out.push(ConversationMessage {
                thread_key: format!("{}|{}|{}", target.account, target.kind, target.id),
                thread_label: Some(target.label).filter(|label| !label.is_empty()),
                thread_kind: target.kind,
                channel: envelope.provider,
                direction: "inbound",
                message_id: event_id,
                sender_id: Some(envelope.actor.platform_id),
                sender_name: envelope.actor.display_name.or(envelope.actor.alias),
                text: envelope.content.text,
                at: Some(envelope.occurred_at.unwrap_or(envelope.received_at)
                    .to_rfc3339_opts(SecondsFormat::Nanos, true)),
                state: None,
            });
        }
        for (intent_id, payload, state, created_at) in publications {
            let Ok(publication) = serde_json::from_str::<shenko_types::subject::PublicationIntent>(&payload) else {
                continue;
            };
            let at = created_at.map(|value| {
                chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S%.f")
                    .map(|time| time.and_utc().to_rfc3339_opts(SecondsFormat::Nanos, true))
            }).transpose()?;
            let target = publication.target;
            out.push(ConversationMessage {
                thread_key: format!("{}|{}|{}", target.account, target.kind, target.id),
                thread_label: None,
                thread_kind: target.kind,
                channel: target.channel,
                direction: "outbound",
                message_id: intent_id,
                sender_id: None,
                sender_name: None,
                text: publication.content,
                at,
                state: Some(state),
            });
        }
        out.sort_unstable_by(|left, right| left.at.is_none().cmp(&right.at.is_none())
            .then_with(|| left.at.cmp(&right.at))
            .then_with(|| left.message_id.cmp(&right.message_id)));
        Ok(out)
    }
}
