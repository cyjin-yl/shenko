//! Persistent Playground — SPEC §43.2 (minimal first increment).
//!
//! Artifacts live under `SHENKO_DATA_DIR/playground/<artifact_id>/`:
//! `content` holds the raw bytes and `meta.json` holds the [`ArtifactRecord`].
//! Writes are atomic (temp file + rename) and bump `revision`, so closing the
//! browser, compacting the subject or restarting Shenko never loses a work.
//!
//! Every successful `PUT` emits an [`InboxEvent`] with kind
//! [`InboxEventKind::ArtifactChanged`] and `source: "playground"`, so changes
//! reach the one subject inbox the model already reads from.
//!
//! TODO(§43.2): real artifact backends are not implemented yet — no
//! RuntimeProvider execution, action schemas, dedup receipts or persistent
//! business state. `POST /playground/{id}/backend` only echoes the request.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
use serde_json::json;
use tokio::sync::mpsc::Sender;
use uuid::Uuid;

use shenko_subject::inbox::{InboxEvent, InboxEventKind};
use shenko_types::ids::EntityId;

/// One mounted Playground artifact. Subset of SPEC §43.2 `ArtifactRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub artifact_id: EntityId,
    pub title: String,
    pub mime: String,
    /// Absolute path of the artifact's raw bytes on this node.
    pub bytes_path: PathBuf,
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
    /// Reserved for the real backend runtime. Not executed yet. TODO(§43.2)
    pub backend_script: Option<String>,
}

/// Filesystem-backed artifact registry shared by the Playground routes.
///
/// The map is an in-memory index; `meta.json` is the durable truth. Startup
/// reloads the index from disk so artifacts survive restarts (SPEC §43.2).
#[derive(Clone)]
pub struct PlaygroundState {
    root: Arc<PathBuf>,
    inbox_tx: Option<Arc<Sender<InboxEvent>>>,
    records: Arc<RwLock<HashMap<EntityId, ArtifactRecord>>>,
}

impl PlaygroundState {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let state = Self {
            root: Arc::new(root.into()),
            inbox_tx: None,
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

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Create or replace an artifact's bytes. Revision is monotonic per artifact.
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
        let revision = records
            .get(&artifact_id)
            .map_or(1, |previous| previous.revision + 1);
        let backend_script = records
            .get(&artifact_id)
            .and_then(|previous| previous.backend_script.clone());

        let dir = self.root.join(artifact_id.to_string());
        fs::create_dir_all(&dir)?;
        let content_path = dir.join("content");
        atomic_write(&content_path, bytes)?;

        let record = ArtifactRecord {
            artifact_id,
            title,
            mime,
            bytes_path: content_path,
            revision,
            updated_at: Utc::now(),
            backend_script,
        };
        let meta = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
        atomic_write(&dir.join("meta.json"), &meta)?;

        records.insert(artifact_id, record.clone());
        Ok(record)
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
            }),
            created_at: Utc::now(),
            source: "playground".to_string(),
        };
        // Never block a browser request on the subject; full inbox is logged.
        if let Err(err) = tx.try_send(event) {
            tracing::warn!(?err, artifact_id=%record.artifact_id, "playground inbox event dropped");
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
            // The stored path may come from another mount; rebind locally.
            record.bytes_path = dir.join("content");
            records.insert(record.artifact_id, record);
        }
    }
}

/// Playground routes, merged into the main axum router.
pub fn router(state: PlaygroundState) -> Router {
    Router::new()
        .route("/playground", get(list_handler))
        .route("/playground/{id}", get(get_handler).put(put_handler))
        .route("/playground/{id}/backend", post(backend_handler))
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

/// Minimal backend endpoint. A stored `backend_script` is NOT executed yet.
/// TODO(§43.2): run it through a registered RuntimeProvider with real action
/// schemas, dedup receipts and persistent state; return unknown on failure.
async fn backend_handler(
    State(pg): State<PlaygroundState>,
    AxumPath(raw_id): AxumPath<String>,
    body: Bytes,
) -> Response {
    let id = match parse_artifact_id(&raw_id) {
        Ok(id) => id,
        Err(response) => return response,
    };
    let Some(record) = pg.get(&id) else {
        return error_response(StatusCode::NOT_FOUND, "artifact not found");
    };

    let request = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice::<serde_json::Value>(&body)
            .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&body)}))
    };

    Json(json!({
        "ok": true,
        "echo": {
            "artifact_id": record.artifact_id.to_string(),
            "revision": record.revision,
            "backend_script_present": record.backend_script.is_some(),
            "request": request,
        }
    }))
    .into_response()
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
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
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
            items.push_str(&format!(
                "<li><a href=\"/playground/{id}\">{title}</a>\
                 <span class=\"meta\">rev {revision} · {mime} · {updated}</span></li>",
                id = id,
                title = escape_html(&record.title),
                revision = record.revision,
                mime = escape_html(&record.mime),
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
            .put(
                id,
                "Second".into(),
                "text/html".into(),
                b"<h1>hi</h1>",
            )
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
}
