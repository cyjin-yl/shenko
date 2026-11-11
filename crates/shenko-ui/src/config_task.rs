//! `config.manage` — configuration through conversation (docs/specs/configuration-playground.md#spec-43-1, R133).
//!
//! The subject changes its own configuration the same way the panel does:
//! through the one [`ConfigService`], over the same durable change ledger, with
//! the same `(origin, logical key)` idempotency and the same separation of
//! saved desired revision from consumer adoption. Nothing here writes a second
//! configuration store and nothing here asks a human to approve a change:
//! `apply` commits the intent and records its receipt, and the consumers report
//! what they actually run.
//!
//! The actor is derived from the delegated native task, never from the model's
//! input. The origin identity is the operation, so a recovered attempt resolves
//! the original change instead of applying a second effect.

use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use shenko_store::Store;
use shenko_subject::{
    NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation,
};
use shenko_types::{
    commit::CommitRef,
    effect::{
        CheckpointContract, EffectContract, EffectContractBuilder, GuaranteeBasis,
        IdempotencyGuarantee, LifecycleContract, ResourceAccess,
    },
    evidence::{DecodeStatus, EvidenceReceipt},
    ids::*,
    object::*,
    operation::*,
    task::{ImplementationRef, TaskRecord},
};

use crate::config_service::{
    ConfigActor, ConfigApplyRequest, ConfigError, ConfigService,
};

const CONTRACT: &[u8] = b"shenko.config.manage/v1: discover, read, preview and apply the installation's registered configuration namespaces through the one ConfigService; actor derived from the delegated native task; no second config store, no approval gate";
const MAX_NORMALIZED_BYTES: usize = 480 * 1024;
const MAX_NAME_BYTES: usize = 128;

/// The runtime the model asks for. One operation per call.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ConfigOp {
    /// Every registered namespace, its keys and how its changes take effect.
    Discover,
    /// The saved desired value plus what consumers adopted and what they run.
    Read {
        namespace: String,
        logical_key: String,
    },
    /// Look at a change without saving it. Returns a stale base when the
    /// caller's expected revision no longer matches.
    Preview {
        namespace: String,
        logical_key: String,
        desired: Value,
        #[serde(default)]
        expected_base_revision: Option<u64>,
    },
    /// Record and apply the change. A repeated origin returns the original
    /// change instead of applying a second effect.
    Apply {
        namespace: String,
        logical_key: String,
        desired: Value,
        #[serde(default)]
        expected_base_revision: Option<u64>,
    },
    /// Reconcile configuration intents whose apply lost its process. Nothing is
    /// re-applied: the namespace's handler is asked only what it runs now, and
    /// the answer is recorded as an observation with an `unknown` outcome.
    Recover {
        #[serde(default)]
        namespace: Option<String>,
        #[serde(default)]
        logical_key: Option<String>,
    },
}

/// The frozen input of one operation. The config service, the namespace
/// handlers and the consumers all read from CAS-sealed facts, so the frozen
/// value is identity, not parameters a later write could change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenConfigInput {
    schema_version: u32,
    agent_id: EntityId,
    /// The delegated native task that owns the change record.
    task_id: EntityId,
    /// The idempotency identity: this operation. A recovered attempt keeps it.
    origin_identity: String,
    /// CAS-sealed `AuthorityRef` of the delegated task.
    delegation_ref: ObjectRef,
    op: ConfigOp,
}

/// One observation of a configuration operation.
struct Observation {
    result: Value,
    outcome: OperationOutcome,
    effect: EffectStatus,
}

pub struct ConfigHandler {
    store: Arc<Store>,
    service: Arc<ConfigService>,
}

impl ConfigHandler {
    pub fn new(store: Arc<Store>, service: Arc<ConfigService>) -> Self {
        Self { store, service }
    }

    fn plugin_id() -> EntityId {
        "9f2c1d74-3a51-4a5f-9a11-6b7c0e5d21c8"
            .parse()
            .expect("fixed config plugin UUID")
    }

    fn seal_bytes(&self, bytes: &[u8], schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            bytes,
            "application/json",
            "native-config",
            schema,
            1,
        ) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.store.close_external_writer();
                Err(error.into())
            }
        }
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        self.seal_bytes(&serde_json::to_vec(value)?, schema)
    }

    fn freeze(&self, input: Value, invocation: &NativeTaskInvocation) -> Result<FrozenConfigInput> {
        let op: ConfigOp = serde_json::from_value(input).context("config_manage_input_invalid")?;
        validate_op(&op)?;
        let delegation_ref = self.seal(&invocation.authority, "config-delegation")?;
        Ok(FrozenConfigInput {
            schema_version: 1,
            agent_id: invocation.task.agent_id,
            task_id: invocation.task.task_id,
            origin_identity: format!("config-operation/{}", invocation.operation_id),
            delegation_ref,
            op,
        })
    }

    fn frozen(&self, operation: &OperationRecord) -> Result<FrozenConfigInput> {
        let object = &operation.final_input_ref;
        let bytes = self.store.cas.read_object(&object.digest)?;
        ensure!(
            bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest
                && operation.input_digest == object.digest,
            "config_frozen_input_mismatch"
        );
        let frozen: FrozenConfigInput = serde_json::from_slice(&bytes)?;
        ensure!(frozen.schema_version == 1, "unsupported_config_input_schema");
        validate_op(&frozen.op)?;
        Ok(frozen)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let bindings = ResourceBindings {
            provider: "store.config_entries.write/delegated_subject/v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "delegated_subject_configuration".into(),
            extra: json!({"agent_id": invocation.task.agent_id}),
        };
        let resource = self.seal(&bindings, "config-binding")?;
        let acceptance = self.seal(
            &"the registered namespace's own apply contract ran, and its consumers report the revision they run; a saved desired value without an adoption is reported as such",
            "config-acceptance",
        )?;
        let retry = self.seal(
            &"resolve the original change by (origin, logical key); never apply a second effect and never reuse the key for different desired bytes",
            "config-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:config-manage-v1".into(),
            },
            symbol: "config.manage.execute".into(),
            schema: SchemaRef {
                name: "config-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "config.manage.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input_ref.digest.clone(),
            final_input_ref: input_ref,
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::None,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        Ok((operation, attempt))
    }

    /// Run one operation and describe what actually happened.
    fn observe(&self, frozen: &FrozenConfigInput) -> Observation {
        match &frozen.op {
            ConfigOp::Discover => Observation {
                result: json!({
                    "schema_version": 1,
                    "status": "ok",
                    "op": "discover",
                    "namespaces": self.service.discover(),
                }),
                outcome: OperationOutcome::Succeeded,
                effect: EffectStatus::None,
            },
            ConfigOp::Read { namespace, logical_key } => match self.service.read(namespace, logical_key) {
                Ok(view) => Observation {
                    result: json!({
                        "schema_version": 1,
                        "status": "ok",
                        "op": "read",
                        "entry": view,
                    }),
                    outcome: OperationOutcome::Succeeded,
                    effect: EffectStatus::None,
                },
                Err(error) => Observation {
                    result: rejected("read", &error),
                    outcome: OperationOutcome::Succeeded,
                    effect: EffectStatus::None,
                },
            },
            ConfigOp::Preview {
                namespace,
                logical_key,
                desired,
                expected_base_revision,
            } => match self.service.preview(
                namespace,
                logical_key,
                desired.clone(),
                *expected_base_revision,
            ) {
                Ok(preview) => Observation {
                    result: json!({
                        "schema_version": 1,
                        "status": if preview.stale { "stale_base" } else { "ok" },
                        "op": "preview",
                        "preview": preview,
                        "note": "a preview seals a candidate value; it is not a saved revision and no consumer adopted it",
                    }),
                    outcome: OperationOutcome::Succeeded,
                    effect: EffectStatus::None,
                },
                Err(error) => Observation {
                    result: rejected("preview", &error),
                    outcome: OperationOutcome::Succeeded,
                    effect: EffectStatus::None,
                },
            },
            ConfigOp::Apply {
                namespace,
                logical_key,
                desired,
                expected_base_revision,
            } => {
                let request = ConfigApplyRequest {
                    namespace: namespace.clone(),
                    logical_key: logical_key.clone(),
                    origin_identity: frozen.origin_identity.clone(),
                    actor: ConfigActor::Subject {
                        task_id: frozen.task_id,
                        delegation_ref: Some(frozen.delegation_ref.clone()),
                    },
                    desired: desired.clone(),
                    expected_base_revision: *expected_base_revision,
                    coordinating_task_id: Some(frozen.task_id),
                };
                match self.service.apply(request) {
                    Ok(applied) => {
                        let state = applied.change.state.clone();
                        let adopted = applied
                            .change
                            .adopted
                            .iter()
                            .map(|adoption| adoption.consumer_id.clone())
                            .collect::<Vec<_>>();
                        let applied_cleanly = state == "applied";
                        Observation {
                            result: json!({
                                "schema_version": 1,
                                "status": if !applied_cleanly {
                                    "recorded"
                                } else if applied.replayed {
                                    "replayed"
                                } else {
                                    "applied"
                                },
                                "op": "apply",
                                "replayed": applied.replayed,
                                "change": applied.change,
                                "adopted_consumers": adopted,
                                "next": if applied_cleanly {
                                    "consumers adopted the revision; read the key again to observe it"
                                } else {
                                    "the desired revision is saved but no apply receipt exists: an interrupted (or still-owned) attempt. Read the key to reconcile it from the consumer's own report. Do not present the value as applied, and do not repeat the same intent to force it; a new decision needs a new intent after reading the current facts"
                                },
                            }),
                            outcome: if applied_cleanly {
                                OperationOutcome::Succeeded
                            } else {
                                OperationOutcome::Failed
                            },
                            effect: if applied_cleanly {
                                EffectStatus::Complete
                            } else {
                                EffectStatus::Partial
                            },
                        }
                    }
                    Err(error) => Observation {
                        result: rejected("apply", &error),
                        outcome: OperationOutcome::Succeeded,
                        effect: EffectStatus::None,
                    },
                }
            }
            ConfigOp::Recover {
                namespace,
                logical_key,
            } => {
                let report = match (namespace, logical_key) {
                    (Some(namespace), Some(logical_key)) => self
                        .service
                        .settle_interrupted(namespace, logical_key)
                        .map(|settled| match settled {
                            Some(change) => json!({
                                "schema_version": 1,
                                "status": "settled",
                                "change": change,
                                "note": "the interrupted intent now has a receipt. Its outcome is unknown unless the consumer's report proved otherwise; no handler was re-run and no adoption was invented",
                            }),
                            None => json!({
                                "schema_version": 1,
                                "status": "nothing_to_recover",
                                "namespace": namespace,
                                "logical_key": logical_key,
                                "note": "no committed change of this key is waiting for a receipt, or its attempt is still owned by a live process",
                            }),
                        }),
                    (None, None) => self.service.recover_interrupted().map(|report| {
                        json!({
                            "schema_version": 1,
                            "status": "recovered",
                            "report": report,
                        })
                    }),
                    _ => Err(ConfigError::Denied(
                        "recover needs both the namespace and the logical key, or neither".into(),
                    )),
                };
                match report {
                    Ok(result) => Observation {
                        result,
                        outcome: OperationOutcome::Succeeded,
                        effect: EffectStatus::None,
                    },
                    Err(error) => Observation {
                        result: rejected("recover", &error),
                        outcome: OperationOutcome::Succeeded,
                        effect: EffectStatus::None,
                    },
                }
            }
        }
    }

    fn record(
        &self,
        operation: &OperationRecord,
        attempt: &AttemptRecord,
        observation: Observation,
    ) -> Result<OperationRecord> {
        let bytes = serde_json::to_vec(&observation.result)?;
        let raw = self.seal_bytes(&bytes, "config-observation")?;
        let (normalized, outcome) = if bytes.len() <= MAX_NORMALIZED_BYTES {
            (raw.clone(), observation.outcome)
        } else {
            let oversized = json!({
                "schema_version": 1,
                "status": "failed",
                "error": {
                    "code": "normalized_result_too_large",
                    "message": "the exact observation is sealed in raw evidence; increase the bounded native result contract before delivery"
                }
            });
            (self.seal(&oversized, "config-result")?, OperationOutcome::Failed)
        };
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "config.manage.v1".into(),
            source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some(format!("config-manage:{}", attempt.attempt_id)),
            producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: None,
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            operation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(outcome),
            observation.effect,
        )
    }

    async fn run(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "config.manage" && invocation.task.definition_version == 1,
            "config_task_identity_mismatch"
        );
        let agent_id = invocation.task.agent_id;
        let (head, _) = self.store.load_subject(agent_id)?;
        let step_id = invocation
            .task
            .created_by_step_id
            .context("config_task_has_no_subject_step")?;
        self.store
            .get_step(&head.branch_id, step_id)?
            .context("config_task_origin_is_outside_current_subject_branch")?;
        let frozen = self.freeze(std::mem::take(&mut invocation.input), &invocation)?;
        let input_ref = self.seal(&frozen, "config-frozen-input")?;
        let (mut operation, mut attempt) = self.operation_parts(&invocation, input_ref)?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        let observation = self.observe(&frozen);
        self.record(&operation, &attempt, observation)
    }
}

fn rejected(op: &str, error: &ConfigError) -> Value {
    json!({
        "schema_version": 1,
        "status": "rejected",
        "op": op,
        "code": error.code(),
        "error": error.to_string(),
        "note": "the request was refused by the configuration service; no effect was applied",
    })
}

fn validate_op(op: &ConfigOp) -> Result<()> {
    match op {
        ConfigOp::Discover => Ok(()),
        ConfigOp::Read { namespace, logical_key } => {
            validate_name("namespace", namespace)?;
            validate_name("logical_key", logical_key)
        }
        ConfigOp::Preview {
            namespace,
            logical_key,
            ..
        }
        | ConfigOp::Apply {
            namespace,
            logical_key,
            ..
        } => {
            validate_name("namespace", namespace)?;
            validate_name("logical_key", logical_key)
        }
        ConfigOp::Recover {
            namespace,
            logical_key,
        } => match (namespace, logical_key) {
            (Some(namespace), Some(logical_key)) => {
                validate_name("namespace", namespace)?;
                validate_name("logical_key", logical_key)
            }
            (None, None) => Ok(()),
            _ => bail!("recover needs both the namespace and the logical key, or neither"),
        },
    }
}

fn validate_name(field: &str, value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= MAX_NAME_BYTES,
        "{field} must be 1..={MAX_NAME_BYTES} bytes"
    );
    Ok(())
}

#[async_trait]
impl NativeTaskHandler for ConfigHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "config.manage".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-config-manager/1".into(),
            authority_refs: vec!["store.config_entries.write/delegated_subject/v1".into()],
            prompt_section: Some(
                "config.manage/v1 管理安装自身的配置。{\"op\":\"discover\"} 列出已注册命名空间及其键；{\"op\":\"read\",\"namespace\":\"…\",\"logical_key\":\"…\"} 读保存的 desired、各消费者已采用的 revision、当前实际运行值，以及该键是否有被中断的 apply；{\"op\":\"preview\",\"namespace\":\"…\",\"logical_key\":\"…\",\"desired\":…} 只看候选，不保存；{\"op\":\"apply\",…,\"desired\":…,\"expected_base_revision\":N} 提交持久修改并取得回执与 adoption；{\"op\":\"recover\",…} 只对账被中断的 apply（可带 namespace 与 logical_key，或都不带），它不重新执行命名空间的 apply 合同。保存值、消费者采用值和健康是三个不同事实：apply 返回 applied 表示消费者已采用该 revision，不表示服务一定健康。同一意图重复提交返回原回执（replayed）。expected_base_revision 不匹配返回 BASE_CONFIG_STALE，应先重新读取再决定新值；不得擅自把 stale 当成覆盖许可。若返回 saved 但无回执，说明那次 apply 的进程已经消失而效果未知：先 read 或 recover 取得对账结果，消费者现在报告的值只是现状，不证明是那次修改造成的；不要靠重复同一意图去逼出结果，需要改变时用新的意图。对账结果 unknown 不填默认置信，也不冒充已生效。普通配置改动不需要人类审批；管理员身份与凭据不能由本工具设置。"
                    .into(),
            ),
        }
    }

    /// docs/specs/durable-state.md#spec-36 effect contract of `config.manage.v1`.
    ///
    /// The only write is the installation's own configuration ledger, which
    /// replicates with cognition facts. Idempotency is strict for the origin
    /// window because the change identity is derived from the operation and
    /// the exact desired bytes: a recovered attempt resolves the original
    /// change, and a different payload under the same key is refused.
    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        vec![
            EffectContractBuilder::new(
                descriptor.implementation_ref.plugin_id,
                descriptor.implementation(),
                "config.manage.v1",
                descriptor.definition_version,
            )
            .resource("store_config_entries", "installation", ResourceAccess::ReadWrite)
            .idempotency(IdempotencyGuarantee::Strict {
                key_namespace: "config-operation".into(),
                max_age_secs: 30 * 24 * 60 * 60,
            })
            .checkpoint(CheckpointContract {
                capturable: vec!["desired revision and its change record".into()],
                excluded: vec!["a consumer's in-memory value after a restart".into()],
                coordinated: false,
                consistency: "the desired revision and its change record commit together".into(),
            })
            .lifecycle(LifecycleContract {
                on_upgrade: "the definition is replaced; committed changes and receipts stay readable".into(),
                on_uninstall: "the definition is withdrawn; saved values stay durable and consumers keep their adopted revision".into(),
                on_crash: "a change commits together with the process that owns its apply attempt; once that owner is provably gone the change is reconciled from the consumer's own report into an unknown receipt (observed_equal / observed_different / unreadable / handler_missing), never by re-running the apply contract".into(),
                receipt_readable_after_removal: true,
            })
            .basis(GuaranteeBasis::ClaimOnly)
            .build(),
        ]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        self.run(invocation).await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == "config.manage.v1",
            "original_operation_contract_mismatch"
        );
        if operation.state == OperationState::Resolved {
            return Ok(operation);
        }
        let attempt = self.store.external_operation_attempt(operation_id)?;
        if operation.state != OperationState::InFlight
            || attempt.dispatch_stage != AttemptDispatchStage::Armed
        {
            // Existing unknown/conflict evidence is authoritative. A later
            // successful observation must not erase it or arm again.
            return Ok(operation);
        }
        let frozen = self.frozen(&operation)?;
        let observation = self.observe(&frozen);
        self.record(&operation, &attempt, observation)
    }
}
