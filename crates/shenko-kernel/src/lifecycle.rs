//! Plugin lifecycle management — SPEC §7
//! generation boundary, epoch tracking, cancellation.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;
use shenko_types::ids::{ControllerEpoch, RuntimeGeneration, EntityId};

/// Current runtime generation and epoch.
#[derive(Debug, Clone)]
pub struct LifecycleState {
    pub generation: RuntimeGeneration,
    pub incarnation: EntityId,
    pub epoch: Arc<AtomicU64>,
    pub cancel: CancellationToken,
}

impl LifecycleState {
    pub fn new(generation: RuntimeGeneration) -> Self {
        Self {
            generation,
            incarnation: EntityId::new(),
            epoch: Arc::new(AtomicU64::new(1)),
            cancel: CancellationToken::new(),
        }
    }

    pub fn current_epoch(&self) -> ControllerEpoch {
        ControllerEpoch(self.epoch.load(Ordering::SeqCst))
    }

    pub fn advance_epoch(&self) -> ControllerEpoch {
        let v = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        ControllerEpoch(v)
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn is_shutdown(&self) -> bool {
        self.cancel.is_cancelled()
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}
