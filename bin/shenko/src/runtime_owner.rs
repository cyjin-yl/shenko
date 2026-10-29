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

}
