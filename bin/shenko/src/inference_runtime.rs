//! The inference runtime composition — docs/specs/inference.md#spec-13, docs/specs/inference.md#spec-41-9, docs/specs/cognition-units.md#spec-45-14.
//!
//! The composition root owns the three things the broker cannot know by
//! itself: the physical resource domain a provider binding really belongs to,
//! the durable runtime error journal of the owning subject, and the
//! publication of the installed provider runtimes as a Cordis service.

use std::sync::Arc;

use shenko_broker::broker::BrokerHandle;
use shenko_broker::error_sink::{RuntimeErrorReport, RuntimeErrorSink};
use shenko_store::Store;

/// The runtime error journal of the owning subject.
///
/// The broker reports the errors it cannot continue from, or that force a
/// degradation, against the agent of the *actual* request. The write is
/// fail-soft: the store counts a dropped entry instead of propagating a
/// second failure into the error path.
pub struct StoreRuntimeErrorSink {
    store: Arc<Store>,
}

impl StoreRuntimeErrorSink {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

impl RuntimeErrorSink for StoreRuntimeErrorSink {
    fn journal_runtime_error(&self, report: &RuntimeErrorReport) -> anyhow::Result<()> {
        let context = shenko_store::RuntimeErrorContext {
            kind: "inference_broker".into(),
            source_module: "shenko-broker".into(),
            message: format!("{}: {}", report.code, report.message),
            step_no: None,
            task_id: None,
            extra: serde_json::json!({
                "request_id": report.request_id.to_string(),
                "conversation_id": report.conversation_id.to_string(),
                "purpose": report.purpose.as_str(),
            }),
        };
        self.store.journal_runtime_error(report.agent_id, context);
        Ok(())
    }
}

/// Install the runtime error journal of the owning subject on the broker.
pub fn install_runtime_error_sink(handle: &BrokerHandle, store: Arc<Store>) {
    handle.set_runtime_error_sink(Arc::new(StoreRuntimeErrorSink::new(store)));
}
