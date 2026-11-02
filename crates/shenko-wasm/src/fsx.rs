//! Capability-scoped filesystem primitives for the Shenko preview1 host.
//!
//! Every guest-visible filesystem operation in this crate is expressed as an
//! operation on an already-admitted directory handle (`cap_std::fs::Dir`),
//! never as a re-resolution of a host pathname (SPEC §8.3, A08). This module
//! holds the two primitives the rest of the crate needs:
//!
//! * [`open_component`] opens exactly one path component relative to a
//!   handle, without following a final symbolic link.
//! * [`open_component_with`] opens one component with explicit create /
//!   truncate / append flags, again relative to the handle.
//!
//! cap-std resolves every path beneath the handle it was given, so a symlink
//! planted by the guest can never redirect a resolution outside the admitted
//! mount. cap-std's own `OpenOptions` cannot express "do not follow the final
//! symlink", so the decision is made here: the entry is inspected with
//! `symlink_metadata` first, and the opened descriptor's `(dev, ino)` is
//! compared against that inspection. A symbolic link is reported as `ELOOP`
//! (the preview1 contract for opening a link without `SYMLINK_FOLLOW`), and
//! an entry that was replaced between the inspection and the open is refused
//! instead of being silently followed.

use cap_std::fs::{Dir, Metadata, OpenOptions};
use std::fs::File;
use std::io;
use std::path::Path;

/// The error returned when a component is a symbolic link and the caller did
/// not ask for link following.
pub const ELOOP: &str = "ELOOP";
/// The error returned when the entry changed between inspection and open.
pub const EAGAIN: &str = "EAGAIN";

/// Filesystem identity of an inspected entry: `(device, inode)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

impl FileIdentity {
    /// Identity of an already-open file.
    pub fn of_file(file: &File) -> io::Result<Self> {
        Ok(meta_identity(&file.metadata()?))
    }

    /// Identity of a capability-scoped metadata value.
    pub fn from_metadata(meta: &Metadata) -> Option<Self> {
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt;
            Some(Self {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            None
        }
    }

}

/// Identity of a `std::fs::Metadata` value.
fn meta_identity(meta: &std::fs::Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FileIdentity {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        FileIdentity { dev: 0, ino: 0 }
    }
}

/// Inspect one component of `dir` without following a final symbolic link.
pub fn component_metadata(dir: &Dir, name: &Path) -> io::Result<Option<Metadata>> {
    match dir.symlink_metadata(name) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Open one component of `dir` as a directory, without following a final
/// symbolic link.
///
/// `Ok(None)` means the component does not exist; `Err` carries an `io::Error`
/// whose message is [`ELOOP`] for a symbolic link.
pub fn open_component(dir: &Dir, name: &Path) -> io::Result<Option<File>> {
    let inspected = match component_metadata(dir, name)? {
        Some(meta) => meta,
        None => return Ok(None),
    };
    if inspected.file_type().is_symlink() {
        return Err(io::Error::other(ELOOP));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    let file = match dir.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let opened = file.into_std();
    let after = opened.metadata()?;
    if !after.is_dir() {
        return Err(io::Error::other("ENOTDIR"));
    }
    let expected = FileIdentity::from_metadata(&inspected);
    let actual = FileIdentity::of_file(&opened)?;
    if let Some(expected) = expected {
        if expected != actual {
            return Err(io::Error::other(EAGAIN));
        }
    }
    Ok(Some(opened))
}

/// Open one component of `dir` as a regular file with explicit options,
/// without following a final symbolic link.
///
/// `Ok(None)` means the component does not exist and the options did not ask
/// for creation; `Err` carries an `io::Error` whose message is [`ELOOP`] for
/// a symbolic link.
pub fn open_component_with(
    dir: &Dir,
    name: &Path,
    options: &OpenOptions,
) -> io::Result<Option<File>> {
    let inspected = match component_metadata(dir, name)? {
        Some(meta) => Some(meta),
        None => None,
    };
    if let Some(meta) = &inspected {
        if meta.file_type().is_symlink() {
            return Err(io::Error::other(ELOOP));
        }
    }
    let file = match dir.open_with(name, options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let opened = file.into_std();
    if let Some(inspected) = &inspected {
        let expected = FileIdentity::from_metadata(inspected);
        let actual = FileIdentity::of_file(&opened)?;
        if let Some(expected) = expected {
            if expected != actual {
                return Err(io::Error::other(EAGAIN));
            }
        }
    }
    Ok(Some(opened))
}

/// Metadata of the directory a handle points at.
pub fn dir_metadata(dir: &Dir) -> io::Result<std::fs::Metadata> {
    let file = dir.try_clone()?.into_std_file();
    file.metadata()
}

/// Capability-scoped metadata of the directory a handle points at.
pub fn dir_cap_metadata(dir: &Dir) -> io::Result<Metadata> {
    Metadata::from_file(&dir.try_clone()?.into_std_file())
}

/// Capability-scoped metadata of an open file.
pub fn file_metadata(file: &File) -> io::Result<Metadata> {
    Metadata::from_file(file)
}
