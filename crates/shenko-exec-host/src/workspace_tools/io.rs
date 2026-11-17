//! Bounded UTF-8 workspace I/O. This is a cooperative workflow, not an OS
//! sandbox: other programs can write between the final check and rename.
use std::{
    fs::{self, File, OpenOptions, Permissions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use shenko_types::{ids::EntityId, object::Digest};

use super::edit::{EditorMode, TextChange, apply_edit, render_anchors};

pub(super) const RESULT_BYTES: usize = 12 * 1024;
const FILE_BYTES: usize = 1024 * 1024;
const SEARCH_ENTRIES: usize = 4096;
const SEARCH_BYTES: usize = 16 * FILE_BYTES;

#[derive(Debug)]
pub(super) struct Failure {
    pub code: String,
    pub message: String,
    /// Once staging starts, an I/O error is conservatively unresolved. A
    /// missing receipt must never cause the adapter to repeat a file write.
    pub uncertain: bool,
}

impl Failure {
    fn new(code: &str, message: impl ToString) -> Self {
        Self {
            code: code.into(),
            message: message.to_string(),
            uncertain: false,
        }
    }

    fn after_staging(error: impl ToString) -> Self {
        Self {
            uncertain: true,
            ..Self::new("write_unresolved", error)
        }
    }
}

type Result<T> = std::result::Result<T, Failure>;

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|e| Failure::new("invalid_request", e))
}

fn fits(value: &Value) -> bool {
    serde_json::to_vec(value).is_ok_and(|v| v.len() <= RESULT_BYTES)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    start_line: Option<usize>,
    max_lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SearchMode {
    Content,
    Path,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
    path: Option<String>,
    mode: Option<SearchMode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateInput {
    path: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    expected_digest: Digest,
    edits: Value,
}

struct Snapshot {
    text: String,
    digest: Digest,
    permissions: Permissions,
}

/// All handlers in one installed profile share this lock. It serializes their
/// write preconditions; it cannot lock out a shell or another process.
pub(super) struct WorkspaceFs {
    pub root: PathBuf,
    writes: Mutex<()>,
}

impl WorkspaceFs {
    pub fn new(root: PathBuf) -> anyhow::Result<Self> {
        let root = root.canonicalize()?;
        anyhow::ensure!(
            root.is_dir(),
            "workspace root must be an existing directory"
        );
        Ok(Self {
            root,
            writes: Mutex::new(()),
        })
    }

    fn relative(&self, path: &str) -> Result<PathBuf> {
        let path = Path::new(path);
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(Failure::new(
                "invalid_path",
                "use a path relative to the configured root, without '..'",
            ));
        }
        Ok(self.root.join(path))
    }

    fn existing(&self, path: &str) -> Result<PathBuf> {
        let target = self
            .relative(path)?
            .canonicalize()
            .map_err(|e| Failure::new("path_unavailable", e))?;
        if !target.starts_with(&self.root) {
            return Err(Failure::new(
                "outside_root",
                "resolved path is outside the configured workspace root",
            ));
        }
        Ok(target)
    }

    fn destination(&self, path: &str) -> Result<PathBuf> {
        let requested = self.relative(path)?;
        let name = requested
            .file_name()
            .ok_or_else(|| Failure::new("invalid_path", "a file name is required"))?;
        let parent = requested
            .parent()
            .ok_or_else(|| Failure::new("invalid_path", "parent is required"))?
            .canonicalize()
            .map_err(|e| Failure::new("parent_unavailable", e))?;
        if !parent.starts_with(&self.root) {
            return Err(Failure::new(
                "outside_root",
                "parent resolves outside the configured workspace root",
            ));
        }
        Ok(parent.join(name))
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    fn snapshot(path: &Path) -> Result<Snapshot> {
        // Refuse known non-files before open (e.g. a FIFO). Concurrent
        // adversarial replacement is outside this cooperative tool contract.
        let metadata = fs::metadata(path).map_err(|e| Failure::new("read_failed", e))?;
        if !metadata.is_file() {
            return Err(Failure::new(
                "not_regular_file",
                "only regular UTF-8 files are supported",
            ));
        }
        if metadata.len() > FILE_BYTES as u64 {
            return Err(Failure::new(
                "file_too_large",
                "workspace tools support files up to 1 MiB; use an execution tool for larger files",
            ));
        }
        let file = File::open(path).map_err(|e| Failure::new("read_failed", e))?;
        let mut bytes = Vec::new();
        file.take((FILE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|e| Failure::new("read_failed", e))?;
        if bytes.len() > FILE_BYTES {
            return Err(Failure::new(
                "file_too_large",
                "file grew beyond the 1 MiB limit during read",
            ));
        }
        let digest = Digest::blake3(&bytes);
        let text = String::from_utf8(bytes).map_err(|e| Failure::new("not_utf8", e))?;
        Ok(Snapshot {
            text,
            digest,
            permissions: metadata.permissions(),
        })
    }

    pub fn read(&self, mode: EditorMode, input: Value) -> Result<Value> {
        let input: ReadInput = parse(input)?;
        let start = input.start_line.unwrap_or(1);
        let count = input.max_lines.unwrap_or(100);
        if start == 0 || !(1..=500).contains(&count) {
            return Err(Failure::new(
                "invalid_range",
                "start_line is 1-based; max_lines must be 1..=500",
            ));
        }
        let path = self.existing(&input.path)?;
        let snapshot = Self::snapshot(&path)?;
        let lines = render_anchors(&snapshot.text);
        if start > lines.len().max(1) {
            return Err(Failure::new(
                "invalid_range",
                format!("file has {} physical lines", lines.len()),
            ));
        }
        let mut result = json!({
            "status":"read", "path":self.display(&path), "digest":snapshot.digest,
            "start_line":start, "end_line":start.saturating_sub(1), "total_lines":lines.len(),
            "truncated":false, "next_line":null
        });
        match mode {
            EditorMode::Replace => {
                result["text"] = json!("");
            }
            EditorMode::Hashline => {
                result["anchors"] = json!([]);
            }
        }
        let mut selected = 0;
        for line in lines.iter().skip(start - 1).take(count) {
            let mut candidate = result.clone();
            match mode {
                EditorMode::Replace => {
                    let text = format!(
                        "{}{}{}",
                        candidate["text"].as_str().unwrap_or(""),
                        line.text,
                        line.ending
                    );
                    candidate["text"] = json!(text);
                }
                EditorMode::Hashline => {
                    candidate["anchors"]
                        .as_array_mut()
                        .expect("array initialized")
                        .push(json!(line));
                }
            }
            candidate["end_line"] = json!(line.line);
            if !fits(&candidate) {
                break;
            }
            result = candidate;
            selected += 1;
        }
        if selected == 0 && !lines.is_empty() {
            return Err(Failure::new(
                "line_too_long",
                "the requested line exceeds the tool response budget; use an execution tool to inspect this file",
            ));
        }
        let next = start - 1 + selected;
        result["truncated"] = json!(next < lines.len());
        if next < lines.len() {
            result["next_line"] = json!(next + 1);
        }
        Ok(result)
    }

    pub fn search(&self, input: Value) -> Result<Value> {
        let input: SearchInput = parse(input)?;
        let path_mode = matches!(input.mode, Some(SearchMode::Path));
        if input.query.is_empty() && !path_mode {
            return Err(Failure::new(
                "empty_query",
                "content searches require a nonempty literal query; mode:path accepts an empty query to list paths",
            ));
        }
        let start = self.existing(input.path.as_deref().unwrap_or("."))?;
        let mut pending = vec![start];
        let mut entries = 0;
        let mut scanned = 0;
        let mut skipped = 0;
        let mut bytes = 0;
        let mut matches = Vec::new();
        let mut truncated = false;
        'walk: while let Some(path) = pending.pop() {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if metadata.is_dir() {
                let children = match fs::read_dir(&path) {
                    Ok(children) => children,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                let mut paths = Vec::new();
                for child in children {
                    entries += 1;
                    if entries > SEARCH_ENTRIES {
                        truncated = true;
                        break;
                    }
                    match child {
                        Ok(child) if child.file_name() != ".git" => paths.push(child.path()),
                        Ok(_) => {}
                        Err(_) => {
                            skipped += 1;
                        }
                    }
                }
                paths.sort();
                pending.extend(paths.into_iter().rev());
                continue;
            }
            if !metadata.is_file() {
                skipped += 1;
                continue;
            }
            scanned += 1;
            let display = self.display(&path);
            let candidates = if path_mode {
                if display.contains(&input.query) {
                    vec![json!({"path":display})]
                } else {
                    Vec::new()
                }
            } else {
                bytes += metadata.len().min(FILE_BYTES as u64 + 1) as usize;
                if bytes > SEARCH_BYTES {
                    truncated = true;
                    break;
                }
                let snapshot = match Self::snapshot(&path) {
                    Ok(snapshot) => snapshot,
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                let lines: Vec<_> = snapshot.text.split_inclusive('\n').collect();
                let mut hits = Vec::new();
                for (index, line) in lines.iter().enumerate() {
                    if line.contains(&input.query) {
                        let from = index.saturating_sub(2);
                        let to = (index + 3).min(lines.len());
                        hits.push(
                            json!({"path":display,"line":index+1,"start_line":from+1,"end_line":to,
                            "digest":snapshot.digest,"text":lines[from..to].concat()}),
                        );
                        if hits.len() >= 100 {
                            truncated = true;
                            break;
                        }
                    }
                }
                hits
            };
            for candidate in candidates {
                if matches.len() == 100 {
                    truncated = true;
                    break 'walk;
                }
                matches.push(candidate);
                if serde_json::to_vec(&matches).map_or(true, |v| v.len() > RESULT_BYTES - 1024) {
                    matches.pop();
                    truncated = true;
                    break 'walk;
                }
            }
        }
        Ok(
            json!({"status":"searched", "matches":matches, "truncated":truncated,
            "scanned_files":scanned, "skipped_entries":skipped,
            "scope":"literal search; symlinks and .git entries skipped; narrow path/query when truncated"}),
        )
    }

    pub fn create(&self, input: Value) -> Result<Value> {
        let input: CreateInput = parse(input)?;
        if input.text.len() > FILE_BYTES {
            return Err(Failure::new("file_too_large", "new file exceeds 1 MiB"));
        }
        let _guard = self.writes.lock();
        let target = self.destination(&input.path)?;
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                return Err(Failure::new(
                    "already_exists",
                    "create never overwrites an existing path; read it and edit with its digest",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Failure::new("path_unavailable", e)),
        }
        let stage = StagedFile::write(&target, &input.text, None)?;
        // hard_link is an atomic absent-only publication, unlike rename, which
        // could replace a file created after the check above.
        fs::hard_link(&stage.path, &target).map_err(Failure::after_staging)?;
        stage.remove()?;
        Self::sync_parent(&target)?;
        Self::verify(&target, &input.text)?;
        Ok(
            json!({"status":"applied", "path":self.display(&target), "before_digest":null,
            "after_digest":Digest::blake3(input.text.as_bytes()),
            "changes":[TextChange { start_byte:0, end_byte:0, old_text:String::new(), new_text:input.text }]}),
        )
    }

    pub fn edit(&self, mode: EditorMode, input: Value) -> Result<Value> {
        let input: EditInput = parse(input)?;
        if input.expected_digest.canonical_blake3_hex().is_none() {
            return Err(Failure::new(
                "invalid_digest",
                "expected_digest must be the full blake3: digest returned by read or search",
            ));
        }
        let _guard = self.writes.lock();
        let target = self.existing(&input.path)?;
        let before = Self::snapshot(&target)?;
        if before.digest != input.expected_digest {
            return Err(Failure::new(
                "stale_snapshot",
                "file differs from expected_digest; read the current file and replan",
            ));
        }
        let plan = apply_edit(mode, &before.text, json!({"edits":input.edits}))
            .map_err(|e| Failure::new(e.code, e.message))?;
        if plan.updated.len() > FILE_BYTES {
            return Err(Failure::new("file_too_large", "edited file exceeds 1 MiB"));
        }
        let stage = StagedFile::write(&target, &plan.updated, Some(before.permissions))?;
        let current = Self::snapshot(&target).map_err(|e| Failure::after_staging(e.message))?;
        if current.digest != before.digest {
            stage.remove()?;
            return Err(Failure::new(
                "stale_snapshot",
                "file changed before commit; read the current file and replan",
            ));
        }
        // Atomic replacement protects against partially written target bytes.
        // It is not a kernel compare-and-swap against uncooperative writers.
        fs::rename(&stage.path, &target).map_err(Failure::after_staging)?;
        Self::sync_parent(&target)?;
        Self::verify(&target, &plan.updated)?;
        Ok(
            json!({"status":"applied", "path":self.display(&target), "before_digest":before.digest,
            "after_digest":Digest::blake3(plan.updated.as_bytes()), "changes":plan.changes}),
        )
    }

    fn sync_parent(target: &Path) -> Result<()> {
        #[cfg(unix)]
        File::open(target.parent().expect("resolved file parent"))
            .and_then(|directory| directory.sync_all())
            .map_err(Failure::after_staging)?;
        #[cfg(not(unix))]
        let _ = target;
        Ok(())
    }

    fn verify(target: &Path, expected: &str) -> Result<()> {
        let after = Self::snapshot(target).map_err(|e| Failure::after_staging(e.message))?;
        if after.text != expected {
            return Err(Failure::after_staging(
                "write was published but readback differs; inspect the current file; do not blindly retry",
            ));
        }
        Ok(())
    }
}

struct StagedFile {
    path: PathBuf,
}

impl StagedFile {
    fn write(target: &Path, text: &str, permissions: Option<Permissions>) -> Result<Self> {
        let path = target.with_file_name(format!(".shenko-write-{}", EntityId::new()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(Failure::after_staging)?;
        // The cleanup guard owns only a file this call actually created.
        let stage = Self { path };
        file.write_all(text.as_bytes())
            .map_err(Failure::after_staging)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)
                .map_err(Failure::after_staging)?;
        }
        file.sync_all().map_err(Failure::after_staging)?;
        Ok(stage)
    }

    fn remove(self) -> Result<()> {
        fs::remove_file(&self.path).map_err(Failure::after_staging)
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Keep provider-facing JSON complete under the child's 16 KiB envelope. The
/// durable raw receipt retains every change, even when the display is shortened.
pub(super) fn display_result(mut value: Value) -> Value {
    let total = value.get("changes").and_then(Value::as_array).map(Vec::len);
    if let Some(total) = total {
        value["change_count"] = json!(total);
        value["diff_truncated"] = json!(false);
        while !fits(&value) {
            if value["changes"]
                .as_array_mut()
                .is_none_or(|changes| changes.pop().is_none())
            {
                break;
            }
            value["diff_truncated"] = json!(true);
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_write_and_late_invalid_edit_leave_original_bytes_unchanged() {
        let root = std::env::temp_dir().join(format!("shenko-workspace-{}", EntityId::new()));
        fs::create_dir(&root).unwrap();
        let workspace = WorkspaceFs::new(root.clone()).unwrap();
        let target = root.join("source.txt");
        fs::write(&target, "first\r\nsecond\n").unwrap();
        let snapshot = workspace
            .read(EditorMode::Replace, json!({"path":"source.txt"}))
            .unwrap();
        fs::write(&target, "someone else's bytes\n").unwrap();
        let stale = workspace
            .edit(
                EditorMode::Replace,
                json!({"path":"source.txt","expected_digest":snapshot["digest"],
            "edits":[{"old_text":"first","new_text":"changed"}]}),
            )
            .unwrap_err();
        assert_eq!(stale.code, "stale_snapshot");
        let current = workspace
            .read(EditorMode::Replace, json!({"path":"source.txt"}))
            .unwrap();
        let invalid = workspace.edit(EditorMode::Replace, json!({"path":"source.txt","expected_digest":current["digest"],
            "edits":[{"old_text":"someone","new_text":"my"},{"old_text":"missing","new_text":"wrong"}]})).unwrap_err();
        assert_eq!(invalid.code, "no_match");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "someone else's bytes\n"
        );
        workspace
            .edit(
                EditorMode::Replace,
                json!({"path":"source.txt","expected_digest":current["digest"],
            "edits":[{"old_text":"someone else's","new_text":"verified"}]}),
            )
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "verified bytes\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn paged_hashlines_and_durable_diff_projection_remain_complete_json() {
        let root = std::env::temp_dir().join(format!("shenko-workspace-{}", EntityId::new()));
        fs::create_dir(&root).unwrap();
        let workspace = WorkspaceFs::new(root.clone()).unwrap();
        let text = "α\r\n".repeat(400);
        workspace
            .create(json!({"path":"created.txt","text":text}))
            .unwrap();
        assert_eq!(
            workspace
                .create(json!({"path":"created.txt","text":"overwrite"}))
                .unwrap_err()
                .code,
            "already_exists"
        );
        let page = workspace
            .read(
                EditorMode::Hashline,
                json!({"path":"created.txt","max_lines":500}),
            )
            .unwrap();
        assert!(page["truncated"].as_bool().unwrap());
        assert!(fits(&page));
        let next = workspace
            .read(
                EditorMode::Hashline,
                json!({"path":"created.txt","start_line":page["next_line"]}),
            )
            .unwrap();
        assert_eq!(next["digest"], page["digest"]);
        let raw = json!({"status":"applied","changes":[{"new_text":"x".repeat(20_000)}]});
        let display = display_result(raw.clone());
        assert!(display["diff_truncated"].as_bool().unwrap());
        assert!(fits(&display));
        assert_eq!(
            raw["changes"][0]["new_text"].as_str().unwrap().len(),
            20_000
        );
        fs::remove_dir_all(root).unwrap();
    }
}
