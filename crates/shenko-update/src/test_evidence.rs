//! Optional export of originals from existing update fixtures, never a replay.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

const FILE_LIMIT: u64 = 64 * 1024 * 1024;
const EXPORT_LIMIT: u64 = 256 * 1024 * 1024;
const ENTRY_LIMIT: usize = 4096;

pub(crate) struct FixtureEvidence {
    root: Option<PathBuf>,
    entries: Vec<serde_json::Value>,
    bytes: u64,
    errors: Vec<String>,
    finished: bool,
    incomplete_installation: Option<PathBuf>,
}

impl FixtureEvidence {
    pub(crate) fn new(name: &str) -> Result<Self> {
        let configured = std::env::var_os("SHENKO_UPDATE_BUILD_EVIDENCE");
        if let Some(root) = &configured {
            anyhow::ensure!(!root.is_empty(), "SHENKO_UPDATE_BUILD_EVIDENCE is empty");
        }
        let root = configured.map(|root| PathBuf::from(root).join(format!("{name}-{}", EntityId::new())));
        if let Some(root) = &root {
            fs::create_dir_all(root.parent().context("evidence parent missing")?)?;
            fs::create_dir(root)?;
        }
        let mut evidence = Self {
            root, entries: Vec::new(), bytes: 0, errors: Vec::new(), finished: false,
            incomplete_installation: None,
        };
        evidence.record("fixture-start", &serde_json::json!({
            "fixture": name,
            "source_revision_environment": std::env::var("SHENKO_SOURCE_REVISION").ok(),
            "scope": "original observations from one existing fixture execution",
            "source_snapshot_frozen": false,
            "product_acceptance": false,
        }));
        Ok(evidence)
    }

    pub(crate) fn enabled(&self) -> bool { self.root.is_some() }

    pub(crate) fn completed(&self) -> bool { self.finished && self.errors.is_empty() }

    fn failure(&mut self, label: &str, error: anyhow::Error) {
        let message = format!("update fixture evidence {label}: {error:#}");
        eprintln!("{message}");
        self.errors.push(message);
    }

    fn save(&mut self, label: &str, bytes: &[u8], original_path: Option<&Path>) -> Result<()> {
        let Some(root) = &self.root else { return Ok(()); };
        anyhow::ensure!(self.entries.len() < ENTRY_LIMIT, "evidence entry limit reached");
        anyhow::ensure!(bytes.len() as u64 <= FILE_LIMIT, "evidence file limit reached");
        let total = self.bytes.checked_add(bytes.len() as u64).context("evidence size overflow")?;
        anyhow::ensure!(total <= EXPORT_LIMIT, "evidence export limit reached");
        let name = format!("{:04}.blob", self.entries.len());
        let mut file = OpenOptions::new().write(true).create_new(true).open(root.join(&name))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        self.bytes = total;
        self.entries.push(serde_json::json!({
            "file": name, "label": label, "original_path": original_path,
            "bytes": bytes.len(), "digest": Digest::blake3(bytes),
            "observed_at": chrono::Utc::now().to_rfc3339(),
        }));
        self.write_index()
    }

    fn write_index(&self) -> Result<()> {
        let Some(root) = &self.root else { return Ok(()); };
        let bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1, "entries": self.entries, "errors": self.errors,
            "fixture_completed": self.finished,
            "incomplete_original_installation": self.incomplete_installation,
            "source_snapshot_frozen": false,
        }))?;
        let pending = root.join("index.next.json");
        let mut file = File::create(&pending)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(pending, root.join("index.json"))?;
        Ok(())
    }

    pub(crate) fn record<T: Serialize>(&mut self, label: &str, value: &T) {
        if !self.enabled() { return; }
        let result = serde_json::to_vec_pretty(value).map_err(anyhow::Error::from)
            .and_then(|bytes| self.save(label, &bytes, None));
        if let Err(error) = result { self.failure(label, error); }
    }

    pub(crate) fn file(&mut self, label: &str, path: &Path) {
        if !self.enabled() { return; }
        let result = (|| {
            let metadata = fs::symlink_metadata(path)?;
            anyhow::ensure!(metadata.file_type().is_file(), "original is not a regular file: {}", path.display());
            anyhow::ensure!(metadata.len() <= FILE_LIMIT, "original file exceeds export limit: {}", path.display());
            let mut bytes = Vec::new();
            File::open(path)?.take(FILE_LIMIT + 1).read_to_end(&mut bytes)?;
            self.save(label, &bytes, Some(path))
        })();
        if let Err(error) = result { self.failure(label, error); }
    }

    pub(crate) fn tree(&mut self, label: &str, path: &Path) {
        if !self.enabled() { return; }
        let result = (|| {
            anyhow::ensure!(fs::symlink_metadata(path)?.file_type().is_dir(), "original tree is not a directory");
            let mut pending = vec![path.to_path_buf()];
            let mut visited = 0_usize;
            while let Some(directory) = pending.pop() {
                for entry in fs::read_dir(directory)? {
                    visited += 1;
                    anyhow::ensure!(visited <= ENTRY_LIMIT, "original tree entry limit reached");
                    let entry = entry?;
                    let kind = entry.file_type()?;
                    if kind.is_dir() { pending.push(entry.path()); }
                    else if kind.is_file() { self.file(label, &entry.path()); }
                    else { anyhow::bail!("original tree has a non-regular entry: {}", entry.path().display()); }
                }
            }
            Ok(())
        })();
        if let Err(error) = result { self.failure(label, error); }
    }

    pub(crate) fn owned_file(&mut self, label: &str, path: &Path, owner: &Path) {
        if !self.enabled() { return; }
        let result = (|| {
            let path = fs::canonicalize(path)?;
            anyhow::ensure!(path.starts_with(fs::canonicalize(owner)?), "artifact is outside its owned fixture");
            self.file(label, &path);
            Ok(())
        })();
        if let Err(error) = result { self.failure(label, error); }
    }

    /// Only fixture-owned installations are watched. On early failure their
    /// current bytes are retained with coherence explicitly unasserted. The
    /// normal path exports after all original Store handles have been dropped.
    #[cfg(unix)]
    pub(crate) fn watch_installation(&mut self, path: &Path) {
        self.incomplete_installation = Some(path.to_path_buf());
    }

    #[cfg(unix)]
    pub(crate) fn closed_installation(&mut self, path: &Path) {
        self.record("installation-handles-dropped", &serde_json::json!({ "original_path": path }));
        self.tree("closed-original-installation", path);
        self.incomplete_installation = None;
    }

    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.errors.is_empty() {
            self.finished = true;
            if let Err(error) = self.write_index() { self.failure("finish", error); }
        }
        anyhow::ensure!(self.errors.is_empty(), "{}", self.errors.join("\n"));
        Ok(())
    }
}

impl Drop for FixtureEvidence {
    fn drop(&mut self) {
        if !self.enabled() || self.completed() { return; }
        if let Some(path) = self.incomplete_installation.take() {
            self.record("incomplete-scope-exit", &serde_json::json!({
                "original_path": path, "thread_panicking": std::thread::panicking(),
                "database_snapshot_consistency": "not asserted",
                "cleanup_or_process_join": "not inferred from scope exit",
            }));
            self.tree("incomplete-original-installation-bytes", &path);
        }
        if let Err(error) = self.write_index() { self.failure("incomplete-index", error); }
        eprintln!("update fixture did not complete its evidence export; retained originals: {:?}; errors: {:?}",
            self.root, self.errors);
    }
}
