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
//! CAS and verify to its declared digest (object first, metadata second —
//! §41.2.2). Speaker claims are stored verbatim with
//! confidence/correction/unknown and carry no identity authority.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::ids::{EntityId, LedgerSeq};
use shenko_types::object::Digest;
use shenko_types::media::*;
use shenko_types::object::ObjectRef;

use crate::store::{Store, storage_error};

/// Additive durable tables; never rewrites already committed rows.
///
/// `open_event_id`/`end_event_id` bind the durable subject inbox event of the
/// open/terminal notice so a replay returns the SAME event identity for
/// consumption reconciliation, exactly like `media_segment_revisions.event_id`.
/// `open_commit_ref` is the immutable commit coordinate of the OPEN fact:
/// `commit_seq` tracks the stream's latest state change and moves on a
/// terminal admission, while the open receipt must keep its own event→commit
/// binding. `media_stream_ends` records every terminal notice keyed by its
/// end identity; a later settlement never rewrites an earlier notice.
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
    open_commit_ref TEXT,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_media_streams_agent
    ON media_streams(agent_id, state);

CREATE TABLE IF NOT EXISTS media_stream_ends (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    end_id TEXT NOT NULL,
    state TEXT NOT NULL,
    ended_at_ms INTEGER,
    reason TEXT,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, end_id)
);
CREATE INDEX IF NOT EXISTS idx_media_stream_ends_event
    ON media_stream_ends(event_id);

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
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, revision_id),
    UNIQUE (segment_key, revision)
);
CREATE INDEX IF NOT EXISTS idx_media_revisions_segment
    ON media_segment_revisions(segment_key, revision);
CREATE INDEX IF NOT EXISTS idx_media_revisions_event
    ON media_segment_revisions(event_id);

-- Recording control: `media_recordings` is the latest-state projection of
-- one recording, `media_recording_controls` keeps every admitted control
-- command as an immutable notice keyed by the provider's control identity.
-- A replayed control returns its ORIGINAL event id and commit; a different
-- fact under the same control identity is a typed conflict.
CREATE TABLE IF NOT EXISTS media_recordings (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    recording_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    state TEXT NOT NULL,
    started_at_ms INTEGER NOT NULL,
    stopped_at_ms INTEGER,
    scope_json TEXT NOT NULL,
    start_control_id TEXT NOT NULL,
    stop_control_id TEXT,
    reason TEXT,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, recording_id)
);
CREATE INDEX IF NOT EXISTS idx_media_recordings_event
    ON media_recordings(event_id);

CREATE TABLE IF NOT EXISTS media_recording_controls (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    control_id TEXT NOT NULL,
    recording_id TEXT NOT NULL,
    action TEXT NOT NULL,
    state TEXT NOT NULL,
    data TEXT NOT NULL,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, control_id)
);
CREATE INDEX IF NOT EXISTS idx_media_recording_controls_event
    ON media_recording_controls(event_id);

-- Priority inputs (barge-in, interjection, stop playback, urgent message)
-- are immutable notices: the same priority identity always replays its own
-- event id and commit.
CREATE TABLE IF NOT EXISTS media_priority_inputs (
    stream_key TEXT NOT NULL REFERENCES media_streams(stream_key),
    priority_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    text TEXT,
    occurred_at_ms INTEGER,
    audio_json TEXT,
    reason TEXT,
    data TEXT NOT NULL,
    event_id TEXT NOT NULL,
    admitted_at_ms INTEGER NOT NULL,
    admitted_commit_ref TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (stream_key, priority_id)
);
CREATE INDEX IF NOT EXISTS idx_media_priority_event
    ON media_priority_inputs(event_id);
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
    if i64::try_from(input.segment).is_err() || i64::try_from(input.revision).is_err() {
        return Err(invalid(
            "segment/revision exceed the supported persistent range",
        ));
    }
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

/// Verify a caller-supplied `Sealed` audio reference BEFORE the metadata
/// commit (§41.2.2): the digest must be a canonical supported one, the
/// object must resolve inside this CAS, and its bytes must hash to the
/// declared digest at the declared length. Same-length corrupted or
/// algorithm-rewritten content is never admitted as sealed audio.
fn verify_sealed_audio(store: &Store, object: &ObjectRef) -> Result<(), ShenkoError> {
    if object.digest.canonical_blake3_hex().is_none() {
        return Err(invalid(
            "sealed audio digest is not a canonical supported blake3 digest",
        ));
    }
    // The integrated CAS exposes read-back instead of a dedicated
    // verify_object: read the bytes and re-check the declared length and
    // digest, so same-length corruption is still rejected before commit.
    match store.cas.read_object(&object.digest) {
        Ok(bytes) if bytes.len() as u64 == object.bytes
            && Digest::blake3(&bytes) == object.digest => Ok(()),
        _ => Err(media_error(
            "media_audio_object_missing",
            RetryStrategy::AfterReconcile,
            "sealed audio object cannot be verified in this ledger's CAS",
        )),
}
}

/// The durable `inbox_events` envelope of one bound media event, in the same
/// serialized `InboxEvent` shape the subject's durable-input drain decodes.
/// `created_at` is the admission time; live wakes deserialize this same
/// original rather than regenerate a delivery timestamp or payload.
fn media_inbox_event_json(
    event: &MediaIngressEvent,
    source: &str,
    created_at: chrono::DateTime<chrono::Utc>,
) -> Result<String, ShenkoError> {
    #[derive(serde::Serialize)]
    struct Original<'a> {
        event_id: EntityId,
        kind: &'static str,
        payload: &'a MediaIngressEvent,
        created_at: chrono::DateTime<chrono::Utc>,
        source: &'a str,
    }
    serde_json::to_string(&Original {
        event_id: event.event_id(), kind: event.inbox_kind(),
        payload: event, created_at, source,
    })
    .map_err(ShenkoError::Serde)
}

/// Commit the durable inbox row of one bound event inside the SAME ledger
/// transaction as the media fact. This row — not the volatile queue send —
/// is what the subject's normal pending drain recovers after a lost wake.
fn insert_media_inbox_event(
    tx: &rusqlite::Transaction<'_>,
    agent_id: EntityId,
    event: &MediaIngressEvent,
) -> Result<(), ShenkoError> {
    let source = format!("media/{}", event.stream().provider);
    let data = media_inbox_event_json(event, &source, chrono::Utc::now())?;
    tx.execute(
        "INSERT INTO inbox_events(event_id, agent_id, kind, source, consumed, data)
         VALUES (?1, ?2, ?3, ?4, 0, ?5)",
        params![
            event.event_id().to_string(),
            agent_id.to_string(),
            event.inbox_kind(),
            source,
            data,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
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
    open_commit_ref: Option<CommitRef>,
    end: Option<MediaStreamEnd>,
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
                open_id, open_event_id, end_id,
                ended_at_ms, end_reason, implicit, descriptor,
                open_commit_ref, commit_seq
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
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, Option<String>>(13)?,
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
            ended,
            reason,
            implicit,
            descriptor,
            open_commit_ref,
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
                open_commit_ref: open_commit_ref
                    .map(|data| serde_json::from_str(&data).map_err(ShenkoError::Serde))
                    .transpose()?,
                end,
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
    open_commit_ref: Option<CommitRef>,
    commit_seq: u64,
) -> Result<(), ShenkoError> {
    conn.execute(
        "INSERT INTO media_streams
            (stream_key, agent_id, provider, account, stream_id,
             open_id, open_event_id, end_id, end_event_id,
             state, opened_at_ms, ended_at_ms, end_reason,
             implicit, descriptor, open_commit_ref, commit_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
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
            open_commit_ref.map(|commit| serde_json::to_string(&commit))
                .transpose().map_err(ShenkoError::Serde)?,
            commit_seq as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// One immutable terminal notice row, keyed by the provider's end identity.
fn stored_stream_end(
    conn: &Connection,
    stream_key: &str,
    end_id: &str,
) -> Result<Option<StoredMediaStreamEnd>, ShenkoError> {
    conn.query_row(
        "SELECT provider, account, stream_id, e.state, e.ended_at_ms, e.reason,
                e.event_id, e.admitted_at_ms, e.admitted_commit_ref
           FROM media_stream_ends e
           JOIN media_streams s ON s.stream_key = e.stream_key
          WHERE e.stream_key = ?1 AND e.end_id = ?2",
        params![stream_key, end_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<i64>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, Option<String>>(8)?,
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
            state,
            ended_at_ms,
            reason,
            event_id,
            admitted_at_ms,
            admitted_commit_ref,
        )| {
            Ok(StoredMediaStreamEnd {
                end_id: end_id.to_owned(),
                stream: MediaStreamDescriptor {
                    provider,
                    account,
                    stream_id,
                },
                state: parse_stream_state(&state)?,
                ended_at_ms,
                reason,
                event_id: parse_entity(&event_id, "end event id")?,
                admitted_at_ms,
                admitted_commit: admitted_commit(admitted_commit_ref)?,
            })
        },
    )
    .transpose()
}

/// Persist one terminal notice as an immutable fact. The stream row's
/// `end_*`/`state` columns stay a separate latest-state projection.
fn insert_stream_end(
    tx: &rusqlite::Transaction<'_>,
    stream_key: &str,
    end: &MediaStreamEnd,
    event_id: EntityId,
    admitted_at_ms: i64,
    admitted_commit: CommitRef,
) -> Result<(), ShenkoError> {
    tx.execute(
        "INSERT INTO media_stream_ends
            (stream_key, end_id, state, ended_at_ms, reason, event_id,
             admitted_at_ms, admitted_commit_ref, commit_seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            stream_key,
            end.end_id,
            stream_state_name(end.state),
            end.ended_at_ms,
            end.reason.clone(),
            event_id.to_string(),
            admitted_at_ms,
            serde_json::to_string(&admitted_commit).map_err(ShenkoError::Serde)?,
            admitted_commit.seq.0 as i64,
        ],
    )
    .map_err(storage_error)?;
    Ok(())
}

/// The receipt fields an end replay returns: THIS notice's own bound event
/// and commit, never the stream's latest-state values.
fn end_receipt(stored: &StoredMediaStreamEnd, replayed: bool) -> MediaStreamReceipt {
    MediaStreamReceipt {
        stream: stored.stream.clone(),
        state: stored.state,
        replayed,
        event_id: Some(stored.event_id),
        commit_ref: stored.admitted_commit,
    }
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

fn admitted_commit(data: Option<String>) -> Result<CommitRef, ShenkoError> {
    let data = data.ok_or_else(|| media_error(
        "media_receipt_incomplete",
        RetryStrategy::AfterReconcile,
        "media fact lacks its original admission commit; reconcile retained source data",
    ))?;
    serde_json::from_str(&data).map_err(ShenkoError::Serde)
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
    stream_key: &str,
    revision_id: &str,
) -> Result<Option<StoredMediaRevision>, ShenkoError> {
    conn.query_row(
        "SELECT segment, revision, partial, text, captured_at_ms, speaker_json,
                audio_json, provider_facts_json, disposition, supersedes_revision,
                superseded_by_revision, event_id, admitted_at_ms, admitted_commit_ref
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
                row.get::<_, Option<String>>(13)?,
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
            admitted_commit_ref,
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
                admitted_commit: admitted_commit(admitted_commit_ref)?,
            })
        },
    )
    .transpose()
}

/// The effective (newest, not superseded) revision of one segment.
pub(crate) fn effective_revision(
    conn: &Connection,
    stream_key: &str,
    segment: u64,
) -> Result<Option<StoredMediaRevision>, ShenkoError> {
    let segment_key = format!("{}{}{}", stream_key, MEDIA_KEY_SEPARATOR, segment);
    match effective_row_id(conn, &segment_key)? {
        Some(revision_id) => stored_revision(conn, stream_key, &revision_id),
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

/// Media recovery is not complete unless immutable facts, their original
/// delivery envelopes and origin receipts agree. Run on both export and
/// import, before a snapshot can yield a verified-recovery acknowledgement.
/// This also rejects older archives containing staged media but omitting the
/// admission tables — recognizable history is not a substitute for dedup truth.
pub(crate) fn media_recovery_gaps(
    rows: &[crate::cognition_archive::CognitionRow],
) -> Vec<crate::cognition_archive::CognitionGap> {
    use anyhow::{Context, Result, ensure};
    use crate::cognition_archive::{CognitionGap, CognitionRow};
    use std::collections::{HashMap, HashSet};

    fn required<'a>(row: &'a CognitionRow, column: &str) -> Result<&'a str> {
        row.value(column).with_context(|| format!("{} {} lacks {column}", row.table, row.key))
    }

    fn receipt(row: &CognitionRow, column: &str) -> Result<CommitRef> {
        let commit: CommitRef = serde_json::from_str(required(row, column)?)?;
        ensure!(commit.ledger_id != EntityId::nil() && commit.seq.0 != 0,
            "media admission has no real origin commit");
        Ok(commit)
    }

    struct RetainedRevision {
        original: StoredMediaRevision,
        superseded_by: Option<u64>,
        prior_effective: Option<Box<StoredMediaRevision>>,
    }

    fn validate(rows: &[CognitionRow]) -> Result<()> {
        let streams: HashMap<_, _> = rows.iter().filter(|row| row.table == "media_streams")
            .map(|row| (row.key.as_str(), row)).collect();
        let inbox: HashMap<_, _> = rows.iter().filter(|row| row.table == "inbox_events")
            .map(|row| (row.key.as_str(), row)).collect();
        let sessions: HashMap<_, _> = rows.iter().filter(|row| row.table == "session_events")
            .map(|row| (row.key.as_str(), row)).collect();
        let mut bound = HashMap::new();
        let mut revisions = HashMap::new();
        for row in rows.iter().filter(|row| matches!(row.table.as_str(),
            "media_streams" | "media_stream_ends" | "media_segment_revisions"
            | "media_recordings" | "media_recording_controls" | "media_priority_inputs"))
        {
            let stream_key = required(row, "stream_key")?;
            let stream = streams.get(stream_key).context("media fact has no stream identity")?;
            let descriptor = MediaStreamDescriptor {
                provider: required(stream, "provider")?.into(),
                account: required(stream, "account")?.into(),
                stream_id: required(stream, "stream_id")?.into(),
            };
            validate_stream(&descriptor)?;
            parse_entity(required(stream, "agent_id")?, "agent id")?;
            ensure!(media_stream_key(&descriptor) == stream_key, "media stream key mismatch");
            let is_open = row.table == "media_streams";
            let event_id = if is_open {
                if row.value("open_id").is_none() {
                    ensure!(row.value("open_event_id").is_none() && row.value("open_commit_ref").is_none(),
                        "implicit media stream carries a partial open receipt");
                    continue;
                }
                identity_field(required(row, "open_id")?, "open_id")?;
                receipt(row, "open_commit_ref")?;
                required(row, "open_event_id")?
            } else {
                if row.table == "media_stream_ends" {
                    identity_field(required(row, "end_id")?, "end_id")?;
                }
                receipt(row, "admitted_commit_ref")?;
                required(row, "event_id")?
            };
            let original = inbox.get(event_id).context("media fact lacks its durable inbox original")?;
            let envelope: serde_json::Value = serde_json::from_str(required(original, "data")?)?;
            if let Some(existing) = bound.get(event_id) {
                // A recording projection row re-binds the event of its own
                // control notice: the same fact legitimately appears once as
                // an immutable notice and once as the latest-state
                // projection, but never with a different envelope.
                ensure!(&envelope == existing, "media facts share an inbox event identity");
            } else {
                bound.insert(event_id, envelope.clone());
            }
            let event: MediaIngressEvent = serde_json::from_value(
                envelope.get("payload").context("media inbox payload missing")?.clone())?;
            let source = format!("media/{}", descriptor.provider);
            ensure!(event.event_id().to_string() == event_id && event.stream() == &descriptor
                && envelope.get("event_id").and_then(serde_json::Value::as_str) == Some(event_id)
                && envelope.get("source").and_then(serde_json::Value::as_str) == Some(source.as_str())
                && envelope.get("kind").and_then(serde_json::Value::as_str) == Some(event.inbox_kind())
                && original.value("source") == Some(source.as_str())
                && original.value("kind") == Some(event.inbox_kind())
                && original.value("agent_id") == stream.value("agent_id"),
                "media fact and durable delivery identity disagree");
            let _: chrono::DateTime<chrono::Utc> = serde_json::from_value(
                envelope.get("created_at").context("media original timestamp missing")?.clone())?;
            match_extra_media_fact(row, &descriptor, &event)?;
            match event {
                MediaIngressEvent::StreamOpened { descriptor, opened_at_ms, .. } => {
                    ensure!(is_open && required(row, "opened_at_ms")? == opened_at_ms.to_string()
                        && serde_json::from_str::<serde_json::Value>(required(row, "descriptor")?)?
                            == serde_json::to_value(descriptor)?, "media open original mismatch");
                }
                MediaIngressEvent::StreamState { state, ended_at_ms, reason, .. } => {
                    ensure!(row.table == "media_stream_ends" && state.is_terminal()
                        && row.value("state") == Some(stream_state_name(state))
                        && row.value("ended_at_ms").map(str::parse::<i64>).transpose()? == ended_at_ms
                        && row.value("reason") == reason.as_deref(), "media terminal original mismatch");
                }
                MediaIngressEvent::Revision { record, prior_effective, .. } => {
                    ensure!(row.table == "media_segment_revisions", "media revision binding mismatch");
                    let input: MediaRevisionInput = serde_json::from_str(required(row, "data")?)?;
                    validate_revision(&input)?;
                    ensure!(input.stream == descriptor
                        && media_segment_key(&input.stream, input.segment) == required(row, "segment_key")?
                        && required(row, "segment")? == input.segment.to_string()
                        && required(row, "revision")? == input.revision.to_string()
                        && required(row, "revision_id")? == input.revision_id
                        && required(row, "text")? == input.text
                        && required(row, "partial")? == (if input.partial { "1" } else { "0" })
                        && row.value("captured_at_ms").map(str::parse::<i64>).transpose()? == input.captured_at_ms
                        && row.value("speaker_json").map(serde_json::from_str::<MediaSpeakerClaim>).transpose()? == input.speaker
                        && row.value("audio_json").map(serde_json::from_str::<MediaAudioRef>).transpose()? == input.audio
                        && serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                            required(row, "provider_facts_json")?)? == input.provider_facts,
                        "media raw revision columns disagree with admitted input");
                    for audio in [input.audio.as_ref(), prior_effective.as_ref().and_then(|prior| prior.audio.as_ref())] {
                        if let Some(MediaAudioRef::Sealed { object }) = audio {
                            ensure!(object.digest.canonical_blake3_hex().is_some(), "invalid sealed media audio identity");
                        }
                    }
                    let expected = StoredMediaRevision {
                        revision_id: input.revision_id, segment: input.segment,
                        revision: input.revision, text: input.text, partial: input.partial,
                        captured_at_ms: input.captured_at_ms, speaker: input.speaker,
                        audio: input.audio, provider_facts: input.provider_facts,
                        disposition: parse_disposition(required(row, "disposition")?)?,
                        supersedes_revision: row.value("supersedes_revision").map(str::parse::<u64>).transpose()?,
                        // Original event snapshots do not acquire later supersession links.
                        superseded_by_revision: None,
                        event_id: parse_entity(event_id, "event id")?,
                        admitted_at_ms: required(row, "admitted_at_ms")?.parse()?,
                        admitted_commit: receipt(row, "admitted_commit_ref")?,
                    };
                    ensure!(record == expected, "media normalized original disagrees with raw admission");
                    let key = (stream_key, expected.segment, expected.revision);
                    let retained = RetainedRevision {
                        original: expected,
                        superseded_by: row.value("superseded_by_revision").map(str::parse::<u64>).transpose()?,
                        prior_effective,
                    };
                    ensure!(revisions.insert(key, retained).is_none(), "duplicate media segment/revision slot");
                }
                // Recording and priority facts validate in
                // `match_extra_media_fact` below; the shared envelope identity
                // checks above already bound them to this delivery event.
                MediaIngressEvent::Recording { .. } | MediaIngressEvent::Priority { .. } => {}
            }
        }

        // The original envelope is immutable and intentionally has no later
        // superseded_by link. Validate that mutable graph separately: every
        // link and embedded historical prior must resolve to the SAME retained
        // fact, and each segment must still have exactly one effective row.
        let mut effective_counts = HashMap::new();
        for (&(stream_key, segment, revision), retained) in &revisions {
            let original = &retained.original;
            let effective = effective_counts.entry((stream_key, segment)).or_insert(0usize);
            if original.disposition != MediaRevisionDisposition::Superseded && retained.superseded_by.is_none() {
                *effective += 1;
            }
            if let Some(next) = retained.superseded_by {
                let successor = revisions.get(&(stream_key, segment, next))
                    .context("media superseded_by link has no retained revision")?;
                ensure!(next > revision && successor.original.supersedes_revision == Some(revision)
                    && matches!(successor.original.disposition,
                        MediaRevisionDisposition::AppliedSupersedes | MediaRevisionDisposition::FinalCorrection),
                    "media supersession forward/backward links disagree");
            }
            let prior = retained.prior_effective.as_deref();
            if let Some(prior) = prior {
                let previous = revisions.get(&(stream_key, segment, prior.revision))
                    .context("media prior-effective snapshot has no retained revision")?;
                ensure!(prior == &previous.original
                    && prior.disposition != MediaRevisionDisposition::Superseded,
                    "media prior-effective snapshot differs from its retained original");
            }
            match original.disposition {
                MediaRevisionDisposition::Applied => {
                    ensure!(original.supersedes_revision.is_none() && prior.is_none(),
                        "initial media revision carries a contradictory predecessor");
                }
                MediaRevisionDisposition::Superseded => {
                    // Two honest late arrivals resolve here: evidence older
                    // than the effective revision, and an interim claim that
                    // reached a segment AFTER it had already finalized (a
                    // final is never displaced by a partial).
                    ensure!(original.supersedes_revision.is_none() && retained.superseded_by.is_none()
                        && prior.is_some_and(|prior| prior.revision > revision
                            || (original.partial && !prior.partial)),
                        "late media revision does not resolve to its newer effective prior");
                }
                MediaRevisionDisposition::AppliedSupersedes | MediaRevisionDisposition::FinalCorrection => {
                    let prior = prior.context("media superseding revision lacks its prior-effective snapshot")?;
                    let previous = revisions.get(&(stream_key, segment, prior.revision))
                        .context("media supersedes link has no retained revision")?;
                    ensure!(prior.revision < revision && original.supersedes_revision == Some(prior.revision)
                        && previous.superseded_by == Some(revision)
                        && original.disposition == (if prior.partial {
                            MediaRevisionDisposition::AppliedSupersedes
                        } else {
                            MediaRevisionDisposition::FinalCorrection
                        }), "media superseding revision disagrees with its retained predecessor");
                }
            }
        }
        ensure!(effective_counts.values().all(|count| *count == 1),
            "media segment does not have exactly one retained effective revision");

        for stream in streams.values() {
            let state = parse_stream_state(required(stream, "state")?)?;
            if let Some(end_id) = stream.value("end_id") {
                let end = rows.iter().find(|row| row.table == "media_stream_ends"
                    && row.value("stream_key") == Some(stream.key.as_str()) && row.value("end_id") == Some(end_id))
                    .context("media latest state lacks its immutable terminal notice")?;
                ensure!(state.is_terminal() && stream.value("end_event_id") == end.value("event_id")
                    && stream.value("state") == end.value("state")
                    && stream.value("ended_at_ms") == end.value("ended_at_ms")
                    && stream.value("end_reason") == end.value("reason"), "media latest state differs from its terminal fact");
            } else {
                ensure!(!state.is_terminal(), "terminal media stream has no terminal notice");
            }
        }
        let mut media_raw_sources = HashMap::new();
        for row in rows.iter().filter(|row| row.table == "inbox_events" || row.table == "session_events") {
            let media_inbox = row.table == "inbox_events" &&
                (row.value("source").is_some_and(|source| source.starts_with("media/"))
                    || matches!(row.value("kind"),
                        Some("media_revision" | "media_stream" | "media_recording" | "media_priority")));
            let Some(data) = row.value("data") else {
                ensure!(!media_inbox, "media inbox original missing");
                continue;
            };
            let value = match serde_json::from_str::<serde_json::Value>(data) {
                Ok(value) => value,
                Err(error) => {
                    ensure!(!media_inbox, "invalid media inbox original: {error}");
                    continue;
                }
            };
            let original = if row.table == "session_events" {
                let Some(original) = value.get("inbox_event") else { continue };
                original
            } else { &value };
            let media_kind = matches!(original.get("kind").and_then(serde_json::Value::as_str),
                Some("media_revision" | "media_stream" | "media_recording" | "media_priority"));
            let original_id = original.get("event_id").and_then(serde_json::Value::as_str);
            if media_kind || media_inbox || original_id.is_some_and(|id| bound.contains_key(id))
            {
                let id = original_id.context("media original event identity missing")?;
                let (source_id, admitted) = bound.get_key_value(id)
                    .context("media original has no immutable admission fact")?;
                ensure!(original == admitted, "staged media differs from the admitted original");
                if row.table == "session_events" {
                    media_raw_sources.insert(row.key.as_str(), *source_id);
                }
            }
        }
        let mut staged_raw = HashSet::new();
        for link in rows.iter().filter(|row| row.table == "subject_raw_inputs") {
            let source_id = required(link, "source_event_id")?;
            let raw_id = required(link, "raw_event_id")?;
            let raw_source = media_raw_sources.get(raw_id).copied();
            if raw_source.is_some() || bound.contains_key(source_id) {
                ensure!(raw_source == Some(source_id), "media staging link has no matching admitted original");
                let raw = sessions.get(raw_id).context("media staging raw event missing")?;
                ensure!(link.value("session_id") == raw.value("session_id")
                    && link.value("seq") == raw.value("seq") && staged_raw.insert(raw_id),
                    "media staging link disagrees with its unique raw event");
            }
        }
        ensure!(media_raw_sources.keys().all(|raw_id| staged_raw.contains(*raw_id)),
            "staged media original lacks its subject-input link");
        Ok(())
    }

    /// Validate one recording or priority fact against its durable envelope
    /// and its own committed columns. Recording projection rows carry no
    /// `data` column; their consistency is checked column-wise against the
    /// envelope of their latest control notice.
    fn match_extra_media_fact(
        row: &CognitionRow,
        descriptor: &MediaStreamDescriptor,
        event: &MediaIngressEvent,
    ) -> Result<()> {
        match event {
            MediaIngressEvent::Recording { recording, action, .. } => {
                if row.table == "media_recording_controls" {
                    let control: MediaRecordingControl = serde_json::from_str(required(row, "data")?)?;
                    validate_recording_control(&control)?;
                    ensure!(control.stream == *descriptor
                        && control.recording_id == recording.recording_id
                        && control.action == *action
                        && control.action.name() == required(row, "action")?
                        && recording.state.name() == required(row, "state")?,
                        "media recording control original mismatch");
                } else {
                    ensure!(row.table == "media_recordings"
                        && recording.recording_id == required(row, "recording_id")?
                        && recording.state.name() == required(row, "state")?
                        && serde_json::to_string(&recording.scope).map_err(ShenkoError::Serde)? == required(row, "scope_json")?,
                        "media recording projection disagrees with its original");
                }
            }
            MediaIngressEvent::Priority { input, .. } => {
                ensure!(row.table == "media_priority_inputs", "media priority binding mismatch");
                let admitted: MediaPriorityInput = serde_json::from_str(required(row, "data")?)?;
                validate_priority_input(&admitted)?;
                ensure!(admitted.stream == *descriptor
                    && admitted.priority_id == input.priority_id
                    && admitted.kind == input.kind
                    && admitted.kind.name() == required(row, "kind")?
                    && row.value("text") == admitted.text.as_deref()
                    && row.value("occurred_at_ms").map(str::parse::<i64>).transpose()? == admitted.occurred_at_ms
                    && row.value("audio_json").map(serde_json::from_str::<MediaAudioRef>).transpose()? == admitted.audio
                    && row.value("reason") == admitted.reason.as_deref(),
                    "media priority original mismatch");
                if let Some(MediaAudioRef::Sealed { object }) = &admitted.audio {
                    ensure!(object.digest.canonical_blake3_hex().is_some(),
                        "invalid sealed media audio identity");
                }
            }
            _ => {}
        }
        Ok(())
    }

    match validate(rows) {
        Ok(()) => vec![],
        Err(error) => vec![CognitionGap {
            kind: "media_input_closure".into(), detail: error.to_string(), blocking: true,
        }],
    }
}

impl Store {
    /// Exact admitted subject-input original for a live wake. It has the
    /// same serialized InboxEvent shape consumed by pending_inbox_events;
    /// never regenerate an event's timestamp or normalized payload on send.
    pub fn media_inbox_event(
        &self,
        agent_id: EntityId,
        event_id: EntityId,
    ) -> Result<Option<String>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT data FROM inbox_events
              WHERE agent_id=?1 AND event_id=?2 AND source LIKE 'media/%'",
            params![agent_id.to_string(), event_id.to_string()],
            |row| row.get(0),
        ).optional().map_err(storage_error)
    }

    /// Admit one typed transcript revision (§15.2, §20).
    ///
    /// Atomically commits the raw revision row and the segment's latest
    /// effective observation plus the durable `inbox_events` original of the
    /// bound subject event. `replayed` receipts carry the SAME bound event
    /// id; callers deliver `notify` to the subject inbox at most once and may
    /// always reconcile consumption via `event_disposition_commit`, while the
    /// subject's pending drain recovers the same durable original.
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
            // Canonical digest validation runs BEFORE any path resolution;
            // the committed reference binds only content verified in this CAS.
            verify_sealed_audio(self, object)?;
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
        let existing_stream = read_stream_row(&tx, &stream_key)?;
        if existing_stream.as_ref().is_some_and(|row| row.agent_id != agent_id) {
            return Err(media_error(
                "media_stream_agent_mismatch",
                RetryStrategy::NeverAutomatically,
                "media stream belongs to a different subject",
            ));
        }

        // Idempotent replay: same durable identity must replay byte-identical
        // facts; anything else is an explicit contradiction, never an overwrite.
        if let Some(stored) = stored_revision(&tx, &stream_key, &input.revision_id)? {
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
        let stream_state = match existing_stream {
            Some(row) => row.state,
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
                    None,
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
                stored_revision(&tx, &stream_key, &revision_id)?
                    .ok_or_else(|| ShenkoError::storage("effective media revision row missing"))
            })
            .transpose()?;

        let (disposition, supersedes) = match &prior_effective {
            None => (MediaRevisionDisposition::Applied, None),
            // An interim claim never displaces a final observation. A late
            // partial that arrives after the segment finalized is retained
            // raw like any other out-of-order evidence, so the effective text
            // never regresses from final to partial beneath the subject.
            Some(prior) if input.partial && !prior.partial => {
                (MediaRevisionDisposition::Superseded, None)
            }
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
                 event_id, admitted_at_ms, admitted_commit_ref, commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18)",
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
                serde_json::to_string(&commit_ref(ledger_id, seq.0))
                    .map_err(ShenkoError::Serde)?,
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
        let notify = MediaIngressEvent::Revision {
            event_id,
            stream: input.stream.clone(),
            record,
            prior_effective: prior_effective.map(Box::new),
        };
        // The durable inbox original commits in the same transaction: the
        // volatile queue send later is only a wake, never the admission.
        insert_media_inbox_event(&tx, agent_id, &notify)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;

        Ok(MediaAdmission {
            receipt: MediaRevisionReceipt {
                revision_id: input.revision_id.clone(),
                disposition,
                replayed: false,
                event_id,
                commit_ref: commit_ref(ledger_id, seq.0),
            },
            notify: Some(notify),
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
                        state: MediaStreamState::Open,
                        replayed: true,
                        event_id: row.open_event_id,
                        // The open receipt keeps the OPEN admission's own
                        // commit; the row's latest-state commit_seq may have
                        // advanced past it through a terminal admission.
                        commit_ref: row.open_commit_ref.ok_or_else(|| media_error(
                            "media_receipt_incomplete",
                            RetryStrategy::AfterReconcile,
                            "the original open commit was not preserved; reconcile the retained source",
                        ))?,
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
                            descriptor = ?4, implicit = 0,
                            open_commit_ref = ?5, commit_seq = ?6
                      WHERE stream_key = ?1",
                    params![
                        stream_key,
                        open.open_id,
                        event_id.to_string(),
                        serde_json::to_string(&open.descriptor).map_err(ShenkoError::Serde)?,
                        serde_json::to_string(&commit_ref(ledger_id, seq.0))
                            .map_err(ShenkoError::Serde)?,
                        seq.0 as i64,
                    ],
                )
                .map_err(storage_error)?;
                let notify = MediaIngressEvent::StreamOpened {
                    event_id,
                    stream: open.stream.clone(),
                    descriptor: open.descriptor.clone(),
                    opened_at_ms,
                };
                insert_media_inbox_event(&tx, agent_id, &notify)?;
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
                    notify: Some(notify),
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
            Some(commit_ref(ledger_id, seq.0)),
            seq.0,
        )?;
        let notify = MediaIngressEvent::StreamOpened {
            event_id,
            stream: open.stream.clone(),
            descriptor: open.descriptor.clone(),
            opened_at_ms,
        };
        insert_media_inbox_event(&tx, agent_id, &notify)?;
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
            notify: Some(notify),
        })
    }

    /// Admit a terminal stream state (§20 cancellation/disconnect/end).
    ///
    /// Committed segments and their effective text are preserved untouched;
    /// the gap list in `media_transcript` stays honest. An end notice for a
    /// stream this ledger has not seen creates the row directly in the
    /// reported terminal state — the missing open is recorded, not invented.
    /// Each notice is an immutable fact keyed by `end_id`: `disconnected` may
    /// later settle to `ended`/`cancelled` under a NEW end identity, and a
    /// replayed `end_id` returns its own original event id and commit —
    /// never the stream's latest terminal projection.
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
            // Terminal notices are immutable facts keyed by end identity. A
            // replay returns THIS notice's own event id and commit — a later
            // settlement may have advanced the stream's latest state, and it
            // never rewrites what this end_id originally reported.
            if let Some(stored) = stored_stream_end(&tx, &stream_key, &end.end_id)? {
                let same_fact = stored.state == end.state
                    && stored.ended_at_ms == end.ended_at_ms
                    && stored.reason == end.reason;
                if !same_fact {
                    return Err(media_error(
                        "media_stream_conflict",
                        RetryStrategy::NeverAutomatically,
                        "a different terminal fact already holds this end identity",
                    ));
                }
                return Ok(MediaStreamAdmission {
                    receipt: end_receipt(&stored, true),
                    notify: None,
                });
            }
            // A disconnect may settle into the provider's real terminal cause;
            // it is a new explicit fact under a NEW end identity, never a
            // silent rewrite. `ended`/`cancelled` stay immutable.
            if let Some(previous) = &row.end {
                // An older/incomplete store may retain only the latest
                // terminal projection. Do not overwrite that sole fact or
                // invent its receipt while settling a new notice.
                if stored_stream_end(&tx, &stream_key, &previous.end_id)?.is_none() {
                    return Err(media_error(
                        "media_receipt_incomplete",
                        RetryStrategy::AfterReconcile,
                        "previous terminal notice lacks its immutable receipt; reconcile retained source data",
                    ));
                }
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
            insert_stream_end(
                &tx,
                &stream_key,
                end,
                event_id,
                chrono::Utc::now().timestamp_millis(),
                commit_ref(ledger_id, seq.0),
            )?;
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
            let notify = MediaIngressEvent::StreamState {
                event_id,
                stream: end.stream.clone(),
                state: end.state,
                reason: end.reason.clone(),
                ended_at_ms: end.ended_at_ms,
                segments,
            };
            insert_media_inbox_event(&tx, agent_id, &notify)?;
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
                notify: Some(notify),
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
            None,
            seq.0,
        )?;
        insert_stream_end(
            &tx,
            &stream_key,
            end,
            event_id,
            chrono::Utc::now().timestamp_millis(),
            commit_ref(ledger_id, seq.0),
        )?;
        let notify = MediaIngressEvent::StreamState {
            event_id,
            stream: end.stream.clone(),
            state: end.state,
            reason: end.reason.clone(),
            ended_at_ms: end.ended_at_ms,
            segments: 0,
        };
        insert_media_inbox_event(&tx, agent_id, &notify)?;
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
            notify: Some(notify),
        })
    }

    /// The current effective revision of one segment, for projection and
    /// reconciliation reads. Committed rows only.
    pub fn media_effective_segment(
        &self,
        stream: &MediaStreamDescriptor,
        segment: u64,
    ) -> Result<Option<StoredMediaRevision>, ShenkoError> {
        if i64::try_from(segment).is_err() {
            return Err(invalid("segment exceeds the supported persistent range"));
        }
        let conn = self.ledger.conn.lock();
        effective_revision(
            &conn,
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
        if i64::try_from(segment).is_err() {
            return Err(invalid("segment exceeds the supported persistent range"));
        }
        let conn = self.ledger.conn.lock();
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
            let record = stored_revision(&conn, &stream_key, &revision_id)?
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

    /// Every immutable terminal notice admitted for one stream, ordered by
    /// retained admission timestamp and end identity (not global causality).
    /// Each keeps its own bound event id and original admission commit. The
    /// stream row's `end` field stays the latest-state projection; these are
    /// the original receipts a provider reconciles `end_id` by.
    pub fn media_stream_ends(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Vec<StoredMediaStreamEnd>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let stream_key = media_stream_key(stream);
        let mut statement = conn
            .prepare(
                "SELECT end_id FROM media_stream_ends
                  WHERE stream_key = ?1 ORDER BY admitted_at_ms, end_id",
            )
            .map_err(storage_error)?;
        let ids = statement
            .query_map(params![stream_key], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        let mut ends = Vec::with_capacity(ids.len());
        for end_id in ids {
            ends.push(
                stored_stream_end(&conn, &stream_key, &end_id)?
                    .ok_or_else(|| ShenkoError::storage("media stream end row missing"))?,
            );
        }
        Ok(ends)
    }

    /// Effective transcript of one stream in segment order with the honest
    /// gap list; `None` when the stream was never admitted.
    pub fn media_transcript(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Option<MediaTranscript>, ShenkoError> {
        let conn = self.ledger.conn.lock();
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
            let effective = stored_revision(&conn, &stream_key, &revision_id)?
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

// ---------------------------------------------------------------------------
// Recording control and priority inputs
// ---------------------------------------------------------------------------

fn validate_recording_control(control: &MediaRecordingControl) -> Result<(), ShenkoError> {
    identity_field(&control.control_id, "control_id")?;
    identity_field(&control.recording_id, "recording_id")?;
    validate_stream(&control.stream)?;
    if control.action == MediaRecordingAction::Start && control.scope.is_none() {
        return Err(invalid("recording start must declare the scope it stores"));
    }
    if let Some(scope) = &control.scope {
        identity_field(&scope.kind, "scope kind")?;
        identity_field(&scope.retention, "scope retention")?;
    }
    Ok(())
}

fn validate_priority_input(input: &MediaPriorityInput) -> Result<(), ShenkoError> {
    identity_field(&input.priority_id, "priority_id")?;
    validate_stream(&input.stream)?;
    if let Some(text) = &input.text {
        if text.trim().is_empty() {
            return Err(invalid("priority input text is empty"));
        }
    }
    if let Some(MediaAudioRef::Provider(provider)) = &input.audio {
        if provider.locator.trim().is_empty() {
            return Err(invalid("provider audio locator is empty"));
        }
    }
    Ok(())
}

/// One admitted recording control notice, exactly as committed.
struct StoredRecordingControl {
    control_id: String,
    recording_id: String,
    action: MediaRecordingAction,
    state: MediaRecordingState,
    data: String,
    event_id: EntityId,
    admitted_commit: CommitRef,
}

fn read_recording_control(
    conn: &Connection,
    stream_key: &str,
    control_id: &str,
) -> Result<Option<StoredRecordingControl>, ShenkoError> {
    conn.query_row(
        "SELECT recording_id, action, state, data, event_id, admitted_commit_ref
           FROM media_recording_controls
          WHERE stream_key = ?1 AND control_id = ?2",
        params![stream_key, control_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(|(recording_id, action, state, data, event_id, commit)| {
        Ok(StoredRecordingControl {
            control_id: control_id.to_owned(),
            recording_id,
            action: MediaRecordingAction::parse(&action).ok_or_else(|| {
                ShenkoError::storage(format!("stored media recording action {action:?} is unknown"))
            })?,
            state: MediaRecordingState::parse(&state).ok_or_else(|| {
                ShenkoError::storage(format!("stored media recording state {state:?} is unknown"))
            })?,
            data,
            event_id: parse_entity(&event_id, "recording control event id")?,
            admitted_commit: admitted_commit(Some(commit))?,
        })
    })
    .transpose()
}

fn read_recording(
    conn: &Connection,
    stream_key: &str,
    recording_id: &str,
) -> Result<Option<StoredMediaRecording>, ShenkoError> {
    conn.query_row(
        "SELECT s.provider, s.account, s.stream_id, r.state, r.started_at_ms,
                r.stopped_at_ms, r.scope_json, r.start_control_id,
                r.stop_control_id, r.reason, r.event_id, r.admitted_at_ms,
                r.admitted_commit_ref
           FROM media_recordings r
           JOIN media_streams s ON s.stream_key = r.stream_key
          WHERE r.stream_key = ?1 AND r.recording_id = ?2",
        params![stream_key, recording_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<i64>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, i64>(11)?,
                row.get::<_, String>(12)?,
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
            state,
            started_at_ms,
            stopped_at_ms,
            scope_json,
            start_control_id,
            stop_control_id,
            reason,
            event_id,
            admitted_at_ms,
            commit,
        )| {
            Ok(StoredMediaRecording {
                recording_id: recording_id.to_owned(),
                stream: MediaStreamDescriptor {
                    provider,
                    account,
                    stream_id,
                },
                state: MediaRecordingState::parse(&state).ok_or_else(|| {
                    ShenkoError::storage(format!("stored media recording state {state:?} is unknown"))
                })?,
                started_at_ms,
                stopped_at_ms,
                scope: serde_json::from_str(&scope_json).map_err(ShenkoError::Serde)?,
                start_control_id,
                stop_control_id,
                reason,
                event_id: parse_entity(&event_id, "recording event id")?,
                admitted_at_ms,
                admitted_commit: admitted_commit(Some(commit))?,
            })
        },
    )
    .transpose()
}


impl Store {
    /// Admit one recording control command (start/stop).
    ///
    /// Atomically commits the immutable control notice, the recording
    /// latest-state projection and the durable `inbox_events` original of
    /// the bound subject event. A replayed `control_id` returns the ORIGINAL
    /// receipt with the same bound event id; a different fact under the same
    /// identity is a typed conflict, never a silent overwrite.
    pub fn admit_media_recording(
        &self,
        agent_id: EntityId,
        control: MediaRecordingControl,
    ) -> Result<MediaRecordingAdmission, ShenkoError> {
        validate_recording_control(&control)?;
        self.ledger.with_mutation(
            || self.try_admit_media_recording(agent_id, &control),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_media_recording(
        &self,
        agent_id: EntityId,
        control: &MediaRecordingControl,
    ) -> Result<MediaRecordingAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(&control.stream);
        let data = serde_json::to_string(control).map_err(ShenkoError::Serde)?;

        if let Some(stored) = read_recording_control(&tx, &stream_key, &control.control_id)? {
            if stored.data != data {
                return Err(media_error(
                    "media_recording_conflict",
                    RetryStrategy::NeverAutomatically,
                    "a different fact already holds this recording control identity",
                ));
            }
            return Ok(MediaRecordingAdmission {
                receipt: MediaRecordingReceipt {
                    recording_id: stored.recording_id,
                    state: stored.state,
                    action: stored.action,
                    replayed: true,
                    event_id: stored.event_id,
                    commit_ref: stored.admitted_commit,
                },
                notify: None,
            });
        }

        let existing_stream = read_stream_row(&tx, &stream_key)?;
        if existing_stream
            .as_ref()
            .is_some_and(|row| row.agent_id != agent_id)
        {
            return Err(media_error(
                "media_stream_agent_mismatch",
                RetryStrategy::NeverAutomatically,
                "media stream belongs to a different subject",
            ));
        }

        let existing_recording = read_recording(&tx, &stream_key, &control.recording_id)?;
        let new_state = match (control.action, &existing_recording) {
            (MediaRecordingAction::Start, None) => MediaRecordingState::Recording,
            (MediaRecordingAction::Start, Some(_)) => {
                return Err(media_error(
                    "media_recording_conflict",
                    RetryStrategy::NeverAutomatically,
                    "this recording identity was already used on the stream",
                ));
            }
            (MediaRecordingAction::Stop, Some(row)) if row.state.is_terminal() => {
                return Err(media_error(
                    "media_recording_conflict",
                    RetryStrategy::NeverAutomatically,
                    "this recording is already stopped or failed",
                ));
            }
            (MediaRecordingAction::Stop, Some(_)) => MediaRecordingState::Stopped,
            (MediaRecordingAction::Stop, None) => {
                return Err(media_error(
                    "media_recording_not_found",
                    RetryStrategy::AfterReconcile,
                    "stop requested for a recording this ledger never started",
                ));
            }
        };

        // One ledger sequence covers notice + projection + inbox original;
        // a stream row created implicitly keeps a lost open an honest gap.
        let seq = self.ledger.allocate_seq(&tx)?;
        if existing_stream.is_none() {
            insert_stream(
                &tx,
                &stream_key,
                agent_id,
                &control.stream,
                MediaStreamState::Open,
                chrono::Utc::now().timestamp_millis(),
                None,
                None,
                None,
                None,
                true,
                &serde_json::Map::new(),
                None,
                seq.0,
            )?;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let event_id = EntityId::new();
        let commit = commit_ref(ledger_id, seq.0);
        let scope = control
            .scope
            .clone()
            .or_else(|| existing_recording.as_ref().map(|row| row.scope.clone()))
            .ok_or_else(|| ShenkoError::storage("media recording scope missing"))?;
        let started_at_ms = existing_recording
            .as_ref()
            .map(|row| row.started_at_ms)
            .unwrap_or(now_ms);
        let start_control_id = existing_recording
            .as_ref()
            .map(|row| row.start_control_id.clone())
            .unwrap_or_else(|| control.control_id.clone());

        tx.execute(
            "INSERT INTO media_recording_controls
                (stream_key, control_id, recording_id, action, state, data,
                 event_id, admitted_at_ms, admitted_commit_ref, commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                stream_key,
                control.control_id,
                control.recording_id,
                control.action.name(),
                new_state.name(),
                data,
                event_id.to_string(),
                now_ms,
                serde_json::to_string(&commit).map_err(ShenkoError::Serde)?,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;

        let stopped_at_ms = match new_state {
            MediaRecordingState::Stopped => Some(now_ms),
            _ => existing_recording
                .as_ref()
                .and_then(|row| row.stopped_at_ms),
        };
        let stop_control_id = match new_state {
            MediaRecordingState::Stopped => Some(control.control_id.clone()),
            _ => existing_recording
                .as_ref()
                .and_then(|row| row.stop_control_id.clone()),
        };
        let reason = control
            .reason
            .clone()
            .or_else(|| existing_recording.as_ref().and_then(|row| row.reason.clone()));

        tx.execute(
            "INSERT INTO media_recordings
                (stream_key, recording_id, agent_id, state, started_at_ms,
                 stopped_at_ms, scope_json, start_control_id, stop_control_id,
                 reason, event_id, admitted_at_ms, admitted_commit_ref,
                 commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT (stream_key, recording_id) DO UPDATE SET
                state = excluded.state,
                stopped_at_ms = excluded.stopped_at_ms,
                stop_control_id = excluded.stop_control_id,
                reason = excluded.reason,
                event_id = excluded.event_id,
                admitted_at_ms = excluded.admitted_at_ms,
                admitted_commit_ref = excluded.admitted_commit_ref,
                commit_seq = excluded.commit_seq",
            params![
                stream_key,
                control.recording_id,
                agent_id.to_string(),
                new_state.name(),
                started_at_ms,
                stopped_at_ms,
                serde_json::to_string(&scope).map_err(ShenkoError::Serde)?,
                start_control_id,
                stop_control_id,
                reason,
                event_id.to_string(),
                now_ms,
                serde_json::to_string(&commit).map_err(ShenkoError::Serde)?,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;

        let recording = StoredMediaRecording {
            recording_id: control.recording_id.clone(),
            stream: control.stream.clone(),
            state: new_state,
            started_at_ms,
            stopped_at_ms,
            scope,
            start_control_id,
            stop_control_id,
            reason,
            event_id,
            admitted_at_ms: now_ms,
            admitted_commit: commit,
        };
        let notify = MediaIngressEvent::Recording {
            event_id,
            stream: control.stream.clone(),
            recording: Box::new(recording),
            action: control.action,
        };
        insert_media_inbox_event(&tx, agent_id, &notify)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;

        Ok(MediaRecordingAdmission {
            receipt: MediaRecordingReceipt {
                recording_id: control.recording_id.clone(),
                state: new_state,
                action: control.action,
                replayed: false,
                event_id,
                commit_ref: commit,
            },
            notify: Some(notify),
        })
    }

    /// The committed recording row, or `None` when this ledger has no fact.
    pub fn media_recording(
        &self,
        stream: &MediaStreamDescriptor,
        recording_id: &str,
    ) -> Result<Option<StoredMediaRecording>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        read_recording(&conn, &media_stream_key(stream), recording_id)
    }

    /// Storage cost actually sealed for one recording's stream: total sealed
    /// audio bytes, revisions carrying sealed audio, and retained revision
    /// count. Computed from the retained facts, never estimated.
    pub fn media_recording_usage(
        &self,
        stream: &MediaStreamDescriptor,
        recording_id: &str,
    ) -> Result<MediaRecordingUsage, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let stream_key = media_stream_key(stream);
        read_recording(&conn, &stream_key, recording_id)?
            .ok_or_else(|| {
                media_error(
                    "media_recording_not_found",
                    RetryStrategy::AfterReconcile,
                    "recording unknown to this ledger",
                )
            })?;
        let revisions: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM media_segment_revisions WHERE stream_key = ?1",
                params![stream_key],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let mut statement = conn
            .prepare("SELECT audio_json FROM media_segment_revisions WHERE stream_key = ?1")
            .map_err(storage_error)?;
        let audio_refs = statement
            .query_map(params![stream_key], |row| row.get::<_, Option<String>>(0))
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        // Storage cost counts each sealed object ONCE: a correction that
        // repeats an earlier reference stores no new byte, and a carried
        // forward reference must not be billed twice. `audio_segments` still
        // counts every retained revision that carries one.
        let mut sealed_bytes: HashMap<String, u64> = HashMap::new();
        let mut audio_segments = 0u64;
        for audio_json in audio_refs.into_iter().flatten() {
            let parsed: serde_json::Value =
                serde_json::from_str(&audio_json).map_err(ShenkoError::Serde)?;
            if parsed.get("kind").and_then(|kind| kind.as_str()) == Some("sealed") {
                let Some(bytes) = parsed.pointer("/object/bytes").and_then(|b| b.as_u64()) else {
                    continue;
                };
                audio_segments += 1;
                let digest = parsed
                    .pointer("/object/digest")
                    .and_then(|digest| digest.as_str())
                    .unwrap_or_default()
                    .to_owned();
                sealed_bytes.insert(digest, bytes);
            }
        }
        let audio_bytes = sealed_bytes.values().copied().sum();
        Ok(MediaRecordingUsage {
            audio_bytes,
            audio_segments,
            revisions,
        })
    }

    /// Admit one priority input of a media session (barge-in, interjection,
    /// stop playback, urgent message). The notice is an immutable fact: a
    /// replayed `priority_id` returns its ORIGINAL event id and commit, and
    /// a different fact under the same identity is a typed conflict.
    pub fn admit_media_priority(
        &self,
        agent_id: EntityId,
        input: MediaPriorityInput,
    ) -> Result<MediaPriorityAdmission, ShenkoError> {
        validate_priority_input(&input)?;
        self.ledger.with_mutation(
            || self.try_admit_media_priority(agent_id, &input),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_media_priority(
        &self,
        agent_id: EntityId,
        input: &MediaPriorityInput,
    ) -> Result<MediaPriorityAdmission, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let ledger_id = self.ledger.ledger_id();
        let stream_key = media_stream_key(&input.stream);
        let data = serde_json::to_string(input).map_err(ShenkoError::Serde)?;

        if let Some(stored) = stored_priority(&tx, &stream_key, &input.priority_id)? {
            let stored_data: String = tx
                .query_row(
                    "SELECT data FROM media_priority_inputs
                      WHERE stream_key = ?1 AND priority_id = ?2",
                    params![stream_key, input.priority_id],
                    |row| row.get(0),
                )
                .map_err(storage_error)?;
            if stored_data != data {
                return Err(media_error(
                    "media_priority_conflict",
                    RetryStrategy::NeverAutomatically,
                    "a different fact already holds this priority identity",
                ));
            }
            return Ok(MediaPriorityAdmission {
                receipt: MediaPriorityReceipt {
                    priority_id: input.priority_id.clone(),
                    kind: stored.kind,
                    replayed: true,
                    event_id: stored.event_id,
                    commit_ref: stored.admitted_commit,
                },
                notify: None,
            });
        }

        let existing_stream = read_stream_row(&tx, &stream_key)?;
        if existing_stream
            .as_ref()
            .is_some_and(|row| row.agent_id != agent_id)
        {
            return Err(media_error(
                "media_stream_agent_mismatch",
                RetryStrategy::NeverAutomatically,
                "media stream belongs to a different subject",
            ));
        }

        let seq = self.ledger.allocate_seq(&tx)?;
        if existing_stream.is_none() {
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
                None,
                seq.0,
            )?;
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let event_id = EntityId::new();
        let commit = commit_ref(ledger_id, seq.0);
        let audio_json = input
            .audio
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(ShenkoError::Serde)?;
        tx.execute(
            "INSERT INTO media_priority_inputs
                (stream_key, priority_id, kind, text, occurred_at_ms, audio_json,
                 reason, data, event_id, admitted_at_ms, admitted_commit_ref,
                 commit_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                stream_key,
                input.priority_id,
                input.kind.name(),
                input.text,
                input.occurred_at_ms,
                audio_json,
                input.reason,
                data,
                event_id.to_string(),
                now_ms,
                serde_json::to_string(&commit).map_err(ShenkoError::Serde)?,
                seq.0 as i64,
            ],
        )
        .map_err(storage_error)?;

        let stored = StoredMediaPriorityInput {
            priority_id: input.priority_id.clone(),
            stream: input.stream.clone(),
            kind: input.kind,
            text: input.text.clone(),
            occurred_at_ms: input.occurred_at_ms,
            audio: input.audio.clone(),
            reason: input.reason.clone(),
            event_id,
            admitted_at_ms: now_ms,
            admitted_commit: commit,
        };
        let notify = MediaIngressEvent::Priority {
            event_id,
            stream: input.stream.clone(),
            input: Box::new(stored),
        };
        insert_media_inbox_event(&tx, agent_id, &notify)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(storage_error)?;

        Ok(MediaPriorityAdmission {
            receipt: MediaPriorityReceipt {
                priority_id: input.priority_id.clone(),
                kind: input.kind,
                replayed: false,
                event_id,
                commit_ref: commit,
            },
            notify: Some(notify),
        })
    }

    /// Every retained priority notice of one stream, in admission order.
    pub fn media_priority_inputs(
        &self,
        stream: &MediaStreamDescriptor,
    ) -> Result<Vec<StoredMediaPriorityInput>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let stream_key = media_stream_key(stream);
        let mut statement = conn
            .prepare(
                "SELECT p.priority_id, p.kind, p.text, p.occurred_at_ms,
                        p.audio_json, p.reason, p.event_id, p.admitted_at_ms,
                        p.admitted_commit_ref
                   FROM media_priority_inputs p
                   JOIN media_streams s ON s.stream_key = p.stream_key
                  WHERE p.stream_key = ?1
                  ORDER BY p.admitted_at_ms, p.priority_id",
            )
            .map_err(storage_error)?;
        let rows = statement
            .query_map(params![stream_key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })
            .map_err(storage_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(storage_error)?;
        rows.into_iter()
            .map(
                |(
                    priority_id,
                    kind,
                    text,
                    occurred_at_ms,
                    audio_json,
                    reason,
                    event_id,
                    admitted_at_ms,
                    commit,
                )| {
                    Ok(StoredMediaPriorityInput {
                        priority_id,
                        stream: stream.clone(),
                        kind: MediaPriorityKind::parse(&kind).ok_or_else(|| {
                            ShenkoError::storage(format!(
                                "stored media priority kind {kind:?} is unknown"
                            ))
                        })?,
                        text,
                        occurred_at_ms,
                        audio: audio_json
                            .map(|json| serde_json::from_str(&json))
                            .transpose()
                            .map_err(ShenkoError::Serde)?,
                        reason,
                        event_id: parse_entity(&event_id, "priority event id")?,
                        admitted_at_ms,
                        admitted_commit: admitted_commit(Some(commit))?,
                    })
                },
            )
            .collect()
    }
}

/// One stored priority notice row for replay detection.
struct StoredPriorityRow {
    kind: MediaPriorityKind,
    event_id: EntityId,
    admitted_commit: CommitRef,
}

fn stored_priority(
    conn: &Connection,
    stream_key: &str,
    priority_id: &str,
) -> Result<Option<StoredPriorityRow>, ShenkoError> {
    conn.query_row(
        "SELECT kind, event_id, admitted_commit_ref
           FROM media_priority_inputs
          WHERE stream_key = ?1 AND priority_id = ?2",
        params![stream_key, priority_id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )
    .optional()
    .map_err(storage_error)?
    .map(|(kind, event_id, commit)| {
        Ok(StoredPriorityRow {
            kind: MediaPriorityKind::parse(&kind).ok_or_else(|| {
                ShenkoError::storage(format!("stored media priority kind {kind:?} is unknown"))
            })?,
            event_id: parse_entity(&event_id, "priority event id")?,
            admitted_commit: admitted_commit(Some(commit))?,
        })
    })
    .transpose()
}
