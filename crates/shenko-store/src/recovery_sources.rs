//! Declared component-owned recovery data outside the control ledger.
//!
//! A read-only online SQLite backup includes committed WAL pages. Immutable CAS
//! chunks carry the actual database, not its path or a live provider binding.
//! Restoring an archive creates data only; it never starts a provider or replays
//! pending inputs/effects. Callers declare the source and its application ID;
//! this module does not discover environment files or credential stores.

use crate::store::Store;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, backup::{Backup, StepResult}, params};
use serde::{Deserialize, Serialize};
use shenko_types::object::{Digest, ObjectRef};
use std::{fs, io::{Read, Seek, Write}, path::Path, time::{Duration, Instant}};

const CHUNK_BYTES: usize = 1024 * 1024;
const SQLITE_CODEC: &str = "sqlite_recovery_chunks";
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoverySource {
    pub source_id: String,
    pub kind: String,
    pub artifact: ObjectRef,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SqliteSnapshot {
    schema: u32,
    application_id: i64,
    user_version: i64,
    bytes: u64,
    chunks: Vec<ObjectRef>,
}

fn sqlite_metadata(connection: &Connection) -> Result<(i64, i64)> {
    Ok((
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?,
        connection.pragma_query_value(None, "user_version", |row| row.get(0))?,
    ))
}

fn check_sqlite(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    ensure!(result == "ok", "recovery database integrity check failed: {result}");
    Ok(())
}

impl Store {
    /// Capture one explicitly declared data database. No timestamp or temporary
    /// path enters its identity, so an unchanged source creates no new debt.
    pub fn stage_sqlite_recovery_source(
        &self,
        source_id: &str,
        kind: &str,
        path: &Path,
        expected_application_id: i64,
    ) -> Result<RecoverySource> {
        let mut source = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        ).context("open declared recovery database read-only")?;
        source.busy_timeout(Duration::from_secs(5))?;
        let snapshot = source.transaction()?;
        let (application_id, user_version) = sqlite_metadata(&snapshot)?;
        ensure!(application_id == expected_application_id,
            "declared recovery database has a different application identity");
        // Establish the read snapshot before the incremental backup, preventing
        // concurrent ingress from repeatedly restarting the copied view.
        let _: i64 = snapshot.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
        let mut scratch = tempfile::NamedTempFile::new()?;
        {
            let mut destination = Connection::open(scratch.path())?;
            {
                let backup = Backup::new(&snapshot, &mut destination)?;
                let started = Instant::now();
                loop {
                    ensure!(started.elapsed() < SNAPSHOT_DEADLINE,
                        "declared recovery database snapshot exceeded 120 seconds");
                    match backup.step(256)? {
                        StepResult::Done => break,
                        StepResult::More => {},
                        StepResult::Busy | StepResult::Locked => std::thread::sleep(Duration::from_millis(10)),
                        _ => bail!("unsupported SQLite backup progress state"),
                    }
                }
            }
            check_sqlite(&destination)?;
            ensure!(sqlite_metadata(&destination)? == (application_id, user_version),
                "SQLite backup changed the declared data identity");
            destination.close().map_err(|(_, error)| error)?;
        }
        snapshot.commit()?;
        scratch.as_file_mut().rewind()?;
        let bytes = scratch.as_file().metadata()?.len();
        let chunk_count = usize::try_from(bytes.div_ceil(CHUNK_BYTES as u64))
            .context("recovery source chunk index exceeds this address space")?;
        let mut chunks = Vec::with_capacity(chunk_count);
        let mut buffer = Vec::with_capacity(CHUNK_BYTES + 1);
        loop {
            buffer.clear();
            scratch.as_file_mut().take(CHUNK_BYTES as u64).read_to_end(&mut buffer)?;
            if buffer.is_empty() { break; }
            chunks.push(self.cas.seal_object(
                &buffer, "application/octet-stream", "recovery-source", "raw", 1,
            )?.object);
        }
        let manifest = SqliteSnapshot { schema: 1, application_id, user_version, bytes, chunks };
        let artifact = self.cas.seal_object(
            &serde_json::to_vec(&manifest)?, "application/json", "recovery-source", SQLITE_CODEC, 1,
        )?.object;
        let data = serde_json::to_string(&artifact)?;
        self.ledger.conn.lock().execute(
            "INSERT INTO recovery_sources(source_id,kind,data) VALUES(?1,?2,?3)
             ON CONFLICT(source_id) DO UPDATE SET kind=excluded.kind,data=excluded.data
             WHERE recovery_sources.kind<>excluded.kind OR recovery_sources.data<>excluded.data",
            params![source_id, kind, data],
        )?;
        Ok(RecoverySource { source_id: source_id.to_owned(), kind: kind.to_owned(), artifact })
    }

    /// Includes retained foreign sources, so another node can export the full
    /// recovery closure again without access to the original machine.
    pub fn recovery_sources(&self) -> Result<Vec<RecoverySource>> {
        let connection = self.ledger.conn.lock();
        let mut query = connection.prepare("SELECT source_id,kind,data FROM recovery_sources ORDER BY source_id")?;
        let rows = query.query_map([], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
        )))?;
        rows.map(|row| {
            let (source_id, kind, data) = row?;
            Ok(RecoverySource { source_id, kind, artifact: serde_json::from_str(&data)? })
        }).collect()
    }

    /// Materialize a verified raw data archive at a new path, without activating
    /// any account/provider or altering an existing file. All original IDs,
    /// pending rows, observations and gap records remain in the database.
    pub fn restore_sqlite_recovery_source(&self, source_id: &str, output: &Path) -> Result<RecoverySource> {
        ensure!(!output.try_exists()?, "recovery output already exists; original left unchanged");
        let source = {
            let connection = self.ledger.conn.lock();
            let row: Option<(String, String)> = connection.query_row(
                "SELECT kind,data FROM recovery_sources WHERE source_id=?1", [source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let (kind, data) = row.context("unknown recovery source")?;
            RecoverySource { source_id: source_id.to_owned(), kind, artifact: serde_json::from_str(&data)? }
        };
        ensure!(source.artifact.codec.name == SQLITE_CODEC && source.artifact.codec.major == 1,
            "recovery source is not a supported SQLite chunk archive");
        let manifest: SqliteSnapshot = serde_json::from_slice(&self.read_recovery_object(&source.artifact)?)?;
        ensure!(manifest.schema == 1, "unsupported SQLite recovery archive schema");
        let parent = output.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        let mut written = 0_u64;
        for chunk in &manifest.chunks {
            written = written.checked_add(chunk.bytes).context("recovery database length overflow")?;
            ensure!(written <= manifest.bytes, "recovery database chunks exceed declared length");
            staged.write_all(&self.read_recovery_object(chunk)?)?;
        }
        ensure!(written == manifest.bytes, "incomplete recovery database chunk closure");
        staged.as_file().sync_all()?;
        {
            let connection = Connection::open_with_flags(
                staged.path(), OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            check_sqlite(&connection)?;
            ensure!(sqlite_metadata(&connection)? == (manifest.application_id, manifest.user_version),
                "restored database metadata differs from the verified snapshot");
        }
        staged.persist_noclobber(output).context("publish recovered database without overwriting existing data")?;
        fs::File::open(parent)?.sync_all()?;
        Ok(source)
    }

    fn read_recovery_object(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        let bytes = self.cas.read_object(&object.digest)?;
        ensure!(bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
            "retained recovery object does not match its declared bytes/digest");
        Ok(bytes)
    }
}
