//! Authored input controls: `[dispose]`, `[obligation]`, `[dialogue]`,
//! `[dnd]`, `[steer]`, `[reconcile]`. These are the subject's own durable
//! decisions about its inputs, attention and unfinished external effects —
//! dispositions with reasons/priority/review times, durable obligations,
//! dialogue turns to children, bounded do-not-disturb switches, adopted
//! steers and explicit acceptance of an unknown effect. Parsing rejects
//! malformed or unbounded controls at the model boundary; the store
//! re-validates the sealed decision at S1.

use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::ids::EntityId;
use shenko_types::output::{
    AttentionSpec, DialogueSpec, DispositionSpec, ObligationSpec, ReconcileSpec, SteerSpec,
};
use shenko_types::subject::{AttentionState, Disposition, ObligationTerminalChange};
fn invalid(line: u64, reason: &str) -> ShenkoError {
    ProtocolError::new(
        "CONTROL_PROTOCOL_INVALID",
        ErrorScope::Request,
        RetryStrategy::NewRevision,
    )
    .with_details(serde_json::json!({"line":line,"reason":reason,"protocol":3}))
    .into()
}

/// `[dispose] {"event_id":..., ...}` — the subject's authored disposition of
/// one input event selected into this step's S0.
pub(super) fn parse_disposition(value: &str, line_no: u64) -> Result<DispositionSpec, ShenkoError> {
    let spec: DispositionSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "dispose requires one JSON object"))?;
    if spec.event_id == EntityId::nil() {
        return Err(invalid(line_no, "dispose requires the durable event id"));
    }
    if spec.reason.as_deref().unwrap_or("").trim().is_empty()
        && spec.disposition != Disposition::Acknowledged
    {
        return Err(invalid(
            line_no,
            "dispose requires a reason unless the event is acknowledged",
        ));
    }
    if spec.priority.is_some() && spec.review_after.is_none() {
        return Err(invalid(
            line_no,
            "priority disposition requires a review time",
        ));
    }
    Ok(spec)
}

/// `[obligation] {...}` — a durable obligation read back into later S0
/// projections until completed or cancelled.
pub(super) fn parse_obligation(value: &str, line_no: u64) -> Result<ObligationSpec, ShenkoError> {
    let spec: ObligationSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "obligation requires one JSON object"))?;
    if spec.description.trim().is_empty() {
        return Err(invalid(line_no, "obligation description must not be blank"));
    }
    if spec.description.len() > 64 * 1024 {
        return Err(invalid(line_no, "obligation description too large"));
    }
    Ok(spec)
}

/// Explicit terminal change of one obligation shown in this step’s S0.
pub(super) fn parse_obligation_terminal(
    value: &str,
    line_no: u64,
) -> Result<ObligationTerminalChange, ShenkoError> {
    let change: ObligationTerminalChange = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "obligation:terminal requires target, terminal state and reason JSON"))?;
    if change.obligation_id == EntityId::nil() || change.reason.trim().is_empty() {
        return Err(invalid(line_no, "obligation:terminal requires a durable target and nonblank reason"));
    }
    Ok(change)
}

/// `[dialogue] {...}` — an authored turn to a durable task.
pub(super) fn parse_dialogue(value: &str, line_no: u64) -> Result<DialogueSpec, ShenkoError> {
    let spec: DialogueSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "dialogue requires one JSON object"))?;
    if spec.target_task_id == EntityId::nil() {
        return Err(invalid(line_no, "dialogue requires the target task id"));
    }
    if spec.message.trim().is_empty() {
        return Err(invalid(line_no, "dialogue message must not be blank"));
    }
    if spec.message.len() > 64 * 1024 {
        return Err(invalid(line_no, "dialogue message too large"));
    }
    // `to_subject` is how a child answers its parent. The subject loop only
    // addresses a child, so an authored `to_subject` turn is a protocol
    // error rather than a message delivered in the wrong direction.
    if spec.direction != shenko_types::subject::DialogueDirection::ToChild {
        return Err(invalid(
            line_no,
            "dialogue from the subject addresses a child; to_subject is a child's own direction",
        ));
    }
    Ok(spec)
}

/// `[dnd] {...}` — the subject's own attention switch. Off is bounded:
/// it names the silenced sources and a review time, never an unbounded
/// silent switch (SPEC §13.8).
pub(super) fn parse_dnd(value: &str, line_no: u64) -> Result<AttentionSpec, ShenkoError> {
    let spec: AttentionSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "dnd requires one JSON object"))?;
    if spec.reason.trim().is_empty() {
        return Err(invalid(line_no, "attention decision requires a reason"));
    }
    if spec.state == AttentionState::Off
        && (spec.sources.is_empty() || spec.review_after.is_none())
    {
        return Err(invalid(
            line_no,
            "attention off requires the silenced sources and a review time",
        ));
    }
    Ok(spec)
}

/// `[steer] {"submission_id":"..."}` — the subject's adoption of one live
/// steer carried into this step. The submission key must be a real one; the
/// loop binds it to the selected event and the store re-checks that binding
/// at S1, so a fabricated key cannot enter the decision.
pub(super) fn parse_steer(value: &str, line_no: u64) -> Result<SteerSpec, ShenkoError> {
    let spec: SteerSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "steer requires one JSON object"))?;
    if spec.submission_id.trim().is_empty() {
        return Err(invalid(line_no, "steer requires the submission key"));
    }
    Ok(spec)
}

/// `[reconcile] {...}` — the subject accepts one unknown external effect for
/// now. It names a case that was carried into this step's S0 and the receipts
/// its reading rests on; it never claims the effect did not happen.
pub(super) fn parse_reconcile(value: &str, line_no: u64) -> Result<ReconcileSpec, ShenkoError> {
    let spec: ReconcileSpec = serde_json::from_str(value)
        .map_err(|_| invalid(line_no, "reconcile requires one JSON object"))?;
    if spec.case_id == EntityId::nil() {
        return Err(invalid(line_no, "reconcile requires the case id"));
    }
    if spec.reason_refs.is_empty() {
        return Err(invalid(
            line_no,
            "reconcile requires the evidence its reading rests on",
        ));
    }
    if spec.reason_refs.iter().any(|id| *id == EntityId::nil()) {
        return Err(invalid(line_no, "reconcile reason refs must be identities"));
    }
    if spec.remaining_impact.trim().is_empty() {
        return Err(invalid(
            line_no,
            "reconcile requires the remaining impact",
        ));
    }
    if spec.remaining_impact.len() > 64 * 1024 {
        return Err(invalid(line_no, "reconcile remaining impact too large"));
    }
    Ok(spec)
}

/// The public prefix of one result: the natural-language text the subject
/// actually thought, before any control syntax is removed. Output streaming
/// (ArchWeb/ArchBroker parts) publishes this verbatim; it never re-derives
/// text from controls.
pub fn public_text_prefix(raw: &str) -> String {
    // Placeholder until the stream wiring lands: the full decoded public text
    // of the parsed result. The function stays pure and total so parts can
    // prefix-incrementally once OutputPart chunking is wired.
    raw.to_owned()
}
