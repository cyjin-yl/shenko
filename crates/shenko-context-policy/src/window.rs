//! Committed windows and the commit transaction (§12.3.2).
//!
//! A committed window records its decoder (policy id/version), source refs and
//! derivatives, so it can be materialized without the plugin that produced it.
//! Where the chain lives is a [`WindowLog`]: in-process, or the durable store.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

use crate::history::{CallStatus, HistoryItem, HistorySource, ItemKind, ItemRef, StorageError, units, unresolved_calls};
use crate::notes::NoteSource;
use crate::policy::{
    CostReport, Derivative, PreparedWindow, WINDOW_BODY_VERSION, WindowBase, WindowEntry,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedWindow {
    pub id: EntityId,
    pub generation: u64,
    /// Body format version. A body written by an older version still
    /// decodes (missing field -> 0); a newer version is refused by the
    /// reader instead of being guessed at.
    #[serde(default)]
    pub body_version: u32,
    /// None for genesis. A model-rebind root links its source but still
    /// materializes the full raw history rather than a compressed prefix.
    pub base: Option<WindowBase>,
    pub policy_id: Option<String>,
    pub policy_version: Option<String>,
    pub entries: Vec<WindowEntry>,
    /// Raw seq/status facts at commit. Missing old-window baselines restore
    /// affected raw units instead of presenting stale execution outcomes.
    #[serde(default)]
    pub call_states: BTreeMap<String, Vec<(u64, CallStatus)>>,
    pub derivatives: Vec<Derivative>,
    /// Raw events after this seq are appended verbatim when materialized.
    pub covers_through: Option<u64>,
    /// Model/template/encoder binding the window's input was built for.
    pub model_generation: u64,
    pub cost: CostReport,
}

/// What actually reaches the model for one entry. Equal identities produce
/// equal input; anything else (including an index into a different
/// derivative list) is a different input.
#[derive(PartialEq)]
enum InputIdentity<'a> {
    Raw(ItemRef, u32, Option<CallStatus>),
    Pointer(u64, u64, &'a str),
    Text(u64, u64, &'a str),
    Image(u64, u64, &'a str, &'a str),
    /// Range-free working state: same revision and same text is the same
    /// input; a new revision is a different one even when the text is equal.
    Note(u64, NoteSource, &'a str),
}

fn identities<'a>(
    entries: &'a [WindowEntry],
    derivatives: &'a [Derivative],
    history: &dyn HistorySource,
) -> Result<Vec<Option<InputIdentity<'a>>>, StorageError> {
    entries
        .iter()
        .map(|entry| match entry {
            WindowEntry::Verbatim { at, tokens } => {
                let item = history.read_event(at.event_id)?
                    .filter(|item| item.at == *at)
                    .ok_or_else(|| StorageError(format!("raw event {} is missing for prefix identity", at.event_id)))?;
                let status = match item.kind {
                    ItemKind::ToolCall { status, .. } | ItemKind::ToolResult { status, .. } => Some(status),
                    _ => None,
                };
                Ok(Some(InputIdentity::Raw(*at, *tokens, status)))
            }
            WindowEntry::Pointer { from, to, note, .. } => {
                Ok(Some(InputIdentity::Pointer(*from, *to, note)))
            }
            WindowEntry::Text { from, to, text, .. } => Ok(Some(InputIdentity::Text(*from, *to, text))),
            WindowEntry::Image { from, to, derivative, .. } => derivatives
                .get(*derivative)
                .map(|d| Some(InputIdentity::Image(*from, *to, &d.media_type, &d.blake3)))
                .ok_or_else(|| StorageError("window derivative missing for prefix identity".into())),
            WindowEntry::Note { revision, source, text, .. } => {
                Ok(Some(InputIdentity::Note(*revision, *source, text)))
            }
        })
        .collect()
}

impl CommittedWindow {
    pub(crate) fn effective_entries(&self, history: &dyn HistorySource) -> Result<Vec<WindowEntry>, StorageError> {
        let entries = self.materialize(history)?;
        let raw = match history.leaf_seq()? {
            Some(leaf) => history.read_range(0, leaf)?,
            None => Vec::new(),
        };
        let current_states = call_states(&raw);
        let mut changed = unresolved_calls(&raw);
        for (call_id, states) in &current_states {
            if self.call_states.get(call_id) != Some(states) {
                changed.insert(call_id.clone());
            }
        }
        let mut restore = BTreeSet::new();
        if !changed.is_empty() {
            for range in units(&raw) {
                if raw[range.clone()].iter().any(|item| match &item.kind {
                    ItemKind::ToolCall { call_id, .. } | ItemKind::ToolResult { call_id, .. } => changed.contains(call_id),
                    _ => false,
                }) {
                    restore.extend(raw[range].iter().map(|item| item.at.seq));
                }
            }
        }
        if restore.is_empty() { Ok(entries) } else { rehydrate(entries, &raw, &restore) }
    }
}

impl CommittedWindow {
    pub fn genesis() -> Self {
        Self {
            id: EntityId::new(),
            generation: 0,
            body_version: WINDOW_BODY_VERSION,
            base: None,
            policy_id: None,
            policy_version: None,
            entries: Vec::new(),
            call_states: BTreeMap::new(),
            derivatives: Vec::new(),
            covers_through: None,
            model_generation: 0,
            cost: CostReport::default(),
        }
    }

    /// The model-visible sequence now: stored entries plus every raw event
    /// that arrived after the window was cut.
    pub fn materialize(&self, history: &dyn HistorySource) -> Result<Vec<WindowEntry>, StorageError> {
        let mut out = self.entries.clone();
        let from = self.covers_through.map_or(0, |seq| seq + 1);
        if let Some(leaf) = history.leaf_seq()? {
            if from <= leaf {
                out.extend(
                    history
                        .read_range(from, leaf)?
                        .into_iter()
                        .map(|item| WindowEntry::Verbatim { at: item.at, tokens: item.tokens }),
                );
            }
        }
        Ok(out)
    }
}

/// Structural admission of a window's entries.
///
/// The only rule that is not already expressed by the entry types themselves
/// is the notebook: one window carries at most one authored note, so a
/// committed input can never mix two journal revisions. A clearing revision
/// is not a note — it is the absence of one — and an empty note is a clear
/// that was written as a replacement. Commit and every body reader run this,
/// so neither a strategy nor an edited body can present a mixed notebook.
pub(crate) fn validate_entries(entries: &[WindowEntry]) -> Result<(), StorageError> {
    let mut note: Option<u64> = None;
    for entry in entries {
        let WindowEntry::Note { revision, source, text, .. } = entry else { continue };
        if let Some(first) = note {
            return Err(StorageError(format!(
                "window carries notebook revisions {first} and {revision}; one input shows one revision"
            )));
        }
        if *source == NoteSource::Reset {
            return Err(StorageError(
                "a clearing revision is not a window note; the clearing is the absence of one".into(),
            ));
        }
        if text.trim().is_empty() {
            return Err(StorageError(format!(
                "window note revision {revision} is empty; clear the notebook instead"
            )));
        }
        note = Some(*revision);
    }
    Ok(())
}

fn call_states(items: &[HistoryItem]) -> BTreeMap<String, Vec<(u64, CallStatus)>> {
    let mut states = BTreeMap::new();
    for item in items {
        match &item.kind {
            ItemKind::ToolCall { call_id, status, .. } | ItemKind::ToolResult { call_id, status } => {
                states.entry(call_id.clone()).or_insert_with(Vec::new).push((item.at.seq, *status));
            }
            _ => {}
        }
    }
    states
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
pub enum CommitError {
    #[error("window was prepared against {prepared}, but {current} is current")]
    StaleSource { prepared: EntityId, current: EntityId },
    #[error("model generation moved from {prepared} to {current}")]
    ModelGenerationChanged { prepared: u64, current: u64 },
    #[error("policy {policy} epoch {epoch} is no longer installed")]
    StalePolicy { policy: String, epoch: u64 },
    #[error("prepared leaf {prepared} is ahead of the history leaf {leaf:?}")]
    LeafAhead { prepared: u64, leaf: Option<u64> },
    #[error(transparent)]
    Storage(#[from] StorageError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAppend {
    Appended,
    SourceChanged,
    ModelGenerationChanged { current: u64 },
}

/// Durable home of one scope's window chain. The ledger validates; the log
/// stores and enforces the head compare-and-append, so two writers cannot
/// both extend the same head.
pub trait WindowLog: Send + Sync {
    /// The branch/scope this chain belongs to. A prepared window is bound to
    /// it, so a window prepared for one branch can never be committed into
    /// another branch's chain.
    fn scope_id(&self) -> EntityId;
    /// Current head; an empty log creates and returns its genesis window.
    fn head(&self) -> Result<CommittedWindow, StorageError>;
    /// Every committed window, oldest first; none is ever discarded.
    fn all(&self) -> Result<Vec<CommittedWindow>, StorageError>;
    /// Atomically append while both the source head and model generation match.
    fn append(
        &self,
        expected: EntityId,
        expected_model_generation: u64,
        window: &CommittedWindow,
    ) -> Result<WindowAppend, StorageError>;
    fn model_generation(&self) -> Result<u64, StorageError>;
    fn model_profile(&self) -> Result<Option<(u64, String)>, StorageError>;
    fn rebind_model(&self, generation: u64, profile_digest: &str) -> Result<(), StorageError>;
    /// The highest installation epoch any committed window of this chain was
    /// prepared under. A new installation starts above it, so a window
    /// prepared by an earlier process can never commit.
    fn max_policy_epoch(&self) -> Result<u64, StorageError>;
}

/// In-process log for strategies and tests without a durable store.
pub struct MemoryWindowLog {
    scope_id: EntityId,
    state: Mutex<(Vec<CommittedWindow>, u64, Option<String>)>,
}

impl Default for MemoryWindowLog {
    fn default() -> Self {
        Self {
            scope_id: EntityId::new(),
            state: Mutex::new((vec![CommittedWindow::genesis()], 0, None)),
        }
    }
}

impl WindowLog for MemoryWindowLog {
    fn scope_id(&self) -> EntityId {
        self.scope_id
    }

    fn head(&self) -> Result<CommittedWindow, StorageError> {
        Ok(self.state.lock().0.last().cloned().expect("log always has genesis"))
    }

    fn all(&self) -> Result<Vec<CommittedWindow>, StorageError> {
        Ok(self.state.lock().0.clone())
    }

    fn append(
        &self,
        expected: EntityId,
        expected_model_generation: u64,
        window: &CommittedWindow,
    ) -> Result<WindowAppend, StorageError> {
        let mut state = self.state.lock();
        if state.0.last().map(|w| w.id) != Some(expected) {
            return Ok(WindowAppend::SourceChanged);
        }
        if state.1 != expected_model_generation {
            return Ok(WindowAppend::ModelGenerationChanged { current: state.1 });
        }
        state.0.push(window.clone());
        Ok(WindowAppend::Appended)
    }

    fn model_generation(&self) -> Result<u64, StorageError> {
        Ok(self.state.lock().1)
    }

    fn model_profile(&self) -> Result<Option<(u64, String)>, StorageError> {
        let state = self.state.lock();
        Ok(state.2.as_ref().map(|profile| (state.1, profile.clone())))
    }

    fn rebind_model(&self, generation: u64, profile_digest: &str) -> Result<(), StorageError> {
        let mut state = self.state.lock();
        if state.1 == generation && state.2.as_deref() == Some(profile_digest) {
            return Ok(());
        }
        if generation <= state.1 || profile_digest.is_empty() {
            return Err(StorageError("model generation and profile must advance together".into()));
        }
        let previous = state.0.last().expect("log always has genesis");
        let mut root = CommittedWindow::genesis();
        root.base = Some(WindowBase {
            source_window: previous.id, leaf_seq: 0,
            model_generation: previous.model_generation, policy_epoch: 0,
        });
        root.generation = previous.generation + 1;
        root.model_generation = generation;
        state.0.push(root);
        state.1 = generation;
        state.2 = Some(profile_digest.to_owned());
        Ok(())
    }

    fn max_policy_epoch(&self) -> Result<u64, StorageError> {
        Ok(self
            .state
            .lock()
            .0
            .iter()
            .filter_map(|window| window.base.as_ref().map(|base| base.policy_epoch))
            .max()
            .unwrap_or(0))
    }
}

pub struct WindowLedger {
    log: Arc<dyn WindowLog>,
    /// Serializes this process's validate-then-append; the log's
    /// compare-and-append covers any other writer.
    commit: Mutex<()>,
}

impl Default for WindowLedger {
    fn default() -> Self {
        Self::new(Arc::new(MemoryWindowLog::default()))
    }
}

impl WindowLedger {
    pub fn new(log: Arc<dyn WindowLog>) -> Self {
        Self { log, commit: Mutex::new(()) }
    }

    pub fn current(&self) -> Result<CommittedWindow, StorageError> {
        self.log.head()
    }

    /// Every committed window, oldest first; none is ever discarded.
    pub fn all(&self) -> Result<Vec<CommittedWindow>, StorageError> {
        self.log.all()
    }

    pub fn model_generation(&self) -> Result<u64, StorageError> {
        self.log.model_generation()
    }

    /// Record a rebind by appending a full-raw root in the same transaction.
    /// Old compressed prefixes stay readable but cannot become the new
    /// binding's input, and old prepared windows cannot commit.
    pub fn model_profile(&self) -> Result<Option<(u64, String)>, StorageError> {
        self.log.model_profile()
    }

    pub fn rebind_model(&self, generation: u64, profile_digest: &str) -> Result<(), StorageError> {
        self.log.rebind_model(generation, profile_digest)
    }

    /// The branch/scope this chain belongs to.
    pub fn scope_id(&self) -> EntityId {
        self.log.scope_id()
    }

    /// The highest installation epoch used by a committed window of this
    /// chain. A new installation starts above it.
    pub fn max_policy_epoch(&self) -> Result<u64, StorageError> {
        self.log.max_policy_epoch()
    }

    /// Verify the base still holds and commit atomically. Events appended after
    /// the prepared leaf stay visible because materialization carries them.
    pub fn commit(
        &self,
        prepared: PreparedWindow,
        history: &dyn HistorySource,
    ) -> Result<CommittedWindow, CommitError> {
        let _serial = self.commit.lock();
        let current = self.log.head()?;
        if prepared.base.source_window != current.id {
            return Err(CommitError::StaleSource {
                prepared: prepared.base.source_window,
                current: current.id,
            });
        }
        let model_generation = self.log.model_generation()?;
        if prepared.base.model_generation != model_generation {
            return Err(CommitError::ModelGenerationChanged {
                prepared: prepared.base.model_generation,
                current: model_generation,
            });
        }
        let leaf = history.leaf_seq()?;
        if leaf.is_none_or(|leaf| prepared.base.leaf_seq > leaf) {
            return Err(CommitError::LeafAhead { prepared: prepared.base.leaf_seq, leaf });
        }

        let call_states = call_states(&history.read_range(0, prepared.base.leaf_seq)?);
        let previous = current.effective_entries(history)?;
        let mut window = CommittedWindow {
            id: EntityId::new(),
            generation: current.generation + 1,
            body_version: WINDOW_BODY_VERSION,
            base: Some(prepared.base),
            policy_id: Some(prepared.policy.id),
            policy_version: Some(prepared.policy.version),
            entries: prepared.entries,
            call_states,
            derivatives: prepared.derivatives,
            covers_through: Some(prepared.base.leaf_seq),
            model_generation: prepared.base.model_generation,
            cost: prepared.cost,
        };
        // A window that mixes two notebook revisions is not a window of this
        // branch: refuse it before it becomes a committed input.
        validate_entries(&window.entries)?;
        let next = window.effective_entries(history)?;
        // Reusable only when the binding is unchanged and the final input of
        // each leading entry is identical, derivative bytes included.
        let crossed_model_generation = current.base.as_ref()
            .is_some_and(|base| base.model_generation != current.model_generation);
        let reused: u32 = if !crossed_model_generation && current.model_generation == window.model_generation {
            identities(&previous, &current.derivatives, history)?
                .into_iter()
                .zip(identities(&next, &window.derivatives, history)?)
                .zip(&next)
                .take_while(|((a, b), _)| a.is_some() && a == b)
                .map(|(_, entry)| entry.tokens())
                .sum()
        } else {
            0
        };
        let total: u32 = next.iter().map(WindowEntry::tokens).sum();
        window.cost.reusable_prefix_tokens = reused;
        window.cost.recompute_tokens = total - reused;
        window.cost.raw_events_covered = covered_raw_events(&window.entries);
        window.cost.open_calls = open_calls(&window.call_states);
        if window.cost.kv_accounting.is_empty() {
            window.cost.kv_accounting = KV_ACCOUNTING.to_string();
        }
        match self.log.append(current.id, window.model_generation, &window)? {
            WindowAppend::Appended => Ok(window),
            WindowAppend::SourceChanged => {
                let current = self.log.head()?.id;
                Err(CommitError::StaleSource { prepared: prepared.base.source_window, current })
            }
            WindowAppend::ModelGenerationChanged { current } => {
                Err(CommitError::ModelGenerationChanged { prepared: window.model_generation, current })
            }
        }
    }
}

/// How the KV figure of a committed window was obtained. It is an input
/// identity bound estimate, never a backend cache-hit measurement.
pub const KV_ACCOUNTING: &str = "input-identity bound prefix reuse estimate (same raw event, derived text and derivative bytes under the same model/template/encoder generation); not a backend cache-hit measurement";

/// How many distinct raw events the window's entries stand for. The raw
/// events themselves stay in the store and stay readable.
///
/// A [`WindowEntry::Note`] stands for no raw event: counting it would claim
/// coverage of a range it never names.
fn covered_raw_events(entries: &[WindowEntry]) -> u64 {
    let mut covered = BTreeSet::new();
    for entry in entries {
        match entry {
            WindowEntry::Verbatim { at, .. } => {
                covered.insert(at.seq);
            }
            WindowEntry::Pointer { from, to, .. }
            | WindowEntry::Text { from, to, .. }
            | WindowEntry::Image { from, to, .. } => {
                for seq in *from..=*to {
                    covered.insert(seq);
                }
            }
            WindowEntry::Note { .. } => {}
        }
    }
    covered.len() as u64
}

/// How many calls the committed window still shows as unresolved.
///
/// The durable fact is the call-state baseline the window committed with;
/// this count is derived from it, so it never becomes a second truth about
/// which calls are open.
fn open_calls(states: &BTreeMap<String, Vec<(u64, CallStatus)>>) -> u32 {
    states
        .values()
        .filter_map(|entries| entries.last().map(|(_, status)| *status))
        .filter(|status| !status.is_resolved())
        .count() as u32
}

/// One model-visible element of a window, provider-neutral. Mapping these to
/// a provider's message/part wire format (roles, tool messages, image parts)
/// belongs to the inference binding (§47), not to the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Projected {
    /// A raw event, with its kind and ledger status exactly as recorded.
    Event(HistoryItem),
    /// Window-produced text (pointer notes, derived text), never raw truth.
    Note { from: u64, to: u64, text: String },
    /// A rendered frame; a lossy derivative of the given raw range.
    Frame { from: u64, to: u64, media_type: String, bytes: Arc<[u8]> },
    /// The branch's authored working notebook. It has no raw range: the text
    /// is working state written while working, so it is neither a raw event
    /// nor a derivative of one.
    WorkingNote { revision: u64, source: NoteSource, text: String },
}

impl CommittedWindow {
    /// Resolve the materialized window into what the model actually sees.
    /// A raw event or frame that cannot be resolved fails the projection:
    /// a context silently missing part of the record is not a valid input.
    pub fn project(&self, history: &dyn HistorySource) -> Result<Vec<Projected>, StorageError> {
        let entries = self.effective_entries(history)?;
        entries.into_iter().map(|entry| match entry {
                WindowEntry::Verbatim { at, .. } => history
                    .read_event(at.event_id)?
                    .filter(|item| item.at == at)
                    .map(Projected::Event)
                    .ok_or_else(|| StorageError(format!("raw event {} at seq {} is missing", at.event_id, at.seq))),
                WindowEntry::Pointer { from, to, note: text, .. } | WindowEntry::Text { from, to, text, .. } => {
                    Ok(Projected::Note { from, to, text })
                }
                WindowEntry::Note { revision, source, text, .. } => {
                    Ok(Projected::WorkingNote { revision, source, text })
                }
                WindowEntry::Image { from, to, derivative, .. } => self
                    .derivatives
                    .get(derivative)
                    .map(|d| Projected::Frame { from, to, media_type: d.media_type.clone(), bytes: Arc::from(d.bytes.as_slice()) })
                    .ok_or_else(|| StorageError(format!("window {} lacks derivative {derivative}", self.id))),
            })
            .collect()
    }
}

fn emit_raw(
    out: &mut Vec<WindowEntry>, emitted: &mut BTreeSet<u64>, raw: &[HistoryItem], from: u64, to: u64,
) -> Result<(), StorageError> {
    let expected = to.checked_sub(from).and_then(|span| span.checked_add(1))
        .ok_or_else(|| StorageError(format!("invalid raw range {from}..={to}")))?;
    let mut found = 0u64;
    for item in raw.iter().filter(|item| from <= item.at.seq && item.at.seq <= to) {
        found += 1;
        if emitted.insert(item.at.seq) {
            out.push(WindowEntry::Verbatim { at: item.at, tokens: item.tokens });
        }
    }
    if found != expected {
        return Err(StorageError(format!("raw range {from}..={to} is incomplete")));
    }
    Ok(())
}

fn lossy_range(entry: &WindowEntry) -> Result<Option<(u64, u64)>, StorageError> {
    let range = match entry {
        WindowEntry::Verbatim { .. } => return Ok(None),
        // A note has no range to restore: it is branch working state, and
        // rehydrating a raw range must not withdraw what the model wrote
        // about the whole branch.
        WindowEntry::Note { .. } => return Ok(None),
        WindowEntry::Pointer { from, to, .. } | WindowEntry::Text { from, to, .. }
        | WindowEntry::Image { from, to, .. } => (*from, *to),
    };
    if range.0 > range.1 {
        return Err(StorageError(format!("invalid lossy range {}..={}", range.0, range.1)));
    }
    Ok(Some(range))
}

fn coalesce(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        if let Some(last) = out.last_mut() {
            if from <= last.1 {
                last.1 = last.1.max(to);
                continue;
            }
        }
        out.push((from, to));
    }
    out
}

/// Coalesce every overlapping pointer/text/frame range before restoring.
/// Otherwise an interior call would be emitted before the older frame prefix.
fn rehydrate(
    entries: Vec<WindowEntry>, raw: &[HistoryItem], restore: &BTreeSet<u64>,
) -> Result<Vec<WindowEntry>, StorageError> {
    let mut groups = Vec::new();
    for entry in &entries {
        if let Some((from, to)) = lossy_range(entry)? {
            if restore.range(from..=to).any(|_| true) {
                groups.push((from, to));
            }
        }
    }
    groups = coalesce(groups);
    while !groups.is_empty() {
        let mut expanded = groups.clone();
        for entry in &entries {
            if let Some(range) = lossy_range(entry)? {
                if groups.iter().any(|(from, to)| range.0 <= *to && range.1 >= *from) {
                    expanded.push(range);
                }
            }
        }
        expanded = coalesce(expanded);
        if expanded == groups {
            break;
        }
        groups = expanded;
    }
    let mut out = Vec::with_capacity(entries.len() + restore.len());
    let mut emitted = BTreeSet::new();
    let mut emitted_groups = BTreeSet::new();
    for entry in entries {
        match entry {
            WindowEntry::Verbatim { at, tokens } => {
                if groups.iter().any(|(from, to)| *from <= at.seq && at.seq <= *to) {
                    continue;
                }
                if restore.contains(&at.seq) {
                    emit_raw(&mut out, &mut emitted, raw, at.seq, at.seq)?;
                } else if !emitted.contains(&at.seq) {
                    out.push(WindowEntry::Verbatim { at, tokens });
                }
            }
            entry @ (WindowEntry::Pointer { .. } | WindowEntry::Text { .. } | WindowEntry::Image { .. }) => {
                let (from, to) = lossy_range(&entry)?.expect("lossy entry has a range");
                if let Some(index) = groups.iter().position(|(start, end)| *start <= from && to <= *end) {
                    if emitted_groups.insert(index) {
                        emit_raw(&mut out, &mut emitted, raw, groups[index].0, groups[index].1)?;
                    }
                } else {
                    out.push(entry);
                }
            }
            // Restoring raw events cannot withdraw or duplicate branch
            // working state: the note does not stand for any of them.
            entry @ WindowEntry::Note { .. } => out.push(entry),
        }
    }
    if let Some(missing) = restore.iter().find(|seq| !emitted.contains(seq)) {
        return Err(StorageError(format!("changed call requires missing raw seq {missing}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{estimate_tokens, CostReport, MemoryHistory, PolicyManifest, PreparedWindow};

    #[test]
    fn changed_execution_status_restores_the_whole_original_unit() {
        let history = MemoryHistory::new();
        let call_id = "call-1";
        history.append(ItemKind::ToolCall {
            call_id: call_id.into(), tool: "native".into(), args_ref: "args".into(),
            status: CallStatus::Executed { ok: true },
        }, "call", 10);
        history.append(ItemKind::User, "interleaved", 10);
        history.append(ItemKind::ToolResult {
            call_id: call_id.into(), status: CallStatus::Executed { ok: true },
        }, "result", 10);
        let ledger = WindowLedger::default();
        let base = WindowBase {
            source_window: ledger.current().unwrap().id, leaf_seq: 2,
            model_generation: 0, policy_epoch: 0,
        };
        let note = "archived completed call".to_string();
        let prepared = PreparedWindow {
            base,
            policy: PolicyManifest {
                id: "fixture".into(),
                version: "1".into(),
                contract: crate::CONTRACT_VERSION,
                requires: vec![],
                ..PolicyManifest::default()
            },
            entries: vec![WindowEntry::Pointer { from: 0, to: 2, tokens: estimate_tokens(&note), note }],
            carried_calls: vec![], derivatives: vec![], cost: CostReport::default(),
        };
        let window = ledger.commit(prepared, &history).unwrap();
        assert!(window.project(&history).unwrap().iter().all(|item| matches!(item, Projected::Note { .. })));
        history.set_call_status(call_id, CallStatus::Unknown);
        let projected = window.project(&history).unwrap();
        assert_eq!(projected.len(), 3);
        assert!(projected.iter().all(|item| matches!(item, Projected::Event(_))));
        assert!(projected.iter().any(|item| matches!(item,
            Projected::Event(event) if matches!(&event.kind, ItemKind::User))));
        history.set_call_status(call_id, CallStatus::Executed { ok: true });
        assert!(window.project(&history).unwrap().iter().all(|item| matches!(item, Projected::Note { .. })));
    }

    #[test]
    fn a_window_carries_one_notebook_revision_or_none() {
        let history = MemoryHistory::new();
        history.append(ItemKind::User, "hello", 10);
        let ledger = WindowLedger::default();
        let base = WindowBase {
            source_window: ledger.current().unwrap().id, leaf_seq: 0,
            model_generation: 0, policy_epoch: 0,
        };
        let policy = PolicyManifest {
            id: "fixture".into(),
            version: "1".into(),
            contract: crate::CONTRACT_VERSION,
            requires: vec![],
            ..PolicyManifest::default()
        };
        let note = |revision: u64, source: NoteSource, text: &str| WindowEntry::Note {
            revision, source, text: text.into(), tokens: estimate_tokens(text),
        };
        let commit = |entries: Vec<WindowEntry>| {
            ledger.commit(PreparedWindow {
                base: base.clone(), policy: policy.clone(), entries,
                carried_calls: vec![], derivatives: vec![], cost: CostReport::default(),
            }, &history)
        };

        // Two revisions in one input is a merge, not a window of this branch.
        let mixed = commit(vec![
            note(1, NoteSource::Tool, "first"),
            note(2, NoteSource::Tool, "second"),
        ]);
        assert!(matches!(&mixed, Err(CommitError::Storage(error))
            if error.to_string().contains("one input shows one revision")));
        // A clearing revision is the absence of a note, not a note.
        let cleared = commit(vec![note(3, NoteSource::Reset, "")]);
        assert!(matches!(&cleared, Err(CommitError::Storage(error))
            if error.to_string().contains("clearing revision is not a window note")));
        // An empty replacement is what `clear` already journals.
        let empty = commit(vec![note(4, NoteSource::Tool, "  ")]);
        assert!(matches!(&empty, Err(CommitError::Storage(error))
            if error.to_string().contains("is empty; clear the notebook instead")));
        // The same revision is admitted and names no raw coverage.
        let window = commit(vec![note(1, NoteSource::Tool, "first")]).unwrap();
        assert_eq!(window.cost.raw_events_covered, 0);
    }

    #[test]
    fn restoring_raw_events_keeps_the_notebook_once() {
        let history = MemoryHistory::new();
        let call_id = "notebook-call";
        history.append(ItemKind::ToolCall {
            call_id: call_id.into(), tool: "native".into(), args_ref: "args".into(),
            status: CallStatus::Executed { ok: true },
        }, "call", 10);
        history.append(ItemKind::ToolResult {
            call_id: call_id.into(), status: CallStatus::Executed { ok: true },
        }, "result", 10);
        let ledger = WindowLedger::default();
        let base = WindowBase {
            source_window: ledger.current().unwrap().id, leaf_seq: 1,
            model_generation: 0, policy_epoch: 0,
        };
        let prepared = PreparedWindow {
            base,
            policy: PolicyManifest {
                id: "fixture".into(),
                version: "1".into(),
                contract: crate::CONTRACT_VERSION,
                requires: vec![],
                ..PolicyManifest::default()
            },
            entries: vec![
                WindowEntry::Note {
                    revision: 2, source: NoteSource::Tool,
                    text: "keep the failing case in mind".into(), tokens: 8,
                },
                WindowEntry::Pointer { from: 0, to: 1, note: "stand-in".into(), tokens: 2 },
            ],
            carried_calls: vec![], derivatives: vec![], cost: CostReport::default(),
        };
        let window = ledger.commit(prepared, &history).unwrap();
        let notes = |projected: &[Projected]| -> Vec<(u64, String)> {
            projected.iter().filter_map(|item| match item {
                Projected::WorkingNote { revision, text, .. } => Some((*revision, text.clone())),
                _ => None,
            }).collect()
        };
        assert_eq!(notes(&window.project(&history).unwrap()), vec![(2, "keep the failing case in mind".into())]);
        // The covered call changed, so its raw unit is restored — the note is
        // working state about the branch and must survive that restoration.
        history.set_call_status(call_id, CallStatus::Unknown);
        let restored = window.project(&history).unwrap();
        assert_eq!(notes(&restored), vec![(2, "keep the failing case in mind".into())]);
        let seqs: Vec<u64> = restored.iter().filter_map(|item| match item {
            Projected::Event(event) => Some(event.at.seq),
            _ => None,
        }).collect();
        assert_eq!(seqs, vec![0, 1], "the changed unit is restored by its raw range");
    }

    #[test]
    fn overlapping_pointer_and_frame_restore_in_source_order() {
        let history = MemoryHistory::new();
        let call_id = "frame-call";
        history.append(ItemKind::ToolCall {
            call_id: call_id.into(), tool: "native".into(), args_ref: "args".into(),
            status: CallStatus::Executed { ok: true },
        }, "call", 10);
        history.append(ItemKind::User, "interleaved", 10);
        history.append(ItemKind::ToolResult {
            call_id: call_id.into(), status: CallStatus::Executed { ok: true },
        }, "result", 10);
        let ledger = WindowLedger::default();
        let base = WindowBase {
            source_window: ledger.current().unwrap().id, leaf_seq: 2,
            model_generation: 0, policy_epoch: 0,
        };
        let prepared = PreparedWindow {
            base,
            policy: PolicyManifest {
                id: "fixture".into(),
                version: "1".into(),
                contract: crate::CONTRACT_VERSION,
                requires: vec![],
                ..PolicyManifest::default()
            },
            entries: vec![
                WindowEntry::Pointer { from: 0, to: 2, note: "pointer".into(), tokens: 1 },
                WindowEntry::Image { from: 0, to: 2, derivative: 0, tokens: 1 },
            ],
            carried_calls: vec![], derivatives: vec![Derivative::new("image/png", b"frame".to_vec())],
            cost: CostReport::default(),
        };
        let window = ledger.commit(prepared, &history).unwrap();
        history.set_call_status(call_id, CallStatus::Unknown);
        let projected = window.project(&history).unwrap();
        let seqs = projected.iter().map(|item| match item {
            Projected::Event(event) => event.at.seq,
            _ => panic!("overlapping lossy group was not restored"),
        }).collect::<Vec<_>>();
        assert_eq!(seqs, vec![0, 1, 2]);
        assert_eq!(window.entries.len(), 2, "committed window itself remains immutable");
    }
}
