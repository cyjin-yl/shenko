//! Scoped error observation for plugin-runtime failures (SPEC §45.14).
//!
//! The kernel cannot journal runtime failures itself: it has no subject
//! store, and before genesis no branch exists. The assembly root attaches
//! exactly one sink after the subject store exists; until then a kernel
//! failure stays stderr plus a counter, which is the truthful shape for a
//! runtime that must not manufacture a branch. This is a direct observation
//! call at each unable-to-continue/downgrade boundary — never a blanket
//! tracing subscriber, so a journal failure cannot recurse back into the
//! observation path.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;
use shenko_types::ids::EntityId;

/// One observed plugin-runtime failure site.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ErrorSite {
    /// Stable machine-readable site, e.g. `cordis_lifecycle`,
    /// `hook_graph_admission`, `hook_graph_revision_conflict`.
    pub kind: &'static str,
    /// Rust module path of the reporting site.
    pub module: &'static str,
    /// The subject step this failure belongs to, when there is one.
    pub step_no: Option<u64>,
    /// The finite task this failure belongs to, when there is one.
    pub task_id: Option<EntityId>,
}

impl ErrorSite {
    pub fn new(kind: &'static str, module: &'static str) -> Self {
        Self {
            kind,
            module,
            step_no: None,
            task_id: None,
        }
    }
}

/// The scoped sink the assembly root attaches after the subject store
/// exists. The implementation journals into the SAME subject store journal
/// as every other runtime-error fact (§45.14) and must never re-enter this
/// observation path on journal failure — `journal_runtime_error` already
/// counts instead of propagating.
pub trait ErrorSink: Send + Sync + 'static {
    fn observe(&self, site: ErrorSite, message: String, extra: Value);
}

/// The kernel's single error-observation point, shared by the plugin
/// registry and the hook registry so one incident is one journal entry.
///
/// Failures observed before a sink exists are counted and traced, never
/// journaled: at startup there is no branch, and manufacturing one would
/// lie. Readers surface `pre_sink_failures` beside the journaled entries so
/// the stderr/counter distinction stays visible instead of being silently
/// absorbed.
#[derive(Default)]
pub struct ErrorObservation {
    sink: RwLock<Option<Arc<dyn ErrorSink>>>,
    pre_sink: AtomicU64,
}

impl ErrorObservation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the store-backed sink once the subject store exists. Attaching
    /// twice replaces the sink; the assembly attaches exactly once.
    pub fn attach(&self, sink: Arc<dyn ErrorSink>) {
        *self.sink.write() = Some(sink);
    }

    pub fn has_sink(&self) -> bool {
        self.sink.read().is_some()
    }

    /// Failures counted before the sink existed; truthful startup record.
    pub fn pre_sink_failures(&self) -> u64 {
        self.pre_sink.load(Ordering::Relaxed)
    }

    /// Observe one kernel-boundary failure. With a sink this is one journal
    /// write; without one it is stderr plus a counter. Never propagates.
    pub fn observe(&self, site: ErrorSite, message: impl Into<String>, extra: Value) {
        let message = message.into();
        let sink = self.sink.read().clone();
        match sink {
            Some(sink) => sink.observe(site, message, extra),
            None => {
                let count = self.pre_sink.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::error!(
                    kind = site.kind,
                    module = site.module,
                    pre_sink_failures = count,
                    %message,
                    "plugin runtime failure before error sink attach; counted, not journaled"
                );
            }
        }
    }
}
