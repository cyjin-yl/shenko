//! Durable history and window chain over the session store (feature `store`).
//!
//! `StoreHistory` reads committed `session_events`; `StoreWindowLog` keeps the
//! committed window chain in `context_windows`.
//!
//! Versioned raw-session payload:
//! `event_type` is one of system / user / assistant / assistant_cancelled /
//! tool_call / tool_result; `data` is JSON `{text, call_id?, tool?, args_ref?,
//! operation_id?, intent_id?, v?}`; payload status claims are not read;
//! `v` above [`PAYLOAD_VERSION`] keeps the raw data as Other. A call or result
//! without execution evidence is Unknown, never success; one without `call_id` cannot be
//! paired and is kept as [`ItemKind::Other`]; any other event type is kept as
//! Other too. Non-JSON `data` becomes the text. Nothing is dropped.
//! When `operation_id` is present the execution ledger is authoritative:
//! a referenced call's status comes from its OperationRecord. Subject intent ids join
//! the real memory receipt or task/operation/T2 records of this branch.
//! A referenced operation that is missing, unparsable or unreadable as a
//! record reads as Unknown, never as the event's own claim.
use std::sync::Arc;

use serde::Deserialize;
use shenko_store::{
    ContextWindowAppend, Store, StoredContextWindow, StoredDerivative, StoredSessionEvent,
};
use shenko_types::ids::EntityId;

use crate::history::{CallStatus, HistoryItem, HistorySource, ItemKind, ItemRef, StorageError};
use crate::notes::{
    MAX_NOTE_BYTES, NoteRevision, NoteSource, NotesError, NotesJournal, RolloverJournal,
    RolloverRequest,
};
use crate::policy::{Derivative, WINDOW_BODY_VERSION, WindowBase};
use crate::window::{CommittedWindow, WindowAppend, WindowLog};

#[derive(Default, Deserialize)]
struct Payload {
    /// Payload schema version; absent means the unversioned v1 shape.
    v: Option<u32>,
    #[serde(default)]
    text: String,
    call_id: Option<String>,
    tool: Option<String>,
    args_ref: Option<String>,
    operation_id: Option<String>,
    intent_id: Option<String>,
}

/// Highest session event payload version this reader interprets.
pub const PAYLOAD_VERSION: u32 = 1;

/// Counter chosen by the caller; an estimate is not a measured tokenizer.
pub type TokenCounter = Arc<dyn Fn(&str) -> u32 + Send + Sync>;

pub struct StoreHistory {
    store: Arc<Store>,
    session_id: EntityId,
    count: TokenCounter,
}

impl StoreHistory {
    pub fn new(store: Arc<Store>, session_id: EntityId, count: TokenCounter) -> Self {
        Self { store, session_id, count }
    }

    fn item(&self, event: StoredSessionEvent) -> Result<HistoryItem, StorageError> {
        let seq = u64::try_from(event.seq)
            .map_err(|_| StorageError(format!("event {} has negative seq", event.event_id)))?;
        let payload = serde_json::from_str::<Payload>(&event.data)
            .unwrap_or_else(|_| Payload { text: event.data.clone(), ..Payload::default() });
        // A newer payload shape is not guessed at with this reader: keep the
        // raw data verbatim under a versioned Other kind.
        if let Some(v) = payload.v.filter(|v| *v > PAYLOAD_VERSION) {
            return Ok(HistoryItem {
                at: ItemRef { seq, event_id: event.event_id },
                kind: ItemKind::Other { event_type: format!("{}@v{v}", event.event_type), role: event.role },
                tokens: (self.count)(&event.data),
                text: event.data,
            });
        }
        let call_status = match payload.intent_id.as_deref() {
            Some(id) => match id.parse::<EntityId>() {
                Ok(id) => self.store.subject_call_evidence(self.session_id, id).map_err(storage)?
                    .map_or(CallStatus::Unknown, |facts| subject_status(id, &facts)),
                Err(_) => CallStatus::Unknown,
            },
            None => match payload.operation_id.as_deref() {
            None => CallStatus::Unknown,
            Some(id) => match id.parse::<EntityId>().ok().map(|id| self.store.get_operation(id)) {
                Some(Ok(Some(op))) => CallStatus::from_operation(
                    op.state,
                    op.outcome,
                    op.outcome_conflict,
                    op.effect_status,
                ),
                _ => CallStatus::Unknown,
            },
            },
        };
        let kind = match (event.event_type.as_str(), payload.call_id) {
            ("system", _) => ItemKind::System,
            ("user", _) => ItemKind::User,
            ("assistant", _) => ItemKind::Assistant { cancelled: false },
            ("assistant_cancelled", _) => ItemKind::Assistant { cancelled: true },
            ("tool_call", Some(call_id)) => ItemKind::ToolCall {
                call_id,
                tool: payload.tool.unwrap_or_default(),
                args_ref: payload.args_ref.unwrap_or_default(),
                status: call_status,
            },
            ("tool_result", Some(call_id)) => ItemKind::ToolResult { call_id, status: call_status },
            (other, _) => ItemKind::Other { event_type: other.to_owned(), role: event.role },
        };
        let text = if matches!(kind, ItemKind::Other { .. }) { event.data } else { payload.text };
        let tokens = (self.count)(&text);
        Ok(HistoryItem {
            at: ItemRef { seq, event_id: event.event_id },
            kind,
            text,
            tokens,
        })
    }
}

fn subject_status(id: EntityId, facts: &shenko_store::intents::SubjectCallEvidence) -> CallStatus {
    if matches!(facts.dispatch_state.as_str(), "unknown" | "blocked") {
        return CallStatus::Unknown;
    }
    if facts.kind == "memory" {
        return match (facts.dispatch_state.as_str(), facts.memory_state.as_deref()) {
            ("settled", Some("committed")) => CallStatus::Executed { ok: true },
            (_, Some("accepted")) => CallStatus::Pending,
            _ => CallStatus::Unknown,
        };
    }
    let Some(operation) = &facts.operation else {
        return if facts.task_state.is_some() { CallStatus::Pending } else { CallStatus::Unknown };
    };
    if operation.task_id != id {
        return CallStatus::Unknown;
    }
    let status = CallStatus::from_operation(operation.state, operation.outcome,
        operation.outcome_conflict, operation.effect_status);
    if !status.is_resolved() {
        return status;
    }
    // A finished operation is not yet a finished native task: T2 also waits
    // for owned children. A queue handoff alone is never a successful call.
    let terminal = facts.task_state.as_deref() == Some("terminal");
    let completed = match facts.kind.as_str() {
        "delegation" => terminal && facts.task_result.as_ref().is_some_and(|result|
            result.task_id == id && result.operation_id == operation.operation_id
                && Some(result.task_revision.0) == facts.task_revision),
        "publication" => terminal && facts.dispatch_state == "settled",
        _ => false,
    };
    if completed { status } else { CallStatus::Pending }
}

fn storage(error: impl std::fmt::Display) -> StorageError {
    StorageError(error.to_string())
}

impl HistorySource for StoreHistory {
    fn read_range(&self, from: u64, to: u64) -> Result<Vec<HistoryItem>, StorageError> {
        let to = i64::try_from(to).unwrap_or(i64::MAX);
        let from = i64::try_from(from).map_err(storage)?;
        self.store
            .session_events_range(self.session_id, from, to)
            .map_err(storage)?
            .into_iter()
            .map(|event| self.item(event))
            .collect()
    }

    fn read_event(&self, event_id: EntityId) -> Result<Option<HistoryItem>, StorageError> {
        self.store
            .session_event(event_id)
            .map_err(storage)?
            .filter(|event| event.session_id == self.session_id)
            .map(|event| self.item(event))
            .transpose()
    }

    fn leaf_seq(&self) -> Result<Option<u64>, StorageError> {
        self.store
            .session_leaf_seq(self.session_id)
            .map_err(storage)?
            .map(|seq| u64::try_from(seq).map_err(storage))
            .transpose()
    }
}

/// WindowLog over the store's `context_windows` chain for one scope. The
/// body is the committed window as JSON without derivative bytes; frames are
/// stored beside it in the same transaction and verified on read.
pub struct StoreWindowLog {
    store: Arc<Store>,
    scope_id: EntityId,
}

/// Stored derivative identity without the payload. `bytes` is the durable
/// byte length the store recorded; `blake3` is the digest the commit-time
/// writer verified, not a value recomputed by this reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivativeMeta {
    pub media_type: String,
    pub blake3: String,
    pub bytes: u64,
}

/// A committed window's durable facts without derivative payloads. Every
/// non-payload field of [`CommittedWindow`] is preserved verbatim:
/// `window.derivatives` is empty by construction and `derivatives` carries
/// the per-frame identity the store recorded.
#[derive(Debug, Clone)]
pub struct CommittedWindowMeta {
    /// Decoded committed body; `derivatives` is always empty here.
    pub window: CommittedWindow,
    pub derivatives: Vec<DerivativeMeta>,
}

/// One bounded page of committed window metadata, newest generation first.
#[derive(Debug, Clone)]
pub struct WindowMetaPage {
    /// Windows committed under the scope, regardless of this page's bounds.
    pub total: u64,
    /// More committed windows exist below this page's oldest generation.
    pub has_older: bool,
    pub windows: Vec<CommittedWindowMeta>,
}

impl StoreWindowLog {
    pub fn new(store: Arc<Store>, scope_id: EntityId) -> Self {
        Self { store, scope_id }
    }

    fn encode(window: &CommittedWindow) -> Result<StoredContextWindow, StorageError> {
        let body = CommittedWindow { derivatives: Vec::new(), ..window.clone() };
        Ok(StoredContextWindow {
            window_id: window.id,
            generation: i64::try_from(window.generation).map_err(storage)?,
            body: serde_json::to_string(&body).map_err(storage)?,
            derivatives: window
                .derivatives
                .iter()
                .map(|d| StoredDerivative {
                    media_type: d.media_type.clone(),
                    blake3: d.blake3.clone(),
                    bytes: d.bytes.clone(),
                })
                .collect(),
        })
    }

    fn decode(stored: StoredContextWindow) -> Result<CommittedWindow, StorageError> {
        let mut window: CommittedWindow = serde_json::from_str(&stored.body).map_err(storage)?;
        if window.id != stored.window_id {
            return Err(storage(format!("window row {} holds body of {}", stored.window_id, window.id)));
        }
        if window.body_version > WINDOW_BODY_VERSION {
            return Err(storage(format!(
                "window row {} holds body version {}, this reader understands up to {}",
                stored.window_id, window.body_version, WINDOW_BODY_VERSION
            )));
        }
        window.derivatives = stored
            .derivatives
            .into_iter()
            .map(|d| Derivative { media_type: d.media_type, blake3: d.blake3, bytes: d.bytes })
            .collect();
        Ok(window)
    }

    /// Bounded page of committed window metadata, newest generation first:
    /// `before` excludes windows with `generation >= before`; at most `limit`
    /// windows are returned. Unlike [`WindowLog::all`], this never reads
    /// derivative payloads — the page carries their recorded identity and
    /// byte length, while bodies still decode through the same committed
    /// format. Read-only metadata cannot mint windows, so an empty chain
    /// yields an empty page rather than the lazily persisted genesis.
    pub fn meta_page(&self, before: Option<u64>, limit: u64) -> Result<WindowMetaPage, StorageError> {
        let before = before.map(|g| i64::try_from(g).map_err(storage)).transpose()?;
        let limit = i64::try_from(limit).map_err(storage)?;
        let page = self
            .store
            .context_window_meta_page(self.scope_id, before, limit)
            .map_err(storage)?;
        let mut windows = Vec::with_capacity(page.windows.len());
        for stored in page.windows {
            let window: CommittedWindow =
                serde_json::from_str(&stored.body).map_err(storage)?;
            if window.id != stored.window_id {
                return Err(storage(format!(
                    "window row {} holds body of {}",
                    stored.window_id, window.id
                )));
            }
            windows.push(CommittedWindowMeta {
                window,
                derivatives: stored
                    .derivatives
                    .into_iter()
                    .map(|d| DerivativeMeta {
                        media_type: d.media_type,
                        blake3: d.blake3,
                        bytes: d.bytes,
                    })
                    .collect(),
            });
        }
        Ok(WindowMetaPage {
            total: u64::try_from(page.total).map_err(storage)?,
            has_older: page.has_older,
            windows,
        })
    }
}

impl WindowLog for StoreWindowLog {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn head(&self) -> Result<CommittedWindow, StorageError> {
        if let Some(head) = self.store.context_window_head(self.scope_id).map_err(storage)? {
            return Self::decode(head);
        }
        // Lazily persist genesis. Losing the race to another writer is fine:
        // whoever landed first is the genesis everyone then reads.
        let genesis = CommittedWindow::genesis();
        match self
            .store
            .append_context_window(self.scope_id, None, None, &Self::encode(&genesis)?)
            .map_err(storage)?
        {
            ContextWindowAppend::Appended | ContextWindowAppend::HeadChanged => {}
            ContextWindowAppend::ModelGenerationChanged { .. } => {
                return Err(storage("genesis append unexpectedly checked model generation"));
            }
        }
        self.store
            .context_window_head(self.scope_id)
            .map_err(storage)?
            .map(Self::decode)
            .transpose()?
            .ok_or_else(|| storage("genesis window missing after append"))
    }

    fn all(&self) -> Result<Vec<CommittedWindow>, StorageError> {
        self.store
            .context_windows(self.scope_id)
            .map_err(storage)?
            .into_iter()
            .map(Self::decode)
            .collect()
    }

    fn append(
        &self,
        expected: EntityId,
        expected_model_generation: u64,
        window: &CommittedWindow,
    ) -> Result<WindowAppend, StorageError> {
        let expected_model_generation = i64::try_from(expected_model_generation).map_err(storage)?;
        match self
            .store
            .append_context_window(
                self.scope_id,
                Some(expected),
                Some(expected_model_generation),
                &Self::encode(window)?,
            )
            .map_err(storage)?
        {
            ContextWindowAppend::Appended => Ok(WindowAppend::Appended),
            ContextWindowAppend::HeadChanged => Ok(WindowAppend::SourceChanged),
            ContextWindowAppend::ModelGenerationChanged { current } => {
                Ok(WindowAppend::ModelGenerationChanged {
                    current: u64::try_from(current).map_err(storage)?,
                })
            }
        }
    }

    fn model_generation(&self) -> Result<u64, StorageError> {
        let generation = self.store.context_model_generation(self.scope_id).map_err(storage)?;
        u64::try_from(generation).map_err(storage)
    }

    fn model_profile(&self) -> Result<Option<(u64, String)>, StorageError> {
        self.store.context_profile(self.scope_id).map_err(storage)?
            .map(|(generation, profile)| Ok((u64::try_from(generation).map_err(storage)?, profile)))
            .transpose()
    }

    fn rebind_model(&self, generation: u64, profile_digest: &str) -> Result<(), StorageError> {
        let generation = i64::try_from(generation).map_err(storage)?;
        let generation_u64 = u64::try_from(generation).map_err(storage)?;
        let current = self.head()?;
        if self.model_profile()?.is_some_and(|(current, profile)|
            current == generation_u64 && profile.as_str() == profile_digest) {
            return Ok(());
        }
        let mut root = CommittedWindow::genesis();
        root.base = Some(WindowBase {
            source_window: current.id, leaf_seq: 0,
            model_generation: current.model_generation, policy_epoch: 0,
        });
        root.generation = current.generation + 1;
        root.model_generation = u64::try_from(generation).map_err(storage)?;
        match self.store.rebind_context_model_generation(self.scope_id, current.id,
            i64::try_from(current.model_generation).map_err(storage)?, generation, profile_digest,
            &Self::encode(&root)?).map_err(storage)? {
            ContextWindowAppend::Appended => Ok(()),
            ContextWindowAppend::HeadChanged => Err(storage("context head changed during model rebind")),
            ContextWindowAppend::ModelGenerationChanged { current } => Err(storage(format!("context model generation changed to {current} during rebind"))),
        }
    }

    /// The highest installation epoch any committed window of this scope was
    /// prepared under. The bodies are read through the bounded metadata page,
    /// so no derivative payload is materialized to answer this.
    fn max_policy_epoch(&self) -> Result<u64, StorageError> {
        let mut highest = 0u64;
        let mut before = None;
        loop {
            let page = self.meta_page(before, 64)?;
            for meta in &page.windows {
                if let Some(base) = meta.window.base.as_ref() {
                    highest = highest.max(base.policy_epoch);
                }
            }
            if !page.has_older {
                break;
            }
            before = page.windows.last().map(|meta| meta.window.generation);
            if before.is_none() {
                break;
            }
        }
        Ok(highest)
    }
}

/// Branch journal of working notes over the store's `context_notes` table.
///
/// The durable journal is the authority: a restart resumes the same scope's
/// latest revision, and a clear stays a revision in the log. The size bound
/// and the compare-and-append rule belong to [`NotesJournal`], so a refused
/// write never reaches the store.
pub struct StoreNotesJournal {
    store: Arc<Store>,
    scope_id: EntityId,
}

impl StoreNotesJournal {
    pub fn new(store: Arc<Store>, scope_id: EntityId) -> Self {
        Self { store, scope_id }
    }

    fn note(stored: shenko_store::StoredContextNote) -> Result<NoteRevision, NotesError> {
        let source = NoteSource::from_label(&stored.source).ok_or_else(|| {
            NotesError::Storage(StorageError(format!(
                "note revision {} of scope {} carries source label {:?} this reader does not interpret",
                stored.revision, stored.scope_id, stored.source
            )))
        })?;
        let revision = u64::try_from(stored.revision).map_err(|_| {
            NotesError::Storage(StorageError(format!(
                "note revision {} of scope {} is negative",
                stored.revision, stored.scope_id
            )))
        })?;
        Ok(NoteRevision {
            scope_id: stored.scope_id,
            revision,
            text: stored.body,
            source,
            written_at_ms: stored.written_at_ms,
            visible: stored.visible,
        })
    }

    fn append(
        &self,
        expected_revision: u64,
        text: String,
        source: NoteSource,
        visible: bool,
    ) -> Result<NoteRevision, NotesError> {
        if text.len() > MAX_NOTE_BYTES {
            return Err(NotesError::Oversize { bytes: text.len(), limit: MAX_NOTE_BYTES });
        }
        let expected = (expected_revision > 0).then_some(i64::try_from(expected_revision).ok())
            .flatten();
        if expected_revision > 0 && expected.is_none() {
            return Err(NotesError::StaleRevision {
                expected: expected_revision,
                current: None,
            });
        }
        let written_at_ms = crate::notes::now_ms();
        match self
            .store
            .append_context_note(self.scope_id, expected, &text, source.label(), written_at_ms, visible)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))?
        {
            shenko_store::ContextNoteAppend::Appended { revision } => Ok(NoteRevision {
                scope_id: self.scope_id,
                revision: u64::try_from(revision).map_err(|_| {
                    NotesError::Storage(StorageError(format!(
                        "appended note revision {revision} is negative"
                    )))
                })?,
                text,
                source,
                written_at_ms,
                visible,
            }),
            shenko_store::ContextNoteAppend::Stale { current } => {
                Err(NotesError::StaleRevision {
                    expected: expected_revision,
                    current: current.and_then(|revision| u64::try_from(revision).ok()),
                })
            }
        }
    }
}

impl NotesJournal for StoreNotesJournal {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn head(&self) -> Result<Option<NoteRevision>, NotesError> {
        self.store
            .context_note_head(self.scope_id)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))?
            .map(Self::note)
            .transpose()
    }

    fn revisions(&self, limit: u64) -> Result<Vec<NoteRevision>, NotesError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.store
            .context_note_page(self.scope_id, None, limit)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))?
            .into_iter()
            .map(Self::note)
            .collect()
    }

    fn replace(
        &self,
        expected_revision: u64,
        text: &str,
        source: NoteSource,
    ) -> Result<NoteRevision, NotesError> {
        if text.trim().is_empty() {
            return Err(NotesError::Empty);
        }
        self.append(expected_revision, text.to_owned(), source, true)
    }

    fn clear(&self, expected_revision: u64) -> Result<NoteRevision, NotesError> {
        self.append(expected_revision, String::new(), NoteSource::Reset, false)
    }

    fn adopt_from(&self, source: &dyn NotesJournal) -> Result<Option<NoteRevision>, NotesError> {
        if let Some(note) = self.head()? {
            return Err(NotesError::AlreadyJournaled { revision: note.revision });
        }
        let Some(note) = source.visible()? else {
            return Ok(None);
        };
        if note.text.trim().is_empty() {
            return Ok(None);
        }
        self.append(0, note.text, NoteSource::Fork, true).map(Some)
    }
}

/// Durable arm/answer of explicit rollover requests over the store's
/// `context_rollover_requests` row.
///
/// The row is the authority across a restart: a request armed before the
/// process stopped is still pending afterwards, and the committed window that
/// answered it is recorded, so a restart cannot cut the same boundary again and
/// a repeated adoption cannot rewrite the audit. A request is not a window and
/// never cuts history.
pub struct StoreRolloverJournal {
    store: Arc<Store>,
    scope_id: EntityId,
}

impl StoreRolloverJournal {
    pub fn new(store: Arc<Store>, scope_id: EntityId) -> Self {
        Self { store, scope_id }
    }

    fn from_stored(stored: shenko_store::StoredRolloverRequest) -> RolloverRequest {
        RolloverRequest {
            request_id: stored.request_id,
            scope_id: stored.scope_id,
            requested_at_ms: stored.requested_at_ms,
        }
    }
}

impl RolloverJournal for StoreRolloverJournal {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn request(&self, requested_at_ms: i64) -> Result<RolloverRequest, NotesError> {
        self.store
            .arm_context_rollover(self.scope_id, requested_at_ms)
            .map(Self::from_stored)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))
    }

    fn pending(&self) -> Result<Option<RolloverRequest>, NotesError> {
        Ok(self
            .store
            .context_rollover_request(self.scope_id)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))?
            .filter(|stored| stored.consumed_window.is_none())
            .map(Self::from_stored))
    }

    fn consume(&self, window: EntityId) -> Result<bool, NotesError> {
        self.store
            .consume_context_rollover(self.scope_id, window)
            .map_err(|error| NotesError::Storage(StorageError(error.to_string())))
    }
}

#[cfg(test)]
mod notes_journal_tests {
    use super::*;

    fn open(dir: &std::path::Path) -> Arc<Store> {
        Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap())
    }

    #[test]
    fn durable_note_journal_is_bounded_compare_and_append_and_reopens() {
        let dir = std::env::temp_dir().join(format!("shenko-notes-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let scope = EntityId::new();
        let store = open(&dir);
        let journal = StoreNotesJournal::new(Arc::clone(&store), scope);

        let first = journal.replace(0, "first working state", NoteSource::Tool).unwrap();
        assert_eq!(first.revision, 1);
        assert_eq!(
            journal.replace(0, "stale writer", NoteSource::Tool),
            Err(NotesError::StaleRevision { expected: 0, current: Some(1) })
        );
        // The bound is UTF-8 bytes and is checked before anything is appended.
        let oversized = "x".repeat(MAX_NOTE_BYTES + 1);
        assert_eq!(
            journal.replace(1, &oversized, NoteSource::Tool),
            Err(NotesError::Oversize { bytes: MAX_NOTE_BYTES + 1, limit: MAX_NOTE_BYTES })
        );
        assert_eq!(journal.head().unwrap().unwrap().text, "first working state");
        // Exactly the limit in multi-byte text is admitted; one byte more is not.
        let exact = "汉".repeat(MAX_NOTE_BYTES / 3);
        assert!(exact.len() <= MAX_NOTE_BYTES);
        let second = journal.replace(1, &exact, NoteSource::Tool).unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(journal.replace(2, &format!("{exact}汉"), NoteSource::Tool), Err(NotesError::Oversize {
            bytes: exact.len() + 3,
            limit: MAX_NOTE_BYTES,
        }));

        // A clear is a revision: the journal keeps it and hides the notebook.
        let cleared = journal.clear(2).unwrap();
        assert_eq!(cleared.revision, 3);
        assert!(!cleared.visible);
        assert!(journal.visible().unwrap().is_none());
        // Rewriting after a clear continues the journal; the cleared note never
        // resurfaces as the visible notebook.
        let rewritten = journal.replace(3, "state after reset", NoteSource::Tool).unwrap();
        assert_eq!(rewritten.revision, 4);
        assert_eq!(journal.visible().unwrap().unwrap().text, "state after reset");
        assert_eq!(journal.revisions(10).unwrap().len(), 4);

        // Reopen: the same scope resumes at the same head without a plugin.
        drop(journal);
        drop(store);
        let reopened = StoreNotesJournal::new(open(&dir), scope);
        let head = reopened.head().unwrap().unwrap();
        assert_eq!(head.revision, 4);
        assert_eq!(head.text, "state after reset");
        assert_eq!(head.source, NoteSource::Tool);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fork_adoption_copies_the_source_notebook_once() {
        let dir = std::env::temp_dir().join(format!("shenko-notes-fork-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = open(&dir);
        let source_scope = EntityId::new();
        let source = StoreNotesJournal::new(Arc::clone(&store), source_scope);
        source.replace(0, "branch state worth carrying", NoteSource::Tool).unwrap();
        let fork = StoreNotesJournal::new(Arc::clone(&store), EntityId::new());
        let adopted = fork.adopt_from(&source).unwrap().unwrap();
        assert_eq!(adopted.source, NoteSource::Fork);
        assert_eq!(adopted.text, "branch state worth carrying");
        assert_eq!(fork.visible().unwrap().unwrap().text, "branch state worth carrying");
        assert_eq!(
            fork.adopt_from(&source),
            Err(NotesError::AlreadyJournaled { revision: 1 })
        );
        // A fork of a cleared notebook carries nothing, not the older revision.
        source.clear(1).unwrap();
        let second = StoreNotesJournal::new(Arc::clone(&store), EntityId::new());
        assert_eq!(second.adopt_from(&source).unwrap(), None);
        assert!(second.head().unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn durable_rollover_request_is_armed_once_answered_once_and_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("shenko-rollover-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let scope = EntityId::new();
        let store = open(&dir);
        let journal = StoreRolloverJournal::new(Arc::clone(&store), scope);
        assert!(journal.pending().unwrap().is_none());

        let armed = journal.request(1_760_000_000_000).unwrap();
        // A repeated request before the boundary keeps the first one.
        let again = journal.request(1_760_000_000_999).unwrap();
        assert_eq!(again.request_id, armed.request_id);
        assert_eq!(again.requested_at_ms, armed.requested_at_ms);
        assert_eq!(journal.pending().unwrap().unwrap().request_id, armed.request_id);

        drop(journal);
        drop(store);
        let store = open(&dir);
        let journal = StoreRolloverJournal::new(Arc::clone(&store), scope);
        assert_eq!(
            journal.pending().unwrap().unwrap().request_id,
            armed.request_id,
            "an armed request survives a restart"
        );

        let window = EntityId::new();
        assert!(journal.consume(window).unwrap());
        assert!(!journal.consume(EntityId::new()).unwrap(), "an answered request is not rewritten");
        assert!(journal.pending().unwrap().is_none());
        drop(journal);
        drop(store);
        let store = open(&dir);
        let journal = StoreRolloverJournal::new(store, scope);
        assert!(journal.pending().unwrap().is_none(), "the answer survives a restart");
        // A later request is a new fact, not a replay of the answered one.
        let later = journal.request(1_760_000_001_000).unwrap();
        assert_ne!(later.request_id, armed.request_id);
        assert_eq!(journal.pending().unwrap().unwrap().request_id, later.request_id);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn journal_of_another_scope_is_refused_by_scope_identity() {
        let dir = std::env::temp_dir().join(format!("shenko-notes-scope-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = open(&dir);
        let scope = EntityId::new();
        let mine = StoreNotesJournal::new(Arc::clone(&store), scope);
        let other = StoreNotesJournal::new(Arc::clone(&store), EntityId::new());
        mine.replace(0, "mine", NoteSource::Tool).unwrap();
        other.replace(0, "theirs", NoteSource::Tool).unwrap();
        assert_eq!(mine.scope_id(), scope);
        assert_eq!(mine.visible().unwrap().unwrap().text, "mine");
        assert_eq!(other.visible().unwrap().unwrap().text, "theirs");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod meta_page_tests {
    use super::*;
    use crate::WindowEntry;

    #[test]
    fn meta_page_bounds_cost_and_preserves_identity_without_payloads() {
        let dir = std::env::temp_dir().join(format!("shenko-meta-page-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let log = StoreWindowLog::new(store, EntityId::new());

        let genesis = log.head().unwrap();
        let mut window = CommittedWindow::genesis();
        window.id = EntityId::new();
        window.generation = genesis.generation + 1;
        window.base = Some(WindowBase {
            source_window: genesis.id,
            leaf_seq: 7,
            model_generation: genesis.model_generation,
            policy_epoch: 0,
        });
        window.policy_id = Some("snapcompact".into());
        window.entries.push(WindowEntry::Image { from: 1, to: 9, derivative: 0, tokens: 7 });
        window.derivatives.push(Derivative::new("image/png", vec![1, 2, 3]));
        window.call_states.insert("call-1".into(), vec![(3, CallStatus::Unknown)]);
        window.cost.window_tokens = 7;
        assert_eq!(log.append(genesis.id, 0, &window).unwrap(), WindowAppend::Appended);

        // First page: only the newest window, with a truthful older cursor.
        let page = log.meta_page(None, 1).unwrap();
        assert_eq!(page.total, 2);
        assert!(page.has_older);
        let [meta] = page.windows.as_slice() else { panic!("one window per page") };
        assert_eq!(meta.window.id, window.id);
        assert_eq!(meta.window.policy_id.as_deref(), Some("snapcompact"));
        assert_eq!(meta.window.call_states, window.call_states);
        assert_eq!(meta.window.cost.window_tokens, 7);
        // The committed body never carries payloads: identity without bytes.
        assert!(meta.window.derivatives.is_empty());
        let [frame] = meta.derivatives.as_slice() else { panic!("one derivative") };
        assert_eq!(frame.blake3, window.derivatives[0].blake3);
        assert_eq!(frame.bytes, 3);

        // Cursor below the page's oldest generation reaches only genesis.
        let rest = log.meta_page(Some(meta.window.generation), 8).unwrap();
        assert_eq!(rest.total, 2);
        assert!(!rest.has_older);
        let [oldest] = rest.windows.as_slice() else { panic!("one older window") };
        assert_eq!(oldest.window.id, genesis.id);
        assert!(oldest.derivatives.is_empty());

        let _ = std::fs::remove_dir_all(dir);
    }
}
