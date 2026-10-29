//! Host-only, fsynced R0 work journal. Version 2 makes reset and interrupted
//! restore durable facts; schema 1 cannot express those recovery boundaries.

use crate::contract::{CellKind, CellRecord};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

pub const JOURNAL_SCHEMA: u32 = 2;
pub const SESSION_MANIFEST: &str = "session.json";
pub const JOURNAL_FILE: &str = "journal.jsonl";
pub const CELLS_DIR: &str = "cells";
pub const GUEST_HOME_DIR: &str = "home";
pub const GUEST_WORK_DIR: &str = "work";
pub const REPLAYS_DIR: &str = "replays";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecord {
    pub kind: String,
    pub schema_version: u32,
    pub session_id: EntityId,
    pub seq: u64,
    pub attempt: EntityId,
    pub artifact: EntityId,
    pub cell: EntityId,
    pub cell_seq: u64,
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub completed_unix_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryRecord {
    pub kind: String,
    pub schema_version: u32,
    pub session_id: EntityId,
    pub id: EntityId,
    /// Reset sequence, or zero for a restore attempt.
    pub seq: u64,
    /// The last cell included when this boundary was written.
    pub after_cell_seq: u64,
}

#[derive(Debug)]
pub struct Journal {
    pub dir: PathBuf,
    pub session_id: EntityId,
    pub cells: Vec<CellRecord>,
    pub replays: Vec<ReplayRecord>,
    pub torn_tail_at: Option<u64>,
    pub next_cell_seq: u64,
    pub next_replay_seq: u64,
    pub last_reset: Option<BoundaryRecord>,
    pub pending_reset: Option<BoundaryRecord>,
    pub active_restore: Option<BoundaryRecord>,
    next_reset_seq: u64,
    ids: HashSet<EntityId>,
    last_replayed_cell_seq: u64,
    module_blake3: String,
    generation: RuntimeGeneration,
}

fn journal_path(dir: &Path) -> PathBuf { dir.join(JOURNAL_FILE) }

pub fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn cell_path(cell: EntityId, extension: &str) -> PathBuf {
    PathBuf::from(CELLS_DIR).join(format!("{cell}.{extension}"))
}
fn replay_path(artifact: EntityId, extension: &str) -> PathBuf {
    PathBuf::from(REPLAYS_DIR).join(format!("{artifact}.{extension}"))
}

impl Journal {
    fn validate_common(&self, schema: u32, session: EntityId, id: EntityId) -> Result<()> {
        if schema != JOURNAL_SCHEMA || session != self.session_id || id == EntityId::nil() {
            bail!("REPL_JOURNAL_CORRUPT: schema, session or identity mismatch");
        }
        Ok(())
    }

    fn admit_cell(&mut self, record: CellRecord) -> Result<()> {
        self.validate_common(record.schema_version, record.session_id, record.cell)?;
        if record.kind != "cell" || record.seq != self.next_cell_seq
            || self.pending_reset.is_some() || self.active_restore.is_some()
            || !self.ids.insert(record.cell)
            || record.source_path != cell_path(record.cell, "source")
            || record.stdout_path != cell_path(record.cell, "stdout")
            || record.stderr_path != cell_path(record.cell, "stderr")
            || !valid_digest(&record.source_blake3) || !valid_digest(&record.stdout_blake3)
            || !valid_digest(&record.stderr_blake3)
            || record.module_blake3 != self.module_blake3
            || record.environment_generation != self.generation
        {
            bail!("REPL_JOURNAL_CORRUPT: invalid cell identity, binding, path or sequence");
        }
        self.next_cell_seq = self.next_cell_seq.checked_add(1).context("cell sequence overflow")?;
        self.cells.push(record);
        Ok(())
    }

    fn admit_replay(&mut self, record: ReplayRecord) -> Result<()> {
        self.validate_common(record.schema_version, record.session_id, record.artifact)?;
        let attempt = self.active_restore.as_ref().context("REPL_JOURNAL_CORRUPT: replay without restore")?;
        let cell = self.cells.iter().find(|c| c.seq == record.cell_seq)
            .context("REPL_JOURNAL_CORRUPT: replay references missing cell")?;
        let boundary = self.last_reset.as_ref().map_or(0, |r| r.after_cell_seq);
        if record.kind != "replay" || record.seq != self.next_replay_seq
            || record.attempt != attempt.id || record.cell != cell.cell || cell.cell_kind != CellKind::Pure
            || record.cell_seq <= boundary || record.cell_seq <= self.last_replayed_cell_seq
            || record.stdout_path != replay_path(record.artifact, "stdout")
            || record.stderr_path != replay_path(record.artifact, "stderr")
            || record.exit_code.is_some() == record.error.is_some()
            || !self.ids.insert(record.artifact)
        {
            bail!("REPL_JOURNAL_CORRUPT: invalid replay identity, path or sequence");
        }
        self.last_replayed_cell_seq = record.cell_seq;
        self.next_replay_seq = self.next_replay_seq.checked_add(1).context("replay sequence overflow")?;
        self.replays.push(record);
        Ok(())
    }

    fn admit_boundary(&mut self, record: BoundaryRecord) -> Result<()> {
        self.validate_common(record.schema_version, record.session_id, record.id)?;
        if record.after_cell_seq != self.next_cell_seq - 1 {
            bail!("REPL_JOURNAL_CORRUPT: boundary cell position mismatch");
        }
        match record.kind.as_str() {
            "reset" => {
                if self.pending_reset.is_some() || self.active_restore.is_some()
                    || record.seq != self.next_reset_seq || !self.ids.insert(record.id)
                { bail!("REPL_JOURNAL_CORRUPT: invalid reset boundary"); }
                self.next_reset_seq = self.next_reset_seq.checked_add(1).context("reset sequence overflow")?;
                self.last_reset = Some(record.clone());
                self.pending_reset = Some(record);
            }
            "reset_complete" => {
                let pending = self.pending_reset.as_ref().context("REPL_JOURNAL_CORRUPT: reset completion without reset")?;
                if pending.id != record.id || pending.seq != record.seq {
                    bail!("REPL_JOURNAL_CORRUPT: reset completion identity mismatch");
                }
                self.pending_reset = None;
            }
            "restore_begin" => {
                if self.pending_reset.is_some() || record.seq != 0 || !self.ids.insert(record.id) {
                    bail!("REPL_JOURNAL_CORRUPT: invalid restore start");
                }
                // A new attempt supersedes an incomplete previous attempt. It
                // always reconstructs from the baseline, never partial state.
                self.active_restore = Some(record);
                self.last_replayed_cell_seq = 0;
            }
            "restore_complete" => {
                let pending = self.active_restore.as_ref().context("REPL_JOURNAL_CORRUPT: restore completion without start")?;
                if record.seq != 0 || pending.id != record.id {
                    bail!("REPL_JOURNAL_CORRUPT: restore completion identity mismatch");
                }
                self.active_restore = None;
            }
            _ => bail!("REPL_JOURNAL_CORRUPT: unknown boundary"),
        }
        Ok(())
    }
}

/// Admit every complete line before executing anything. Only an unterminated
/// final line is a torn tail; a malformed newline-terminated record is corrupt.
pub fn load(dir: &Path, session_id: EntityId, module: &str, generation: &RuntimeGeneration) -> Result<Journal> {
    let file = File::open(journal_path(dir)).context("REPL_SESSION_INVALID: missing journal")?;
    let mut reader = BufReader::new(file);
    let mut journal = Journal {
        dir: dir.to_path_buf(), session_id, cells: Vec::new(), replays: Vec::new(),
        torn_tail_at: None, next_cell_seq: 1, next_replay_seq: 1, last_reset: None,
        pending_reset: None, active_restore: None, next_reset_seq: 1, ids: HashSet::new(),
        last_replayed_cell_seq: 0, module_blake3: module.to_owned(), generation: generation.clone(),
    };
    let mut offset = 0u64;
    loop {
        let mut line = Vec::new();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 { break; }
        if line.last() != Some(&b'\n') {
            journal.torn_tail_at = Some(offset);
            break;
        }
        offset = offset.checked_add(n as u64).context("journal offset overflow")?;
        let parsed: serde_json::Value = serde_json::from_slice(&line).context("REPL_JOURNAL_CORRUPT: invalid JSON")?;
        match parsed.get("kind").and_then(|v| v.as_str()) {
            Some("cell") => journal.admit_cell(serde_json::from_slice(&line).context("REPL_JOURNAL_CORRUPT: invalid cell")?)?,
            Some("replay") => {
                if parsed.get("exit_code").is_none() || parsed.get("error").is_none() {
                    bail!("REPL_JOURNAL_CORRUPT: missing replay outcome fields");
                }
                journal.admit_replay(serde_json::from_slice(&line).context("REPL_JOURNAL_CORRUPT: invalid replay")?)?
            }
            Some("reset" | "reset_complete" | "restore_begin" | "restore_complete") =>
                journal.admit_boundary(serde_json::from_slice(&line).context("REPL_JOURNAL_CORRUPT: invalid boundary")?)?,
            _ => bail!("REPL_JOURNAL_CORRUPT: unknown record kind"),
        }
    }
    Ok(journal)
}

fn repair_torn_tail(journal: &mut Journal) -> Result<()> {
    if let Some(at) = journal.torn_tail_at {
        let file = OpenOptions::new().write(true).open(journal_path(&journal.dir))?;
        file.set_len(at)?;
        file.sync_all()?;
        journal.torn_tail_at = None;
    }
    Ok(())
}

fn append_line(journal: &mut Journal, value: &impl Serialize) -> Result<()> {
    repair_torn_tail(journal)?;
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = OpenOptions::new().append(true).open(journal_path(&journal.dir))?;
    let at = file.metadata()?.len();
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        // Do not let a retry append after an uncertain or partial write.
        journal.torn_tail_at = Some(at);
        return Err(error).context("append and fsync journal");
    }
    Ok(())
}

pub fn append_cell(journal: &mut Journal, mut record: CellRecord) -> Result<CellRecord> {
    record.kind = "cell".into(); record.schema_version = JOURNAL_SCHEMA;
    if record.seq != journal.next_cell_seq || record.session_id != journal.session_id
        || journal.pending_reset.is_some() || journal.active_restore.is_some()
    { bail!("REPL_JOURNAL_CORRUPT: cannot append cell"); }
    append_line(journal, &record)?;
    journal.admit_cell(record.clone())?;
    Ok(record)
}

pub fn append_replay(journal: &mut Journal, mut record: ReplayRecord) -> Result<u64> {
    record.kind = "replay".into(); record.schema_version = JOURNAL_SCHEMA;
    record.seq = journal.next_replay_seq;
    append_line(journal, &record)?;
    let seq = record.seq;
    journal.admit_replay(record)?;
    Ok(seq)
}

fn boundary(journal: &mut Journal, kind: &str, id: EntityId, seq: u64) -> Result<BoundaryRecord> {
    let record = BoundaryRecord { kind: kind.into(), schema_version: JOURNAL_SCHEMA,
        session_id: journal.session_id, id, seq, after_cell_seq: journal.next_cell_seq - 1 };
    append_line(journal, &record)?;
    journal.admit_boundary(record.clone())?;
    Ok(record)
}

pub fn begin_reset(journal: &mut Journal) -> Result<()> {
    if journal.pending_reset.is_some() || journal.active_restore.is_some() {
        bail!("REPL_RECOVERY_REQUIRED: finish interrupted operation first");
    }
    let seq = journal.next_reset_seq;
    boundary(journal, "reset", EntityId::new(), seq)?;
    Ok(())
}

pub fn complete_reset(journal: &mut Journal) -> Result<()> {
    let reset = journal.pending_reset.clone().context("no reset pending")?;
    boundary(journal, "reset_complete", reset.id, reset.seq)?;
    Ok(())
}

pub fn begin_restore(journal: &mut Journal) -> Result<EntityId> {
    let id = EntityId::new();
    boundary(journal, "restore_begin", id, 0)?;
    Ok(id)
}

pub fn complete_restore(journal: &mut Journal, id: EntityId) -> Result<()> {
    boundary(journal, "restore_complete", id, 0)?;
    Ok(())
}

pub fn write_fresh_file(path: &Path, bytes: &[u8]) -> Result<File> {
    let mut file = OpenOptions::new().read(true).write(true).create_new(true).open(path)
        .with_context(|| format!("create fresh durable file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    File::open(path.parent().context("file has no parent")?)?.sync_all()?;
    file.rewind()?;
    Ok(file)
}

pub fn hash_reader(mut file: impl Read) -> Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 { break; }
        hasher.update(&buffer[..n]);
        count = count.checked_add(n as u64).context("file length overflow")?;
    }
    Ok((hasher.finalize().to_hex().to_string(), count))
}

pub fn hash_file(path: &Path) -> Result<String> {
    Ok(hash_reader(File::open(path).with_context(|| format!("read {}", path.display()))?)?.0)
}
pub fn hash_bytes(bytes: &[u8]) -> String { blake3::hash(bytes).to_hex().to_string() }

pub fn write_manifest(dir: &Path, manifest: &impl Serialize) -> Result<()> {
    let path = dir.join(SESSION_MANIFEST);
    let tmp = dir.join(format!(".{SESSION_MANIFEST}.{}", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    write_fresh_file(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Copy only regular files/directories and sync data and directory entries
/// bottom-up before the fork manifest can become durable.
pub fn copy_dir(source: &Path, target: &Path) -> Result<()> {
    if target.exists() { bail!("REPL_FORK_CONFLICT: {} already exists", target.display()); }
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = target.join(entry.file_name());
        if ty.is_dir() { copy_dir(&entry.path(), &dest)?; }
        else if ty.is_file() {
            let mut source = File::open(entry.path())?;
            let mut target = OpenOptions::new().write(true).create_new(true).open(&dest)?;
            std::io::copy(&mut source, &mut target)?;
            target.sync_all()?;
        } else { bail!("REPL_FORK_UNSUPPORTED: {} is not a regular file or directory", entry.path().display()); }
    }
    File::open(target)?.sync_all()?;
    Ok(())
}
