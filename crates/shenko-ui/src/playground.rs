//! Versioned local artifacts and their durable subject inputs (SPEC §43.2).
//! Content and version records are immutable; only `meta.json` selects a head.
//! A version is written and synced before the head changes. Its stable inbox
//! fact is recoverable from that record if the process stops before SQL import.
//! Backend actions use the existing native task/operation ledger, not these files.

use std::{collections::HashMap, fs, io::{self, Write}, path::{Path, PathBuf}, sync::Arc};
use axum::{
    body::{Body, Bytes}, extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Json, Response}, routing::{get, post}, Router,
};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_store::Store;
use shenko_subject::{NativeChildService, InboxEvent, InboxEventKind};
use shenko_types::{ids::EntityId, object::{Digest, ObjectRef}};
use tokio::sync::mpsc::Sender;
use uuid::Uuid;
use crate::playground_backend::{ActionError, BackendActionRequest, BackendBinding, BackendReceipt};

const ARTIFACT_SCHEMA: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub schema_version: u32,
    pub artifact_id: EntityId,
    pub title: String,
    pub mime: String,
    pub bytes_path: PathBuf,
    pub content_ref: ObjectRef,
    pub revision: u64,
    pub parent_revision: Option<u64>,
    pub restored_from: Option<u64>,
    pub updated_at: DateTime<Utc>,
    pub backend: Option<BackendBinding>,
}

#[derive(Clone)]
pub struct PlaygroundState {
    pub(crate) root: Arc<PathBuf>,
    pub(crate) store: Arc<Store>,
    pub(crate) agent_id: EntityId,
    pub(crate) native: Option<Arc<NativeChildService>>,
    inbox_tx: Option<Sender<InboxEvent>>,
    records: Arc<RwLock<HashMap<EntityId, ArtifactRecord>>>,
}

impl PlaygroundState {
    /// Open the existing artifact tree and recover unimported facts from its
    /// committed version chain. No Store/subject means no writable Playground.
    pub fn open(root: PathBuf, store: Arc<Store>) -> anyhow::Result<Self> {
        let agent_id = store.get_existing_agent_id()?
            .ok_or_else(|| anyhow::anyhow!("Playground requires an existing subject"))?;
        store.load_subject(agent_id)?;
        fs::create_dir_all(&root)?;
        let state = Self {
            root: Arc::new(root.canonicalize()?), store, agent_id,
            native: None, inbox_tx: None,
            records: Arc::new(RwLock::new(HashMap::new())),
        };
        state.load()?;
        state.recover_notifications()?;
        Ok(state)
    }

    pub fn with_inbox(mut self, tx: Sender<InboxEvent>) -> Self {
        self.inbox_tx = Some(tx);
        self
    }

    pub fn with_native(mut self, native: Arc<NativeChildService>) -> Self {
        self.native = Some(native);
        self
    }

    pub fn root(&self) -> &Path { self.root.as_path() }

    pub(crate) fn artifact_dir(&self, id: EntityId) -> PathBuf {
        self.root.join(id.to_string())
    }

    fn version_path(&self, id: EntityId, revision: u64) -> PathBuf {
        self.artifact_dir(id).join("versions").join(revision.to_string())
    }

    fn version_record_path(&self, id: EntityId, revision: u64) -> PathBuf {
        self.artifact_dir(id).join("versions").join(format!("{revision}.json"))
    }

    fn seal_content(&self, bytes: &[u8], mime: &str) -> io::Result<ObjectRef> {
        match self.store.cas.seal_object(bytes, mime, "playground", "artifact-content", 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => { self.store.close_external_writer(); Err(io::Error::other(error)) }
        }
    }

    /// Allocate beyond orphaned writes too: a crash never authorizes erasing
    /// an old version's bytes, even when it never became the selected head.
    fn next_revision(&self, id: EntityId) -> io::Result<u64> {
        let dir = self.artifact_dir(id).join("versions");
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(1),
            Err(e) => return Err(e),
        };
        let mut highest = 0u64;
        for entry in entries {
            let name = entry?.file_name();
            if let Some(number) = name.to_str().and_then(|s| s.trim_end_matches(".json").parse::<u64>().ok()) {
                highest = highest.max(number);
            }
        }
        highest.checked_add(1).ok_or_else(|| io::Error::other("artifact revision overflow"))
    }

    /// Commit immutable bytes + record before selecting the new head. The
    /// committed record is also the artifact's stable notification outbox.
    fn commit_version(&self, record: &ArtifactRecord, bytes: &[u8]) -> io::Result<()> {
        write_immutable(&self.version_path(record.artifact_id, record.revision), bytes)?;
        write_immutable(&self.version_record_path(record.artifact_id, record.revision),
            &serde_json::to_vec(record).map_err(io::Error::other)?)?;
        atomic_write(&self.artifact_dir(record.artifact_id).join("meta.json"),
            &serde_json::to_vec(record).map_err(io::Error::other)?)
    }

    pub fn put(&self, id: EntityId, title: String, mime: String, bytes: &[u8]) -> io::Result<ArtifactRecord> {
        let mut records = self.records.write();
        let previous = records.get(&id);
        let revision = self.next_revision(id)?;
        let record = ArtifactRecord {
            schema_version: ARTIFACT_SCHEMA, artifact_id: id, title, mime: mime.clone(),
            bytes_path: self.version_path(id, revision), content_ref: self.seal_content(bytes, &mime)?,
            revision, parent_revision: previous.map(|r| r.revision), restored_from: None,
            updated_at: Utc::now(), backend: previous.and_then(|r| r.backend.clone()),
        };
        self.commit_version(&record, bytes)?;
        records.insert(id, record.clone());
        // A failure here does not undo the committed artifact. The caller sees
        // an error; reopen/import retries this SAME stable fact, not the write.
        self.notify_changed(&record).map_err(io::Error::other)?;
        Ok(record)
    }

    pub fn get(&self, id: &EntityId) -> Option<ArtifactRecord> {
        self.records.read().get(id).cloned()
    }

    pub fn list(&self) -> Vec<ArtifactRecord> {
        let mut records: Vec<_> = self.records.read().values().cloned().collect();
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        records
    }

    pub fn read_content(&self, record: &ArtifactRecord) -> io::Result<Vec<u8>> {
        let bytes = fs::read(self.version_path(record.artifact_id, record.revision))?;
        if bytes.len() as u64 != record.content_ref.bytes || Digest::blake3(&bytes) != record.content_ref.digest {
            return Err(io::Error::other("artifact version content digest mismatch"));
        }
        Ok(bytes)
    }

    pub(crate) fn read_version(&self, id: EntityId, revision: u64) -> io::Result<ArtifactRecord> {
        let mut record: ArtifactRecord = serde_json::from_slice(
            &fs::read(self.version_record_path(id, revision))?).map_err(io::Error::other)?;
        if record.schema_version != ARTIFACT_SCHEMA || record.artifact_id != id || record.revision != revision {
            return Err(io::Error::other("artifact version identity/schema mismatch"));
        }
        record.bytes_path = self.version_path(id, revision);
        Ok(record)
    }

    /// Only the committed ancestor chain is history; unrelated orphan writes
    /// remain on disk but are not silently published as earlier versions.
    pub fn revisions(&self, id: EntityId) -> io::Result<Vec<u64>> {
        let Some(head) = self.get(&id) else { return Ok(Vec::new()) };
        let mut next = Some(head.revision);
        let mut revisions = Vec::new();
        while let Some(revision) = next {
            let record = self.read_version(id, revision)?;
            if record.parent_revision.is_some_and(|parent| parent >= revision) {
                return Err(io::Error::other("artifact history is not monotonic"));
            }
            revisions.push(revision);
            next = record.parent_revision;
        }
        revisions.reverse();
        Ok(revisions)
    }

    /// Restore the exact earlier content, MIME, title AND executable binding
    /// as a new version. Business state lives separately and is never erased.
    pub fn rollback(&self, id: EntityId, source_revision: u64) -> io::Result<ArtifactRecord> {
        if !self.revisions(id)?.contains(&source_revision) {
            return Err(io::Error::new(io::ErrorKind::NotFound, "committed artifact revision not found"));
        }
        let mut records = self.records.write();
        let head = records.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact not found"))?;
        let source = self.read_version(id, source_revision)?;
        let bytes = self.read_content(&source)?;
        let revision = self.next_revision(id)?;
        let record = ArtifactRecord {
            revision, parent_revision: Some(head.revision), restored_from: Some(source_revision),
            updated_at: Utc::now(), bytes_path: self.version_path(id, revision), ..source
        };
        self.commit_version(&record, &bytes)?;
        records.insert(id, record.clone());
        self.notify_changed(&record).map_err(io::Error::other)?;
        Ok(record)
    }

    /// Every binding is a new artifact version, including its invocation
    /// contract and CAS module identity. A code rollback restores this binding
    /// while keeping all state written by newer code.
    pub fn bind_backend(&self, id: EntityId, module: &[u8]) -> io::Result<ArtifactRecord> {
        let binding = BackendBinding::seal(&self.store, module).map_err(io::Error::other)?;
        let mut records = self.records.write();
        let previous = records.get(&id).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "artifact not found"))?;
        let bytes = self.read_content(previous)?;
        let revision = self.next_revision(id)?;
        let record = ArtifactRecord {
            revision, parent_revision: Some(previous.revision), restored_from: None,
            updated_at: Utc::now(), bytes_path: self.version_path(id, revision),
            backend: Some(binding), ..previous.clone()
        };
        self.commit_version(&record, &bytes)?;
        records.insert(id, record.clone());
        self.notify_changed(&record).map_err(io::Error::other)?;
        Ok(record)
    }

    fn change_event(&self, record: &ArtifactRecord) -> InboxEvent {
        let backend = record.backend.as_ref().map(|binding| &binding.module_ref);
        InboxEvent {
            event_id: stable_id(&format!("artifact-change/v2:{}:{}", record.artifact_id, record.revision)),
            kind: InboxEventKind::ArtifactChanged, created_at: record.updated_at, source: "playground".into(),
            payload: json!({
                "artifact_id": record.artifact_id, "revision": record.revision,
                "title": record.title, "mime": record.mime,
                "parent_revision": record.parent_revision, "restored_from": record.restored_from,
                "content_digest": record.content_ref.digest, "content_ref": record.content_ref,
                "backend_module_ref": backend,
                "explanation": format!("Artifact '{}' committed revision {} ({} bytes, {}). Exact content and executable references are attached; binary content is not treated as text.",
                    record.title, record.revision, record.content_ref.bytes, record.mime),
            }),
        }
    }

    pub fn notify_changed(&self, record: &ArtifactRecord) -> anyhow::Result<()> {
        let event = self.change_event(record);
        self.store.admit_inbox_event(event.event_id, self.agent_id, "artifact_changed", "playground",
            &serde_json::to_value(&event)?)?;
        // This is only a wake. C11's pending_inbox_events drains the already
        // committed full InboxEvent if the queue is full/closed or absent.
        if let Some(tx) = &self.inbox_tx { let _ = tx.try_send(event); }
        Ok(())
    }

    fn recover_notifications(&self) -> anyhow::Result<()> {
        for artifact in self.list() {
            for revision in self.revisions(artifact.artifact_id)? {
                self.notify_changed(&self.read_version(artifact.artifact_id, revision)?)?;
            }
        }
        Ok(())
    }

    fn load(&self) -> anyhow::Result<()> {
        for entry in fs::read_dir(self.root.as_path())? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() { continue; }
            let Some(id) = entry.file_name().to_str().and_then(|name| name.parse::<EntityId>().ok()) else { continue };
            let meta = entry.path().join("meta.json");
            let raw = match fs::read(&meta) {
                Ok(bytes) => bytes,
                // A content write before first head selection is an orphan,
                // not an artifact to invent during recovery.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let value: Value = serde_json::from_slice(&raw)?;
            let record = if value.get("schema_version").and_then(Value::as_u64) == Some(ARTIFACT_SCHEMA as u64) {
                let mut head: ArtifactRecord = serde_json::from_value(value)?;
                head.bytes_path = self.version_path(id, head.revision);
                let immutable = self.read_version(id, head.revision)?;
                anyhow::ensure!(head.artifact_id == id && serde_json::to_value(&head)? == serde_json::to_value(&immutable)?,
                    "artifact head differs from immutable version");
                self.read_content(&immutable)?;
                immutable
            } else {
                // Explicit one-way migration of the original flat content /
                // meta.json layout. Preserve the exact old metadata and content
                // files; do not execute its old reserved backend_script field.
                #[derive(Deserialize)]
                struct Legacy { artifact_id: EntityId, title: String, mime: String, revision: u64, updated_at: DateTime<Utc> }
                let legacy: Legacy = serde_json::from_slice(&raw)?;
                anyhow::ensure!(legacy.artifact_id == id, "legacy artifact identity mismatch");
                let bytes = fs::read(entry.path().join("content"))?;
                let record = ArtifactRecord {
                    schema_version: ARTIFACT_SCHEMA, artifact_id: id,
                    title: legacy.title, mime: legacy.mime.clone(), revision: legacy.revision,
                    content_ref: self.seal_content(&bytes, &legacy.mime)?, bytes_path: self.version_path(id, legacy.revision),
                    parent_revision: None, restored_from: None, updated_at: legacy.updated_at, backend: None,
                };
                write_immutable(&entry.path().join("legacy-meta.json"), &raw)?;
                self.commit_version(&record, &bytes)?;
                record
            };
            self.records.write().insert(id, record);
        }
        Ok(())
    }
}

pub(crate) fn stable_id(identity: &str) -> EntityId {
    let hash = blake3::hash(identity.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    EntityId(Uuid::from_bytes(bytes))
}

fn write_immutable(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("version parent missing"))?;
    fs::create_dir_all(parent)?;
    match fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => { file.write_all(bytes)?; file.sync_all()?; }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            if fs::read(path)? != bytes { return Err(io::Error::other("immutable artifact version conflict")); }
        }
        Err(e) => return Err(e),
    }
    fs::File::open(parent)?.sync_all()?;
    if let Some(grandparent) = parent.parent() { fs::File::open(grandparent)?.sync_all()?; }
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("artifact parent missing"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".artifact-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() { let _ = fs::remove_file(&tmp); }
    result
}

pub fn router(state: PlaygroundState) -> Router {
    Router::new()
        .route("/playground", get(list_handler))
        .route("/playground/{id}", get(get_handler).put(put_handler))
        .route("/playground/{id}/versions", get(versions_handler))
        .route("/playground/{id}/rollback", post(rollback_handler))
        .route("/playground/{id}/backend", get(backend_info_handler).put(bind_backend_handler))
        .route("/playground/{id}/backend/action", post(backend_action_handler).get(action_status_handler))
        .with_state(state)
}

async fn list_handler(State(pg): State<PlaygroundState>) -> Html<String> {
    Html(render_list(&pg.list()))
}

async fn get_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    let Some(record) = pg.get(&id) else { return error_response(StatusCode::NOT_FOUND, "artifact not found") };
    match pg.read_content(&record) {
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&record.mime)
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
            if let Ok(value) = HeaderValue::from_str(&record.revision.to_string()) { response.headers_mut().insert("x-artifact-revision", value); }
            response
        }
    }
}

async fn put_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Query(query): Query<HashMap<String, String>>, headers: HeaderMap, bytes: Bytes) -> Response {
    let title = query.get("title").cloned().or_else(|| header_str(&headers, "x-artifact-title"))
        .unwrap_or_else(|| format!("Artifact {id}"));
    let mime = query.get("mime").cloned().or_else(|| header_str(&headers, header::CONTENT_TYPE.as_str()))
        .unwrap_or_else(|| "application/octet-stream".into());
    artifact_response(pg.put(id, title, mime, &bytes))
}

async fn versions_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.revisions(id) {
        Ok(revisions) if revisions.is_empty() => error_response(StatusCode::NOT_FOUND, "artifact not found"),
        Ok(revisions) => Json(json!({"ok": true, "revisions": revisions})).into_response(),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackRequest { revision: u64 }
async fn rollback_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>, Json(request): Json<RollbackRequest>) -> Response {
    artifact_response(pg.rollback(id, request.revision))
}

async fn backend_info_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>) -> Response {
    match pg.get(&id) {
        Some(record) => Json(json!({"ok": true, "revision": record.revision, "backend": record.backend})).into_response(),
        None => error_response(StatusCode::NOT_FOUND, "artifact not found"),
    }
}

async fn bind_backend_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>, bytes: Bytes) -> Response {
    artifact_response(pg.bind_backend(id, &bytes))
}

async fn backend_action_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Json(request): Json<BackendActionRequest>) -> Response {
    receipt_response(pg.invoke_action(id, request))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusQuery { request_id: String }
async fn action_status_handler(State(pg): State<PlaygroundState>, AxumPath(id): AxumPath<EntityId>,
    Query(query): Query<StatusQuery>) -> Response {
    receipt_response(pg.action_status(id, &query.request_id))
}

fn receipt_response(receipt: Result<BackendReceipt, ActionError>) -> Response {
    match receipt {
        Ok(receipt) => {
            let status = if receipt.result.is_some() || receipt.evidence.is_some() { StatusCode::OK } else { StatusCode::ACCEPTED };
            (status, Json(json!({"accepted": true, "receipt": receipt}))).into_response()
        }
        Err(error) => error.into_response(),
    }
}

fn artifact_response(result: io::Result<ArtifactRecord>) -> Response {
    match result {
        Ok(record) => Json(json!({"ok": true, "notified": "durably_admitted", "artifact": record})).into_response(),
        Err(e) => error_response(if e.kind() == io::ErrorKind::NotFound { StatusCode::NOT_FOUND } else { StatusCode::INTERNAL_SERVER_ERROR },
            &format!("artifact write/import failed; any committed version is retained for recovery: {e}")),
    }
}

fn error_response(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({"ok": false, "error": error}))).into_response()
}
fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}
fn escape_html(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
fn render_list(records: &[ArtifactRecord]) -> String {
    let mut body = String::from("<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Playground · Shenko</title><link rel=\"stylesheet\" href=\"/assets/app.css\"><main><h1>Playground</h1><a href=\"/\">← 时间线</a><ul>");
    for record in records {
        body.push_str(&format!("<li><a href=\"/playground/{}\">{}</a> — rev {} · {}</li>",
            record.artifact_id, escape_html(&record.title), record.revision, escape_html(&record.mime)));
    }
    if records.is_empty() { body.push_str("<li>还没有作品。</li>"); }
    body.push_str("</ul></main></html>");
    body
}

#[cfg(test)]
#[path = "playground_tests.rs"]
mod tests;
