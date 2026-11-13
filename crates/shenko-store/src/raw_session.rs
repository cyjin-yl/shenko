//! Durable raw conversation facts of a subject branch.
//!
//! The raw session is the evidence of what the subject actually received and
//! produced: append-only rows with a stable id, a monotonic order and a
//! source. One write policy serves every path — the legacy `WriteBatch`
//! writer and the subject staging path below share it, so there is never a
//! second rule: a replayed fact is a no-op, a different fact under a taken
//! id or seq is a conflict that rolls the whole transaction back, and a
//! committed row is never rewritten.
//!
//! Staging is a durable input fact BEFORE projection and the final S0 seal,
//! never an S1 acknowledgement. A crash between staging and sealing can
//! therefore recover the same inputs through the same API without losing,
//! duplicating or acknowledging them.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::sync::atomic::Ordering;

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{BranchId, EntityId};
use shenko_types::store::{RawSession, RawSessionEvent};
use shenko_types::subject::{
    ControlOutputRejection, MemoryIntentKind, StepDecision, SubjectBranchHead, SubjectStep,
    SubjectStepState,
};

use crate::session_events::{COLUMNS, StoredSessionEvent, parse_row, row};
use crate::store::{Store, checked_head, read_step, storage_error, validate_step_binding};

/// One structured runtime-error journal entry — docs/specs/cognition-units.md#spec-45-14.
///
/// A runtime error is a fact of the subject's own history, not a diagnostic
/// lost to stderr: it commits as a `runtime_error` raw session event of the
/// subject's CURRENT branch. The write never joins the failing operation's
/// transaction and never depends on the caller's possibly stale head copy,
/// so it stays possible exactly when the original work could not commit.
#[derive(Debug, Clone, Default)]
pub struct RuntimeErrorContext {
    /// Stable machine-readable site, e.g. `subject_loop`, `native_execute`.
    pub kind: String,
    /// Rust module path of the reporting site.
    pub source_module: String,
    /// Human-readable summary; credentials are redacted before commit and
    /// the text is bounded. The caller passes its best message.
    pub message: String,
    /// Subject step number when the error belongs to a step, else None.
    pub step_no: Option<u64>,
    /// Finite task when the error belongs to a native dispatch, else None.
    pub task_id: Option<EntityId>,
    /// Bounded extra fields; unknown/failed values stay inside the object.
    pub extra: serde_json::Value,
}

/// The event_type (and role) of committed runtime-error facts.
pub const RUNTIME_ERROR_EVENT_TYPE: &str = "runtime_error";

const RUNTIME_ERROR_MESSAGE_MAX_CHARS: usize = 4096;
const RUNTIME_ERROR_EXTRA_MAX_BYTES: usize = 4096;

/// Mechanical, best-effort redaction of assignments and bearer tokens.
/// Quoted keys and values honor escapes; unrelated text is retained.
pub(crate) fn redact_journal_text(text: &str) -> String {
    redact_bearer(redact_assignments(text))
}

fn credential_key(key: &str) -> bool {
    const KEYS: [&str; 10] = [
        "token", "secret", "password", "passwd", "apikey", "api_key", "api-key",
        "authorization", "auth", "credential",
    ];
    KEYS.iter().any(|key_part| key.as_bytes().windows(key_part.len())
        .any(|part| part.eq_ignore_ascii_case(key_part.as_bytes())))
}

fn escaped_quote(text: &str, index: usize) -> bool {
    text.as_bytes()[..index].iter().rev().take_while(|&&byte| byte == b'\\').count() % 2 != 0
}

fn credential_assignment(head: &str) -> bool {
    let head = head.trim_end();
    if let Some(quote @ ('"' | '\'')) = head.chars().next_back() {
        let Some((start, _)) = head[..head.len() - 1].rmatch_indices(quote)
            .find(|(index, _)| !escaped_quote(head, *index)) else { return false; };
        let key = &head[start + 1..head.len() - 1];
        if quote == '"' && key.contains('\\') {
            return serde_json::from_str::<String>(&head[start..]).ok()
                .is_some_and(|key| credential_key(&key));
        }
        return credential_key(key);
    }
    let start = head.rfind(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '-' | '.'))
        .map_or(0, |index| index + head[index..].chars().next().unwrap().len_utf8());
    credential_key(&head[start..])
}

fn redact_assignments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find(['=', ':']) {
        let (head, tail) = rest.split_at(index);
        out.push_str(&rest[..index + 1]);
        if credential_assignment(head) {
            let after = &tail[1..];
            let spaced = after.len() - after.trim_start_matches([' ', '\t']).len();
            let value = &after[spaced..];
            out.push_str(&after[..spaced]);
            let value_len = if let Some(quote @ ('"' | '\'')) = value.chars().next() {
                let end = value.char_indices().skip(1)
                    .find(|(index, c)| *c == quote && !escaped_quote(value, *index));
                out.push(quote);
                out.push_str("<redacted>");
                if let Some((index, _)) = end {
                    out.push(quote);
                    index + 1
                } else { value.len() }
            } else {
                out.push_str("<redacted>");
                if tail.starts_with(':') {
                    value.find([',', '}', ']', '\r', '\n']).unwrap_or(value.len())
                } else {
                    let first = value.find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '}' | ']' | '"' | '\''))
                        .unwrap_or(value.len());
                    // An unquoted authorization assignment can still carry
                    // a scheme and token, rather than just the first word.
                    if value[..first].eq_ignore_ascii_case("bearer") || value[..first].eq_ignore_ascii_case("basic") {
                        let token = value[first..].trim_start_matches([' ', '\t']);
                        value.len() - token.len() + token.find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '}' | ']' | '"' | '\''))
                            .unwrap_or(token.len())
                    } else { first }
                }
            };
            rest = &value[value_len..];
        } else {
            rest = &rest[index + 1..];
        }
    }
    out.push_str(rest);
    out
}

fn redact_bearer(text: String) -> String {
    let mut out = String::new();
    let mut rest = text.as_str();
    loop {
        let Some(pos) = rest.as_bytes().windows(7).position(|word|
            word[..6].eq_ignore_ascii_case(b"bearer") && word[6].is_ascii_whitespace()) else { break; };
        if out.capacity() == 0 { out.reserve(text.len()); }
        let word_boundary = pos == 0
            || !rest[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !word_boundary {
            out.push_str(&rest[..pos + 7]);
            rest = &rest[pos + 7..];
            continue;
        }
        let after = &rest[pos + 6..];
        let trimmed = after.trim_start();
        out.push_str(&rest[..pos + 6 + after.len() - trimmed.len()]);
        let end = trimmed.find(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '}' | ']' | '"' | '\''))
            .unwrap_or(trimmed.len());
        out.push_str("<redacted>");
        rest = &trimmed[end..];
    }
    if out.capacity() == 0 { return text; }
    out.push_str(rest);
    out
}

fn bound_chars(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.char_indices();
    let last = chars.nth(max_chars.saturating_sub(1));
    if max_chars != 0 && chars.next().is_none() {
        return (text.to_owned(), false);
    }
    if max_chars == 0 { return (String::new(), !text.is_empty()); }
    let mut cut = text[..last.expect("truncated text has a final character").0].to_owned();
    cut.push('…');
    (cut, true)
}

fn journal_message(text: &str) -> (String, bool) {
    // Bound work before redaction. An unfinished quoted secret consumes the
    // remaining prefix; its unseen suffix is never copied into the journal.
    let prefix_end = text.char_indices().nth(RUNTIME_ERROR_MESSAGE_MAX_CHARS)
        .map_or(text.len(), |(index, _)| index);
    let redacted = redact_journal_text(&text[..prefix_end]);
    let (mut message, truncated) = bound_chars(&redacted, RUNTIME_ERROR_MESSAGE_MAX_CHARS);
    if prefix_end != text.len() && !truncated {
        if let Some((index, _)) = message.char_indices().nth(RUNTIME_ERROR_MESSAGE_MAX_CHARS - 1) {
            message.truncate(index);
        }
        message.push('…');
    }
    (message, truncated || prefix_end != text.len())
}

fn redact_journal_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => redact_journal_text(text).into(),
        serde_json::Value::Array(values) => values.iter().map(redact_journal_value).collect(),
        serde_json::Value::Object(values) => values.iter().map(|(key, value)| (
            key.clone(), if credential_key(key) { serde_json::Value::String("<redacted>".into()) }
                else { redact_journal_value(value) },
        )).collect(),
        _ => value.clone(),
    }
}

fn journal_extra(value: &serde_json::Value) -> serde_json::Value {
    let mut buffer = [0u8; RUNTIME_ERROR_EXTRA_MAX_BYTES];
    // Keep the original whole-object byte admission: redaction must not
    // turn an oversized input into an apparently complete journal object.
    if serde_json::to_writer(buffer.as_mut_slice(), value).is_ok() {
        let redacted = redact_journal_value(value);
        if serde_json::to_writer(buffer.as_mut_slice(), &redacted).is_ok() { return redacted; }
    }
    serde_json::json!({"journal_extra_dropped": "context extra exceeded 4096 bytes or was not serializable"})
}

/// Link rows from a subject step to the raw rows it staged.
///
/// The link rows are an index over raw events, never a second truth store:
/// the raw row stays the only record of what was staged, and the link only
/// says which step owns which source event.
pub(crate) const RAW_SESSION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS subject_raw_inputs (
    step_id TEXT NOT NULL REFERENCES subject_steps(step_id),
    source_event_id TEXT NOT NULL,
    raw_event_id TEXT NOT NULL REFERENCES session_events(event_id),
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (step_id, source_event_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_subject_raw_inputs_raw
    ON subject_raw_inputs(raw_event_id);
CREATE INDEX IF NOT EXISTS idx_subject_raw_inputs_step_seq
    ON subject_raw_inputs(step_id, seq);
CREATE INDEX IF NOT EXISTS idx_subject_raw_inputs_source_session
    ON subject_raw_inputs(source_event_id, session_id, seq);
";

/// One durable input fact of a subject step, before projection and the final
/// S0 seal.
#[derive(Debug, Clone, PartialEq)]
pub struct SubjectHistoryInput {
    /// Stable id of the original inbox event.
    pub event_id: EntityId,
    /// The conversation kind of the fact, e.g. `user` or `system`.
    pub event_type: String,
    /// The v1 payload: the visible text plus the original event.
    pub data: serde_json::Value,
}

/// Durable source coordinates of an S0 projection, checked again by its CAS.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SubjectContextRef {
    pub scope_id: EntityId,
    pub source_commit_seq: u64,
    pub leaf_seq: Option<u64>,
    pub input_raw_event_ids: Vec<EntityId>,
    pub window_id: EntityId,
    pub window_generation: u64,
    pub model_generation: u64,
}

/// The canonical runtime feedback for a rejected control result.
///
/// The correction request the subject sends and the raw history must carry
/// the same words, so the store owns the text and the loop reuses it.
pub fn control_rejection_feedback(rejection: &ControlOutputRejection) -> String {
    format!(
        "运行时控制协议拒绝：{} {}。此结果的全部动作均未接纳，输入事件仍待处理。请依据原输入纠正完整结果，不要重用旧语法；示例放入引用或代码块。",
        rejection.error.code, rejection.error.details
    )
}

/// The conversation role of a raw fact.
///
/// The store never invents a speaker: it maps the caller's declared kind
/// onto the role vocabulary and keeps anything else verbatim.
fn role_of(event_type: &str) -> &str {
    match event_type {
        "tool_call" => "assistant",
        "tool_result" => "tool",
        other => other,
    }
}

/// The immutable description of a branch's raw session.
fn raw_session_data(agent_id: EntityId, branch_id: &BranchId) -> String {
    serde_json::json!({
        "v": 1,
        "kind": "subject_branch",
        "agent_id": agent_id.to_string(),
        "branch_id": branch_id.0.to_string(),
    })
    .to_string()
}

/// Write one raw session parent row.
///
/// A replay of the same facts is a no-op; a different fact under an existing
/// session id is a conflict that rolls the whole transaction back.
pub(crate) fn write_raw_session(
    conn: &Connection,
    session: &RawSession,
) -> Result<(), ShenkoError> {
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO raw_sessions (session_id, agent_id, started_at, data)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session.session_id.to_string(),
                session.agent_id.to_string(),
                session.started_at,
                session.data
            ],
        )
        .map_err(storage_error)?;
    if inserted == 0 {
        let same: bool = conn
            .query_row(
                "SELECT agent_id = ?2 AND started_at = ?3 AND data = ?4
                   FROM raw_sessions WHERE session_id = ?1",
                params![
                    session.session_id.to_string(),
                    session.agent_id.to_string(),
                    session.started_at,
                    session.data
                ],
                |r| r.get(0),
            )
            .map_err(storage_error)?;
        if !same {
            return Err(ShenkoError::conflict(format!(
                "raw session {} already exists with different facts",
                session.session_id
            )));
        }
    }
    Ok(())
}

/// Write one raw session event.
///
/// `event_id` is idempotent and `(session_id, seq)` belongs to exactly one
/// event: a replay of the same fact is a no-op, a different fact under either
/// key is a conflict that rolls the whole transaction back.
pub(crate) fn write_session_event(
    conn: &Connection,
    event: &RawSessionEvent,
) -> Result<(), ShenkoError> {
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO session_events (event_id, session_id, seq, event_type, role, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.event_id.to_string(),
                event.session_id.to_string(),
                event.seq,
                event.event_type,
                event.role,
                event.data
            ],
        )
        .map_err(storage_error)?;
    if inserted == 0 {
        let same: Option<bool> = conn
            .query_row(
                "SELECT session_id = ?2 AND seq = ?3 AND event_type = ?4 AND role = ?5 AND data = ?6
                   FROM session_events WHERE event_id = ?1",
                params![
                    event.event_id.to_string(),
                    event.session_id.to_string(),
                    event.seq,
                    event.event_type,
                    event.role,
                    event.data
                ],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if same != Some(true) {
            return Err(ShenkoError::conflict(format!(
                "session {} seq {} already holds a different event than {}",
                event.session_id, event.seq, event.event_id
            )));
        }
    }
    Ok(())
}

/// The next monotonic seq of a raw session.
///
/// The store assigns the position in the history; a caller never chooses it.
pub(crate) fn next_session_seq(
    conn: &Connection,
    session_id: EntityId,
) -> Result<i64, ShenkoError> {
    conn.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_events WHERE session_id = ?1",
        params![session_id.to_string()],
        |r| r.get(0),
    )
    .map_err(storage_error)
}

/// Append one fact to a raw session at the store-assigned next seq.
///
/// This is the internal transaction append helper: it writes inside the
/// caller's transaction, so a fact and the state change that produced it
/// commit or roll back together.
pub(crate) fn append_fact(
    conn: &Connection,
    session_id: EntityId,
    event_type: &str,
    data: String,
) -> Result<StoredSessionEvent, ShenkoError> {
    let seq = next_session_seq(conn, session_id)?;
    let event = RawSessionEvent {
        event_id: EntityId::new(),
        session_id,
        seq,
        event_type: event_type.to_owned(),
        role: role_of(event_type).to_owned(),
        data,
    };
    write_session_event(conn, &event)?;
    Ok(StoredSessionEvent {
        event_id: event.event_id,
        session_id,
        seq,
        event_type: event.event_type,
        role: event.role,
        data: event.data,
    })
}

fn append_identified_fact(
    conn: &Connection,
    session_id: EntityId,
    fact_id: EntityId,
    event_type: &str,
    data: String,
) -> Result<(StoredSessionEvent, bool), ShenkoError> {
    let role = role_of(event_type);
    if let Some(stored) = read_session_event(conn, fact_id)? {
        if stored.session_id != session_id || stored.event_type != event_type
            || stored.role != role || stored.data != data
        {
            return Err(ShenkoError::conflict(format!("raw fact {fact_id} already exists with different facts")));
        }
        return Ok((stored, false));
    }
    let event = RawSessionEvent {
        event_id: fact_id, session_id, seq: next_session_seq(conn, session_id)?,
        event_type: event_type.to_owned(), role: role.to_owned(), data,
    };
    write_session_event(conn, &event)?;
    Ok((StoredSessionEvent {
        event_id: event.event_id, session_id, seq: event.seq,
        event_type: event.event_type, role: event.role, data: event.data,
    }, true))
}

/// Ensure the immutable raw session parent of a branch exists.
///
/// The parent row is created once, with the real creation time. Later calls
/// reuse it and refuse a different agent or a different description, so the
/// parent never changes under a committed history.
pub(crate) fn ensure_raw_session(
    conn: &Connection,
    session_id: EntityId,
    agent_id: EntityId,
    data: &str,
) -> Result<(), ShenkoError> {
    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT agent_id, data FROM raw_sessions WHERE session_id = ?1",
            params![session_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(storage_error)?;
    match existing {
        Some((owner, stored)) => {
            if owner != agent_id.to_string() || stored != data {
                return Err(ShenkoError::conflict(format!(
                    "raw session {session_id} already exists with different facts"
                )));
            }
            Ok(())
        }
        None => write_raw_session(
            conn,
            &RawSession {
                session_id,
                agent_id,
                started_at: chrono::Utc::now().to_rfc3339(),
                data: data.to_owned(),
            },
        ),
    }
}

/// Stage one input fact.
///
/// A replay of the same fact is a no-op; a different fact under the same
/// source id is a conflict that rolls the whole transaction back.
fn stage_input(
    tx: &Transaction<'_>,
    session_id: EntityId,
    step: &SubjectStep,
    input: &SubjectHistoryInput,
    commit_seq: u64,
) -> Result<bool, ShenkoError> {
    if input.event_type.trim().is_empty() {
        return Err(ShenkoError::conflict("subject input needs an event type"));
    }
    if !input.data.is_object() {
        return Err(ShenkoError::conflict(
            "subject input data must be a JSON object",
        ));
    }
    let data = serde_json::to_string(&input.data)?;
    let role = role_of(&input.event_type);
    let existing: Option<(String, String, String)> = tx
        .query_row(
            "SELECT event_type, role, data FROM session_events
              WHERE event_id IN (SELECT raw_event_id FROM subject_raw_inputs
                                  WHERE step_id = ?1 AND source_event_id = ?2)",
            params![step.step_id.to_string(), input.event_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(storage_error)?;
    if let Some((event_type, stored_role, stored_data)) = existing {
        if event_type != input.event_type || stored_role != role || stored_data != data {
            return Err(ShenkoError::conflict(format!(
                "subject input {} is already staged with different facts",
                input.event_id
            )));
        }
        return Ok(false);
    }
    let seq = next_session_seq(tx, session_id)?;
    let event = RawSessionEvent {
        event_id: EntityId::new(),
        session_id,
        seq,
        event_type: input.event_type.clone(),
        role: role.to_owned(),
        data,
    };
    write_session_event(tx, &event)?;
    tx.execute(
        "INSERT INTO subject_raw_inputs
            (step_id, source_event_id, raw_event_id, session_id, seq, commit_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            step.step_id.to_string(),
            input.event_id.to_string(),
            event.event_id.to_string(),
            session_id.to_string(),
            seq,
            commit_seq as i64
        ],
    )
    .map_err(storage_error)?;
    // Staging is durable here: the recovery flag moves inside the same
    // transaction as the staging link, so a crash can never leave an input
    // staged but still queued for recovery delivery. This does not claim a
    // sealed presentation; selected receipts require sealed InputPlan membership.
    // Web/Playground/media persist complete input originals, and the subject's
    // own "subject" self-wake source shares this lane (docs/specs/subject-runtime.md#spec-13-8). Native
    // observations and native child T2 use committed S1 dispositions for
    // recovery readiness; staging must leave a Deferred observation eligible
    // for the next step.
    tx.execute(
        "UPDATE inbox_events SET consumed=1
         WHERE event_id=?1 AND consumed=0
           AND (source IN (?2, 'playground', 'subject') OR source LIKE 'media/%')",
        params![input.event_id.to_string(), crate::web_input::WEB_INPUT_SOURCE],
    )
    .map_err(storage_error)?;
    Ok(true)
}

/// Every input record staged for one step, in seq order.
pub(crate) fn staged_inputs(
    conn: &Connection,
    step_id: EntityId,
) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM session_events WHERE event_id IN
         (SELECT raw_event_id FROM subject_raw_inputs WHERE step_id = ?1) ORDER BY seq"
    )).map_err(storage_error)?;
    let mut rows = statement.query(params![step_id.to_string()]).map_err(storage_error)?;
    let mut out = Vec::new();
    while let Some(record) = rows.next().map_err(storage_error)? {
        out.push(parse_row(row(record).map_err(storage_error)?)?);
    }
    Ok(out)
}

/// One stored raw event by its id.
fn read_session_event(
    conn: &Connection,
    event_id: EntityId,
) -> Result<Option<StoredSessionEvent>, ShenkoError> {
    let found = conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM session_events WHERE event_id = ?1"),
            params![event_id.to_string()],
            row,
        )
        .optional()
        .map_err(storage_error)?;
    found.map(parse_row).transpose()
}

/// The raw event ids staged for one step, in seq order.
///
/// The link table is indexed by step and seq, so this stays cheap inside a
/// transaction: it is exactly the set of raw rows the reader will see for
/// this step.
pub(crate) fn staged_event_ids(
    conn: &Connection,
    step_id: EntityId,
) -> Result<Vec<EntityId>, ShenkoError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT raw_event_id FROM subject_raw_inputs WHERE step_id = ?1 ORDER BY seq",
        )
        .map_err(storage_error)?;
    let ids = stmt
        .query_map(params![step_id.to_string()], |r| r.get::<_, String>(0))
        .map_err(storage_error)?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(storage_error)?;
    drop(stmt);
    ids.into_iter()
        .map(|id| {
            id.parse::<EntityId>()
                .map_err(|e| ShenkoError::storage(format!("bad raw event id {id}: {e}")))
        })
        .collect()
}

impl Store {
    /// The first durable input original and its projection in the agent's
    /// current branch, whether its step is still planned, sealed or committed.
    ///
    /// A deferred event may occur in more than one step. Its first raw row
    /// remains the normalization and projection authority across upgrades;
    /// reading it neither stages an input nor acknowledges its disposition.
    pub fn first_subject_input(
        &self,
        agent_id: EntityId,
        source_event_id: EntityId,
    ) -> Result<Option<StoredSessionEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let raw_id: Option<String> = conn.query_row(
            "SELECT i.raw_event_id FROM subject_raw_inputs i
             JOIN subject_branch_heads h ON i.session_id=h.branch_id
             WHERE h.agent_id=?1 AND i.source_event_id=?2
             ORDER BY i.seq LIMIT 1",
            params![agent_id.to_string(), source_event_id.to_string()],
            |row| row.get(0),
        ).optional().map_err(storage_error)?;
        let Some(raw_id) = raw_id else { return Ok(None); };
        let id = raw_id.parse::<EntityId>()
            .map_err(|error| ShenkoError::storage(format!("bad raw event id {raw_id}: {error}")))?;
        read_session_event(&conn, id)?.map(Some).ok_or_else(|| {
            ShenkoError::conflict("staged input original raw row missing")
        })
    }

    /// The old loop carried only the last thought in memory. Admitting an
    /// upgraded committed history without its raw result would silently drop
    /// even that carry, so require explicit migration instead of guessing.
    pub fn require_subject_raw_coverage(
        &self,
        head: &SubjectBranchHead,
        step: &SubjectStep,
    ) -> Result<(), ShenkoError> {
        let object = step.model_result_ref.as_ref()
            .ok_or_else(|| ShenkoError::conflict("committed model result missing"))?;
        let conn = self.ledger.conn.lock();
        let covered: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_events
                WHERE session_id=?1 AND event_type='assistant'
                  AND json_extract(data,'$.step_id')=?2
                  AND json_extract(data,'$.model_result_ref.digest')=?3)",
            params![head.branch_id.0.to_string(), step.step_id.to_string(), object.digest.to_string()],
            |row| row.get(0),
        ).map_err(storage_error)?;
        if !covered {
            return Err(ShenkoError::conflict(
                "raw history migration required: committed model result has no branch raw fact; inference blocked",
            ));
        }
        Ok(())
    }

    /// Global mutation boundary used by S0 to detect execution-ledger changes
    /// even when they do not append a raw session event.
    pub fn current_commit_seq(&self) -> Result<u64, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let seq: i64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| row.get(0))
            .map_err(storage_error)?;
        u64::try_from(seq).map_err(|error| ShenkoError::storage(error.to_string()))
    }

    /// The read/project/seal gap must not omit a concurrently staged input or
    /// bind a request to a window that ceased to be the scope's head.
    pub(crate) fn validate_subject_context(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
    ) -> Result<(), ShenkoError> {
        let object = step.input_plan_ref.as_ref()
            .ok_or_else(|| ShenkoError::conflict("S0 input plan missing"))?;
        let input: serde_json::Value = serde_json::from_slice(&self.cas.read_object(&object.digest)?)?;
        if !crate::store::admitted_input_plan_schema(
            input.get("schema_version").and_then(serde_json::Value::as_u64),
        )
            || input.get("step_id") != Some(&serde_json::to_value(step.step_id)?)
            || input.get("step_no").and_then(serde_json::Value::as_u64) != Some(step.step_no)
        {
            return Err(ShenkoError::conflict("S0 schema/step identity mismatch"));
        }
        let frozen_wait: Option<shenko_types::subject::SemanticWait> = serde_json::from_value(
            input.get("semantic_wait").cloned()
                .ok_or_else(|| ShenkoError::conflict("S0 semantic wait snapshot missing"))?,
        )?;
        let current_wait = crate::semantic_wait::read_latest(tx, head.agent_id)?;
        if frozen_wait != current_wait {
            return Err(ShenkoError::ResnapshotRequired {
                reason: "S0 semantic wait changed during projection".into(),
            });
        }
        if let Some(wait) = current_wait.as_ref().filter(|wait| wait.is_active()) {
            let binding = input.get("provider_binding")
                .ok_or_else(|| ShenkoError::conflict("S0 semantic wait provider binding missing"))?;
            if shenko_types::object::Digest::blake3(&serde_json::to_vec(binding)?)
                != wait.model_binding_digest
            {
                return Err(ShenkoError::conflict("S0 semantic wait model binding changed"));
            }
        }
        let context: SubjectContextRef = serde_json::from_value(
            input.pointer("/context/source").cloned()
                .ok_or_else(|| ShenkoError::conflict("S0 context source missing"))?
        )?;
        let source_commit_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| row.get(0),
        ).map_err(storage_error)?;
        if u64::try_from(source_commit_seq).ok() != Some(context.source_commit_seq) {
            return Err(ShenkoError::ResnapshotRequired {
                reason: "S0 execution evidence/source commit changed".into(),
            });
        }
        if context.scope_id != EntityId(head.branch_id.0)
            || context.input_raw_event_ids != staged_event_ids(tx, step.step_id)?
        {
            return Err(ShenkoError::conflict("S0 staged input scope/set changed"));
        }
        let records = staged_inputs(tx, step.step_id)?;
        let originals = records.iter().map(|record| {
            let data: serde_json::Value = serde_json::from_str(&record.data)?;
            data.get("inbox_event").cloned()
                .ok_or_else(|| ShenkoError::conflict("staged original inbox event missing"))
        }).collect::<Result<Vec<_>, ShenkoError>>()?;
        if input.get("events").and_then(serde_json::Value::as_array) != Some(&originals) {
            return Err(ShenkoError::conflict("S0 original inbox facts changed"));
        }
        let leaf: Option<i64> = tx.query_row(
            "SELECT MAX(seq) FROM session_events WHERE session_id = ?1",
            params![context.scope_id.to_string()], |row| row.get(0),
        ).map_err(storage_error)?;
        let leaf = leaf.map(u64::try_from).transpose()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let window: Option<(String, i64, String)> = tx.query_row(
            "SELECT window_id, generation, body FROM context_windows WHERE scope_id = ?1
             ORDER BY generation DESC LIMIT 1",
            params![context.scope_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(storage_error)?;
        let (generation, stored_profile): (i64, Option<String>) = tx.query_row(
            "SELECT model_generation,profile_digest FROM context_window_scopes WHERE scope_id = ?1",
            params![context.scope_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(storage_error)?.unwrap_or((0, None));
        let profile: shenko_types::object::Digest = serde_json::from_value(
            input.pointer("/context/profile_digest").cloned()
                .ok_or_else(|| ShenkoError::conflict("S0 context profile missing"))?
        )?;
        let window = window.map(|(id, generation, body)| -> Result<(String, i64, u64), ShenkoError> {
            let body: serde_json::Value = serde_json::from_str(&body)?;
            let model_generation = body.get("model_generation").and_then(serde_json::Value::as_u64)
                .ok_or_else(|| ShenkoError::conflict("S0 window model generation missing"))?;
            Ok((id, generation, model_generation))
        }).transpose()?;
        if leaf != context.leaf_seq
            || window != Some((context.window_id.to_string(),
                i64::try_from(context.window_generation)
                    .map_err(|_| ShenkoError::conflict("S0 window generation overflow"))?,
                context.model_generation))
            || u64::try_from(generation).ok() != Some(context.model_generation)
            || stored_profile.as_deref() != Some(profile.0.as_str())
        {
            return Err(ShenkoError::conflict("S0 history/window/model generation changed"));
        }
        Ok(())
    }

    /// Bounded recovery of complete subject-input originals, read directly by
    /// the normal Planned loop when a volatile wake was lost or backpressured.
    /// Web, Playground, media and the subject's own "subject" self-wake
    /// ingress share this lane and its staging transaction; native-child
    /// task_result rows retain their existing T2 delivery path.
    ///
    /// Each row carries its stable event identity next to the original bytes:
    /// a caller that cannot decode one admitted input must be able to name the
    /// fact it left untouched instead of dropping an anonymous row.
    ///
    /// Native E1 observations remain ready until a nonDeferred S1, as
    /// native-child T2 results do in their own delivery path. Filter
    /// disposition before LIMIT so acknowledged observations cannot starve
    /// newer input.
    ///
    /// A deferred disposition is an honest RECORD that the subject did not
    /// act on an input; it does not keep that input in the queue forever.
    /// The queue advances so the subject can reach newer input, and the
    /// disposition index keeps saying which inputs were left undecided
    /// (docs/specs/subject-runtime.md#spec-57-3). An input the subject's own attention decision holds
    /// stays out of the window until the pause ends (docs/specs/subject-runtime.md#spec-57-3, AW14).
    pub fn pending_inbox_events(
        &self,
        agent_id: EntityId,
        limit: usize,
    ) -> Result<Vec<(EntityId, String)>, ShenkoError> {
        let limit = i64::try_from(limit)
            .map_err(|_| ShenkoError::conflict("pending input limit is too large"))?;
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT i.event_id, i.data FROM inbox_events i
             LEFT JOIN subject_branch_heads h ON h.agent_id=i.agent_id
             LEFT JOIN subject_event_dispositions d
               ON d.branch_id=h.branch_id AND d.event_id=i.event_id
             WHERE i.agent_id=?1 AND (
               (i.consumed=0 AND (i.source IN (?2, 'playground', 'subject') OR i.source LIKE 'media/%'))
               OR (i.source=?3 AND (d.disposition IS NULL OR d.disposition='\"deferred\"'
                    OR json_extract(d.disposition,'$.disposition')='deferred')))
             ORDER BY i.rowid LIMIT ?4",
        ).map_err(storage_error)?;
        let rows = stmt.query_map(params![agent_id.to_string(), crate::web_input::WEB_INPUT_SOURCE,
            shenko_types::task::NATIVE_EVIDENCE_OBSERVATION_SOURCE, limit],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|(event_id, data)| {
                let event_id = event_id
                    .parse::<EntityId>()
                    .map_err(|error| ShenkoError::storage(error.to_string()))?;
                Ok((event_id, data))
            })
            .collect()
    }

    /// Stage the real input facts of one planned subject step.
    ///
    /// The scope of the raw session is the branch, not a guessed global agent
    /// session. The store assigns the monotonic seq, attaches the stable
    /// step/input ownership and creates the immutable raw session parent once.
    ///
    /// The returned records are ALL input records already staged for this
    /// same planned step, in seq order. Calling it with an empty `inputs`
    /// list therefore recovers the inputs of a step after a crash or a lost
    /// inbox delivery, without losing, duplicating or acknowledging them.
    ///
    /// Staging is a durable input fact BEFORE projection and the final S0
    /// seal, not an S1 acknowledgement: the head must be the live one and the
    /// step must still be the registered planned step, so no input can be
    /// inserted into a step whose seal is already fixed.
    pub fn stage_subject_inputs(
        &self,
        expected_head: &SubjectBranchHead,
        step: &SubjectStep,
        inputs: &[SubjectHistoryInput],
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_stage_subject_inputs(expected_head, step, inputs),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_stage_subject_inputs(
        &self,
        expected_head: &SubjectBranchHead,
        step: &SubjectStep,
        inputs: &[SubjectHistoryInput],
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let head = checked_head(&tx, expected_head)?;
        validate_step_binding(&head, step)?;
        let registered = read_step(&tx, &head.branch_id, step.step_id)?
            .ok_or_else(|| ShenkoError::not_found("registered subject step"))?;
        validate_step_binding(&head, &registered)?;
        if registered.state != SubjectStepState::Planned {
            return Err(ShenkoError::conflict(
                "subject step is not planned; its inputs are already sealed",
            ));
        }
        if registered.record_revision != step.record_revision {
            return Err(ShenkoError::conflict("stale subject step revision"));
        }
        if inputs.is_empty() {
            return staged_inputs(&tx, registered.step_id);
        }
        let session_id = EntityId(head.branch_id.0);
        ensure_raw_session(
            &tx,
            session_id,
            head.agent_id,
            &raw_session_data(head.agent_id, &head.branch_id),
        )?;
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        let mut written = 0i64;
        for input in inputs {
            if stage_input(&tx, session_id, &registered, input, commit_seq.0)? {
                written += 1;
            }
        }
        let staged = staged_inputs(&tx, registered.step_id)?;
        self.record_subject_commit(
            &tx,
            CommitRef::new(self.ledger.ledger_id(), commit_seq),
            written,
        )?;
        tx.commit().map_err(storage_error)?;
        Ok(staged)
    }

    /// Append one raw fact to the branch's raw session in its own
    /// transaction.
    ///
    /// `fact_id` is the identity of the fact: it must come from the actual
    /// receipt or evidence the fact describes, never from a fresh random id
    /// per call. The same id with the same complete facts is a no-op that
    /// returns the stored row, the same id with changed facts is a conflict,
    /// and two independent facts with equal text stay two rows.
    ///
    /// The store assigns the next monotonic seq and applies the same replay
    /// and conflict rules as every other raw write. The subject loop reuses
    /// it after integration to record a real tool result; a fact without a
    /// real operation record stays `unknown` for the reader, never success.
    pub fn append_subject_raw_fact(
        &self,
        expected_head: &SubjectBranchHead,
        fact_id: EntityId,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Result<StoredSessionEvent, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_append_subject_raw_fact(expected_head, fact_id, event_type, data),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_append_subject_raw_fact(
        &self,
        expected_head: &SubjectBranchHead,
        fact_id: EntityId,
        event_type: &str,
        data: &serde_json::Value,
    ) -> Result<StoredSessionEvent, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let head = checked_head(&tx, expected_head)?;
        let session_id = EntityId(head.branch_id.0);
        ensure_raw_session(
            &tx,
            session_id,
            head.agent_id,
            &raw_session_data(head.agent_id, &head.branch_id),
        )?;
        let (fact, written) = append_identified_fact(
            &tx, session_id, fact_id, event_type, serde_json::to_string(data)?,
        )?;
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        self.record_subject_commit(&tx, CommitRef::new(self.ledger.ledger_id(), commit_seq), i64::from(written))?;
        tx.commit().map_err(storage_error)?;
        Ok(fact)
    }

    /// Commit one runtime-error fact to the agent's current raw session —
    /// docs/specs/cognition-units.md#spec-45-14. The session is resolved inside the write transaction from
    /// the durable head row, so a caller holding a stale head or running on a
    /// broken path still appends to the same committed history the subject
    /// reads. The entry is bounded and credential-looking spans are redacted
    /// before they become durable.
    ///
    /// The journal write is its own transaction and never joins the failing
    /// operation's fate. `journal_runtime_error` additionally refuses to
    /// propagate a journal failure: it increments `error_journal_dropped`,
    /// which later committed entries surface to readers.
    pub fn record_runtime_error(
        &self,
        agent_id: EntityId,
        context: RuntimeErrorContext,
    ) -> Result<StoredSessionEvent, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_record_runtime_error(agent_id, &context),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// Fail-soft entry point for error paths: a journal failure is counted
    /// and traced, never propagated into the error path that produced it.
    pub fn journal_runtime_error(&self, agent_id: EntityId, context: RuntimeErrorContext) {
        if let Err(error) = self.record_runtime_error(agent_id, context) {
            let dropped =
                self.error_journal_dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::warn!(
                %agent_id, dropped, %error,
                "runtime error journal write failed; counting instead of propagating"
            );
        }
    }

    fn try_record_runtime_error(
        &self,
        agent_id: EntityId,
        context: &RuntimeErrorContext,
    ) -> Result<StoredSessionEvent, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        // The durable head row owns the current raw session. A caller's
        // in-memory head can be stale exactly when the journal must write;
        // the raw session of the recorded branch is the correct scope even
        // after the head advanced.
        let branch: Option<String> = tx
            .query_row(
                "SELECT branch_id FROM subject_branch_heads WHERE agent_id=?1",
                [agent_id.to_string()],
                |r| r.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let branch = branch.ok_or_else(|| {
            ShenkoError::not_found(format!("no subject branch for agent {agent_id}"))
        })?;
        let branch_id = BranchId(
            branch
                .parse::<EntityId>()
                .map_err(|e| ShenkoError::storage(format!("bad branch id {branch}: {e}")))?
                .0,
        );
        let session_id = EntityId(branch_id.0);
        ensure_raw_session(
            &tx,
            session_id,
            agent_id,
            &raw_session_data(agent_id, &branch_id),
        )?;
        let (message, message_truncated) = journal_message(&context.message);
        let error_digest = format!("blake3:{}", blake3::hash(context.message.as_bytes()).to_hex());
        let extra = journal_extra(&context.extra);
        let data = serde_json::json!({
            "v": 1,
            "kind": bound_chars(&redact_journal_text(&context.kind), 128).0,
            "source_module": bound_chars(&redact_journal_text(&context.source_module), 256).0,
            "message": message,
            "message_truncated": message_truncated,
            "step_no": context.step_no,
            "task_id": context.task_id,
            "error_digest": error_digest,
            "occurred_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "journal_dropped_total": self.error_journal_dropped.load(Ordering::Relaxed),
            "extra": extra,
        });
        let fact = append_fact(
            &tx,
            session_id,
            RUNTIME_ERROR_EVENT_TYPE,
            serde_json::to_string(&data)?,
        )?;
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        self.record_subject_commit(
            &tx,
            CommitRef::new(self.ledger.ledger_id(), commit_seq),
            1,
        )?;
        tx.commit().map_err(storage_error)?;
        Ok(fact)
    }

    /// Capture a real local-memory or publication receipt in its effect's
    /// transaction. Native-child T2 arrives through its durable inbox result.
    pub(crate) fn append_intent_result_fact(
        &self,
        tx: &Transaction<'_>,
        intent_id: EntityId,
        fact_id: EntityId,
        evidence: &serde_json::Value,
    ) -> Result<(), ShenkoError> {
        let (scope, agent): (String, String) = tx.query_row(
            "SELECT s.branch_id,i.agent_id FROM subject_intents i
             JOIN subject_steps s ON s.step_id=i.step_id WHERE i.intent_id=?1",
            params![intent_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(storage_error)?;
        let scope: EntityId = scope.parse().map_err(|error| ShenkoError::storage(format!("bad raw scope: {error}")))?;
        let agent: EntityId = agent.parse().map_err(|error| ShenkoError::storage(format!("bad raw agent: {error}")))?;
        ensure_raw_session(tx, scope, agent, &raw_session_data(agent, &BranchId(scope.0)))?;
        let data = serde_json::json!({
            "v":1, "text":evidence.to_string(), "call_id":intent_id.to_string(),
            "intent_id":intent_id.to_string(), "source":"subject_intent_receipt",
            "evidence":evidence,
        });
        append_identified_fact(tx, scope, fact_id, "tool_result", serde_json::to_string(&data)?)?;
        Ok(())
    }

    /// The exact sealed text of a recorded model result.
    fn sealed_model_text(&self, step: &SubjectStep) -> Result<String, ShenkoError> {
        let object = step
            .model_result_ref
            .as_ref()
            .ok_or_else(|| ShenkoError::conflict("missing recorded model result"))?;
        let bytes = self.cas.read_object(&object.digest)?;
        if object.locator.is_empty()
            || bytes.len() as u64 != object.bytes
            || shenko_types::object::Digest::blake3(&bytes) != object.digest
        {
            return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
        }
        String::from_utf8(bytes).map_err(|e| ShenkoError::storage(e.to_string()))
    }

    /// Append the recorded model result of a step to the branch raw history.
    ///
    /// The sealed text and its provenance join the history in the SAME
    /// transaction as the ResultRecorded transition, so a result that the
    /// protocol later rejects is still recorded as what the model actually
    /// produced.
    pub(crate) fn append_model_result_fact(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
    ) -> Result<(), ShenkoError> {
        let session_id = EntityId(head.branch_id.0);
        ensure_raw_session(
            tx,
            session_id,
            head.agent_id,
            &raw_session_data(head.agent_id, &head.branch_id),
        )?;
        let text = self.sealed_model_text(step)?;
        let data = serde_json::json!({
            "v": 1,
            "text": text,
            "step_id": step.step_id.to_string(),
            "step_no": step.step_no,
            "request_id": step.active_request_id.map(|id| id.to_string()),
            "model_result_ref": step.model_result_ref,
            "model_outcome_ref": step.model_outcome_ref,
        });
        append_fact(
            tx,
            session_id,
            "assistant",
            serde_json::to_string(&data)?,
        )?;
        Ok(())
    }

    /// Append the canonical runtime rejection feedback at R0.
    ///
    /// The feedback joins the history in the SAME transaction as the
    /// retirement and the correction step, so rejected or incomplete output
    /// can never be mistaken for accepted work.
    pub(crate) fn append_rejection_fact(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        retired: &SubjectStep,
        rejection: &ControlOutputRejection,
    ) -> Result<(), ShenkoError> {
        let session_id = EntityId(head.branch_id.0);
        ensure_raw_session(
            tx,
            session_id,
            head.agent_id,
            &raw_session_data(head.agent_id, &head.branch_id),
        )?;
        let data = serde_json::json!({
            "v": 1,
            "text": control_rejection_feedback(rejection),
            "step_id": retired.step_id.to_string(),
            "supersedes_step_id": rejection.supersedes_step_id.to_string(),
            "replacement_step_id": rejection.replacement_step_id.to_string(),
            "request_id": rejection.request_id.to_string(),
            "model_result_ref": rejection.model_result_ref,
            "model_outcome_ref": rejection.model_outcome_ref,
            "rejection": {
                "code": rejection.error.code,
                "scope": rejection.error.scope,
                "retry": rejection.error.retry,
                "details": rejection.error.details,
            },
        });
        append_fact(
            tx,
            session_id,
            "user",
            serde_json::to_string(&data)?,
        )?;
        Ok(())
    }

    /// Append the raw tool call facts of the intents committed by one step.
    ///
    /// The calls are written in the SAME transaction as the committed intent
    /// rows, so a rolled back S1 leaves neither. The operation status is not
    /// part of the fact: without a real ledger/operation linkage the reader
    /// keeps the call `unknown`.
    pub(crate) fn append_committed_intent_facts(
        &self,
        tx: &Transaction<'_>,
        head: &SubjectBranchHead,
        step: &SubjectStep,
        decision: &StepDecision,
    ) -> Result<(), ShenkoError> {
        let session_id = EntityId(head.branch_id.0);
        ensure_raw_session(
            tx,
            session_id,
            head.agent_id,
            &raw_session_data(head.agent_id, &head.branch_id),
        )?;
        // The sealed decision carries the arguments of every committed intent.
        let args_ref = step
            .decision_ref
            .as_ref()
            .map(|object| object.digest.to_string())
            .ok_or_else(|| ShenkoError::conflict("committed intent decision missing"))?;
        let append = |kind: &str, intent_id: EntityId, text: &str| -> Result<(), ShenkoError> {
            let data = serde_json::json!({
                "v": 1,
                "text": text,
                "call_id": intent_id.to_string(),
                "tool": kind,
                "args_ref": args_ref,
                "intent_id": intent_id.to_string(),
                "intent_kind": kind,
                "source": "subject_step",
                "step_id": step.step_id.to_string(),
            });
            append_fact(
                tx,
                session_id,
                "tool_call",
                serde_json::to_string(&data)?,
            )?;
            Ok(())
        };
        for intent in &decision.delegated_task_intents {
            append("delegation", intent.intent_id, &intent.goal)?;
        }
        for intent in &decision.memory_intents {
            let kind = match intent.kind {
                MemoryIntentKind::Save => "save",
                MemoryIntentKind::Correct => "correct",
                MemoryIntentKind::Forget => "forget",
                MemoryIntentKind::Recall => "recall",
            };
            append("memory", intent.intent_id, &format!("memory {kind} intent"))?;
        }
        for intent in &decision.publication_intents {
            append("publication", intent.intent_id, &intent.content)?;
        }
        // Semantic wait is a subject policy decision, not a tool call. Its
        // original control is already in the recorded assistant result; S1
        // stores the outcome in the typed wait receipt and intent rows.
        Ok(())
    }
}
