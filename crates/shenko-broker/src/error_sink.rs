//! The runtime error sink of the broker — docs/specs/durable-state.md#spec-41-14, docs/specs/cognition-units.md#spec-45-14.
//!
//! A broker failure that cannot continue, or that forces a degradation, is a
//! fact about the owning subject. It belongs in the same durable runtime
//! error journal the rest of the product writes to, with the agent, task and
//! request identity of the *actual* request — never a global default subject.
//!
//! The sink is fail-soft: a journal that cannot be written is a warning, not a
//! second failure that hides the first. It is not a settlement path: the
//! provider terminal receipt remains the authority on what the provider did.

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use std::sync::{Arc, Mutex};

use crate::request::{InferenceRequest, RequestPurpose};

/// One runtime error that the broker could not continue from, or that forces
/// a degradation of the request it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeErrorReport {
    /// The request the error belongs to.
    pub request_id: EntityId,
    /// The agent that owns the request.
    pub agent_id: EntityId,
    /// The conversation the request belongs to.
    pub conversation_id: EntityId,
    /// Why the call was made.
    pub purpose: RequestPurpose,
    /// A stable, machine-readable code.
    pub code: String,
    /// The message, already redacted by the caller's own rules.
    pub message: String,
    /// When the error was observed, in milliseconds since the epoch.
    pub occurred_at_ms: i64,
}

/// The durable runtime error journal of the owning subject.
///
/// The implementation writes to `Store::journal_runtime_error`; the broker
/// only knows this contract.
pub trait RuntimeErrorSink: Send + Sync + 'static {
    /// Journal one runtime error against the owning agent of the request.
    fn journal_runtime_error(&self, report: &RuntimeErrorReport) -> anyhow::Result<()>;
}

/// The shared cell that holds the installed runtime error sink.
pub type ErrorSinkCell = Arc<Mutex<Option<Arc<dyn RuntimeErrorSink>>>>;

/// A fresh, empty sink cell.
pub fn new_error_sink_cell() -> ErrorSinkCell {
    Arc::new(Mutex::new(None))
}

/// Build the report of one request. The owner is the request's own agent and
/// conversation, never a global default subject.
pub fn report_for(
    request: &InferenceRequest,
    purpose: RequestPurpose,
    code: &str,
    message: &str,
) -> RuntimeErrorReport {
    RuntimeErrorReport {
        request_id: request.request_id,
        agent_id: request.agent_id,
        conversation_id: request.conversation_id,
        purpose,
        code: code.to_string(),
        message: message.to_string(),
        occurred_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

/// Journal one runtime error. The write is fail-soft: a journal that cannot
/// be written is a warning and never replaces the error it reports.
pub fn journal_runtime_error(cell: &ErrorSinkCell, report: &RuntimeErrorReport) {
    let sink = match cell.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    let Some(sink) = sink else { return };
    if let Err(error) = sink.journal_runtime_error(report) {
        tracing::warn!(
            %error,
            request_id=%report.request_id,
            "runtime error could not be journalled"
        );
    }
}

/// A sink that records nothing. It exists so a broker without an owner does
/// not pretend to have journalled anything.
pub struct NoRuntimeErrorSink;

impl RuntimeErrorSink for NoRuntimeErrorSink {
    fn journal_runtime_error(&self, _report: &RuntimeErrorReport) -> anyhow::Result<()> {
        Ok(())
    }
}
