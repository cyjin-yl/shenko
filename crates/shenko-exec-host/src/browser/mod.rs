//! Native browser provider: real engines, real persistent identities, real
//! live sessions (SPEC §24.6, §19.1, §41).
//!
//! What this module is:
//! - A real browser process (an installed Chromium-family or Firefox-family
//!   engine) launched against a real persistent profile directory, bound to
//!   its own CDP or WebDriver BiDi client.
//! - Several isolated persistent identities (`profile_id`), each with its own
//!   profile directory, its own cookies/localStorage and its own single write
//!   owner. One profile is never opened by two writers: the writer lock is
//!   checked across processes and nodes before a browser starts.
//! - A live session with a controller epoch and an input lease, so a human
//!   can take over the same live session and the agent's late input is
//!   rejected instead of racing the human.
//! - Fresh observation and action receipts bound to the durable Store
//!   operation/attempt/evidence contract.
//!
//! What this module deliberately does NOT claim:
//! - A reopened profile is NOT live page continuity. Live tabs, in-page
//!   memory and open WebSockets are never restored; `RestoreCoverage` says
//!   so in every session receipt.
//! - A delivery ACK is not an application-level effect. A click that was
//!   delivered but whose effect cannot be seen is recorded as unknown, and
//!   the next step observes before it acts.
//! - Nothing is ever replayed after a crash. Recovery re-opens the profile
//!   (a NEW run identity) and reconciles the original operation as unknown
//!   unless the live session can still prove the outcome.
mod actions;
pub mod engine;
pub mod handlers;
pub mod protocol;
pub mod protocol_bidi;
pub mod protocol_cdp;
pub mod scripts;
mod session;

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

use engine::{EngineFamily, EngineSpec, SessionMode, VirtualDisplay};
use protocol::{
    AxNode, BrowserProtocol, Cookie, PageTarget, RestoreCoverage, TabHandle, Viewport,
};

/// The provider's contract digest, bound into every operation contract.
pub const CONTRACT: &[u8] = b"shenko.browser/v1: real installed browser engine (CDP or WebDriver BiDi) with isolated persistent profile identities, a live session with controller epoch and input lease, fresh observation/action receipts, and no blind replay after a crash";
/// Longest accepted profile id.
const MAX_PROFILE_ID: usize = 64;
/// Longest accepted display mode / engine name.
const MAX_NAME: usize = 64;
/// Longest accepted URL.
const MAX_URL: usize = 4096;
/// Longest accepted action list.
const MAX_ACTIONS: usize = 32;
/// How long an observation stays usable for a bound action.
const OBSERVATION_VALIDITY_MS: i64 = 120_000;

// ---------------------------------------------------------------------------
// Public request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequest {
    /// "auto" (default) prefers virtual-headed and degrades to headless only
    /// when the node has no display capability; "virtual_headed" and
    /// "headless" are explicit.
    pub mode: Option<String>,
    /// Engine family or binary path/name; "auto" discovers installed engines.
    pub engine: Option<String>,
    /// The persistent identity to use. A missing identity is created.
    pub profile_id: Option<String>,
    /// The URL of the first tab.
    pub url: Option<String>,
    /// A tab group label inside this session.
    pub group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFacts {
    pub session_id: EntityId,
    pub profile_id: String,
    /// A new run identity: a restart or a crash produces a different one.
    pub run_id: String,
    pub engine_name: String,
    pub engine_version: String,
    pub engine_path: String,
    pub engine_family: String,
    pub protocol: String,
    pub product: String,
    pub mode_requested: String,
    pub mode_effective: String,
    /// The reason the effective mode differs from the request, when it does.
    pub mode_degradation: Option<String>,
    pub display: Option<String>,
    /// The shared display domain id (`x11/<endpoint>/root<hex>`), when the
    /// session runs on a real display.
    pub display_domain_id: Option<String>,
    /// The control endpoint with its secret path removed.
    pub endpoint: String,
    pub tabs: Vec<TabFacts>,
    pub controller_epoch: u64,
    pub restore: RestoreCoverage,
    pub opened_at: String,
    pub writer_owner: WriterFacts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabFacts {
    pub tab_id: String,
    pub group: String,
    pub url: String,
    pub title: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriterFacts {
    pub instance: EntityId,
    pub node: String,
    pub pid: u32,
    pub acquired_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObserveOptions {
    /// Capture a PNG screenshot of the page as well.
    pub screenshot: bool,
    /// Ask the engine for its accessibility tree (CDP only).
    pub accessibility: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    /// Content-bound id of this observation; an action must carry it.
    pub observation_id: String,
    pub session_id: EntityId,
    pub profile_id: String,
    pub tab_id: String,
    pub group: String,
    pub url: String,
    pub title: String,
    pub ready_state: String,
    pub revision: u64,
    pub captured_at: String,
    pub viewport: Viewport,
    pub text: String,
    pub text_truncated: bool,
    pub links: Vec<PageTarget>,
    pub inputs: Vec<PageTarget>,
    pub buttons: Vec<PageTarget>,
    pub ax_nodes: Option<Vec<AxNode>>,
    pub ax_available: bool,
    pub ax_note: Option<String>,
    /// Digest over the full observation content.
    pub content_digest: String,
    /// Digest over the page identity (url/title/structure). An action is
    /// rejected when this changed since the observation.
    pub identity_digest: String,
    /// The controller epoch in force when the observation was taken.
    pub controller_epoch: u64,
    pub valid_for_ms: i64,
    pub screenshot_digest: Option<String>,
    pub restore: RestoreCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRequest {
    pub session_id: EntityId,
    pub tab_id: Option<String>,
    /// The observation the action is bound to.
    pub observation_id: String,
    /// The input lease token. A revoked epoch rejects the action.
    pub lease_token: EntityId,
    pub actions: Vec<PageAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReport {
    pub session_id: EntityId,
    pub tab_id: String,
    pub observation_id: String,
    pub controller_epoch: u64,
    pub receipts: Vec<ActionReceipt>,
    /// The fresh observation taken after the batch, so the next step acts on
    /// a page it actually saw.
    pub after: Option<Observation>,
    pub effect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReceipt {
    pub index: usize,
    pub kind: String,
    /// "acked" | "rejected" | "failed" | "unknown"
    pub transport: String,
    /// "none" | "partial" | "complete" | "unknown"
    pub effect: String,
    pub detail: Option<String>,
    /// What was actually observed after the action, when it could be seen.
    pub observed_effect: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControllerRole {
    Agent,
    Human,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    pub session_id: EntityId,
    /// "acquire" | "release" | "takeover" | "hand_back" | "status"
    pub op: String,
    pub role: Option<ControllerRole>,
    pub lease_token: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlFacts {
    pub session_id: EntityId,
    pub controller_epoch: u64,
    pub holder: Option<String>,
    pub role: Option<ControllerRole>,
    pub lease_token: Option<EntityId>,
    pub acquired_at: Option<String>,
    pub display_domain_id: Option<String>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityRequest {
    /// "create" | "list" | "describe" | "export_state" | "import_state"
    pub op: String,
    pub profile_id: Option<String>,
    pub engine: Option<String>,
    pub session_id: Option<EntityId>,
    pub tab_id: Option<String>,
    /// For "import_state": the exported storage state document.
    pub state: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityFacts {
    pub profile_id: String,
    pub directory: String,
    pub engine_family: String,
    pub created_at: String,
    pub writer: Option<WriterFacts>,
    pub live_sessions: Vec<EntityId>,
    pub storage_state: Option<StorageState>,
    pub note: String,
}

/// The weaker, origin-scoped persistence form. It is NOT live page
/// continuity and is never described as such.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageState {
    pub origin: String,
    pub cookies: Vec<Cookie>,
    pub local_storage: serde_json::Map<String, serde_json::Value>,
    pub exported_at: String,
    pub note: String,
}

/// One typed page action.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PageAction {
    Navigate {
        url: String,
    },
    Click {
        selector: Option<String>,
        text: Option<String>,
    },
    Fill {
        selector: String,
        value: String,
        #[serde(default)]
        submit: bool,
    },
    Type {
        text: String,
    },
    Press {
        key: String,
    },
    Scroll {
        dy: f64,
    },
    WaitFor {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Evaluate {
        script: String,
    },
    Screenshot,
}

impl PageAction {
    pub fn kind(&self) -> &'static str {
        match self {
            PageAction::Navigate { .. } => "navigate",
            PageAction::Click { .. } => "click",
            PageAction::Fill { .. } => "fill",
            PageAction::Type { .. } => "type",
            PageAction::Press { .. } => "press",
            PageAction::Scroll { .. } => "scroll",
            PageAction::WaitFor { .. } => "wait_for",
            PageAction::Evaluate { .. } => "evaluate",
            PageAction::Screenshot => "screenshot",
        }
    }
}

// ---------------------------------------------------------------------------
// Internal state
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct WriterRecord {
    instance: EntityId,
    node: String,
    pid: u32,
    session_id: EntityId,
    acquired_at: String,
}

struct Identity {
    profile_id: String,
    directory: PathBuf,
    family: EngineFamily,
    created_at: String,
    writer: Option<WriterRecord>,
}

struct Lease {
    token: EntityId,
    holder: EntityId,
    role: ControllerRole,
    epoch: u64,
    acquired_at: String,
}

#[derive(Clone)]
struct TabEntry {
    handle: TabHandle,
    group: String,
    url: String,
    title: String,
    revision: u64,
}

struct BrowserSession {
    session_id: EntityId,
    profile_id: String,
    run_id: String,
    engine: EngineSpec,
    mode_requested: String,
    mode_effective: SessionMode,
    mode_degradation: Option<String>,
    display: Option<String>,
    domain_id: Option<String>,
    endpoint: String,
    product: String,
    protocol: Arc<dyn BrowserProtocol>,
    process: AsyncMutex<Option<tokio::process::Child>>,
    display_handle: AsyncMutex<Option<VirtualDisplay>>,
    epoch: AtomicU64,
    lease: AsyncMutex<Option<Lease>>,
    tabs: AsyncMutex<BTreeMap<String, TabEntry>>,
    /// The latest observation per tab; an action must bind to one of these.
    observations: AsyncMutex<BTreeMap<String, Observation>>,
    closed: AtomicBool,
}

#[derive(Default)]
struct ProviderState {
    identities: BTreeMap<String, Identity>,
    sessions: BTreeMap<EntityId, Arc<BrowserSession>>,
}

/// The live browser backend. One instance is shared by every browser native
/// task kind, so the session registry, the identity registry and the write
/// lease are one truth.
pub struct BrowserProvider {
    store: Arc<Store>,
    root: PathBuf,
    state: Mutex<ProviderState>,
}

impl BrowserProvider {
    /// `root` holds the identity directories, the identity metadata and the
    /// writer locks. It is created on demand.
    pub fn new(store: Arc<Store>, root: PathBuf) -> Self {
        Self {
            store,
            root,
            state: Mutex::new(ProviderState::default()),
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    fn identities_dir(&self) -> PathBuf {
        self.root.join("identities")
    }

    fn locks_dir(&self) -> PathBuf {
        self.root.join("locks")
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ProviderState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // ------------------------------------------------------------------
    // Identities
    // ------------------------------------------------------------------

    /// Validate a profile id. The id is used as a directory name, so it is
    /// restricted to characters that cannot escape the identity directory.
    fn validate_profile_id(profile_id: &str) -> Result<()> {
        ensure!(
            !profile_id.is_empty() && profile_id.len() <= MAX_PROFILE_ID,
            "profile_id must be 1..={MAX_PROFILE_ID} characters"
        );
        ensure!(
            profile_id
                .chars()
                .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-' || character == '_'),
            "profile_id may only contain lowercase ascii letters, digits, '-' and '_'"
        );
        ensure!(
            profile_id.starts_with(|character: char| character.is_ascii_lowercase() || character.is_ascii_digit()),
            "profile_id must start with a lowercase letter or a digit"
        );
        Ok(())
    }

    fn profile_dir(&self, profile_id: &str) -> PathBuf {
        self.profiles_dir().join(profile_id)
    }

    fn identity_meta_path(&self, profile_id: &str) -> PathBuf {
        self.identities_dir().join(format!("{profile_id}.json"))
    }

    fn lock_path(&self, profile_id: &str) -> PathBuf {
        self.locks_dir().join(format!("{profile_id}.lock"))
    }

    /// Create the persistent identity: its own profile directory and its
    /// metadata record. The directory is the real browser profile; nothing
    /// here is a stand-in for one.
    pub fn create_identity(&self, profile_id: &str, family: EngineFamily) -> Result<IdentityFacts> {
        Self::validate_profile_id(profile_id)?;
        std::fs::create_dir_all(self.profiles_dir())?;
        std::fs::create_dir_all(self.identities_dir())?;
        std::fs::create_dir_all(self.locks_dir())?;
        let directory = self.profile_dir(profile_id);
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("create browser profile directory {}", directory.display()))?;
        let created_at = chrono::Utc::now().to_rfc3339();
        let meta = serde_json::json!({
            "profile_id": profile_id,
            "engine_family": match family {
                EngineFamily::Chromium => "chromium",
                EngineFamily::Firefox => "firefox",
            },
            "created_at": created_at,
            "directory": directory.to_string_lossy(),
            "persistent_state": "the browser's own profile directory (cookies, localStorage, cache)",
            "not_restored": "live tabs, in-page memory and open websockets are not restored by reopening a profile",
        });
        let meta_path = self.identity_meta_path(profile_id);
        let tmp = meta_path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&meta)?)?;
        std::fs::rename(&tmp, &meta_path)?;
        let mut state = self.state();
        state
            .identities
            .entry(profile_id.to_string())
            .or_insert_with(|| Identity {
                profile_id: profile_id.to_string(),
                directory: directory.clone(),
                family,
                created_at: created_at.clone(),
                writer: None,
            });
        Ok(IdentityFacts {
            profile_id: profile_id.to_string(),
            directory: directory.to_string_lossy().to_string(),
            engine_family: match family {
                EngineFamily::Chromium => "chromium",
                EngineFamily::Firefox => "firefox",
            }
            .into(),
            created_at,
            writer: None,
            live_sessions: vec![],
            storage_state: None,
            note: "persistent identity: the browser's own profile directory; separate identities have separate cookies and storage".into(),
        })
    }

    /// List every known identity.
    pub fn list_identities(&self) -> Vec<IdentityFacts> {
        let state = self.state();
        state
            .identities
            .values()
            .map(|identity| IdentityFacts {
                profile_id: identity.profile_id.clone(),
                directory: identity.directory.to_string_lossy().to_string(),
                engine_family: match identity.family {
                    EngineFamily::Chromium => "chromium",
                    EngineFamily::Firefox => "firefox",
                }
                .into(),
                created_at: identity.created_at.clone(),
                writer: identity.writer.as_ref().map(writer_facts),
                live_sessions: state
                    .sessions
                    .values()
                    .filter(|session| session.profile_id == identity.profile_id)
                    .map(|session| session.session_id)
                    .collect(),
                storage_state: None,
                note: "persistent identity: the browser's own profile directory".into(),
            })
            .collect()
    }

    // ------------------------------------------------------------------
    // Writer ownership (one writer per profile)
    // ------------------------------------------------------------------

    /// Take the single write ownership of a profile. The lock file records
    /// the owner so a later process on another node can see that the profile
    /// is already being written, and can tell a live owner from a dead one.
    fn acquire_writer(
        &self,
        profile_id: &str,
        instance: EntityId,
        node: &str,
        session_id: EntityId,
    ) -> Result<WriterFacts> {
        std::fs::create_dir_all(self.locks_dir())?;
        let path = self.lock_path(profile_id);
        let record = WriterRecord {
            instance,
            node: node.to_string(),
            pid: std::process::id(),
            session_id,
            acquired_at: chrono::Utc::now().to_rfc3339(),
        };
        let payload = serde_json::to_vec(&record)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(&payload)?;
                return Ok(writer_facts(&record));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(anyhow!("cannot create the profile writer lock: {error}"));
            }
        }
        // The lock exists. A dead owner does not keep the profile closed.
        let existing = std::fs::read(&path).unwrap_or_default();
        let owner: Option<WriterRecord> = serde_json::from_slice(&existing).ok();
        match &owner {
            Some(owner) if process_alive(owner.pid) => {
                return Err(anyhow!(
                    "profile {profile_id} is already written by instance {} (pid {}) on {}; \
                     one profile has exactly one write owner",
                    owner.instance,
                    owner.pid,
                    owner.node
                ));
            }
            _ => {}
        }
        // Stale lock: take it over and record the fact.
        std::fs::write(&path, &payload)?;
        Ok(writer_facts(&record))
    }

    fn release_writer(&self, profile_id: &str, instance: EntityId) {
        let path = self.lock_path(profile_id);
        let existing = std::fs::read(&path).unwrap_or_default();
        if let Ok(record) = serde_json::from_slice::<WriterRecord>(&existing) {
            if record.instance != instance {
                return;
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Is a process still alive? Used to tell a live profile writer from a
/// crashed one.
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

fn writer_facts(record: &WriterRecord) -> WriterFacts {
    WriterFacts {
        instance: record.instance,
        node: record.node.clone(),
        pid: record.pid,
        acquired_at: record.acquired_at.clone(),
    }
}

/// Remove the secret path of a control endpoint. The endpoint path grants
/// full control of the browser, so durable receipts keep only the local
/// address and the port.
pub fn redact_endpoint(endpoint: &str) -> String {
    match endpoint.find("://") {
        Some(index) => {
            let (scheme, rest) = endpoint.split_at(index + 3);
            let (authority, _) = rest.split_once('/').unwrap_or((rest, ""));
            format!("{scheme}{authority}/<redacted>")
        }
        None => endpoint.to_string(),
    }
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn digest_hex(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}
