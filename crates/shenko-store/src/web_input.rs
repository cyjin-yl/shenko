//! Durable admission and replay of browser input submissions — docs/specs/web-interface.md#spec-41-18.
//!
//! A submission is deduplicated by (agent_id, submission_id): a replay of the
//! same payload returns the original event and admission commit, and the same
//! key with a different payload is a conflict that never rewrites the original
//! fact. The InboxEvent row and its submission binding commit in one ledger
//! transaction, so "accepted" is a durable fact before the transport answers.
//!
//! Consumption state stays honest: `inbox_events.consumed` means the row was
//! durably staged into a planned subject step, set in the same transaction as the
//! staging link. The normal loop drains unstaged inputs in bounded batches.
//! Staging is recovery evidence, not a selected receipt: selection requires
//! the exact event and raw-event identity in the step's sealed input plan.

use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::subject::{SubjectStep, SubjectStepState};

use crate::store::{Store, storage_error};

/// Source label recorded on InboxEvent rows admitted through the WebUI.
pub const WEB_INPUT_SOURCE: &str = "web_ui";

pub(crate) const WEB_INPUT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS web_input_submissions (
    agent_id TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    payload TEXT NOT NULL,
    accepted_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, submission_id)
);
CREATE INDEX IF NOT EXISTS idx_web_input_event
    ON web_input_submissions(event_id);
CREATE INDEX IF NOT EXISTS idx_web_input_history
    ON web_input_submissions(agent_id, accepted_commit_seq DESC);
CREATE TABLE IF NOT EXISTS web_input_withdrawals (
    agent_id TEXT NOT NULL,
    submission_id TEXT NOT NULL,
    cancel_id TEXT NOT NULL,
    event_id TEXT NOT NULL,
    result TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (agent_id, submission_id, cancel_id)
);
CREATE INDEX IF NOT EXISTS idx_web_withdrawals_event
    ON web_input_withdrawals(event_id);
";

/// The durable event identity behind one submission.
pub struct WebSubmissionRecord {
    /// submission key as stored (echo for callers that parsed a partial row)
    pub submission_id: String,
    /// stable identity of the durable InboxEvent
    pub event_id: EntityId,
    /// canonical payload JSON admitted under this key
    pub payload: String,
    /// ledger commit that admitted this input
    pub accepted_commit: CommitRef,
}

pub struct WebSubmissionHistoryRow {
    pub record: WebSubmissionRecord,
    /// Original InboxEvent time, with the persisted SQL admission time as
    /// fallback for older rows. Never the time this history was read.
    pub created_at: String,
}

pub struct WebSubmissionPage {
    pub items: Vec<WebSubmissionHistoryRow>,
    pub total: u64,
    pub has_older: bool,
    pub next_before: Option<u64>,
}

pub enum WebAdmission {
    /// New input committed; caller must attempt live-loop delivery once.
    Admitted(WebSubmissionRecord),
    /// Same key and payload replayed; do not deliver again.
    Replay(WebSubmissionRecord),
    /// Same key, different payload — original fact preserved, never reused.
    Conflict,
}

/// The settled result of one withdrawal control (docs/specs/web-interface.md#spec-41-18 撤回).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebWithdrawResultKind {
    /// The input was withdrawn before any sealed presentation/S1 consumed it:
    /// the original event stays retained, but it must not participate in
    /// ordinary selection any more.
    WithdrawnBeforePresentation,
    /// The input was already selected into a sealed step or disposed by S1:
    /// the withdrawal is appended as a causally linked control fact for a
    /// later step; the original fact and decision are never rewritten.
    WithdrawalQueued,
}

/// One durable withdrawal control record (docs/specs/web-interface.md#spec-41-18).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WebWithdrawalRecord {
    /// the submission this withdrawal targets
    pub submission_id: String,
    /// stable identity of this withdrawal control
    pub cancel_id: String,
    /// the durable input event it targets
    pub event_id: EntityId,
    pub result: WebWithdrawResultKind,
    /// the ledger commit that recorded this withdrawal
    pub commit: CommitRef,
}

/// Why a withdrawal control could not be recorded. Distinct from a store
/// failure: the UI maps this to `not_admitted`, never to `temporarily_unavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebWithdrawRejection {
    /// No admitted submission exists under this key; the original fact was
    /// never admitted, so there is nothing to withdraw.
    NotAdmitted,
}

impl Store {
    /// Owner message history, newest admission first. Action submissions have
    /// their own presentation lane and are not conversation messages.
    pub fn web_submissions_page(
        &self,
        agent_id: EntityId,
        before: Option<u64>,
        limit: usize,
    ) -> Result<WebSubmissionPage, ShenkoError> {
        let limit = limit.clamp(1, 200);
        let before = before.map(i64::try_from).transpose().map_err(storage_error)?;
        let conn = self.ledger.conn.lock();
        let agent = agent_id.to_string();
        let total: u64 = conn.query_row(
            "SELECT COUNT(*) FROM web_input_submissions \
             WHERE agent_id=?1 AND json_extract(payload,'$.type')='submit'",
            [&agent], |row| row.get(0),
        ).map_err(storage_error)?;
        let mut statement = conn.prepare(
            "SELECT s.submission_id, s.event_id, s.payload, s.accepted_commit_seq, \
             COALESCE(json_extract(i.data,'$.created_at'), i.created_at, s.created_at) \
             FROM web_input_submissions s LEFT JOIN inbox_events i \
             ON i.event_id=s.event_id AND i.agent_id=s.agent_id \
             WHERE s.agent_id=?1 AND json_extract(s.payload,'$.type')='submit' \
             AND (?2 IS NULL OR s.accepted_commit_seq<?2) \
             ORDER BY s.accepted_commit_seq DESC LIMIT ?3",
        ).map_err(storage_error)?;
        let rows = statement.query_map(params![agent, before, (limit + 1) as i64], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, String>(2)?, row.get::<_, u64>(3)?, row.get::<_, String>(4)?))
        }).map_err(storage_error)?;
        let mut items = Vec::with_capacity(limit + 1);
        for row in rows {
            let (submission_id, event_id, payload, seq, created_at) = row.map_err(storage_error)?;
            items.push(WebSubmissionHistoryRow {
                record: WebSubmissionRecord {
                    submission_id,
                    event_id: event_id.parse().map_err(storage_error)?,
                    payload,
                    accepted_commit: CommitRef::new(self.ledger.ledger_id(), LedgerSeq(seq)),
                },
                created_at: crate::trajectory::utc_timestamp(&created_at).map_err(storage_error)?,
            });
        }
        let has_older = items.len() > limit;
        items.truncate(limit);
        let next_before = has_older.then(|| items.last().unwrap().record.accepted_commit.seq.0);
        Ok(WebSubmissionPage { items, total, has_older, next_before })
    }

    /// Admit a browser input durably before any transport receipt. The
    /// InboxEvent and its submission binding commit atomically; a replayed
    /// identical payload replays the original record without another input.
    pub fn admit_web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        payload: &str,
        event: &shenko_types::store::RawInboxEvent,
    ) -> Result<WebAdmission, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_admit_web_submission(agent_id, submission_id, payload, event),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        payload: &str,
        event: &shenko_types::store::RawInboxEvent,
    ) -> Result<WebAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(record) =
            read_web_submission(&tx, agent_id, submission_id, self.ledger.ledger_id())?
        {
            if record.payload != payload {
                return Ok(WebAdmission::Conflict);
            }
            tx.commit().map_err(storage_error)?;
            return Ok(WebAdmission::Replay(record));
        }
        let commit_seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO inbox_events (event_id, agent_id, kind, source, consumed, data)
             VALUES (?1, ?2, ?3, ?4, 0, ?5)",
            params![
                event.event_id.to_string(),
                event.agent_id.to_string(),
                event.kind,
                event.source,
                event.payload_json,
            ],
        )
        .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO web_input_submissions
                (agent_id, submission_id, event_id, payload, accepted_commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                agent_id.to_string(),
                submission_id,
                event.event_id.to_string(),
                payload,
                commit_seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        self.record_subject_commit(
            &tx,
            CommitRef::new(self.ledger.ledger_id(), commit_seq),
            1,
        )?;
        tx.commit().map_err(storage_error)?;
        Ok(WebAdmission::Admitted(WebSubmissionRecord {
            submission_id: submission_id.to_owned(),
            event_id: event.event_id,
            payload: payload.to_owned(),
            accepted_commit: CommitRef::new(self.ledger.ledger_id(), commit_seq),
        }))
    }

    /// The durable record behind one submission key, for reconnect queries.
    pub fn web_submission(
        &self,
        agent_id: EntityId,
        submission_id: &str,
    ) -> Result<Option<WebSubmissionRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_web_submission(&conn, agent_id, submission_id, self.ledger.ledger_id())
    }

    /// Record one withdrawal control for an admitted browser input — the
    /// durable half of docs/specs/web-interface.md#spec-41-18 撤回, classified in ONE Immediate
    /// transaction against the durable facts that decide it:
    /// `withdrawal_queued` when (a) an S1 disposition already consumed the
    /// event, (b) the event is staged into a step that left `Planned` (a
    /// sealed presentation), or (c) the event is staged into a live `Planned`
    /// step (staging binds it to that step's input plan — queuing preserves
    /// the fold the next S0 needs); otherwise
    /// `withdrawn_before_presentation`, which excludes the event from future
    /// selection without deleting any history. Idempotent on
    /// (agent_id, submission_id, cancel_id): the same key replays the
    /// original record. Nothing rewrites the original event or a committed
    /// decision; a queued withdrawal is folded at the next S0 and cited by
    /// cancel_id in that step's disposition.
    pub fn submit_web_withdrawal(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        cancel_id: &str,
    ) -> Result<Result<WebWithdrawalRecord, WebWithdrawRejection>, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_submit_web_withdrawal(agent_id, submission_id, cancel_id),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_submit_web_withdrawal(
        &self,
        agent_id: EntityId,
        submission_id: &str,
        cancel_id: &str,
    ) -> Result<Result<WebWithdrawalRecord, WebWithdrawRejection>, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        if let Some(existing) = tx
            .query_row(
                "SELECT event_id, result, commit_seq FROM web_input_withdrawals
                 WHERE agent_id=?1 AND submission_id=?2 AND cancel_id=?3",
                params![agent_id.to_string(), submission_id, cancel_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?
        {
            let (event, result, seq) = existing;
            tx.commit().map_err(storage_error)?;
            let result: WebWithdrawResultKind = serde_json::from_str(&result)?;
            return Ok(Ok(WebWithdrawalRecord {
                submission_id: submission_id.to_owned(),
                cancel_id: cancel_id.to_owned(),
                event_id: event.parse::<EntityId>().map_err(storage_error)?,
                result,
                commit: CommitRef::new(
                    self.ledger.ledger_id(),
                    LedgerSeq(
                        u64::try_from(seq)
                            .map_err(|_| ShenkoError::storage("withdrawal seq overflow"))?,
                    ),
                ),
            }));
        }
        let Some(submission) =
            read_web_submission(&tx, agent_id, submission_id, self.ledger.ledger_id())?
        else {
            tx.commit().map_err(storage_error)?;
            return Ok(Err(WebWithdrawRejection::NotAdmitted));
        };
        let branch: Option<String> = tx
            .query_row(
                "SELECT branch_id FROM subject_branch_heads WHERE agent_id=?1",
                params![agent_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let disposed = match branch.as_deref() {
            Some(branch) => {
                let count: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM subject_event_dispositions
                         WHERE branch_id=?1 AND event_id=?2",
                        params![branch, submission.event_id.to_string()],
                        |row| row.get(0),
                    )
                    .map_err(storage_error)?;
                count > 0
            }
            None => false,
        };
        // (b) a sealed presentation (step left Planned) or (c) staged into a
        // live Planned step — both are `subject_raw_inputs` bindings; never
        // take the connection lock inside this transaction.
        let staged: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM subject_raw_inputs r
                 JOIN subject_steps s ON s.step_id = r.step_id
                 JOIN subject_branch_heads h ON h.branch_id = s.branch_id
                 WHERE h.agent_id=?1 AND r.source_event_id=?2",
                params![agent_id.to_string(), submission.event_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let result = if disposed || staged > 0 {
            WebWithdrawResultKind::WithdrawalQueued
        } else {
            WebWithdrawResultKind::WithdrawnBeforePresentation
        };
        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT INTO web_input_withdrawals
                (agent_id, submission_id, cancel_id, event_id, result, commit_seq)
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                agent_id.to_string(),
                submission_id,
                cancel_id,
                submission.event_id.to_string(),
                serde_json::to_string(&result)?,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        self.record_subject_commit(&tx, commit, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(Ok(WebWithdrawalRecord {
            submission_id: submission_id.to_owned(),
            cancel_id: cancel_id.to_owned(),
            event_id: submission.event_id,
            result,
            commit,
        }))
    }

    /// The latest withdrawal recorded for one submission — the durable value
    /// behind the docs/specs/web-interface.md#spec-41-18 `WebInputView.withdrawal` field.
    pub fn web_input_withdrawal(
        &self,
        agent_id: EntityId,
        submission_id: &str,
    ) -> Result<Option<WebWithdrawalRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let row = conn
            .query_row(
                "SELECT cancel_id, event_id, result, commit_seq FROM web_input_withdrawals
                 WHERE agent_id=?1 AND submission_id=?2
                 ORDER BY commit_seq DESC LIMIT 1",
                params![agent_id.to_string(), submission_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(storage_error)?;
        row.map(|(cancel, event, result, seq)| {
            let result: WebWithdrawResultKind = serde_json::from_str(&result)?;
            Ok(WebWithdrawalRecord {
                submission_id: submission_id.to_owned(),
                cancel_id: cancel,
                event_id: event.parse::<EntityId>().map_err(storage_error)?,
                result,
                commit: CommitRef::new(
                    self.ledger.ledger_id(),
                    LedgerSeq(
                        u64::try_from(seq)
                            .map_err(|_| ShenkoError::storage("withdrawal seq overflow"))?,
                    ),
                ),
            })
        })
        .transpose()
    }

    /// Whether this event was withdrawn before any presentation. The subject
    /// loop's volatile wake path checks this per merged event so a withdrawn
    /// input already copied into the bounded channel never enters a step.
    pub fn event_withdrawn_before_presentation(
        &self,
        agent_id: EntityId,
        event_id: EntityId,
    ) -> Result<bool, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM web_input_withdrawals
                 WHERE agent_id=?1 AND event_id=?2 AND result='\"withdrawn_before_presentation\"'",
                params![agent_id.to_string(), event_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        Ok(count > 0)
    }

    /// Withdrawal controls committed after `after_commit_seq`, in commit
    /// order. S0 folds queued withdrawals into the step's causal support so a
    /// withdrawn-but-selected input is dispositioned with its withdrawal
    /// (citing cancel_id) instead of being acted on blindly.
    pub fn pending_web_withdrawals(
        &self,
        agent_id: EntityId,
        after_commit_seq: u64,
    ) -> Result<Vec<WebWithdrawalRecord>, ShenkoError> {
        let after = i64::try_from(after_commit_seq)
            .map_err(|_| ShenkoError::conflict("withdrawal cursor overflow"))?;
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(
                "SELECT submission_id, cancel_id, event_id, result, commit_seq
                 FROM web_input_withdrawals
                 WHERE agent_id=?1 AND commit_seq>?2
                 ORDER BY commit_seq",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![agent_id.to_string(), after], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(|(submission, cancel, event, result, seq)| {
                let result: WebWithdrawResultKind = serde_json::from_str(&result)?;
                Ok(WebWithdrawalRecord {
                    submission_id: submission,
                    cancel_id: cancel,
                    event_id: event.parse::<EntityId>().map_err(storage_error)?,
                    result,
                    commit: CommitRef::new(
                        self.ledger.ledger_id(),
                        LedgerSeq(
                            u64::try_from(seq)
                                .map_err(|_| ShenkoError::storage("withdrawal seq overflow"))?,
                        ),
                    ),
                })
            })
            .collect()
    }

    /// The step an input was selected into, if its S0 actually sealed.
    /// `subject_raw_inputs` rows persist while a step is still `Planned` —
    /// staging alone is not selection, so a failed projection, CAS failure
    /// or resnapshot must not report `selected`. The sealed plan must belong
    /// to the actual step and contain both this raw identity and the exact
    /// admitted event. `prepare_context` admits every fresh raw ID into its
    /// projection before S0; Store's S0 validation freezes that set atomically.
    pub fn web_input_selected_step(
        &self,
        agent_id: EntityId,
        event_id: EntityId,
    ) -> Result<Option<EntityId>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT s.data, r.raw_event_id, i.data FROM subject_raw_inputs r
             JOIN subject_steps s ON s.step_id = r.step_id
             JOIN subject_branch_heads h ON h.branch_id = s.branch_id
             JOIN inbox_events i ON i.event_id = r.source_event_id
               AND i.agent_id = h.agent_id
             WHERE h.agent_id = ?1 AND r.source_event_id = ?2
               AND s.state != 'planned'
             ORDER BY r.commit_seq DESC, r.seq DESC",
        ).map_err(storage_error)?;
        let mut rows = stmt.query(params![agent_id.to_string(), event_id.to_string()])
            .map_err(storage_error)?;
        while let Some(row) = rows.next().map_err(storage_error)? {
            let step: SubjectStep = serde_json::from_str(&row.get::<_, String>(0).map_err(storage_error)?)?;
            if step.state == SubjectStepState::Planned {
                continue;
            }
            let raw_id: String = row.get(1).map_err(storage_error)?;
            let original: serde_json::Value = serde_json::from_str(
                &row.get::<_, String>(2).map_err(storage_error)?,
            )?;
            let object = step.input_plan_ref.as_ref()
                .ok_or_else(|| ShenkoError::conflict("sealed web-input step has no input plan"))?;
            let bytes = self.cas.read_object(&object.digest)?;
            if object.locator.is_empty()
                || bytes.len() as u64 != object.bytes
                || shenko_types::object::Digest::blake3(&bytes) != object.digest
            {
                return Err(ShenkoError::ObjectClosureIncomplete { missing_count: 1 });
            }
            let plan: serde_json::Value = serde_json::from_slice(&bytes)?;
            if !crate::store::admitted_input_plan_schema(
                plan.get("schema_version").and_then(serde_json::Value::as_u64),
            )
                || plan.get("step_id") != Some(&serde_json::to_value(step.step_id)?)
                || plan.get("step_no").and_then(serde_json::Value::as_u64) != Some(step.step_no)
            {
                return Err(ShenkoError::conflict("sealed web-input plan identity mismatch"));
            }
            let raw_is_selected = plan.pointer("/context/source/input_raw_event_ids")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(raw_id.as_str())));
            let event_is_selected = plan.get("events").and_then(serde_json::Value::as_array)
                .is_some_and(|events| events.contains(&original));
            if raw_is_selected && event_is_selected {
                return Ok(Some(step.step_id));
            }
        }
        Ok(None)
    }
}

fn read_web_submission(
    conn: &rusqlite::Connection,
    agent_id: EntityId,
    submission_id: &str,
    ledger_id: EntityId,
) -> Result<Option<WebSubmissionRecord>, ShenkoError> {
    conn.query_row(
        "SELECT event_id, payload, accepted_commit_seq FROM web_input_submissions
         WHERE agent_id = ?1 AND submission_id = ?2",
        params![agent_id.to_string(), submission_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(|(event_id, payload, seq)| {
        let event_id = event_id.parse::<EntityId>().map_err(|e| {
            ShenkoError::storage(format!("bad web submission event id {event_id}: {e}"))
        })?;
        Ok(WebSubmissionRecord {
            submission_id: submission_id.to_owned(),
            event_id,
            payload,
            accepted_commit: CommitRef::new(
                ledger_id,
                LedgerSeq(u64::try_from(seq).map_err(|error| {
                    ShenkoError::storage(format!("bad commit seq {seq}: {error}"))
                })?),
            ),
        })
    })
    .transpose()
}


