//! Notes-backed rollover compact — an independently installable ContextPolicy
//! plugin (SPEC §12.3.1, "OMP notes-backed windows").
//!
//! The adopted behaviour is the installed OMP rollover contract (docs
//! `compaction.md`, "Experimental notes-backed context windows"; the SPEC pins
//! OMP `0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0` and snapcompact 18.12.6):
//!
//! * a rollover starts a new window at a safe boundary and retains the latest
//!   visible notebook plus complete recent tool-call/result units — no
//!   summarization model is called;
//! * the notebook is carried inside the stand-in entry of the dropped run it
//!   replaces, so every entry's `from..=to` is exactly the raw range it stands
//!   for. A notebook never gets a range of its own: an invented range would
//!   claim raw events another entry already shows, double-count them, and be
//!   discarded by the first raw restoration of that range;
//! * an explicit request for a new window at the next safe boundary is a
//!   boundary the model asked for, so it cuts a window even when a small
//!   history drops nothing. Such a window carries the complete recent units
//!   and unresolved calls verbatim and invents no range for the notebook,
//!   which stays journaled and readable through the `read` operation;
//! * the notebook is branch-journaled: a replacement is a new revision of this
//!   scope, and clearing it hides the notebook without deleting its history;
//! * the raw history is never rewritten, so the original events remain
//!   recoverable by seq/event id through the installed history read/grep
//!   definition (`history.query`);
//! * near the automatic threshold the model receives a once-per-window
//!   reminder to save its working state.
//!
//! Intentional differences from OMP, kept explicit:
//!
//! * OMP's `context_notes` / `new_context` tool names are not copied. This
//!   product installs one native definition, `context.notebook`, whose `read`,
//!   `replace`, `clear` and `rollover` operations cover the same behaviour and
//!   whose withdrawal follows the policy unit's own Fiber.
//! * OMP rebinds the notebook to the session branch directly; here the notebook
//!   revision belongs to the context chain's branch scope, and the window cut
//!   records the raw leaf it read, so a stale notebook can never be attached to
//!   a younger window by accident.
//! * OMP's `history://current/full` URI does not exist here; the original
//!   evidence path is the caller's own committed raw session through
//!   `history.query`, which is equally immutable and window-independent.
use std::borrow::Cow;
use std::sync::Arc;

use cordis_core::{Context, InjectSpec, Plugin, Service};
use shenko_context_policy::{
    Availability, CONTRACT_VERSION, ContextInput, ContextPolicy, CostReport, ItemKind,
    ModelCaps, NoteRevision, NotesJournal, PolicyError, PolicyManifest, PolicyRegistry,
    PreparedWindow, TriggerInput, WindowEntry, estimate_tokens, install_on, unit_is_unfinished,
    units, unresolved_calls,
};

pub mod notebook_task;

pub use notebook_task::ContextNotebookHandler;

pub const POLICY_ID: &str = "notes-rollover-compact";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RolloverConfig {
    /// Automatic trigger: start a new window once the used input exceeds this
    /// share of the model context. An explicit rollover request fires earlier.
    pub trigger_ratio: f32,
    /// Share of the model context at which the once-per-window reminder to
    /// save working state is issued. It never cuts a window by itself.
    pub reminder_ratio: f32,
    /// Complete recent units retained verbatim. Unfinished call units are
    /// always retained whatever this value is.
    pub retain_units: usize,
}

impl Default for RolloverConfig {
    fn default() -> Self {
        Self { trigger_ratio: 0.85, reminder_ratio: 0.7, retain_units: 4 }
    }
}

#[derive(Debug, Clone)]
pub struct NotesRollover {
    config: RolloverConfig,
    notes: Arc<dyn NotesJournal>,
}

impl NotesRollover {
    pub fn new(config: RolloverConfig, notes: Arc<dyn NotesJournal>) -> Self {
        Self { config, notes }
    }

    /// The default configuration document of this strategy.
    pub fn default_config() -> serde_json::Value {
        let config = RolloverConfig::default();
        serde_json::json!({
            "trigger_ratio": config.trigger_ratio,
            "reminder_ratio": config.reminder_ratio,
            "retain_units": config.retain_units,
        })
    }

    /// Read a configuration document. A malformed field is an error, never a
    /// silently ignored default.
    pub fn config_from_json(value: &serde_json::Value) -> Result<RolloverConfig, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "notes rollover configuration must be an object".to_string())?;
        let default = RolloverConfig::default();
        let ratio = |key: &str, fallback: f32| -> Result<f32, String> {
            match object.get(key) {
                None => Ok(fallback),
                Some(value) => value
                    .as_f64()
                    .map(|number| number as f32)
                    .ok_or_else(|| format!("notes rollover {key} must be a number")),
            }
        };
        let config = RolloverConfig {
            trigger_ratio: ratio("trigger_ratio", default.trigger_ratio)?,
            reminder_ratio: ratio("reminder_ratio", default.reminder_ratio)?,
            retain_units: match object.get("retain_units") {
                None => default.retain_units,
                Some(value) => value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|value| *value <= 64)
                    .ok_or_else(|| "notes rollover retain_units must be an integer in 0..=64".to_string())?,
            },
        };
        let valid = |value: f32| value > 0.0 && value <= 1.0;
        if !valid(config.trigger_ratio) || !valid(config.reminder_ratio) {
            return Err("notes rollover ratios must be in (0, 1]".into());
        }
        Ok(config)
    }

    pub fn manifest_document() -> PolicyManifest {
        PolicyManifest {
            id: POLICY_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            contract: CONTRACT_VERSION,
            // Text-only: no vision envelope is required, and no summarization
            // model is called.
            requires: Vec::new(),
            provides: vec!["context.notes-rollover".into(), "context.notebook".into()],
            summary: "Starts a new context window at a safe boundary and keeps the latest journaled notebook plus complete recent tool-call/result units; the raw history stays readable and no summarization model is called.".into(),
            dependencies: Vec::new(),
        }
    }
}

fn pointer_note(from: u64, to: u64) -> String {
    format!(
        "[history seq {from}..={to} left this window; the raw events stay readable by seq or event id through history.query]"
    )
}

/// The stand-in entry for one dropped run of complete units.
///
/// One entry per run keeps coverage exact: `from..=to` is the raw range this
/// entry stands for, so no entry claims events another entry already shows and
/// a later rehydration of a changed call can restore exactly this run.
///
/// The notebook is working state about the whole branch, not a derivative of
/// this range. It travels with the first stand-in because a window has no
/// other place a note without raw coverage can honestly occupy: giving it its
/// own invented range would double-count raw events and would be discarded by
/// the first raw restoration of that range.
fn stand_in(from: u64, to: u64, notebook: Option<&NoteRevision>) -> WindowEntry {
    let pointer = pointer_note(from, to);
    let Some(note) = notebook else {
        let tokens = estimate_tokens(&pointer);
        return WindowEntry::Pointer { from, to, note: pointer, tokens };
    };
    let text = format!(
        "{pointer}\n\n[working notebook r{} ({}), latest visible revision of this branch; bounded working state, not a transcript — the raw events stay readable through history.query by seq or event id]\n{}",
        note.revision,
        note.source.label(),
        note.text
    );
    let tokens = estimate_tokens(&text);
    WindowEntry::Text { from, to, text, tokens }
}

impl ContextPolicy for NotesRollover {
    fn manifest(&self) -> PolicyManifest {
        Self::manifest_document()
    }

    fn availability(&self, _caps: &ModelCaps) -> Availability {
        // The notebook is text; no model capability gates the strategy. The
        // journal is local storage, so an unreadable journal would surface as
        // a preparation error, not as a capability limit.
        Availability::Available
    }

    fn should_compact(&self, trigger: &TriggerInput<'_>) -> bool {
        trigger.used_tokens as f32 > trigger.caps.max_context as f32 * self.config.trigger_ratio
    }

    fn prepare(&self, input: &ContextInput<'_>) -> Result<PreparedWindow, PolicyError> {
        let items = input.items;
        let units = units(items);
        let unresolved = unresolved_calls(items);
        let system_units = units
            .iter()
            .take_while(|unit| items[unit.start..unit.end].iter().all(|item| item.kind == ItemKind::System))
            .count();
        let keep_from = units
            .len()
            .saturating_sub(self.config.retain_units)
            .max(system_units);

        // One read of the journal head decides what this window carries: the
        // newest revision only, and nothing when the newest revision is a
        // clear. Reading the journal again after the entries were built could
        // attach a revision a concurrent replacement already retired.
        let mut notebook = self
            .notes
            .head()
            .map_err(|error| PolicyError::Other(format!("notebook is unreadable: {error}")))?
            .filter(|note| note.visible && !note.text.trim().is_empty());

        let verbatim = |entries: &mut Vec<WindowEntry>, range: std::ops::Range<usize>| {
            entries.extend(
                items[range].iter().map(|item| WindowEntry::Verbatim { at: item.at, tokens: item.tokens }),
            );
        };
        // The notebook is taken by the first stand-in entry, so one window
        // never repeats it and no entry claims a range it does not stand for.
        let mut flush = |entries: &mut Vec<WindowEntry>, run: Option<(u64, u64)>| {
            if let Some((from, to)) = run {
                entries.push(stand_in(from, to, notebook.as_ref()));
                notebook = None;
            }
        };
        let mut entries = Vec::new();
        let mut dropped: Option<(u64, u64)> = None;
        for (index, unit) in units.iter().enumerate() {
            let keep = index < system_units
                || index >= keep_from
                || unit_is_unfinished(&items[unit.clone()], &unresolved);
            if keep {
                flush(&mut entries, dropped.take());
                verbatim(&mut entries, unit.clone());
            } else {
                let (first, last) = (items[unit.start].at.seq, items[unit.end - 1].at.seq);
                dropped = Some(dropped.map_or((first, last), |(from, _)| (from, last)));
            }
        }
        flush(&mut entries, dropped.take());

        // A size-triggered preparation that drops nothing is not a rollover:
        // it would replace the same raw history with the same window and cost
        // a full prefill. An explicit request is a boundary the model asked
        // for, so it still cuts a window that keeps every complete recent unit
        // and every unresolved call, even when there is nothing to drop. An
        // empty history is not a boundary in either case.
        let replaced = entries
            .iter()
            .any(|entry| !matches!(entry, WindowEntry::Verbatim { .. }));
        if items.is_empty() || (!replaced && !input.rollover) {
            return Err(PolicyError::NoReduction);
        }

        let mut carried_calls: Vec<String> = unresolved.into_iter().collect();
        carried_calls.sort();
        let cost = CostReport {
            source_tokens: items.iter().map(|item| item.tokens).sum(),
            window_tokens: entries.iter().map(WindowEntry::tokens).sum(),
            model_calls: 0,
            ..CostReport::default()
        };
        Ok(PreparedWindow {
            base: input.base,
            policy: self.manifest(),
            entries,
            carried_calls,
            derivatives: Vec::new(),
            cost,
        })
    }
}

/// Everything the plugin needs beyond its configuration. The note journal and
/// the registry are real services of this process, not values derived from
/// the JSON configuration document.
#[derive(Clone)]
pub struct RolloverHosts {
    pub config: RolloverConfig,
    pub notes: Arc<dyn NotesJournal>,
    pub registry: Arc<PolicyRegistry>,
}

/// A config that passed [`NotesRolloverPlugin`]'s `prepare`. It is the
/// plugin's only Cordis Input, so a same-Fiber update cannot skip validation.
#[derive(Clone)]
pub struct ValidRolloverHosts(RolloverHosts);

impl ValidRolloverHosts {
    pub fn config(&self) -> RolloverConfig {
        self.0.config
    }
}

/// The installable plugin. Disposing its Fiber uninstalls notes rollover and
/// withdraws the reminder threshold it registered; nothing else is touched.
pub struct NotesRolloverPlugin;

impl Plugin for NotesRolloverPlugin {
    type Config = RolloverHosts;
    type Input = ValidRolloverHosts;
    type PrepareError = std::io::Error;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed(POLICY_ID)
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(PolicyRegistry::NAME)
    }

    fn prepare(&self, hosts: RolloverHosts) -> Result<ValidRolloverHosts, std::io::Error> {
        let valid = |r: f32| r > 0.0 && r <= 1.0;
        if !valid(hosts.config.trigger_ratio) || !valid(hosts.config.reminder_ratio) {
            return Err(std::io::Error::other("notes rollover ratios must be in (0, 1]"));
        }
        if hosts.config.retain_units > 64 {
            return Err(std::io::Error::other("notes rollover retain_units must be in 0..=64"));
        }
        if hosts.notes.scope_id() != hosts.registry.ledger().scope_id() {
            return Err(std::io::Error::other(format!(
                "notebook scope {} is not this context chain's scope {}",
                hosts.notes.scope_id(),
                hosts.registry.ledger().scope_id()
            )));
        }
        Ok(ValidRolloverHosts(hosts))
    }

    async fn apply(&self, ctx: Context, hosts: &ValidRolloverHosts) -> Result<(), std::io::Error> {
        let epoch = install_on(
            &ctx,
            Arc::new(NotesRollover::new(hosts.0.config, Arc::clone(&hosts.0.notes))),
        )?;
        // The reminder threshold is this strategy's configuration, so it is
        // registered with this installation's epoch and withdrawn with it.
        let registry = Arc::clone(&hosts.0.registry);
        registry.set_reminder_threshold(epoch, hosts.0.config.reminder_ratio);
        ctx.effect_sync(move || registry.clear_reminder_threshold(epoch))
            .map_err(std::io::Error::other)?;
        Ok(())
    }
}
