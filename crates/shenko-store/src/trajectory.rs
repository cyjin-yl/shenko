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
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    operation::OperationRecord,
    subject::StepDecision,
    task::{TaskOwner, TaskRole},
};

/// Records above this size are summarized by reference instead of inlined.
pub const TRAJECTORY_RECORD_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub kind: String,
    pub role: TaskRole,
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
    /// The complete sealed input when it is JSON within the record limit.
    /// A subject delegation retains its whole S1 decision here.
    pub input: Option<Value>,
    /// Display text from this task's bound delegation or ordinary input.
    /// Missing, malformed or ambiguous provenance leaves it unavailable.
    pub brief: Option<String>,
    pub children: Vec<TaskSummary>,
    pub operations: Vec<TrajectoryOperation>,
}

#[derive(Clone, Copy)]
enum TrajectoryInputSource {
    SubjectDecision,
    Object,
    Unavailable,
}

fn input_source(
    task: &TaskSummary,
    record: &Value,
    input_ref: Option<&ObjectRef>,
    intent_step: Option<&str>,
    intent_decision: Option<&str>,
) -> TrajectoryInputSource {
    match record.pointer("/memos/native_input") {
        Some(Value::String(encoding)) if encoding == "object" => {
            return TrajectoryInputSource::Object;
        }
        None => {}
        Some(Value::String(encoding)) if encoding == "subject_decision" => {}
        _ => return TrajectoryInputSource::Unavailable,
    }
    // A delegation without a native definition also retains its S1 input.
    // The subject-owned intent, not a definition name, binds that source.
    if task.role != TaskRole::Delegation
        || !task.created_by_step_id.as_deref().is_some_and(|step| Some(step) == intent_step)
        || record.pointer("/memos/intent_id").and_then(Value::as_str) != Some(task.task_id.as_str())
        || input_ref.is_none()
    {
        return TrajectoryInputSource::Unavailable;
    }
    let decision_ref = intent_decision
        .and_then(|data| serde_json::from_str::<ObjectRef>(data).ok());
    if input_ref.is_some_and(|reference| Some(reference) == decision_ref.as_ref()) {
        TrajectoryInputSource::SubjectDecision
    } else {
        TrajectoryInputSource::Unavailable
    }
}

fn brief_of(task: &TaskSummary, input: &Value, source: TrajectoryInputSource) -> Option<String> {
    match source {
        TrajectoryInputSource::SubjectDecision => {
            let decision = StepDecision::deserialize(input).ok()?;
            if !crate::channel::ADMITTED_DECISION_SCHEMA_VERSIONS.contains(&decision.schema_version)
                || Some(decision.step_id) != task.created_by_step_id.as_deref()?.parse::<EntityId>().ok()
            {
                return None;
            }
            let task_id = task.task_id.parse::<EntityId>().ok()?;
            let mut intents = decision.delegated_task_intents.into_iter()
                .filter(|intent| intent.intent_id == task_id);
            let intent = intents.next()?;
            if intents.next().is_some() || intent.goal.trim().is_empty() {
                return None;
            }
            Some(intent.goal)
        }
        TrajectoryInputSource::Object => ["goal", "prompt", "task", "instruction"].into_iter()
            .filter_map(|key| input.get(key).and_then(Value::as_str))
            .chain(input.pointer("/spec/goal").and_then(Value::as_str))
            .find(|text| !text.trim().is_empty()).map(str::to_owned),
        TrajectoryInputSource::Unavailable => None,
    }
}

pub(crate) fn utc_timestamp(value: &str) -> Result<String> {
    use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
    let time = match DateTime::parse_from_rfc3339(value) {
        Ok(time) => time.with_timezone(&Utc),
        Err(_) => NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")?.and_utc(),
    };
    Ok(time.to_rfc3339_opts(SecondsFormat::Nanos, true))
}

fn summary(
    task_id: String,
    data: &str,
    created_at: String,
    subject_root: Option<String>,
    intent_kind: Option<String>,
) -> Result<TaskSummary> {
    let value: Value = serde_json::from_str(data)?;
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let owner: Option<TaskOwner> = value.get("owner")
        .and_then(|owner| TaskOwner::deserialize(owner).ok());
    // The subject root owns itself; a self-owner is not a parent.
    let parent_task_id = match &owner {
        Some(TaskOwner::Task { id }) => Some(id.to_string()).filter(|id| *id != task_id),
        _ => None,
    };
    let self_owned = matches!(&owner, Some(TaskOwner::Task { .. })) && parent_task_id.is_none();
    let known_record = value.get("record_schema_version").and_then(Value::as_u64) == Some(1)
        && value.get("task_id").and_then(Value::as_str) == Some(task_id.as_str());
    let role = if !known_record {
        TaskRole::Unknown
    } else if subject_root.as_deref() == Some(task_id.as_str())
        && self_owned
    {
        TaskRole::SubjectRoot
    } else if owner.is_some() && intent_kind.as_deref() == Some("delegation") {
        TaskRole::Delegation
    } else if parent_task_id.as_deref().is_some_and(|parent| {
        value.pointer("/memos/child_tool_parent").and_then(Value::as_str) == Some(parent)
    }) {
        TaskRole::Tool
    } else {
        TaskRole::Unknown
    };
    Ok(TaskSummary {
        task_id,
        kind: text(value.get("kind")).unwrap_or_else(|| "unknown".into()),
        role,
        state: text(value.pointer("/state/status")).unwrap_or_else(|| "unknown".into()),
        parent_task_id,
        created_by_step_id: text(value.get("created_by_step_id")),
        created_at: utc_timestamp(&created_at)?,
        terminal_at: value.get("terminal_at").and_then(Value::as_str).map(utc_timestamp).transpose()?,
    })
}

impl Store {
    /// Recent tasks with their parent and creating step, newest first.
    pub fn list_task_summaries(&self, agent_id: EntityId, limit: usize) -> Result<Vec<TaskSummary>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT t.task_id, t.data, t.created_at, json_extract(h.data,'$.subject_root_task_id'), i.kind \
             FROM tasks t \
             LEFT JOIN subject_branch_heads h ON h.agent_id=json_extract(t.data,'$.agent_id') \
             LEFT JOIN subject_intents i ON i.intent_id=t.task_id AND i.agent_id=json_extract(t.data,'$.agent_id') \
             WHERE json_extract(t.data,'$.agent_id')=?1 \
             ORDER BY t.created_at DESC, t.task_id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![agent_id.to_string(), limit.clamp(1, 500) as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, data, created_at, root, intent) = row?;
            out.push(summary(id, &data, created_at, root, intent)?);
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
                    "SELECT t.task_id, t.data, t.created_at, json_extract(h.data,'$.subject_root_task_id'), i.kind, i.step_id, i.decision_ref \
                     FROM tasks t \
                     LEFT JOIN subject_branch_heads h ON h.agent_id=json_extract(t.data,'$.agent_id') \
                     LEFT JOIN subject_intents i ON i.intent_id=t.task_id AND i.agent_id=json_extract(t.data,'$.agent_id') \
                     WHERE t.task_id=?1 AND json_extract(t.data,'$.agent_id')=?2",
                    rusqlite::params![task_key, agent_key],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?, r.get::<_, Option<String>>(6)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            let record: Value = serde_json::from_str(&task.1)?;
            let input_ref = record.get("input_ref")
                .and_then(|reference| ObjectRef::deserialize(reference).ok());
            let task_summary = summary(task.0, &task.1, task.2, task.3, task.4)?;
            let source = input_source(&task_summary, &record, input_ref.as_ref(), task.5.as_deref(), task.6.as_deref());
            let task = (task_summary, input_ref, source);
            let mut stmt = conn.prepare(
                "SELECT t.task_id, t.data, t.created_at, json_extract(h.data,'$.subject_root_task_id'), i.kind \
                 FROM tasks t \
                 LEFT JOIN subject_branch_heads h ON h.agent_id=json_extract(t.data,'$.agent_id') \
                 LEFT JOIN subject_intents i ON i.intent_id=t.task_id AND i.agent_id=json_extract(t.data,'$.agent_id') \
                 WHERE json_extract(t.data,'$.owner.kind')='task' AND json_extract(t.data,'$.owner.id')=?1 AND t.task_id<>?1 \
                 AND json_extract(t.data,'$.agent_id')=?2 \
                 ORDER BY t.created_at, t.rowid",
            )?;
            let children = stmt
                .query_map(rusqlite::params![task_key, agent_key], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?))
                })?
                .map(|row| {
                    let (id, data, at, root, intent) = row?;
                    summary(id, &data, at, root, intent)
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
        let (task, input_ref, source) = task;
        let input = input_ref
            .filter(|object| object.bytes <= TRAJECTORY_RECORD_LIMIT)
            .and_then(|object| self.cas.read_object(&object.digest).ok().filter(|bytes| {
                bytes.len() as u64 == object.bytes
                    && Digest::blake3(bytes) == object.digest
            }))
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let brief = input.as_ref().and_then(|input| brief_of(&task, input, source));
        let operations = operations
            .iter()
            .filter_map(|data| serde_json::from_str::<OperationRecord>(data).ok())
            .map(|operation| self.trajectory_operation(operation))
            .collect();
        Ok(Some(TaskTrajectory { task, input, brief, children, operations }))
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
    /// `inbound` from an observed counterpart, `own` for observed account
    /// origin, `unknown` when attribution was not recorded, and `outbound`
    /// for the subject's publication. Account origin does not name a client.
    pub direction: &'static str,
    pub message_id: String,
    pub sender_id: Option<String>,
    pub sender_name: Option<String>,
    /// The sender holds an owner-set administrative grant on this channel.
    pub sender_admin: bool,
    pub text: String,
    /// When the platform says an inbound line happened; for a publication,
    /// the receipt observation, recorded task creation, or deciding step time.
    pub at: Option<String>,
    /// The publication ledger's actual state for outbound messages.
    pub state: Option<String>,
}

/// A publication with the facts the conversation view needs.
struct PublicationRow {
    intent_id: String,
    payload: String,
    state: String,
    recorded_at: Option<String>,
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
                "SELECT i.intent_id, i.payload, i.state, COALESCE(t.created_at, s.created_at), \
                   (SELECT e.data FROM evidence_receipts e WHERE e.operation_id = i.intent_id \
                    ORDER BY e.updated_commit_seq DESC, e.receipt_id DESC LIMIT 1) \
                 FROM subject_intents i LEFT JOIN subject_steps s ON s.step_id = i.step_id \
                 LEFT JOIN tasks t ON t.task_id = i.intent_id AND json_extract(t.data,'$.agent_id') = i.agent_id \
                 WHERE i.agent_id = ?1 AND i.kind = 'publication' \
                 ORDER BY i.created_seq DESC, i.intent_id DESC LIMIT ?2",
            )?;
            let publications = stmt
                .query_map(rusqlite::params![agent_key, limit as i64], |r| {
                    Ok(PublicationRow {
                        intent_id: r.get(0)?,
                        payload: r.get(1)?,
                        state: r.get(2)?,
                        recorded_at: r.get(3)?,
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
                None => row.recorded_at.as_deref().map(utc_timestamp).transpose()?,
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
            if envelope.actor.own_account == Some(true)
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
                direction: match envelope.actor.own_account {
                    Some(true) => "own",
                    Some(false) => "inbound",
                    None => "unknown",
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shenko_types::{object::SchemaRef, output::ParsedOutput, subject::DelegationIntent};

    fn reference(bytes: &[u8]) -> ObjectRef {
        let digest = Digest::blake3(bytes);
        ObjectRef {
            digest: digest.clone(), bytes: bytes.len() as u64,
            media_type: "application/json".into(), store_namespace: "trajectory-test".into(),
            locator: digest.to_string(),
            codec: SchemaRef { name: "StepDecision".into(), major: crate::channel::DECISION_SCHEMA_VERSION, digest },
        }
    }

    fn task(task_id: EntityId, step_id: EntityId) -> TaskSummary {
        TaskSummary {
            task_id: task_id.to_string(), kind: "future.plugin.observe".into(), role: TaskRole::Delegation,
            state: "running".into(), parent_task_id: None, created_by_step_id: Some(step_id.to_string()),
            created_at: "2026-11-13T00:00:00Z".into(), terminal_at: None,
        }
    }

    #[test]
    fn delegation_brief_matches_one_bound_intent_and_leaves_missing_or_ambiguous_goals_unavailable() {
        let first_id = EntityId::new();
        let current_id = EntityId::new();
        let step_id = EntityId::new();
        let current = task(current_id, step_id);
        let goal = "  Read this task's retained source.\n`[delegate]` is literal.\n";
        let original = reference(b"retained S0 and raw output identities");
        let intent = |intent_id, goal: &str| DelegationIntent {
            intent_id, goal: goal.into(), system_prompt: "PRIVATE_SYSTEM_PROMPT".into(),
            model: None, capabilities: vec![], budget_hint: None, reply_grant: None, definition: None,
        };
        let decision = StepDecision {
            schema_version: crate::channel::DECISION_SCHEMA_VERSION, step_id,
            output: ParsedOutput {
                schema_version: crate::channel::OUTPUT_SCHEMA_VERSION, step_id,
                raw_ref: original.clone(), text: "PRIVATE_OTHER_OUTPUT".into(), controls: vec![],
            },
            identity_intent: None, input_plan_ref: original, model_result_ref: None,
            applied_steering_refs: vec![], event_dispositions: vec![],
            delegated_task_intents: vec![intent(first_id, "A different task's goal"), intent(current_id, goal)],
            child_dialogue_intents: vec![], memory_intents: vec![], policy_intents: vec![],
            publication_intents: vec![], pending_obligations: vec![], unknown_acknowledgements: vec![],
            attention_policy: None, evidence_refs: vec![], successor_step_id: EntityId::new(),
        };
        let input = serde_json::to_value(&decision).unwrap();
        let sealed = reference(&serde_json::to_vec(&input).unwrap());
        let sealed_json = serde_json::to_string(&sealed).unwrap();
        let step_key = step_id.to_string();
        let record = |task: &TaskSummary| json!({"memos": {
            "intent_id": task.task_id, "native_input": "subject_decision",
        }});
        let project = |task: &TaskSummary, input: &Value| {
            let source = input_source(task, &record(task), Some(&sealed), Some(&step_key), Some(&sealed_json));
            brief_of(task, input, source)
        };
        assert_eq!(project(&current, &input).as_deref(), Some(goal));
        assert_eq!(project(&task(first_id, step_id), &input).as_deref(), Some("A different task's goal"));
        assert!(project(&task(EntityId::new(), step_id), &input).is_none());

        let mut ambiguous = input.clone();
        ambiguous["delegated_task_intents"].as_array_mut().unwrap().push(json!(decision.delegated_task_intents[1]));
        assert!(project(&current, &ambiguous).is_none());
        let mut malformed = input.clone();
        malformed["delegated_task_intents"][1]["goal"] = json!({"private": "not text"});
        assert!(project(&current, &malformed).is_none());
        let mut wrong_step = input.clone();
        wrong_step["step_id"] = json!(EntityId::new());
        assert!(project(&current, &wrong_step).is_none());
        let mut unknown_schema = input.clone();
        unknown_schema["schema_version"] = json!(99);
        assert!(project(&current, &unknown_schema).is_none());

        let unavailable = input_source(&current, &record(&current), Some(&sealed), None, Some(&sealed_json));
        assert!(brief_of(&current, &input, unavailable).is_none());
        let other_seal = serde_json::to_string(&reference(b"another decision")).unwrap();
        let unavailable = input_source(&current, &record(&current), Some(&sealed), Some(&step_key), Some(&other_seal));
        assert!(brief_of(&current, &input, unavailable).is_none());
        let mut missing_role = current;
        missing_role.role = TaskRole::Unknown;
        assert!(project(&missing_role, &input).is_none());
        assert_eq!(input, serde_json::to_value(&decision).unwrap(), "projection retains the original decision");
    }

    #[test]
    fn ordinary_task_briefs_preserve_text_shapes_without_serializing_private_input() {
        let task = task(EntityId::new(), EntityId::new());
        let record = json!({"memos": {"native_input": "object"}});
        let source = input_source(&task, &record, None, None, None);
        let text = "  Keep exact input text.\r\n[reply] literal example\n";
        for key in ["goal", "prompt", "task", "instruction"] {
            let input = Value::Object(serde_json::Map::from_iter([(key.into(), json!(text))]));
            assert_eq!(brief_of(&task, &input, source).as_deref(), Some(text));
        }
        assert_eq!(brief_of(&task, &json!({"spec": {"goal": text}}), source).as_deref(), Some(text));
        let sealed = reference(&serde_json::to_vec(&json!({"goal": text})).unwrap());
        let sealed_json = serde_json::to_string(&sealed).unwrap();
        let explicit_object = input_source(&task, &record, Some(&sealed), task.created_by_step_id.as_deref(), Some(&sealed_json));
        assert_eq!(brief_of(&task, &json!({"goal": text}), explicit_object).as_deref(), Some(text));
        assert!(brief_of(&task, &json!({"private_context": "not a brief"}), source).is_none());
        assert!(brief_of(&task, &json!({"goal": "  ", "spec": {"goal": 17}}), source).is_none());
        assert!(brief_of(&task, &json!("PRIVATE_RAW_INPUT"), source).is_none());
        let unknown_source = input_source(&task, &json!({"memos": {"native_input": 17}}), None, None, None);
        assert!(brief_of(&task, &json!({"goal": text}), unknown_source).is_none());
    }
}
