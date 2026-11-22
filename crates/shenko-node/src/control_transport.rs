//! Bounded control-prefix and CAS transfer. No RPC repeats the whole history.
use super::{Node, Result, digest};
use crate::{
    ControlEntry,
    control::{collect_admission_references, control_value_digest},
    election::{control_kind_name, state_number},
};
use anyhow::{Context as _, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::CommitAdmissionObject,
    native_evidence::{
        AnchoredNativeEvidence, NativeEvidenceObjectIdentity, NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR,
        NATIVE_EVIDENCE_CLOSURE_BYTES, NATIVE_EVIDENCE_CLOSURE_OBJECTS, NATIVE_EVIDENCE_CODEC,
    },
    object::{Digest, ObjectRef},
};
use std::collections::{BTreeMap, BTreeSet};

pub const CONTROL_CHUNK_BYTES: usize = 64 * 1024;
pub const CONTROL_PAGE_BYTES: usize = 512 * 1024;
pub const MAX_CONTROL_ENTRY_BYTES: usize = 256 * 1024;
pub(crate) const CONTROL_TRANSPORT_SCHEMA: &str = "
CREATE TABLE node_control_admission_keys(conflict_key TEXT PRIMARY KEY,idx INTEGER NOT NULL);
CREATE UNIQUE INDEX node_control_admission_index ON node_control_admissions(idx);
CREATE TABLE node_control_prefixes(idx INTEGER PRIMARY KEY,digest TEXT NOT NULL,ballot_term INTEGER,proposer TEXT,run_start INTEGER NOT NULL);
CREATE INDEX node_control_kind_index ON node_control_log(kind,idx);
CREATE TABLE node_control_object_closures(digest TEXT NOT NULL REFERENCES objects(digest),media_type TEXT NOT NULL,bytes INTEGER NOT NULL,PRIMARY KEY(digest,media_type));
CREATE TABLE node_control_object_chunks(digest TEXT NOT NULL,total INTEGER NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(digest,offset));";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlFrontier {
    pub index: u64,
    pub term: u64,
    pub digest: Digest,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlLogPage {
    pub after: ControlFrontier,
    pub entries: Vec<ControlEntry>,
    pub through: ControlFrontier,
    pub head: ControlFrontier,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlObjectChunk {
    pub digest: Digest,
    pub total: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlObjectProgress {
    pub complete: bool,
    pub next_offset: u64,
}

#[derive(Default)]
struct NativeEvidenceDeltaBudget {
    seen: BTreeSet<NativeEvidenceObjectIdentity>,
    counted: BTreeSet<Digest>,
    bytes: u64,
}

impl NativeEvidenceDeltaBudget {
    fn include(
        &mut self,
        reference: &ObjectRef,
        predecessors: &BTreeSet<NativeEvidenceObjectIdentity>,
    ) -> Result<bool> {
        let identity = NativeEvidenceObjectIdentity::from(reference);
        if predecessors.contains(&identity) || !self.seen.insert(identity) {
            return Ok(false);
        }
        ensure!(
            self.seen.len() <= NATIVE_EVIDENCE_CLOSURE_OBJECTS,
            "native E1 incremental closure object budget exceeded"
        );
        if self.counted.insert(reference.digest.clone()) {
            self.bytes = self.bytes.checked_add(reference.bytes)
                .filter(|bytes| *bytes <= NATIVE_EVIDENCE_CLOSURE_BYTES)
                .context("native E1 incremental closure byte budget exceeded")?;
        }
        Ok(true)
    }
}

impl Node {
    pub(crate) fn remember_control_entry(tx: &Transaction<'_>, entry: &ControlEntry) -> Result<()> {
        let previous = if entry.index == 1 {
            Digest::blake3(b"shenko-control-prefix-v2")
        } else {
            Digest(
                tx.query_row(
                    "SELECT digest FROM node_control_prefixes WHERE idx=?1",
                    [i64::try_from(entry.index - 1)?],
                    |row| row.get(0),
                )
                .context("control predecessor digest missing")?,
            )
        };
        let root = digest(&(previous, control_value_digest(entry)?))?;
        let ballot = entry.durability.as_ref().map(|proof| &proof.ballot);
        let mut run_start = entry.index;
        if let Some(ballot) = ballot {
            let prior: Option<(Option<u64>, Option<String>, u64)> = tx
                .query_row(
                    "SELECT ballot_term,proposer,run_start FROM node_control_prefixes WHERE idx=?1",
                    [i64::try_from(entry.index - 1)?],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((Some(term), Some(proposer), start)) = prior
                && term == ballot.term
                && proposer == ballot.proposer.to_string()
            {
                run_start = start;
            }
        }
        let old: Option<String> = tx
            .query_row(
                "SELECT digest FROM node_control_prefixes WHERE idx=?1",
                [i64::try_from(entry.index)?],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            old.as_ref().is_none_or(|old| *old == root.0),
            "control prefix digest changed at a committed index"
        );
        tx.execute("INSERT OR IGNORE INTO node_control_prefixes(idx,digest,ballot_term,proposer,run_start) VALUES(?1,?2,?3,?4,?5)",
            params![i64::try_from(entry.index)?,root.0,ballot.map(|b| i64::try_from(b.term)).transpose()?,
                ballot.map(|b| b.proposer.to_string()),i64::try_from(run_start)?])?;
        Ok(())
    }

    pub(crate) fn migrate_control_frontiers(tx: &Transaction<'_>, existing: bool) -> Result<()> {
        for entry in Self::control_log_conn(tx)? {
            Self::remember_control_entry(tx, &entry)?;
        }
        tx.execute("INSERT INTO node_control_admission_keys(conflict_key,idx) SELECT conflict_key,idx FROM node_control_admissions",[])?;
        // Inline objects from the development schema-9 format remain in the
        // original immutable entry too. Copying them to CAS changes no value.
        for entry in Self::control_log_conn(tx)?
            .iter()
            .filter(|e| e.kind == crate::ControlKind::Admission)
        {
            let payload: crate::ControlAdmissionPayload =
                serde_json::from_value(entry.payload.clone())?;
            Self::store_admission_objects_tx(tx, &payload.objects)?;
        }
        let accepted: Vec<String> = tx
            .prepare("SELECT data FROM node_control_acceptances")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for raw in accepted {
            let accepted: crate::AcceptedControl = serde_json::from_str(&raw)?;
            if accepted.entry.kind == crate::ControlKind::Admission {
                let payload: crate::ControlAdmissionPayload =
                    serde_json::from_value(accepted.entry.payload)?;
                Self::store_admission_objects_tx(tx, &payload.objects)?;
            }
        }
        // Old full-prefix prepare records remain evidence. A format upgrade
        // cannot reuse one as a compact live promise; the next campaign must
        // recover its accepted value under a strictly later ballot.
        if existing && state_number(tx, "term")? > 0 {
            let term = state_number(tx, "term")?
                .checked_add(1)
                .context("control term exhausted during migration")?;
            crate::election::set_state(tx, "term", &term.to_string())?;
            crate::election::set_state(
                tx,
                "voted_for",
                &Option::<shenko_types::ids::EntityId>::None,
            )?;
            crate::election::set_state(tx, "role", &crate::ElectionRole::Follower)?;
        }
        Ok(())
    }

    pub(crate) fn control_frontier_at(conn: &Connection, index: u64) -> Result<ControlFrontier> {
        if index == 0 {
            return Ok(ControlFrontier {
                index: 0,
                term: 0,
                digest: Digest::blake3(b"shenko-control-prefix-v2"),
            });
        }
        ensure!(
            index <= state_number(conn, "commit_index")?,
            "frontier exceeds the committed control prefix"
        );
        let entry = Self::control_entry(conn, index)?.context("committed control entry missing")?;
        let digest = conn.query_row(
            "SELECT digest FROM node_control_prefixes WHERE idx=?1",
            [i64::try_from(index)?],
            |row| row.get::<_, String>(0),
        )?;
        Ok(ControlFrontier {
            index,
            term: entry.term,
            digest: Digest(digest),
        })
    }

    pub(crate) fn control_frontier_conn(conn: &Connection) -> Result<ControlFrontier> {
        Self::control_frontier_at(conn, state_number(conn, "commit_index")?)
    }
    pub fn control_frontier(&self) -> Result<ControlFrontier> {
        Self::control_frontier_conn(&self.conn)
    }
    pub fn retained_control_frontier(&self, index: u64) -> Result<ControlFrontier> {
        Self::control_frontier_at(&self.conn, index)
    }

    pub(crate) fn last_control_kind(
        conn: &Connection,
        index: u64,
        kind: crate::ControlKind,
    ) -> Result<Option<ControlEntry>> {
        let raw: Option<String> = conn.query_row("SELECT data FROM node_control_log WHERE kind=?1 AND idx<=?2 ORDER BY idx DESC LIMIT 1",
            params![control_kind_name(kind),i64::try_from(index)?],|row|row.get(0)).optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub(crate) fn configuration_at_frontier(
        conn: &Connection,
        index: u64,
    ) -> Result<Option<crate::MemberConfiguration>> {
        let Some(entry) =
            Self::last_control_kind(conn, index, crate::ControlKind::MemberConfiguration)?
        else {
            return Ok(None);
        };
        let payload: crate::ConfigurationEntry = serde_json::from_value(entry.payload)?;
        Self::config_at(conn, payload.revision)
    }

    pub(crate) fn prepare_covers_frontier(
        conn: &Connection,
        request: &crate::VoteRequest,
    ) -> Result<bool> {
        Self::prepare_covers_index(conn, request, state_number(conn, "commit_index")?)
    }

    pub(crate) fn prepare_covers_index(
        conn: &Connection,
        request: &crate::VoteRequest,
        index: u64,
    ) -> Result<bool> {
        let current = Self::control_frontier_at(conn, index)?;
        if request.last_log_index > current.index {
            return Ok(false);
        }
        let prepared = Self::control_frontier_at(conn, request.last_log_index)?;
        if prepared.digest != request.prefix_digest || prepared.term != request.last_log_term {
            return Ok(false);
        }
        if current.index == prepared.index {
            return Ok(true);
        }
        let (term, proposer, start): (Option<u64>, Option<String>, u64) = conn.query_row(
            "SELECT ballot_term,proposer,run_start FROM node_control_prefixes WHERE idx=?1",
            [i64::try_from(current.index)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(term == Some(request.term)
            && proposer.as_deref() == Some(request.candidate_id.to_string().as_str())
            && start <= request.last_log_index + 1)
    }

    /// One bounded page, anchored by an exact committed frontier. History is
    /// retained on disk; the wire sends only the missing suffix.
    pub fn control_log_page(&self, after: u64) -> Result<ControlLogPage> {
        let tx = self.conn.unchecked_transaction()?;
        let head = Self::control_frontier_conn(&tx)?;
        ensure!(
            after <= head.index,
            "requested control cursor is ahead of the source"
        );
        let after = Self::control_frontier_at(&tx, after)?;
        let mut through = after.clone();
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        {
            let mut statement = tx.prepare(
                "SELECT data FROM node_control_log WHERE idx>?1 AND idx<=?2 ORDER BY idx",
            )?;
            let rows = statement.query_map(
                params![i64::try_from(after.index)?, i64::try_from(head.index)?],
                |row| row.get::<_, String>(0),
            )?;
            for raw in rows {
                let raw = raw?;
                ensure!(
                    raw.len() <= crate::MAX_NODE_REQUEST / 2,
                    "retained control entry needs a larger explicit recovery transport"
                );
                if !entries.is_empty() && bytes + raw.len() > CONTROL_PAGE_BYTES {
                    break;
                }
                let entry: ControlEntry = serde_json::from_str(&raw)?;
                bytes += raw.len();
                through = Self::control_frontier_at(&tx, entry.index)?;
                entries.push(entry);
            }
        }
        tx.commit()?;
        Ok(ControlLogPage {
            after,
            entries,
            through,
            head,
        })
    }

    pub(crate) fn store_admission_objects_tx(
        tx: &Transaction<'_>,
        objects: &[CommitAdmissionObject],
    ) -> Result<()> {
        for object in objects {
            ensure!(
                object.reference.bytes == u64::try_from(object.bytes.len())?
                    && object.reference.digest == Digest::blake3(&object.bytes),
                "control object digest or length mismatch"
            );
            let existing: Option<Vec<u8>> = tx
                .query_row(
                    "SELECT bytes FROM objects WHERE digest=?1",
                    [&object.reference.digest.0],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                ensure!(existing == object.bytes, "control CAS digest collision")
            } else {
                tx.execute(
                    "INSERT INTO objects(digest,bytes) VALUES(?1,?2)",
                    params![object.reference.digest.0, object.bytes],
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn admission_objects_conn(
        conn: &Connection,
        root: &ObjectRef,
    ) -> Result<Vec<CommitAdmissionObject>> {
        let mut pending = vec![root.clone()];
        let mut visited = BTreeSet::new();
        let mut objects = BTreeMap::new();
        while let Some(reference) = pending.pop() {
            let bytes: Vec<u8> = conn
                .query_row(
                    "SELECT bytes FROM objects WHERE digest=?1",
                    [&reference.digest.0],
                    |row| row.get(0),
                )
                .with_context(|| {
                    format!("control admission object missing: {}", reference.digest)
                })?;
            ensure!(
                reference.bytes == u64::try_from(bytes.len())?
                    && reference.digest == Digest::blake3(&bytes),
                "invalid control admission object"
            );
            if (reference.media_type == "application/json"
                || reference.media_type.ends_with("+json"))
                && visited.insert(reference.digest.clone())
            {
                collect_admission_references(&serde_json::from_slice(&bytes)?, &mut pending);
            }
            objects
                .entry(reference.digest.clone())
                .or_insert(CommitAdmissionObject { reference, bytes });
        }
        Ok(objects.into_values().collect())
    }

    /// Only a verified, immutable whole subtree can be skipped. Receiving
    /// its root bytes alone never establishes this marker. Node CAS has no
    /// delete/update path; a future collector must preserve these closures.
    pub fn control_closure_present(&self, reference: &ObjectRef) -> Result<bool> {
        Self::control_closure_present_conn(&self.conn, reference)
    }

    fn control_closure_present_conn(conn: &Connection, reference: &ObjectRef) -> Result<bool> {
        let bytes: Option<u64> = conn
            .query_row(
                "SELECT bytes FROM node_control_object_closures WHERE digest=?1 AND media_type=?2",
                params![reference.digest.0, reference.media_type],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(bytes) = bytes {
            ensure!(
                bytes == reference.bytes,
                "verified control closure byte length changed"
            );
        }
        Ok(bytes.is_some())
    }

    pub(crate) fn verify_admission_closure_tx(
        tx: &Transaction<'_>,
        root: &ObjectRef,
    ) -> Result<()> {
        let mut pending = vec![(root.clone(), false)];
        let mut visiting = BTreeSet::new();
        while let Some((reference, finish)) = pending.pop() {
            let key = (reference.digest.clone(), reference.media_type.clone());
            if finish {
                tx.execute("INSERT OR IGNORE INTO node_control_object_closures(digest,media_type,bytes) VALUES(?1,?2,?3)",
                    params![reference.digest.0,reference.media_type,i64::try_from(reference.bytes)?])?;
                visiting.remove(&key);
                continue;
            }
            if Self::control_closure_present_conn(tx, &reference)? {
                continue;
            }
            ensure!(
                visiting.insert(key),
                "control content graph contains a cycle"
            );
            let bytes: Vec<u8> = tx
                .query_row(
                    "SELECT bytes FROM objects WHERE digest=?1",
                    [&reference.digest.0],
                    |row| row.get(0),
                )
                .with_context(|| {
                    format!("control admission object missing: {}", reference.digest)
                })?;
            ensure!(
                reference.bytes == bytes.len() as u64 && reference.digest == Digest::blake3(&bytes),
                "invalid control admission object"
            );
            let mut children = Vec::new();
            if reference.media_type == "application/json" || reference.media_type.ends_with("+json")
            {
                collect_admission_references(&serde_json::from_slice(&bytes)?, &mut children);
            }
            pending.push((reference, true));
            pending.extend(children.into_iter().map(|reference| (reference, false)));
        }
        Ok(())
    }

    pub fn control_object_progress(
        &self,
        digest: &Digest,
        total: u64,
    ) -> Result<ControlObjectProgress> {
        let size: Option<u64> = self
            .conn
            .query_row(
                "SELECT length(bytes) FROM objects WHERE digest=?1",
                [&digest.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(size) = size {
            ensure!(size == total, "control object size conflicts");
            return Ok(ControlObjectProgress {
                complete: true,
                next_offset: total,
            });
        }
        let last: Option<(u64,u64,u64)> = self.conn.query_row(
            "SELECT offset,length(bytes),total FROM node_control_object_chunks WHERE digest=?1 ORDER BY offset DESC LIMIT 1",
            [&digest.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let next_offset = if let Some((offset, length, stored_total)) = last {
            ensure!(
                stored_total == total,
                "control object transfer size changed"
            );
            offset + length
        } else {
            0
        };
        Ok(ControlObjectProgress {
            complete: false,
            next_offset,
        })
    }

    pub fn store_control_object_chunk(
        &mut self,
        chunk: &ControlObjectChunk,
    ) -> Result<ControlObjectProgress> {
        ensure!(
            chunk.digest.canonical_blake3_hex().is_some()
                && chunk.bytes.len() <= CONTROL_CHUNK_BYTES,
            "invalid control object chunk identity or frame size"
        );
        let end = chunk
            .offset
            .checked_add(u64::try_from(chunk.bytes.len())?)
            .context("control object offset overflow")?;
        ensure!(
            end <= chunk.total
                && chunk.offset.is_multiple_of(CONTROL_CHUNK_BYTES as u64)
                && (end == chunk.total || chunk.bytes.len() == CONTROL_CHUNK_BYTES),
            "control object chunk boundary mismatch"
        );
        i64::try_from(chunk.total).context("control object exceeds SQLite size range")?;
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        let present: Option<u64> = tx
            .query_row(
                "SELECT length(bytes) FROM objects WHERE digest=?1",
                [&chunk.digest.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(size) = present {
            ensure!(size == chunk.total, "control object size conflicts");
            tx.commit()?;
            return Ok(ControlObjectProgress {
                complete: true,
                next_offset: size,
            });
        }
        let previous: Option<(u64, Vec<u8>)> = tx
            .query_row(
                "SELECT total,bytes FROM node_control_object_chunks WHERE digest=?1 AND offset=?2",
                params![chunk.digest.0, i64::try_from(chunk.offset)?],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let last: Option<(u64,u64,u64)> = tx.query_row(
            "SELECT offset,length(bytes),total FROM node_control_object_chunks WHERE digest=?1 ORDER BY offset DESC LIMIT 1",
            [&chunk.digest.0],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let next = if let Some((offset, length, total)) = last {
            ensure!(total == chunk.total, "control object transfer size changed");
            offset + length
        } else {
            0
        };
        let next = if let Some((total, bytes)) = previous {
            ensure!(
                total == chunk.total && bytes == chunk.bytes,
                "control object chunk changed on retry"
            );
            next
        } else {
            ensure!(
                chunk.offset == next,
                "control object transfer must continue its durable cursor"
            );
            tx.execute("INSERT INTO node_control_object_chunks(digest,total,offset,bytes) VALUES(?1,?2,?3,?4)",
                params![chunk.digest.0,i64::try_from(chunk.total)?,i64::try_from(chunk.offset)?,chunk.bytes])?;
            end
        };
        let finished = next == chunk.total;
        if finished {
            // Assemble and hash once, after all contiguous chunks are durable.
            // Earlier chunks are not reread for every append.
            let mut complete = Vec::with_capacity(usize::try_from(chunk.total)?);
            {
                let mut statement = tx.prepare(
                    "SELECT bytes FROM node_control_object_chunks WHERE digest=?1 ORDER BY offset",
                )?;
                let rows =
                    statement.query_map([&chunk.digest.0], |row| row.get::<_, Vec<u8>>(0))?;
                for bytes in rows {
                    complete.extend(bytes?);
                }
            }
            ensure!(
                Digest::blake3(&complete) == chunk.digest,
                "completed control object digest mismatch"
            );
            tx.execute(
                "INSERT INTO objects(digest,bytes) VALUES(?1,?2)",
                params![chunk.digest.0, complete],
            )?;
            tx.execute(
                "DELETE FROM node_control_object_chunks WHERE digest=?1",
                [&chunk.digest.0],
            )?;
        }
        tx.commit()?;
        Ok(ControlObjectProgress {
            complete: finished,
            next_offset: next,
        })
    }

    /// Read direct typed references after a bounded transfer completed. The
    /// caller walks this closure locally; no unbounded reference list is sent.
    pub fn control_object_children(&self, reference: &ObjectRef) -> Result<Vec<ObjectRef>> {
        let bytes: Vec<u8> = self.conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&reference.digest.0],
            |row| row.get(0),
        )?;
        ensure!(
            reference.bytes == bytes.len() as u64 && reference.digest == Digest::blake3(&bytes),
            "control object reference mismatch"
        );
        let mut references = Vec::new();
        if reference.media_type == "application/json" || reference.media_type.ends_with("+json") {
            collect_admission_references(&serde_json::from_slice(&bytes)?, &mut references);
        }
        Ok(references)
    }

    pub fn read_control_object_chunk(
        &self,
        digest: &Digest,
        offset: u64,
    ) -> Result<ControlObjectChunk> {
        let (total, bytes): (u64, Vec<u8>) = self.conn.query_row(
            "SELECT length(bytes),substr(bytes,?2,?3) FROM objects WHERE digest=?1",
            params![
                digest.0,
                i64::try_from(
                    offset
                        .checked_add(1)
                        .context("control object offset overflow")?
                )?,
                i64::try_from(CONTROL_CHUNK_BYTES)?
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(offset <= total, "control object cursor is past the content");
        Ok(ControlObjectChunk {
            digest: digest.clone(),
            total,
            offset,
            bytes,
        })
    }
}

impl Node {
    /// Bring two replicas to their common certified frontier using bounded
    /// pages. The caller supplies one already authenticated peer route. This
    /// mechanism performs factual replication only; votes and acceptances are
    /// separate requests and never inferred from a successful page transfer.
    pub fn synchronize_control_peer(
        &mut self,
        peer: shenko_types::ids::EntityId,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<ControlFrontier> {
        let mut remote: ControlFrontier =
            serde_json::from_value(exchange(crate::Request::ControlFrontier)?)?;
        let local = self.control_frontier()?;
        if remote.index > local.index {
            let target = remote.index;
            let mut cursor = local;
            while cursor.index < target {
                let page: ControlLogPage =
                    serde_json::from_value(exchange(crate::Request::ControlPage {
                        after_index: cursor.index,
                    })?)?;
                ensure!(
                    page.after == cursor && !page.entries.is_empty(),
                    "peer control page does not extend the retained frontier"
                );
                for entry in &page.entries {
                    self.pull_entry_objects(entry, exchange)?;
                }
                let outcome = self.append_control_entries(&crate::AppendRequest {
                    schema: crate::ELECTION_WIRE_SCHEMA,
                    leader_id: peer,
                    prev_index: cursor.index,
                    prev_term: cursor.term,
                    entries: page.entries,
                    proposal: None,
                })?;
                ensure!(
                    outcome.reason == "accepted",
                    "peer control suffix refused: {}",
                    outcome.reason
                );
                cursor = self.control_frontier()?;
                ensure!(
                    cursor == page.through,
                    "peer page frontier differs from its verified values"
                );
            }
            remote = cursor;
        } else {
            ensure!(
                self.retained_control_frontier(remote.index)? == remote,
                "peer committed control prefix diverges; no history may be overwritten"
            );
            let own = self.local_node_id()?.context("node not initialized")?;
            while remote.index < local.index {
                let page = self.control_log_page(remote.index)?;
                ensure!(
                    page.after == remote && !page.entries.is_empty(),
                    "local control suffix changed during transfer"
                );
                for entry in &page.entries {
                    self.push_entry_objects(entry, exchange)?;
                }
                let outcome: crate::AppendOutcome =
                    serde_json::from_value(exchange(crate::Request::Append {
                        request: crate::AppendRequest {
                            schema: crate::ELECTION_WIRE_SCHEMA,
                            leader_id: own,
                            prev_index: remote.index,
                            prev_term: remote.term,
                            entries: page.entries,
                            proposal: None,
                        },
                    })?)?;
                ensure!(
                    outcome.reason == "accepted",
                    "control catch-up refused: {}",
                    outcome.reason
                );
                remote = serde_json::from_value(exchange(crate::Request::ControlFrontier)?)?;
                ensure!(
                    remote.index >= page.through.index
                        && remote.index <= self.control_frontier()?.index
                        && self.retained_control_frontier(remote.index)? == remote,
                    "receiver did not retain the transferred control frontier"
                );
            }
        }
        Ok(remote)
    }

    fn pull_entry_objects(
        &mut self,
        entry: &ControlEntry,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        if entry.kind == crate::ControlKind::Admission {
            let payload: crate::ControlAdmissionPayload =
                serde_json::from_value(entry.payload.clone())?;
            if !payload.objects.is_empty() {
                let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
                Self::store_admission_objects_tx(&tx, &payload.objects)?;
                tx.commit()?;
            }
            self.pull_control_objects(&payload.admission.payload_ref, exchange)?;
        }
        Ok(())
    }

    fn push_entry_objects(
        &self,
        entry: &ControlEntry,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        if entry.kind == crate::ControlKind::Admission {
            let payload: crate::ControlAdmissionPayload =
                serde_json::from_value(entry.payload.clone())?;
            self.push_control_objects(&payload.admission.payload_ref, exchange)?;
        }
        Ok(())
    }

    fn push_control_objects(
        &self,
        root: &ObjectRef,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        let mut pending = vec![root.clone()];
        let mut seen = BTreeSet::new();
        while let Some(reference) = pending.pop() {
            if !seen.insert((reference.digest.clone(), reference.media_type.clone())) {
                continue;
            }
            let closed: bool =
                serde_json::from_value(exchange(crate::Request::ControlClosurePresent {
                    reference: reference.clone(),
                })?)?;
            if closed {
                continue;
            }
            let mut progress: ControlObjectProgress =
                serde_json::from_value(exchange(crate::Request::ControlObjectProgress {
                    digest: reference.digest.clone(),
                    total: reference.bytes,
                })?)?;
            while !progress.complete {
                let chunk =
                    self.read_control_object_chunk(&reference.digest, progress.next_offset)?;
                ensure!(
                    chunk.total == reference.bytes,
                    "retained control object reference differs from its bytes"
                );
                let end = chunk.offset + chunk.bytes.len() as u64;
                progress = serde_json::from_value(exchange(crate::Request::StoreControlObject {
                    chunk,
                })?)?;
                ensure!(
                    progress.next_offset >= end && progress.next_offset <= reference.bytes,
                    "receiver returned an invalid control object cursor"
                );
            }
            ensure!(
                progress.next_offset == reference.bytes,
                "receiver completed another object size"
            );
            pending.extend(self.control_object_children(&reference)?);
        }
        Ok(())
    }

    fn pull_control_objects(
        &mut self,
        root: &ObjectRef,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        self.pull_objects_bounded(root, exchange, usize::MAX, u64::MAX)
    }

    /// Reuse immutable CAS transport without adding an admission, vote, log
    /// entry, execution receipt or grant. The Store adapter selects the root
    /// from its actual source observations before calling this method.
    pub fn retain_native_evidence_objects(
        &mut self,
        root: &ObjectRef,
        objects: &[CommitAdmissionObject],
        local_predecessor: &mut impl FnMut(&ObjectRef, bool) -> Result<Vec<CommitAdmissionObject>>,
    ) -> Result<()> {
        ensure!(objects.len() <= NATIVE_EVIDENCE_CLOSURE_OBJECTS, "native E1 closure object budget exceeded");
        let bytes = objects.iter().try_fold(0u64, |total, object| total.checked_add(object.reference.bytes));
        ensure!(bytes.is_some_and(|bytes| bytes <= NATIVE_EVIDENCE_CLOSURE_BYTES), "native E1 closure byte budget exceeded");
        let envelope = if Self::is_anchored_native_evidence(root) {
            let root_object = objects.iter().find(|object| {
                NativeEvidenceObjectIdentity::from(&object.reference)
                    == NativeEvidenceObjectIdentity::from(root)
            }).context("anchored native E1 envelope is missing from its objects")?;
            Some(AnchoredNativeEvidence::decode(root, &root_object.bytes)
                .map_err(anyhow::Error::msg)?)
        } else {
            None
        };
        if let Some(envelope) = &envelope {
            self.cache_local_native_evidence_predecessors(envelope, local_predecessor)?;
        }
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        // Inspect the immutable predecessor graph BEFORE accepting any of the
        // supplied bytes. E1 transport cannot repair or introduce a missing T1.
        let predecessors = envelope.as_ref()
            .map(|envelope| Self::native_evidence_predecessors_conn(&tx, envelope))
            .transpose()?;
        Self::store_admission_objects_tx(&tx, objects)?;
        if let Some(predecessors) = &predecessors {
            let (_, budget) = Self::native_evidence_delta_objects_conn(&tx, root, predecessors)?;
            ensure!(objects.iter().all(|object| {
                budget.seen.contains(&NativeEvidenceObjectIdentity::from(&object.reference))
            }), "native E1 transport includes objects outside its incremental closure");
        }
        Self::verify_admission_closure_tx(&tx, root)?;
        tx.commit()?;
        Ok(())
    }

    pub fn pull_native_evidence_objects(
        &mut self,
        root: &ObjectRef,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
        local_predecessor: &mut impl FnMut(&ObjectRef, bool) -> Result<Vec<CommitAdmissionObject>>,
    ) -> Result<Vec<CommitAdmissionObject>> {
        if !Self::is_anchored_native_evidence(root) {
            // The capacity and interpretation of original codecs stay fixed.
            self.pull_objects_bounded(root, exchange, NATIVE_EVIDENCE_CLOSURE_OBJECTS, NATIVE_EVIDENCE_CLOSURE_BYTES)?;
            return self.admission_objects(root);
        }
        ensure!(root.bytes <= NATIVE_EVIDENCE_CLOSURE_BYTES, "native E1 envelope byte budget exceeded");
        self.pull_native_evidence_object(root, exchange)?;
        let envelope = AnchoredNativeEvidence::decode(root, &Self::native_evidence_object_conn(&self.conn, root)?.bytes)
            .map_err(anyhow::Error::msg)?;
        self.cache_local_native_evidence_predecessors(&envelope, local_predecessor)?;
        let predecessors = Self::native_evidence_predecessors_conn(&self.conn, &envelope)?;
        let mut budget = NativeEvidenceDeltaBudget::default();
        let mut pending = vec![root.clone()];
        let mut objects = BTreeMap::new();
        while let Some(reference) = pending.pop() {
            if !budget.include(&reference, &predecessors)? {
                continue;
            }
            self.pull_native_evidence_object(&reference, exchange)?;
            let object = Self::native_evidence_object_conn(&self.conn, &reference)?;
            Self::native_evidence_children(&object, &mut pending)?;
            objects.entry(reference.digest).or_insert(object);
        }
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        Self::verify_admission_closure_tx(&tx, root)?;
        tx.commit()?;
        Ok(objects.into_values().collect())
    }

    fn is_anchored_native_evidence(root: &ObjectRef) -> bool {
        root.codec.name == NATIVE_EVIDENCE_CODEC
            && root.codec.major == NATIVE_EVIDENCE_ANCHORED_CODEC_MAJOR
    }

    /// Populate only this Node's byte cache from already applied, verified
    /// LOCAL Store predecessors. The adapter must use Store's factual
    /// predecessor lookup, never the remote E1 exchange. This grants no
    /// admission and installs no Store history. Existing closure markers are
    /// not repaired here: their actual bytes are checked by the inventory walk.
    /// The callback always validates actual local application; its bool asks
    /// for bytes only when this Node does not already have a closed cache.
    fn cache_local_native_evidence_predecessors(
        &mut self,
        envelope: &AnchoredNativeEvidence,
        local_predecessor: &mut impl FnMut(&ObjectRef, bool) -> Result<Vec<CommitAdmissionObject>>,
    ) -> Result<()> {
        for root in &envelope.predecessor_roots {
            let cached = self.control_closure_present(root)?;
            let objects = local_predecessor(root, !cached)
                .context("native E1 requires its original predecessor already applied locally")?;
            if cached {
                continue;
            }
            let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
            Self::store_admission_objects_tx(&tx, &objects)?;
            Self::verify_admission_closure_tx(&tx, root)?;
            tx.commit()?;
        }
        Ok(())
    }

    fn native_evidence_object_conn(conn: &Connection, reference: &ObjectRef) -> Result<CommitAdmissionObject> {
        let bytes: Vec<u8> = conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&reference.digest.0],
            |row| row.get(0),
        ).with_context(|| format!("native E1 predecessor or observation object missing: {}", reference.digest))?;
        ensure!(reference.bytes == bytes.len() as u64 && reference.digest == Digest::blake3(&bytes),
            "native E1 predecessor or observation object differs from its reference");
        Ok(CommitAdmissionObject { reference: reference.clone(), bytes })
    }

    fn native_evidence_children(object: &CommitAdmissionObject, pending: &mut Vec<ObjectRef>) -> Result<()> {
        if object.reference.media_type == "application/json" || object.reference.media_type.ends_with("+json") {
            collect_admission_references(&serde_json::from_slice(&object.bytes)?, pending);
        }
        Ok(())
    }

    fn native_evidence_predecessors_conn(
        conn: &Connection,
        envelope: &AnchoredNativeEvidence,
    ) -> Result<BTreeSet<NativeEvidenceObjectIdentity>> {
        for root in &envelope.predecessor_roots {
            ensure!(Self::control_closure_present_conn(conn, root)?,
                "native E1 predecessor closure was not already verified locally");
        }
        let mut pending = envelope.predecessor_roots.clone();
        let mut inventory = BTreeSet::new();
        while let Some(reference) = pending.pop() {
            if !inventory.insert(NativeEvidenceObjectIdentity::from(&reference)) {
                continue;
            }
            // Recheck actual bytes, including opaque bulk leaves. A closure
            // marker alone must not conceal corruption or a missing object.
            let object = Self::native_evidence_object_conn(conn, &reference)?;
            Self::native_evidence_children(&object, &mut pending)?;
        }
        Ok(inventory)
    }

    fn native_evidence_delta_objects_conn(
        conn: &Connection,
        root: &ObjectRef,
        predecessors: &BTreeSet<NativeEvidenceObjectIdentity>,
    ) -> Result<(Vec<CommitAdmissionObject>, NativeEvidenceDeltaBudget)> {
        let mut pending = vec![root.clone()];
        let mut budget = NativeEvidenceDeltaBudget::default();
        let mut objects = BTreeMap::new();
        while let Some(reference) = pending.pop() {
            if !budget.include(&reference, predecessors)? {
                continue;
            }
            let object = Self::native_evidence_object_conn(conn, &reference)?;
            Self::native_evidence_children(&object, &mut pending)?;
            objects.entry(reference.digest).or_insert(object);
        }
        Ok((objects.into_values().collect(), budget))
    }

    fn pull_native_evidence_object(
        &mut self,
        reference: &ObjectRef,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        let mut progress = self.control_object_progress(&reference.digest, reference.bytes)?;
        while !progress.complete {
            let chunk: ControlObjectChunk = serde_json::from_value(exchange(crate::Request::ReadControlObject {
                digest: reference.digest.clone(),
                offset: progress.next_offset,
            })?)?;
            ensure!(chunk.digest == reference.digest && chunk.total == reference.bytes
                && chunk.offset == progress.next_offset,
                "peer native E1 object chunk differs from the requested reference and cursor");
            progress = self.store_control_object_chunk(&chunk)?;
        }
        Ok(())
    }

    fn pull_objects_bounded(
        &mut self,
        root: &ObjectRef,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
        max_objects: usize,
        max_bytes: u64,
    ) -> Result<()> {
        let mut pending = vec![root.clone()];
        let mut seen = BTreeSet::new();
        let mut counted = BTreeSet::new();
        let mut total = 0u64;
        while let Some(reference) = pending.pop() {
            ensure!(pending.len() < max_objects, "factual CAS reference budget exceeded");
            if !seen.insert((reference.digest.clone(), reference.media_type.clone())) {
                continue;
            }
            if counted.insert(reference.digest.clone()) {
                ensure!(counted.len() <= max_objects, "factual CAS object budget exceeded");
                total = total.checked_add(reference.bytes).filter(|bytes| *bytes <= max_bytes)
                    .context("factual CAS byte budget exceeded")?;
            }
            if max_objects == usize::MAX && self.control_closure_present(&reference)? {
                continue;
            }
            let mut progress = self.control_object_progress(&reference.digest, reference.bytes)?;
            while !progress.complete {
                let chunk: ControlObjectChunk =
                    serde_json::from_value(exchange(crate::Request::ReadControlObject {
                        digest: reference.digest.clone(),
                        offset: progress.next_offset,
                    })?)?;
                ensure!(
                    chunk.digest == reference.digest
                        && chunk.total == reference.bytes
                        && chunk.offset == progress.next_offset,
                    "peer control object chunk differs from the requested reference and cursor"
                );
                progress = self.store_control_object_chunk(&chunk)?;
            }
            pending.extend(self.control_object_children(&reference)?);
        }
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
        Self::verify_admission_closure_tx(&tx, root)?;
        tx.commit()?;
        Ok(())
    }

    /// A compact prepare response may reveal a newer committed prefix or an
    /// accepted original plan. Obtain those immutable bytes before tallying;
    /// the prepare/accept validation still decides whether it grants authority.
    pub fn learn_control_prepare(
        &mut self,
        response: &crate::VoteResponse,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<()> {
        if response.committed.index > self.control_frontier()?.index {
            self.synchronize_control_peer(response.responder_id, exchange)?;
        }
        if let Some(accepted) = &response.accepted_control {
            self.pull_entry_objects(&accepted.entry, exchange)?;
        }
        Ok(())
    }

    /// Obtain an actual durable acceptance after bounded prefix/content
    /// transfer. The caller chooses whether to deliver this reply to its
    /// learner; a lost reply leaves the remote acceptance recoverable.
    pub fn replicate_control_to(
        &mut self,
        peer: shenko_types::ids::EntityId,
        request: &crate::ControlReplication,
        exchange: &mut impl FnMut(crate::Request) -> Result<serde_json::Value>,
    ) -> Result<crate::ControlAcknowledgement> {
        self.synchronize_control_peer(peer, exchange)?;
        self.push_entry_objects(&request.accepted.entry, exchange)?;
        let ack: crate::ControlAcknowledgement =
            serde_json::from_value(exchange(crate::Request::AcceptControl {
                request: request.clone(),
            })?)?;
        ensure!(
            ack.responder_id == peer,
            "control acknowledgement does not name the authenticated peer route"
        );
        Ok(ack)
    }
}
