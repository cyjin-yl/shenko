//! Subject launch — the launcher-side half of the activation contract.
//!
//! Every time the subject process starts it resolves the *durable* control
//! authority (the activation selector) and binds its launch to it. A launch
//! is not "the binary that happened to run": it is a receipt naming the
//! generation, the selector revision/epoch and the slot the process must
//! serve. Retrying a launch with the same `launch_id` returns the first
//! receipt instead of re-entering the store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shenko_types::activation::ActivationOrigin;
use shenko_types::ids::{EntityId, RuntimeGeneration};

/// One launch of the subject program.
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    /// Deduplication key. A retry with the same id yields the first receipt.
    pub launch_id: EntityId,
    pub installation_id: EntityId,
    pub agent_id: EntityId,
    /// The slot directory this process resolved to start from.
    pub slot_dir: PathBuf,
    /// Why the subject is being (re)launched.
    pub reason: String,
    /// True when the launch must not mutate anything (dry run / inspection).
    pub read_only: bool,
}

/// The durable answer to "which software generation is this process?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubjectLaunchReceipt {
    pub launch_id: EntityId,
    pub slot_dir: PathBuf,
    pub generation: RuntimeGeneration,
    pub selector_revision: u64,
    pub selector_epoch: u64,
    pub origin: ActivationOrigin,
    pub read_only: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("no activation selector for this subject; run genesis or activate first")]
    NoActivationSelector,
    #[error("the launch slot ({slot}) does not match the selector's generation ({selector})")]
    SlotGenerationMismatch {
        slot: String,
        selector: String,
    },
    #[error("the selector belongs to another subject")]
    SelectorSubjectMismatch,
    #[error("the launch slot has no readable manifest: {0}")]
    UnreadableSlot(String),
    #[error(transparent)]
    Store(#[from] shenko_types::error::ShenkoError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// The slot manifest, owned by `shenko-update::slots`. Seed reads it
/// read-only so the launcher can verify the slot/generation binding without
/// depending on the update orchestration crate.
#[derive(Debug, Deserialize)]
struct SlotManifestView {
    schema_version: u32,
    generation: RuntimeGeneration,
}

fn read_slot_manifest(slot_dir: &Path) -> Result<SlotManifestView, LaunchError> {
    let path = slot_dir.join("slot.json");
    let bytes = std::fs::read(&path)
        .map_err(|error| LaunchError::UnreadableSlot(format!("{}: {error}", path.display())))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| LaunchError::UnreadableSlot(format!("{}: {error}", path.display())))
}

/// The slot manifest if one is published; a data dir that never went
/// through an activation has no manifest and is its own slot.
fn try_read_slot_manifest(slot_dir: &Path) -> Result<Option<SlotManifestView>, LaunchError> {
    match std::fs::symlink_metadata(slot_dir.join("slot.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(LaunchError::UnreadableSlot(error.to_string())),
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() =>
            Err(LaunchError::UnreadableSlot("slot manifest is not a literal regular file".into())),
        Ok(_) => {
            let manifest = read_slot_manifest(slot_dir)?;
            if manifest.schema_version != 1 {
                return Err(LaunchError::UnreadableSlot("unsupported slot manifest schema".into()));
            }
            Ok(Some(manifest))
        }
    }
}

/// Binds process launches to the committed selector.
pub struct LaunchService {
    store: Arc<shenko_store::Store>,
    agent_id: EntityId,
}

impl LaunchService {
    pub fn new(store: Arc<shenko_store::Store>, agent_id: EntityId) -> Self {
        Self {
            store,
            agent_id,
        }
    }

    /// Resolve one launch. The same `launch_id` always returns the same
    /// receipt only while its persisted selector binding is still current.
    /// Durable launch rows provide cross-process deduplication.
    pub fn resolve(
        &self,
        request: &LaunchRequest,
    ) -> Result<SubjectLaunchReceipt, LaunchError> {
        let selector = self
            .store
            .activation_selector(self.agent_id)
            .map_err(LaunchError::Store)?
            .ok_or(LaunchError::NoActivationSelector)?;
        if selector.agent_id != request.agent_id {
            return Err(LaunchError::SelectorSubjectMismatch);
        }
        if !request.read_only {
            if let Some(stored) = self.store.activation_launch(request.launch_id)? {
                let receipt: SubjectLaunchReceipt = serde_json::from_value(stored)?;
                if receipt.slot_dir != request.slot_dir || receipt.generation != selector.generation
                    || receipt.selector_revision != selector.selector_revision
                    || receipt.selector_epoch != selector.active_epoch {
                    return Err(LaunchError::UnreadableSlot("durable launch identity differs from current selector/request".into()));
                }
                return Ok(receipt);
            }
        }
        // Bind the launch to the slot only when the slot publishes a
        // manifest (post-activation layout); a pre-slot data dir is bound
        // by the selector alone.
        if let Some(manifest) = try_read_slot_manifest(&request.slot_dir)? {
            if manifest.generation != selector.generation {
                return Err(LaunchError::SlotGenerationMismatch {
                    slot: format!("{:?}: {}", request.slot_dir, manifest.generation.0),
                    selector: selector.generation.0.clone(),
                });
            }
        }
        let receipt = SubjectLaunchReceipt {
            launch_id: request.launch_id,
            slot_dir: request.slot_dir.clone(),
            generation: selector.generation.clone(),
            selector_revision: selector.selector_revision,
            selector_epoch: selector.active_epoch,
            origin: selector.origin.clone(),
            read_only: request.read_only,
        };
        if !request.read_only {
            self.store.record_activation_launch(request.launch_id, &selector,
                &serde_json::to_value(&receipt).map_err(LaunchError::Json)?)
                .map_err(LaunchError::Store)?;
        }
        Ok(receipt)
    }
}
