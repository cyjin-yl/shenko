//! Interpreter state is guest-controlled. Never turn its guest pathname into an
//! ambient host pathname: resolve every operation under an opened preopen.

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const BASELINE_FILE: &str = "baseline.state";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub bytes: u64,
    pub blake3: String,
}

pub struct StateFile {
    root: Dir,
    relative: PathBuf,
}

fn sync_directory(dir: &Dir) -> Result<()> {
    // cap-std directory capabilities may use O_PATH, which cannot be fsynced.
    // Open "." read-only relative to the pinned capability, not an ambient path.
    dir.open(".")?.sync_all().context("sync state directory")
}

impl StateFile {
    pub fn new(root: &Path, relative: PathBuf) -> Result<Self> {
        Ok(Self {
            root: Dir::open_ambient_dir(root, ambient_authority())
                .with_context(|| format!("open state preopen {}", root.display()))?,
            relative,
        })
    }

    fn parent(&self, create: bool) -> Result<Option<Dir>> {
        let path = self.relative.parent().unwrap_or(Path::new(""));
        let path = if path.as_os_str().is_empty() { Path::new(".") } else { path };
        if create {
            self.root.create_dir_all(path)?;
        }
        match self.root.open_dir(path) {
            Ok(parent) => Ok(Some(parent)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !create => Ok(None),
            Err(error) => Err(error).context("open confined state parent"),
        }
    }

    fn name(&self) -> &std::ffi::OsStr {
        self.relative.file_name().expect("validated state path has a file name")
    }

    fn sync_ancestors(&self) -> Result<()> {
        let mut current = self.root.open_dir(".")?;
        sync_directory(&current)?;
        for component in self.relative.parent().unwrap_or(Path::new("")).components() {
            let std::path::Component::Normal(name) = component else {
                bail!("REPL_STATE_INVALID: non-normal state path");
            };
            current = match current.open_dir(name) {
                Ok(dir) => dir,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error).context("open confined state ancestor"),
            };
            sync_directory(&current)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<Option<String>> {
        let file = match self.root.open(&self.relative) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open confined state file"),
        };
        if !file.metadata()?.is_file() {
            bail!("REPL_STATE_INVALID: state is not a regular file");
        }
        let (digest, _) = crate::journal::hash_reader(file)?;
        Ok(Some(digest))
    }

    /// Snapshot the initial/fork state once, before publishing its manifest.
    pub fn snapshot(&self, dir: &Path) -> Result<Option<Baseline>> {
        let mut source = match self.root.open(&self.relative) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("open confined baseline state"),
        };
        if !source.metadata()?.is_file() {
            bail!("REPL_STATE_INVALID: baseline state is not a regular file");
        }
        let mut target = fs::OpenOptions::new().write(true).create_new(true)
            .open(dir.join(BASELINE_FILE))?;
        let (blake3, bytes) = copy_and_hash(&mut source, &mut target)?;
        target.sync_all()?;
        File::open(dir)?.sync_all()?;
        Ok(Some(Baseline { bytes, blake3 }))
    }

    /// Unlink the final entry relative to its capability-confined parent. The
    /// final symlink itself is unlinked; its target is never removed.
    pub fn clear(&self) -> Result<()> {
        if let Some(parent) = self.parent(false)? {
            match parent.remove_file(self.name()) {
                Ok(()) => sync_directory(&parent)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error).context("remove confined state file"),
            }
        }
        self.sync_ancestors()
    }

    /// Restore the frozen baseline through a fresh, same-directory temporary
    /// file. Verify the bytes while copying, before publishing the rename.
    pub fn restore(&self, session_dir: &Path, baseline: Option<&Baseline>) -> Result<()> {
        let Some(baseline) = baseline else { return self.clear(); };
        let mut source = File::open(session_dir.join(BASELINE_FILE))
            .context("REPL_STATE_CORRUPT: baseline missing")?;
        if !source.metadata()?.is_file() {
            bail!("REPL_STATE_CORRUPT: baseline is not a regular file");
        }
        let parent = self.parent(true)?.expect("created state parent");
        let tmp = format!(".shenko-state-{}", uuid::Uuid::new_v4());
        let result = (|| -> Result<()> {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            let mut target = parent.open_with(&tmp, &opts)?;
            let (blake3, bytes) = copy_and_hash(&mut source, &mut target)?;
            if bytes != baseline.bytes || blake3 != baseline.blake3 {
                bail!("REPL_STATE_CORRUPT: baseline bytes or digest mismatch");
            }
            target.sync_all()?;
            parent.rename(&tmp, &parent, self.name())?;
            sync_directory(&parent)?;
            self.sync_ancestors()?;
            Ok(())
        })();
        // An unsuccessful attempt may leave a fresh temporary entry; it never
        // replaces the admitted state and a future attempt gets a new name.
        result
    }
}

fn copy_and_hash(source: &mut impl Read, target: &mut impl Write) -> Result<(String, u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 { break; }
        target.write_all(&buffer[..n])?;
        hasher.update(&buffer[..n]);
        bytes = bytes.checked_add(n as u64).context("state file too large")?;
    }
    Ok((hasher.finalize().to_hex().to_string(), bytes))
}
