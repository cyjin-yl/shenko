//! Persistent Playground — SPEC §43.2.
//!
//! Artifacts live under `SHENKO_DATA_DIR/playground/<artifact_id>/`:
//!
//! - `versions/<revision>` — every content revision ever committed. The
//!   revision file is written before `meta.json` points at it, so a crash
//!   between them leaves either the old or the new truth, never a partial.
//! - `meta.json` — the durable [`ArtifactRecord`].
//! - `backend/module` — the bound WASI guest (WAT or WASM bytes), pinned by
//!   [`BackendBinding::module_digest`].
//! - `state/` — the guest's isolated business state; mounted read-write at
//!   `/home` inside WASI, while the whole artifact directory is preopened
//!   read-only at `/artifact`.
//! - `actions/<request_id>.json|stdout|stderr|request.json` — durable action
//!   admissions and receipts.
//!
//! Backend actions run through the real WASI command runtime (shenko-wasm),
//! admitted durably by client `request_id` before any guest effect exists.
//! A replay returns the stored receipt; the same key with different payload is
//! `ACTION_CONFLICT`; a different bound module is `ARTIFACT_VERSION_STALE`.
//! WASI has no external effect channel, so the guest's own `/home` state plus
//! bounded stdout are its only effects — host failures record `unknown` and a
//! replay serves that receipt instead of re-arming the guest.
//!
//! Committed changes emit [`InboxEventKind::ArtifactChanged`]. Action events
//! carry enough state for a non-text change to be explained or referenced
//! (action name, outcome, changed files, output reference) and are delivered
//! with a bounded await — never silently dropped on a full inbox.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use shenko_store::store::Store;
use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::{ids::EntityId, object::Digest, object::ObjectRef};
use shenko_wasm::{
    InputFile, MemoryOutputPipe, MountMode, MountSpec, WasiCommand, WasmRuntime,
    HOME_GUEST_PATH,
};

/// Schema of persisted records on this node. Bump when fields change shape.
const ACTION_SCHEMA_VERSION: u32 = 1;
const BINDING_SCHEMA_VERSION: u32 = 1;
/// Bounded guest stdout/stderr — a fuller pipe makes the guest write trap
/// rather than growing unbounded.
const GUEST_OUTPUT_CAP: usize = 64 * 1024;
/// Bound on module bytes accepted by `PUT .../backend` and frozen inputs.
const MAX_MODULE_BYTES: usize = 2 * 1024 * 1024;
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Action events wait this long for inbox room; PUT events keep try_send.
const ACTION_INBOX_WAIT: Duration = Duration::from_secs(1);

/// Guest path where the artifact directory is preopened read-only so the
/// backend can read its own content/meta but can only write to `/home`.
const ARTIFACT_GUEST_PATH: &str = "/artifact";

/// Versioned binding of an artifact to an executable WASI backend. The module
/// digest pins the exact guest bytes; the contract documents how the runtime
/// invokes it. Actions record the digest they ran against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendBinding {
    pub schema_version: u32,
    /// Content digest of `backend/module` — the guest's executable identity.
    pub module_digest: Digest,
    /// "wat" (text module) or "wasm" (binary).
    pub module_format: String,
    pub bound_at: DateTime<Utc>,
    /// CAS object holding the module bytes when the durable store is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_ref: Option<ObjectRef>,
    pub contract: BackendContract,
}

/// The fixed invocation contract between the host runtime and a bound guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendContract {
    /// argv: program name, then the action name.
    pub arguments: Vec<String>,
    /// Environment keys set on the command, all values filled per-action.
    pub environment: Vec<String>,
    /// Exact frozen `input` JSON bytes of the accepted request.
    pub stdin: String,
    /// Bounded UTF-8; parsed as JSON when valid.
    pub stdout: String,
    /// RW preopen = artifact state dir (`/home`), RO preopen = artifact dir.
    pub mounts: Vec<String>,
}

fn backend_contract() -> BackendContract {
    BackendContract {
        arguments: vec!["playground-backend".into(), "<action>".into()],
        environment: vec![
            "SHENKO_ARTIFACT_ID".into(),
            "SHENKO_ACTION".into(),
            "SHENKO_REQUEST_ID".into(),
        ],
        stdin: "frozen input JSON of the accepted request".into(),
        stdout: format!("UTF-8, bounded to {GUEST_OUTPUT_CAP} bytes; JSON parsed when valid"),
        mounts: vec![
            format!("{HOME_GUEST_PATH} rw <- artifact state/ (business state)"),
            format!("{ARTIFACT_GUEST_PATH} ro <- artifact dir (content/meta/versions)"),
        ],
    }
}

/// Persisted admission + receipt for one `(artifact_id, request_id)` action.
/// Written BEFORE any guest effect exists, updated in place with the outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub schema_version: u32,
    pub request_id: String,
    pub artifact_id: EntityId,
    pub artifact_revision: u64,
    /// Guest module this action executed against. Replays under a different
    /// module are rejected stale rather than re-executed.
    pub backend_digest: Digest,
    pub action: String,
    pub input_digest: Digest,
    /// Frozen input bytes as admitted (exact request payload).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_ref: Option<ObjectRef>,
    pub state: ActionState,
    pub admitted_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Structured output when the guest wrote valid JSON to bounded stdout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    /// State files the guest created or modified — the change reference.
    pub changed_files: Vec<String>,
    /// CAS object holding the completed receipt, when the store is attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt_ref: Option<ObjectRef>,
    /// Host-side error text when the guest never ran cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Whether the ArtifactChanged event reached the subject inbox.
    #[serde(default)]
    pub inbox_delivered: bool,
}

enum ClaimOutcome {
    Claimed,
    Existing(ActionRecord),
    InFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    /// Written before the guest ran; a crash leaves this pending forever —
    /// replays serve the pending receipt rather than re-executing.
    Admitted,
    /// Guest exited 0.
    Executed,
    /// Guest ran and exited nonzero; no re-execution on replay.
    Failed,
    /// Host failure (compile, mounts, trap, blocked thread). The guest may
    /// have partial `/home` writes; the record is terminal, not re-armed.
    Unknown,
}

impl ActionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Admitted => "admitted",
            Self::Executed => "executed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

/// Admission rejection for `POST .../backend/action`. The `code` strings are
/// stable machine-readable tokens; details go in `error`.
#[derive(Debug)]
pub enum ActionReject {
    /// The artifact does not exist.
    NotFound,
    /// Another invocation is writing this request's admission right now.
    InFlight,
    /// request_id/action failed the safe-token rule or input exceeded bounds.
    Invalid(String),
    /// Same request_id with a different action or input bytes.
    Conflict,
    /// Same request_id under a different bound module digest.
    Stale,
    /// The artifact has no bound backend.
    NoBackend,
}

/// One mounted Playground artifact. Subset of SPEC §43.2 `ArtifactRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: EntityId,
    pub title: String,
    pub mime: String,
    /// Absolute path of this revision's bytes (`versions/<revision>`).
    pub bytes_path: PathBuf,
    pub revision: u64,
    /// Set when this revision was created by rollback — history is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<u64>,
    pub updated_at: DateTime<Utc>,
    /// Bound WASI backend, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<BackendBinding>,
}

/// Filesystem-backed artifact registry shared by the Playground routes.
///
/// The map is an in-memory index; `meta.json` and the `versions/` and
/// `actions/` directories are the durable truth. Startup reloads the index
/// from disk so artifacts, bindings and action receipts survive restarts.
#[derive(Clone)]
pub struct PlaygroundState {
    root: Arc<PathBuf>,
    inbox_tx: Option<Arc<Sender<InboxEvent>>>,
    store: Option<Arc<Store>>,
    records: Arc<RwLock<HashMap<EntityId, ArtifactRecord>>>,
}

/// What the host observed after one guest run.
struct BackendOutcome {
    state: ActionState,
    exit_code: Option<i32>,
    error: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output: Option<Value>,
    changed_files: Vec<String>,
}

impl PlaygroundState {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let state = Self {
            root: Arc::new(root.into()),
            inbox_tx: None,
            store: None,
            records: Arc::new(RwLock::new(HashMap::new())),
        };
        state.load_from_disk();
        state
    }

    /// Route artifact changes into the subject inbox. SPEC §43.2.
    pub fn with_inbox(mut self, inbox_tx: Sender<InboxEvent>) -> Self {
        self.inbox_tx = Some(Arc::new(inbox_tx));
        self
    }

    /// Attach the durable store so modules, inputs and receipts are also
    /// sealed into CAS. Without it the files remain the truth and the
    /// `*_ref` fields honestly stay empty.
    pub fn with_store(mut self, store: Arc<Store>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn artifact_dir(&self, artifact_id: &EntityId) -> PathBuf {
        self.root.join(artifact_id.to_string())
    }

    fn versions_dir(&self, artifact_id: &EntityId) -> PathBuf {
        self.artifact_dir(artifact_id).join("versions")
    }

    fn version_path(&self, artifact_id: &EntityId, revision: u64) -> PathBuf {
        self.versions_dir(artifact_id).join(revision.to_string())
    }

    fn actions_dir(&self, artifact_id: &EntityId) -> PathBuf {
        self.artifact_dir(artifact_id).join("actions")
    }

    fn action_path(&self, artifact_id: &EntityId, request_id: &str) -> PathBuf {
        self.actions_dir(artifact_id).join(format!("{request_id}.json"))
    }

    fn meta_path(&self, artifact_id: &EntityId) -> PathBuf {
        self.artifact_dir(artifact_id).join("meta.json")
    }

    /// Create or replace an artifact's bytes. Revision is monotonic per
    /// artifact; every committed revision is kept under `versions/`.
    ///
    /// Ordering: the new revision file is fully written before `meta.json`
    /// points at it, so a crash mid-write either leaves the old meta (old
    /// truth) or a meta whose revision file exists (new truth). Orphaned
    /// revision files from a mid-write crash are harmless and are
    /// overwritten if that revision number is reached again.
    pub fn put(
        &self,
        artifact_id: EntityId,
        title: String,
        mime: String,
        bytes: &[u8],
    ) -> io::Result<ArtifactRecord> {
        // Hold the write lock across the file writes so concurrent PUTs cannot
        // both read the same base revision. This is sync I/O, no await inside.
        let mut records = self.records.write();
        let previous = records.get(&artifact_id);
        let revision = previous.map_or(1, |p| p.revision + 1);
        let backend = previous.and_then(|p| p.backend.clone());

        let version_path = self.version_path(&artifact_id, revision);
        atomic_write(&version_path, bytes)?;

        let record = ArtifactRecord {
            artifact_id,
            title,
            mime,
            bytes_path: version_path,
            revision,
            restored_from: None,
            updated_at: Utc::now(),
            backend,
        };
        self.save_meta(&record)?;

        records.insert(artifact_id, record.clone());
        Ok(record)
    }

    /// Persist `meta.json` atomically.
    fn save_meta(&self, record: &ArtifactRecord) -> io::Result<()> {
        let meta = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        atomic_write(&self.meta_path(&record.artifact_id), &meta)
    }

    /// Persist an action record atomically.
    fn save_action(&self, record: &ActionRecord) -> io::Result<()> {
        let data = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        atomic_write(&self.action_path(&record.artifact_id, &record.request_id), &data)
    }

    /// Claim `(artifact_id, request_id)` durably and exactly once: write the
    /// record to a temp file, then hard-link it into place. The link fails
    /// `AlreadyExists` if another invocation claimed the key first — losing
    /// claims reload the winner's record and dedup on it. A crash can only
    /// leave a complete admitted record or none at all.
    fn claim_action(&self, record: &ActionRecord) -> io::Result<ClaimOutcome> {
        let target = self.action_path(&record.artifact_id, &record.request_id);
        let tmp = self.actions_dir(&record.artifact_id).join(format!(
            ".{}.json.tmp-{}",
            record.request_id,
            Uuid::new_v4()
        ));
        let data = serde_json::to_vec_pretty(record).map_err(io::Error::other)?;
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&data)?;
            file.sync_all()?;
        }
        let claimed = fs::hard_link(&tmp, &target);
        let _ = fs::remove_file(&tmp);
        match claimed {
            Ok(()) => Ok(ClaimOutcome::Claimed),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                match self.load_action(&record.artifact_id, &record.request_id) {
                    Some(existing) => Ok(ClaimOutcome::Existing(existing)),
                    // Present but not yet readable — admission is mid-write.
                    None => Ok(ClaimOutcome::InFlight),
                }
            }
            Err(err) => Err(err),
        }
    }

    /// The dedup contract on a stored admission: identical payload replays
    /// the receipt; different action/input conflicts; a different bound
    /// module is stale.
    fn replay_check(
        existing: &ActionRecord,
        binding: &BackendBinding,
        action: &str,
        input_digest: &Digest,
    ) -> Result<(), ActionReject> {
        if existing.backend_digest != binding.module_digest {
            return Err(ActionReject::Stale);
        }
        if existing.action != action || &existing.input_digest != input_digest {
            return Err(ActionReject::Conflict);
        }
        Ok(())
    }

    fn load_action(&self, artifact_id: &EntityId, request_id: &str) -> Option<ActionRecord> {
        let data = fs::read(self.action_path(artifact_id, request_id)).ok()?;
        match serde_json::from_slice::<ActionRecord>(&data) {
            Ok(record) => Some(record),
            Err(err) => {
                tracing::warn!(%err, request_id, "skipping malformed action record");
                None
            }
        }
    }

    /// All committed revisions of an artifact, ascending.
    pub fn revisions(&self, artifact_id: &EntityId) -> Vec<u64> {
        let mut revisions: Vec<u64> = fs::read_dir(self.versions_dir(artifact_id))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().to_str()?.parse::<u64>().ok())
                    .collect()
            })
            .unwrap_or_default();
        revisions.sort_unstable();
        revisions
    }

    /// Restore an exact earlier revision as a NEW revision. History is kept:
    /// `restored_from` records the source and no version file is erased.
    /// Returns `Ok(None)` when the artifact or revision does not exist.
    pub fn rollback(
        &self,
        artifact_id: EntityId,
        revision: u64,
    ) -> io::Result<Option<ArtifactRecord>> {
        let mut records = self.records.write();
        let Some(previous) = records.get(&artifact_id) else {
            return Ok(None);
        };
        let version_path = self.version_path(&artifact_id, revision);
        let bytes = match fs::read(&version_path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let new_revision = previous.revision + 1;
        let new_path = self.version_path(&artifact_id, new_revision);
        atomic_write(&new_path, &bytes)?;

        let record = ArtifactRecord {
            artifact_id,
            title: previous.title.clone(),
            mime: previous.mime.clone(),
            bytes_path: new_path,
            revision: new_revision,
            restored_from: Some(revision),
            updated_at: Utc::now(),
            backend: previous.backend.clone(),
        };
        self.save_meta(&record)?;
        records.insert(artifact_id, record.clone());
        Ok(Some(record))
    }

    pub fn get(&self, artifact_id: &EntityId) -> Option<ArtifactRecord> {
        self.records.read().get(artifact_id).cloned()
    }

    /// Newest first — matches the Playground index ordering.
    pub fn list(&self) -> Vec<ArtifactRecord> {
        let mut items: Vec<_> = self.records.read().values().cloned().collect();
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items
    }

    pub fn read_content(&self, record: &ArtifactRecord) -> io::Result<Vec<u8>> {
        fs::read(&record.bytes_path)
    }

    /// Seal bytes into CAS when the durable store is attached; the artifact
    /// files remain the truth either way, and a failed seal is recorded as
    /// `None` rather than faked.
    fn seal(&self, bytes: &[u8], codec: &str) -> Option<ObjectRef> {
        let store = self.store.as_ref()?;
        match store
            .cas
            .seal_object(bytes, "application/octet-stream", "playground", codec, 1)
        {
            Ok(sealed) => Some(sealed.object),
            Err(err) => {
                tracing::warn!(%err, codec, "playground CAS seal failed");
                None
            }
        }
    }

    /// Bind a WASI backend module (WAT text or WASM binary) to an artifact.
    /// The module is pinned by content digest; actions under an older binding
    /// answer `ARTIFACT_VERSION_STALE`.
    pub fn bind_backend(&self, artifact_id: EntityId, bytes: &[u8]) -> io::Result<ArtifactRecord> {
        if bytes.is_empty() || bytes.len() > MAX_MODULE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "module must be 1 byte..2MiB",
            ));
        }
        let module_format = if bytes.starts_with(b"\0asm") {
            "wasm"
        } else if bytes.starts_with(b"(module") || bytes.starts_with(b";;") || bytes.starts_with(b"(")
        {
            "wat"
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "module is neither WASM binary nor WAT text",
            ));
        };

        let backend_dir = self.artifact_dir(&artifact_id).join("backend");

        let binding = BackendBinding {
            schema_version: BINDING_SCHEMA_VERSION,
            module_digest: Digest::blake3(bytes),
            module_format: module_format.to_string(),
            bound_at: Utc::now(),
            module_ref: self.seal(bytes, "playground-backend-module"),
            contract: backend_contract(),
        };

        let mut records = self.records.write();
        let Some(previous) = records.get(&artifact_id).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "artifact not found",
            ));
        };
        fs::create_dir_all(&backend_dir)?;
        atomic_write(&backend_dir.join("module"), bytes)?;
        let record = ArtifactRecord {
            backend: Some(binding),
            updated_at: Utc::now(),
            ..previous
        };
        self.save_meta(&record)?;
        records.insert(artifact_id, record.clone());
        Ok(record)
    }

    /// Admit and run a bound backend action. Durable before effects: the
    /// admitted record is on disk before the guest starts, so a replay after
    /// a crash returns the stored receipt instead of re-executing.
    pub fn invoke_action(
        &self,
        artifact_id: EntityId,
        request_id: &str,
        action: &str,
        input: Value,
    ) -> Result<ActionRecord, ActionReject> {
        let record = self.get(&artifact_id).ok_or(ActionReject::NotFound)?;
        let Some(binding) = record.backend.clone() else {
            return Err(ActionReject::NoBackend);
        };
        if !safe_token(request_id) {
            return Err(ActionReject::Invalid(
                "request_id must be 1..64 of A-Za-z0-9_-".into(),
            ));
        }
        if !safe_token(action) {
            return Err(ActionReject::Invalid(
                "action must be 1..64 of A-Za-z0-9_-".into(),
            ));
        }
        let input_bytes = serde_json::to_vec(&input)
            .map_err(|e| ActionReject::Invalid(format!("input not serializable: {e}")))?;
        if input_bytes.len() > MAX_INPUT_BYTES {
            return Err(ActionReject::Invalid("input exceeds 64KiB".into()));
        }
        let input_digest = Digest::blake3(&input_bytes);

        // Dedup: the durable record wins over memory and restart.
        if let Some(existing) = self.load_action(&artifact_id, request_id) {
            Self::replay_check(&existing, &binding, action, &input_digest)?;
            return Ok(existing);
        }

        // Admit before any effect exists.
        let actions_dir = self.actions_dir(&artifact_id);
        fs::create_dir_all(&actions_dir).map_err(host_reject)?;
        fs::create_dir_all(self.artifact_dir(&artifact_id).join("state")).map_err(host_reject)?;
        atomic_write(
            &actions_dir.join(format!("{request_id}.request.json")),
            &input_bytes,
        )
        .map_err(host_reject)?;

        let mut admitted = ActionRecord {
            schema_version: ACTION_SCHEMA_VERSION,
            request_id: request_id.to_string(),
            artifact_id,
            artifact_revision: record.revision,
            backend_digest: binding.module_digest.clone(),
            action: action.to_string(),
            input_digest,
            input: Some(input),
            input_ref: self.seal(&input_bytes, "playground-action-input"),
            state: ActionState::Admitted,
            admitted_at: Utc::now(),
            executed_at: None,
            exit_code: None,
            output: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            changed_files: Vec::new(),
            receipt_ref: None,
            error: None,
            inbox_delivered: false,
        };
        match self.claim_action(&admitted).map_err(host_reject)? {
            ClaimOutcome::Claimed => {}
            ClaimOutcome::Existing(existing) => {
                Self::replay_check(&existing, &binding, action, &admitted.input_digest)?;
                return Ok(existing);
            }
            ClaimOutcome::InFlight => return Err(ActionReject::InFlight),
        }

        // Guest + mount state are independent of the async loop.
        let artifact_dir = self.artifact_dir(&artifact_id);
        let module = match fs::read(artifact_dir.join("backend").join("module")) {
            Ok(module) => module,
            Err(err) => return Err(ActionReject::Invalid(format!("module unreadable: {err}"))),
        };
        let stdin_path = actions_dir.join(format!("{request_id}.request.json"));
        let action_owned = action.to_string();
        let request_owned = request_id.to_string();
        let artifact_string = artifact_id.to_string();
        let outcome = tokio::task::spawn_blocking(move || {
            run_backend(
                &artifact_dir,
                &module,
                &action_owned,
                &request_owned,
                &artifact_string,
                &stdin_path,
            )
        })
        .await
        .unwrap_or_else(|err| BackendOutcome {
            state: ActionState::Unknown,
            exit_code: None,
            error: Some(format!("execution thread: {err}")),
            stdout: Vec::new(),
            stderr: Vec::new(),
            output: None,
            changed_files: Vec::new(),
        });

        // Persist the guest's stream bytes as receipt files (bounded already).
        let _ = atomic_write(
            &actions_dir.join(format!("{request_id}.stdout")),
            &outcome.stdout,
        );
        let _ = atomic_write(
            &actions_dir.join(format!("{request_id}.stderr")),
            &outcome.stderr,
        );

        admitted.state = outcome.state;
        admitted.executed_at = Some(Utc::now());
        admitted.exit_code = outcome.exit_code;
        admitted.output = outcome.output;
        admitted.stdout_bytes = outcome.stdout.len() as u64;
        admitted.stderr_bytes = outcome.stderr.len() as u64;
        admitted.changed_files = outcome.changed_files;
        admitted.error = outcome.error;
        admitted.receipt_ref = self.seal(
            &serde_json::to_vec(&admitted).unwrap_or_default(),
            "playground-action-receipt",
        );
        self.save_action(&admitted).map_err(host_reject)?;

        admitted.inbox_delivered = self.notify_action(&admitted).await;
        if admitted.inbox_delivered {
            self.save_action(&admitted).map_err(host_reject)?;
        }
        Ok(admitted)
    }

    /// Notify the subject that a real artifact change happened. SPEC §43.2
    /// requires these events to land in the same subject Inbox as messages.
    pub fn notify_changed(&self, record: &ArtifactRecord) {
        let Some(tx) = &self.inbox_tx else { return };
        let event = InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::ArtifactChanged,
            payload: json!({
                "artifact_id": record.artifact_id.to_string(),
                "title": record.title,
                "mime": record.mime,
                "revision": record.revision,
                "restored_from": record.restored_from,
            }),
            created_at: Utc::now(),
            source: "playground".to_string(),
        };
        // Never block a browser request on the subject; full inbox is logged.
        if let Err(err) = tx.try_send(event) {
            tracing::warn!(?err, artifact_id=%record.artifact_id, "playground inbox event dropped");
        }
    }

    /// Artifact action events carry change references (changed state files,
    /// outcome, output presence) so a non-text change can be explained or
    /// referenced. Unlike PUT notifications they are awaited for a bounded
    /// window: the subject must not silently lose a state change it caused.
    /// Returns whether the event actually reached the inbox.
    async fn notify_action(&self, record: &ActionRecord) -> bool {
        let Some(tx) = &self.inbox_tx else { return false };
        let event = InboxEvent {
            event_id: EntityId::new(),
            kind: InboxEventKind::ArtifactChanged,
            payload: json!({
                "artifact_id": record.artifact_id.to_string(),
                "revision": record.artifact_revision,
                "action": record.action,
                "request_id": record.request_id,
                "backend_digest": record.backend_digest,
                "action_state": record.state.as_str(),
                "exit_code": record.exit_code,
                "changed_files": record.changed_files,
                "has_output": record.output.is_some(),
            }),
            created_at: Utc::now(),
            source: "playground".to_string(),
        };
        match tokio::time::timeout(ACTION_INBOX_WAIT, tx.send(event)).await {
            Ok(Ok(())) => true,
            Ok(Err(err)) => {
                tracing::warn!(?err, "playground action inbox closed");
                false
            }
            Err(_) => {
                tracing::warn!(request_id=%record.request_id, "playground action inbox full; event dropped after wait");
                false
            }
        }
    }

    fn load_from_disk(&self) {
        let Ok(entries) = fs::read_dir(self.root.as_path()) else {
            return;
        };
        let mut records = self.records.write();
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let Ok(meta) = fs::read(dir.join("meta.json")) else {
                continue;
            };
            let Ok(mut record) = serde_json::from_slice::<ArtifactRecord>(&meta) else {
                tracing::warn!(path=%dir.display(), "skipping malformed playground meta.json");
                continue;
            };
            // Rebind bytes_path to this node's revision file; the meta's stored
            // absolute path may come from another mount. Fall back to a legacy
            // flat `content` file for pre-versioning artifacts.
            let version_path = dir.join("versions").join(record.revision.to_string());
            record.bytes_path = if version_path.exists() {
                version_path
            } else {
                dir.join("content")
            };
            records.insert(record.artifact_id, record);
        }
    }
}

fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn host_reject(err: io::Error) -> ActionReject {
    ActionReject::Invalid(format!("host: {err}"))
}

/// Snapshot of state-file content digests (name → digest) — detects both
/// created and modified files in the guest's state dir. Top-level files
/// only; subdirectories are outside this increment's change reference.
fn state_snapshot(state_dir: &Path) -> HashMap<String, Digest> {
    let mut snapshot = HashMap::new();
    if let Ok(entries) = fs::read_dir(state_dir) {
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else { continue };
            if !file_type.is_file() {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            snapshot.insert(name, Digest::blake3(&bytes));
        }
    }
    snapshot
}

/// Run the bound guest once against its isolated mounts and report only what
/// the host actually observed. No retries, no second attempt on failure.
fn run_backend(
    artifact_dir: &Path,
    module: &[u8],
    action: &str,
    request_id: &str,
    artifact_string: &str,
    stdin_path: &Path,
) -> BackendOutcome {
    let state_dir = artifact_dir.join("state");
    let before = state_snapshot(&state_dir);
    let home = match state_dir.canonicalize() {
        Ok(home) => home,
        Err(err) => {
            return BackendOutcome {
                state: ActionState::Unknown,
                exit_code: None,
                error: Some(format!("state dir: {err}")),
                stdout: Vec::new(),
                stderr: Vec::new(),
                output: None,
                changed_files: Vec::new(),
            };
        }
    };
    let artifact_root = match artifact_dir.canonicalize() {
        Ok(root) => root,
        Err(err) => {
            return BackendOutcome {
                state: ActionState::Unknown,
                exit_code: None,
                error: Some(format!("artifact dir: {err}")),
                stdout: Vec::new(),
                stderr: Vec::new(),
                output: None,
                changed_files: Vec::new(),
            };
        }
    };
    let stdin = match fs::File::open(stdin_path).map(InputFile::new) {
        Ok(stdin) => stdin,
        Err(err) => {
            return BackendOutcome {
                state: ActionState::Unknown,
                exit_code: None,
                error: Some(format!("frozen stdin: {err}")),
                stdout: Vec::new(),
                stderr: Vec::new(),
                output: None,
                changed_files: Vec::new(),
            };
        }
    };
    let stdout_pipe = MemoryOutputPipe::new(GUEST_OUTPUT_CAP);
    let stderr_pipe = MemoryOutputPipe::new(GUEST_OUTPUT_CAP);

    let mut runtime = match WasmRuntime::with_mounts(vec![
        MountSpec::new(home, HOME_GUEST_PATH, MountMode::ReadWrite, true, false),
        MountSpec::new(artifact_root, ARTIFACT_GUEST_PATH, MountMode::ReadOnly, true, true),
    ]) {
        Ok(runtime) => runtime,
        Err(err) => {
            return BackendOutcome {
                state: ActionState::Unknown,
                exit_code: None,
                error: Some(format!("mounts: {err}")),
                stdout: Vec::new(),
                stderr: Vec::new(),
                output: None,
                changed_files: Vec::new(),
            };
        }
    };

    let mut command = WasiCommand::new(stdin, stdout_pipe.clone(), stderr_pipe.clone());
    command.arguments = vec!["playground-backend".into(), action.to_string()];
    command.environment = vec![
        ("SHENKO_ARTIFACT_ID".into(), artifact_string.to_string()),
        ("SHENKO_ACTION".into(), action.to_string()),
        ("SHENKO_REQUEST_ID".into(), request_id.to_string()),
    ];

    let run = runtime.run_wasi_module(module, command);
    let stdout = stdout_pipe.contents().as_ref().to_vec();
    let stderr = stderr_pipe.contents().as_ref().to_vec();
    let output = serde_json::from_slice::<Value>(&stdout).ok();

    // Changed = created OR content-modified files in the guest's state dir.
    let after = state_snapshot(&state_dir);
    let changed_files: Vec<String> = after
        .iter()
        .filter(|(name, digest)| before.get(*name) != Some(*digest))
        .map(|(name, _)| (*name).clone())
        .collect();

    match run {
        Ok(0) => BackendOutcome {
            state: ActionState::Executed,
            exit_code: Some(0),
            error: None,
            stdout,
            stderr,
            output,
            changed_files,
        },
        Ok(code) => BackendOutcome {
            state: ActionState::Failed,
            exit_code: Some(code),
            error: Some(format!("guest exited {code}")),
            stdout,
            stderr,
            output,
            changed_files,
        },
        Err(err) => BackendOutcome {
            state: ActionState::Unknown,
            exit_code: None,
            error: Some(format!("{err}")),
            stdout,
            stderr,
            output,
            changed_files,
        },
    }
}

/// Playground routes, merged into the main axum router.
pub fn router(state: PlaygroundState) -> Router {
    Router::new()
        .route("/playground", get(list_handler))
        .route("/playground/{id}", get(get_handler).put(put_handler))
        .route("/playground/{id}/versions", get(versions_handler))
        .route("/playground/{id}/rollback", post(rollback_handler))
        .route(
            "/playground/{id}/backend",
            get(backend_info_handler).put(bind_backend_handler),
        )
        .route("/playground/{id}/backend/action", post(backend_action_handler))
        .with_state(state)
}

/// Write `bytes` to `path` atomically: temp file in the same directory, then
/// rename. A crash leaves either the old or the new content, never a partial.
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "artifact".to_string());
    let tmp = path.with_file_name(format!(".{file_name}.tmp-{}", Uuid::new_v4()));

    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

async fn list_handler(State(pg): State<PlaygroundState>) -> Html<String> {
    Html(render_list(&pg.list()))
}

async fn get_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(record) = pg.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    let bytes = match pg.read_content(&record) {
        Ok(bytes) => bytes,
        Err(err) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("artifact content unreadable: {err}"),
            );
        }
    };

    let mut response = Response::new(Body::from(bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&record.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(value) = HeaderValue::from_str(&record.revision.to_string()) {
        headers.insert("x-artifact-revision", value);
    }
    if let Ok(value) = HeaderValue::from_str(&record.updated_at.to_rfc3339()) {
        headers.insert("x-artifact-updated-at", value);
    }
    response
}

async fn put_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let title = query
        .get("title")
        .cloned()
        .or_else(|| header_str(&headers, "x-artifact-title"))
        .unwrap_or_else(|| format!("Artifact {id}"));
    let mime = query
        .get("mime")
        .cloned()
        .or_else(|| header_str(&headers, header::CONTENT_TYPE.as_str()))
        .unwrap_or_else(|| "application/octet-stream".to_string());

    match pg.put(id, title, mime, &body) {
        Ok(record) => {
            pg.notify_changed(&record);
            (
                StatusCode::OK,
                Json(json!({"ok": true, "artifact": record})),
            )
                .into_response()
        }
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("artifact write failed: {err}"),
        ),
    }
}

async fn versions_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(record) = pg.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    Json(json!({
        "ok": true,
        "artifact_id": id.to_string(),
        "revision": record.revision,
        "restored_from": record.restored_from,
        "revisions": pg.revisions(&id),
    }))
    .into_response()
}

async fn rollback_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let request = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| json!({}));
    let revision = request.get("revision").and_then(Value::as_u64);
    let Some(revision) = revision else {
        return error_response(StatusCode::BAD_REQUEST, "body needs revision (u64)");
    };

    match pg.rollback(id, revision) {
        Ok(Some(record)) => {
            pg.notify_changed(&record);
            (StatusCode::OK, Json(json!({"ok": true, "artifact": record}))).into_response()
        }
        Ok(None) => error_response(
            StatusCode::NOT_FOUND,
            "artifact or revision not found",
        ),
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("rollback failed: {err}"),
        ),
    }
}

async fn backend_info_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(record) = pg.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };
    Json(json!({"ok": true, "backend": record.backend})).into_response()
}

/// Bind the WASI backend module. Raw WAT text or WASM binary bytes.
async fn bind_backend_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    match pg.bind_backend(id, &body) {
        Ok(record) => {
            pg.notify_changed(&record);
            (StatusCode::OK, Json(json!({"ok": true, "artifact": record}))).into_response()
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            error_response(StatusCode::NOT_FOUND, "artifact not found")
        }
        Err(err) if err.kind() == io::ErrorKind::InvalidInput => {
            error_response(StatusCode::UNPROCESSABLE_ENTITY, &err.to_string())
        }
        Err(err) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("backend bind failed: {err}"),
        ),
    }
}

/// The real action path: durable admission → WASI guest → persistent receipt.
/// `ok` mirrors whether the guest actually executed; the receipt is the
/// truth, not the HTTP code.
async fn backend_action_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let request = match serde_json::from_slice::<Value>(&body) {
        Ok(request) if request.is_object() => request,
        _ => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "body needs {request_id, action, input?}",
            )
        }
    };
    let request_id = request.get("request_id").and_then(Value::as_str);
    let action = request.get("action").and_then(Value::as_str);
    let (Some(request_id), Some(action)) = (request_id, action) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "request_id and action are required strings",
        );
    };
    let input = request.get("input").cloned().unwrap_or(Value::Null);

    match pg.invoke_action(id, request_id, action, input).await {
        Ok(record) => Json(json!({
            "ok": record.state == ActionState::Executed,
            "action": record,
        }))
        .into_response(),
        Err(reject) => {
            let (status, code, message) = match reject {
                ActionReject::NotFound => (
                    StatusCode::NOT_FOUND,
                    "ARTIFACT_NOT_FOUND",
                    "artifact not found".into(),
                ),
                ActionReject::InFlight => (
                    StatusCode::CONFLICT,
                    "ACTION_IN_FLIGHT",
                    "this request_id is being admitted; retry with the same payload".into(),
                ),
                ActionReject::Invalid(message) => {
                    (StatusCode::UNPROCESSABLE_ENTITY, "ACTION_INVALID", message)
                }
                ActionReject::Conflict => (
                    StatusCode::CONFLICT,
                    "ACTION_CONFLICT",
                    "request_id replayed with a different action or input".into(),
                ),
                ActionReject::Stale => (
                    StatusCode::CONFLICT,
                    "ARTIFACT_VERSION_STALE",
                    "request_id was admitted under a different bound module".into(),
                ),
                ActionReject::NoBackend => (
                    StatusCode::CONFLICT,
                    "NO_BACKEND",
                    "artifact has no bound backend or does not exist".into(),
                ),
            };
            (
                status,
                Json(json!({"ok": false, "code": code, "error": message})),
            )
                .into_response()
        }
    }
}

fn parse_artifact_id(raw: &str) -> Result<EntityId, Response> {
    raw.parse::<EntityId>()
        .map_err(|_| error_response(StatusCode::BAD_REQUEST, "invalid artifact id"))
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"ok": false, "error": message}))).into_response()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok().map(str::to_string))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Index page. Tokens mirror `src/index.html` so the Playground reads as part
/// of the same Shenko surface rather than a second design system.
fn render_list(records: &[ArtifactRecord]) -> String {
    let body = if records.is_empty() {
        "<p class=\"empty\">还没有作品。通过 <code>PUT /playground/&lt;id&gt;</code> 创建一个。</p>"
            .to_string()
    } else {
        let mut items = String::new();
        for record in records {
            let id = record.artifact_id.to_string();
            let backend = if record.backend.is_some() { " · backend" } else { "" };
            items.push_str(&format!(
                "<li><a href=\"/playground/{id}\">{title}</a>\
                 <span class=\"meta\">rev {revision} · {mime}{backend} · {updated}</span></li>",
                id = id,
                title = escape_html(&record.title),
                revision = record.revision,
                mime = escape_html(&record.mime),
                backend = backend,
                updated = record
                    .updated_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ));
        }
        format!("<ul>{items}</ul>")
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1,viewport-fit=cover">
<title>Playground · Shenko</title>
<style>
  :root {{
    --bg:          oklch(13% 0.005 250);
    --bg-raised:   oklch(17% 0.008 250);
    --border:      oklch(28% 0.012 250);
    --text:        oklch(92% 0.006 250);
    --text-muted:  oklch(58% 0.012 250);
    --accent:      oklch(68% 0.19  250);
    --radius-md:   10px;
    --space-3:     12px;
    --space-4:     16px;
    --space-5:     20px;
  }}
  @media (prefers-color-scheme: light) {{
    :root {{
      --bg:          oklch(97% 0.003 250);
      --bg-raised:   oklch(100% 0 0);
      --border:      oklch(84% 0.010 250);
      --text:        oklch(15% 0.010 250);
      --text-muted:  oklch(44% 0.015 250);
      --accent:      oklch(52% 0.21  250);
    }}
  }}
  * {{ box-sizing: border-box; margin: 0; padding: 0; }}
  body {{
    background: var(--bg);
    color: var(--text);
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", "PingFang SC", sans-serif;
    font-size: 15px;
    line-height: 1.6;
    min-height: 100dvh;
    padding: var(--space-5);
    max-width: 820px;
    margin: 0 auto;
  }}
  header {{
    display: flex;
    align-items: baseline;
    gap: var(--space-3);
    padding-bottom: var(--space-4);
    border-bottom: 1px solid var(--border);
    margin-bottom: var(--space-5);
  }}
  h1 {{ font-size: 18px; font-weight: 600; letter-spacing: -0.01em; }}
  header a {{ color: var(--text-muted); font-size: 13px; text-decoration: none; }}
  header a:hover {{ color: var(--accent); }}
  ul {{ list-style: none; display: flex; flex-direction: column; gap: var(--space-3); }}
  li {{
    background: var(--bg-raised);
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: var(--space-3) var(--space-4);
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: var(--space-3);
  }}
  li a {{ color: var(--accent); text-decoration: none; font-weight: 600; }}
  li a:hover {{ text-decoration: underline; }}
  .meta {{ color: var(--text-muted); font-size: 12px; }}
  .empty {{ color: var(--text-muted); font-size: 14px; }}
  code {{ font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }}
</style>
</head>
<body>
  <header>
    <h1>Playground</h1>
    <a href="/">← 时间线</a>
  </header>
  {body}
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "shenko-playground-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    /// Real WASI guest fixture: writes `state.txt` into its RW /home and a
    /// structured JSON receipt to stdout — exercises the whole backend
    /// contract (preopens, state mutation, bounded structured output).
    const BACKEND_WAT: &str = r#"
        (module
          (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
          (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
          (memory (export "memory") 1)
          (data (i32.const 200) "state.txt")
          (data (i32.const 240) "guest-data")
          (data (i32.const 260) "{\"ok\":true,\"actor\":\"guest\"}")
          (func (export "_start")
            ;; write state.txt under /home (preopen fd 3)
            (i32.store (i32.const 300) (i32.const 240))
            (i32.store (i32.const 304) (i32.const 10))
            (if (i32.ne
                  (call $path_open
                    (i32.const 3) (i32.const 0) (i32.const 200) (i32.const 9)
                    (i32.const 9) (i64.const 64) (i64.const 0)
                    (i32.const 0) (i32.const 312))
                  (i32.const 0))
              (then (call $exit (i32.const 51))))
            (if (i32.ne
                  (call $fd_write (i32.load (i32.const 312)) (i32.const 300)
                                  (i32.const 1) (i32.const 320))
                  (i32.const 0))
              (then (call $exit (i32.const 52))))
            ;; bounded JSON receipt on stdout
            (i32.store (i32.const 300) (i32.const 260))
            (i32.store (i32.const 304) (i32.const 32))
            (if (i32.ne
                  (call $fd_write (i32.const 1) (i32.const 300) (i32.const 1)
                                  (i32.const 320))
                  (i32.const 0))
              (then (call $exit (i32.const 53))))
            (call $exit (i32.const 0))))
    "#;

    #[test]
    fn registry_atomic_write_and_revision_bump() {
        let root = temp_root("registry");
        let pg = PlaygroundState::new(root.clone());
        let id = EntityId::new();

        let first = pg
            .put(id, "First".into(), "text/plain".into(), b"hello")
            .unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(fs::read(&first.bytes_path).unwrap(), b"hello");
        assert_eq!(pg.list().len(), 1);

        let second = pg
            .put(id, "Second".into(), "text/html".into(), b"<h1>hi</h1>")
            .unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(fs::read(&second.bytes_path).unwrap(), b"<h1>hi</h1>");

        // meta.json is the durable truth and reloads after a restart.
        let meta: ArtifactRecord =
            serde_json::from_slice(&fs::read(root.join(id.to_string()).join("meta.json")).unwrap())
                .unwrap();
        assert_eq!(meta.revision, 2);
        assert_eq!(meta.title, "Second");
        assert_eq!(meta.mime, "text/html");

        let reloaded = PlaygroundState::new(root.clone());
        let record = reloaded.get(&id).expect("record survives restart");
        assert_eq!(record.revision, 2);
        assert_eq!(record.title, "Second");
        assert_eq!(reloaded.read_content(&record).unwrap(), b"<h1>hi</h1>");

        // Atomic writes leave no temp files behind.
        let leftovers: Vec<_> = fs::read_dir(root.join(id.to_string()))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files remain: {leftovers:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn versions_survive_partial_write_and_rollback() {
        let root = temp_root("versions");
        let pg = PlaygroundState::new(root.clone());
        let id = EntityId::new();

        pg.put(id, "a".into(), "text/plain".into(), b"rev-one").unwrap();
        pg.put(id, "b".into(), "text/plain".into(), b"rev-two").unwrap();
        assert_eq!(pg.revisions(&id), vec![1, 2]);

        // Crash shape: an orphaned revision file exists while meta still
        // points at rev 2 — reopening must not roll forward or lose rev 1/2.
        atomic_write(&pg.version_path(&id, 3), b"orphan").unwrap();
        let reloaded = PlaygroundState::new(root.clone());
        let record = reloaded.get(&id).unwrap();
        assert_eq!(record.revision, 2, "orphan rev file must not roll forward");
        assert_eq!(reloaded.read_content(&record).unwrap(), b"rev-two");
        // rev 3 exists on disk but is orphaned; next put reuses it cleanly.
        let third = reloaded.put(id, "c".into(), "text/plain".into(), b"rev-three").unwrap();
        assert_eq!(third.revision, 3);
        assert_eq!(fs::read(&third.bytes_path).unwrap(), b"rev-three");

        // Rollback restores the exact earlier bytes as a new revision and
        // keeps every version file.
        let restored = reloaded.rollback(id, 1).unwrap().unwrap();
        assert_eq!(restored.revision, 4);
        assert_eq!(restored.restored_from, Some(1));
        assert_eq!(reloaded.read_content(&restored).unwrap(), b"rev-one");
        assert_eq!(reloaded.revisions(&id), vec![1, 2, 3, 4]);
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn backend_binding_and_real_wasi_action() {
        let root = temp_root("backend");
        let pg = PlaygroundState::new(root.clone());
        let id = EntityId::new();
        pg.put(id, "game".into(), "text/html".into(), b"<h1>x</h1>").unwrap();

        let bound = pg.bind_backend(id, BACKEND_WAT.as_bytes()).unwrap();
        let binding = bound.backend.as_ref().unwrap();
        assert_eq!(binding.module_format, "wat");
        assert_eq!(binding.module_digest, Digest::blake3(BACKEND_WAT.as_bytes()));

        let record = pg
            .invoke_action(id, "req-1", "tick", json!({"n": 1}))
            .await
            .expect("action admitted");
        assert_eq!(record.state, ActionState::Executed);
        assert_eq!(record.exit_code, Some(0));
        assert_eq!(record.output, Some(json!({"ok": true, "actor": "guest"})));
        assert_eq!(record.changed_files, vec!["state.txt".to_string()]);
        // The guest's own state mutation persisted in its /home mount.
        assert_eq!(
            fs::read_to_string(
                root.join(id.to_string()).join("state").join("state.txt")
            )
            .unwrap(),
            "guest-data"
        );
        // Durable admission files exist on disk.
        assert!(pg.action_path(&id, "req-1").exists());
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn action_replay_serves_receipt_and_conflict_is_rejected() {
        let root = temp_root("dedup");
        let pg = PlaygroundState::new(root.clone());
        let id = EntityId::new();
        pg.put(id, "game".into(), "text/html".into(), b"<h1>x</h1>").unwrap();
        pg.bind_backend(id, BACKEND_WAT.as_bytes()).unwrap();

        let first = pg.invoke_action(id, "req-9", "tick", json!({"n": 1})).await.unwrap();
        let replay = pg.invoke_action(id, "req-9", "tick", json!({"n": 1})).await.unwrap();
        assert_eq!(replay.state, first.state);
        assert_eq!(replay.admitted_at, first.admitted_at, "replay must serve the original receipt");
        assert_eq!(replay.executed_at, first.executed_at, "replay must not re-execute");

        // Same key + different payload: conflict, not a second execution.
        let conflict = pg.invoke_action(id, "req-9", "tick", json!({"n": 2})).await;
        assert!(matches!(conflict, Err(ActionReject::Conflict)));

        // Rebind a different module: old request_id is stale, never re-armed.
        let other_wat = BACKEND_WAT.replace("guest-data", "guest-datb");
        pg.bind_backend(id, other_wat.as_bytes()).unwrap();
        let stale = pg.invoke_action(id, "req-9", "tick", json!({"n": 1})).await;
        assert!(matches!(stale, Err(ActionReject::Stale)));

        // No backend at all.
        let bare = EntityId::new();
        pg.put(bare, "bare".into(), "text/plain".into(), b"x").unwrap();
        let no_backend = pg.invoke_action(bare, "req-1", "tick", json!(null)).await;
        assert!(matches!(no_backend, Err(ActionReject::NoBackend)));
        fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn action_failure_and_unknown_are_terminal_not_replayed() {
        let root = temp_root("failure");
        let pg = PlaygroundState::new(root.clone());
        let id = EntityId::new();
        pg.put(id, "game".into(), "text/html".into(), b"<h1>x</h1>").unwrap();
        pg.bind_backend(id, BACKEND_WAT.as_bytes()).unwrap();

        // Guest exits nonzero -> failed receipt, replay does not re-execute.
        let failing = BACKEND_WAT.replace("call $exit (i32.const 0)", "call $exit (i32.const 4)");
        pg.bind_backend(id, failing.as_bytes()).unwrap();
        let failed = pg.invoke_action(id, "req-f", "tick", json!(null)).await.unwrap();
        assert_eq!(failed.state, ActionState::Failed);
        assert_eq!(failed.exit_code, Some(4));
        let again = pg.invoke_action(id, "req-f", "tick", json!(null)).await.unwrap();
        assert_eq!(again.executed_at, failed.executed_at);

        // Module that never instantiates -> unknown, terminal, not re-armed.
        pg.bind_backend(id, b"(module (func (export \"_start\") unreachable))")
            .unwrap();
        let unknown = pg.invoke_action(id, "req-u", "tick", json!(null)).await.unwrap();
        assert_eq!(unknown.state, ActionState::Unknown);
        assert!(unknown.error.is_some());
        fs::remove_dir_all(&root).ok();
    }
}
