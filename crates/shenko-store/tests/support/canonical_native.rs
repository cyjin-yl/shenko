//! Shared Store-native canonical fixtures. The retained-value authority is a
//! test fixture, not real consensus; production SSH delivery is tested separately.
use anyhow::Result;
use shenko_store::Store;
use std::sync::Arc;

pub fn draft_dispatch_operation(
    store: &Store,
    invocation: &shenko_types::task::TaskInvocation,
    key: &str,
    operation_id: shenko_types::ids::EntityId,
) -> Result<(
    shenko_types::task::TaskInvocation,
    shenko_types::operation::OperationRecord,
    shenko_types::operation::AttemptRecord,
)> {
    use shenko_types::{ids::*, object::*, operation::*};
    let task = store.external_task(invocation.task_id)?;
    let mut current = invocation.clone();
    current.record_revision = task.record_revision;
    let object = task.input_ref;
    let reader = VersionRef {
        artifact: ArtifactRef {
            digest: object.digest.clone(),
            locator: object.locator.clone(),
        },
        symbol: "dispatch-probe".into(),
        schema: object.codec.clone(),
    };
    let operation = OperationRecord {
        record_schema_version: 1,
        operation_id,
        record_revision: RecordRevision(0),
        task_id: task.task_id,
        logical_key: key.into(),
        operation_name: "dispatch.probe".into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: object.clone(),
        input_digest: object.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: task.implementation_ref.plugin_id,
            artifact_digest: object.digest.clone(),
            contract_digest: object.digest.clone(),
            receipt_reader_ref: reader.clone(),
        },
        resource_bindings_ref: object.clone(),
        authority_ref: AuthorityRef {
            delegation_id: task.task_id,
            delegation_revision: task.record_revision,
            decision_ref: task.task_id,
        },
        acceptance_predicate_ref: object.clone(),
        retry_contract_ref: object,
        checkpoint_scope_ref: None,
        state: OperationState::InFlight,
        outcome: None,
        outcome_conflict: false,
        effect_status: EffectStatus::Unknown,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: task.updated_commit,
        evidence_through: None,
    };
    let attempt = AttemptRecord {
        record_schema_version: 1,
        attempt_id: EntityId::new(),
        operation_id: operation.operation_id,
        attempt_no: 1,
        invocation_id: current.invocation_id,
        worker_id: current.worker_id,
        node_id: current.node_id,
        epoch: current.epoch,
        implementation_ref: reader,
        dispatch_stage: AttemptDispatchStage::Armed,
        armed_commit: task.updated_commit,
        provider_handle: None,
        provider_idempotency: None,
        transport_ref: None,
        started_at: None,
        last_observed_at: None,
        final_receipt_id: None,
    };
    Ok((current, operation, attempt))
}

pub struct CanonicalGuard(pub Option<shenko_types::authority::CommitAuthorityStamp>);
impl shenko_types::authority::CommitAuthorityGuard for CanonicalGuard {
    fn stamp(&self) -> Option<&shenko_types::authority::CommitAuthorityStamp> {
        self.0.as_ref()
    }
}

#[derive(Default)]
pub struct NativeT2AuthorityState {
    pub grants: std::collections::BTreeMap<String, shenko_types::authority::CommitAuthorityGrant>,
    pub last: String,
    pub lose_reply: bool,
    pub fail_before_choose: bool,
    /// Interleave after the retained choice, immediately before materializing
    /// it. Release the fixture lock before entering the real Store or SQLite.
    pub before_materialize: Option<Box<dyn FnOnce() -> Result<()> + Send>>,
}

pub struct NativeT2Authority {
    pub source: &'static str,
    pub stamp: Option<shenko_types::authority::CommitAuthorityStamp>,
    pub state: Arc<parking_lot::Mutex<NativeT2AuthorityState>>,
}

impl shenko_types::authority::CommitAuthorityProvider for NativeT2Authority {
    fn source_id(&self) -> &str {
        self.source
    }

    fn prepare(
        &self,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> std::result::Result<
        Option<shenko_types::authority::CommitAuthorityGrant>,
        shenko_types::error::ShenkoError,
    > {
        let mut state = self.state.lock();
        if std::mem::take(&mut state.fail_before_choose) {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture failed before choosing dispatch",
            ));
        }
        let grant = if let Some(original) = state.grants.get(&admission.conflict_key) {
            if original.admission != *admission {
                return Err(shenko_types::error::ShenkoError::conflict(
                    "fixture cannot replace a chosen action",
                ));
            }
            let mut replay = original.clone();
            replay.replayed = true;
            replay
        } else {
            let grant = shenko_types::authority::CommitAuthorityGrant {
                admission: admission.clone(),
                grant_id: format!("native-chosen:{}", admission.action_id),
                proof: serde_json::json!({"test":"native T2 retained chosen value"}),
                replayed: false,
            };
            state
                .grants
                .insert(admission.conflict_key.clone(), grant.clone());
            grant
        };
        state.last = admission.conflict_key.clone();
        if std::mem::take(&mut state.lose_reply) {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture lost chosen T2 reply",
            ));
        }
        Ok(Some(grant))
    }

    fn acquire(
        &self,
        grant: Option<&shenko_types::authority::CommitAuthorityGrant>,
    ) -> std::result::Result<
        Box<dyn shenko_types::authority::CommitAuthorityGuard>,
        shenko_types::error::ShenkoError,
    > {
        let mut state = self.state.lock();
        if let Some(grant) = grant
            && state
                .grants
                .get(&grant.admission.conflict_key)
                .is_none_or(|original| {
                    original.grant_id != grant.grant_id || original.admission != grant.admission
                })
        {
            return Err(shenko_types::error::ShenkoError::conflict(
                "fixture has no matching chosen T2",
            ));
        }
        let interleave = grant.and_then(|_| state.before_materialize.take());
        drop(state);
        if let Some(interleave) = interleave {
            interleave().map_err(|error| shenko_types::error::ShenkoError::other(error.to_string()))?;
        }
        Ok(Box::new(CanonicalGuard(self.stamp.clone())))
    }
}
