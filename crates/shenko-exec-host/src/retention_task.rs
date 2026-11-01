//! Retention handler — `retention.apply`.
//!
//! Applies one bound retention mechanism to the sealed object set of the
//! subject's own history. Every mechanism is closure-complete: the request
//! freezes the exact object set, the write records a tombstone or an archive
//! move whose closure lists every affected reference, and the content store's
//! no-delete CAS keeps the bytes reachable from the sealed closure. Nothing
//! here destroys user data; `erase` records what an erasure means and keeps
//! the recovery path explicit.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_store::research::{ArchiveMoveRecord, RetentionTombstone};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::ids::*;
use shenko_types::object::*;
use shenko_types::operation::OperationOutcome;
use shenko_types::task::{ImplementationRef, TaskRecord};
use std::sync::Arc;

use crate::research_task::{record_outcome, run_recorded};

const RETENTION_CONTRACT: &[u8] = b"shenko.host.retention-apply/v1: one bound idempotent retention mechanism over the sealed object set of the subject's own history; the closure is recorded; the content store keeps the bytes recoverable; no user data is destroyed";

const MAX_REASON_BYTES: usize = 4096;
const MAX_TARGETS: usize = 64;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionInput {
    request_id: EntityId,
    mechanism: MechanismInput,
    target_refs: Vec<ObjectRef>,
    reason: String,
    /// Archive-move target, required only for mechanism=archive_move.
    target: Option<ArchiveTargetInput>,
    /// Manifest of the moved closure, required for archive_move.
    manifest_ref: Option<String>,
    /// Verification the caller performed on the archive target.
    verified: Option<ArchiveVerificationInput>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveTargetInput {
    provider: String,
    namespace: String,
    locator: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveVerificationInput {
    hash_verified: bool,
    readback_verified: bool,
    /// Whether the local copy was released. The content store has no object
    /// delete, so an honest move keeps the local copy unless an external
    /// releaser did the work; the note says which.
    local_released: Option<bool>,
    local_release_note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MechanismInput {
    SemanticForget,
    ArchiveMove,
    Erase,
    Status,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenRetention {
    schema_version: u32,
    request_id: EntityId,
    mechanism: MechanismInput,
    target_refs: Vec<ObjectRef>,
    reason: String,
    target: Option<ArchiveTargetInput>,
    manifest_ref: Option<String>,
    verified: Option<ArchiveVerificationInput>,
}

pub struct RetentionApplyHandler {
    store: Arc<Store>,
}

impl RetentionApplyHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "1d2e3f4a-5b6c-4d7e-8f9a-0b1c2d3e4f5a"
            .parse()
            .expect("fixed built-in retention plugin UUID")
    }

    fn freeze(&self, input: serde_json::Value) -> Result<FrozenRetention> {
        let input: RetentionInput = serde_json::from_value(input)?;
        ensure!(
            !input.target_refs.is_empty(),
            "retention request must name at least one target object"
        );
        ensure!(
            input.target_refs.len() <= MAX_TARGETS,
            "retention request binds more than {MAX_TARGETS} target objects"
        );
        ensure!(
            !input.reason.trim().is_empty(),
            "retention reason must not be empty"
        );
        ensure!(
            input.reason.len() <= MAX_REASON_BYTES,
            "retention reason exceeds {MAX_REASON_BYTES} bytes"
        );
        if matches!(input.mechanism, MechanismInput::ArchiveMove) {
            let target = input
                .target
                .as_ref()
                .context("archive_move requires a target")?;
            ensure!(
                !target.provider.trim().is_empty()
                    && !target.namespace.trim().is_empty()
                    && !target.locator.trim().is_empty(),
                "archive_move target fields must be non-empty"
            );
            ensure!(
                input.manifest_ref.as_deref().is_some_and(|m| !m.trim().is_empty()),
                "archive_move requires a manifest_ref of the moved closure"
            );
        }
        if input.target.is_some() && !matches!(input.mechanism, MechanismInput::ArchiveMove) {
            anyhow::bail!("target is only accepted for archive_move");
        }
        Ok(FrozenRetention {
            schema_version: 1,
            request_id: input.request_id,
            mechanism: input.mechanism,
            target_refs: input.target_refs,
            reason: input.reason,
            target: input.target,
            manifest_ref: input.manifest_ref,
            verified: input.verified,
        })
    }

    fn closure_targets(frozen: &FrozenRetention) -> Vec<String> {
        frozen
            .target_refs
            .iter()
            .map(|object| object.digest.to_string())
            .collect()
    }

    fn write(&self, frozen: &FrozenRetention) -> Result<(serde_json::Value, OperationOutcome)> {
        match frozen.mechanism {
            MechanismInput::SemanticForget | MechanismInput::Erase => {
                let mechanism = match frozen.mechanism {
                    MechanismInput::SemanticForget => "semantic_forget",
                    _ => "erase",
                };
                let targets = Self::closure_targets(frozen);
                let closure = serde_json::json!({
                    "mechanism": mechanism,
                    "target_digests": targets,
                    "bytes_recoverable": true,
                    "recovery": "the content store has no object delete; every byte remains reachable at its digest",
                    "note": if mechanism == "erase" {
                        "erase records the erasure intent and its recovery path; it does not destroy bytes"
                    } else {
                        "the fact is dropped from future projections; the bytes stay in the store"
                    },
                });
                let tombstone = RetentionTombstone {
                    tombstone_id: frozen.request_id,
                    target_kind: String::from("object"),
                    target_id: targets.join(","),
                    scope: String::from("own_subject_history"),
                    reason: frozen.reason.clone(),
                    effect_record: serde_json::to_string(&closure)?,
                    deleted_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                self.store.record_tombstone(&tombstone)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": if mechanism == "erase" { "erased" } else { "forgotten" },
                        "tombstone_id": tombstone.tombstone_id.to_string(),
                        "closure": closure,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            MechanismInput::ArchiveMove => {
                let target = frozen
                    .target
                    .as_ref()
                    .context("archive_move requires a target")?;
                let verified = frozen.verified.clone().unwrap_or(ArchiveVerificationInput {
                    hash_verified: false,
                    readback_verified: false,
                    local_released: None,
                    local_release_note: None,
                });
                let total_bytes: u64 = frozen.target_refs.iter().map(|object| object.bytes).sum();
                let record = ArchiveMoveRecord {
                    archive_id: frozen.request_id,
                    source_ref: serde_json::to_string(&Self::closure_targets(frozen))?,
                    target_provider: target.provider.clone(),
                    target_namespace: target.namespace.clone(),
                    target_locator: target.locator.clone(),
                    manifest_ref: frozen
                        .manifest_ref
                        .clone()
                        .context("archive_move requires a manifest_ref")?,
                    object_count: frozen.target_refs.len() as u64,
                    total_bytes,
                    hash_verified: verified.hash_verified,
                    readback_verified: verified.readback_verified,
                    local_released: verified.local_released.unwrap_or(false),
                    local_release_note: verified.local_release_note.clone().or_else(|| {
                        if verified.local_released.unwrap_or(false) {
                            None
                        } else {
                            Some(String::from(
                                "local copy kept: the content store has no object delete",
                            ))
                        }
                    }),
                    created_at_ms: chrono::Utc::now().timestamp_millis(),
                };
                self.store.record_archive_move(&record)?;
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "archived",
                        "archive_id": record.archive_id.to_string(),
                        "object_count": record.object_count,
                        "total_bytes": record.total_bytes,
                        "hash_verified": record.hash_verified,
                        "readback_verified": record.readback_verified,
                        "local_released": record.local_released,
                        "local_release_note": record.local_release_note,
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
            MechanismInput::Status => {
                let digests = Self::closure_targets(frozen);
                let tombstones: Vec<_> = self
                    .store
                    .retention_tombstones(Some("object"))?
                    .into_iter()
                    .filter(|tombstone| {
                        digests.iter().any(|digest| {
                            tombstone
                                .target_id
                                .split(',')
                                .any(|part| part.trim() == digest.as_str())
                        })
                    })
                    .collect();
                let archives: Vec<_> = self
                    .store
                    .archive_moves()?
                    .into_iter()
                    .filter(|record| record.source_ref.contains(&digests[0]))
                    .collect();
                Ok((
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "reported",
                        "tombstones": tombstones
                            .iter()
                            .map(|tombstone| serde_json::json!({
                                "tombstone_id": tombstone.tombstone_id.to_string(),
                                "reason": tombstone.reason,
                                "deleted_at_ms": tombstone.deleted_at_ms,
                            }))
                            .collect::<Vec<_>>(),
                        "archive_moves": archives
                            .iter()
                            .map(|record| serde_json::json!({
                                "archive_id": record.archive_id.to_string(),
                                "target_provider": record.target_provider,
                                "hash_verified": record.hash_verified,
                                "readback_verified": record.readback_verified,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    OperationOutcome::Succeeded,
                ))
            }
        }
    }
}

#[async_trait]
impl NativeTaskHandler for RetentionApplyHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "retention.apply".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(RETENTION_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-retention-apply/1".into(),
            authority_refs: vec!["store.retention.own_subject_objects/v1".into()],
            prompt_section: Some(
                "retention.apply/v1 对密封的对象集执行一种保留机制。[delegate:retention.apply:1] {\"request_id\":\"…\",\"mechanism\":\"semantic_forget|archive_move|erase|status\",\"target_refs\":[{…}],\"reason\":\"…\"}。semantic_forget 与 erase 记录墓碑：事实不再进入投影，但内容存储无对象删除，字节仍按摘要可恢复。archive_move 额外要求 target{provider,namespace,locator} 与 manifest_ref，并报告 hash/readback 校验与本地副本去向。status 只读报告对象的墓碑与归档。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let frozen = self.freeze(invocation.input)?;
        run_recorded(
            &self.store,
            &invocation,
            RETENTION_CONTRACT,
            Self::plugin_id(),
            "retention.apply.v1",
            "retention.apply.execute",
            "retention-apply-observation-v1",
            "retention_apply_own_subject_objects",
            &frozen,
            move |_store, frozen| self.write(frozen),
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id,
            "original_operation_contract_mismatch"
        );
        if operation.state == shenko_types::operation::OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != shenko_types::operation::OperationState::InFlight
            || attempt.dispatch_stage != shenko_types::operation::AttemptDispatchStage::Armed
        {
            return Ok(operation);
        }
        let frozen = read_frozen_retention(&self.store, &operation)?;
        let (result, outcome) = self.write(&frozen)?;
        record_outcome(
            &self.store,
            &operation,
            &attempt,
            "retention.apply.v1",
            &result,
            outcome,
            shenko_types::operation::EffectStatus::Complete,
        )
    }
}

fn read_frozen_retention(
    store: &Store,
    operation: &shenko_types::operation::OperationRecord,
) -> Result<FrozenRetention> {
    let object = &operation.final_input_ref;
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes
            && Digest::blake3(&bytes) == object.digest
        && operation.input_digest == object.digest,
        "retention_frozen_input_mismatch"
    );
    serde_json::from_slice(&bytes).context("retention frozen input decode")
}
