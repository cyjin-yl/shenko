//! Channel publication facts and the shared channel operation ledger.
//! SPEC §15.3, §16, §35.6, §41.6.
//!
//! The Store owns the GENERIC facts of a channel publication: the intent, the
//! operation, the attempt, the receipt and the subject-readable summary of
//! gaps, health and outcomes. It does not own the platform action, its
//! parameters, its acceptance predicate or its credentials — those belong to
//! the channel plugin and travel with the `ChannelDispatch` the plugin
//! produced.
//!
//! Two facts stay apart on purpose:
//!
//! * a publication that no installed channel serves was NEVER dispatched. It
//!   is refused with effect `none`; it is not an unknown remote effect and
//!   it never stops the runtime;
//! * a publication whose request may have reached the platform is `unknown`.
//!   It stays unknown until a receipt or a reconciliation settles it, and it
//!   is never repeated automatically.
use anyhow::{Context as _, Result, ensure};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::channel::{ChannelDispatch, ChannelReceipt};
use shenko_types::commit::CommitRef;
use shenko_types::ids::{ControllerEpoch, EntityId, LedgerSeq, RecordRevision};
use shenko_types::object::{ArtifactRef, Digest, ObjectRef, SchemaRef, VersionedRef};
use shenko_types::operation::{
    AttemptDispatchStage, AttemptRecord, AuthorityRef, EffectStatus, OperationContractRef,
    OperationOutcome, OperationRecord, OperationState, ResourceBindings,
};
use shenko_types::subject::{PublicationIntent, SubjectBranchHead, SubjectMode, SubjectStep};
use shenko_types::task::{DispatchEligibility, RunMode, TaskRecord, TaskState};

use crate::store::Store;

/// The version of the sealed publication-target record this Store accepts.
pub const PUBLICATION_TARGET_VERSION: u32 = 2;

/// The version of the sealed decision record this Store WRITES.
///
/// 9 adds the subject's acknowledgements of unknown external effects
/// (SPEC §41.4 X11). Decisions sealed at 8 are still admitted verbatim: they
/// simply could not carry an acknowledgement, and their meaning does not
/// change (SPEC §51.1: old bytes are not rewritten, old readings stay).
pub const DECISION_SCHEMA_VERSION: u32 = 9;

/// Every committed decision schema this build still admits as a decision.
pub const ADMITTED_DECISION_SCHEMA_VERSIONS: [u32; 2] = [8, DECISION_SCHEMA_VERSION];

/// The first decision schema that can carry a reconciliation
/// acknowledgement. A schema-8 decision never can: admitting one would make
/// older readers silently drop a decision the subject actually took.
pub const RECONCILIATION_DECISION_SCHEMA_VERSION: u32 = 9;

/// The decision schema the ONE publication-target rewrite produces.
///
/// That cutover rewrote version-1 publication targets inside already sealed
/// decisions; it never added acknowledgements. Its migration record must keep
/// naming the version it actually produced (8), not the schema this build
/// writes today, or the ledger would claim a rewrite that never happened.
pub const PUBLICATION_TARGET_DECISION_VERSION: u32 = 8;

/// The decision schema whose publication targets carried no version. The
/// migration rewrites exactly these objects, and every reader that accepts a
/// rewritten decision (an obligation creation keeps its immutable reference to
/// the original object) authenticates that rewrite through this source
/// version and the two digests the migration recorded.
pub(crate) const LEGACY_DECISION_SCHEMA_VERSION: u32 = 7;

/// The version of the sealed parsed-output record this Store accepts.
pub const OUTPUT_SCHEMA_VERSION: u32 = 5;

/// The contract of the generic channel publication operation.
///
/// It is channel-agnostic on purpose: the platform action, its parameters and
/// its acceptance predicate live in the plugin's operation contract.
pub const CHANNEL_PUBLISH_CONTRACT: &str = "shenko.channel/publish/2";

/// The channel fact kinds the runtime records.
pub const CHANNEL_FACT_KINDS: &[&str] = &[
    "ingress_gap",
    "health",
    "publication_outcome",
    "attachment_locator",
    "admin_grant",
];

pub const CHANNEL_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS channel_runtime_facts(
 fact_id TEXT PRIMARY KEY,
 channel TEXT NOT NULL,
 account TEXT NOT NULL,
 kind TEXT NOT NULL,
 state TEXT NOT NULL,
 subject_ref TEXT,
 detail TEXT NOT NULL,
 revision INTEGER NOT NULL DEFAULT 0,
 created_seq INTEGER NOT NULL,
 updated_seq INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS channel_runtime_facts_pending
 ON channel_runtime_facts(channel,kind,state,created_seq);
CREATE TABLE IF NOT EXISTS channel_record_migrations(
 record_kind TEXT NOT NULL,
 record_id TEXT NOT NULL,
 from_version INTEGER NOT NULL,
 to_version INTEGER NOT NULL,
 source_digest TEXT NOT NULL,
 target_digest TEXT NOT NULL,
 migrated_seq INTEGER NOT NULL,
 PRIMARY KEY(record_kind,record_id));
"#;

/// One subject-readable channel fact.
///
/// The fact is a projection of what actually happened, not a second truth
/// store: the ingress ledger I stays with the plugin that owns it, and the
/// operation ledger stays with the Store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelFact {
    pub fact_id: EntityId,
    pub channel: String,
    pub account: String,
    pub kind: String,
    pub state: String,
    pub subject_ref: Option<String>,
    pub detail: serde_json::Value,
    pub revision: u64,
    pub created_seq: u64,
    pub updated_seq: u64,
}

/// A new channel fact to record.
#[derive(Debug, Clone)]
pub struct NewChannelFact {
    pub channel: String,
    pub account: String,
    pub kind: String,
    pub state: String,
    pub subject_ref: Option<String>,
    pub detail: serde_json::Value,
}

/// A publication whose durable operation is armed and may be dispatched.
#[derive(Debug, Clone)]
pub struct ArmedPublication {
    pub operation_id: EntityId,
    pub attempt_id: EntityId,
    pub task_id: EntityId,
    pub account: String,
    pub armed_commit: CommitRef,
}

/// What the committed facts allow for one publication intent.
///
/// Only `Arm` authorizes the caller to touch the network. `Deferred` changes
/// nothing: the subject is paused, or the record is in a state this admission
/// cannot decide about.
/// `Undispatchable` is a committed fact that can never make this intent
/// dispatchable again; it is settled durably instead of being retried.
enum PublicationAdmission {
    NotReady,
    Deferred(&'static str),
    Undispatchable {
        kind: &'static str,
        reason: String,
    },
    Arm {
        task: TaskRecord,
        authoring_step: EntityId,
        epoch: ControllerEpoch,
    },
}

/// The result of one T1 attempt, after its transaction settled.
enum PublicationArm {
    Armed(ArmedPublication),
    Idle,
    Undispatchable {
        kind: &'static str,
        reason: String,
    },
}

impl Store {
    /// Record one subject-readable channel fact.
    pub fn record_channel_fact(&self, fact: &NewChannelFact) -> Result<EntityId> {
        let fact_id = EntityId::new();
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO channel_runtime_facts(fact_id,channel,account,kind,state,subject_ref,detail,revision,created_seq,updated_seq)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?8)",
                    params![
                        fact_id.to_string(),
                        fact.channel,
                        fact.account,
                        fact.kind,
                        fact.state,
                        fact.subject_ref,
                        fact.detail.to_string(),
                        seq.0 as i64,
                    ],
                )?;
                tx.commit()?;
                Ok(fact_id)
            },
            |error: &anyhow::Error| {
                error.is::<rusqlite::Error>()
                    || error
                        .downcast_ref::<shenko_types::error::ShenkoError>()
                        .is_some_and(|error| matches!(error, shenko_types::error::ShenkoError::Storage(_)))
            },
        )
    }

    /// Read the subject-readable channel facts.
    pub fn channel_facts(
        &self,
        channel: Option<&str>,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ChannelFact>> {
        let conn = self.ledger.conn.lock();
        let mut statement = conn.prepare(
            "SELECT fact_id,channel,account,kind,state,subject_ref,detail,revision,created_seq,updated_seq
             FROM channel_runtime_facts
             WHERE (?1 IS NULL OR channel=?1) AND (?2 IS NULL OR kind=?2)
             ORDER BY created_seq,fact_id LIMIT ?3",
        )?;
        let rows = statement
            .query_map(params![channel, kind, limit as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(
                    fact_id,
                    channel,
                    account,
                    kind,
                    state,
                    subject_ref,
                    detail,
                    revision,
                    created_seq,
                    updated_seq,
                )| {
                    Ok(ChannelFact {
                        fact_id: fact_id.parse()?,
                        channel,
                        account,
                        kind,
                        state,
                        subject_ref,
                        detail: serde_json::from_str(&detail)?,
                        revision: u64::try_from(revision)?,
                        created_seq: u64::try_from(created_seq)?,
                        updated_seq: u64::try_from(updated_seq)?,
                    })
                },
            )
            .collect()
    }

    /// The committed publications the channel runtime may dispatch.
    pub fn ready_publications(&self, limit: usize) -> Result<Vec<PublicationIntent>> {
        self.committed_intents(Some("publication"), Some("ready"), limit)?
            .into_iter()
            .map(|intent| Ok(serde_json::from_value(intent.payload)?))
            .collect()
    }

    /// T1: claim the publication intent and arm its operation and attempt.
    ///
    /// The intent claim, the armed operation/attempt and the task projection
    /// commit in ONE transaction (SPEC §41.4 X07). A crash can therefore never
    /// leave an intent claimed without the attempt that approves a send, so
    /// the outbox has no durable state that is neither dispatchable nor
    /// reconcilable. A repeated admission — or any intent that is not `ready`
    /// — returns `None` and never authorizes a second send.
    ///
    /// A `ready` intent that committed facts can never dispatch again (its
    /// task was cancelled, is terminal or parked, or its authoring step
    /// belongs to a superseded controller epoch) is settled durably as never
    /// dispatched with effect `none`. It is not a transport failure and it
    /// never stops the caller's dispatch loop; the subject can read the
    /// settled intent and decide again.
    pub fn arm_publication(
        &self,
        intent_id: EntityId,
        dispatch: &ChannelDispatch,
    ) -> Result<Option<ArmedPublication>> {
        let outcome = self.ledger.with_mutation(
            || self.try_arm_publication(intent_id, dispatch),
            |error| {
                error.is::<rusqlite::Error>()
                    || error.is::<serde_json::Error>()
                    || error
                        .downcast_ref::<shenko_types::error::ShenkoError>()
                        .is_some_and(|error| {
                            matches!(error, shenko_types::error::ShenkoError::Storage(_))
                        })
            },
        )?;
        match outcome {
            PublicationArm::Armed(armed) => Ok(Some(armed)),
            PublicationArm::Idle => Ok(None),
            PublicationArm::Undispatchable { kind, reason } => {
                // No claim was committed, so this intent is still `ready` and
                // no dispatcher can ever have sent it: settling it describes
                // the intent, not an external effect.
                self.refuse_publication_detail(
                    intent_id,
                    serde_json::json!({
                        "kind": "publication_never_dispatched",
                        "reason_kind": kind,
                        "reason": reason,
                        "dispatched": false,
                        "effect_status": "none",
                    }),
                    false,
                )?;
                Ok(None)
            }
        }
    }

    fn try_arm_publication(
        &self,
        intent_id: EntityId,
        dispatch: &ChannelDispatch,
    ) -> Result<PublicationArm> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.check_dispatch_owner(&tx)?;
        let (task, authoring_step, epoch) = match self.publication_admission(&tx, intent_id)? {
            PublicationAdmission::NotReady | PublicationAdmission::Deferred(_) => {
                return Ok(PublicationArm::Idle);
            }
            PublicationAdmission::Undispatchable { kind, reason } => {
                return Ok(PublicationArm::Undispatchable { kind, reason });
            }
            PublicationAdmission::Arm {
                task,
                authoring_step,
                epoch,
            } => (task, authoring_step, epoch),
        };
        let armed = self.arm_publication_tx(&tx, intent_id, dispatch, &task, authoring_step, epoch)?;
        tx.commit()?;
        Ok(PublicationArm::Armed(armed))
    }

    fn arm_publication_tx(
        &self,
        tx: &Transaction<'_>,
        intent_id: EntityId,
        dispatch: &ChannelDispatch,
        task: &TaskRecord,
        authoring_step: EntityId,
        epoch: ControllerEpoch,
    ) -> Result<ArmedPublication> {
        let mut record = self.publication_operation(intent_id, dispatch, authoring_step)?;
        let seq = self.ledger.allocate_seq(tx)?;
        let commit = CommitRef::new(self.ledger.ledger_id(), seq);
        record.prepared_commit = commit;
        let attempt_id = EntityId::new();
        let invocation_id = EntityId::new();
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id,
            operation_id: intent_id,
            attempt_no: 1,
            invocation_id,
            worker_id: self.capabilities.incarnation,
            node_id: self.ledger.ledger_id(),
            epoch,
            implementation_ref: record.contract_ref.receipt_reader_ref.clone(),
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: commit,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: Some(record.resource_bindings_ref.clone()),
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        let mut task = task.clone();
        task.state = TaskState::Running {
            checkpoint_ref: None,
            invocation_id,
            mode: RunMode::Run,
            epoch,
        };
        task.record_revision = task.record_revision.next();
        task.updated_commit = commit;
        task.memos.insert(
            "external:last_operation".into(),
            serde_json::json!({
                "operation_id": intent_id,
                "attempt_id": attempt_id,
            }),
        );
        tx.execute(
            "INSERT INTO operations(operation_id,record_revision,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![
                intent_id.to_string(),
                record.record_revision.0 as i64,
                serde_json::to_string(&record)?,
                seq.0 as i64
            ],
        )?;
        tx.execute(
            "INSERT INTO attempts(attempt_id,operation_id,data,updated_commit_seq) VALUES(?1,?2,?3,?4)",
            params![
                attempt_id.to_string(),
                intent_id.to_string(),
                serde_json::to_string(&attempt)?,
                seq.0 as i64
            ],
        )?;
        tx.execute(
            "UPDATE tasks SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE task_id=?1",
            params![
                intent_id.to_string(),
                task.record_revision.0 as i64,
                serde_json::to_string(&task)?,
                seq.0 as i64
            ],
        )?;
        let claimed = tx.execute(
            "UPDATE subject_intents SET state='claimed',owner=?2,revision=revision+1,updated_seq=?3 WHERE intent_id=?1 AND state='ready'",
            params![
                intent_id.to_string(),
                self.capabilities.incarnation.to_string(),
                seq.0 as i64
            ],
        )?;
        ensure!(claimed == 1, "publication intent left the ready index inside T1");
        tx.execute(
            "INSERT INTO commit_log(ledger_id,seq,incarnation,entity_count) VALUES(?1,?2,?3,4)",
            params![
                commit.ledger_id.to_string(),
                seq.0 as i64,
                self.capabilities.incarnation.to_string()
            ],
        )?;
        Ok(ArmedPublication {
            operation_id: intent_id,
            attempt_id,
            task_id: intent_id,
            account: dispatch.account.clone(),
            armed_commit: commit,
        })
    }

    /// Classify one publication intent from its committed facts.
    ///
    /// The admission is read inside the T1 transaction that acts on it, so a
    /// cancellation, a park or an epoch advance cannot be observed stale.
    fn publication_admission(
        &self,
        tx: &Transaction<'_>,
        intent_id: EntityId,
    ) -> Result<PublicationAdmission> {
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM subject_intents WHERE intent_id=?1 AND kind='publication'",
                [intent_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if state.as_deref() != Some("ready") {
            return Ok(PublicationAdmission::NotReady);
        }
        // A `ready` intent that already owns an operation cannot come out of
        // T1 (claim and operation commit together). If some legacy or restored
        // record produced it, an armed attempt may exist: settling it as never
        // dispatched would be a false statement about an external effect, and
        // arming again would be a second send. Leave it untouched for
        // reconciliation instead.
        let owns_operation: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM operations WHERE operation_id=?1",
                [intent_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if owns_operation.is_some() {
            return Ok(PublicationAdmission::Deferred(
                "a ready publication already owns an operation",
            ));
        }
        let data: String = tx.query_row(
            "SELECT data FROM tasks WHERE task_id=?1",
            [intent_id.to_string()],
            |row| row.get(0),
        )?;
        let task: TaskRecord = serde_json::from_str(&data)?;
        if let Some(abort) = &task.abort_request {
            return Ok(PublicationAdmission::Undispatchable {
                kind: "cancelled",
                reason: format!("publication was cancelled before T1: {}", abort.reason),
            });
        }
        if task.is_terminal() {
            return Ok(PublicationAdmission::Undispatchable {
                kind: "task_terminal",
                reason: "publication task is already terminal".into(),
            });
        }
        match &task.dispatch_eligibility {
            DispatchEligibility::Ready => {}
            DispatchEligibility::Blocked { reason_ref } => {
                return Ok(PublicationAdmission::Undispatchable {
                    kind: "blocked",
                    reason: format!("publication dispatch is blocked: {reason_ref}"),
                });
            }
            DispatchEligibility::Parked { reason_ref, .. } => {
                return Ok(PublicationAdmission::Undispatchable {
                    kind: "parked",
                    reason: format!("publication dispatch is parked: {reason_ref}"),
                });
            }
        }
        if task.memos.contains_key("native_settlement_only") {
            return Ok(PublicationAdmission::Undispatchable {
                kind: "settlement_invocation",
                reason: "a settlement-only invocation never arms an external send".into(),
            });
        }
        let authoring_step = task
            .created_by_step_id
            .context("publication task has no authoring step")?;
        let step: SubjectStep = {
            let data: String = tx.query_row(
                "SELECT data FROM subject_steps WHERE step_id=?1",
                [authoring_step.to_string()],
                |row| row.get(0),
            )?;
            serde_json::from_str(&data)?
        };
        let head: SubjectBranchHead = {
            let data: String = tx.query_row(
                "SELECT data FROM subject_branch_heads WHERE branch_id=?1",
                [step.branch_id.0.to_string()],
                |row| row.get(0),
            )?;
            serde_json::from_str(&data)?
        };
        if head.mode != SubjectMode::Active {
            return Ok(PublicationAdmission::Deferred("subject is not active"));
        }
        if head.controller_epoch != step.controller_epoch {
            return Ok(PublicationAdmission::Undispatchable {
                kind: "authority_superseded",
                reason: format!(
                    "controller epoch advanced from {:?} to {:?} after this step committed",
                    step.controller_epoch, head.controller_epoch
                ),
            });
        }
        Ok(PublicationAdmission::Arm {
            task,
            authoring_step,
            epoch: head.controller_epoch,
        })
    }

    /// E1: the provider answered. The receipt is the evidence.
    pub fn record_publication_receipt(
        &self,
        armed: &ArmedPublication,
        receipt: &ChannelReceipt,
    ) -> Result<()> {
        let outcome = if receipt.accepted {
            OperationOutcome::Succeeded
        } else {
            OperationOutcome::Failed
        };
        let effect = if receipt.accepted {
            EffectStatus::Complete
        } else {
            EffectStatus::None
        };
        let evidence = self.publication_evidence(armed, receipt, Some(outcome), Some(effect))?;
        self.record_external_evidence(
            armed.operation_id,
            armed.attempt_id,
            evidence,
            Some(outcome),
            effect,
        )?;
        Ok(())
    }

    /// E1: the transport lost the answer. The external effect is unknown.
    ///
    /// The operation stays reconciling. Nothing is resent automatically: the
    /// subject or a delegated worker decides after looking at the platform.
    pub fn record_publication_unknown(
        &self,
        armed: &ArmedPublication,
        detail: &str,
    ) -> Result<()> {
        let evidence = self.publication_evidence(
            armed,
            &ChannelReceipt {
                status: "unknown".to_string(),
                retcode: -1,
                provider_message_id: None,
                raw: serde_json::json!({ "transport_error": detail }),
                observed_at: chrono::Utc::now(),
                accepted: false,
            },
            None,
            None,
        )?;
        self.record_external_evidence(
            armed.operation_id,
            armed.attempt_id,
            evidence,
            None,
            EffectStatus::Unknown,
        )?;
        Ok(())
    }

    /// Refuse a publication that no installed channel serves.
    ///
    /// Nothing was dispatched: the effect is `none` and the failure is
    /// durable. The intent stays visible as blocked instead of spinning in
    /// the ready index, and the runtime keeps running.
    pub fn refuse_publication(&self, intent_id: EntityId, reason: &str) -> Result<()> {
        self.refuse_publication_detail(
            intent_id,
            serde_json::json!({
                "kind": "publication_refused",
                "reason": reason,
                "dispatched": false,
                "effect_status": "none",
            }),
            true,
        )?;
        Ok(())
    }

    /// Settle one publication that was never dispatched.
    ///
    /// `strict` is the caller's contract, not a tuning knob: the plugin
    /// refusal path requires the intent to still be `ready`, because a caller
    /// that lost a race must not overwrite another dispatcher's claim. An
    /// admission that read committed facts saying "never dispatch" passes
    /// `false`: a claim committed in between is left alone and answers
    /// `false`.
    fn refuse_publication_detail(
        &self,
        intent_id: EntityId,
        detail: serde_json::Value,
        strict: bool,
    ) -> Result<bool> {
        let object = self
            .cas
            .seal_object(
                &serde_json::to_vec(&detail)?,
                "application/json",
                "channel_publication_outcomes",
                "json",
                1,
            )?
            .object;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let state: Option<String> = tx
                    .query_row(
                        "SELECT state FROM subject_intents WHERE intent_id=?1 AND kind='publication'",
                        [intent_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if state.as_deref() != Some("ready") {
                    ensure!(!strict, "publication is not ready: {state:?}");
                    tx.commit()?;
                    return Ok(false);
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                tx.execute(
                    "UPDATE subject_intents SET state='blocked',owner=?2,revision=revision+1,updated_seq=?3,result_ref=?4
                     WHERE intent_id=?1 AND state='ready'",
                    params![
                        intent_id.to_string(),
                        self.capabilities.incarnation.to_string(),
                        seq.0 as i64,
                        object.locator.clone(),
                    ],
                )?;
                let task: Option<String> = tx
                    .query_row(
                        "SELECT data FROM tasks WHERE task_id=?1",
                        [intent_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(data) = task {
                    let mut task: shenko_types::task::TaskRecord = serde_json::from_str(&data)?;
                    // A publication that never armed has no send to fence and no
                    // abort handler of its own: the settlement is the only writer
                    // that can retire its lifetime, cancelled or not.
                    if !task.is_terminal() {
                        task.state = shenko_types::task::TaskState::Terminal {
                            outcome_ref: object.clone(),
                        };
                        task.record_revision = task.record_revision.next();
                        task.updated_commit = commit;
                        task.terminal_at = Some(chrono::Utc::now());
                        tx.execute(
                            "UPDATE tasks SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE task_id=?1",
                            params![
                                intent_id.to_string(),
                                task.record_revision.0 as i64,
                                serde_json::to_string(&task)?,
                                seq.0 as i64,
                            ],
                        )?;
                    }
                }
                tx.commit()?;
                Ok(true)
            },
            |error| {
                error.is::<rusqlite::Error>()
                    || error
                        .downcast_ref::<shenko_types::error::ShenkoError>()
                        .is_some_and(|error| matches!(error, shenko_types::error::ShenkoError::Storage(_)))
            },
        )
    }

    fn publication_operation(
        &self,
        operation_id: EntityId,
        dispatch: &ChannelDispatch,
        authoring_step: EntityId,
    ) -> Result<OperationRecord> {
        let input = self.seal_channel_json(
            "channel_publication_input",
            &serde_json::json!({
                "channel": dispatch.channel,
                "action": dispatch.action,
                "platform_action": dispatch.platform_action,
                "params": dispatch.params,
                "account": dispatch.account,
                "tenant": dispatch.tenant,
                "conversation": dispatch.conversation,
            }),
        )?;
        let bindings = self.seal_channel_json(
            "channel_publication_bindings",
            &serde_json::to_value(ResourceBindings {
                provider: dispatch.channel.clone(),
                account_identity: dispatch.account.clone(),
                runtime_node: "local".to_string(),
                mount_generation: 0,
                capability_level: dispatch.platform_action.clone(),
                extra: serde_json::json!({
                    "tenant": dispatch.tenant,
                    "conversation": dispatch.conversation,
                }),
            })?,
        )?;
        let acceptance = self.seal_channel_json(
            "channel_publication_predicates",
            &serde_json::json!({
                "predicate": dispatch.acceptance_predicate,
                "channel": dispatch.channel,
            }),
        )?;
        let retry = self.seal_channel_json(
            "channel_publication_predicates",
            &serde_json::json!({
                "max_attempts": 1,
                "on_timeout": "reconcile",
                "blind_resend": false,
            }),
        )?;
        Ok(OperationRecord {
            record_schema_version: 1,
            operation_id,
            record_revision: RecordRevision(0),
            task_id: operation_id,
            logical_key: format!(
                "channel.{}:{}:{}",
                dispatch.channel, dispatch.platform_action, operation_id
            ),
            operation_name: format!("channel.{}.{}", dispatch.channel, dispatch.platform_action),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input.clone(),
            input_digest: input.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: EntityId::nil(),
                artifact_digest: dispatch.contract_digest.clone(),
                contract_digest: dispatch.contract_digest.clone(),
                receipt_reader_ref: VersionedRef {
                    artifact: ArtifactRef {
                        digest: dispatch.contract_digest.clone(),
                        locator: format!("channel:{}", dispatch.channel),
                    },
                    symbol: "ChannelReceipt".to_string(),
                    schema: SchemaRef {
                        name: "channel_receipt".to_string(),
                        major: 1,
                        digest: Digest::blake3(b"channel_receipt"),
                    },
                },
            },
            resource_bindings_ref: bindings,
            authority_ref: AuthorityRef {
                delegation_id: operation_id,
                delegation_revision: RecordRevision(0),
                decision_ref: authoring_step,
            },
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: CommitRef::new(self.ledger.ledger_id(), LedgerSeq::ZERO),
            evidence_through: None,
        })
    }

    fn publication_evidence(
        &self,
        armed: &ArmedPublication,
        receipt: &ChannelReceipt,
        outcome: Option<OperationOutcome>,
        effect: Option<EffectStatus>,
    ) -> Result<shenko_types::evidence::EvidenceReceipt> {
        let attempt = self.external_operation_attempt(armed.operation_id)?;
        let raw = self.seal_channel_json(
            "channel_publication_receipts",
            &serde_json::to_value(receipt)?,
        )?;
        Ok(shenko_types::evidence::EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(armed.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: armed.account.clone(),
            source_instance_id: armed.account.clone(),
            source_event_key: None,
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("channel_receipt".to_string()),
            raw_ref: raw,
            decoder_ref: Some(attempt.implementation_ref.clone()),
            decode_status: shenko_types::evidence::DecodeStatus::Decoded,
            observation_level: outcome.map(|_| {
                shenko_types::evidence::ObservationLevel::ProviderAccepted
            }),
            normalized_ref: None,
            observed_at: Some(receipt.observed_at),
            received_commit: armed.armed_commit,
        })
    }

    fn seal_channel_json(
        &self,
        namespace: &str,
        value: &serde_json::Value,
    ) -> Result<ObjectRef> {
        Ok(self
            .cas
            .seal_object(
                &serde_json::to_vec(value)?,
                "application/json",
                namespace,
                "json",
                1,
            )?
            .object)
    }

    /// E1 remains append-only and independent. A CURRENT dispatcher derives T2
    /// progress from it; a late callback itself does not advance the task.
    ///
    /// A process restart is local loss-of-control evidence, NOT proof that the
    /// remote send stopped or failed: an armed operation that lost its owner
    /// becomes `unknown`, and it stays unknown until a receipt or a
    /// reconciliation settles it.
    pub fn reconcile_subject_publications(&self) -> Result<()> {
        let mut outcomes = std::collections::HashMap::new();
        let changed = {
                let conn = self.ledger.conn.lock();
            // `acknowledged` belongs here: a subject that accepted this
            // unknown effect still settles it the moment definite evidence
            // arrives, and the case it acknowledged turns `resolved` with
            // it. It is never re-armed and never silently resent.
            let mut statement = conn.prepare("SELECT i.intent_id FROM subject_intents i JOIN operations o ON o.operation_id=i.intent_id WHERE i.kind='publication' AND i.state IN ('claimed','unknown','acknowledged') AND (i.result_ref IS NULL OR i.result_ref!=o.data) AND (json_extract(o.data,'$.state') IN ('resolved','reconciling') OR (i.state='unknown' AND i.owner IS NULL)) ORDER BY i.updated_seq LIMIT 32")?;
                    statement
                .query_map([], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
        for id in changed {
            let (state, owner): (String, Option<String>) = {
                let conn = self.ledger.conn.lock();
                conn.query_row(
                    "SELECT state,owner FROM subject_intents WHERE intent_id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                                    )?
                                };
            if !matches!(state.as_str(), "claimed" | "unknown" | "acknowledged") {
                continue;
                }
            let intent_id: EntityId = id.parse()?;
            if let Some(mut operation) = self.get_operation(intent_id)? {
                if state == "unknown"
                    && owner.is_none()
                    && operation.state == OperationState::InFlight
                {
                    let attempt = self.external_operation_attempt(intent_id)?;
                    let raw = self.cas.seal_object(br#"{"observation":"dispatcher restarted; armed send outcome unknown","provider_stop_proven":false}"#,"application/json","recovery_evidence","json",1)?.object;
                    let evidence = shenko_types::evidence::EvidenceReceipt {
                        record_schema_version: 1,
                        receipt_id: EntityId::new(),
                        operation_id: Some(intent_id),
                        attempt_id: Some(attempt.attempt_id),
                        source_id: "subject-dispatch-recovery".into(),
                        source_instance_id: self.capabilities.incarnation.to_string(),
                        source_event_key: None,
                        producer_epoch: None,
                        producer_schema_version: Some("1".into()),
                        raw_ref: raw,
                        decoder_ref: None,
                        decode_status: shenko_types::evidence::DecodeStatus::Decoded,
                        observation_level: None,
                        normalized_ref: None,
                        observed_at: Some(chrono::Utc::now()),
                        received_commit: attempt.armed_commit,
                    };
                    operation = self.record_external_evidence(
                            intent_id,
                        attempt.attempt_id,
                        evidence,
                        None,
                        EffectStatus::Unknown,
                    )?;
                }
                if operation.state == OperationState::Resolved {
                    let bytes = serde_json::to_vec(&operation)?;
                                let object = self
                                    .cas
                                    .seal_object(
                            &bytes,
                                        "application/json",
                            "publication_outcomes",
                                        "OperationRecord",
                                        1,
                                    )?
                                    .object;
                    outcomes.insert(intent_id, (operation.record_revision, object));
                }
                            }
                        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                self.check_dispatch_owner(&tx)?;
                let rows: Vec<(String, String)> = {
                    // An acknowledged intent is only revisited when the
                    // operation reached a definite result: while it is still
                    // reconciling the subject's decision stands and must not
                    // be overwritten back to `unknown` on every pass.
                    let mut statement = tx.prepare("SELECT i.intent_id,o.data FROM subject_intents i JOIN operations o ON o.operation_id=i.intent_id WHERE i.kind='publication' AND i.state IN ('claimed','unknown','acknowledged') AND (json_extract(o.data,'$.state')='resolved' OR (json_extract(o.data,'$.state')='reconciling' AND i.state IN ('claimed','unknown'))) AND (i.result_ref IS NULL OR i.result_ref!=o.data) LIMIT 32")?;
                    statement
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?
                };
                if rows.is_empty() {
                    return Ok(());
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                for (id, data) in rows {
                    let operation: OperationRecord = serde_json::from_str(&data)?;
                    let resolved = operation.state == OperationState::Resolved;
                    if resolved
                        && !outcomes
                            .get(&operation.operation_id)
                            .is_some_and(|(revision, _)| *revision == operation.record_revision)
                    {
                        continue;
                    }
                    let result = serde_json::to_string(&operation)?;
                    tx.execute(
                        "UPDATE subject_intents SET state=?2,owner=?3,revision=revision+1,updated_seq=?4,result_ref=?5 WHERE intent_id=?1",
                        params![
                           id,
                            if resolved { "settled" } else { "unknown" },
                            self.capabilities.incarnation.to_string(),
                            seq.0 as i64,
                            result
                        ],
                    )?;
                    if resolved {
                        let task: Option<String> = tx
                            .query_row(
                                "SELECT data FROM tasks WHERE task_id=?1",
                                [&id],
                                |row| row.get(0),
                            )
                            .optional()?;
                        if let Some(data) = task {
                            let mut task: shenko_types::task::TaskRecord =
                                serde_json::from_str(&data)?;
                            if !task.is_terminal() && task.abort_request.is_none() {
                                task.state = shenko_types::task::TaskState::Terminal {
                                    outcome_ref: outcomes
                                        .get(&operation.operation_id)
                                        .context("resolved publication lacks its sealed outcome")?
                                        .1.clone(),
                               };
                                task.record_revision = task.record_revision.next();
                                task.updated_commit = commit;
                                task.terminal_at = Some(chrono::Utc::now());
                                tx.execute(
                                    "UPDATE tasks SET record_revision=?2,data=?3,updated_commit_seq=?4 WHERE task_id=?1",
                                    params![
                                       id,
                                        task.record_revision.0 as i64,
                                        serde_json::to_string(&task)?,
                                        seq.0 as i64
                                    ],
                                )?;
                            }
                        }
                        let receipt: Option<(String, String)> = tx.query_row(
                            "SELECT receipt_id,data FROM evidence_receipts WHERE operation_id=?1
                            ORDER BY updated_commit_seq DESC LIMIT 1", [&id],
                           |row| Ok((row.get(0)?, row.get(1)?)),
                        ).optional()?;
                        if let Some((receipt_id, receipt)) = receipt {
                            self.append_intent_result_fact(
                                &tx,
                                operation.operation_id,
                                receipt_id.parse()?,
                                &serde_json::json!({"kind":"publication_receipt", "operation":operation,
                                    "receipt":serde_json::from_str::<serde_json::Value>(&receipt)?}),
                    )?;
                        }
                    }
                }
                self.intent_log(&tx, commit)?;
                tx.commit()?;
                Ok(())
            },
            |error: &anyhow::Error| {
                error.is::<rusqlite::Error>()
                    || error
                        .downcast_ref::<shenko_types::error::ShenkoError>()
                        .is_some_and(|error| matches!(error, shenko_types::error::ShenkoError::Storage(_)))
            },
        )
    }
}

/// The rows of a table that still hold a legacy publication target.
fn legacy_publication_rows(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT intent_id FROM subject_intents WHERE kind='publication' AND payload LIKE '%\"target\":%'",
    )?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Rewrite one legacy publication intent payload into the versioned shape.
fn migrate_intent_payload(payload: &str) -> Result<Option<String>> {
    let mut value: serde_json::Value = serde_json::from_str(payload)?;
    let Some(target) = value.get("target") else {
        return Ok(None);
    };
    if target.get("schema_version").is_some() {
        return Ok(None);
    }
    let Some(migrated) = shenko_types::output::PublicationTarget::read_legacy(target) else {
        return Ok(None);
    };
    value["target"] = serde_json::to_value(migrated)?;
    Ok(Some(value.to_string()))
}

/// Migrate the sealed publication targets of this installation.
///
/// Older records carried a numeric id and no account scope, and the decision
/// object that contains them used the previous schema versions. They are
/// converted explicitly:
///
/// * the id becomes its exact decimal text;
/// * the account scope stays empty, because an old record carried none — an
///   unscoped target is served only by a plugin bound to a single account,
///   and the account it actually used is recorded in the operation bindings;
/// * the decision object is sealed again with the current schema versions and
///   the step points at the new object.
///
/// The previous objects stay in the CAS. Nothing is deleted and no reader
/// keeps an unversioned alias.
pub fn migrate_publication_targets(store: &Store) -> Result<usize> {
    let mut migrated = 0usize;
    let legacy_intents = {
        let conn = store.ledger.conn.lock();
        legacy_publication_rows(&conn)?
    };
    for intent_id in legacy_intents {
        let payload: String = {
            let conn = store.ledger.conn.lock();
            conn.query_row(
                "SELECT payload FROM subject_intents WHERE intent_id=?1",
                [&intent_id],
                |row| row.get(0),
            )?
        };
        let Some(migrated_payload) = migrate_intent_payload(&payload)? else {
            continue;
        };
        let id = EntityId::new();
        store.ledger.with_mutation(
            || {
                let mut conn = store.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let seq = store.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "UPDATE subject_intents SET payload=?2,revision=revision+1,updated_seq=?3 WHERE intent_id=?1",
                    params![intent_id, migrated_payload, seq.0 as i64],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO channel_record_migrations(record_kind,record_id,from_version,to_version,source_digest,target_digest,migrated_seq)
                     VALUES('subject_intent',?1,1,?2,'','',?3)",
                    params![intent_id, PUBLICATION_TARGET_VERSION, seq.0 as i64],
                )?;
                tx.commit()?;
                Ok(())
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>(),
        )?;
        migrated += 1;
        let _ = id;
    }
    migrated += migrate_decision_objects(store)?;
    Ok(migrated)
}

/// Rewrite the committed decision objects that still hold a legacy target.
fn migrate_decision_objects(store: &Store) -> Result<usize> {
    let steps: Vec<(String, String)> = {
        let conn = store.ledger.conn.lock();
        let mut statement = conn.prepare(
            "SELECT step_id,data FROM subject_steps WHERE state='committed' ORDER BY step_no",
        )?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut migrated = 0usize;
    for (step_id, data) in steps {
        let step: shenko_types::subject::SubjectStep = serde_json::from_str(&data)?;
        let Some(decision_ref) = step.decision_ref.clone() else {
            continue;
        };
        let bytes = match store.cas.read_object(&decision_ref.digest) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let Ok(mut decision) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            continue;
        };
        if !decision_has_legacy_target(&decision) {
            continue;
        }
        rewrite_legacy_targets(&mut decision);
        let new_bytes = serde_json::to_vec(&decision)?;
        let sealed = store.cas.seal_object(
            &new_bytes,
            "application/json",
            "decisions",
            "StepDecision",
            PUBLICATION_TARGET_DECISION_VERSION,
        )?;
        store.ledger.with_mutation(
            || {
                let mut conn = store.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let seq = store.ledger.allocate_seq(&tx)?;
                let mut step: shenko_types::subject::SubjectStep = serde_json::from_str(&data)?;
                step.decision_ref = Some(sealed.object.clone());
                tx.execute(
                    "UPDATE subject_steps SET data=?2 WHERE step_id=?1",
                    params![step_id, serde_json::to_string(&step)?],
                )?;
                // A frozen child input points at the same decision object. It
                // must follow, or the child could no longer read its own
                // decision.
                tx.execute(
                    "UPDATE tasks SET data=json_set(data,'$.input_ref',json(?2))
                     WHERE json_extract(data,'$.input_ref.digest')=?1
                       AND json_extract(data,'$.state.status') IS NOT 'terminal'",
                    params![decision_ref.digest.to_string(), serde_json::to_string(&sealed.object)?],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO channel_record_migrations(record_kind,record_id,from_version,to_version,source_digest,target_digest,migrated_seq)
                     VALUES('subject_step',?1,?2,?3,?4,?5,?6)",
                    params![
                        step_id,
                        PUBLICATION_TARGET_DECISION_VERSION,                        LEGACY_DECISION_SCHEMA_VERSION,
                        DECISION_SCHEMA_VERSION,                        decision_ref.digest.to_string(),
                        sealed.object.digest.to_string(),
                        seq.0 as i64,
                    ],
                )?;
                tx.commit()?;
                Ok(())
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>(),
        )?;
        migrated += 1;
    }
    Ok(migrated)
}

/// Whether a decision object still holds a target without a version.
pub(crate) fn decision_has_legacy_target(decision: &serde_json::Value) -> bool {
    if let Some(intents) = decision.get("publication_intents").and_then(|v| v.as_array()) {
        for intent in intents {
            if let Some(target) = intent.get("target") {
                if target.get("schema_version").is_none() {
                    return true;
                }
            }
        }
    }
    if let Some(controls) = decision
        .get("output")
        .and_then(|output| output.get("controls"))
        .and_then(|v| v.as_array())
    {
        for control in controls {
            if let Some(target) = control.pointer("/action/target") {
                if target.get("schema_version").is_none() {
                    return true;
                }
            }
        }
    }
    false
}

/// Rewrite every legacy target of a decision object and bump its versions.
pub(crate) fn rewrite_legacy_targets(decision: &mut serde_json::Value) {
    if let Some(intents) = decision
        .get_mut("publication_intents")
        .and_then(|v| v.as_array_mut())
    {
        for intent in intents.iter_mut() {
            if let Some(target) = intent.get_mut("target") {
                if target.get("schema_version").is_none() {
                    if let Some(migrated) =
                        shenko_types::output::PublicationTarget::read_legacy(target)
                    {
                        *target = serde_json::to_value(migrated).unwrap_or(serde_json::Value::Null);
                    }
                }
            }
        }
    }
    if let Some(controls) = decision
        .get_mut("output")
        .and_then(|output| output.get_mut("controls"))
        .and_then(|v| v.as_array_mut())
    {
        for control in controls.iter_mut() {
            if let Some(target) = control.pointer_mut("/action/target") {
                if target.get("schema_version").is_none() {
                    if let Some(migrated) =
                        shenko_types::output::PublicationTarget::read_legacy(target)
                    {
                        *target = serde_json::to_value(migrated).unwrap_or(serde_json::Value::Null);
                    }
                }
            }
        }
    }
    if let Some(object) = decision.as_object_mut() {
        if object.get("schema_version").and_then(|v| v.as_u64()) == Some(7) {
            object.insert("schema_version".to_string(), serde_json::json!(8));
        }
    }
    if let Some(output) = decision
        .get_mut("output")
        .and_then(|output| output.as_object_mut())
    {
        if output.get("schema_version").and_then(|v| v.as_u64()) == Some(4) {
            output.insert("schema_version".to_string(), serde_json::json!(5));
        }
    }
}
