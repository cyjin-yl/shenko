//! Notes-backed rollover behaviour, exercised through the real policy and
//! registry contracts: exact raw coverage, one head read, explicit boundary
//! windows, whole unfinished units, and a request answered only by a committed
//! window.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use shenko_compact_rollover::{NotesRollover, RolloverConfig};
use shenko_context_policy::{
    CallStatus, ContextInput, ContextPolicy, HistoryItem, HistorySource, ItemKind, MemoryHistory,
    MemoryNotesJournal, MemoryRolloverJournal, ModelCaps, NoteRevision, NoteSource, NotesError,
    NotesJournal, PolicyError, PolicyRegistry, PublishError, PreparedWindow, Projected, RolloverJournal,
    WindowBase, WindowEntry, WindowLedger, units,
};
use shenko_types::ids::EntityId;

const CAPS: ModelCaps = ModelCaps { model_id: String::new(), max_context: 131_072, vision: None };

/// One system unit followed by `turns` complete call units; `open` names the
/// call left unresolved.
fn history(turns: usize, open: Option<&str>) -> MemoryHistory {
    let history = MemoryHistory::new();
    history.append(ItemKind::System, "system prompt", 50);
    for n in 0..turns {
        let call = format!("c{n}");
        let status = if open == Some(call.as_str()) {
            CallStatus::Unknown
        } else {
            CallStatus::Executed { ok: true }
        };
        history.append(ItemKind::User, format!("step {n}: repair the failing test"), 60);
        history.append(
            ItemKind::ToolCall {
                call_id: call.clone(),
                tool: "shell".into(),
                args_ref: format!("ledger://call/{call}"),
                status,
            },
            "cargo test -p shenko-store",
            20,
        );
        history.append(
            ItemKind::ToolResult { call_id: call, status },
            "test result: FAILED. 3 passed; 1 failed",
            120,
        );
    }
    history
}

fn base(history: &MemoryHistory, source_window: EntityId, model_generation: u64) -> WindowBase {
    WindowBase {
        source_window,
        leaf_seq: history.leaf_seq().unwrap().unwrap_or(0),
        model_generation,
        policy_epoch: 1,
    }
}

fn strategy(notes: Arc<dyn NotesJournal>, retain_units: usize) -> NotesRollover {
    NotesRollover::new(
        RolloverConfig { trigger_ratio: 0.85, reminder_ratio: 0.7, retain_units },
        notes,
    )
}

fn prepare(
    policy: &NotesRollover,
    history: &MemoryHistory,
    rollover: bool,
) -> Result<PreparedWindow, PolicyError> {
    prepare_against(base(history, EntityId::new(), 1), policy, history, rollover)
}

/// Prepare against the ledger's live head, so the window can be committed there.
fn prepare_on(
    ledger: &WindowLedger,
    policy: &NotesRollover,
    history: &MemoryHistory,
    rollover: bool,
) -> Result<PreparedWindow, PolicyError> {
    let head = ledger.current().unwrap().id;
    let generation = ledger.model_generation().unwrap();
    prepare_against(base(history, head, generation), policy, history, rollover)
}

fn prepare_against(
    base: WindowBase,
    policy: &NotesRollover,
    history: &MemoryHistory,
    rollover: bool,
) -> Result<PreparedWindow, PolicyError> {
    let items = history.snapshot();
    let input = ContextInput {
        base,
        items: &items,
        caps: &CAPS,
        used_tokens: items.iter().map(|item| item.tokens).sum(),
        rollover,
    };
    policy.prepare(&input)
}

fn replaced_ranges(entries: &[WindowEntry]) -> Vec<(u64, u64)> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            WindowEntry::Verbatim { .. } | WindowEntry::Note { .. } => None,
            WindowEntry::Pointer { from, to, .. }
            | WindowEntry::Text { from, to, .. }
            | WindowEntry::Image { from, to, .. } => Some((*from, *to)),
        })
        .collect()
}

fn kept_seqs(entries: &[WindowEntry]) -> Vec<u64> {
    entries
        .iter()
        .filter_map(|entry| match entry {
            WindowEntry::Verbatim { at, .. } => Some(at.seq),
            _ => None,
        })
        .collect()
}

/// The one notebook entry of a window, if it carries one.
fn notebook(entries: &[WindowEntry]) -> Option<&WindowEntry> {
    let mut found = None;
    for entry in entries {
        if matches!(entry, WindowEntry::Note { .. }) {
            assert!(found.is_none(), "a window must not carry two notebook entries");
            found = Some(entry);
        }
    }
    found
}

/// Journal wrapper that counts reads, so "the head is read once, and
/// `visible()` is not used to re-read it" is observed rather than assumed.
struct CountingNotes {
    inner: MemoryNotesJournal,
    heads: AtomicUsize,
    visibles: AtomicUsize,
}

impl CountingNotes {
    fn new(scope: EntityId) -> Self {
        Self {
            inner: MemoryNotesJournal::new(scope),
            heads: AtomicUsize::new(0),
            visibles: AtomicUsize::new(0),
        }
    }
}

impl NotesJournal for CountingNotes {
    fn scope_id(&self) -> EntityId {
        self.inner.scope_id()
    }

    fn head(&self) -> Result<Option<NoteRevision>, NotesError> {
        self.heads.fetch_add(1, Ordering::SeqCst);
        self.inner.head()
    }

    fn visible(&self) -> Result<Option<NoteRevision>, NotesError> {
        self.visibles.fetch_add(1, Ordering::SeqCst);
        self.inner.visible()
    }

    fn revisions(&self, limit: u64) -> Result<Vec<NoteRevision>, NotesError> {
        self.inner.revisions(limit)
    }

    fn replace(&self, expected: u64, text: &str, source: NoteSource) -> Result<NoteRevision, NotesError> {
        self.inner.replace(expected, text, source)
    }

    fn clear(&self, expected: u64) -> Result<NoteRevision, NotesError> {
        self.inner.clear(expected)
    }

    fn adopt_from(&self, source: &dyn NotesJournal) -> Result<Option<NoteRevision>, NotesError> {
        self.inner.adopt_from(source)
    }
}

#[test]
fn the_notebook_is_its_own_range_free_entry_and_coverage_is_exact() {
    let history = history(8, None);
    let notes = Arc::new(MemoryNotesJournal::new(EntityId::new()));
    notes
        .replace(0, "the failing test is store_commit_rejects_stale_head", NoteSource::Tool)
        .unwrap();
    let notes: Arc<dyn NotesJournal> = notes;
    let ledger = WindowLedger::default();
    let prepared = prepare_on(&ledger, &strategy(notes, 2), &history, false).unwrap();

    // The notebook is carried once, as its own typed entry with the revision
    // and provenance it was read from, and it names no raw range: it is not a
    // derivative of the dropped run and never stands in for one.
    let WindowEntry::Note { revision, source, text, .. } =
        notebook(&prepared.entries).expect("the notebook enters the window once")
    else {
        unreachable!("notebook() returns the Note entry")
    };
    assert_eq!(*revision, 1);
    assert_eq!(*source, NoteSource::Tool);
    assert_eq!(text, "the failing test is store_commit_rejects_stale_head");
    // The dropped-run stand-in is a pointer over exactly its own range, so no
    // second entry repeats the notebook text.
    let repeated = prepared
        .entries
        .iter()
        .filter(|entry| matches!(entry, WindowEntry::Text { text, .. } if text.contains("working notebook")))
        .count();
    assert_eq!(repeated, 0, "the notebook must not be embedded in a stand-in as well");

    // No entry claims a raw seq another entry already shows. The replaced
    // window covered 0..=max with the notebook, overlapping every retained
    // unit; one entry per dropped run keeps coverage exact.
    let mut seen: Vec<u64> = Vec::new();
    for (from, to) in replaced_ranges(&prepared.entries) {
        assert!(from <= to, "invalid lossy range {from}..={to}");
        for seq in from..=to {
            assert!(!seen.contains(&seq), "raw seq {seq} is covered twice");
            seen.push(seq);
        }
    }
    for seq in kept_seqs(&prepared.entries) {
        assert!(!seen.contains(&seq), "verbatim seq {seq} is inside a replaced range");
    }

    // Complete recent units stay verbatim; no summarization model runs.
    let items = history.snapshot();
    let all_units = units(&items);
    let kept = kept_seqs(&prepared.entries);
    for unit in all_units.iter().rev().take(2) {
        for item in &items[unit.clone()] {
            assert!(kept.contains(&item.at.seq), "recent unit seq {} was not retained", item.at.seq);
        }
    }
    assert_eq!(prepared.cost.model_calls, 0);
    assert!(prepared.cost.window_tokens < prepared.cost.source_tokens);

    // The committed window stands for exactly the raw events its entries name,
    // the notebook adds no coverage, and it is projected once.
    let window = ledger.commit(prepared, &history).unwrap();
    let expected: u64 = window
        .entries
        .iter()
        .map(|entry| match entry {
            WindowEntry::Verbatim { .. } => 1u64,
            WindowEntry::Note { .. } => 0,
            WindowEntry::Pointer { from, to, .. }
            | WindowEntry::Text { from, to, .. }
            | WindowEntry::Image { from, to, .. } => to - from + 1,
        })
        .sum();
    assert_eq!(window.cost.raw_events_covered, expected);
    let projected = window.project(&history).unwrap();
    let working: Vec<(u64, NoteSource, String)> = projected
        .iter()
        .filter_map(|projected| match projected {
            Projected::WorkingNote { revision, source, text } =>
                Some((*revision, *source, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        working,
        vec![(1, NoteSource::Tool, "the failing test is store_commit_rejects_stale_head".to_string())],
        "the window projects exactly one notebook revision and it has no raw range"
    );
    assert!(projected.iter().any(|projected| matches!(projected, Projected::Note { .. })));
}

#[test]
fn only_the_latest_revision_is_read_and_only_once() {
    let counting = Arc::new(CountingNotes::new(EntityId::new()));
    counting.replace(0, "revision one", NoteSource::Tool).unwrap();
    counting.replace(1, "revision two", NoteSource::Tool).unwrap();
    let notes: Arc<dyn NotesJournal> = counting.clone();
    let history = history(8, None);
    let prepared = prepare(&strategy(notes, 2), &history, false).unwrap();
    let WindowEntry::Note { revision, source, text, .. } =
        notebook(&prepared.entries).expect("the notebook enters the window")
    else {
        unreachable!("notebook() returns the Note entry")
    };
    assert_eq!(*revision, 2);
    assert_eq!(*source, NoteSource::Tool);
    assert_eq!(text, "revision two", "a retired revision resurfaced");
    assert_eq!(counting.heads.load(Ordering::SeqCst), 1, "the head must be read once");
    assert_eq!(
        counting.visibles.load(Ordering::SeqCst),
        0,
        "visible() would re-read the head after the decision was made"
    );
}

#[test]
fn a_cleared_notebook_is_not_carried_and_an_older_revision_never_resurfaces() {
    let notes = Arc::new(MemoryNotesJournal::new(EntityId::new()));
    notes.replace(0, "old working state", NoteSource::Tool).unwrap();
    notes.clear(1).unwrap();
    let notes: Arc<dyn NotesJournal> = notes;
    let history = history(8, None);
    let prepared = prepare(&strategy(notes, 2), &history, false).unwrap();
    assert!(notebook(&prepared.entries).is_none());
}

#[test]
fn explicit_rollover_cuts_a_boundary_window_that_drops_nothing() {
    let history = history(2, None);
    let notes = Arc::new(MemoryNotesJournal::new(EntityId::new()));
    notes.replace(0, "state worth carrying into the next window", NoteSource::Tool).unwrap();
    let notes: Arc<dyn NotesJournal> = notes;
    let policy = strategy(notes, 4);
    // The notebook alone is not a reason to cut a window: a size-triggered
    // preparation that drops nothing stays a refusal.
    assert_eq!(prepare(&policy, &history, false), Err(PolicyError::NoReduction));
    let ledger = WindowLedger::default();
    let prepared = prepare_on(&ledger, &policy, &history, true).unwrap();
    assert!(prepared.entries.iter().all(|entry| matches!(
        entry,
        WindowEntry::Verbatim { .. } | WindowEntry::Note { .. }
    )));
    assert!(replaced_ranges(&prepared.entries).is_empty(), "a boundary window invents no raw range");
    // A boundary that drops nothing still carries the working state the model
    // wrote; a later tool read is not equivalent to carrying it.
    let WindowEntry::Note { revision, text, .. } =
        notebook(&prepared.entries).expect("the boundary window carries the notebook")
    else {
        unreachable!("notebook() returns the Note entry")
    };
    assert_eq!(*revision, 1);
    assert_eq!(text, "state worth carrying into the next window");
    assert_eq!(prepared.cost.model_calls, 0);
    // Every complete unit is present: a boundary is not a summary.
    assert_eq!(prepared.entries.len(), history.snapshot().len() + 1);
    let window = ledger.commit(prepared, &history).unwrap();
    assert_eq!(window.cost.raw_events_covered, history.snapshot().len() as u64);
}

#[test]
fn unfinished_call_units_are_kept_whole_and_carried() {
    let history = history(8, Some("c2"));
    let notes: Arc<dyn NotesJournal> = Arc::new(MemoryNotesJournal::new(EntityId::new()));
    let prepared = prepare(&strategy(notes, 1), &history, false).unwrap();
    assert!(prepared.carried_calls.contains(&"c2".to_string()));
    let items = history.snapshot();
    let resolved = units(&items)
        .into_iter()
        .find(|unit| {
            items[unit.clone()].iter().any(|item: &HistoryItem| {
                matches!(&item.kind, ItemKind::ToolCall { call_id, .. } if call_id == "c2")
            })
        })
        .expect("the unresolved unit exists");
    let kept = kept_seqs(&prepared.entries);
    for item in &items[resolved] {
        assert!(kept.contains(&item.at.seq), "unresolved unit seq {} was replaced", item.at.seq);
    }
}

#[test]
fn registry_bypasses_the_gain_guard_for_an_explicit_request_and_answers_it_once() {
    let history = history(2, None);
    let ledger = WindowLedger::default();
    let scope = ledger.scope_id();
    let registry = PolicyRegistry::with_ledger(ledger);
    let notes: Arc<dyn NotesJournal> = Arc::new(MemoryNotesJournal::new(scope));
    let journal = Arc::new(MemoryRolloverJournal::new(scope));
    let rollover_journal: Arc<dyn RolloverJournal> = journal.clone();
    registry.attach_rollover_journal(rollover_journal).unwrap();
    registry.install(Arc::new(strategy(notes, 4))).unwrap();
    let items = history.snapshot();
    let used = items.iter().map(|item| item.tokens).sum::<u32>();

    // Without a request the 5% gain guard refuses a window that drops nothing.
    assert!(registry.prepare_candidate(&history, &CAPS, used).is_err());

    let armed = registry.request_rollover(scope).unwrap();
    assert_eq!(registry.rollover_request().unwrap().request_id, armed.request_id);
    assert_eq!(journal.pending().unwrap().unwrap().request_id, armed.request_id);
    let candidate = registry.prepare_candidate(&history, &CAPS, used).unwrap();
    assert!(candidate.rollover, "the candidate names the request it answers");
    registry.publish(candidate).unwrap();
    let (window, _) = registry.adopt_prepared(&history).unwrap().expect("the boundary window commits");
    assert_ne!(window.id, EntityId::nil());
    assert!(journal.pending().unwrap().is_none(), "the committed window answers the request");
    assert!(!registry.rollover_pending());
}

#[test]
fn a_request_armed_before_this_process_is_pending_again_after_attach() {
    let ledger = WindowLedger::default();
    let scope = ledger.scope_id();
    let journal = MemoryRolloverJournal::new(scope);
    let armed = journal.request(1_760_000_000_000).unwrap();
    let registry = PolicyRegistry::with_ledger(ledger);
    assert!(!registry.rollover_pending());
    registry.attach_rollover_journal(Arc::new(journal)).unwrap();
    assert!(registry.rollover_pending());
    assert_eq!(registry.rollover_request().unwrap().request_id, armed.request_id);
}

#[test]
fn a_dropped_candidate_leaves_the_request_pending_for_the_next_boundary() {
    let history = history(8, None);
    let ledger = WindowLedger::default();
    let scope = ledger.scope_id();
    let registry = PolicyRegistry::with_ledger(ledger);
    let notes: Arc<dyn NotesJournal> = Arc::new(MemoryNotesJournal::new(scope));
    let journal = Arc::new(MemoryRolloverJournal::new(scope));
    let rollover_journal: Arc<dyn RolloverJournal> = journal.clone();
    registry.attach_rollover_journal(rollover_journal).unwrap();
    registry.install(Arc::new(strategy(notes, 2))).unwrap();
    let armed = registry.request_rollover(scope).unwrap();
    let items = history.snapshot();
    let used = items.iter().map(|item| item.tokens).sum::<u32>();
    let candidate = registry.prepare_candidate(&history, &CAPS, used).unwrap();
    assert!(candidate.rollover);
    // A strategy replaced after preparation cannot commit its window: the
    // candidate is dropped, and the request is still unanswered.
    registry.install(Arc::new(strategy(
        Arc::new(MemoryNotesJournal::new(scope)) as Arc<dyn NotesJournal>,
        2,
    )));
    assert!(
        matches!(registry.publish(candidate), Err(PublishError::StalePolicy { .. })),
        "a candidate from a replaced strategy must be refused at publish"
    );
    assert!(registry.adopt_prepared(&history).unwrap().is_none());
    assert_eq!(
        registry.rollover_request().unwrap().request_id,
        armed.request_id,
        "a dropped candidate must not consume the request"
    );
}
