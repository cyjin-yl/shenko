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

pub(crate) fn utc_timestamp(value: &str) -> Result<String> {
    use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
    let time = match DateTime::parse_from_rfc3339(value) {
        Ok(time) => time.with_timezone(&Utc),
        Err(_) => NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")?.and_utc(),
    };
    Ok(time.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

fn summary(task_id: String, data: &str, created_at: String) -> Result<TaskSummary> {
    let v: Value = serde_json::from_str(data).unwrap_or_default();
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let owner = v.get("owner");
    // The subject root owns itself; a self-owner is not a parent.
    let parent_task_id = (owner.and_then(|o| o.get("kind")).and_then(Value::as_str) == Some("task"))
        .then(|| text(owner.and_then(|o| o.get("id"))))
        .flatten()
        .filter(|parent| *parent != task_id);
    Ok(TaskSummary {
        task_id,
        kind: text(v.get("kind")).unwrap_or_else(|| "unknown".into()),
        state: text(v.get("state").and_then(|s| s.get("status"))).unwrap_or_else(|| "unknown".into()),
        parent_task_id,
        created_by_step_id: text(v.get("created_by_step_id")),
        created_at: utc_timestamp(&created_at)?,
        terminal_at: v.get("terminal_at").and_then(Value::as_str).map(utc_timestamp).transpose()?,
    })
}

impl Store {
    /// Recent tasks with their parent and creating step, newest first.
    pub fn list_task_summaries(&self, agent_id: EntityId, limit: usize) -> Result<Vec<TaskSummary>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT task_id, data, created_at FROM tasks WHERE json_extract(data,'$.agent_id')=?1 \
             ORDER BY created_at DESC, task_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![agent_id.to_string(), limit.clamp(1, 500) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data, created_at) = row?;
            out.push(summary(id, &data, created_at)?);
        }
        Ok(out)
    }

    /// The subject's task trajectory, or `None` for an absent or foreign task.
    pub fn task_trajectory(&self, agent_id: EntityId, task_id: EntityId) -> Result<Option<TaskTrajectory>> {
        let agent_key = agent_id.to_string();
        let task_key = task_id.to_string();
        let (task, children, operations) = {
            let conn = self.ledger.conn.lock();
            let Some(task) = conn
                .query_row(
                    "SELECT task_id, data, created_at FROM tasks WHERE task_id=?1 AND json_extract(data,'$.agent_id')=?2",
                    rusqlite::params![task_key, agent_key],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let input_ref = serde_json::from_str::<Value>(&task.1).ok()
                .and_then(|v| v.get("input_ref").cloned())
                .and_then(|r| serde_json::from_value::<shenko_types::object::ObjectRef>(r).ok());
            let task = (summary(task.0, &task.1, task.2)?, input_ref);
            let mut stmt = conn.prepare(
                "SELECT task_id, data, created_at FROM tasks \
                 WHERE json_extract(data,'$.owner.kind')='task' AND json_extract(data,'$.owner.id')=?1 AND task_id<>?1 \
                 AND json_extract(data,'$.agent_id')=?2 \
                 ORDER BY created_at, rowid",
            )?;
            let children = stmt
                .query_map(rusqlite::params![task_key, agent_key], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })?
                .map(|row| {
                    let (id, data, at) = row?;
                    summary(id, &data, at)
                })
                .collect::<Result<Vec<_>>>()?;
            let mut stmt = conn.prepare(
                "SELECT data FROM operations WHERE json_extract(data,'$.task_id')=?1 ORDER BY rowid",
            )?;
            let operations = stmt
                .query_map([task_key.as_str()], |r| r.get::<_, String>(0))?
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
/// Every channel plugin reaches this view the same way: an IM platform, a
/// relay to another Shenko on the local network or an embodied robot's
/// channel is one more provider, not a special case.
///
/// [`ChannelEnvelope`]: shenko_types::channel::ChannelEnvelope
#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub thread_key: String,
    /// The plugin's human label for the audience, when it gave one.
    pub thread_label: Option<String>,
    pub thread_kind: String,
    /// How many counterparts the audience reaches, as the plugin stated it.
    pub thread_shape: Option<shenko_types::channel::AudienceShape>,
    pub channel: String,
    /// `inbound` from a counterpart, `own` for the account's own line sent
    /// from another client, `outbound` for the subject's publication.
    pub direction: &'static str,
    pub message_id: String,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    /// The sender holds an owner-set administrative grant on this channel.
    pub sender_admin: bool,
    pub text: String,
    /// When the platform says an inbound line happened; for a publication,
    /// when the provider answered the send, else when the deciding step began.
    pub at: Option<String>,
    /// The publication ledger's actual state for outbound messages.
    pub state: Option<String>,
}

/// A publication with the facts the conversation view needs.
struct PublicationRow {
    intent_id: String,
    payload: String,
    state: String,
    step_started: Option<String>,
    receipt: Option<String>,
}

impl Store {
    /// The subject's recent channel conversation messages, oldest first.
    ///
    /// Publications keep their most recent `limit` rows. Staged inputs are
    /// scanned newest first until `limit` unique, classified messages with a
    /// reply audience have been admitted, or the history is exhausted. An
    /// own-account line that echoes a publication in that window is omitted
    /// only when its provider and exact reply audience also match.
    pub fn channel_conversation_messages(
        &self,
        agent_id: EntityId,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>> {
        use chrono::SecondsFormat;
        use shenko_types::channel::{ChannelEnvelope, ChannelReceipt};
        use shenko_types::evidence::EvidenceReceipt;
        let limit = limit.clamp(1, 2000);
        let agent_key = agent_id.to_string();
        let publications = {
            let conn = self.ledger.conn.lock();
            let mut stmt = conn.prepare(
                "SELECT i.intent_id, i.payload, i.state, s.created_at, \
                   (SELECT e.data FROM evidence_receipts e WHERE e.operation_id = i.intent_id \
                    ORDER BY e.updated_commit_seq DESC, e.receipt_id DESC LIMIT 1) \
                 FROM subject_intents i LEFT JOIN subject_steps s ON s.step_id = i.step_id \
                 WHERE i.agent_id = ?1 AND i.kind = 'publication' \
                 ORDER BY i.created_seq DESC, i.intent_id DESC LIMIT ?2",
            )?;
            let publications = stmt
                .query_map(rusqlite::params![agent_key, limit as i64], |r| {
                    Ok(PublicationRow {
                        intent_id: r.get(0)?,
                        payload: r.get(1)?,
                        state: r.get(2)?,
                        step_started: r.get(3)?,
                        receipt: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            publications
        };

        let mut out = Vec::new();
        let mut published_targets: std::collections::HashMap<
            String,
            Vec<shenko_types::output::PublicationTarget>,
        > = std::collections::HashMap::new();
        for row in publications {
            let publication: shenko_types::subject::PublicationIntent =
                serde_json::from_str(&row.payload)?;
            let evidence = row
                .receipt
                .as_deref()
                .map(serde_json::from_str::<EvidenceReceipt>)
                .transpose()?;
            // Recovery observations are also evidence, but their raw object
            // is not a provider answer. Decode only the declared channel
            // receipt schema, and propagate failures of that actual decoder.
            let provider_id = match evidence.as_ref().filter(|evidence| {
                evidence.producer_schema_version.as_deref() == Some("channel_receipt")
            }) {
                Some(evidence) => {
                    let bytes = self.cas.read_object(&evidence.raw_ref.digest)?;
                    serde_json::from_slice::<ChannelReceipt>(&bytes)?.provider_message_id
                }
                None => None,
            };
            let at = match evidence.and_then(|evidence| evidence.observed_at) {
                Some(at) => Some(at.to_rfc3339_opts(SecondsFormat::Nanos, true)),
                None => row.step_started.as_deref().map(utc_timestamp).transpose()?,
            };
            let target = &publication.target;
            out.push(ConversationMessage {
                thread_key: format!("{}|{}|{}", target.account, target.kind, target.id),
                thread_label: None,
                thread_kind: target.kind.clone(),
                thread_shape: None,
                channel: target.channel.clone(),
                direction: "outbound",
                message_id: row.intent_id,
                sender_id: None,
                sender_name: None,
                sender_admin: false,
                text: publication.content.clone(),
                at,
                state: Some(row.state),
            });
            if let Some(provider_id) = provider_id {
                published_targets
                    .entry(provider_id)
                    .or_default()
                    .push(publication.target);
            }
        }
        // A channel message is a staged input of the step that took it
        // (`{"v":1,"inbox_event":…}`), not an `inbox_events` row. Deferred
        // inputs can appear in many later steps. Stream the durable rows so
        // neither those copies nor inapplicable inputs consume the window;
        // retaining only admitted event ids bounds memory by the limit. The
        // immutable owning raw session, not the envelope's channel account,
        // determines which subject may read the input.
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT json_extract(e.data,'$.inbox_event.event_id'), \
                    json_extract(e.data,'$.inbox_event.payload') \
             FROM session_events e \
             WHERE e.event_type='user' \
               AND json_extract(e.data,'$.inbox_event.kind')='message' \
               AND EXISTS (SELECT 1 FROM raw_sessions r \
                           WHERE r.session_id = e.session_id AND r.agent_id = ?1) \
             ORDER BY e.rowid DESC",
        )?;
        let mut inbound = stmt.query([agent_key.as_str()])?;
        let mut seen = std::collections::HashSet::new();
        while seen.len() < limit {
            let Some(row) = inbound.next()? else {
                break;
            };
            let Some(event_id) = row.get::<_, Option<String>>(0)? else {
                continue;
            };
            if seen.contains(&event_id) {
                continue;
            }
            let Some(payload) = row.get::<_, Option<String>>(1)? else {
                continue;
            };
            // Only classified envelopes carry a reply audience; other inputs
            // (the owner's web input, unclassified frames) are not channel threads.
            let Ok(envelope) = serde_json::from_str::<ChannelEnvelope>(&payload) else {
                continue;
            };
            let Some(target) = envelope.reply_target.as_ref() else {
                continue;
            };
            if envelope.actor.own_account
                && published_targets
                    .get(&envelope.message_id)
                    .is_some_and(|targets| {
                        targets.iter().any(|published| {
                            published.channel == envelope.provider
                                && published.account == target.account
                                && published.kind == target.kind
                                && published.id == target.id
                        })
                    })
            {
                continue;
            }
            seen.insert(event_id.clone());
            out.push(ConversationMessage {
                thread_key: format!("{}|{}|{}", target.account, target.kind, target.id),
                thread_label: Some(target.label.clone()).filter(|label| !label.is_empty()),
                thread_kind: target.kind.clone(),
                thread_shape: target.shape,
                channel: envelope.provider.clone(),
                direction: if envelope.actor.own_account {
                    "own"
                } else {
                    "inbound"
                },
                message_id: event_id,
                sender_id: Some(envelope.actor.platform_id.clone()),
                sender_name: envelope
                    .actor
                    .display_name
                    .clone()
                    .or_else(|| envelope.actor.alias.clone()),
                sender_admin: envelope.actor.role_grant_ref.is_some(),
                text: envelope.content.text.clone(),
                at: Some(
                    envelope
                        .occurred_at
                        .unwrap_or(envelope.received_at)
                        .to_rfc3339_opts(SecondsFormat::Nanos, true),
                ),
                state: None,
            });
        }
        out.sort_unstable_by(|left, right| left.at.is_none().cmp(&right.at.is_none())
            .then_with(|| left.at.cmp(&right.at))
            .then_with(|| left.message_id.cmp(&right.message_id)));
        Ok(out)
    }
}
