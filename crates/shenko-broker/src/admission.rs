use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const NOMINAL_FLOWS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityPool {
    Compute,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityProvenance {
    Configured,
    Estimated,
    Unknown,
}

/// A software compute budget, not evidence of physical KV residency or four
/// measured backend streams. Unknown physical discovery remains distinguishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacitySnapshot {
    pub pool: CapacityPool,
    pub value: Option<u64>,
    pub provenance: CapacityProvenance,
}

#[derive(Clone)]
pub struct AdmissionController {
    active: Arc<AtomicU64>,
    max: u64,
    provenance: CapacityProvenance,
}

impl AdmissionController {
    pub fn new(max: u64) -> Self {
        Self::with_provenance(max, CapacityProvenance::Configured)
    }

    pub fn nominal() -> Self {
        Self::with_provenance(NOMINAL_FLOWS, CapacityProvenance::Estimated)
    }

    fn with_provenance(max: u64, provenance: CapacityProvenance) -> Self {
        Self {
            active: Arc::new(AtomicU64::new(0)),
            max,
            provenance,
        }
    }

    pub fn try_admit(&self) -> Option<AdmissionGuard> {
        self.active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < self.max).then_some(current + 1)
            })
            .ok()
            .map(|_| AdmissionGuard {
                counter: Arc::clone(&self.active),
                released: false,
            })
    }

    /// An armed request recovered without terminal evidence is already debt,
    /// not a new admission. It may exceed the configured cap and blocks new
    /// work until the original request is reconciled.
    pub(crate) fn recover_unknown(&self) -> AdmissionGuard {
        self.active.fetch_add(1, Ordering::SeqCst);
        AdmissionGuard {
            counter: Arc::clone(&self.active),
            released: false,
        }
    }

    pub fn active_count(&self) -> u64 {
        self.active.load(Ordering::SeqCst)
    }

    pub fn capacity(&self) -> CapacitySnapshot {
        CapacitySnapshot {
            pool: CapacityPool::Compute,
            value: Some(self.max),
            provenance: self.provenance,
        }
    }
}

pub struct AdmissionGuard {
    counter: Arc<AtomicU64>,
    released: bool,
}

impl AdmissionGuard {
    /// Release only after matching provider/backend terminal evidence.
    pub fn release_terminal(self) {
        self.release_with_evidence();
    }

    /// Release a reservation that was positively never dispatched. This is a
    /// separate proof from a remote request becoming terminal.
    pub fn release_never_dispatched(self) {
        self.release_with_evidence();
    }

    fn release_with_evidence(mut self) {
        self.released = true;
    }
}

impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if self.released {
            self.counter.fetch_sub(1, Ordering::SeqCst);
        }
        // Dropping a guard without evidence deliberately retains occupancy:
        // local cancellation is not proof that the remote request stopped.
    }
}
