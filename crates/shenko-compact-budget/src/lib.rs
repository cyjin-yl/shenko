//! Budget compact — an independently installable ContextPolicy plugin (§12.3.1).
//!
//! Follows Codex `a3d93e1` `compact_token_budget.rs`: no summary model call;
//! start a new context window that keeps the system prefix, every unfinished
//! call unit and a recent tail of whole units, and replaces the rest with
//! pointers the history tools resolve by seq/event id. Raw events are untouched.
use std::borrow::Cow;
use std::sync::Arc;

use cordis_core::{Context, InjectSpec, Plugin, Service};
use shenko_context_policy::{
    Availability, CONTRACT_VERSION, ContextInput, ContextPolicy, CostReport, ItemKind, ModelCaps,
    PolicyError, PolicyManifest, PolicyRegistry, PreparedWindow, WindowEntry, estimate_tokens,
    install_on, unit_is_unfinished, units, unresolved_calls,
};

pub const POLICY_ID: &str = "budget-compact";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BudgetConfig {
    /// Compact once used tokens exceed this share of the model context.
    pub trigger_ratio: f32,
    /// Share of the model context the verbatim recent tail may use.
    pub recent_ratio: f32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self { trigger_ratio: 0.85, recent_ratio: 0.3 }
    }
}

#[derive(Debug, Clone)]
pub struct BudgetCompact {
    config: BudgetConfig,
}

impl BudgetCompact {
    pub fn new(config: BudgetConfig) -> Self {
        Self { config }
    }
}

fn pointer(from: u64, to: u64) -> WindowEntry {
    let note = format!(
        "[history seq {from}..={to} is outside this window; raw events stay readable by seq or event id through the history tools]"
    );
    let tokens = estimate_tokens(&note);
    WindowEntry::Pointer { from, to, note, tokens }
}

impl ContextPolicy for BudgetCompact {
    fn manifest(&self) -> PolicyManifest {
        PolicyManifest {
            id: POLICY_ID.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            contract: CONTRACT_VERSION,
            requires: Vec::new(),
        }
    }

    fn availability(&self, _caps: &ModelCaps) -> Availability {
        Availability::Available
    }

    fn should_compact(&self, input: &ContextInput<'_>) -> bool {
        input.used_tokens as f32 > input.caps.max_context as f32 * self.config.trigger_ratio
    }

    fn prepare(&self, input: &ContextInput<'_>) -> Result<PreparedWindow, PolicyError> {
        let items = input.items;
        let units = units(items);
        let unresolved = unresolved_calls(items);
        let system_units = units
            .iter()
            .take_while(|u| items[u.start..u.end].iter().all(|i| i.kind == ItemKind::System))
            .count();

        // Recent tail of whole units, newest first, within the tail budget.
        let tail_budget = (input.caps.max_context as f32 * self.config.recent_ratio) as u32;
        let mut tail_start = units.len();
        let mut tail_tokens = 0u32;
        while tail_start > system_units {
            let unit = &units[tail_start - 1];
            let tokens: u32 = items[unit.clone()].iter().map(|i| i.tokens).sum();
            if tail_tokens + tokens > tail_budget && tail_start < units.len() {
                break;
            }
            tail_tokens += tokens;
            tail_start -= 1;
        }

        let verbatim = |entries: &mut Vec<WindowEntry>, range: std::ops::Range<usize>| {
            entries.extend(
                items[range].iter().map(|i| WindowEntry::Verbatim { at: i.at, tokens: i.tokens }),
            );
        };
        let mut entries = Vec::new();
        let mut dropped: Option<(u64, u64)> = None;
        for (index, unit) in units.iter().enumerate() {
            let keep = index < system_units
                || index >= tail_start
                || unit_is_unfinished(&items[unit.clone()], &unresolved);
            if keep {
                if let Some((from, to)) = dropped.take() {
                    entries.push(pointer(from, to));
                }
                verbatim(&mut entries, unit.clone());
            } else {
                let (first, last) = (items[unit.start].at.seq, items[unit.end - 1].at.seq);
                dropped = Some(dropped.map_or((first, last), |(from, _)| (from, last)));
            }
        }
        if let Some((from, to)) = dropped {
            entries.push(pointer(from, to));
        }
        if !entries.iter().any(|e| matches!(e, WindowEntry::Pointer { .. })) {
            return Err(PolicyError::NoReduction);
        }

        let mut carried_calls: Vec<String> = unresolved.into_iter().collect();
        carried_calls.sort();
        let cost = CostReport {
            source_tokens: items.iter().map(|i| i.tokens).sum(),
            window_tokens: entries.iter().map(WindowEntry::tokens).sum(),
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

/// The installable plugin. Updating its config is a Cordis restart/update of
/// this Fiber only; disposing it uninstalls budget compact and nothing else.
pub struct BudgetCompactPlugin;

impl Plugin for BudgetCompactPlugin {
    type Config = BudgetConfig;
    type Input = BudgetConfig;
    type PrepareError = std::io::Error;
    type ApplyError = std::io::Error;

    fn name(&self) -> Cow<'_, str> {
        Cow::Borrowed(POLICY_ID)
    }

    fn inject(&self) -> InjectSpec {
        InjectSpec::none().require(PolicyRegistry::NAME)
    }

    fn prepare(&self, config: BudgetConfig) -> Result<BudgetConfig, std::io::Error> {
        let valid = |r: f32| r > 0.0 && r <= 1.0;
        if !valid(config.trigger_ratio) || !valid(config.recent_ratio) {
            return Err(std::io::Error::other("budget compact ratios must be in (0, 1]"));
        }
        Ok(config)
    }

    async fn apply(&self, ctx: Context, input: &BudgetConfig) -> Result<(), std::io::Error> {
        install_on(&ctx, Arc::new(BudgetCompact::new(*input)))?;
        Ok(())
    }
}
