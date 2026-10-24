//! Durable QQ outbound operation ledger — SPEC §35.6, §41.4 X03
//!
//! The invariant is: **persist the outbound intent before the bytes leave the
//! process**. A send whose response never arrives is not a failure — it is an
//! *unknown* external effect, recorded for reconciliation and never blindly
//! resent.
//!
//! Mapping onto `OperationRecord` (shenko-types §41.4 X03):
//!   intent recorded            → state `prepared`,  effect `none`
//!   echo received (retcode ok) → state `resolved`,  outcome `succeeded`, effect `complete`
//!   echo received (retcode !0) → state `resolved`,  outcome `failed`,    effect `none`
//!   no echo / disconnect       → state `reconciling`, effect `unknown`
use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use shenko_store::store::Store;
use shenko_types::commit::CommitRef;
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};
use shenko_types::operation::{
    AuthorityRef, EffectStatus, OperationContractRef, OperationOutcome, OperationRecord,
    OperationState, ResourceBindings,
};
use shenko_types::store::WriteBatch;

/// Which OneBot action this operation performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundKind {
    SendGroup,
    SendPrivate,
}

impl OutboundKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OutboundKind::SendGroup => "send_group",
            OutboundKind::SendPrivate => "send_private",
        }
    }

    pub fn action(self) -> &'static str {
        match self {
            OutboundKind::SendGroup => "send_group_msg",
            OutboundKind::SendPrivate => "send_private_msg",
        }
    }
}

/// Lifecycle of one outbound operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OutboundState {
    /// Intent recorded; the action was handed to the transport.
    Sent,
    /// NapCat answered with a retcode. `retcode == 0` means accepted.
    Acked { retcode: i64 },
    /// No answer within the response window (or the socket dropped first).
    /// The effect status is genuinely unknown.
    Unknown { since: DateTime<Utc> },
}

/// One outbound QQ message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundOperation {
    pub operation_id: EntityId,
    pub kind: OutboundKind,
    pub target: u64,
    pub message: String,
    /// OneBot `echo` token; correlates the wire response with this operation.
    pub echo_token: String,
    pub state: OutboundState,
    pub created_at: DateTime<Utc>,
}

impl OutboundOperation {
    pub fn new(kind: OutboundKind, target: u64, message: impl Into<String>) -> Self {
        Self {
            operation_id: EntityId::new(),
            kind,
            target,
            message: message.into(),
            echo_token: uuid::Uuid::new_v4().to_string(),
            state: OutboundState::Sent,
            created_at: Utc::now(),
        }
    }

    /// `(task_id, logical_key)` is the uniqueness key of an operation. There is
    /// no user task for a channel reply, so the task component is nil and the
    /// echo token makes the key unique.
    pub fn logical_key(&self) -> String {
        format!("qq.{}:{}:{}", self.kind.as_str(), self.target, self.echo_token)
    }
}

/// Resolved-or-not view used after a restart to answer "what happened?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AckState {
    Acked { retcode: i64 },
    Failed { retcode: i64 },
    Unknown { since: DateTime<Utc> },
}

/// In-memory operation ledger plus an optional durable store.
pub struct OutboundLedger {
    ops: Mutex<HashMap<EntityId, OutboundOperation>>,
    store: Option<Arc<Store>>,
    /// Bot QQ number, recorded as the account resource identity.
    account: u64,
}

impl Default for OutboundLedger {
    fn default() -> Self { Self::new() }
}

impl OutboundLedger {
    /// In-memory only — used by tests and by transports without a store.
    pub fn new() -> Self {
        Self { ops: Mutex::new(HashMap::new()), store: None, account: 0 }
    }

    /// Durable ledger writing `OperationRecord`s into the shared store.
    pub fn with_store(store: Arc<Store>) -> Self {
        Self { ops: Mutex::new(HashMap::new()), store: Some(store), account: 0 }
    }

    pub fn with_account(mut self, account: u64) -> Self {
        self.account = account;
        self
    }

    pub fn get(&self, operation_id: EntityId) -> Option<OutboundOperation> {
        self.ops.lock().get(&operation_id).cloned()
    }

    /// Record the intent **before** sending. Returns `Err` when durability
    /// failed; the caller must then not send the message.
    pub fn record_intent(&self, op: &OutboundOperation) -> anyhow::Result<()> {
        if let Some(store) = &self.store {
            persist(
                store,
                self.account,
                op,
                OperationState::Prepared,
                None,
                EffectStatus::None,
                RecordRevision(0),
            )?;
        }
        self.ops.lock().insert(op.operation_id, op.clone());
        Ok(())
    }

    /// Record the provider's answer. A non-zero retcode is a *resolved*
    /// failure, not an unknown effect.
    pub fn record_ack(&self, operation_id: EntityId, retcode: i64) -> anyhow::Result<()> {
        let (op, revision) = {
            let mut ops = self.ops.lock();
            let Some(op) = ops.get_mut(&operation_id) else {
                return Ok(());
            };
            op.state = OutboundState::Acked { retcode };
            (op.clone(), self.next_revision(operation_id))
        };

        if let Some(store) = &self.store {
            let (outcome, effect) = if retcode == 0 {
                (Some(OperationOutcome::Succeeded), EffectStatus::Complete)
            } else {
                (Some(OperationOutcome::Failed), EffectStatus::None)
            };
            persist(
                store,
                self.account,
                &op,
                OperationState::Resolved,
                outcome,
                effect,
                revision,
            )?;
        }
        Ok(())
    }

    /// The response window elapsed (or the socket dropped): the effect is
    /// unknown. SPEC §35.6 — reconcile, never blind-resend.
    pub fn record_unknown(&self, operation_id: EntityId) -> anyhow::Result<()> {
        let (op, revision) = {
            let mut ops = self.ops.lock();
            let Some(op) = ops.get_mut(&operation_id) else {
                return Ok(());
            };
            op.state = OutboundState::Unknown { since: Utc::now() };
            (op.clone(), self.next_revision(operation_id))
        };

        if let Some(store) = &self.store {
            persist(
                store,
                self.account,
                &op,
                OperationState::Reconciling,
                None,
                EffectStatus::Unknown,
                revision,
            )?;
        }
        Ok(())
    }

    /// Resolve the durable state of an operation, e.g. after a restart.
    /// Queries the store first, then the in-memory ledger.
    pub fn reconcile(&self, operation_id: EntityId) -> anyhow::Result<Option<AckState>> {
        if let Some(store) = &self.store {
            let record = store
                .get_operation(operation_id)
                .map_err(|e| anyhow::anyhow!("read operation {operation_id}: {e}"))?;
            if let Some(record) = record {
                return Ok(Some(ack_from_record(&record)));
            }
        }
        Ok(self
            .ops
            .lock()
            .get(&operation_id)
            .map(|op| ack_from_state(&op.state, op.created_at)))
    }

    fn next_revision(&self, operation_id: EntityId) -> RecordRevision {
        if let Some(store) = &self.store {
            match store.get_operation(operation_id) {
                Ok(Some(record)) => return record.record_revision.next(),
                Ok(None) => {}
                Err(e) => warn!(%operation_id, err=%e, "read operation revision failed"),
            }
        }
        RecordRevision(1)
    }
}

fn ack_from_state(state: &OutboundState, created_at: DateTime<Utc>) -> AckState {
    match state {
        OutboundState::Acked { retcode: 0 } => AckState::Acked { retcode: 0 },
        OutboundState::Acked { retcode } => AckState::Failed { retcode: *retcode },
        OutboundState::Unknown { since } => AckState::Unknown { since: *since },
        OutboundState::Sent => AckState::Unknown { since: created_at },
    }
}

fn ack_from_record(record: &OperationRecord) -> AckState {
    if record.effect_status == EffectStatus::Complete
        || record.outcome == Some(OperationOutcome::Succeeded)
    {
        return AckState::Acked { retcode: 0 };
    }
    if record.outcome == Some(OperationOutcome::Failed)
        || record.outcome == Some(OperationOutcome::Cancelled)
    {
        // The exact provider retcode is not part of OperationRecord; the
        // committed outcome is what matters after a restart.
        return AckState::Failed { retcode: -1 };
    }
    AckState::Unknown { since: Utc::now() }
}

fn seal_json(store: &Store, namespace: &str, value: &serde_json::Value) -> anyhow::Result<ObjectRef> {
    let bytes = serde_json::to_vec(value)?;
    let sealed = store
        .cas
        .seal_object(&bytes, "application/json", namespace, "json", 1)
        .map_err(|e| anyhow::anyhow!("CAS seal {namespace}: {e}"))?;
    Ok(sealed.object)
}

fn persist(
    store: &Store,
    account: u64,
    op: &OutboundOperation,
    state: OperationState,
    outcome: Option<OperationOutcome>,
    effect: EffectStatus,
    revision: RecordRevision,
) -> anyhow::Result<()> {
    let payload = serde_json::json!({
        "kind": op.kind.as_str(),
        "target": op.target.to_string(),
        "message": op.message,
        "echo_token": op.echo_token,
        "created_at": op.created_at.to_rfc3339(),
    });
    let input = seal_json(store, "qq_outbound_input", &payload)?;

    let bindings = serde_json::to_vec(&ResourceBindings {
        provider: "napcat".to_string(),
        account_identity: format!("qq:{account}"),
        runtime_node: "local".to_string(),
        mount_generation: 0,
        capability_level: "send_message".to_string(),
        extra: serde_json::json!({ "kind": op.kind.as_str(), "target": op.target.to_string() }),
    })?;
    let resource_bindings_ref = store
        .cas
        .seal_object(&bindings, "application/json", "qq_outbound_bindings", "json", 1)
        .map_err(|e| anyhow::anyhow!("CAS seal resource bindings: {e}"))?
        .object;

    let acceptance_predicate_ref = seal_json(
        store,
        "qq_outbound_predicates",
        &serde_json::json!({ "predicate": "onebot_retcode_zero", "provider": "napcat" }),
    )?;
    let retry_contract_ref = seal_json(
        store,
        "qq_outbound_predicates",
        &serde_json::json!({
            "max_attempts": 1,
            "on_timeout": "reconcile",
            "blind_resend": false,
        }),
    )?;

    let record = OperationRecord {
        record_schema_version: 1,
        operation_id: op.operation_id,
        record_revision: revision,
        // Channel replies are not tied to a user task.
        task_id: EntityId::nil(),
        logical_key: op.logical_key(),
        operation_name: "qq.send".to_string(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: input.clone(),
        input_digest: input.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: EntityId::nil(),
            artifact_digest: Digest::blake3(b"shenko-channel-napcat"),
            contract_digest: Digest::blake3(b"onebot-v11/send"),
            receipt_reader_ref: VersionedRef {
                artifact: ArtifactRef {
                    digest: Digest::blake3(b"shenko-channel-napcat"),
                    locator: "crates/shenko-channel-napcat".to_string(),
                },
                symbol: "SendReceipt".to_string(),
                schema: SchemaRef {
                    name: "onebot_v11_response".to_string(),
                    major: 1,
                    digest: Digest::blake3(b"onebot_v11_response"),
                },
            },
        },
        resource_bindings_ref,
        authority_ref: AuthorityRef {
            // A channel reply is an ordinary subject action; no delegated
            // authority record exists yet, so the nil delegation is explicit.
            delegation_id: EntityId::nil(),
            delegation_revision: RecordRevision(0),
            decision_ref: EntityId::nil(),
        },
        acceptance_predicate_ref,
        retry_contract_ref,
        checkpoint_scope_ref: None,
        state,
        outcome,
        outcome_conflict: false,
        effect_status: effect,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        // The commit that durably carries this record is the one produced by
        // committing this batch; `seq 0` denotes "prepared before the write".
        prepared_commit: CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO),
        evidence_through: None,
    };

    let mut batch = WriteBatch::new();
    batch.operation_upserts.push(record);
    store
        .commit(batch)
        .map_err(|e| anyhow::anyhow!("commit outbound operation: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_then_ack_resolves_operation() {
        let ledger = OutboundLedger::new();
        let op = OutboundOperation::new(OutboundKind::SendGroup, 123456, "hello");
        let id = op.operation_id;

        ledger.record_intent(&op).expect("record intent");
        assert_eq!(ledger.get(id).expect("op present").state, OutboundState::Sent);
        assert_eq!(
            ledger.reconcile(id).expect("reconcile"),
            Some(AckState::Unknown { since: op.created_at }),
            "before the ack the external effect is unknown"
        );

        ledger.record_ack(id, 0).expect("record ack");
        assert_eq!(
            ledger.get(id).expect("op present").state,
            OutboundState::Acked { retcode: 0 }
        );
        assert_eq!(
            ledger.reconcile(id).expect("reconcile"),
            Some(AckState::Acked { retcode: 0 })
        );
    }

    #[test]
    fn unknown_outcome_reconciles_without_blind_resend() {
        let ledger = OutboundLedger::new();
        let op = OutboundOperation::new(OutboundKind::SendPrivate, 998877, "hi");
        let id = op.operation_id;

        ledger.record_intent(&op).expect("record intent");
        ledger.record_unknown(id).expect("record unknown");

        let state = ledger.get(id).expect("op present").state;
        assert!(matches!(state, OutboundState::Unknown { .. }));

        // Reconciliation reports the unknown effect and leaves the operation
        // untouched: the caller must decide, not resend automatically.
        assert!(matches!(
            ledger.reconcile(id).expect("reconcile"),
            Some(AckState::Unknown { .. })
        ));
        assert!(matches!(
            ledger.get(id).expect("op present").state,
            OutboundState::Unknown { .. }
        ));
    }
}
