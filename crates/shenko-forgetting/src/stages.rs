//! Forgetting stages, macro installation, and the plugin that the subject
//! loop calls to evaluate stage advances.
//!
//! The six stages are a fixed ordering. The forgetting macro carries:
//! - The stage order (immutable).
//! - Weights and principles (revisable content written as definitions).
//! - The plugin entry point that the subject loop calls per-cycle.
//!
//! The plugin does NOT contain a formula. It prepares a [`ForgettingContext`]
//! with all mechanism facts (active runtime, signals, current stage, adoption
//! status) and surfaces it to the subject. The subject decides whether to
//! advance. The plugin records the decision as a `Projection` attitude.

use serde::{Deserialize, Serialize};
use shenko_store::active_runtime::MainActiveRuntime;
use shenko_store::store::Store;
use shenko_types::cognition::{
    AdoptionTarget, AttitudeConfidence, AttitudeRole, ContentAttitude,
    ProjectionStage, ReasonPointer, ReasonTermination,
};
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use tracing::{debug, info, warn};

use crate::signals::{SignalBatch, ForgettingSignal};

/// The fixed stage ordering. Lower index = fresher; higher = more forgotten.
pub const STAGE_ORDER: &[ProjectionStage] = &[
    ProjectionStage::Degraded,
    ProjectionStage::Fuzzy,
    ProjectionStage::NearForgotten,
    ProjectionStage::Archived,
    ProjectionStage::LongTerm,
    ProjectionStage::Forgotten,
];

/// The zero-based position of a stage in the ordering.
pub fn stage_index(stage: ProjectionStage) -> usize {
    match stage {
        ProjectionStage::Degraded => 0,
        ProjectionStage::Fuzzy => 1,
        ProjectionStage::NearForgotten => 2,
        ProjectionStage::Archived => 3,
        ProjectionStage::LongTerm => 4,
        ProjectionStage::Forgotten => 5,
    }
}

/// The stage name for diagnostics.
pub fn stage_name(stage: ProjectionStage) -> &'static str {
    match stage {
        ProjectionStage::Degraded => "degraded",
        ProjectionStage::Fuzzy => "fuzzy",
        ProjectionStage::NearForgotten => "near_forgotten",
        ProjectionStage::Archived => "archived",
        ProjectionStage::LongTerm => "long_term",
        ProjectionStage::Forgotten => "forgotten",
    }
}

/// Whether `from` is earlier (fresher) than `to` in the forgetting ordering.
pub fn is_forward(from: ProjectionStage, to: ProjectionStage) -> bool {
    stage_index(from) < stage_index(to)
}

/// The current projection state of one piece of content, as read from the
/// atoms ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectionRecord {
    /// The content this record is about.
    pub digest: Digest,
    /// The current stage, if any projection attitude exists.
    pub current_stage: Option<ProjectionStage>,
    /// The attitude that set the current stage.
    pub projection_attitude: Option<ContentAttitude>,
    /// Whether this content is adopted by the `forgetting` unit.
    pub adopted: bool,
}

/// The mechanism facts the subject sees when deciding whether to advance a
/// stage. This is observation, not formula.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgettingContext {
    /// The content being evaluated.
    pub digest: Digest,
    /// Current projection record from the ledger.
    pub current: ProjectionRecord,
    /// Main-loop active runtime of the subject at evaluation time.
    pub active_runtime: MainActiveRuntime,
    /// Active time (ms) since the content was last referenced by any signal.
    /// `None` when there is no signal history — unknown last-use is not zero.
    pub active_ms_since_last_use: Option<i64>,
    /// Signals about this content in the current evaluation batch.
    pub batch_signals: Vec<ForgettingSignal>,
    /// Total signal count for this content across all history (if available).
    pub total_signal_count: Option<u64>,
}

/// The outcome of a stage-advance evaluation for one piece of content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageAdvanceOutcome {
    /// No change: content stays at its current stage.
    NoChange,
    /// Content advanced to a new stage; the attitude was recorded.
    Advanced {
        from: Option<ProjectionStage>,
        to: ProjectionStage,
        attitude_id: EntityId,
    },
    /// Content was refreshed (moved to a fresher stage); the attitude was
    /// recorded.
    Refreshed {
        from: ProjectionStage,
        to: ProjectionStage,
        attitude_id: EntityId,
    },
    /// The advance was skipped because a reason was missing or the content
    /// is not adopted.
    Skipped { reason: String },
}

/// The installed forgetting macro. It carries no formula — only the immutable
/// stage order, the unit identity for adoption, and the store reference.
///
/// The macro is installed as `cap.forgetting` v1, recommended by
/// `humanlike-mind` (SPEC §51.8). It can be replaced: a new macro supersedes
/// the old one's attitudes without rewriting them.
pub struct ForgettingMacro {
    /// The unit id used for adoption attitudes.
    unit_id: String,
}

impl ForgettingMacro {
    /// The default unit id for the forgetting macro.
    pub const DEFAULT_UNIT_ID: &'static str = "forgetting";

    pub fn new() -> Self {
        Self {
            unit_id: Self::DEFAULT_UNIT_ID.to_string(),
        }
    }

    pub fn with_unit_id(unit_id: impl Into<String>) -> Self {
        Self {
            unit_id: unit_id.into(),
        }
    }

    pub fn unit_id(&self) -> &str {
        &self.unit_id
    }

    /// Build an adoption target for this macro.
    pub fn adoption_target(&self) -> AdoptionTarget {
        AdoptionTarget::unit(&self.unit_id)
    }

    /// Build an adoption target for a specific member slot.
    pub fn member_target(&self, member: &str) -> AdoptionTarget {
        AdoptionTarget::unit(&self.unit_id).with_member(member)
    }
}

impl Default for ForgettingMacro {
    fn default() -> Self {
        Self::new()
    }
}

/// The forgetting plugin that the subject loop calls. It holds a reference
/// to the store and the installed macro.
///
/// The plugin never runs a formula. It:
/// 1. Reads the current projection record for each content digest.
/// 2. Reads the active runtime from the meter.
/// 3. Assembles a [`ForgettingContext`] with all mechanism facts.
/// 4. When the subject decides to advance, records the new projection
///    attitude through the canonical atoms API.
pub struct ForgettingPlugin {
    macro_def: ForgettingMacro,
}

impl ForgettingPlugin {
    pub fn new(macro_def: ForgettingMacro) -> Self {
        Self { macro_def }
    }

    /// Read the current projection record of one piece of content.
    pub fn projection_record(
        &self,
        store: &Store,
        agent_id: EntityId,
        digest: &Digest,
    ) -> Result<ProjectionRecord, ShenkoError> {
        // Read the latest projection attitude for this content.
        let projection = store.latest_attitude(agent_id, digest, Some("projection"))?;
        let current_stage = projection
            .as_ref()
            .and_then(|attitude| attitude.role.projection_stage());
        let adopted = store.is_adopted(agent_id, self.macro_def.unit_id(), digest)?;
        Ok(ProjectionRecord {
            digest: digest.clone(),
            current_stage,
            projection_attitude: projection,
            adopted,
        })
    }

    /// Prepare the full forgetting context for one piece of content.
    ///
    /// This reads all mechanism facts from the store and meter. The returned
    /// context is what the subject sees when deciding whether to advance.
    pub fn prepare_context(
        &self,
        store: &Store,
        agent_id: EntityId,
        digest: &Digest,
        batch: &SignalBatch,
        now_ms: i64,
    ) -> Result<ForgettingContext, ShenkoError> {
        let current = self.projection_record(store, agent_id, digest)?;
        let active_runtime = store.subject_active_runtime(agent_id)?;

        // Find the most recent signal for this content to compute active
        // time since last use. An unknown last-use stays None.
        let latest_signal_ms = batch
            .signals_for(digest)
            .iter()
            .map(|signal| signal.recorded_at_ms)
            .max();
        let active_ms_since_last_use = store.subject_active_runtime_since(
            agent_id,
            latest_signal_ms,
            now_ms,
        )?;

        let batch_signals: Vec<ForgettingSignal> = batch
            .signals_for(digest)
            .into_iter()
            .cloned()
            .collect();

        Ok(ForgettingContext {
            digest: digest.clone(),
            current,
            active_runtime,
            active_ms_since_last_use,
            batch_signals,
            total_signal_count: None, // caller may fill from signal store
        })
    }

    /// Record a stage advance as a Projection attitude.
    ///
    /// The reasons must include the mechanism facts that led to the decision.
    /// This function validates the advance ordering and records through the
    /// canonical atoms API.
    ///
    /// Returns `StageAdvanceOutcome::Skipped` if the content is not adopted
    /// by the forgetting unit, or if the target stage is not forward from
    /// the current stage (unless `allow_refresh` is true).
    pub fn record_stage_advance(
        &self,
        store: &Store,
        agent_id: EntityId,
        digest: &Digest,
        target_stage: ProjectionStage,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
        allow_refresh: bool,
    ) -> Result<StageAdvanceOutcome, ShenkoError> {
        let record = self.projection_record(store, agent_id, digest)?;

        if !record.adopted {
            return Ok(StageAdvanceOutcome::Skipped {
                reason: "content is not adopted by the forgetting unit".to_string(),
            });
        }

        if reasons.is_empty() {
            return Ok(StageAdvanceOutcome::Skipped {
                reason: "a stage advance requires at least one reason".to_string(),
            });
        }

        // Check ordering
        if let Some(current) = record.current_stage {
            if current == target_stage {
                return Ok(StageAdvanceOutcome::NoChange);
            }
            let forward = is_forward(current, target_stage);
            if !forward && !allow_refresh {
                return Ok(StageAdvanceOutcome::Skipped {
                    reason: format!(
                        "cannot move backward from {} to {} without allow_refresh",
                        stage_name(current),
                        stage_name(target_stage),
                    ),
                });
            }
        }

        // We need the ObjectRef for the content to record the attitude.
        let content = store.content(digest)?.ok_or_else(|| {
            ShenkoError::not_found(format!(
                "content {digest} is not registered in the cognition ledger"
            ))
        })?;

        let draft = shenko_types::cognition::AttitudeDraft::projection(
            target_stage,
            reasons,
            confidence,
        );
        let supersedes = record
            .projection_attitude
            .as_ref()
            .map(|attitude| attitude.attitude_id);
        let draft = if let Some(prev) = supersedes {
            draft.with_supersedes(prev)
        } else {
            draft
        };

        let recorded = store.record_attitude_for_digest(
            agent_id,
            content.object,
            draft,
        )?;

        let from = record.current_stage;
        let attitude_id = recorded.attitude.attitude_id;

        if let Some(from_stage) = from {
            if is_forward(from_stage, target_stage) {
                info!(
                    %digest,
                    from = stage_name(from_stage),
                    to = stage_name(target_stage),
                    "forgetting stage advanced"
                );
                Ok(StageAdvanceOutcome::Advanced {
                    from: Some(from_stage),
                    to: target_stage,
                    attitude_id,
                })
            } else {
                info!(
                    %digest,
                    from = stage_name(from_stage),
                    to = stage_name(target_stage),
                    "forgetting stage refreshed"
                );
                Ok(StageAdvanceOutcome::Refreshed {
                    from: from_stage,
                    to: target_stage,
                    attitude_id,
                })
            }
        } else {
            info!(
                %digest,
                to = stage_name(target_stage),
                "forgetting stage set (first projection)"
            );
            Ok(StageAdvanceOutcome::Advanced {
                from: None,
                to: target_stage,
                attitude_id,
            })
        }
    }

    /// Adopt content into the forgetting unit. This records an Adoption
    /// attitude so that stage advances can proceed.
    pub fn adopt_content(
        &self,
        store: &Store,
        agent_id: EntityId,
        digest: &Digest,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Result<EntityId, ShenkoError> {
        let content = store.content(digest)?.ok_or_else(|| {
            ShenkoError::not_found(format!(
                "content {digest} is not registered in the cognition ledger"
            ))
        })?;

        let draft = shenko_types::cognition::AttitudeDraft::adoption(
            self.macro_def.adoption_target(),
            reasons,
            confidence,
        );
        let recorded = store.record_attitude_for_digest(
            agent_id,
            content.object,
            draft,
        )?;
        info!(%digest, "content adopted into forgetting unit");
        Ok(recorded.attitude.attitude_id)
    }

    /// Write or revise the forgetting weights/principles as definition
    /// content in the `forgetting` definition slot.
    ///
    /// Weights are revisable content: the subject can update them at any
    /// time by calling this with new bytes. Old versions stay in the
    /// revision chain.
    pub fn write_weights(
        &self,
        store: &Store,
        agent_id: EntityId,
        weights_text: &str,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Result<Digest, ShenkoError> {
        use shenko_store::atoms::ContentWrite;

        let write = ContentWrite::text(weights_text);
        let draft = shenko_types::cognition::AttitudeDraft::definition(
            &self.macro_def.unit_id,
            reasons,
            confidence,
        );
        let recorded = store.record_content_attitude(agent_id, write, draft)?;
        info!(
            digest = %recorded.content.digest(),
            "forgetting weights/principles written"
        );
        Ok(recorded.content.digest().clone())
    }

    /// Read the current effective forgetting weights/principles.
    pub fn current_weights(
        &self,
        store: &Store,
        agent_id: EntityId,
    ) -> Result<Option<(Vec<u8>, ContentAttitude)>, ShenkoError> {
        match store.effective_definition(agent_id, &self.macro_def.unit_id)? {
            None => Ok(None),
            Some((content, attitude)) => {
                let bytes = store.content_bytes(content.digest())?;
                Ok(Some((bytes, attitude)))
            }
        }
    }

    /// The definition history of forgetting weights.
    pub fn weights_history(
        &self,
        store: &Store,
        agent_id: EntityId,
    ) -> Result<Vec<ContentAttitude>, ShenkoError> {
        store.definition_history(agent_id, &self.macro_def.unit_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_order_is_monotonic() {
        for pair in STAGE_ORDER.windows(2) {
            assert!(
                stage_index(pair[0]) < stage_index(pair[1]),
                "{:?} should come before {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn is_forward_reflects_ordering() {
        assert!(is_forward(ProjectionStage::Degraded, ProjectionStage::Fuzzy));
        assert!(is_forward(
            ProjectionStage::Degraded,
            ProjectionStage::Forgotten
        ));
        assert!(!is_forward(
            ProjectionStage::Forgotten,
            ProjectionStage::Degraded
        ));
        assert!(!is_forward(
            ProjectionStage::Archived,
            ProjectionStage::Archived
        ));
    }

    #[test]
    fn stage_names_are_stable() {
        assert_eq!(stage_name(ProjectionStage::Degraded), "degraded");
        assert_eq!(stage_name(ProjectionStage::Fuzzy), "fuzzy");
        assert_eq!(stage_name(ProjectionStage::NearForgotten), "near_forgotten");
        assert_eq!(stage_name(ProjectionStage::Archived), "archived");
        assert_eq!(stage_name(ProjectionStage::LongTerm), "long_term");
        assert_eq!(stage_name(ProjectionStage::Forgotten), "forgotten");
    }
}
