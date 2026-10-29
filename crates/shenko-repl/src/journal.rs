//! The R0 work-record journal: one fsynced JSONL per session plus per-cell
//! durable files. The journal lives on the host side of the session root — it
//! is deliberately outside every guest preopen, so a guest can never rewrite
//! its own work record.

use crate::contract::CellRecord;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// JSONL schema version for independent readers. 1 = first shipped layout.
pub const JOURNAL_SCHEMA: u32 = 1;

pub const SESSION_MANIFEST: &str = "session.json";
pub const JOURNAL_FILE: &str = "journal.jsonl";
pub const CELLS_DIR: &str = "cells";
pub const GUEST_HOME_DIR: &str = "home";
pub const GUEST_WORK_DIR: &str = "work";
pub const REPLAYS_DIR: &str = "replays";

/// Replay audit line: a new work-record fact, never a mutated cell record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayRecord {
    /// "replay".
    pub kind: String,
    pub schema_version: u32,
    pub session_id: EntityId,
    /// 1-based position in the replay series of this session.
    pub seq: u64,
    /// Cell whose source was re-executed.
    pub cell: EntityId,
    /// Source cell position being replayed.
    pub cell_seq: u64,
    /// Where the replayed output went (files under `replays/`).
    pub stdout_path: PathBuf,
    pub stderr_path: PathBuf,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    pub completed_unix_ms: i64,
}

#[derive(Debug)]
pub struct Journal {
    pub dir: PathBuf,
    /// Parsed cell records in order (torn tail excluded).
    pub cells: Vec<CellRecord>,
    pub replays: Vec<ReplayRecord>,
    /// Byte offset where a torn trailing record starts, if a crash truncated
    /// the last write. Appending repairs by truncating to this offset first.
    pub torn_tail_at: Option<u64>,
    /// Next seq for the cell and replay series (monotonic, never rewound).
    pub next_cell_seq: u64,
    pub next_replay_seq: u64,
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join(JOURNAL_FILE)
}

enum LastLine {
    Cell(usize),
    Replay(usize),
    None,
}

/// Load a session journal. A torn tail — an unparseable final line or a last
/// record whose terminating newline never landed — is tolerated, excluded
/// from the record list and reported, rather than silently fabricating a gap
/// or corrupting the next append. A corrupt mid-journal line is a hard error.
pub fn load(dir: &Path) -> Result<Journal> {
    let path = journal_path(dir);
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("REPL_SESSION_INVALID: missing journal {}", path.display())
        }
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let file_len = file.metadata()?.len();
    let ends_with_newline = if file_len == 0 {
        true
    } else {
        let mut tail = [0u8; 1];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut tail)?;
        tail[0] == b'\n'
    };
    file.seek(SeekFrom::Start(0))?;
    let reader = BufReader::new(&file);

    let mut cells = Vec::new();
    let mut replays = Vec::new();
    let mut offset: u64 = 0;
    let mut torn_tail_at = None;
    let mut last_line: (u64, LastLine) = (0, LastLine::None);
    for (index, line) in reader.split(b'\n').enumerate() {
        let line = line?;
        let start = offset;
        offset += line.len() as u64 + 1;
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<serde_json::Value>(&line) {
            Ok(parsed) => {
                last_line = match parsed.get("kind").and_then(|k| k.as_str()) {
                    Some("cell") => {
                        cells.push(serde_json::from_value(parsed).context(
                            "REPL_JOURNAL_CORRUPT: cell record does not match schema",
                        )?);
                        (start, LastLine::Cell(cells.len() - 1))
                    }
                    Some("replay") => {
                        replays.push(serde_json::from_value(parsed).context(
                            "REPL_JOURNAL_CORRUPT: replay record does not match schema",
                        )?);
                        (start, LastLine::Replay(replays.len() - 1))
                    }
                    other => {
                        bail!("REPL_JOURNAL_CORRUPT: line {index} has kind {other:?}")
                    }
                };
            }
            Err(_) => {
                // Only the final segment may be torn; verify it reaches EOF.
                if start + line.len() as u64 == file_len {
                    torn_tail_at = Some(start);
                    break;
                }
                bail!("REPL_JOURNAL_CORRUPT: line {index} is not valid JSON");
            }
        }
    }
    // A last record that parsed but whose terminating newline never landed is
    // indistinguishable from a torn write: exclude it and repair by truncate.
    if torn_tail_at.is_none() && !ends_with_newline {
        match last_line {
            (start, LastLine::Cell(i)) => {
                cells.remove(i);
                torn_tail_at = Some(start);
            }
            (start, LastLine::Replay(i)) => {
                replays.remove(i);
                torn_tail_at = Some(start);
            }
            (_, LastLine::None) => {}
        }
    }
    let next_cell_seq = cells.last().map(|c| c.seq + 1).unwrap_or(1);
    let next_replay_seq = replays.last().map(|r| r.seq + 1).unwrap_or(1);
    Ok(Journal {
        dir: dir.to_path_buf(),
        cells,
        replays,
        torn_tail_at,
        next_cell_seq,
        next_replay_seq,
    })
}

/// Repair a torn tail before appending: truncate to the last complete line.
fn repair_torn_tail(journal: &mut Journal) -> Result<()> {
    if let Some(at) = journal.torn_tail_at.take() {
        let file = OpenOptions::new()
            .write(true)
            .open(journal_path(&journal.dir))?;
        file.set_len(at)?;
        file.sync_all()?;
    }
    Ok(())
}

fn append_line(journal: &Journal, value: &impl Serialize) -> Result<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(journal_path(&journal.dir))
        .context("open journal for append")?;
    serde_json::to_writer(&mut file, value).context("serialize journal record")?;
    file.write_all(b"\n")?;
    file.sync_all().context("fsync journal")?;
    Ok(())
}

/// Append a completed cell record and fsync the journal. The record is the
/// durable R0 fact; failure here must propagate, never report success.
/// The journal assigns seq — callers must pass the current `next_cell_seq`.
/// Returns the stored record.
pub fn append_cell(journal: &mut Journal, mut record: CellRecord) -> Result<CellRecord> {
    if record.seq != journal.next_cell_seq {
        bail!("REPL_JOURNAL_CORRUPT: cell seq {} does not continue journal", record.seq);
    }
    repair_torn_tail(journal)?;
    record.kind = "cell".into();
    record.schema_version = JOURNAL_SCHEMA;
    append_line(journal, &record)?;
    journal.next_cell_seq += 1;
    let stored = record.clone();
    journal.cells.push(record);
    Ok(stored)
}

/// Append a replay audit record; the journal assigns seq (`seq == 0` in).
pub fn append_replay(journal: &mut Journal, mut record: ReplayRecord) -> Result<u64> {
    if record.seq != 0 {
        bail!("REPL_JOURNAL_CORRUPT: caller must not assign replay seq");
    }
    repair_torn_tail(journal)?;
    record.seq = journal.next_replay_seq;
    record.kind = "replay".into();
    record.schema_version = JOURNAL_SCHEMA;
    append_line(journal, &record)?;
    journal.next_replay_seq += 1;
    journal.replays.push(record);
    Ok(journal.next_replay_seq - 1)
}

/// Write exact bytes to a fresh durable file, fsync file and parent
/// directory. Fails rather than overwrite an existing path — reuse of a cell
/// file name would mean the journal is being rewritten, which is corruption.
pub fn write_fresh_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create fresh durable file {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// BLAKE3 hex of a file's exact bytes.
pub fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

/// BLAKE3 hex of bytes.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Write the session manifest atomically (temp + rename + fsync).
pub fn write_manifest(dir: &Path, manifest: &impl Serialize) -> Result<()> {
    let path = dir.join(SESSION_MANIFEST);
    let tmp = dir.join(format!(".{SESSION_MANIFEST}.tmp"));
    let bytes = serde_json::to_vec_pretty(manifest)?;
    {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

pub fn read_manifest<T: for<'de> Deserialize<'de>>(dir: &Path) -> Result<T> {
    let bytes =
        fs::read(dir.join(SESSION_MANIFEST)).context("REPL_SESSION_INVALID: missing manifest")?;
    serde_json::from_slice(&bytes).context("REPL_SESSION_INVALID: manifest does not match schema")
}

/// Recursive copy used by `fork`: copy guest-visible state directories only,
/// never the journal or cell output files.
pub fn copy_dir(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        bail!("REPL_FORK_CONFLICT: {} already exists", target.display());
    }
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = target.join(entry.file_name());
        if ty.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else if ty.is_file() {
            fs::copy(entry.path(), &dest)?;
        } else {
            bail!(
                "REPL_FORK_UNSUPPORTED: {} is not a regular file or directory",
                entry.path().display()
            );
        }
    }
    Ok(())
}
