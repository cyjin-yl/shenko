//! Lifetime ownership of a native runtime's existing directory inode.
//!
//! This is local advisory exclusion, not an activation selector or a fence for
//! independently copied stores. It creates no lock file in the target.
use std::path::Path;

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub struct RuntimeOwner {
    directory: std::fs::File,
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
impl RuntimeOwner {
    pub fn try_acquire(path: &Path) -> anyhow::Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        let directory = std::fs::OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        // SAFETY: the directory is an open descriptor owned by this guard;
        // flock only changes the kernel's advisory lock associated with it.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let owner = Self { directory };
        owner.verify_path(path)?;
        Ok(owner)
    }

    pub fn verify_path(&self, path: &Path) -> anyhow::Result<()> {
        use std::os::unix::fs::MetadataExt;
        let opened = self.directory.metadata()?;
        let named = std::fs::metadata(path)?;
        anyhow::ensure!(opened.is_dir() && named.is_dir()
            && opened.dev() == named.dev() && opened.ino() == named.ino(),
            "runtime directory identity changed during admission");
        Ok(())
    }
}

/// Inspect an existing store without opening it as a writer or creating WAL
/// sidecars. Immutable SQLite deliberately ignores WAL, so any nonempty WAL or
/// rollback journal is indeterminate, not proof of an absent/foreign ledger.
/// Preserve it and require a WAL-aware read-only admission before starting.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub fn admit_existing_store(data_dir: &Path) -> anyhow::Result<Option<shenko_types::ids::EntityId>> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    use rusqlite::OpenFlags;

    let db = data_dir.join("shenko.db");
    let manifest = data_dir.join("home-manifest.json");
    let metadata = match std::fs::symlink_metadata(&db) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(!manifest.exists(), "DATA_PATH_CONFLICT: existing home manifest has no store; preserve it for recovery");
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(metadata.is_file() && metadata.nlink() == 1,
        "DATA_PATH_CONFLICT: store must be a non-aliased regular file");
    for suffix in ["-wal", "-journal"] {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::symlink_metadata(&name) {
            Ok(sidecar) => anyhow::ensure!(sidecar.is_file() && sidecar.len() == 0,
                "STORE_ADMISSION_INDETERMINATE: nonempty or non-regular SQLite {suffix}; preserve it and use WAL-aware read-only recovery"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
            Err(error) => return Err(error.into()),
        }
    }

    // Preserve arbitrary Unix path bytes in the SQLite URI; '?'/'#'/'%' are
    // encoded, not mistaken for query parameters. This mode never reads WAL.
    let mut uri = String::from("file:");
    for byte in db.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(*byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut uri, "%{byte:02X}")?;
        }
    }
    uri.push_str("?immutable=1");
    let connection = rusqlite::Connection::open_with_flags(uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    let required: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('schema_version','ledger_identity','commit_log','subject_genesis','subject_branch_heads','subject_steps')",
        [], |row| row.get(0),
    )?;
    anyhow::ensure!(required == 6, "DATA_PATH_CONFLICT: existing SQLite file is not a recognized Shenko ledger");
    let ledger: String = connection.query_row(
        "SELECT ledger_id FROM ledger_identity WHERE singleton=1", [], |row| row.get(0),
    )?;
    let ledger: shenko_types::ids::EntityId = ledger.parse()?;
    let rows: i64 = connection.query_row("SELECT count(*) FROM ledger_identity", [], |row| row.get(0))?;
    anyhow::ensure!(rows == 1, "DATA_PATH_CONFLICT: invalid ledger identity cardinality");
    drop(connection);

    match std::fs::symlink_metadata(&manifest) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_file(), "DATA_PATH_CONFLICT: home manifest must be a regular file");
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    Ok(Some(ledger))
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub struct RuntimeOwner;

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
impl RuntimeOwner {
    pub fn try_acquire(_path: &Path) -> anyhow::Result<Self> {
        anyhow::bail!("native directory ownership is unavailable on this target")
    }
    pub fn verify_path(&self, _path: &Path) -> anyhow::Result<()> {
        anyhow::bail!("native directory ownership is unavailable on this target")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub fn admit_existing_store(_data_dir: &Path) -> anyhow::Result<Option<shenko_types::ids::EntityId>> {
    anyhow::bail!("native directory ownership is unavailable on this target")
}

#[cfg(all(test, any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod tests {
    use super::*;

    #[test]
    fn same_directory_inode_excludes_another_owner_without_creating_a_file() {
        let dir = std::env::temp_dir().join(format!("shenko-owner-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let owner = RuntimeOwner::try_acquire(&dir).unwrap();
        assert!(RuntimeOwner::try_acquire(&dir).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        drop(owner);
        drop(RuntimeOwner::try_acquire(&dir).unwrap());
        std::fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn foreign_sqlite_named_as_store_is_not_modified() {
        let dir = std::env::temp_dir().join(format!("shenko-foreign-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let db = dir.join("shenko.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("CREATE TABLE foreign_data(value TEXT); INSERT INTO foreign_data VALUES('keep');").unwrap();
        drop(connection);
        let original = std::fs::read(&db).unwrap();
        let owner = RuntimeOwner::try_acquire(&dir).unwrap();
        assert!(admit_existing_store(&dir).is_err());
        assert_eq!(std::fs::read(&db).unwrap(), original);
        assert!(!dir.join("shenko.db-wal").exists());
        assert!(!dir.join("shenko.db-shm").exists());
        assert!(!dir.join("runtime-owner.sqlite").exists());
        drop(owner);
        // Preserve the fixture directory for inspection.
    }

    #[test]
    fn unmanifested_home_is_rejected_before_store_creation() {
        let dir = std::env::temp_dir().join(format!("shenko-foreign-home-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::create_dir(dir.join("home")).unwrap();
        let sentinel = dir.join("home/foreign");
        std::fs::write(&sentinel, b"preserve").unwrap();
        let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(), ..Default::default()
        });
        assert!(bootstrap.validate_target().is_err());
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserve");
        assert!(!dir.join("shenko.db").exists());
    }

    #[test]
    fn existing_checkpointed_store_is_recognized() {
        let dir = std::env::temp_dir().join(format!("shenko-existing-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = shenko_store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap();
        drop(store);
        let owner = RuntimeOwner::try_acquire(&dir).unwrap();
        assert!(admit_existing_store(&dir).unwrap().is_some());
        drop(owner);
    }

    #[test]
    fn nonempty_wal_is_preserved_as_indeterminate() {
        let dir = std::env::temp_dir().join(format!("shenko-wal-admission-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = shenko_store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap();
        let db = std::fs::read(dir.join("shenko.db")).unwrap();
        let wal = std::fs::read(dir.join("shenko.db-wal")).unwrap();
        assert!(!wal.is_empty());
        let error = admit_existing_store(&dir).unwrap_err().to_string();
        assert!(error.contains("STORE_ADMISSION_INDETERMINATE"));
        assert_eq!(std::fs::read(dir.join("shenko.db")).unwrap(), db);
        assert_eq!(std::fs::read(dir.join("shenko.db-wal")).unwrap(), wal);
        drop(store);
    }
}
