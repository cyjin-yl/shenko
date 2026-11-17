//! Physical installation coordination shared by Store, Node and handoff.
//!
//! The stable lock file lives outside SQLite and generation copies. Holding
//! it grants no primary authority or permission to send. Callers still verify
//! their selected route and original invocation under this exclusion. Nested
//! work borrows the original permit explicitly; there is no thread-local or
//! process-local reentrancy exemption.
use std::{
    fs::{File, OpenOptions},
    io,
    path::{Path, PathBuf},
};

pub const MUTATION_LOCK_FILENAME: &str = ".shenko-installation-mutation.lock";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    file: u64,
}

#[derive(Debug, Clone)]
pub struct InstallationDomain {
    root: PathBuf,
    identity: FileIdentity,
}

/// One owned, non-cloneable installation reservation. The descriptor stays
/// owned until Drop; a physical writer receives this exact ownership value.
#[derive(Debug)]
pub struct InstallationPermit {
    domain: InstallationDomain,
    file: File,
}

impl InstallationDomain {
    pub fn open(root: &Path) -> io::Result<Self> {
        let directory = open_directory(root)?;
        let domain = Self {
            root: root.canonicalize()?,
            identity: identity(&directory)?,
        };
        domain.validate_root()?;
        Ok(domain)
    }

    /// These are the installed public paths. Other standalone databases do
    /// not have a software-slot handoff domain; they cannot become an installed
    /// writer by passing a foreign manifest or archive.
    pub fn for_store(db: &Path, cas: &Path) -> io::Result<Option<Self>> {
        if db.file_name().is_none_or(|name| name != "shenko.db")
            || cas.file_name().is_none_or(|name| name != "cas")
        {
            return Ok(None);
        }
        let Some(root) = cas.parent() else { return Ok(None) };
        // Sharing a CAS does not make arbitrary copied databases installed
        // software slots. Only the root and its published versions participate
        // in the selected-writer fence; detached migration snapshots do not.
        let root = root.canonicalize()?;
        let Some(parent) = db.parent() else { return Ok(None) };
        let parent = parent.canonicalize()?;
        if parent != root && parent.parent() != Some(root.join("versions").as_path()) {
            return Ok(None);
        }
        Self::open(&root).map(Some)
    }

    pub fn for_node(dir: &Path) -> io::Result<Option<Self>> {
        if dir.file_name().is_none_or(|name| name != "node") {
            return Ok(None);
        }
        dir.parent().map(Self::open).transpose()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn validate_root(&self) -> io::Result<()> {
        if identity(&open_directory(&self.root)?)? != self.identity {
            return Err(io::Error::other("installation root identity changed"));
        }
        Ok(())
    }

    pub fn acquire(&self) -> io::Result<InstallationPermit> {
        self.validate_root()?;
        let file = open_lock(&self.root.join(MUTATION_LOCK_FILENAME), true)?;
        validate_regular_lock(&file)?;
        // std owns the platform implementation: flock on Unix and a real
        // LockFileEx reservation on Windows. Unsupported backends return an
        // error; no platform receives an unlocked successful capability.
        loop {
            match file.lock() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        let permit = InstallationPermit {
            domain: self.clone(),
            file,
        };
        permit.validate(self)?;
        Ok(permit)
    }
}

impl InstallationPermit {
    pub fn root(&self) -> &Path {
        self.domain.root()
    }

    pub fn validate(&self, domain: &InstallationDomain) -> io::Result<()> {
        domain.validate_root()?;
        if self.domain.root != domain.root || self.domain.identity != domain.identity {
            return Err(io::Error::other(
                "permit belongs to another physical installation",
            ));
        }
        let named = open_lock(&domain.root.join(MUTATION_LOCK_FILENAME), false)?;
        validate_regular_lock(&named)?;
        if identity(&self.file)? != identity(&named)? {
            return Err(io::Error::other("installation lock file was replaced"));
        }
        Ok(())
    }
}

impl Drop for InstallationPermit {
    fn drop(&mut self) {
        // Closing the original owned descriptor is the final release even if
        // an explicit platform unlock fails. No cloned handle escapes it.
        let _ = self.file.unlock();
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other(
            "installation root must be a nonsymlink directory",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        };
        options
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_dir() || is_reparse(&file)? {
        return Err(io::Error::other("installation root was replaced"));
    }
    Ok(file)
}

fn open_lock(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(create).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if is_reparse(&file)? {
        return Err(io::Error::other("installation lock is a reparse point"));
    }
    Ok(file)
}

fn validate_regular_lock(file: &File) -> io::Result<()> {
    if !file.metadata()?.is_file() || links(file)? != 1 || is_reparse(file)? {
        return Err(io::Error::other(
            "installation lock must be one regular file with one link",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn identity(file: &File) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: metadata.ino(),
    })
}
#[cfg(unix)]
fn links(file: &File) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.nlink())
}
#[cfg(unix)]
fn is_reparse(_file: &File) -> io::Result<bool> {
    Ok(false)
}

#[cfg(windows)]
fn information(
    file: &File,
) -> io::Result<windows_sys::Win32::Storage::FileSystem::BY_HANDLE_FILE_INFORMATION> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the file owns a live Windows handle and the output points to a
    // complete writable struct. The struct is read only after API success.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.assume_init() })
}
#[cfg(windows)]
fn identity(file: &File) -> io::Result<FileIdentity> {
    let info = information(file)?;
    Ok(FileIdentity {
        volume: u64::from(info.dwVolumeSerialNumber),
        file: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    })
}
#[cfg(windows)]
fn links(file: &File) -> io::Result<u64> {
    Ok(u64::from(information(file)?.nNumberOfLinks))
}
#[cfg(windows)]
fn is_reparse(file: &File) -> io::Result<bool> {
    Ok(information(file)?.dwFileAttributes
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0)
}

#[cfg(not(any(unix, windows)))]
fn identity(_file: &File) -> io::Result<FileIdentity> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical installation identity unavailable on this platform",
    ))
}
#[cfg(not(any(unix, windows)))]
fn links(_file: &File) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical installation identity unavailable on this platform",
    ))
}
#[cfg(not(any(unix, windows)))]
fn is_reparse(_file: &File) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "physical installation identity unavailable on this platform",
    ))
}
