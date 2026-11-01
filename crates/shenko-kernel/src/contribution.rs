//! The one merge rule for prepared context contributions — SPEC §41.8.2.
//!
//! Two producers that contribute the same material for the same slot and key
//! are not a race to be won: identical contributions collapse, and any
//! difference is a conflict. The rule lives here, in the kernel, because the
//! hook graph's `collect` synthesis and the context policy registry must
//! merge the same way — there is exactly one rule, not one per caller.
//!
//! The identity of a contribution is `(slot, key)`. SPEC §41.8.2: a conflict
//! may only be merged when the slot declares an explicit reducer; otherwise it
//! is reported as `CONTRIBUTION_CONFLICT`. There is no "the later producer
//! wins" rule anywhere in this path.
use shenko_types::ids::EntityId;
use shenko_types::plugin_runtime::{ContextContribution, ContributionSlot};

/// The literal error code a contribution conflict reports (SPEC §41.8.2).
pub const CONTRIBUTION_CONFLICT: &str = "CONTRIBUTION_CONFLICT";

/// Two producers disagreed about the same (slot, key).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("CONTRIBUTION_CONFLICT: slot {slot:?} key {key} was contributed differently by {producers:?}")]
pub struct ContributionConflict {
    pub slot: ContributionSlot,
    pub key: String,
    /// The plugins that produced the disagreeing contributions.
    pub producers: Vec<EntityId>,
}

impl ContributionConflict {
    /// The stable error code, independent of the message.
    pub fn code(&self) -> &'static str {
        CONTRIBUTION_CONFLICT
    }
}

/// Merge contributions for one (slot, key).
///
/// Byte-identical contributions — same producer, same data reference, same
/// token count, same lifetime and same required flag — collapse into one, so
/// re-collecting the same prepared material is idempotent. Any difference is
/// a conflict: the caller reports it and skips that handler's output; the
/// later producer never silently wins.
pub fn merge_contributions(
    slot: ContributionSlot,
    key: &str,
    contributions: Vec<ContextContribution>,
) -> Result<ContextContribution, ContributionConflict> {
    let mut merged: Option<ContextContribution> = None;
    for contribution in contributions {
        if contribution.slot != slot || contribution.key != key {
            return Err(ContributionConflict {
                slot,
                key: key.to_string(),
                producers: vec![contribution.source.plugin_id],
            });
        }
        match merged.as_ref() {
            None => merged = Some(contribution),
            Some(existing) => {
                if !same_contribution(existing, &contribution) {
                    let mut producers =
                        vec![existing.source.plugin_id, contribution.source.plugin_id];
                    producers.sort();
                    producers.dedup();
                    return Err(ContributionConflict {
                        slot,
                        key: key.to_string(),
                        producers,
                    });
                }
            }
        }
    }
    merged.ok_or(ContributionConflict {
        slot,
        key: key.to_string(),
        producers: Vec::new(),
    })
}

/// Whether two contributions describe the same material.
/// Preparation timestamps are observation metadata; scope, source, input
/// revision and the complete material contract define consumption identity.
pub fn same_contribution(left: &ContextContribution, right: &ContextContribution) -> bool {
    left.slot == right.slot
        && left.key == right.key
        && left.source == right.source
        && left.scope == right.scope
        && left.input_revision == right.input_revision
        && left.lifetime == right.lifetime
        && left.required == right.required
        && left.budget_tokens == right.budget_tokens
        && left.tokens == right.tokens
        && left.data == right.data
}
