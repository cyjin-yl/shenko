//! Branch-journaled working notes for notes-backed context windows — docs/specs/context-policies.md#spec-12-3-1.
//!
//! The notebook is not a second transcript: it is a small, model-authored
//! working state. Each replacement is a journal revision of one branch scope,
//! and only the latest revision may enter a model context. A clearing
//! revision stays in the journal, so "reset" is a recorded fact and an older
//! note never resurfaces merely because a newer one was cleared.
//!
//! The journal owns two invariants the tool surface must not re-derive:
//!
//! * a note is bounded to [`MAX_NOTE_BYTES`] UTF-8 bytes, and an oversized
//!   write is refused before anything is appended, so the previous note stays
//!   exactly as it was;
//! * a replacement names the revision it read. A stale expected revision is
//!   refused instead of overwriting another writer's note.
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::ObjectRef;

use crate::history::StorageError;
use crate::registry::RolloverRequest;

/// Longest note a journal entry may carry, in UTF-8 bytes (not characters).
pub const MAX_NOTE_BYTES: usize = 16_384;

/// Media type of a registered notebook revision's content. Note text is UTF-8
/// by construction (the byte bound is checked on the encoded string).
pub const NOTE_MEDIA_TYPE: &str = "text/plain; charset=utf-8";

/// Who wrote a note revision. The label is provenance, not authority: a fork
/// copy is not a model note, and a reset is not a replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteSource {
    /// Written through the installed notebook tool.
    Tool,
    /// A reset: the visible notebook ends here, earlier revisions stay.
    Reset,
    /// Copied from the source branch when this branch was forked from it.
    Fork,
    /// Written by the runtime while bootstrapping a scope.
    Bootstrap,
}

impl NoteSource {
    /// Stable journal label. Readers never parse a free-form string.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Reset => "reset",
            Self::Fork => "fork",
            Self::Bootstrap => "bootstrap",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "tool" => Some(Self::Tool),
            "reset" => Some(Self::Reset),
            "fork" => Some(Self::Fork),
            "bootstrap" => Some(Self::Bootstrap),
            _ => None,
        }
    }
}

/// One journaled note revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteRevision {
    pub scope_id: EntityId,
    pub revision: u64,
    /// The note text. Empty on a clearing revision.
    pub text: String,
    pub source: NoteSource,
    pub written_at_ms: i64,
    /// Whether this revision is the model-visible notebook. A clearing
    /// revision is journaled with `visible: false`.
    pub visible: bool,
}

impl NoteRevision {
    pub fn is_clear(&self) -> bool {
        !self.visible
    }
}

/// Why a note read or write failed. A failed write never changes the journal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum NotesError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("note of {bytes} UTF-8 bytes exceeds the {limit}-byte limit; the previous note is unchanged")]
    Oversize { bytes: usize, limit: usize },
    #[error("note revision {expected} is no longer the head; the journal is at {current:?}")]
    StaleRevision { expected: u64, current: Option<u64> },
    #[error("note journal belongs to scope {expected}, not {actual}")]
    ScopeMismatch { expected: EntityId, actual: EntityId },
    #[error("a visible note cannot be empty; clear it instead of replacing it with nothing")]
    Empty,
    #[error("this scope already journaled revision {revision}; a fork copy cannot rewrite its history")]
    AlreadyJournaled { revision: u64 },
}

/// Read and append access to one branch's note journal.
///
/// `revision` 0 means "this scope has no note yet". Both writers take the
/// expected revision, so two concurrent tools cannot silently overwrite each
/// other; the refusal reports the current head so the caller can re-read.
pub trait NotesJournal: Send + Sync {
    /// The branch scope this journal belongs to.
    fn scope_id(&self) -> EntityId;

    /// The newest revision, visible or cleared.
    fn head(&self) -> Result<Option<NoteRevision>, NotesError>;

    /// The notebook a model context may show: the newest revision only, and
    /// nothing at all when that newest revision is a clear.
    fn visible(&self) -> Result<Option<NoteRevision>, NotesError> {
        Ok(self.head()?.filter(|note| note.visible))
    }

    /// Bounded page of revisions, newest first.
    fn revisions(&self, limit: u64) -> Result<Vec<NoteRevision>, NotesError>;

    /// One retained revision, or `None` when the journal never recorded it.
    ///
    /// This is the explicit-version read: the retained originals stay
    /// readable, and nothing replaces them with a summary.
    fn revision(&self, revision: u64) -> Result<Option<NoteRevision>, NotesError> {
        if revision == 0 {
            return Ok(None);
        }
        Ok(self
            .revisions(revision.saturating_add(1))?
            .into_iter()
            .find(|note| note.revision == revision))
    }

    /// The content reference of one retained revision (docs/specs/cognition-units.md#spec-51-1: content is
    /// immutable and addressed by digest).
    ///
    /// A store-backed journal seals the retained bytes into the content store
    /// and returns the `ObjectRef`; sealing is idempotent and never rewrites
    /// the journal row. A journal without a content store returns `None`
    /// instead of fabricating a reference.
    fn content(&self, revision: u64) -> Result<Option<ObjectRef>, NotesError> {
        let _ = revision;
        Ok(None)
    }

    /// Replace the notebook. The text must be non-empty and within the size
    /// bound; the check runs before the journal is touched.
    fn replace(
        &self,
        expected_revision: u64,
        text: &str,
        source: NoteSource,
    ) -> Result<NoteRevision, NotesError>;

    /// End the visible notebook without deleting its revisions.
    fn clear(&self, expected_revision: u64) -> Result<NoteRevision, NotesError>;

    /// Carry the source scope's visible notebook into this scope, as a real
    /// journal revision with fork provenance. Returns the adopted revision, or
    /// `None` when the source has no visible note. Refuses when this scope
    /// already journaled a revision, so a later call cannot rewrite history.
    fn adopt_from(&self, source: &dyn NotesJournal) -> Result<Option<NoteRevision>, NotesError>;
}

/// The note text a model context may show for this journal, or `None`.
pub fn visible_note_text(journal: &dyn NotesJournal) -> Result<Option<String>, NotesError> {
    Ok(journal.visible()?.filter(|note| !note.text.is_empty()).map(|note| note.text))
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryState {
    scope_id: EntityId,
    revisions: Vec<NoteRevision>,
}

/// In-process journal for strategies and tests without a durable store.
pub struct MemoryNotesJournal {
    state: parking_lot::Mutex<MemoryState>,
}

impl MemoryNotesJournal {
    pub fn new(scope_id: EntityId) -> Self {
        Self {
            state: parking_lot::Mutex::new(MemoryState { scope_id, revisions: Vec::new() }),
        }
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
        let mut state = self.state.lock();
        let current = state.revisions.last().map(|note| note.revision);
        if current.unwrap_or(0) != expected_revision {
            return Err(NotesError::StaleRevision { expected: expected_revision, current });
        }
        let revision = NoteRevision {
            scope_id: state.scope_id,
            revision: current.map_or(1, |revision| revision + 1),
            text,
            source,
            written_at_ms: now_ms(),
            visible,
        };
        state.revisions.push(revision.clone());
        Ok(revision)
    }
}

impl NotesJournal for MemoryNotesJournal {
    fn scope_id(&self) -> EntityId {
        self.state.lock().scope_id
    }

    fn head(&self) -> Result<Option<NoteRevision>, NotesError> {
        Ok(self.state.lock().revisions.last().cloned())
    }

    fn revisions(&self, limit: u64) -> Result<Vec<NoteRevision>, NotesError> {
        let state = self.state.lock();
        Ok(state.revisions.iter().rev().take(limit as usize).cloned().collect())
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
        let text = note.text;
        if text.trim().is_empty() {
            return Ok(None);
        }
        self.append(0, text, NoteSource::Fork, true).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_are_compare_and_append_and_clear_does_not_resurrect() {
        let journal = MemoryNotesJournal::new(EntityId::new());
        assert_eq!(journal.visible().unwrap(), None);
        let first = journal.replace(0, "working state", NoteSource::Tool).unwrap();
        assert_eq!(first.revision, 1);
        assert!(journal.replace(0, "stale write", NoteSource::Tool).is_err());
        let cleared = journal.clear(1).unwrap();
        assert_eq!(cleared.revision, 2);
        assert_eq!(journal.head().unwrap().unwrap().revision, 2);
        assert_eq!(journal.visible().unwrap(), None);
        assert_eq!(journal.revisions(10).unwrap().len(), 2);
    }

    #[test]
    fn oversize_write_leaves_the_previous_note_untouched() {
        let journal = MemoryNotesJournal::new(EntityId::new());
        journal.replace(0, "kept", NoteSource::Tool).unwrap();
        let oversized = "x".repeat(MAX_NOTE_BYTES + 1);
        assert_eq!(
            journal.replace(1, &oversized, NoteSource::Tool),
            Err(NotesError::Oversize { bytes: MAX_NOTE_BYTES + 1, limit: MAX_NOTE_BYTES })
        );
        assert_eq!(journal.visible().unwrap().unwrap().text, "kept");
        // The byte limit is bytes, not characters: a note of exactly the
        // limit in multi-byte text is admitted only when it fits in bytes.
        let cjk = "汉".repeat(MAX_NOTE_BYTES / 3);
        assert_eq!(journal.replace(1, &cjk, NoteSource::Tool).unwrap().revision, 2);
    }

    #[test]
    fn fork_copies_the_source_visible_note_once() {
        let source = MemoryNotesJournal::new(EntityId::new());
        source.replace(0, "branch state", NoteSource::Tool).unwrap();
        let fork = MemoryNotesJournal::new(EntityId::new());
        let adopted = fork.adopt_from(&source).unwrap().unwrap();
        assert_eq!(adopted.source, NoteSource::Fork);
        assert_eq!(adopted.text, "branch state");
        assert!(fork.adopt_from(&source).is_err());
        source.clear(1).unwrap();
        let second = MemoryNotesJournal::new(EntityId::new());
        assert_eq!(second.adopt_from(&source).unwrap(), None);
    }
}

/// Durable arm/answer of an explicit rollover request.
///
/// The journal makes "start a new window at the next safe boundary" a fact
/// that outlives the process that armed it. It stores no window and never
/// cuts history: only the adoption of a committed window marks it answered.
pub trait RolloverJournal: Send + Sync {
    fn scope_id(&self) -> EntityId;

    /// Arm the request. An already pending request is returned unchanged, so
    /// a repeated tool call cannot stack windows.
    fn request(&self, requested_at_ms: i64) -> Result<RolloverRequest, NotesError>;

    /// The pending request, if any. An answered request is not pending.
    fn pending(&self) -> Result<Option<RolloverRequest>, NotesError>;

    /// Record the committed window that answered the pending request.
    /// Returns whether a pending request existed; an answered request keeps
    /// its original window.
    fn consume(&self, window: EntityId) -> Result<bool, NotesError>;
}

/// In-process rollover journal for strategies and tests without a store.
pub struct MemoryRolloverJournal {
    scope_id: EntityId,
    state: parking_lot::Mutex<Option<(RolloverRequest, Option<EntityId>)>>,
}

impl MemoryRolloverJournal {
    pub fn new(scope_id: EntityId) -> Self {
        Self { scope_id, state: parking_lot::Mutex::new(None) }
    }
}

impl RolloverJournal for MemoryRolloverJournal {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn request(&self, requested_at_ms: i64) -> Result<RolloverRequest, NotesError> {
        let mut state = self.state.lock();
        if let Some((request, None)) = state.as_ref() {
            return Ok(request.clone());
        }
        let request = RolloverRequest {
            request_id: EntityId::new(),
            scope_id: self.scope_id,
            requested_at_ms,
        };
        *state = Some((request.clone(), None));
        Ok(request)
    }

    fn pending(&self) -> Result<Option<RolloverRequest>, NotesError> {
        Ok(self
            .state
            .lock()
            .as_ref()
            .filter(|(_, consumed)| consumed.is_none())
            .map(|(request, _)| request.clone()))
    }

    fn consume(&self, window: EntityId) -> Result<bool, NotesError> {
        let mut state = self.state.lock();
        match state.as_mut() {
            Some((_, consumed @ None)) => {
                *consumed = Some(window);
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// Durable once-per-window answer of the notes strategy's reminder.
///
/// The reminder is prompt material, not a window: this journal records only
/// that the committed window the model is looking at already received its one
/// reminder, so a process that restarts inside the same window cannot show it
/// twice. It never answers an explicit rollover request — that fact has its
/// own journal — and it never cuts history.
///
/// The key is the committed window, not a step or a request: a window gets at
/// most one reminder however many steps it spans, and a new committed window
/// is a new opportunity.
pub trait ReminderJournal: Send + Sync {
    /// The branch scope this journal belongs to.
    fn scope_id(&self) -> EntityId;

    /// Whether this committed window already received its reminder.
    fn issued(&self, window: EntityId) -> Result<bool, NotesError>;

    /// Record the window as reminded under the installation `policy_epoch`.
    /// Returns whether this call created the record; `false` means the
    /// window was already recorded and the original record is unchanged.
    fn mark_issued(&self, window: EntityId, policy_epoch: u64) -> Result<bool, NotesError>;
}

/// In-process reminder journal for strategies and tests without a store.
pub struct MemoryReminderJournal {
    scope_id: EntityId,
    issued: parking_lot::Mutex<std::collections::BTreeMap<EntityId, u64>>,
}

impl MemoryReminderJournal {
    pub fn new(scope_id: EntityId) -> Self {
        Self { scope_id, issued: parking_lot::Mutex::new(std::collections::BTreeMap::new()) }
    }
}

impl ReminderJournal for MemoryReminderJournal {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn issued(&self, window: EntityId) -> Result<bool, NotesError> {
        Ok(self.issued.lock().contains_key(&window))
    }

    fn mark_issued(&self, window: EntityId, policy_epoch: u64) -> Result<bool, NotesError> {
        Ok(self.issued.lock().insert(window, policy_epoch).is_none())
    }
}
