//! Portable, immutable external-history snapshots over the existing Store.
//!
//! A provider's index is a locator and query aid. Original source bytes enter
//! CAS, source versions and full-text projections enter raw_sessions/events,
//! and ordinary cognition export carries their complete closure. No inbox,
//! subject branch, semantic-memory write or second database is created here.

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use shenko_store::{SessionEventFilter, Store};
use shenko_types::{
    ids::EntityId,
    object::{Digest, ObjectRef},
    store::{RawSession, RawSessionEvent},
};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

const FORMAT: &str = "shenko.external-history-source/1";
const EVENT_TYPE: &str = "external_history_record";
const TEXT_EVENT_TYPE: &str = "external_history_text_chunk";
const CHUNK_BYTES: usize = 1024 * 1024;
const TEXT_CHUNK_BYTES: usize = 8 * 1024;
const QUERY_OVERLAP_BYTES: usize = 4096;
const MAX_PROJECTION_BYTES: usize = 128 * 1024;
const SEARCH_BYTE_BUDGET: usize = 16 * 1024 * 1024;
const SEARCH_RECORD_BUDGET: usize = 256;
const MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SOURCE_RECORDS: usize = 32_768;
const MAX_DECODED_PROJECTION_BYTES: usize = 128 * 1024 * 1024;
const MAX_INDEX_PROJECTION_BYTES: usize = 256 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 128 * 1024;
const MAX_PAGE_RECORDS: usize = 100;
const SESSION_SCAN_BUDGET: usize = 256;

/// Portable logical origin, independent of the current node and local path.
/// A configured root label plus relative source key is legitimate when the
/// upstream has no native session ID, but is not a claim of native identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalHistorySource {
    pub provider: String,
    pub source_instance: String,
    pub source_key: String,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceChunk {
    pub offset: u64,
    pub bytes: usize,
    /// Hex rather than a CAS ObjectRef: a chunk hash is not a stored object.
    pub blake3_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedSource {
    pub archive_session_id: EntityId,
    pub source: ExternalHistorySource,
    pub raw_ref: ObjectRef,
    pub record_count: usize,
    pub event_count: usize,
    pub invalid_records: usize,
    pub bytes: u64,
    pub chunks: Vec<SourceChunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SourceEnvelope {
    format: String,
    archive: ArchivedSource,
}

/// Mechanical metadata projection, not a second harness parser. The exact
/// JSON keys and all other metadata remain in the authenticated source span.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecordFields {
    pub native_id: Option<String>,
    pub role: Option<String>,
    pub timestamp: Option<String>,
    pub parent_id: Option<String>,
    pub branch_id: Option<String>,
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedRecord {
    pub format: String,
    pub event_id: EntityId,
    /// Same logical source position across versions. Distinct equal-text
    /// records have different ordinals and never collapse into one event.
    pub logical_event_id: EntityId,
    pub archive_session_id: EntityId,
    pub source: ExternalHistorySource,
    pub source_digest: Digest,
    pub ordinal: usize,
    pub byte_start: u64,
    pub byte_end: u64,
    pub record_blake3_hex: String,
    pub fields: SourceRecordFields,
    pub parse_error: Option<String>,
    pub projection_encoding: String,
    /// The complete text projection has its own authenticated object. Raw
    /// source bytes remain authoritative; this derivative can be rebuilt.
    pub text_ref: Option<ObjectRef>,
    /// A complete string-leaf projection, never a summary or fixed prefix.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TextChunk {
    record: ArchivedRecord,
    text_start_char: usize,
    text_end_byte: usize,
    next_record_seq: i64,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveCursor {
    pub archive_session_id: EntityId,
    pub next_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchivedHit {
    pub event_id: EntityId,
    pub logical_event_id: EntityId,
    pub archive_session_id: EntityId,
    pub source: ExternalHistorySource,
    pub source_digest: Digest,
    pub ordinal: usize,
    pub byte_start: u64,
    pub byte_end: u64,
    pub fields: SourceRecordFields,
    pub preview: String,
    pub preview_start_char: usize,
    pub preview_truncated_before: bool,
    pub preview_truncated_after: bool,
    pub citation: String,
    /// An archive is a historical version, not proof of the live source.
    pub freshness: String,
    pub original_integrity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveSearch {
    pub hits: Vec<ArchivedHit>,
    pub next_cursor: Option<ArchiveCursor>,
    pub scanned_sessions: usize,
    pub scanned_records: usize,
    pub scanned_bytes: usize,
    pub budget_exhausted: bool,
    pub skipped_retired_versions: usize,
    pub unavailable_versions: Vec<EntityId>,
    pub scope: String,
    pub source_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceView {
    pub archive: ArchivedSource,
    /// Presence is checked; integrity is authenticated when bytes are read.
    pub object_status: String,
    pub retention_status: String,
    pub freshness: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveOverview {
    pub sources: Vec<SourceView>,
    pub next_after: Option<EntityId>,
    pub scanned_sessions: usize,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OriginalPage {
    pub archive_session_id: EntityId,
    pub digest: Digest,
    pub total_bytes: u64,
    pub offset: u64,
    pub next_offset: Option<u64>,
    pub content_base64: String,
    pub authenticated: bool,
}

fn stable_id(domain: &str, parts: &[&[u8]]) -> EntityId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    // RFC 9562 UUIDv8: application-defined deterministic identity.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    EntityId(uuid::Uuid::from_bytes(bytes))
}

fn source_key(source: &ExternalHistorySource) -> Result<Vec<u8>> {
    for value in [
        &source.provider,
        &source.source_instance,
        &source.source_key,
    ] {
        ensure!(
            !value.trim().is_empty() && value.len() <= 4096,
            "invalid external-history source identity"
        );
    }
    ensure!(
        source.session_id.as_ref().is_none_or(|id| id.len() <= 4096),
        "external-history native session hint exceeds metadata bound"
    );
    // Native session metadata is informative; a later discovery of its ID
    // must not quietly change the configured portable origin identity.
    Ok(serde_json::to_vec(&(
        &source.provider,
        &source.source_instance,
        &source.source_key,
    ))?)
}

fn retired_objects(store: &Store) -> Result<BTreeSet<String>> {
    Ok(store
        .retention_tombstones(Some("object"))?
        .into_iter()
        .flat_map(|t| {
            t.target_id
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect())
}

/// Capture the full file, refusing an over-budget or observed changing source
/// rather than calling a prefix complete. Two reads use the same descriptor;
/// the archived bytes are committed only after length, metadata and hash agree.
pub fn archive_file(
    store: &Store,
    agent_id: EntityId,
    source: &ExternalHistorySource,
    path: &Path,
    max_bytes: u64,
) -> Result<ArchivedSource> {
    source_key(source)?;
    let max_bytes = max_bytes.min(MAX_SOURCE_BYTES as u64);
    ensure!(
        max_bytes > 0,
        "external-history source byte budget must be positive"
    );
    let mut file = File::open(path).context("external-history source unavailable")?;
    let before = file.metadata()?;
    ensure!(
        before.is_file(),
        "external-history source is not a regular file"
    );
    ensure!(
        before.len() <= max_bytes,
        "external-history source exceeds byte budget; no prefix archived"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 == before.len(),
        "external-history source changed while capturing"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut second = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut reread = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        reread += n as u64;
        ensure!(
            reread <= max_bytes,
            "external-history source grew while verifying"
        );
        second.update(&buffer[..n]);
    }
    let after = file.metadata()?;
    ensure!(
        before.len() == after.len()
            && before.modified()? == after.modified()?
            && reread == bytes.len() as u64
            && second.finalize() == blake3::hash(&bytes),
        "external-history source changed while verifying; retry against a stable snapshot"
    );
    archive_bytes(store, agent_id, source, &bytes)
}

/// Admit a complete source snapshot. This does not imply that the source was
/// fully indexed upstream, or that other devices have already replicated it.
pub fn archive_bytes(
    store: &Store,
    agent_id: EntityId,
    source: &ExternalHistorySource,
    bytes: &[u8],
) -> Result<ArchivedSource> {
    let origin = source_key(source)?;
    ensure!(
        bytes.len() <= MAX_SOURCE_BYTES,
        "external-history source exceeds 64 MiB archive bound; no prefix archived"
    );
    let digest = Digest::blake3(bytes);
    ensure!(
        !retired_objects(store)?.contains(&digest.0),
        "external-history source is retired; old imports cannot revive it"
    );
    let archive_id = stable_id(
        FORMAT,
        &[
            agent_id.to_string().as_bytes(),
            &origin,
            digest.0.as_bytes(),
        ],
    );
    let records = parse_records(bytes, source, archive_id, &digest, &origin)?;
    let record_count = records.len();
    let invalid_records = records.iter().filter(|r| r.parse_error.is_some()).count();
    let started_at = records
        .iter()
        .filter_map(|r| r.fields.timestamp.as_ref())
        .find(|time| chrono::DateTime::parse_from_rfc3339(time).is_ok())
        .cloned()
        .unwrap_or_default();
    let mut events = Vec::new();
    let mut index_bytes = 0usize;
    for mut record in records {
        let text = std::mem::take(&mut record.text);
        let mut text_ref = store
            .cas
            .seal_object(
                text.as_bytes(),
                "text/plain; charset=utf-8",
                "external-history",
                "external-history-text-projection",
                1,
            )
            .inspect_err(|_| {
                store.close_external_writer();
            })?
            .object;
        text_ref.locator.clear();
        record.text_ref = Some(text_ref);
        let spans = text_spans(&text);
        let next_record_seq = i64::try_from(events.len() + 1 + spans.len())?;
        let role = record
            .fields
            .role
            .clone()
            .unwrap_or_else(|| "external_observation".into());
        let data = serde_json::to_string(&record)?;
        ensure!(
            data.len() <= MAX_PROJECTION_BYTES,
            "external-history record metadata exceeds projection bound"
        );
        index_bytes += data.len();
        ensure!(
            index_bytes <= MAX_INDEX_PROJECTION_BYTES,
            "external-history index projection exceeds 256 MiB; no partial snapshot committed"
        );
        events.push(RawSessionEvent {
            event_id: record.event_id,
            session_id: archive_id,
            seq: i64::try_from(events.len())?,
            event_type: EVENT_TYPE.into(),
            role: role.clone(),
            data,
        });
        for (index, (start, end, char_offset)) in spans.into_iter().enumerate() {
            let chunk = TextChunk {
                record: record.clone(),
                text_start_char: char_offset,
                text_end_byte: end,
                next_record_seq,
                text: text[start..end].to_owned(),
            };
            let data = serde_json::to_string(&chunk)?;
            ensure!(
                data.len() <= MAX_PROJECTION_BYTES,
                "external-history text chunk exceeds projection bound"
            );
            index_bytes += data.len();
            ensure!(
                index_bytes <= MAX_INDEX_PROJECTION_BYTES,
                "external-history index projection exceeds 256 MiB; no partial snapshot committed"
            );
            events.push(RawSessionEvent {
                event_id: stable_id(
                    "external-history-text-chunk/1",
                    &[
                        record.event_id.to_string().as_bytes(),
                        &(index as u64).to_le_bytes(),
                    ],
                ),
                session_id: archive_id,
                seq: i64::try_from(events.len())?,
                event_type: TEXT_EVENT_TYPE.into(),
                role: role.clone(),
                data,
            });
        }
    }
    let mut raw_ref = store
        .cas
        .seal_object(
            bytes,
            "application/octet-stream",
            "external-history",
            "external-history-original",
            1,
        )
        .inspect_err(|_| {
            store.close_external_writer();
        })?
        .object;
    // A node-local file:// locator would make independently imported copies
    // conflict. The digest resolves in each receiving node's own CAS.
    raw_ref.locator.clear();
    let archive = ArchivedSource {
        archive_session_id: archive_id,
        source: source.clone(),
        raw_ref,
        record_count,
        event_count: events.len(),
        invalid_records,
        bytes: bytes.len() as u64,
        chunks: bytes
            .chunks(CHUNK_BYTES)
            .enumerate()
            .map(|(i, chunk)| SourceChunk {
                offset: (i * CHUNK_BYTES) as u64,
                bytes: chunk.len(),
                blake3_hex: blake3::hash(chunk).to_hex().to_string(),
            })
            .collect(),
    };
    let session = RawSession {
        session_id: archive_id,
        agent_id,
        started_at,
        data: serde_json::to_string(&SourceEnvelope {
            format: FORMAT.into(),
            archive: archive.clone(),
        })?,
    };
    ensure!(
        session.data.len() <= MAX_PROJECTION_BYTES,
        "external-history source descriptor exceeds bound"
    );
    // The Store rechecks tombstones under the same mutation lock as these
    // rows, closing the capture/deletion race without a plugin-owned table.
    store.commit_raw_session_snapshot(&session, &events, &[digest])?;
    Ok(archive)
}

/// UTF-8 aligned windows with enough overlap for every admitted query.
/// Character offsets are accumulated once, avoiding a quadratic long-output
/// split. Overlap is a projection detail, never a second source event.
fn text_spans(text: &str) -> Vec<(usize, usize, usize)> {
    let mut spans = Vec::new();
    let mut position = 0;
    let mut char_position = 0;
    while position < text.len() {
        let mut end = (position + TEXT_CHUNK_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        let mut start = position.saturating_sub(QUERY_OVERLAP_BYTES);
        while !text.is_char_boundary(start) {
            start -= 1;
        }
        let overlap_chars = text[start..position].chars().count();
        spans.push((start, end, char_position - overlap_chars));
        char_position += text[position..end].chars().count();
        position = end;
    }
    spans
}

fn parse_records(
    bytes: &[u8],
    source: &ExternalHistorySource,
    archive: EntityId,
    digest: &Digest,
    origin: &[u8],
) -> Result<Vec<ArchivedRecord>> {
    let mut result = Vec::new();
    let origin_bytes = serde_json::to_vec(source)?.len();
    let mut projection_bytes = 0usize;
    let mut stream = serde_json::Deserializer::from_slice(bytes).into_iter::<serde_json::Value>();
    let mut start = 0usize;
    while let Some(value) = stream.next() {
        ensure!(
            result.len() < MAX_SOURCE_RECORDS,
            "external-history source exceeds 32768 records; use an explicitly identified source partition"
        );
        let end = if value.is_ok() {
            stream.byte_offset()
        } else {
            bytes.len()
        };
        // Whitespace is still in the original CAS object and the first span;
        // keeping contiguous byte spans makes lossless reconstruction simple.
        let ordinal = result.len();
        let mut text = String::new();
        let (fields, parse_error, encoding) = match value {
            Ok(value) => {
                collect_text(&value, &mut text);
                (
                    project_fields(&value),
                    None,
                    "json_string_leaves".to_owned(),
                )
            }
            Err(error) => {
                if let Ok(raw) = std::str::from_utf8(&bytes[start..end]) {
                    text.push_str(raw);
                }
                (
                    SourceRecordFields::default(),
                    Some(error.to_string()),
                    if text.is_empty() {
                        "opaque_bytes"
                    } else {
                        "raw_utf8"
                    }
                    .to_owned(),
                )
            }
        };
        projection_bytes += text.len()
            + origin_bytes
            + serde_json::to_vec(&fields)?.len()
            + std::mem::size_of::<ArchivedRecord>();
        ensure!(
            projection_bytes <= MAX_DECODED_PROJECTION_BYTES,
            "external-history decoded projection exceeds 128 MiB; no prefix archived"
        );
        result.push(ArchivedRecord {
            format: FORMAT.into(),
            event_id: stable_id(
                "external-history-event/1",
                &[
                    archive.to_string().as_bytes(),
                    &(ordinal as u64).to_le_bytes(),
                ],
            ),
            logical_event_id: stable_id(
                "external-history-position/1",
                &[origin, &(ordinal as u64).to_le_bytes()],
            ),
            archive_session_id: archive,
            source: source.clone(),
            source_digest: digest.clone(),
            ordinal,
            byte_start: start as u64,
            byte_end: end as u64,
            record_blake3_hex: blake3::hash(&bytes[start..end]).to_hex().to_string(),
            fields,
            parse_error,
            projection_encoding: encoding,
            text_ref: None,
            text,
        });
        start = end;
        if result.last().is_some_and(|r| r.parse_error.is_some()) {
            break;
        }
    }
    Ok(result)
}

fn collect_text(value: &serde_json::Value, text: &mut String) {
    match value {
        serde_json::Value::String(s) => {
            text.push_str(s);
            text.push('\n');
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_text(value, text);
            }
        }
        serde_json::Value::Object(values) => {
            for (key, value) in values {
                text.push_str(key);
                text.push('\n');
                collect_text(value, text);
            }
        }
        other => {
            text.push_str(&other.to_string());
            text.push('\n');
        }
    }
}

fn project_fields(value: &serde_json::Value) -> SourceRecordFields {
    let field = |pointers: &[&str]| {
        pointers.iter().find_map(|pointer| {
            value
                .pointer(pointer)
                .and_then(serde_json::Value::as_str)
                .filter(|s| s.len() <= 512)
                .map(str::to_owned)
        })
    };
    SourceRecordFields {
        native_id: field(&["/id", "/uuid", "/message/id", "/payload/id"]),
        role: field(&["/role", "/message/role", "/payload/role"]),
        timestamp: field(&[
            "/timestamp",
            "/created_at",
            "/time",
            "/message/timestamp",
            "/payload/timestamp",
        ]),
        parent_id: field(&[
            "/parentId",
            "/parent_id",
            "/parentUuid",
            "/message/parent_id",
        ]),
        branch_id: field(&["/branch_id", "/branchId", "/branch"]),
        tool_call_id: field(&[
            "/tool_call_id",
            "/call_id",
            "/payload/call_id",
            "/message/toolCallId",
        ]),
    }
}

fn decode_source(data: &str) -> Result<Option<ArchivedSource>> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("format").and_then(serde_json::Value::as_str) != Some(FORMAT) {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_value::<SourceEnvelope>(value)?.archive,
    ))
}

fn source_archive(store: &Store, agent: EntityId, id: EntityId) -> Result<ArchivedSource> {
    let row = store
        .raw_session(id)?
        .context("external-history snapshot not found")?;
    ensure!(
        row.agent_id == agent,
        "external-history snapshot belongs to another subject"
    );
    let source =
        decode_source(&row.data)?.context("raw session is not an external-history snapshot")?;
    ensure!(
        source.archive_session_id == id,
        "external-history snapshot identity mismatch"
    );
    Ok(source)
}

/// A bounded source/version view. UUID order is only pagination order. This
/// says what is archived locally, not what is newest or globally complete.
pub fn overview(
    store: &Store,
    agent: EntityId,
    after: Option<EntityId>,
    limit: usize,
) -> Result<ArchiveOverview> {
    ensure!(
        (1..=MAX_PAGE_RECORDS).contains(&limit),
        "overview limit must be 1..=100"
    );
    let retired = retired_objects(store)?;
    let rows = store.raw_sessions_for_agent(agent, after, SESSION_SCAN_BUDGET + 1)?;
    let has_more = rows.len() > SESSION_SCAN_BUDGET;
    let mut sources = Vec::new();
    let mut scanned = 0;
    let mut last = after;
    let mut next_after = None;
    for row in rows.into_iter().take(SESSION_SCAN_BUDGET) {
        if sources.len() == limit {
            next_after = last;
            break;
        }
        scanned += 1;
        last = Some(row.session_id);
        if let Some(archive) = decode_source(&row.data)? {
            sources.push(SourceView {
                object_status: if store.cas.object_size(&archive.raw_ref.digest)
                    == Some(archive.bytes)
                {
                    "present_unverified"
                } else {
                    "missing_or_wrong_size"
                }
                .into(),
                retention_status: if retired.contains(&archive.raw_ref.digest.0) {
                    "retired"
                } else {
                    "active"
                }
                .into(),
                freshness: "not_revalidated".into(),
                archive,
            });
        }
    }
    if next_after.is_none() && has_more {
        next_after = last;
    }
    Ok(ArchiveOverview {
        sources,
        next_after,
        scanned_sessions: scanned,
        scope: "local_archived_versions; source completeness and recency are unknown".into(),
    })
}

/// Full string search over archived records, including CJK and long output
/// tails. A bounded scan returns a continuation even when this page is empty.
/// The result deliberately retains source versions; repeated versions of the
/// same logical_event_id are not independent corroboration.
pub fn search(
    store: &Store,
    agent: EntityId,
    query: &str,
    source: Option<&ExternalHistorySource>,
    cursor: Option<&ArchiveCursor>,
    limit: usize,
) -> Result<ArchiveSearch> {
    let query = query.trim();
    ensure!(
        !query.is_empty() && query.len() <= QUERY_OVERLAP_BYTES,
        "search requires 1..=4096 query bytes"
    );
    ensure!(
        (1..=MAX_PAGE_RECORDS).contains(&limit),
        "search limit must be 1..=100"
    );
    ensure!(
        cursor.is_none_or(|c| c.next_seq >= 0),
        "negative archive cursor"
    );
    let retired = retired_objects(store)?;
    let mut current = cursor
        .map(|c| store.raw_session(c.archive_session_id))
        .transpose()?
        .flatten();
    if let Some(row) = &current {
        ensure!(
            row.agent_id == agent,
            "archive cursor belongs to another subject"
        );
    }
    let mut after_session = cursor.map(|c| c.archive_session_id);
    let mut out = ArchiveSearch {
        hits: Vec::new(),
        next_cursor: None,
        scanned_sessions: 0,
        scanned_records: 0,
        scanned_bytes: 0,
        budget_exhausted: false,
        skipped_retired_versions: 0,
        unavailable_versions: Vec::new(),
        scope: "local_archived_versions; historical material, not current instructions".into(),
        source_status: if source.is_some() {
            "not_found_in_scanned_range"
        } else {
            "unfiltered"
        }
        .into(),
    };
    loop {
        if out.scanned_sessions == SESSION_SCAN_BUDGET
            || SEARCH_BYTE_BUDGET - out.scanned_bytes < MAX_PROJECTION_BYTES
        {
            out.budget_exhausted = true;
            out.next_cursor = after_session.map(|id| ArchiveCursor {
                archive_session_id: id,
                next_seq: i64::MAX,
            });
            return Ok(out);
        }
        let row = match current.take() {
            Some(row) => row,
            None => match store
                .raw_sessions_for_agent(agent, after_session, 1)?
                .into_iter()
                .next()
            {
                Some(row) => row,
                None => {
                    if cursor.is_none()
                        && source.is_some()
                        && out.source_status == "not_found_in_scanned_range"
                    {
                        out.source_status = "not_archived".into();
                    }
                    return Ok(out);
                }
            },
        };
        out.scanned_sessions += 1;
        after_session = Some(row.session_id);
        ensure!(
            row.data.len() <= MAX_PROJECTION_BYTES,
            "raw-session metadata exceeds history query bound"
        );
        out.scanned_bytes += row.data.len();
        let Some(archive) = decode_source(&row.data)? else {
            continue;
        };
        if source.is_some_and(|wanted| {
            wanted.provider != archive.source.provider
                || wanted.source_instance != archive.source.source_instance
                || wanted.source_key != archive.source.source_key
        }) {
            continue;
        }
        if source.is_some() {
            out.source_status = "present_in_scanned_range".into();
        }
        if retired.contains(&archive.raw_ref.digest.0) {
            out.skipped_retired_versions += 1;
            continue;
        }
        if store.cas.object_size(&archive.raw_ref.digest) != Some(archive.bytes) {
            out.unavailable_versions.push(row.session_id);
            continue;
        }
        let mut from = cursor
            .filter(|c| c.archive_session_id == row.session_id)
            .map_or(0, |c| c.next_seq);
        while from < i64::try_from(archive.event_count)? {
            // Bound bytes before fetching, even for a page with no matches.
            // Every projection row was admitted under MAX_PROJECTION_BYTES.
            let allowed = ((SEARCH_BYTE_BUDGET - out.scanned_bytes) / MAX_PROJECTION_BYTES)
                .min(SEARCH_RECORD_BUDGET - out.scanned_records)
                .min(16);
            if allowed == 0 {
                out.budget_exhausted = true;
                out.next_cursor = Some(ArchiveCursor {
                    archive_session_id: row.session_id,
                    next_seq: from,
                });
                return Ok(out);
            }
            let events = store.list_session_events(
                row.session_id,
                &SessionEventFilter {
                    from_seq: from,
                    limit: allowed,
                    event_types: vec![TEXT_EVENT_TYPE.into()],
                    ..Default::default()
                },
            )?;
            if events.is_empty() {
                break;
            }
            ensure!(
                events
                    .iter()
                    .all(|event| event.data.len() <= MAX_PROJECTION_BYTES),
                "external-history projection exceeds declared bound"
            );
            out.scanned_bytes += events.iter().map(|event| event.data.len()).sum::<usize>();
            out.scanned_records += events.len();
            for event in events {
                // A matched record skips its remaining overlapping chunks,
                // including any that the small batch already fetched.
                if event.seq < from {
                    continue;
                }
                ensure!(
                    event.data.len() <= MAX_PROJECTION_BYTES,
                    "external-history projection exceeds declared bound"
                );
                let chunk: TextChunk = serde_json::from_str(&event.data)?;
                ensure!(
                    chunk.record.archive_session_id == row.session_id
                        && chunk.record.source_digest == archive.raw_ref.digest,
                    "external-history text projection binding mismatch"
                );
                ensure!(
                    chunk.next_record_seq > event.seq
                        && chunk.next_record_seq <= i64::try_from(archive.event_count)?,
                    "external-history projection continuation is invalid"
                );
                if chunk
                    .record
                    .text_ref
                    .as_ref()
                    .is_some_and(|r| retired.contains(&r.digest.0))
                {
                    from = chunk.next_record_seq;
                    continue;
                }
                from = event.seq + 1;
                if literal_position(&chunk.text, query).is_none() {
                    continue;
                }
                let record = chunk.record;
                let (preview, start, before, after) = excerpt(&chunk.text, query);
                out.hits.push(ArchivedHit {
                    event_id: record.event_id,
                    logical_event_id: record.logical_event_id,
                    archive_session_id: record.archive_session_id,
                    source: record.source,
                    source_digest: record.source_digest.clone(),
                    ordinal: record.ordinal,
                    byte_start: record.byte_start,
                    byte_end: record.byte_end,
                    fields: record.fields,
                    preview,
                    preview_start_char: chunk.text_start_char + start,
                    preview_truncated_before: before || chunk.text_start_char > 0,
                    preview_truncated_after: after
                        || record
                            .text_ref
                            .as_ref()
                            .is_some_and(|r| r.bytes > chunk.text_end_byte as u64),
                    citation: format!(
                        "shenko-history://{}/{}?version={}&bytes={}-{}",
                        record.archive_session_id,
                        record.event_id,
                        record.source_digest,
                        record.byte_start,
                        record.byte_end
                    ),
                    freshness: "not_revalidated".into(),
                    original_integrity: "not_checked".into(),
                });
                from = chunk.next_record_seq;
                if out.hits.len() == limit {
                    out.next_cursor = Some(ArchiveCursor {
                        archive_session_id: row.session_id,
                        next_seq: from,
                    });
                    return Ok(out);
                }
            }
        }
    }
}

fn literal_position(text: &str, query: &str) -> Option<usize> {
    text.find(query)
        .or_else(|| {
            if query.is_ascii() {
                text.as_bytes()
                    .windows(query.len())
                    .position(|part| part.eq_ignore_ascii_case(query.as_bytes()))
            } else {
                None
            }
        })
        .filter(|&i| text.is_char_boundary(i))
}

fn excerpt(text: &str, query: &str) -> (String, usize, bool, bool) {
    let index = text
        .find(query)
        .or_else(|| {
            if query.is_ascii() {
                text.as_bytes()
                    .windows(query.len())
                    .position(|part| part.eq_ignore_ascii_case(query.as_bytes()))
            } else {
                None
            }
        })
        .filter(|&i| text.is_char_boundary(i));
    let start = index.map_or(0, |i| text[..i].chars().count().saturating_sub(100));
    let chars: Vec<char> = text.chars().skip(start).take(401).collect();
    let after = chars.len() > 400;
    (
        chars.into_iter().take(400).collect(),
        start,
        start > 0,
        after,
    )
}

pub fn read_record(
    store: &Store,
    agent: EntityId,
    event: EntityId,
    include_retired: bool,
) -> Result<ArchivedRecord> {
    let row = store
        .session_event(event)?
        .context("external-history event not found")?;
    ensure!(
        row.event_type == EVENT_TYPE,
        "not an external-history record"
    );
    let archive = source_archive(store, agent, row.session_id)?;
    let retired = retired_objects(store)?;
    ensure!(
        include_retired || !retired.contains(&archive.raw_ref.digest.0),
        "external-history version is retired"
    );
    let mut record: ArchivedRecord = serde_json::from_str(&row.data)?;
    ensure!(
        include_retired
            || record
                .text_ref
                .as_ref()
                .is_none_or(|r| !retired.contains(&r.digest.0)),
        "external-history text projection is retired"
    );
    ensure!(
        record.event_id == event && record.source_digest == archive.raw_ref.digest,
        "external-history record binding mismatch"
    );
    // Do not turn a surviving index row into proof of an original whose
    // archive bytes have been lost or corrupted.
    let original = read_range(
        store,
        &archive,
        record.byte_start,
        record
            .byte_end
            .checked_sub(record.byte_start)
            .context("external-history record span is reversed")?,
    )?;
    ensure!(
        blake3::hash(&original).to_hex().as_str() == record.record_blake3_hex,
        "external-history record integrity mismatch"
    );
    if let Some(text_ref) = &record.text_ref {
        let bytes = store.cas.read_object(&text_ref.digest)?;
        ensure!(
            bytes.len() as u64 == text_ref.bytes && Digest::blake3(&bytes) == text_ref.digest,
            "external-history text projection integrity mismatch"
        );
        record.text =
            String::from_utf8(bytes).context("external-history text projection is not UTF-8")?;
    }
    Ok(record)
}

pub fn read_original(
    store: &Store,
    agent: EntityId,
    archive_id: EntityId,
    offset: u64,
    limit: usize,
    include_retired: bool,
) -> Result<OriginalPage> {
    ensure!(
        (1..=MAX_PAGE_BYTES).contains(&limit),
        "original read limit must be 1..=131072 bytes"
    );
    let archive = source_archive(store, agent, archive_id)?;
    ensure!(
        include_retired || !retired_objects(store)?.contains(&archive.raw_ref.digest.0),
        "external-history version is retired"
    );
    ensure!(
        offset <= archive.bytes,
        "original offset exceeds source length"
    );
    let length = (archive.bytes - offset).min(limit as u64);
    let bytes = read_range(store, &archive, offset, length)?;
    let end = offset + length;
    Ok(OriginalPage {
        archive_session_id: archive_id,
        digest: archive.raw_ref.digest,
        total_bytes: archive.bytes,
        offset,
        next_offset: (end < archive.bytes).then_some(end),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        authenticated: true,
    })
}

fn read_range(
    store: &Store,
    archive: &ArchivedSource,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>> {
    let end = offset
        .checked_add(length)
        .context("external-history range overflow")?;
    ensure!(
        end <= archive.bytes && archive.raw_ref.bytes == archive.bytes,
        "external-history range exceeds source"
    );
    ensure!(
        store.cas.object_size(&archive.raw_ref.digest) == Some(archive.bytes),
        "external-history original is missing or has changed size"
    );
    let mut result = Vec::with_capacity(usize::try_from(length)?);
    let mut covered = offset;
    for chunk in &archive.chunks {
        let chunk_end = chunk
            .offset
            .checked_add(chunk.bytes as u64)
            .context("chunk range overflow")?;
        if chunk_end <= offset || chunk.offset >= end {
            continue;
        }
        ensure!(
            chunk.offset <= covered && chunk.bytes <= CHUNK_BYTES && chunk_end <= archive.bytes,
            "external-history chunk manifest has a gap or invalid range"
        );
        let digest = Digest(format!("blake3:{}", chunk.blake3_hex));
        let bytes = store.cas.read_indexed_range(
            &archive.raw_ref.digest,
            archive.bytes,
            chunk.offset,
            chunk.bytes,
            &digest,
        )?;
        let from = usize::try_from(covered.saturating_sub(chunk.offset))?;
        let to = usize::try_from(end.min(chunk_end) - chunk.offset)?;
        result.extend_from_slice(&bytes[from..to]);
        covered = end.min(chunk_end);
    }
    ensure!(
        covered == end && result.len() as u64 == length,
        "external-history chunk manifest is incomplete"
    );
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_store::{CognitionImportOptions, RetentionTombstone};
    use std::path::PathBuf;

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Result<Self> {
            let path =
                std::env::temp_dir().join(format!("shenko-external-history-{}", EntityId::new()));
            std::fs::create_dir_all(&path)?;
            Ok(Self(path))
        }
        fn store(&self, name: &str) -> Result<Store> {
            Ok(Store::open(
                self.0.join(format!("{name}.db")),
                self.0.join(format!("{name}-cas")),
            )?)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn source() -> ExternalHistorySource {
        ExternalHistorySource {
            provider: "pi".into(),
            source_instance: "owned-history".into(),
            source_key: "session:trace-1".into(),
            session_id: Some("trace-1".into()),
        }
    }
    fn whole(store: &Store, agent: EntityId, archive: EntityId, retired: bool) -> Result<Vec<u8>> {
        let mut result = Vec::new();
        let mut offset = 0;
        loop {
            let page = read_original(store, agent, archive, offset, MAX_PAGE_BYTES, retired)?;
            assert!(page.authenticated);
            result.extend(base64::engine::general_purpose::STANDARD.decode(&page.content_base64)?);
            if let Some(next) = page.next_offset {
                offset = next;
            } else {
                break;
            }
        }
        Ok(result)
    }

    fn nonempty_search(
        store: &Store,
        agent: EntityId,
        query: &str,
        mut cursor: Option<ArchiveCursor>,
        limit: usize,
    ) -> Result<ArchiveSearch> {
        for _ in 0..16 {
            let page = search(store, agent, query, None, cursor.as_ref(), limit)?;
            assert!(page.scanned_bytes <= SEARCH_BYTE_BUDGET);
            assert!(page.scanned_records <= SEARCH_RECORD_BUDGET);
            if !page.hits.is_empty() || page.next_cursor.is_none() {
                return Ok(page);
            }
            cursor = page.next_cursor;
        }
        anyhow::bail!("history cursor did not make bounded progress");
    }

    #[test]
    fn complete_history_versions_keep_equal_events_and_search_chinese_tail() -> Result<()> {
        let tmp = Temp::new()?;
        let store = tmp.store("original")?;
        let agent = EntityId::new();
        let long = serde_json::json!({"id":"tool-1","parentId":"msg-0","branch_id":"original-branch",
            "message":{"role":"tool","toolCallId":"call-1","content":format!("{}真正结论在长输出尾部", "prefix ".repeat(380_000))},
            "timestamp":"2026-11-15T08:00:00Z"}).to_string();
        let same = r#"{"role":"user","content":"同样的字出现两次"}"#;
        let boundary = serde_json::json!({"content":format!("{}跨块边界独有证据", "x".repeat(TEXT_CHUNK_BYTES - 8 - 3))}).to_string();
        let bytes = format!("{long}\n{boundary}\n{same}\n{same}\n").into_bytes();
        let file = tmp.0.join("trace.jsonl");
        std::fs::write(&file, &bytes)?;
        let saved = archive_file(&store, agent, &source(), &file, bytes.len() as u64)?;
        assert_eq!(saved.record_count, 4);
        let copy = tmp.0.join("copied-history.jsonl");
        std::fs::write(&copy, &bytes)?;
        assert_eq!(
            saved,
            archive_file(&store, agent, &source(), &copy, bytes.len() as u64)?
        );
        std::fs::remove_file(file)?;
        assert_eq!(
            whole(&store, agent, saved.archive_session_id, false)?,
            bytes
        );
        let mut selector = source();
        selector.session_id = None; // A native-ID hint is not source identity.
        let initial = search(
            &store,
            agent,
            "真正结论在长输出尾部",
            Some(&selector),
            None,
            10,
        )?;
        assert!(initial.hits.is_empty() && initial.budget_exhausted);
        assert_eq!(initial.source_status, "present_in_scanned_range");
        let tail = nonempty_search(
            &store,
            agent,
            "真正结论在长输出尾部",
            initial.next_cursor,
            10,
        )?;
        assert_eq!(tail.hits.len(), 1);
        assert!(tail.hits[0].preview.contains("真正结论在长输出尾部"));
        assert!(tail.hits[0].preview_truncated_before);
        assert_eq!(tail.hits[0].fields.role.as_deref(), Some("tool"));
        assert_eq!(tail.hits[0].fields.parent_id.as_deref(), Some("msg-0"));
        let full = read_record(&store, agent, tail.hits[0].event_id, false)?;
        assert!(full.text.contains("真正结论在长输出尾部"));
        let boundary_hit = nonempty_search(&store, agent, "跨块边界独有证据", None, 10)?;
        assert_eq!(boundary_hit.hits.len(), 1);
        assert!(boundary_hit.hits[0].preview.contains("跨块边界独有证据"));
        let repeated = nonempty_search(&store, agent, "同样的字", None, 10)?;
        assert_eq!(repeated.hits.len(), 2);
        assert_ne!(
            repeated.hits[0].logical_event_id,
            repeated.hits[1].logical_event_id
        );
        let mut changed = bytes.clone();
        changed.extend_from_slice(
            b"{\"role\":\"assistant\",\"content\":\"a later source revision\"}\n",
        );
        let newer = archive_bytes(&store, agent, &source(), &changed)?;
        assert_ne!(newer.archive_session_id, saved.archive_session_id);
        let view = overview(&store, agent, None, 10)?;
        assert_eq!(view.sources.len(), 2);
        assert!(
            view.sources
                .iter()
                .all(|v| v.freshness == "not_revalidated")
        );
        let page = nonempty_search(&store, agent, "同样的字", None, 1)?;
        let next = nonempty_search(&store, agent, "同样的字", page.next_cursor, 1)?;
        assert_ne!(page.hits[0].event_id, next.hits[0].event_id);
        Ok(())
    }

    #[test]
    fn independently_imported_sources_replicate_and_old_archives_do_not_revive_retirement()
    -> Result<()> {
        let tmp = Temp::new()?;
        let a = tmp.store("node-a")?;
        let b = tmp.store("node-b")?;
        let agent = EntityId::new();
        let bytes =
            "{\"id\":\"origin\",\"role\":\"user\",\"content\":\"此前的设计结论\"}\n".as_bytes();
        let original = archive_bytes(&a, agent, &source(), bytes)?;
        let independent = archive_bytes(&b, agent, &source(), bytes)?;
        assert_eq!(original, independent);
        let mut archive = Vec::new();
        let exported = a.write_cognition_archive(&Default::default(), None, &mut archive)?;
        assert!(exported.complete, "{:?}", exported.gaps);
        let imported =
            b.import_cognition_archive(&archive[..], &CognitionImportOptions::default())?;
        assert!(imported.conflicts.is_empty(), "{:?}", imported.conflicts);
        assert!(imported.verified_objects > 0);
        assert_eq!(whole(&b, agent, original.archive_session_id, false)?, bytes);
        b.record_tombstone(&RetentionTombstone {
            tombstone_id: EntityId::new(),
            target_kind: "object".into(),
            target_id: original.raw_ref.digest.0.clone(),
            scope: "own_subject_history".into(),
            reason: "superseded design".into(),
            effect_record: r#"{"mechanism":"semantic_forget","bytes_recoverable":true}"#.into(),
            deleted_at_ms: 1,
        })?;
        b.import_cognition_archive(&archive[..], &Default::default())?;
        let hidden = search(&b, agent, "此前的设计", None, None, 10)?;
        assert!(hidden.hits.is_empty());
        assert_eq!(hidden.skipped_retired_versions, 1);
        assert!(archive_bytes(&b, agent, &source(), bytes).is_err());
        assert!(read_original(&b, agent, original.archive_session_id, 0, 100, false).is_err());
        // Semantic retirement leaves an explicit historical read available;
        // it is never reported as physical deletion.
        assert_eq!(whole(&b, agent, original.archive_session_id, true)?, bytes);
        let c = tmp.store("recovered-node")?;
        let mut retired_archive = Vec::new();
        b.write_cognition_archive(&Default::default(), None, &mut retired_archive)?;
        c.import_cognition_archive(&retired_archive[..], &Default::default())?;
        assert!(
            search(&c, agent, "此前的设计", None, None, 10)?
                .hits
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn incomplete_source_and_corrupt_original_are_visible_without_fabricated_evidence() -> Result<()>
    {
        let tmp = Temp::new()?;
        let store = tmp.store("node")?;
        let agent = EntityId::new();
        let bytes = "{\"id\":\"first\",\"content\":\"完整一条\"}\n{\"content\":\"未完成但原文保留"
            .as_bytes();
        let path = tmp.0.join("active.jsonl");
        std::fs::write(&path, bytes)?;
        assert!(archive_file(&store, agent, &source(), &path, 4).is_err());
        assert!(overview(&store, agent, None, 10)?.sources.is_empty());
        let saved = archive_file(&store, agent, &source(), &path, 1024)?;
        assert_eq!(saved.invalid_records, 1);
        assert_eq!(
            whole(&store, agent, saved.archive_session_id, false)?,
            bytes
        );
        assert_eq!(
            search(&store, agent, "未完成但原文保留", None, None, 10)?
                .hits
                .len(),
            1
        );
        let cas_path = store.cas.verified_object_path(&saved.raw_ref)?;
        let mut corrupt = bytes.to_vec();
        corrupt[0] = b'!';
        std::fs::write(&cas_path, &corrupt)?;
        assert!(read_original(&store, agent, saved.archive_session_id, 0, 1024, false).is_err());
        let hit = search(&store, agent, "未完成但原文保留", None, None, 10)?
            .hits
            .into_iter()
            .next()
            .context("corrupted-original query should retain an unverified index hit")?;
        assert_eq!(hit.original_integrity, "not_checked");
        assert!(read_record(&store, agent, hit.event_id, false).is_err());
        std::fs::remove_file(cas_path)?;
        let missing = search(&store, agent, "未完成但原文保留", None, None, 10)?;
        assert!(missing.hits.is_empty());
        assert_eq!(missing.unavailable_versions, vec![saved.archive_session_id]);
        Ok(())
    }
}
