//! Admit recovery without letting an unfamiliar database mutate its source.
//!
//! Common admission stays immutable. A genuine interrupted rollback migration
//! needs SQLite recovery first, so the exceptional path recovers a private copy.
//! Its source is locked by SQLite's own VFS, not by unrelated flock locks. The
//! staged VFS cannot follow a super-journal back out into the original tree.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, ffi};
use std::{
    cell::UnsafeCell,
    ffi::{CStr, CString, c_char, c_int, c_void},
    fs::{File, OpenOptions},
    io::Write,
    os::{fd::AsRawFd, unix::{ffi::OsStrExt, fs::{MetadataExt, OpenOptionsExt}}},
    path::{Path, PathBuf},
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

const LOCK_WAIT: Duration = Duration::from_secs(5);

pub(super) struct InitializationLock {
    _directory: File,
}

pub(super) fn lock_initialization(dir: &Path) -> Result<InitializationLock> {
    let directory = OpenOptions::new().read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(dir)?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        // The file remains alive until Node::open returns. No lock file is
        // created in an installation whose ownership is not established yet.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(InitializationLock { _directory: directory });
        }
        let error = std::io::Error::last_os_error();
        if !matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) {
            return Err(error.into());
        }
        ensure!(Instant::now() < deadline, "node initialization is busy");
        thread::sleep(Duration::from_millis(10));
    }
}

pub(super) fn validate_existing(db: &Path) -> Result<()> {
    let main = std::fs::symlink_metadata(db)?;
    ensure!(main.is_file() && main.nlink() == 1 && !main.file_type().is_symlink(),
        "node main must be one private regular file with one link");
    let journal = sidecar(db, "-journal");
    if sidecar_present(&journal)? {
        let meta = std::fs::symlink_metadata(&journal)?;
        ensure!(meta.is_file() && meta.nlink() == 1 && !meta.file_type().is_symlink(),
            "node journal must be one private regular file with one link");
    }
    // Any sidecar requires the confined staged preflight even when the main
    // image happens to validate: ordinary rollback could otherwise follow a
    // crafted super-journal name outside this directory.
    if sidecar_present(&journal)? || sidecar_present(&sidecar(db, "-wal"))? {
        return recover_private(db)
            .with_context(|| "node sidecar admission failed");
    }
    let immutable = (|| -> Result<()> {
        let mut connection = Connection::open_with_flags(
            super::readonly_uri(db)?,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let tx = connection.transaction()?;
        super::validate_installation(&tx)?;
        tx.commit()?;
        Ok(())
    })();
    let Err(original_error) = immutable else { return Ok(()) };
    // A pre-existing writer can commit between failed immutable admission and
    // this fallback, including deleting its DELETE journal. Sample presence
    // only after acquiring the same SQLite file exclusion that stops writers.
    recover_private(db)
        .with_context(|| format!("immutable node admission failed ({original_error}); private recovery rejected"))
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(suffix);
    name.into()
}

fn c_path(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

fn sqlite_ok(code: c_int, operation: &str) -> Result<()> {
    ensure!(code == ffi::SQLITE_OK, "SQLite VFS {operation} failed ({code})");
    Ok(())
}

/// A native SQLite file, with no pager and therefore no recovery. MAIN_DB
/// tracking is intentional even for journals: closing a raw Rust descriptor
/// could cancel locks on an unrelated database hardlinked to that file.
struct SqliteSource {
    file: NonNull<ffi::sqlite3_file>,
    filename: ffi::sqlite3_filename,
    vfs: *mut ffi::sqlite3_vfs,
    path: PathBuf,
}

impl SqliteSource {
    fn open(db: &Path, writable: bool) -> Result<Self> {
        let db = std::path::absolute(db)?;
        unsafe {
            sqlite_ok(ffi::sqlite3_initialize(), "initialize")?;
            let vfs = ffi::sqlite3_vfs_find(ptr::null());
            ensure!(!vfs.is_null() && (*vfs).szOsFile > 0, "SQLite default VFS unavailable");
            ensure!(!(*vfs).zName.is_null() && CStr::from_ptr((*vfs).zName).to_bytes() == b"unix",
                "rollback admission requires the bundled Unix locking VFS");
            let bytes = usize::try_from((*vfs).szOsFile)?;
            let allocation = ffi::sqlite3_malloc64(bytes as u64).cast::<ffi::sqlite3_file>();
            let file = NonNull::new(allocation).context("SQLite VFS file allocation failed")?;
            ptr::write_bytes(allocation.cast::<u8>(), 0, bytes);
            let mut owned = Self { file, filename: ptr::null(), vfs, path: db };
            let main = c_path(&owned.path)?;
            let journal = c_path(&sidecar(&owned.path, "-journal"))?;
            let wal = c_path(&sidecar(&owned.path, "-wal"))?;
            // VFS filenames carry SQLite metadata beyond the first NUL. A lone
            // CString is not the public xOpen filename contract.
            owned.filename = ffi::sqlite3_create_filename(main.as_ptr(), journal.as_ptr(), wal.as_ptr(), 0, ptr::null_mut());
            ensure!(!owned.filename.is_null(), "SQLite VFS filename allocation failed");
            let open = (*vfs).xOpen.context("SQLite VFS xOpen unavailable")?;
            let mut actual_flags = 0;
            let mode = if writable { ffi::SQLITE_OPEN_READWRITE } else { ffi::SQLITE_OPEN_READONLY };
            sqlite_ok(open(vfs, owned.filename, allocation,
                mode | ffi::SQLITE_OPEN_MAIN_DB | ffi::SQLITE_OPEN_NOFOLLOW,
                &mut actual_flags), "open existing source")?;
            ensure!(actual_flags & mode != 0, "SQLite source did not grant its required open mode");
            owned.check_path()?;
            Ok(owned)
        }
    }

    fn lock_exclusive(&self) -> Result<()> {
        let lock = self.methods()?.xLock.context("SQLite VFS xLock unavailable")?;
        let unlock = self.methods()?.xUnlock.context("SQLite VFS xUnlock unavailable")?;
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            let shared = unsafe { lock(self.file.as_ptr(), ffi::SQLITE_LOCK_SHARED) };
            let code = if shared == ffi::SQLITE_OK {
                unsafe { lock(self.file.as_ptr(), ffi::SQLITE_LOCK_EXCLUSIVE) }
            } else { shared };
            if code == ffi::SQLITE_OK { return self.check_path(); }
            // A RESERVED writer needs us to release SHARED before it can
            // commit. Never wait for that writer while blocking its progress.
            let released = unsafe { unlock(self.file.as_ptr(), ffi::SQLITE_LOCK_NONE) };
            if code != ffi::SQLITE_BUSY { sqlite_ok(code, "lock source")?; }
            sqlite_ok(released, "release contended source lock")?;
            ensure!(Instant::now() < deadline, "node recovery source is busy");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn methods(&self) -> Result<&ffi::sqlite3_io_methods> {
        unsafe { (*self.file.as_ptr()).pMethods.as_ref().context("SQLite VFS file methods unavailable") }
    }

    fn check_path(&self) -> Result<()> {
        let control = self.methods()?.xFileControl.context("SQLite VFS xFileControl unavailable")?;
        let mut moved: c_int = 0;
        sqlite_ok(unsafe { control(self.file.as_ptr(), ffi::SQLITE_FCNTL_HAS_MOVED,
            (&mut moved as *mut c_int).cast()) }, "check source identity")?;
        ensure!(moved == 0, "node recovery source path changed");
        Ok(())
    }

    fn copy_to(&self, output: &mut impl Write) -> Result<blake3::Hash> {
        self.check_path()?;
        let methods = self.methods()?;
        let size = methods.xFileSize.context("SQLite VFS xFileSize unavailable")?;
        let read = methods.xRead.context("SQLite VFS xRead unavailable")?;
        let mut length: i64 = 0;
        sqlite_ok(unsafe { size(self.file.as_ptr(), &mut length) }, "size source")?;
        let physical = std::fs::symlink_metadata(&self.path)?;
        // unixFileSize maps a physical one-byte file to zero. That must not
        // turn an incomplete foreign file into an exact empty-file receipt.
        ensure!(physical.is_file() && length >= 0 && physical.len() == length as u64,
            "SQLite source size does not describe its physical bytes");
        let mut offset: i64 = 0;
        let mut buffer = [0u8; 65536];
        let mut digest = blake3::Hasher::new();
        while offset < length {
            let amount = usize::try_from((length - offset).min(buffer.len() as i64))?;
            sqlite_ok(unsafe { read(self.file.as_ptr(), buffer.as_mut_ptr().cast(), amount as c_int, offset) }, "read source")?;
            digest.update(&buffer[..amount]);
            output.write_all(&buffer[..amount])?;
            offset += amount as i64;
        }
        let mut after: i64 = 0;
        sqlite_ok(unsafe { size(self.file.as_ptr(), &mut after) }, "recheck source size")?;
        ensure!(after == length && std::fs::symlink_metadata(&self.path)?.len() == length as u64,
            "node recovery source changed while captured");
        self.check_path()?;
        Ok(digest.finalize())
    }
}

impl Drop for SqliteSource {
    fn drop(&mut self) {
        unsafe {
            if let Some(methods) = (*self.file.as_ptr()).pMethods.as_ref() {
                if let Some(unlock) = methods.xUnlock { let _ = unlock(self.file.as_ptr(), ffi::SQLITE_LOCK_NONE); }
                if let Some(close) = methods.xClose { let _ = close(self.file.as_ptr()); }
            }
            if !self.filename.is_null() { ffi::sqlite3_free_filename(self.filename); }
            ffi::sqlite3_free(self.file.as_ptr().cast());
        }
    }
}

fn journal_to(journal: &Path, db: &Path, output: &mut impl Write) -> Result<blake3::Hash> {
    let main = std::fs::symlink_metadata(db)?;
    let meta = std::fs::symlink_metadata(journal)?;
    ensure!(meta.is_file() && meta.nlink() == 1 && !meta.file_type().is_symlink()
        && (meta.dev(), meta.ino()) != (main.dev(), main.ino()),
        "recovery journal must be a separate single-link regular file");
    let input = SqliteSource::open(journal, false)?;
    input.copy_to(output)
}

fn private_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?)
}

fn sidecar_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn recover_private(db: &Path) -> Result<()> {
    let source = SqliteSource::open(db, true)?;
    source.lock_exclusive()?;
    let journal = sidecar(db, "-journal");
    // Node writes DELETE journals. Do not silently discard WAL state while
    // deciding whether an otherwise unrecognized source belongs to Node.
    ensure!(!sidecar_present(&sidecar(db, "-wal"))? && !sidecar_present(&sidecar(db, "-shm"))?,
        "rollback admission does not ignore WAL sidecars");
    let stage = tempfile::Builder::new().prefix("shenko-node-recovery-").tempdir()?;
    let copied = stage.path().canonicalize()?.join("node.sqlite3");
    let main_digest = source.copy_to(&mut private_file(&copied)?)?;
    let journal_digest = if sidecar_present(&journal)? {
        Some(journal_to(&journal, db, &mut private_file(&sidecar(&copied, "-journal"))?)?)
    } else { None };
    let restricted = RecoveryVfs::register(source.vfs, &copied)?;
    let validation = (|| -> Result<()> {
        let mut connection = Connection::open_with_flags_and_vfs(&copied,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            restricted.name.to_str()?)?;
        let tx = connection.transaction()?;
        super::validate_installation(&tx)?;
        tx.commit()?;
        connection.close().map_err(|(_, error)| error)?;
        Ok(())
    })();
    ensure!(!restricted.denied.load(Ordering::Relaxed),
        "private recovery requested outside-stage, WAL or extension IO");
    validation?;
    let current_journal = if sidecar_present(&journal)? {
        Some(journal_to(&journal, db, &mut std::io::sink())?)
    } else { None };
    ensure!(source.copy_to(&mut std::io::sink())? == main_digest
        && current_journal == journal_digest
        && !sidecar_present(&sidecar(db, "-wal"))? && !sidecar_present(&sidecar(db, "-shm"))?,
        "node recovery source changed after private validation");
    // Drop the independent VFS lock before the ordinary pager opens the source.
    // The directory guard still serializes cooperating openers. This is not an
    // atomic transfer against hostile replacement inside the private root.
    Ok(())
}

/// Only pager file requests for these exact staged names may reach the OS.
/// A denied master/super-journal lookup is sticky: SQLite must not interpret an
/// outside path as merely absent and thereby produce an admitted false view.
#[repr(C)]
struct RecoveryVfs {
    // The SQLite registry can update pNext while another VFS is registered.
    raw: UnsafeCell<ffi::sqlite3_vfs>,
    base: *mut ffi::sqlite3_vfs,
    allowed: [CString; 4],
    denied: AtomicBool,
    name: CString,
}

impl RecoveryVfs {
    fn register(base: *mut ffi::sqlite3_vfs, db: &Path) -> Result<Box<Self>> {
        let name = CString::new(format!("shenko-recovery-{}", shenko_types::ids::EntityId::new()))?;
        // SQLite's registry mutates pNext under its own mutex. Read only the
        // built-in Unix descriptor's immutable fields, never its whole header.
        let raw = unsafe { ffi::sqlite3_vfs {
            iVersion: (*base).iVersion,
            szOsFile: (*base).szOsFile,
            mxPathname: (*base).mxPathname,
            pNext: ptr::null_mut(),
            zName: name.as_ptr(),
            pAppData: (*base).pAppData,
            xOpen: Some(stage_open),
            xDelete: Some(stage_delete),
            xAccess: Some(stage_access),
            xFullPathname: Some(stage_full_path),
            xDlOpen: Some(stage_no_extension),
            xDlError: (*base).xDlError,
            xDlSym: (*base).xDlSym,
            xDlClose: (*base).xDlClose,
            xRandomness: (*base).xRandomness,
            xSleep: (*base).xSleep,
            xCurrentTime: (*base).xCurrentTime,
            xGetLastError: (*base).xGetLastError,
            xCurrentTimeInt64: (*base).xCurrentTimeInt64,
            xSetSystemCall: (*base).xSetSystemCall,
            xGetSystemCall: (*base).xGetSystemCall,
            xNextSystemCall: (*base).xNextSystemCall,
        } };
        let owned = Box::new(Self {
            // Keep default pAppData for unchanged callbacks; changed callbacks
            // explicitly delegate using the original descriptor pointer.
            raw: UnsafeCell::new(raw), base,
            allowed: [c_path(db)?, c_path(&sidecar(db, "-journal"))?,
                c_path(&sidecar(db, "-wal"))?, c_path(&sidecar(db, "-shm"))?],
            denied: AtomicBool::new(false), name,
        });
        sqlite_ok(unsafe { ffi::sqlite3_vfs_register(owned.raw.get(), 0) }, "register private stage")?;
        Ok(owned)
    }

    unsafe fn permits(&self, path: *const c_char) -> bool {
        let permitted = !path.is_null() && self.allowed.iter()
            .any(|allowed| allowed.as_bytes() == unsafe { CStr::from_ptr(path) }.to_bytes());
        if !permitted { self.denied.store(true, Ordering::Relaxed); }
        permitted
    }
}

impl Drop for RecoveryVfs {
    fn drop(&mut self) {
        // Every staged Connection is scoped inside recover_private and closes
        // before this registration and its callback state are dropped.
        unsafe { ffi::sqlite3_vfs_unregister(self.raw.get()); }
    }
}

unsafe extern "C" fn stage_open(vfs: *mut ffi::sqlite3_vfs, name: ffi::sqlite3_filename,
    file: *mut ffi::sqlite3_file, flags: c_int, actual: *mut c_int) -> c_int {
    unsafe {
        let state = &*vfs.cast::<RecoveryVfs>();
        // Unix xShmMap/xShmUnmap bypass VFS namespace callbacks. This is a
        // DELETE-journal recovery path: stop before opening any WAL, which
        // precedes shared-memory IO in the bundled pager.
        let wal = flags & ffi::SQLITE_OPEN_WAL != 0;
        if wal { state.denied.store(true, Ordering::Relaxed); }
        if wal || !state.permits(name) {
            if !file.is_null() { (*file).pMethods = ptr::null(); }
            return ffi::SQLITE_CANTOPEN;
        }
        match (*state.base).xOpen {
            Some(open) => open(state.base, name, file, flags, actual),
            None => ffi::SQLITE_CANTOPEN,
        }
    }
}

unsafe extern "C" fn stage_delete(vfs: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    unsafe {
        let state = &*vfs.cast::<RecoveryVfs>();
        if !state.permits(name) { return ffi::SQLITE_IOERR; }
        match (*state.base).xDelete {
            Some(delete) => delete(state.base, name, sync),
            None => ffi::SQLITE_IOERR,
        }
    }
}

unsafe extern "C" fn stage_access(vfs: *mut ffi::sqlite3_vfs, name: *const c_char,
    flags: c_int, result: *mut c_int) -> c_int {
    unsafe {
        let state = &*vfs.cast::<RecoveryVfs>();
        if !state.permits(name) {
            if !result.is_null() { *result = 0; }
            return ffi::SQLITE_IOERR;
        }
        match (*state.base).xAccess {
            Some(access) => access(state.base, name, flags, result),
            None => ffi::SQLITE_IOERR,
        }
    }
}

unsafe extern "C" fn stage_full_path(vfs: *mut ffi::sqlite3_vfs, name: *const c_char,
    capacity: c_int, output: *mut c_char) -> c_int {
    unsafe {
        let state = &*vfs.cast::<RecoveryVfs>();
        if !state.permits(name) {
            if capacity > 0 && !output.is_null() { *output = 0; }
            return ffi::SQLITE_CANTOPEN;
        }
        let Some(full_path) = (*state.base).xFullPathname else { return ffi::SQLITE_CANTOPEN; };
        let code = full_path(state.base, name, capacity, output);
        if code == ffi::SQLITE_OK && !state.permits(output) { return ffi::SQLITE_CANTOPEN; }
        code
    }
}

unsafe extern "C" fn stage_no_extension(vfs: *mut ffi::sqlite3_vfs, _name: *const c_char) -> *mut c_void {
    unsafe { (*vfs.cast::<RecoveryVfs>()).denied.store(true, Ordering::Relaxed); }
    ptr::null_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_sqlite_recovery_cannot_mutate_an_outside_database() -> Result<()> {
        let stage = tempfile::tempdir()?;
        let db = stage.path().canonicalize()?.join("node.sqlite3");
        let connection = Connection::open(&db)?;
        connection.execute_batch("CREATE TABLE anchor(value TEXT);")?;
        connection.close().map_err(|(_, error)| error)?;
        let unrelated = tempfile::tempdir()?;
        let outside = unrelated.path().join("unrelated.sqlite");
        let other = Connection::open(&outside)?;
        other.execute_batch("CREATE TABLE marker(value TEXT); INSERT INTO marker VALUES('keep');")?;
        other.close().map_err(|(_, error)| error)?;
        let before = std::fs::read(&outside)?;
        let base = unsafe { ffi::sqlite3_vfs_find(ptr::null()) };
        ensure!(!base.is_null(), "SQLite default VFS unavailable");
        let restricted = RecoveryVfs::register(base, &db)?;
        let connection = Connection::open_with_flags_and_vfs(&db,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            restricted.name.to_str()?)?;
        let attempted = connection.execute("ATTACH DATABASE CAST(?1 AS TEXT) AS external",
                [outside.as_os_str().as_bytes()])
            .and_then(|_| connection.execute("UPDATE external.marker SET value='changed'", []));
        assert!(attempted.is_err());
        connection.close().map_err(|(_, error)| error)?;
        assert!(restricted.denied.load(Ordering::Relaxed));
        assert_eq!(std::fs::read(&outside)?, before);
        assert!(!sidecar(&outside, "-journal").exists());
        Ok(())
    }
}
