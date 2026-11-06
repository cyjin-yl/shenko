//! Live withdrawal of already-open guest write capabilities (SPEC §8.3, §41.3).
//!
//! Withdrawing `writable` is not the same as reopening a mount read-only. A COW
//! mount keeps its mode, its branch overlay and its merged view: the guest
//! continues to read overlay-over-base, and only the ability to mutate is
//! withdrawn. Because a guest may already hold writable file and directory
//! descriptors when the withdrawal is requested, the withdrawal must reach
//! *those* descriptors instead of waiting for the next run.
//!
//! The mechanism is deliberately small and honest:
//!
//! * [`WasiRevocation`] is a cheap, shareable *request* handle. It is carried
//!   in [`crate::runtime::WasiRunControl`], so another thread of the same
//!   process (the subject loop, a supervisor, a test) can request withdrawal
//!   while the guest is executing.
//! * The preview1 host consults that shared request at every mutation
//!   boundary. It is the existing descriptor table ([`crate::p1::P1Host`]'s
//!   `fds` plus the admitted [`crate::mount::MountBinding`] list) that is
//!   gated — no second table of open files is introduced, and no descriptor
//!   number changes.
//! * A hostcall that is already inside a write completes on the guest thread.
//!   Its bytes are not rolled back and are not hidden: the receipt counts how
//!   many writes were in flight when the request was made, how many completed
//!   before and after it, and how many were refused. "Withdrawn" therefore
//!   never claims "no further byte was written by a call that was already
//!   running"; it claims exactly what is observable.
//!
//! This is a capability withdrawal, not a rollback: nothing that already left
//! the sandbox is undone, and the branch overlay still contains everything the
//! guest wrote before the withdrawal.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;

/// Which mounts a withdrawal applies to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MountSelection {
    all: bool,
    mounts: BTreeSet<usize>,
}

/// Shared withdrawal state. One instance belongs to one run.
#[derive(Debug)]
pub struct RevocationState {
    selection: Mutex<MountSelection>,
    /// Fast path: any withdrawal at all has been requested.
    any: AtomicBool,
    requested_at_unix_ms: AtomicU64,
    /// First mutation boundary that observed the request (`0` = none yet).
    first_observed_at_unix_ms: AtomicU64,
    writes_in_flight: AtomicU64,
    writes_in_flight_at_request: AtomicU64,
    writes_completed: AtomicU64,
    writes_completed_at_request: AtomicU64,
    writes_refused: AtomicU64,
}

impl RevocationState {
    fn new() -> Self {
        Self {
            selection: Mutex::new(MountSelection::default()),
            any: AtomicBool::new(false),
            requested_at_unix_ms: AtomicU64::new(0),
            first_observed_at_unix_ms: AtomicU64::new(0),
            writes_in_flight: AtomicU64::new(0),
            writes_in_flight_at_request: AtomicU64::new(0),
            writes_completed: AtomicU64::new(0),
            writes_completed_at_request: AtomicU64::new(0),
            writes_refused: AtomicU64::new(0),
        }
    }

    /// Whether writes through `mount` are withdrawn.
    pub fn mount_revoked(&self, mount: usize) -> bool {
        if !self.any.load(Ordering::Acquire) {
            return false;
        }
        let selection = self.selection.lock();
        selection.all || selection.mounts.contains(&mount)
    }

    /// Whether any withdrawal has been requested.
    pub fn is_revoked(&self) -> bool {
        self.any.load(Ordering::Acquire)
    }

    fn record_request(&self, mount: Option<usize>) {
        let mut selection = self.selection.lock();
        match mount {
            Some(index) => {
                selection.mounts.insert(index);
            }
            None => selection.all = true,
        }
        // Sample the counters exactly once, at the first request, so the
        // receipt describes the boundary rather than the last repeat request.
        if !self.any.swap(true, Ordering::AcqRel) {
            self.requested_at_unix_ms
                .store(now_unix_ms(), Ordering::Release);
            self.writes_in_flight_at_request.store(
                self.writes_in_flight.load(Ordering::Acquire),
                Ordering::Release,
            );
            self.writes_completed_at_request.store(
                self.writes_completed.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
    }

    fn observe(&self) {
        if self.first_observed_at_unix_ms.load(Ordering::Acquire) == 0 {
            let _ = self.first_observed_at_unix_ms.compare_exchange(
                0,
                now_unix_ms(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
    }

    /// The observable outcome of this withdrawal so far.
    pub fn receipt(&self) -> RevocationReceipt {
        let selection = self.selection.lock().clone();
        let observed = self.first_observed_at_unix_ms.load(Ordering::Acquire);
        RevocationReceipt {
            requested: self.is_revoked(),
            requested_at_unix_ms: self.requested_at_unix_ms.load(Ordering::Acquire),
            first_observed_at_unix_ms: (observed != 0).then_some(observed),
            all_mounts: selection.all,
            mounts: selection.mounts.into_iter().collect(),
            writes_in_flight_at_request: self.writes_in_flight_at_request.load(Ordering::Acquire),
            writes_completed_before_request: self
                .writes_completed_at_request
                .load(Ordering::Acquire),
            writes_completed_total: self.writes_completed.load(Ordering::Acquire),
            writes_refused: self.writes_refused.load(Ordering::Acquire),
        }
    }
}

/// What a withdrawal did and did not do.
///
/// `writes_completed_total > writes_completed_before_request` means a write
/// that was already inside a hostcall completed after the request. Those bytes
/// remain in the overlay: this receipt is not a rollback record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevocationReceipt {
    pub requested: bool,
    /// When the request was first made (`0` before any request).
    pub requested_at_unix_ms: u64,
    /// First mutation boundary that observed the request, when it was observed.
    pub first_observed_at_unix_ms: Option<u64>,
    pub all_mounts: bool,
    /// Mount indices whose writes were withdrawn, ascending.
    pub mounts: Vec<usize>,
    pub writes_in_flight_at_request: u64,
    pub writes_completed_before_request: u64,
    pub writes_completed_total: u64,
    pub writes_refused: u64,
}

impl RevocationReceipt {
    /// Writes that were already running when the request was made and finished
    /// afterwards. Their bytes stay in place.
    pub fn completed_after_request(&self) -> u64 {
        self.writes_completed_total
            .saturating_sub(self.writes_completed_before_request)
    }
}

/// A shareable request handle: clone it into the thread that decides to
/// withdraw writes while the run that owns the guest keeps executing.
#[derive(Debug, Clone)]
pub struct WasiRevocation {
    state: Arc<RevocationState>,
}

impl WasiRevocation {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RevocationState::new()),
        }
    }

    /// Withdraw write capability for every mounted filesystem. Reads keep the
    /// same merged view.
    pub fn revoke_all(&self) {
        self.state.record_request(None);
    }

    /// Withdraw write capability for one mount index (the preopen order used
    /// by the run). Reads keep the same merged view.
    pub fn revoke_mount(&self, mount: usize) {
        self.state.record_request(Some(mount));
    }

    pub fn is_revoked(&self) -> bool {
        self.state.is_revoked()
    }

    /// Observable outcome of the withdrawal so far.
    pub fn receipt(&self) -> RevocationReceipt {
        self.state.receipt()
    }

    /// The run's host state shares this exact state, never a copy.
    pub(crate) fn state(&self) -> Arc<RevocationState> {
        Arc::clone(&self.state)
    }
}

impl Default for WasiRevocation {
    fn default() -> Self {
        Self::new()
    }
}

/// One write hostcall in progress on the guest thread.
///
/// Declared before any byte is handed to the filesystem and finished with
/// [`WriteInFlight::commit`] once the write returned. The counters make an
/// in-flight write at withdrawal time visible instead of being silently
/// assumed absent, and they never claim the write was undone.
pub(crate) struct WriteInFlight {
    state: Arc<RevocationState>,
}

impl WriteInFlight {
    /// Enter a write hostcall for `mount`, or count a refusal and return
    /// `None` when write capability was already withdrawn for it.
    pub(crate) fn begin(state: &Arc<RevocationState>, mount: usize) -> Option<Self> {
        if state.mount_revoked(mount) {
            state.observe();
            state.writes_refused.fetch_add(1, Ordering::AcqRel);
            return None;
        }
        state.writes_in_flight.fetch_add(1, Ordering::AcqRel);
        Some(Self {
            state: Arc::clone(state),
        })
    }

    /// The write returned from the filesystem; its bytes exist.
    pub(crate) fn commit(self) {
        self.state.writes_completed.fetch_add(1, Ordering::AcqRel);
    }
}

impl Drop for WriteInFlight {
    fn drop(&mut self) {
        // A panicking or erroring write still stops being "in flight"; it is
        // simply not counted as completed, so the receipt never overstates
        // what reached the filesystem.
        self.state.writes_in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Withdrawal-aware check for a non-descriptor mutation (create, unlink,
/// rename, symlink, truncate) on `mount`. Returns the number of refusals as a
/// side effect; `false` means the operation must be refused with `EPERM`.
pub(crate) fn permits_write(state: &Arc<RevocationState>, mount: usize) -> bool {
    if state.mount_revoked(mount) {
        state.observe();
        state.writes_refused.fetch_add(1, Ordering::AcqRel);
        false
    } else {
        true
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
