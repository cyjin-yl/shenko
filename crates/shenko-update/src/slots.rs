//! A/B slots: self-contained generation directories — SPEC §9.5, §35.7.
//!
//! A slot is a complete installation copy: the ledger database, the content
//! store, the managed home and the memory repository. The live generation is
//! the one named by the route authority that the activation selector points
//! at; the previous slot is retained so a rollback has a runnable old closure.
//!
//! The copy uses the SQLite backup API: copying the live database file of a
//! running WAL database is not a consistent transaction snapshot.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, DatabaseName};
use serde::{Deserialize, Serialize};

use shenko_types::ids::RuntimeGeneration;
use shenko_types::object::Digest;

/// The ledger database file inside a slot.
pub const SLOT_DB_FILENAME: &str = "shenko.db";

/// The slot descriptor inside a slot directory.
pub const SLOT_MANIFEST_FILENAME: &str = "slot.json";

/// The directory that names the live slot of an installation.
pub const CURRENT_SLOT_FILENAME: &str = "current-slot";

/// Where the generations of one installation live.
#[derive(Debug, Clone)]
pub struct SlotLayout {
    /// The installation's data directory.
    pub data_dir: PathBuf,
}

impl SlotLayout {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// The directory of one generation.
    pub fn slot_dir(&self, generation: &RuntimeGeneration) -> PathBuf {
        self.data_dir
            .join("versions")
            .join(sanitize_generation(generation))
    }

    /// The pointer file that names the live slot.
    pub fn current_slot_file(&self) -> PathBuf {
        self.data_dir.join(CURRENT_SLOT_FILENAME)
    }
}

/// One generation's directory: program artifact plus its own state copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotManifest {
    pub schema_version: u32,
    pub generation: String,
    /// The shared content store of the installation. Every generation of
    /// one installation reads and writes the same CAS; it is never copied
    /// with a slot.
    pub cas_dir: String,
    /// The candidate program of this generation, if it was built.
    pub artifact_path: Option<String>,
    pub artifact_digest: Option<Digest>,
    /// The ledger watermark this slot was copied at.
    pub copied_through_seq: Option<u64>,
    pub created_at: String,
}

impl SlotManifest {
    pub const SCHEMA_VERSION: u32 = 1;
}

/// One A/B slot: a runtime generation with its artifact and its state copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbSlot {
    pub generation: RuntimeGeneration,
    /// The slot directory. It holds the database, the content store and the
    /// managed workspaces of this generation.
    pub slot_dir: PathBuf,
    pub db_path: PathBuf,
    /// The shared content store of the installation.
    pub cas_dir: PathBuf,
    /// The candidate program of this generation, if it was built.
    pub artifact_path: Option<PathBuf>,
    pub artifact_digest: Option<Digest>,
    /// True for the slot that currently holds the control authority.
    pub is_active: bool,
}

impl AbSlot {
    pub fn manifest(&self) -> SlotManifest {
        SlotManifest {
            schema_version: SlotManifest::SCHEMA_VERSION,
            generation: self.generation.0.clone(),
            cas_dir: self.cas_dir.to_string_lossy().to_string(),
            artifact_path: self
                .artifact_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            artifact_digest: self.artifact_digest.clone(),
            copied_through_seq: None,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        }
    }
}

/// Errors of the slot operations.
#[derive(Debug, thiserror::Error)]
pub enum SlotError {
    #[error("slot {0} does not exist")]
    MissingSlot(String),
    #[error("slot directory {0} already exists")]
    SlotExists(String),
    #[error("could not create staging path {0}: {1}")]
    Staging(PathBuf, #[source] std::io::Error),
    #[error("could not prepare staging directory {0}: {1}")]
    Preparation(PathBuf, #[source] Box<SlotError>),
    #[error(
        "publication from {staged} to {published} returned an error: {source}; the outcome may be uncertain, inspect both paths before recovery"
    )]
    Publication {
        staged: PathBuf,
        published: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = SlotError> = std::result::Result<T, E>;

/// Copy a slot into a new generation directory.
///
/// The tree is copied first while the live SQLite files are skipped, then the
/// database is taken with the SQLite backup API so the copy is a consistent
/// transaction snapshot. The finished directory is published with an
/// exclusive no-replace rename, so an existing generation directory is never
/// overwritten and a failed publication is left for inspection instead of
/// being retried blindly.
pub fn copy_slot(source: &AbSlot, target: &SlotLayout, generation: &RuntimeGeneration) -> Result<AbSlot> {
    if !source.slot_dir.is_dir() {
        return Err(SlotError::MissingSlot(source.slot_dir.display().to_string()));
    }
    if !source.db_path.is_file() {
        return Err(SlotError::MissingSlot(source.db_path.display().to_string()));
    }
    let slot_dir = target.slot_dir(generation);
    if slot_dir.exists() {
        return Err(SlotError::SlotExists(slot_dir.display().to_string()));
    }
    let staged = target
        .data_dir
        .join("versions")
        .join(format!(".shenko-stage-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(staged.parent().unwrap_or(&target.data_dir)).map_err(|source| {
        SlotError::Staging(staged.clone(), source)
    })?;
    fs::create_dir(&staged).map_err(|source| SlotError::Staging(staged.clone(), source))?;

    let result = (|| -> Result<Option<u64>> {
        let source_dir = source.slot_dir.canonicalize()?;
        let db_canon = source.db_path.canonicalize()?;
        let installation_root = target.data_dir.canonicalize()?;
        copy_dir_tree(&source_dir, &staged, &db_canon, &installation_root)?;
        backup_database(&source.db_path, &staged.join(SLOT_DB_FILENAME))?;
        let copied_through_seq = crate::migration::watermark_of(&staged.join(SLOT_DB_FILENAME))
            .map_err(|error| SlotError::MissingSlot(format!("backup watermark unreadable: {error}")))?
            .map(|commit| commit.seq.0);
        // Copies preserve the subject/genesis/installation identity, but
        // independent local commits cannot share (ledger_id, sequence).
        // Historical source CommitRefs remain unchanged in copied rows.
        let copy = Connection::open(staged.join(SLOT_DB_FILENAME))?;
        copy.execute("UPDATE ledger_identity SET ledger_id=?1 WHERE singleton=1",
            [uuid::Uuid::new_v4().to_string()])?;
        copy.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
        drop(copy);
        fs::File::open(staged.join(SLOT_DB_FILENAME))?.sync_all()?;
        fs::File::open(&staged)?.sync_all()?;
        Ok(copied_through_seq)
    })();

    let copied_through_seq = result.map_err(|source| SlotError::Preparation(staged.clone(), Box::new(source)))?;

    // Never retry or remove either path after an error: a rename can succeed
    // on the server while the client sees an error.
    rename_no_replace(&staged, &slot_dir).map_err(|source| SlotError::Publication {
        staged: staged.clone(),
        published: slot_dir.clone(),
        source,
    })?;
    fs::File::open(slot_dir.parent().unwrap_or(&target.data_dir))?.sync_all()?;

    let db_path = slot_dir.join(SLOT_DB_FILENAME);
    let manifest = SlotManifest {
        schema_version: SlotManifest::SCHEMA_VERSION,
        generation: generation.0.clone(),
        cas_dir: target
            .data_dir
            .join("cas")
            .to_string_lossy()
            .to_string(),
        artifact_path: None,
        artifact_digest: None,
        copied_through_seq,
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    write_slot_manifest(&slot_dir, &manifest)?;

    Ok(AbSlot {
        generation: generation.clone(),
        slot_dir,
        db_path,
        cas_dir: target.data_dir.join("cas"),
        artifact_path: None,
        artifact_digest: None,
        is_active: false,
    })
}

/// Read the descriptor of a slot.
pub fn read_slot_manifest(slot_dir: &Path) -> Result<SlotManifest> {
    let bytes = fs::read(slot_dir.join(SLOT_MANIFEST_FILENAME))?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Write the descriptor of a slot.
pub fn write_slot_manifest(slot_dir: &Path, manifest: &SlotManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    durable_replace(slot_dir, SLOT_MANIFEST_FILENAME, &bytes)?;
    Ok(())
}

/// The slot that currently holds the control authority of an installation.
///
/// The pointer file is a convenience for the launcher; the authority itself
/// is the activation selector inside the slot's database. A pointer that
/// disagrees with the selector is reported, not silently trusted.
pub fn resolve_live_slot(data_dir: &Path) -> Result<Option<PathBuf>> {
    let pointer = data_dir.join(CURRENT_SLOT_FILENAME);
    let bytes = match fs::read(&pointer) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let value: SlotPointer = serde_json::from_slice(&bytes)?;
    if value.schema_version != 1 { return Err(SlotError::MissingSlot("unsupported launcher pointer schema".into())); }
    let path = validate_slot_path(data_dir, &value.slot_dir)?;
    Ok(Some(path))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotPointer { schema_version: u32, slot_dir: PathBuf }

fn validate_slot_path(data_dir: &Path, slot_dir: &Path) -> Result<PathBuf> {
    let root = data_dir.canonicalize()?;
    let slot = slot_dir.canonicalize()?;
    if slot != root && slot.parent() != Some(root.join("versions").as_path()) {
        return Err(SlotError::MissingSlot("selected slot is outside this installation's declared slot roots".into()));
    }
    if !slot.join(SLOT_DB_FILENAME).is_file() {
        return Err(SlotError::MissingSlot(slot.display().to_string()));
    }
    Ok(slot)
}

fn durable_replace(directory: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = directory.join(format!(".{name}.stage-{}", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, directory.join(name))?;
    fs::File::open(directory)?.sync_all()?;
    Ok(())
}

/// Point the launcher at a slot. This is a convenience pointer, published
/// after the selector CAS; it never carries authority by itself.
pub fn publish_current_slot(data_dir: &Path, slot_dir: &Path) -> Result<()> {
    let pointer = SlotPointer { schema_version: 1, slot_dir: validate_slot_path(data_dir, slot_dir)? };
    durable_replace(data_dir, CURRENT_SLOT_FILENAME, &serde_json::to_vec(&pointer)?)?;
    Ok(())
}

/// Copy the source database with the SQLite backup API into a new file.
pub fn backup_database(src_db: &Path, dst_db: &Path) -> Result<()> {
    let src = Connection::open(src_db)?;
    // A source artifact with this name must not be replaced by the backup.
    let new_db = fs::OpenOptions::new().write(true).create_new(true).open(dst_db)?;
    drop(new_db);
    src.backup(DatabaseName::Main, dst_db, None)?;
    Ok(())
}

/// Open a slot's database for reading.
pub fn open_slot_db(slot: &AbSlot) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        &slot.db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn copy_dir_tree(src: &Path, dst: &Path, skip_db: &Path, installation_root: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        if src == installation_root && matches!(entry.file_name().to_str(),
            Some("versions" | "cas" | "node" | "current-slot" | "update-control.json")) {
            continue;
        }
        if is_sqlite_file(&src_path, skip_db) {
            continue;
        }
        let dst_path = dst.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_dir_tree(&src_path, &dst_path, skip_db, installation_root)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            {
                let target = fs::read_link(&src_path)?;
                std::os::unix::fs::symlink(target, &dst_path)?;
            }
            #[cfg(not(unix))]
            {
                fs::copy(&src_path, &dst_path)?;
            }
        } else {
            fs::copy(&src_path, &dst_path)?;
            fs::File::open(&dst_path)?.sync_all()?;
        }
    }
    fs::File::open(dst)?.sync_all()?;
    Ok(())
}

fn is_sqlite_file(path: &Path, db_path: &Path) -> bool {
    if path == db_path {
        return true;
    }
    ["-wal", "-shm", "-journal"]
        .iter()
        .any(|suffix| path == sidecar_path(db_path, suffix))
}

fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os = db_path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// A filesystem-safe name for a generation.
pub fn sanitize_generation(generation: &RuntimeGeneration) -> String {
    let sanitized: String = generation
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{}-{}", sanitized.trim_matches('.').chars().take(48).collect::<String>(),
        Digest::blake3(generation.0.as_bytes()).to_string().replace(':', "-"))
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileW;

    let mut from: Vec<u16> = from.as_os_str().encode_wide().collect();
    let mut to: Vec<u16> = to.as_os_str().encode_wide().collect();
    if from.contains(&0) || to.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    from.push(0);
    to.push(0);
    // SAFETY: both pointers refer to NUL-terminated strings alive for this
    // call; MoveFileW does not replace an existing destination.
    if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos"
)))]
fn rename_no_replace(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive directory publication is not supported on this platform",
    ))
}
