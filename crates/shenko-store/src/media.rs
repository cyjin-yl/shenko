//! Durable transcript revision ingress for continuous media — SPEC §15.2, §20.
//!
//! One typed admission records the raw provider revision AND the segment's
//! latest effective observation in a single ledger commit. Idempotent replays
//! return the original receipt with the same bound subject event id; a
//! different fact under the same identity is a typed conflict, never a silent
//! overwrite. Out-of-order revisions are retained as explicit `superseded`
//! raw facts; a revision applied over an already-final segment is an explicit
//! `final_correction`, so contradictions stay visible instead of merging into
//! one invented utterance.
//!
//! The store never produces transcript text: only real provider output is
//! admitted, and a caller-supplied `Sealed` audio object must already exist in
//! CAS (object first, metadata second — §41.2.2). Speaker claims are stored
//! verbatim with confidence/correction/unknown and carry no identity
//! authority.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::media::*;

use crate::store::{Store, storage_error};

/// Additive durable tables; never rewrites already committed rows.
///
/// `open_event_id`/`end_event_id` bind the durable subject inbox event of the
/// open/terminal notice so a replay returns the SAME event identity for
/// consumption reconciliation, exactly like `media_segment_revisions.event_id`.
pub const MEDIA_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS media_streams (
    stream_key TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    account TEXT NOT NULL,
    stream_id TEXT NOT NULL,
    open_id TEXT,
    open_event_id TEXT,
    end_id TEXT,
    end_event_id TEXT,
    state TEXT NOT NULL,
    opened_at_ms INTEGER NOT NULL,
    ended_at_ms INTEGER,
    end_reason TEXT,
    implicit INTEGER NOT NULL DEFAULT 0,
    descriptor TEXT NOT NULL DEFAULT '{}',
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_media_streams_agent
    ON media_streams(agent_id, state);

CREATE TABLE IF NOT EXISTS media_segment_revisions (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    segment_key TEXT NOT NULL,
    segment INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    revision_id TEXT NOT NULL,
    partial INTEGER NOT NULL,
    text TEXT NOT NULL,
    captured_at_ms INTEGER,
    speaker_json TEXT,
    audio_json TEXT,
    provider_facts_json TEXT NOT NULL DEFAULT '{}',
    data TEXT NOT NULL,
    disposition TEXT NOT NULL,
    supersedes_revision INTEGER,
    superseded_by_revision INTEGER,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, revision_id),
    UNIQUE (segment_key, revision)
);
CREATE INDEX IF NOT EXISTS idx_media_revisions_segment
    ON media_segment_revisions(segment_key, revision);
CREATE INDEX IF NOT EXISTS idx_media_revisions_event
    ON media_segment_revisions(event_id);
"#;

fn media_error(code: &str, retry: RetryStrategy, message: impl Into<String>) -> ShenkoError {
    ShenkoError::Protocol(ProtocolError {
        code: code.to_owned(),
        scope: ErrorScope::Inbox,
        retry,
        operation_id: None,
        commit_ref: None,
        details: serde_json::json!({ "detail": message.into() }),
    })
}

fn invalid(message: impl Into<String>) -> ShenkoError {
    media_error(
        "media_invalid_input",
        RetryStrategy::NeverAutomatically,
        message,
    )
}

fn identity_field(value: &str, name: &str) -> Result<(), ShenkoError> {
    if value.trim().is_empty() {
        return Err(invalid(format!("media identity field {name} is empty")));
    }
    if value.contains(MEDIA_KEY_SEPARATOR) {
        return Err(invalid(format!(
            "media identity field {name} contains the reserved key separator"
        )));
    }
    Ok(())
}

fn validate_stream(stream: &MediaStreamDescriptor) -> Result<(), ShenkoError> {
    identity_field(&stream.provider, "provider")?;
    identity_field(&stream.account, "account")?;
    identity_field(&stream.stream_id, "stream_id")?;
    Ok(())
}

fn validate_revision(input: &MediaRevisionInput) -> Result<(), ShenkoError> {
    identity_field(&input.revision_id, "revision_id")?;
    validate_stream(&input.stream)?;
    if input.text.trim().is_empty() {
        return Err(invalid("transcript revision text is empty"));
    }
    if let Some(claim) = &input.speaker {
        if let Some(confidence) = claim.confidence {
            if !confidence.is_finite() || !(0.0..=1.0).contains(&confidence) {
                return Err(invalid("speaker confidence outside [0.0, 1.0]"));
            }
        }
        if claim
            .speaker
            .as_deref()
            .is_some_and(|speaker| speaker.trim().is_empty())
        {
            return Err(invalid("speaker claim tag is empty"));
        }
    }
    if let Some(MediaAudioRef::Provider(provider)) = &input.audio {
        if provider.locator.trim().is_empty() {
            return Err(invalid("provider audio locator is empty"));
        }
    }
    Ok(())
}

fn stream_state_name(state: MediaStreamState) -> &'static str {
    match state {
        MediaStreamState::Open => "open",
        MediaStreamState::Cancelled => "cancelled",
        MediaStreamState::Disconnected => "disconnected",
        MediaStreamState::Ended => "ended",
    }
}

fn parse_stream_state(name: &str) -> Result<MediaStreamState, ShenkoError> {
    match name {
        "open" => Ok(MediaStreamState::Open),
        "cancelled" => Ok(MediaStreamState::Cancelled),
        "disconnected" => Ok(MediaStreamState::Disconnected),
        "ended" => Ok(MediaStreamState::Ended),
        other => Err(ShenkoError::storage(format!(
            "stored media stream state {other:?} is unknown"
        ))),
    }
}

fn disposition_name(disposition: MediaRevisionDisposition) -> &'static str {
    match disposition {
        MediaRevisionDisposition::Applied => "applied",
        MediaRevisionDisposition::AppliedSupersedes => "applied_supersedes",
        MediaRevisionDisposition::FinalCorrection => "final_correction",
        MediaRevisionDisposition::Superseded => "superseded",
    }
}

fn parse_disposition(name: &str) -> Result<MediaRevisionDisposition, ShenkoError> {
    match name {
        "applied" => Ok(MediaRevisionDisposition::Applied),
        "applied_supersedes" => Ok(MediaRevisionDisposition::AppliedSupersedes),
        "final_correction" => Ok(MediaRevisionDisposition::FinalCorrection),
        "superseded" => Ok(MediaRevisionDisposition::Superseded),
        other => Err(ShenkoError::storage(format!(
            "stored media revision disposition {other:?} is unknown"
        ))),
    }
}

fn parse_entity(text: &str, what: &str) -> Result<EntityId, ShenkoError> {
    Ok(EntityId(text.parse().map_err(|error| {
        ShenkoError::storage(format!("stored media {what}: {error}"))
    })?))
}

struct StoredStreamRow {
    stream: MediaStreamDescriptor,
    agent_id: EntityId,
    state: MediaStreamState,
    opened_at_ms: i64,
    open_id: Option<String>,
    open_event_id: Option<EntityId>,
    end: Option<MediaStreamEnd>,
    end_event_id: Option<EntityId>,
    implicit: bool,
    descriptor: serde_json::Map<String, serde_json::Value>,
    commit_seq: u64,
}

fn read_stream_row(
    conn: &Connection,
    stream_key: &str,
) -> Result<Option<StoredStreamRow>, ShenkoError> {
    conn.query_row(
        "SELECT provider, account, stream_id, agent_id, state, opened_at_ms,
                open_id, open_event_id, end_id, end_event_id,
                ended_at_ms, end_reason, implicit, descriptor, commit_seq
           FROM media_streams WHERE stream_key = ?1",
        params![stream_key],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, i64>(14)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(
        |(
            provider,
            account,
            stream_id,
            agent,
            state,
            opened,
            open_id,
            open_event,
            end_id,
            end_event,
            ended,
            reason,
            implicit,
            descriptor,
            commit_seq,
        )| {
            let state = parse_stream_state(&state)?;
            let end = match end_id {
                Some(end_id) => Some(MediaStreamEnd {
                    end_id,
                    stream: MediaStreamDescriptor {
                        provider: provider.clone(),
                        account: account.clone(),
                        stream_id: stream_id.clone(),
                    },
                    state,
                    ended_at_ms: ended,
                    reason,
                }),
                None => None,
            };
            Ok(StoredStreamRow {
                stream: MediaStreamDescriptor {
                    provider,
                    account,
                    stream_id,
                },
                agent_id: parse_entity(&agent, "agent id")?,
                state,
                opened_at_ms: opened,
                open_id,
                open_event_id: open_event
                    .map(|text| parse_entity(&text, "open event id"))
                    .transpose()?,
                end,
                end_event_id: end_event
                    .map(|text| parse_entity(&text, "end event id"))
                    .transpose()?,
                implicit: implicit != 0,
                descriptor: serde_json::from_str(&descriptor).map_err(ShenkoError::Serde)?,
                commit_seq: commit_seq as u64,
            })
        },
    )
    .transpose()
}

fn insert_stream(
    conn: &Connection,
    stream_key: &str,
    agent_id: EntityId,
    stream: &MediaStreamDescriptor,
    state: MediaStreamState,
    opened_at_ms: i64,
    open_id: Option<&str>,
    open_event_id: Option<EntityId>,
    end: Option<&MediaStreamEnd>,
    end_event_id: Option<EntityId>,
    implicit: bool,
    descriptor: &serde_json::Map<String, serde_json::Value>,
    commit_seq: u64,
) -> Result<(), ShenkoError> {
    conn.execute(
        "INSERT INTO media_streams
            (stream_key, agent_id, provider, account, stream_id,
             open_id, open_event_id, end_id, end_event_id,
             state, opened_at_ms, ended_at_ms, end_reason,
             implicit, descriptor, commit_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            stream_key,
            agent_id.to_string(),
            stream.provider,
            stream.account,
            stream.stream_id,
            open_id,
            open_event_id.map(|id| id.to_string()),
            end.map(|end| end.end_id.as_str()),
            end_event_id.map(|id| id.to_string()),
            stream_state_name(state),
            opened_at_ms,
            end.and_then(|end| end.ended_at_ms),
            end.and_then(|end| end.reason.clone()),
            i64::from(implicit),
            serde_json::to_string(descriptor).map_err(ShenkoError::Serde)?,
            commit_seq as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

fn to_stream(row: StoredStreamRow) -> MediaStream {
    MediaStream {
        stream: row.stream,
        agent_id: row.agent_id,
        state: row.state,
        opened_at_ms: row.opened_at_ms,
        end: row.end,
        implicit: row.implicit,
        descriptor: row.descriptor,
        commit_seq: row.commit_seq,
    }
}

fn commit_ref(ledger_id: EntityId, seq: u64) -> CommitRef {
    CommitRef::new(ledger_id, LedgerSeq(seq))
}

/// The effective revision of one segment: newest non-superseded row.
/// `disposition != 'superseded'` guards the rare case where a late out-of-order
/// row carries no superseded_by link.
fn effective_row_id(conn: &Connection, segment_key: &str) -> Result<Option<String>, ShenkoError> {
    conn.query_row(
        "SELECT revision_id FROM media_segment_revisions
          WHERE segment_key = ?1 AND superseded_by_revision IS NULL
            AND disposition != 'superseded'
          ORDER BY revision DESC LIMIT 1",
        params![segment_key],
        |row| row.get(0),
    )
    .optional()
    .map_err(storage_error)
}

fn stored_revision(
    conn: &Connection,
    ledger_id: EntityId,
    stream_key: &str,
    revision_id: &str,
) -> Result<Option<StoredMediaRevision>, ShenkoError> {
    conn.query_row(
        "SELECT segment, revision, partial, text, captured_at_ms, speaker_json,
                audio_json, provider_facts_json, disposition, supersedes_revision,
                superseded_by_revision, event_id, admitted_at_ms, commit_seq
           FROM media_segment_revisions
          WHERE stream_key = ?1 AND revision_id = ?2",
        params![stream_key, revision_id],
        |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<u64>>(9)?,
                row.get::<_, Option<u64>>(10)?,
                row.get::<_, String>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(
        |(
            segment,
            revision,
            partial,
            text,
            captured,
            speaker,
            audio,
            facts,
            disposition,
            supersedes,
            superseded_by,
            event,
            admitted,
            seq,
        )| {
            Ok(StoredMediaRevision {
                revision_id: revision_id.to_owned(),
                segment,
                revision,
                text,
                partial,
                captured_at_ms: captured,
                speaker: speaker
                    .map(|json| serde_json::from_str(&json))
                    .transpose()
                    .map_err(ShenkoError::Serde)?,
                audio: audio
                    .map(|json| serde_json::from_str(&json))
                    .transpose()
                    .map_err(ShenkoError::Serde)?,
                provider_facts: serde_json::from_str(&facts).map_err(ShenkoError::Serde)?,
                disposition: parse_disposition(&disposition)?,
                supersedes_revision: supersedes,
                superseded_by_revision: superseded_by,
                event_id: parse_entity(&event, "event id")?,
                admitted_at_ms: admitted,
                admitted_commit: commit_ref(ledger_id, seq as u64),
            })
        },
    )
    .transpose()
}

/// The effective (newest, not superseded) revision of one segment.
pub(crate) fn effective_revision(
    conn: &Connection,
    ledger_id: EntityId,
    stream_key: &str,
    segment: u64,
) -> Result<Option<StoredMediaRevision>, ShenkoError> {
    let segment_key = format!("{}{}{}", stream_key, MEDIA_KEY_SEPARATOR, segment);
    match effective_row_id(conn, &segment_key)? {
        Some(revision_id) => stored_revision(conn, ledger_id, stream_key, &revision_id),
        None => Ok(None),
    }
}

fn segment_count(conn: &Connection, stream_key: &str) -> Result<u64, ShenkoError> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT segment) FROM media_segment_revisions WHERE stream_key = ?1",
            params![stream_key],
            |row| row.get(0),
        )
        .map_err(storage_error)?;
    u64::try_from(count).map_err(|error| ShenkoError::storage(error.to_string()))
}

impl Store {
    /// Admit one typed transcript revision (§15.2, §20).
    ///
    /// Atomically commits the raw revision row and the segment's latest
    /// effective observation. `replayed` receipts carry the SAME bound event
    /// id; callers deliver `notify` to the subject inbox at most once and may
    /// always reconcile consumption via `event_disposition_commit`.
    ///
    /// `Sealed` audio objects are verified present in CAS BEFORE the metadata
    /// commit; provider-local handles are recorded verbatim only.
    pub fn admit_media_revision(
        &self,
        agent_id: EntityId,
        input: MediaRevisionInput,
    ) -> Result<MediaAdmission, ShenkoError> {
        validate_revision(&input)?;
        if let Some(MediaAudioRef::Sealed { object }) = &input.audio {
            let exists = self.cas.object_exists(&object.digest)
                && self.cas.object_size(&object.digest) == Some(object.bytes);
            if !exists {
                return Err(media_error(
                    "media_audio_object_missing",
                    RetryStrategy::AfterReconcile,
                    "sealed audio object is not present in this ledger's CAS",
                ));
            }
        }
        self.ledger.with_mutation(
            || self.try_admit_media_revision(agent_id, &input),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_media_revision(
        &self,
        agent_id: EntityId,
        input: &MediaRevisionInput,
    ) -> Result<MediaAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(&input.stream);
        let segment_key = media_segment_key(&input.stream, input.segment);
        let data = serde_json::to_string(input).map_err(ShenkoError::Serde)?;

        // Idempotent replay: same durable identity must replay byte-identical
        // facts; anything else is an explicit contradiction, never an overwrite.
        if let Some(stored) = stored_revision(&tx, ledger_id, &stream_key, &input.revision_id)? {
            let stored_data: String = tx
                .query_row(
                    "SELECT data FROM media_segment_revisions
                      WHERE stream_key = ?1 AND revision_id = ?2",
                    params![stream_key, input.revision_id],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if stored_data != data {
                return Err(media_error(
                    "media_revision_conflict",
                    RetryStrategy::NeverAutomatically,
                    "a different fact already holds this revision identity",
                ));
            }
            return Ok(MediaAdmission {
                receipt: MediaRevisionReceipt {
                    revision_id: input.revision_id.clone(),
                    disposition: stored.disposition,
                    replayed: true,
                    event_id: stored.event_id,
                    commit_ref: stored.admitted_commit,
                },
                notify: None,
            });
        }
        // A provider may reuse a revision number only by taking the segment's
        // (segment, revision) slot explicitly; refuse before touching state.
        let slot_taken: Option<String> = tx
            .query_row(
                "SELECT revision_id FROM media_segment_revisions
                  WHERE segment_key = ?1 AND revision = ?2",
                params![segment_key, input.revision],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        if let Some(existing) = slot_taken {
            return Err(media_error(
                "media_revision_conflict",
                RetryStrategy::NewRevision,
                format!(
                    "segment {} revision {} already committed as {existing}",
                    input.segment, input.revision
                ),
            ));
        }

        // One ledger commit sequence for raw fact + effective update + stream.
        let seq = self.ledger.allocate_seq(&tx)?;
        let stream_state = match read_stream_row(&tx, &stream_key)? {
            Some(row) => {
                if row.agent_id != agent_id {
                    return Err(media_error(
                        "media_stream_agent_mismatch",
                        RetryStrategy::NeverAutomatically,
                        "media stream belongs to a different subject",
                    ));
                }
                row.state
            }
            None => {
                insert_stream(
                    &tx,
                    &stream_key,
                    agent_id,
                    &input.stream,
                    MediaStreamState::Open,
                    chrono::Utc::now().timestamp_millis(),
                    None,
                    None,
                    None,
                    None,
                    true,
                    &serde_json::Map::new(),
                    seq.0,
                )?;
                MediaStreamState::Open
            }
        };
        if stream_state.is_terminal() {
            return Err(media_error(
                "media_stream_closed",
                RetryStrategy::NeverAutomatically,
                format!(
                    "stream is {}; a provider continues on a new stream identity",
                    stream_state_name(stream_state)
                ),
            ));
        }

        let prior_effective = effective_row_id(&tx, &segment_key)?
            .map(|revision_id| {
                stored_revision(&tx, ledger_id, &stream_key, &revision_id)?
                    .ok_or_else(|| ShenkoError::storage("effective media revision row missing"))
            })
            .transpose()?;

        let (disposition, supersedes) = match &prior_effective {
            None => (MediaRevisionDisposition::Applied, None),
            Some(prior) => {
                if input.revision > prior.revision {
                    (
                        if prior.partial {
                            MediaRevisionDisposition::AppliedSupersedes
                        } else {
                            MediaRevisionDisposition::FinalCorrection
                        },
                        Some(prior.revision),
                    )
                } else {
                    (MediaRevisionDisposition::Superseded, None)
                }
            }
        };

        let event_id = EntityId::new();
        let admitted_at_ms = chrono::Utc::now().timestamp_millis();
        let speaker_json = input
            .speaker
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(ShenkoError::Serde)?;
        let audio_json = input
            .audio
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(ShenkoError::Serde)?;
        let facts_json =
            serde_json::to_string(&input.provider_facts).map_err(ShenkoError::Serde)?;
        tx.execute(
            "INSERT INTO media_segment_revisions
                (stream_key, segment_key, segment, revision, revision_id, partial,
                 text, captured_at_ms, speaker_json, audio_json,
                 provider_facts_json, data, disposition, supersedes_revision,
                 event_id, admitted_at_ms, commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17)",
            params![
                stream_key,
                segment_key,
                input.segment,
                input.revision,
                input.revision_id,
                i64::from(input.partial),
                input.text,
                input.captured_at_ms,
                speaker_json,
                audio_json,
                facts_json,
                data,
                disposition_name(disposition),
                supersedes.map(|revision| revision as i64),
                event_id.to_string(),
                admitted_at_ms,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;
        if let Some(previous) = supersedes {
            let updated = tx
                .execute(
                    "UPDATE media_segment_revisions SET superseded_by_revision = ?3
                      WHERE segment_key = ?1 AND revision = ?2",
                    params![segment_key, previous as i64, input.revision],
                )
                .map_err(storage_error)?;
            if updated != 1 {
                return Err(ShenkoError::storage(
                    "media supersede did not mark exactly one prior revision",
                ));
            }
        }
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;

        let record = StoredMediaRevision {
            revision_id: input.revision_id.clone(),
            segment: input.segment,
            revision: input.revision,
            text: input.text.clone(),
            partial: input.partial,
            captured_at_ms: input.captured_at_ms,
            speaker: input.speaker.clone(),
            audio: input.audio.clone(),
            provider_facts: input.provider_facts.clone(),
            disposition,
            supersedes_revision: supersedes,
            superseded_by_revision: None,
            event_id,
            admitted_at_ms,
            admitted_commit: commit_ref(ledger_id, seq.0),
        };
        Ok(MediaAdmission {
            receipt: MediaRevisionReceipt {
                revision_id: input.revision_id.clone(),
                disposition,
                replayed: false,
                event_id,
                commit_ref: commit_ref(ledger_id, seq.0),
            },
            notify: Some(MediaIngressEvent::Revision {
                event_id,
                stream: input.stream.clone(),
                record,
                prior_effective: prior_effective.map(Box::new),
            }),
        })
    }

    /// Admit an explicit stream open (§20 session scope). Replays return the
    /// original open receipt with the same bound event id; opening over a
    /// different identity fact or a terminal stream is a typed conflict.
    /// The first real descriptor may upgrade a row created implicitly by an
    /// early revision; committed raw revision rows are never rewritten.
    pub fn admit_media_stream(
        &self,
        agent_id: EntityId,
        open: MediaStreamOpen,
    ) -> Result<MediaStreamAdmission, ShenkoError> {
        identity_field(&open.open_id, "open_id")?;
        validate_stream(&open.stream)?;
        self.ledger.with_mutation(
            || self.try_admit_media_stream(agent_id, &open),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_media_stream(
        &self,
        agent_id: EntityId,
        open: &MediaStreamOpen,
    ) -> Result<MediaStreamAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(&open.stream);
        if let Some(row) = read_stream_row(&tx, &stream_key)? {
            if row.agent_id != agent_id {
                return Err(media_error(
                    "media_stream_agent_mismatch",
                    RetryStrategy::NeverAutomatically,
                    "media stream belongs to a different subject",
                ));
            }
            if row.open_id.as_deref() == Some(open.open_id.as_str()) {
                if row.descriptor != open.descriptor {
                    return Err(media_error(
                        "media_stream_conflict",
                        RetryStrategy::NeverAutomatically,
                        "a different descriptor already holds this open identity",
                    ));
                }
                return Ok(MediaStreamAdmission {
                    receipt: MediaStreamReceipt {
                        stream: open.stream.clone(),
                        state: row.state,
                        replayed: true,
                        event_id: row.open_event_id,
                        commit_ref: commit_ref(ledger_id, row.commit_seq),
                    },
                    notify: None,
                });
            }
            if row.implicit && row.state == MediaStreamState::Open && row.open_id.is_none() {
                let seq = self.ledger.allocate_seq(&tx)?;
                let event_id = EntityId::new();
                // The event reports the row's real creation time (the first
                // revision's admission), not the late open's delivery time.
                let opened_at_ms = row.opened_at_ms;
                tx.execute(
                    "UPDATE media_streams SET open_id = ?2, open_event_id = ?3,
                            descriptor = ?4, implicit = 0, commit_seq = ?5
                      WHERE stream_key = ?1",
                    params![
                        stream_key,
                        open.open_id,
                        event_id.to_string(),
                        serde_json::to_string(&open.descriptor).map_err(ShenkoError::Serde)?,
                        seq.0 as i64,
                    ],
                )
                .map_err(storage_error)?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit().map_err(storage_error)?;
                return Ok(MediaStreamAdmission {
                    receipt: MediaStreamReceipt {
                        stream: open.stream.clone(),
                        state: MediaStreamState::Open,
                        replayed: false,
                        event_id: Some(event_id),
                        commit_ref: commit_ref(ledger_id, seq.0),
                    },
                    notify: Some(MediaIngressEvent::StreamOpened {
                        event_id,
                        stream: open.stream.clone(),
                        descriptor: open.descriptor.clone(),
                        opened_at_ms,
                    }),
                });
            }
            return Err(media_error(
                "media_stream_conflict",
                RetryStrategy::NeverAutomatically,
                format!(
                    "stream already exists with state {}; stream identity is not reusable",
                    stream_state_name(row.state)
                ),
            ));
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        let event_id = EntityId::new();
        let opened_at_ms = chrono::Utc::now().timestamp_millis();
        insert_stream(
            &tx,
            &stream_key,
            agent_id,
            &open.stream,
            MediaStreamState::Open,
            opened_at_ms,
            Some(open.open_id.as_str()),
            Some(event_id),
            None,
            None,
            false,
            &open.descriptor,
            seq.0,
        )?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(MediaStreamAdmission {
            receipt: MediaStreamReceipt {
                stream: open.stream.clone(),
                state: MediaStreamState::Open,
                replayed: false,
                event_id: Some(event_id),
                commit_ref: commit_ref(ledger_id, seq.0),
            },
            notify: Some(MediaIngressEvent::StreamOpened {
                event_id,
                stream: open.stream.clone(),
                descriptor: open.descriptor.clone(),
                opened_at_ms,
            }),
        })
    }

    /// Admit a terminal stream state (§20 cancellation/disconnect/end).
    ///
    /// Committed segments and their effective text are preserved untouched;
    /// the gap list in `media_transcript` stays honest. An end notice for a
    /// stream this ledger has not seen creates the row directly in the
    /// reported terminal state — the missing open is recorded, not invented.
    /// `disconnected` may later settle to `ended`/`cancelled` by the provider;
    /// `ended` and `cancelled` are immutable.
    pub fn end_media_stream(
        &self,
        agent_id: EntityId,
        end: MediaStreamEnd,
    ) -> Result<MediaStreamAdmission, ShenkoError> {
        identity_field(&end.end_id, "end_id")?;
        validate_stream(&end.stream)?;
        if !end.state.is_terminal() {
            return Err(invalid("stream end state must be terminal"));
        }
        self.ledger.with_mutation(
            || self.try_end_media_stream(agent_id, &end),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_end_media_stream(
        &self,
        agent_id: EntityId,
        end: &MediaStreamEnd,
    ) -> Result<MediaStreamAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(&end.stream);
        if let Some(row) = read_stream_row(&tx, &stream_key)? {
            if row.agent_id != agent_id {
                return Err(media_error(
                    "media_stream_agent_mismatch",
                    RetryStrategy::NeverAutomatically,
                    "media stream belongs to a different subject",
                ));
            }
            if let Some(stored) = &row.end {
                if stored == end {
                    return Ok(MediaStreamAdmission {
                        receipt: MediaStreamReceipt {
                            stream: end.stream.clone(),
                            state: row.state,
                            replayed: true,
                            event_id: row.end_event_id,
                            commit_ref: commit_ref(ledger_id, row.commit_seq),
                        },
                        notify: None,
                    });
                }
                // A disconnect may settle into the provider's real terminal
                // cause; it is a new explicit fact, never a silent rewrite.
                let can_settle = matches!(row.state, MediaStreamState::Disconnected)
                    && !matches!(end.state, MediaStreamState::Disconnected);
                if !can_settle {
                    return Err(media_error(
                        "media_stream_conflict",
                        RetryStrategy::NeverAutomatically,
                        format!(
                            "stream already terminal as {}; terminal state is immutable",
                            stream_state_name(row.state)
                        ),
                    ));
                }
            }
            let seq = self.ledger.allocate_seq(&tx)?;
            let event_id = EntityId::new();
            tx.execute(
                "UPDATE media_streams SET state = ?2, end_id = ?3, end_event_id = ?4,
                        ended_at_ms = ?5, end_reason = ?6, commit_seq = ?7
                  WHERE stream_key = ?1",
                params![
                    stream_key,
                    stream_state_name(end.state),
                    end.end_id,
                    event_id.to_string(),
                    end.ended_at_ms,
                    end.reason.clone(),
                    seq.0 as i64,
                ],
            )
            .map_err(storage_error)?;
            let segments = segment_count(&tx, &stream_key)?;
            self.ledger.record_commit(&tx, seq, 1)?;
            tx.commit().map_err(storage_error)?;
            return Ok(MediaStreamAdmission {
                receipt: MediaStreamReceipt {
                    stream: end.stream.clone(),
                    state: end.state,
                    replayed: false,
                    event_id: Some(event_id),
                    commit_ref: commit_ref(ledger_id, seq.0),
                },
                notify: Some(MediaIngressEvent::StreamState {
                    event_id,
                    stream: end.stream.clone(),
                    state: end.state,
                    reason: end.reason.clone(),
                    ended_at_ms: end.ended_at_ms,
                    segments,
                }),
            });
        }
        // Unknown stream: record the terminal fact honestly instead of
        // fabricating an open that never arrived.
        let seq = self.ledger.allocate_seq(&tx)?;
        let event_id = EntityId::new();
        insert_stream(
            &tx,
            &stream_key,
            agent_id,
            &end.stream,
            end.state,
            end.ended_at_ms
                .unwrap_or_else(|| chrono::Utc::now().timestamp_millis()),
            None,
            None,
            Some(end),
            Some(event_id),
            true,
            &serde_json::Map::new(),
            seq.0,
        )?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;
        Ok(MediaStreamAdmission {
            receipt: MediaStreamReceipt {
                stream: end.stream.clone(),
                state: end.state,
                replayed: false,
                event_id: Some(event_id),
                commit_ref: commit_ref(ledger_id, seq.0),
            },
            notify: Some(MediaIngressEvent::StreamState {
                event_id,
                stream: end.stream.clone(),
                state: end.state,
                reason: end.reason.clone(),
                ended_at_ms: end.ended_at_ms,
                segments: 0,
            }),
        })
    }

    /// The current effective revision of one segment, for projection and
    /// reconciliation reads. Committed rows only.
    pub fn media_effective_segment(
        &self,
        stream: &MediaStreamDescriptor,
        segment: u64,
    ) -> Result<Option<StoredMediaRevision>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        effective_revision(
            &conn,
            self.ledger.ledger_id(),
            &media_stream_key(stream),
            segment,
        )
    }

    /// Every retained raw revision of one segment in revision order —
    /// effective, superseded and final-correction rows alike. The raw trail
    /// is the proof a revision was retained, not merged away.
    pub fn media_revisions(
        &self,
        stream: &MediaStreamDescriptor,
        segment: u64,
    ) -> Result<Vec<StoredMediaRevision>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(stream);
        let segment_key = media_segment_key(stream, segment);
        let mut statement = conn
            .prepare(
                "SELECT revision_id FROM media_segment_revisions
                  WHERE segment_key = ?1 ORDER BY revision",
            )
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![segment_key], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        let mut revisions = Vec::with_capacity(ids.len());
        for revision_id in ids {
            let record = stored_revision(&conn, ledger_id, &stream_key, &revision_id)?
                .ok_or_else(|| ShenkoError::storage("media revision row missing"))?;
            revisions.push(record);
        }
        Ok(revisions)
    }

    /// The committed stream row, or `None` when this ledger has no fact.
    pub fn media_stream(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Option<MediaStream>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_stream_row(&conn, &media_stream_key(stream))?
            .map(to_stream)
            .map(Ok)
            .transpose()
    }

    /// Effective transcript of one stream in segment order with the honest
    /// gap list; `None` when the stream was never admitted.
    pub fn media_transcript(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Option<MediaTranscript>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(stream);
        let Some(row) = read_stream_row(&conn, &stream_key)? else {
            return Ok(None);
        };
        // Effective rows joined with full per-segment aggregates: the count
        // must cover ALL retained raw revisions, not only the effective row.
        let mut statement = conn
            .prepare(
                "SELECT e.segment, e.revision_id, a.revisions, a.final_corrections
                   FROM media_segment_revisions e
                   JOIN (
                       SELECT segment_key,
                              COUNT(*) AS revisions,
                              MAX(disposition = 'final_correction') AS final_corrections
                         FROM media_segment_revisions
                        WHERE stream_key = ?1
                        GROUP BY segment_key
                   ) a ON a.segment_key = e.segment_key
                  WHERE e.stream_key = ?1 AND e.superseded_by_revision IS NULL
                    AND e.disposition != 'superseded'
                  ORDER BY e.segment",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![stream_key], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        let mut segments = Vec::with_capacity(rows.len());
        let mut gaps = Vec::new();
        let mut previous: Option<u64> = None;
        for (segment, revision_id, count, had_correction) in rows {
            if let Some(last) = previous {
                if segment > last + 1 {
                    gaps.push((last + 1, segment - 1));
                }
            }
            previous = Some(segment);
            let effective = stored_revision(&conn, ledger_id, &stream_key, &revision_id)?
                .ok_or_else(|| ShenkoError::storage("effective media revision row missing"))?;
            segments.push(EffectiveSegment {
                segment,
                effective,
                revisions: u64::try_from(count)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?,
                had_final_correction: had_correction != 0,
            });
        }
        Ok(Some(MediaTranscript {
            stream: to_stream(row),
            gaps,
            segments,
        }))
    }
}
