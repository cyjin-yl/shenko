//! Installation-local claim files. All file operations stay relative to one
//! admitted directory descriptor; existing files are never truncated/replaced.

use std::{ffi::CStr, path::Path};
use anyhow::Result;

#[cfg(unix)]
use {
    anyhow::ensure,
    std::{
        fs::{File, Metadata, OpenOptions, Permissions},
        io::{self, Read, Write},
        os::{fd::{AsRawFd, FromRawFd}, unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}},
    },
};

pub(super) struct ClaimControl {
    #[cfg(unix)]
    directory: File,
}

#[cfg(unix)]
fn check_directory(metadata: &Metadata) -> Result<()> {
    // SAFETY: geteuid has no pointer arguments and does not mutate state.
    ensure!(metadata.is_dir() && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o022 == 0,
        "owner control directory must be owned by this runtime user and not writable by others");
    Ok(())
}

#[cfg(unix)]
impl ClaimControl {
    pub(super) fn open(data_dir: &Path, create: bool) -> Result<Option<Self>> {
        let root = match OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(data_dir) {
            Ok(root) => root,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        check_directory(&root.metadata()?)?;
        if create {
            // SAFETY: root owns the live descriptor and the basename is a
            // static, NUL-terminated single component.
            if unsafe { libc::mkdirat(root.as_raw_fd(), c"bootstrap".as_ptr(), 0o700) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists { return Err(error.into()) }
            }
        }
        // SAFETY: same descriptor/basename contract as mkdirat; O_NOFOLLOW
        // rejects a substituted bootstrap symlink before any private read.
        let fd = unsafe { libc::openat(root.as_raw_fd(), c"bootstrap".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if !create && error.kind() == io::ErrorKind::NotFound { return Ok(None) }
            return Err(error.into());
        }
        // SAFETY: openat returned a fresh owned descriptor.
        let directory = unsafe { File::from_raw_fd(fd) };
        let metadata = directory.metadata()?;
        check_directory(&metadata)?;
        if create {
            // Older versions created a 0755 digest-only directory. Tighten
            // this already-admitted inode before publishing any plaintext.
            if metadata.mode() & 0o077 != 0 {
                directory.set_permissions(Permissions::from_mode(0o700))?;
            }
            directory.sync_all()?;
            root.sync_all()?;
        }
        Ok(Some(Self { directory }))
    }

    fn open_file(&self, name: &CStr, flags: i32) -> Result<Option<File>> {
        // SAFETY: only module-owned constant basenames or generated UUID
        // basenames reach this private API; the directory fd stays alive.
        let fd = unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC, 0o600) };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if flags & libc::O_CREAT == 0 && error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error.into());
        }
        // SAFETY: openat returned a fresh owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        // Admission precedes reading: no symlink, hardlink, special file, or
        // credential readable by another user can become a claim record.
        ensure!(metadata.is_file() && metadata.nlink() == 1
            && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "owner claim file must be a private, singly-linked regular file owned by this runtime user");
        Ok(Some(file))
    }

    pub(super) fn read(&self, name: &CStr) -> Result<Option<Vec<u8>>> {
        const LIMIT: u64 = 4096;
        let Some(file) = self.open_file(name, libc::O_RDONLY)? else { return Ok(None) };
        let len = file.metadata()?.len();
        ensure!(len <= LIMIT, "owner claim file exceeds its size bound");
        let mut bytes = Vec::with_capacity(len as usize);
        file.take(LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= LIMIT, "owner claim file exceeds its size bound");
        Ok(Some(bytes))
    }

    pub(super) fn write_new(&self, name: &CStr, bytes: &[u8]) -> Result<()> {
        let temporary = std::ffi::CString::new(format!(".claim-{}", shenko_types::ids::EntityId::new()))?;
        let mut file = self.open_file(&temporary, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?
            .expect("O_CREAT creates the private staging file or returns an error");
        let published = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.publish(&temporary, name)
        })();
        // Always retire this attempt's staging name; preserve any primary
        // write/publication error over a secondary cleanup failure.
        let retired = self.unlink(&temporary);
        published?;
        retired?;
        self.directory.sync_all()?;
        Ok(())
    }

    fn publish(&self, temporary: &CStr, name: &CStr) -> Result<()> {
        // Atomic no-replace rename avoids a crash window with two links to
        // a published credential, which singly-linked admission must reject.
        // SAFETY: both basenames belong to this module; directory owns the
        // live fd and all pointer arguments remain valid during the call.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let result = unsafe { libc::syscall(libc::SYS_renameat2,
            self.directory.as_raw_fd(), temporary.as_ptr(),
            self.directory.as_raw_fd(), name.as_ptr(), libc::RENAME_NOREPLACE) };
        #[cfg(target_vendor = "apple")]
        let result = unsafe { libc::renameatx_np(self.directory.as_raw_fd(), temporary.as_ptr(),
            self.directory.as_raw_fd(), name.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        {
            if result != 0 { return Err(io::Error::last_os_error().into()) }
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
        anyhow::bail!("atomic native claim publication is unavailable on this target")
    }

    fn unlink(&self, name: &CStr) -> Result<()> {
        // SAFETY: name is a module-owned basename and directory remains live.
        if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::NotFound { return Err(error.into()) }
        }
        Ok(())
    }

    pub(super) fn remove(&self, name: &CStr) -> Result<()> {
        if self.open_file(name, libc::O_RDONLY)?.is_some() {
            self.unlink(name)?;
            self.directory.sync_all()?;
        }
        Ok(())
    }
}

// The native runtime already requires Unix directory ownership. Do not turn
// an unsupported platform into a plaintext file with unverified protection.
#[cfg(not(unix))]
impl ClaimControl {
    pub(super) fn open(_: &Path, _: bool) -> Result<Option<Self>> {
        anyhow::bail!("protected native installation control is unavailable on this target")
    }
    pub(super) fn read(&self, _: &CStr) -> Result<Option<Vec<u8>>> {
        anyhow::bail!("protected native installation control is unavailable on this target")
    }
    pub(super) fn write_new(&self, _: &CStr, _: &[u8]) -> Result<()> {
        anyhow::bail!("protected native installation control is unavailable on this target")
    }
    pub(super) fn remove(&self, _: &CStr) -> Result<()> {
        anyhow::bail!("protected native installation control is unavailable on this target")
    }
}
