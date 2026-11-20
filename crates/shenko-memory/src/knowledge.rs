//! Git text knowledge + object references.
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
//! stores reference each other without merging.
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
    projection: std::sync::Mutex<()>,
}

impl GitKnowledgeStore {
    /// Open the memory repository, initializing it when the directory exists
    /// but the repository was never created (recovery from a copied data dir).
    pub fn open(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("create memory knowledge root {}", root.display()))?;
        let store = Self { root, projection: std::sync::Mutex::new(()) };
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
        let _guard = self.projection.lock().map_err(|_| anyhow::anyhow!("memory Git projection lock poisoned"))?;
        self.sync_entry_locked(entry)
    }

    fn sync_entry_locked(&self, entry: &MemoryEntry) -> Result<(String, String)> {
        let path = self.entry_path(entry);
        let body = render_entry(entry);
        let absolute = self.root.join(&path);
        if let Some(parent) = absolute.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let unchanged = match std::fs::read_to_string(&absolute) {
            Ok(existing) => existing == body,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !unchanged {
            std::fs::write(&absolute, &body)?;
        }
        self.run_git(&["add", "--", &path])?;
        let status = self.run_git(&["status", "--porcelain", "--", &path])?;
        if status.trim().is_empty() {
            let head = self.head_commit()?;
            ensure!(self.run_git(&["show", &format!("{head}:{path}")])? == body, "Git HEAD does not contain projected memory bytes");
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
        ensure!(self.run_git(&["show", &format!("{commit}:{path}")])? == body, "Git commit does not contain projected memory bytes");
        Ok((commit, path))
    }

    /// Project one exact revision and acknowledge only the actual committed
    /// text. Serialize the reload/write/ack boundary within this repository.
    pub fn sync_projected_entry(&self, store: &Store, entry: &MemoryEntry) -> Result<bool> {
        let _guard = self.projection.lock().map_err(|_| anyhow::anyhow!("memory Git projection lock poisoned"))?;
        let current = store.memory_entry(entry.entry_id)?.context("memory entry vanished")?;
        if current.record_revision != entry.record_revision { return Ok(false); }
        match self.sync_entry_locked(&current) {
            Ok((commit,path)) => Ok(store.mark_memory_git(current.entry_id,current.record_revision,&commit,&path)?),
            Err(error) => {
                store.record_memory_git_failure(current.entry_id,current.record_revision)
                    .with_context(||format!("record Git projection failure after {error:#}"))?;
                Err(error)
            }
        }
    }

    /// Drain all keyset pages, including retired rows. Failed entries remain
    /// retryable; continue past them and report failure after the scan.
    pub fn sync_pending(&self, store: &Store, agent: EntityId) -> Result<usize> {
        let mut after = None;
        let mut synced = 0;
        let mut failures = Vec::new();
        loop {
            let pending = store.pending_memory_git_page(agent, after, 256)?;
            if pending.is_empty() { break; }
            for entry in pending {
                after = Some(entry.entry_id);
                match self.sync_projected_entry(store,&entry) {
                    Ok(true) => synced += 1,
                    Ok(false) => {},
                    Err(error) => failures.push(format!("{} rev {}: {error:#}",entry.entry_id,entry.record_revision)),
                }
            }
        }
        ensure!(failures.is_empty(), "memory Git projection failures: {}", failures.join("; "));
        Ok(synced)
    }

    /// Write a versioned object pointer for a binary attachment.
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
        let repository = Some(self.run_git(&["rev-parse", "--show-toplevel"])?.trim().to_string());
        // --all is valid for an unborn repository too; command failures are
        // errors rather than invented healthy zero-commit results.
        let all_commits: u64 = self.run_git(&["rev-list", "--count", "--all"])?.trim().parse()?;
        let (head,commits)=if all_commits==0 {(None,0)} else {
            (Some(self.head_commit()?),self.run_git(&["rev-list","--count","HEAD"])?.trim().parse()?)
        };
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

    fn native_root(tag: &str) -> PathBuf {
        let root=std::env::var_os("SHENKO_NATIVE_ARTIFACT_ROOT")
            .expect("parent must provide a persistent native artifact root");
        let dir=PathBuf::from(root).join(format!("memory-{tag}-{}",EntityId::new()));
        let mut builder=std::fs::DirBuilder::new();
        #[cfg(unix)] {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&dir).unwrap();
        dir
    }

    #[test]
    fn projection_pages_pass_4096_acked_rows_and_keep_retired_work() {
        let root=native_root("pages");
        let store=Store::open(root.join("ledger.db"),root.join("cas")).unwrap();
        let agent=EntityId::new();
        let git=GitKnowledgeStore::open(root.join("knowledge")).unwrap();
        // Fixture rows are actual memory rows; the test never registers a
        // cognition content by hand or fabricates a local Git acknowledgement.
        let mut conn=rusqlite::Connection::open(root.join("ledger.db")).unwrap();
        let tx=conn.transaction().unwrap();
        for index in 0..4105 {
            tx.execute("INSERT INTO memory_entries(entry_id,agent_id,kind,content,created_at_ms)
                VALUES(?1,?2,'note',?3,?4)",
                rusqlite::params![EntityId::new().to_string(),agent.to_string(),format!("fact {index}"),index]).unwrap();
        }
        tx.commit().unwrap();
        let mut all=Vec::new();
        let mut cursor=None;
        loop {
            let page=store.pending_memory_git_page(agent,cursor,113).unwrap();
            if page.is_empty(){break;}
            cursor=page.last().map(|entry|entry.entry_id);
            all.extend(page);
        }
        assert_eq!(all.len(),4105);
        // Materialize the full text projection in one real Git commit, then
        // acknowledge its first 4096 entries with that actual commit/path.
        for entry in &all {
            let path=git.entry_path(entry);
            std::fs::create_dir_all(git.root.join(&path).parent().unwrap()).unwrap();
            std::fs::write(git.root.join(path),render_entry(entry)).unwrap();
        }
        git.run_git(&["add","--","knowledge"]).unwrap();
        git.run_git(&["commit","--quiet","-m","native memory projection fixture"]).unwrap();
        let commit=git.head_commit().unwrap();
        for entry in &all[..4096] {
            assert!(store.mark_memory_git(entry.entry_id,entry.record_revision,&commit,&git.entry_path(entry)).unwrap());
        }
        let retired=&all[4096];
        conn.execute("UPDATE memory_entries SET deleted=1,record_revision=record_revision+1 WHERE entry_id=?1",
            [retired.entry_id.to_string()]).unwrap();
        assert_eq!(git.sync_pending(&store,agent).unwrap(),9);
        assert!(store.pending_memory_git_page(agent,None,4096).unwrap().is_empty());
        assert!(git.run_git(&["show",&format!("HEAD:{}",git.entry_path(retired))]).unwrap().contains("deleted: true"));
    }

    #[test]
    fn failed_projection_does_not_starve_and_stale_revision_cannot_ack() {
        let root=native_root("failure-race");
        let store=Store::open(root.join("ledger.db"),root.join("cas")).unwrap();
        let agent=EntityId::new();
        let git=GitKnowledgeStore::open(root.join("knowledge")).unwrap();
        let entries=(0..3).map(|i|MemoryEntry::new(agent,format!("fact {i}"))).collect::<Vec<_>>();
        for entry in &entries {store.insert_memory_entry(entry).unwrap();}
        let broken=git.root.join(git.entry_path(&entries[0]));
        std::fs::create_dir_all(&broken).unwrap(); // real filesystem write failure
        assert!(git.sync_pending(&store,agent).is_err());
        let conn=rusqlite::Connection::open(root.join("ledger.db")).unwrap();
        let projected:i64=conn.query_row("SELECT COUNT(*) FROM memory_git_ack WHERE state='projected'",[],|row|row.get(0)).unwrap();
        let failed:i64=conn.query_row("SELECT COUNT(*) FROM memory_git_ack WHERE state='failed'",[],|row|row.get(0)).unwrap();
        assert_eq!((projected,failed),(2,1));
        let old=store.memory_entry(entries[1].entry_id).unwrap().unwrap();
        let (commit,path)=git.sync_entry(&old).unwrap();
        conn.execute("UPDATE memory_entries SET content='new exact revision',record_revision=record_revision+1 WHERE entry_id=?1",
            [old.entry_id.to_string()]).unwrap();
        assert!(!store.mark_memory_git(old.entry_id,old.record_revision,&commit,&path).unwrap());
        assert!(store.pending_memory_git_page(agent,None,4096).unwrap().iter().any(|entry|entry.entry_id==old.entry_id));
        // A captured older revision must not overwrite a newer Git projection.
        let current=store.memory_entry(old.entry_id).unwrap().unwrap();
        assert!(git.sync_projected_entry(&store,&current).unwrap());
        assert!(!git.sync_projected_entry(&store,&old).unwrap());
        assert!(git.run_git(&["show",&format!("HEAD:{path}")]).unwrap().contains("new exact revision"));
    }
}
