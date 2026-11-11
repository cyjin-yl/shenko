//! Git text knowledge + object references — docs/specs/memory.md#spec-10-3 and docs/specs/memory.md#spec-10-5.
//!
//! The managed memory directory is a real Git repository (the seed
//! bootstrap initializes `<data_dir>/memory`). Organized knowledge is written
//! there as explainable Markdown text with frontmatter, one commit per fact
//! change; the SQLite ledger stays the truth of record and the Git history
//! is its readable projection. Large binaries never enter Git: they live in
//! the content-addressed object store and Git holds a versioned pointer with
//! the object id, byte size, backend namespace and a non-secret key handle.
//!
//! SQLite raw events are never committed into this repository — the two
//! stores reference each other without merging (docs/specs/memory.md#spec-12-1).
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use shenko_store::{MemoryEntry, Store};
use shenko_types::ids::EntityId;

use crate::backend::GitHealth;

const GIT_AUTHOR: &str = "Shenko Memory <memory@shenko.local>";

/// One explainable text unit of knowledge in the memory repository.
pub struct GitKnowledgeStore {
    root: PathBuf,
}

impl GitKnowledgeStore {
    /// Open the memory repository, initializing it when the directory exists
    /// but the repository was never created (recovery from a copied data dir).
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("create memory knowledge root {}", root.display()))?;
        let store = Self { root };
        if !store.root.join(".git").exists() {
            store.run_git(&["init", "--quiet", "--initial-branch=main"])?;
        }
        store.run_git(&["rev-parse", "--show-toplevel"])?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write the text form of one entry and commit it. Returns
    /// `(commit, repo-relative path)`. A rewrite of unchanged content is a
    /// no-op: the existing commit is kept, nothing is fabricated.
    pub fn sync_entry(&self, entry: &MemoryEntry) -> Result<(String, String)> {
        let path = self.entry_path(entry);
        let body = render_entry(entry);
        let absolute = self.root.join(&path);
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let unchanged = std::fs::read_to_string(&absolute).ok().as_deref() == Some(body.as_str());
        if unchanged {
            let head = self.head_commit()?;
            return Ok((head, path));
        }
        std::fs::write(&absolute, &body)?;
        self.run_git(&["add", "--", &path])?;
        let status = self.run_git(&["status", "--porcelain", "--", &path])?;
        if status.trim().is_empty() {
            let head = self.head_commit()?;
            return Ok((head, path));
        }
        let message = format!(
            "memory: {} {} rev {}",
            if entry.deleted { "forget" } else { "retain" },
            entry.entry_id,
            entry.record_revision,
        );
        self.run_git(&[
            "-c",
            &format!("user.name={}", GIT_AUTHOR),
            "-c",
            &format!("user.email={}", GIT_AUTHOR),
            "commit",
            "--quiet",
            "-m",
            &message,
            "--",
            &path,
        ])?;
        let commit = self.head_commit()?;
        Ok((commit, path))
    }

    /// Commit every live entry whose text form is not in the repository yet
    /// and record the refs back on the durable rows. Returns how many entries
    /// were synced.
    pub fn sync_pending(&self, store: &Store, _agent: EntityId) -> Result<usize> {
        let pending = store
            .list_memory(4096)
            .context("list memory for knowledge sync")?;
        let mut synced = 0;
        for entry in pending {
            if entry.git_commit.is_some() {
                continue;
            }
            let (commit, path) = self.sync_entry(&entry)?;
            store
                .mark_memory_git(entry.entry_id, &commit, &path)
                .context("record memory git ref")?;
            synced += 1;
        }
        Ok(synced)
    }

    /// Write a versioned object pointer for a binary attachment (docs/specs/memory.md#spec-10-5).
    /// The pointer records format version, object id, byte size, backend
    /// namespace and a non-secret key handle; it is deliberately not Git LFS.
    pub fn record_pointer(
        &self,
        object: &shenko_types::object::ObjectRef,
        key_handle: Option<&str>,
    ) -> Result<String> {
        let pointer = serde_json::json!({
            "format_version": 1,
            "object_id": object.digest.0,
            "bytes": object.bytes,
            "backend_namespace": object.store_namespace,
            "key_handle": key_handle.unwrap_or(""),
        });
        let name = format!("objects/{}.json", object.digest.0.replace(':', "-"));
        let absolute = self.root.join(&name);
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&pointer)?;
        if std::fs::read_to_string(&absolute).ok().as_deref() != Some(body.as_str()) {
            std::fs::write(&absolute, &body)?;
        }
        Ok(name)
    }

    /// Repository health: resolvable HEAD and commit count.
    pub fn health(&self) -> Result<GitHealth> {
        let repository = self
            .run_git(&["rev-parse", "--show-toplevel"])
            .ok()
            .map(|output| output.trim().to_string());
        let head = self.head_commit().ok();
        let commits = self
            .run_git(&["rev-list", "--count", "HEAD"])
            .ok()
            .and_then(|output| output.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Ok(GitHealth { repository, head, commits })
    }

    fn entry_path(&self, entry: &MemoryEntry) -> String {
        let scope = sanitize(&entry.scope);
        let kind = sanitize(&entry.kind);
        format!("knowledge/{scope}/{kind}/{}.md", entry.entry_id)
    }

    fn head_commit(&self) -> Result<String> {
        let output = self.run_git(&["rev-parse", "HEAD"])?;
        Ok(output.trim().to_string())
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Shenko Memory")
            .env("GIT_AUTHOR_EMAIL", "memory@shenko.local")
            .env("GIT_COMMITTER_NAME", "Shenko Memory")
            .env("GIT_COMMITTER_EMAIL", "memory@shenko.local")
            .output()
            .with_context(|| format!("execute git {:?} in {}", args, self.root.display()))?;
        ensure!(
            output.status.success(),
            "git {:?} failed in {}: {}",
            args,
            self.root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

/// Render one entry as Markdown with a small YAML frontmatter block. The
/// frontmatter stays hand-readable: every value is one plain line.
fn render_entry(entry: &MemoryEntry) -> String {
    let mut out = String::with_capacity(entry.content.len() + 512);
    out.push_str("---\n");
    out.push_str(&format!("entry_id: {}\n", entry.entry_id));
    out.push_str(&format!("kind: {}\n", entry.kind));
    out.push_str(&format!("scope: {}\n", entry.scope));
    if let Some(source) = &entry.source {
        out.push_str(&format!("source: {}\n", source));
    }
    if let Some(applicability) = &entry.applicability {
        out.push_str(&format!("applicability: {}\n", applicability));
    }
    if let Some(confidence) = entry.confidence {
        out.push_str(&format!("confidence: {confidence}\n"));
    }
    if let Some(previous) = entry.supersedes {
        out.push_str(&format!("supersedes: {previous}\n"));
    }
    if let Some(next) = entry.superseded_by {
        out.push_str(&format!("superseded_by: {next}\n"));
    }
    out.push_str(&format!("record_revision: {}\n", entry.record_revision));
    out.push_str(&format!("deleted: {}\n", entry.deleted));
    out.push_str("---\n");
    out.push('\n');
    out.push_str(&entry.content);
    if !entry.content.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Keep repo paths safe without a filesystem dependency on the platform.
fn sanitize(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_frontmatter_with_supersession() {
        let mut entry = MemoryEntry::new(EntityId::new(), "正文内容");
        entry.applicability = Some("只在办公室".into());
        entry.confidence = Some(0.8);
        let text = render_entry(&entry);
        assert!(text.contains("entry_id:"));
        assert!(text.contains("applicability: 只在办公室"));
        assert!(text.contains("confidence: 0.8"));
        assert!(text.ends_with("正文内容\n"));
    }

    #[test]
    fn sanitize_maps_unsafe_path_parts() {
        assert_eq!(sanitize("personal"), "personal");
        assert_eq!(sanitize("a/b c"), "a_b_c");
        assert_eq!(sanitize(""), "default");
    }
}
