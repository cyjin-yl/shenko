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
    /// Adopt the helper's SAME open-file-description lock across exec. The
    /// directory identity and lock are verified before any Store is opened.
    pub fn adopt_inherited(path: &Path, fd: std::os::fd::RawFd) -> anyhow::Result<Self> {
        use std::os::fd::{FromRawFd, AsRawFd};
        anyhow::ensure!(fd > 2, "invalid inherited runtime-owner descriptor");
        let directory = unsafe { std::fs::File::from_raw_fd(fd) };
        let owner = Self { directory };
        owner.verify_path(path)?;
        if unsafe { libc::flock(owner.directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        anyhow::ensure!(flags >= 0, "inherited owner descriptor unavailable");
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(owner)
    }

    /// Only the selected successor exec inherits this descriptor. All
    /// ordinary helper/provider spawns see the original CLOEXEC guard.
    pub fn prepare_exec_transfer(&self) -> anyhow::Result<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        let fd = self.directory.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        anyhow::ensure!(flags >= 0, "runtime owner descriptor unavailable");
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(fd)
    }

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

#[cfg(all(test, any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
mod tests {
    use super::*;

    #[test]
    fn same_directory_inode_excludes_another_owner_without_creating_a_file() -> anyhow::Result<()> {
        let dir = std::env::temp_dir().join(format!("shenko-owner-{}", shenko_types::ids::EntityId::new()));
        std::fs::create_dir(&dir)?;
        let owner = RuntimeOwner::try_acquire(&dir)?;
        assert!(RuntimeOwner::try_acquire(&dir).is_err());
        assert_eq!(std::fs::read_dir(&dir)?.count(), 0);
        drop(owner);
        drop(RuntimeOwner::try_acquire(&dir)?);

        // The two-second recovery window may expire before a real blocking
        // worker. Both explicit host finish and an error's RAII path retain
        // the directory owner until that same worker actually ends.
        for explicit_finish in [true, false] {
            let (entered, started) = std::sync::mpsc::channel();
            let (release, waiting) = std::sync::mpsc::channel();
            let (retired, retained) = std::sync::mpsc::channel();
            let ended = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_ended = ended.clone();
            let path = dir.clone();
            let joining = std::thread::spawn(move || -> anyhow::Result<()> {
                let _owner = RuntimeOwner::try_acquire(&path)?;
                let mut host = crate::runtime_host::RuntimeHost::new(false)?;
                let runtime = tokio::runtime::Runtime::new()?;
                drop(runtime.spawn_blocking(move || {
                    let _ = entered.send(());
                    if waiting.recv().is_ok() {
                        worker_ended.store(true, std::sync::atomic::Ordering::Release);
                    }
                }));
                host.retire(runtime)?;
                retired.send(host.stop.clone())?;
                let result = Err(crate::runtime_authority::AuthorityRecoveryRequired(
                    "original recovery failure while an owned worker remains".into()).into());
                if explicit_finish { host.finish(result) } else { result }
            });
            let observed = (|| -> anyhow::Result<_> {
                started.recv_timeout(std::time::Duration::from_secs(10))?;
                let stop = retained.recv_timeout(std::time::Duration::from_secs(10))?;
                stop.request();
                Ok((stop.start_if_running(|| ()).is_none(), joining.is_finished(),
                    ended.load(std::sync::atomic::Ordering::Acquire), RuntimeOwner::try_acquire(&dir).is_err()))
            })();
            // Release and join the original thread even if an observation
            // failed. A failed assertion must not detach a test-owned worker.
            let released = release.send(());
            let outcome = joining.join().map_err(|_| anyhow::anyhow!("owned host test panicked"))?;
            let (stopped, detached, prematurely_ended, owner_held) = observed?;
            released?;
            assert!(stopped, "a stopped host admitted replacement work");
            assert!(!detached, "host detached the still-live blocking worker");
            assert!(!prematurely_ended);
            assert!(owner_held, "owner unlocked before its worker joined");
            assert!(outcome.as_ref().err().is_some_and(|error|
                error.is::<crate::runtime_authority::AuthorityRecoveryRequired>()),
                "human stop erased the original recovery error");
            assert!(ended.load(std::sync::atomic::Ordering::Acquire));
            drop(RuntimeOwner::try_acquire(&dir)?);
        }
        std::fs::remove_dir(dir)?;
        Ok(())
    }

}
