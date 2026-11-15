//! Configured document.parse native task, using the existing T1/E1/T2 ledger.
//! The document crate owns the replaceable provider protocol. This composition
//! adapter owns CAS receipts; it adds no task table, scheduler or plugin path.

use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use shenko_document::{
    CredentialHandle, DocumentProvider, OxrAttemptBinding, OxrAttemptClaim, OxrAttemptContext,
    OxrAttemptReceipt, OxrAttemptStore, OxrClient, OxrClientConfig, OxrEngineBinding, OxrProvider,
    OxrProviderConfig, OxrTransport, ParseOptions, ParseRequest, ParseRun, ParseState, run_parse,
};
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_store::{Store, external::ExternalEvidenceFold};
use shenko_types::{
    commit::CommitRef,
    effect::{
        CancelContract, CheckpointContract, EffectActionKind, EffectContract,
        EffectContractBuilder, ExternalEffect, GuaranteeBasis, IdempotencyGuarantee,
        ResourceAccess,
    },
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    plugin_runtime::NativeCapabilityDeclaration,
    task::{ImplementationRef, TaskInvocation, TaskRecord},
};
use std::sync::Arc;
use std::time::Duration;

const CONTRACT: &[u8] = b"shenko.document.parse/v1: one original armed OXR upload; immutable accepted-task and page receipts; unknown reconciles without upload replay";
const OPERATION: &str = "document.parse.v1";
const TRANSPORT_RECEIPT: &str = "document.oxr.transport.v1";
const RESULT_RECEIPT: &str = "document.parse.result.v1";
const MAX_SOURCE_BYTES: u64 = 200 * 1024 * 1024;
const MAX_RESULT_BYTES: usize = 480 * 1024;
const MAX_FOLD_RETRIES: usize = 16;

/// No endpoint means no registration. A configured deployment must name its
/// actual model, non-secret account identity and credential; there is no
/// implicit public service, model fallback, or engine contact at startup.
pub fn configured_handler(store: Arc<Store>) -> Result<Option<Arc<dyn NativeTaskHandler>>> {
    let Some(endpoint) = optional_env("SHENKO_OXR_ENDPOINT") else {
        return Ok(None);
    };
    let required = |name| {
        optional_env(name)
            .with_context(|| format!("{name} is required when SHENKO_OXR_ENDPOINT is set"))
    };
    let account = required("SHENKO_OXR_ACCOUNT")?;
    let credential = CredentialHandle::new(required("SHENKO_OXR_API_KEY")?);
    let endpoint = OxrClient::new(OxrClientConfig::new(endpoint, credential.clone()))?
        .endpoint()
        .to_owned();
    let transport = match optional_env("SHENKO_OXR_TRANSPORT").as_deref() {
        None | Some("async") => OxrTransport::Async,
        Some("sync") => OxrTransport::Sync,
        Some(_) => anyhow::bail!("SHENKO_OXR_TRANSPORT must be async or sync"),
    };
    let engine = match optional_env("SHENKO_OXR_ENGINE").as_deref() {
        None | Some("remote") => OxrEngineBinding::Remote,
        Some("local") => OxrEngineBinding::Local {
            attestation: optional_env("SHENKO_OXR_ENGINE_ATTESTATION"),
        },
        Some(_) => anyhow::bail!("SHENKO_OXR_ENGINE must be remote or local"),
    };
    let config = OxrProviderConfig {
        endpoint: Some(endpoint),
        model_identity: Some(required("SHENKO_OXR_MODEL")?),
        scope: optional_env("SHENKO_OXR_SCOPE"),
        credential: Some(credential),
        engine,
        transport,
        request_timeout: duration_env("SHENKO_OXR_REQUEST_TIMEOUT_MS")?,
        poll_timeout: duration_env("SHENKO_OXR_POLL_TIMEOUT_MS")?,
        poll_interval: duration_env("SHENKO_OXR_POLL_INTERVAL_MS")?,
        attempt: None,
    };
    Ok(Some(Arc::new(DocumentHandler {
        store,
        config,
        account,
    })))
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn duration_env(name: &str) -> Result<Option<Duration>> {
    optional_env(name)
        .map(|value| {
            let milliseconds: u64 = value
                .parse()
                .with_context(|| format!("{name} must be an integer in milliseconds"))?;
            ensure!(
                (1..=86_400_000).contains(&milliseconds),
                "{name} must be between 1 millisecond and 24 hours"
            );
            Ok(Duration::from_millis(milliseconds))
        })
        .transpose()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentInput {
    source_ref: ObjectRef,
    request: ParseRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrozenInput {
    schema_version: u32,
    input: DocumentInput,
    provider: serde_json::Value,
}

struct DocumentHandler {
    store: Arc<Store>,
    config: OxrProviderConfig,
    account: String,
}

fn seal<T: Serialize>(store: &Store, value: &T, schema: &str) -> Result<ObjectRef> {
    let bytes = serde_json::to_vec(value)?;
    match store
        .cas
        .seal_object(&bytes, "application/json", "native-document", schema, 1)
    {
        Ok(sealed) => Ok(sealed.object),
        Err(error) => {
            store.close_external_writer();
            Err(error.into())
        }
    }
}

fn read<T: for<'de> Deserialize<'de>>(store: &Store, object: &ObjectRef) -> Result<T> {
    let bytes = store.cas.read_object(&object.digest)?;
    ensure!(
        bytes.len() as u64 == object.bytes && Digest::blake3(&bytes) == object.digest,
        "document CAS identity mismatch"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

impl DocumentHandler {
    fn plugin_id() -> EntityId {
        EntityId::derived("native-document-plugin/v1", CONTRACT)
    }

    fn provider_binding(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": self.config.endpoint, "model": self.config.model_identity,
            "account": self.account, "scope": self.config.scope,
            "transport": format!("{:?}", self.config.transport),
            "engine": format!("{:?}", self.config.engine),
            "request_timeout": self.config.request_timeout.unwrap_or(shenko_document::DEFAULT_REQUEST_TIMEOUT),
            "poll_timeout": self.config.poll_timeout.unwrap_or(shenko_document::DEFAULT_POLL_TIMEOUT),
            "poll_interval": self.config.poll_interval.unwrap_or(shenko_document::DEFAULT_POLL_INTERVAL),
        })
    }

    fn source(&self, input: &DocumentInput) -> Result<Vec<u8>> {
        ensure!(
            input.source_ref.bytes <= MAX_SOURCE_BYTES,
            "document source exceeds 200 MiB"
        );
        ensure!(
            input.source_ref.digest.0 == input.request.source_digest,
            "document source digest differs from request"
        );
        let bytes = self.store.cas.read_object(&input.source_ref.digest)?;
        ensure!(
            bytes.len() as u64 == input.source_ref.bytes
                && Digest::blake3(&bytes) == input.source_ref.digest,
            "document source CAS identity mismatch"
        );
        Ok(bytes)
    }

    fn operation_parts(
        &self,
        invocation: &NativeTaskInvocation,
        input: ObjectRef,
    ) -> Result<(OperationRecord, AttemptRecord)> {
        let descriptor = self.descriptor();
        let version = descriptor.implementation();
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let resource = seal(
            &self.store,
            &ResourceBindings {
                provider: "oxr/1.0".into(),
                account_identity: self.account.clone(),
                runtime_node: invocation.task_invocation.node_id.to_string(),
                mount_generation: 0,
                capability_level: "document_upload_and_task_query".into(),
                extra: self.provider_binding(),
            },
            "document-resource-binding",
        )?;
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: OPERATION.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            input_digest: input.digest.clone(),
            final_input_ref: input,
            contract_ref: OperationContractRef {
                plugin_id: descriptor.implementation_ref.plugin_id,
                artifact_digest: descriptor.implementation_ref.artifact_digest,
                contract_digest: Digest::blake3(CONTRACT),
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority.clone(),
            acceptance_predicate_ref: seal(
                &self.store,
                &"valid mapped requested pages or explicit failure/unavailable; unknown is never success",
                "document-acceptance",
            )?,
            retry_contract_ref: seal(
                &self.store,
                &"one original upload; accepted task may only be queried; an unknown upload may not be replayed",
                "document-retry",
            )?,
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
            // The operation identity is assigned by the native runtime. Its
            // sole transport attempt remains the same on repeated execute.
            attempt_id: EntityId::derived(
                "document.parse/attempt/v1",
                invocation.operation_id.to_string().as_bytes(),
            ),
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

    fn provider(&self, bridge: Arc<StoreReceipts>) -> OxrProvider {
        let mut config = self.config.clone();
        config.attempt = Some(OxrAttemptContext {
            operation_id: bridge.operation.operation_id,
            attempt_id: bridge.attempt.attempt_id,
            account_identity: self.account.clone(),
            store: bridge,
        });
        OxrProvider::new(config)
    }

    async fn parse(
        &self,
        input: &DocumentInput,
        source: &[u8],
        provider: OxrProvider,
        bridge: &StoreReceipts,
    ) -> Result<OperationRecord> {
        let binding = provider.attempt_binding(source, &input.request)?;
        if self
            .store
            .find_external_operation(binding.operation_id)?
            .is_some()
        {
            // A replay must preserve the whole native request, including the
            // requested pages. The transport binding deliberately omits page
            // selection because one upload can supply several page callers.
            bridge.validate(&binding)?;
        }
        let provider: Arc<dyn DocumentProvider> = Arc::new(provider);
        let run = run_parse(&input.request, &provider, source, ParseOptions::default()).await?;
        self.finish(bridge, &binding, &run)
    }

    fn finish(
        &self,
        bridge: &StoreReceipts,
        binding: &OxrAttemptBinding,
        run: &ParseRun,
    ) -> Result<OperationRecord> {
        let (operation, never_dispatched) = match self
            .store
            .find_external_operation(bridge.operation.operation_id)?
        {
            Some(_) => (bridge.validate(binding)?, false),
            None => {
                // Preflight did not authorize an upload. Record an explicit
                // unavailable result through ordinary T1/E1, with no dispatch
                // marker. A racing caller may already have admitted a send;
                // only this call's fresh no-send admission proves no effect.
                ensure!(
                    run.state == ParseState::ProviderUnavailable,
                    "document parsing ended before transport admission"
                );
                let never_dispatched = bridge.arm(binding, false)?;
                (bridge.validate(binding)?, never_dispatched)
            }
        };
        // A concurrent late observation must never turn a prior terminal
        // result back into unknown or bypass a conflict recorded by Store.
        if operation.state == OperationState::Resolved || operation.outcome_conflict {
            return Ok(operation);
        }
        let checkpoints = run
            .checkpoints
            .iter()
            .map(|checkpoint| {
                Ok(serde_json::json!({ "cache_key": checkpoint.cache_key,
                "checkpoint_ref": seal(&self.store, checkpoint, "document-page-checkpoint")? }))
            })
            .collect::<Result<Vec<_>>>()?;
        let manifest = seal(
            &self.store,
            &serde_json::json!({ "schema_version": 1,
                "operation_id": operation.operation_id, "attempt_id": bridge.attempt.attempt_id,
                "state": run.state, "pages": checkpoints,
            }),
            "document-checkpoint-manifest",
        )?;
        let unknown = run.state == ParseState::Running
            || run.state == ParseState::PageCountUnknown
            || run.document_unknown_outcome.is_some();
        let failed = run.document_failure.is_some()
            || run.checkpoints.iter().any(|page| page.failure.is_some());
        let (status, outcome, effect) = if unknown {
            ("unknown", None, EffectStatus::Unknown)
        } else if run.state == ParseState::ProviderUnavailable {
            // No fabricated engine completion. If an earlier transport receipt
            // exists, current unavailability cannot erase its unknown effect.
            if !never_dispatched {
                ("unavailable", None, EffectStatus::Unknown)
            } else {
                (
                    "unavailable",
                    Some(OperationOutcome::Failed),
                    EffectStatus::None,
                )
            }
        } else if run.state == ParseState::Cancelled {
            (
                "cancelled",
                Some(OperationOutcome::Cancelled),
                EffectStatus::Unknown,
            )
        } else if failed {
            (
                "failed",
                Some(OperationOutcome::Failed),
                EffectStatus::Complete,
            )
        } else {
            (
                "succeeded",
                Some(OperationOutcome::Succeeded),
                EffectStatus::Complete,
            )
        };
        let raw = seal(&self.store, run, "document-parse-run")?;
        let full = serde_json::json!({ "schema_version": 1, "status": status,
            "run": run, "checkpoint_manifest_ref": manifest });
        let bytes = serde_json::to_vec(&full)?;
        let normalized = if bytes.len() <= MAX_RESULT_BYTES {
            seal(&self.store, &full, "document-parse-result")?
        } else {
            seal(
                &self.store,
                &serde_json::json!({ "schema_version": 1, "status": status,
                "page_count": run.checkpoints.len(), "result_ref": raw,
                "checkpoint_manifest_ref": manifest, "inline_result_truncated": true,
                "document_failure": run.document_failure, "document_unknown_outcome": run.document_unknown_outcome,
                "unavailable_reason": run.unavailable_reason }),
                "document-parse-result",
            )?
        };
        let evidence = bridge.evidence(RESULT_RECEIPT, raw, Some(normalized), None);
        for _ in 0..MAX_FOLD_RETRIES {
            let current = bridge.validate(binding)?;
            if current.state == OperationState::Resolved || current.outcome_conflict {
                return Ok(current);
            }
            // Read/fold all original transport receipts at this head. The CAS
            // below prevents a concurrently appended contradiction from being
            // overwritten by this precomputed ParseRun settlement.
            bridge.replay(binding)?;
            if let Some(committed) = self.store.record_external_evidence_if_current(
                operation.operation_id,
                bridge.attempt.attempt_id,
                evidence.clone(),
                outcome,
                effect,
                ExternalEvidenceFold {
                    expected_revision: current.record_revision,
                    terminal_content_conflict: false,
                },
            )? {
                return Ok(committed);
            }
        }
        anyhow::bail!("document evidence head kept changing; original attempt remains reconcilable")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TransportObservation {
    schema_version: u32,
    binding_digest: String,
    receipt: OxrAttemptReceipt,
}

/// A receipt view over the existing operation. Only an execute invocation
/// may attempt atomic T1+dispatch; reconstruction has no invocation and can
/// only read or append facts about an already armed attempt.
struct StoreReceipts {
    store: Arc<Store>,
    operation: OperationRecord,
    attempt: AttemptRecord,
    invocation: Option<TaskInvocation>,
}

impl std::fmt::Debug for StoreReceipts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreReceipts")
            .field("operation_id", &self.operation.operation_id)
            .finish_non_exhaustive()
    }
}

impl StoreReceipts {
    fn new(store: Arc<Store>, operation: OperationRecord, attempt: AttemptRecord) -> Self {
        Self {
            store,
            operation,
            attempt,
            invocation: None,
        }
    }

    fn arm(&self, binding: &OxrAttemptBinding, dispatch: bool) -> Result<bool> {
        let invocation = self
            .invocation
            .as_ref()
            .context("reconciliation cannot arm a document attempt")?;
        let mut operation = self.operation.clone();
        let mut attempt = self.attempt.clone();
        operation.checkpoint_scope_ref =
            Some(seal(&self.store, binding, "document-attempt-binding")?);
        let existing = if dispatch {
            self.store.arm_native_operation_for_dispatch(
                invocation,
                &mut operation,
                &mut attempt,
            )?
        } else {
            self.store
                .arm_external_operation(invocation, &mut operation, &mut attempt)?
        };
        self.validate(binding)?;
        Ok(existing.is_none())
    }

    fn validate(&self, binding: &OxrAttemptBinding) -> Result<OperationRecord> {
        ensure!(
            binding.operation_id == self.operation.operation_id
                && binding.attempt_id == self.attempt.attempt_id,
            "document original operation/attempt changed"
        );
        let operation = self.store.external_operation(binding.operation_id)?;
        let expected = &self.operation;
        // Match ordinary T1's frozen-input/contract replay checks, without
        // admitting anything or requiring the old invocation to remain live.
        ensure!(
            operation.operation_name == OPERATION
                && operation.task_id == expected.task_id
                && operation.input_digest == expected.input_digest
                && operation.resource_bindings_ref.digest == expected.resource_bindings_ref.digest
                && operation.contract_ref.contract_digest == expected.contract_ref.contract_digest
                && operation.authority_ref.delegation_id == expected.authority_ref.delegation_id
                && operation.authority_ref.delegation_revision
                    == expected.authority_ref.delegation_revision
                && operation.authority_ref.decision_ref == expected.authority_ref.decision_ref
                && operation.acceptance_predicate_ref.digest
                    == expected.acceptance_predicate_ref.digest
                && operation.retry_contract_ref.digest == expected.retry_contract_ref.digest,
            "document original frozen input or contract changed"
        );
        let reference = operation
            .checkpoint_scope_ref
            .as_ref()
            .context("document attempt checkpoint missing")?;
        let original: OxrAttemptBinding = read(&self.store, reference)?;
        ensure!(
            &original == binding,
            "document original input/provider/account/scope binding changed"
        );
        Ok(operation)
    }

    fn replay(&self, binding: &OxrAttemptBinding) -> Result<Option<OxrAttemptReceipt>> {
        let mut result: Option<OxrAttemptReceipt> = None;
        for evidence in self
            .store
            .external_operation_evidence(binding.operation_id)?
        {
            if evidence.source_id != TRANSPORT_RECEIPT {
                continue;
            }
            ensure!(
                evidence.attempt_id == Some(binding.attempt_id),
                "document transport evidence belongs to another attempt"
            );
            let observed: TransportObservation = read(&self.store, &evidence.raw_ref)?;
            ensure!(
                observed.schema_version == 1 && observed.binding_digest == binding.digest()?,
                "document transport receipt binding mismatch"
            );
            let incoming = observed.receipt;
            if let Some(previous) = &result {
                if let (Some(old), Some(new)) = (&previous.task, &incoming.task) {
                    ensure!(old == new, "document attempt has conflicting task handles");
                }
                if terminal(previous) {
                    if terminal(&incoming) {
                        ensure!(
                            previous.outcome == incoming.outcome,
                            "document attempt has conflicting terminal receipts"
                        );
                    }
                    continue;
                }
            }
            let task = incoming
                .task
                .or_else(|| result.as_ref().and_then(|receipt| receipt.task.clone()));
            result = Some(OxrAttemptReceipt {
                task,
                outcome: incoming.outcome,
            });
        }
        Ok(result)
    }

    fn evidence(
        &self,
        source: &str,
        raw: ObjectRef,
        normalized: Option<ObjectRef>,
        level: Option<ObservationLevel>,
    ) -> EvidenceReceipt {
        EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(self.operation.operation_id),
            attempt_id: Some(self.attempt.attempt_id),
            source_id: source.into(),
            source_instance_id: self.attempt.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(self.attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(self.operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: DecodeStatus::Decoded,
            observation_level: level,
            normalized_ref: normalized,
            observed_at: Some(chrono::Utc::now()),
            received_commit: self.operation.prepared_commit,
        }
    }
}

fn terminal(receipt: &OxrAttemptReceipt) -> bool {
    match &receipt.outcome {
        Some(Ok(_)) => true,
        Some(Err(error)) => !error.outcome_is_unknown(),
        None => false,
    }
}

fn receipts_conflict(previous: &OxrAttemptReceipt, incoming: &OxrAttemptReceipt) -> bool {
    matches!((&previous.task, &incoming.task), (Some(old), Some(new)) if old != new)
        || (terminal(previous) && terminal(incoming) && previous.outcome != incoming.outcome)
}

impl OxrAttemptStore for StoreReceipts {
    fn receipt(&self, binding: &OxrAttemptBinding) -> Result<Option<OxrAttemptReceipt>> {
        if self
            .store
            .find_external_operation(binding.operation_id)?
            .is_none()
        {
            ensure!(
                self.invocation.is_some(),
                "original document operation is missing"
            );
            return Ok(None);
        }
        let current = self.validate(binding)?;
        ensure!(
            !current.outcome_conflict,
            "document original operation has conflicting terminal evidence"
        );
        self.replay(binding)
    }

    fn claim(&self, binding: &OxrAttemptBinding) -> Result<OxrAttemptClaim> {
        // Availability has already been checked. The one fresh T1 and
        // dispatch decision share a transaction; there is no awaited probe
        // or externally visible half-admission between them.
        if self.invocation.is_some() && self.arm(binding, true)? {
            return Ok(OxrAttemptClaim::Dispatch);
        }
        let current = self.validate(binding)?;
        ensure!(
            !current.outcome_conflict,
            "document original operation has conflicting terminal evidence"
        );
        Ok(OxrAttemptClaim::Replay(self.replay(binding)?))
    }

    fn append(&self, binding: &OxrAttemptBinding, receipt: &OxrAttemptReceipt) -> Result<()> {
        self.validate(binding)?;
        let raw = seal(
            &self.store,
            &TransportObservation {
                schema_version: 1,
                binding_digest: binding.digest()?,
                receipt: receipt.clone(),
            },
            "oxr-transport-observation",
        )?;
        let level = if terminal(receipt) {
            Some(ObservationLevel::RemoteCompleted)
        } else if receipt.task.is_some() {
            Some(ObservationLevel::ProviderAccepted)
        } else {
            None
        };
        let evidence = self.evidence(TRANSPORT_RECEIPT, raw, None, level);
        for _ in 0..MAX_FOLD_RETRIES {
            let current = self.validate(binding)?;
            let conflict = if current.outcome_conflict {
                true
            } else {
                self.replay(binding)?
                    .as_ref()
                    .is_some_and(|previous| receipts_conflict(previous, receipt))
            };
            // This E1 persists transport facts, not task completion. A
            // contradictory terminal body is admitted with sticky Unknown in
            // the same transaction, including when a prior run already ended.
            if let Some(committed) = self.store.record_external_evidence_if_current(
                binding.operation_id,
                binding.attempt_id,
                evidence.clone(),
                None,
                EffectStatus::Unknown,
                ExternalEvidenceFold {
                    expected_revision: current.record_revision,
                    terminal_content_conflict: conflict,
                },
            )? {
                ensure!(
                    !committed.outcome_conflict,
                    "document attempt has conflicting terminal evidence"
                );
                return Ok(());
            }
        }
        anyhow::bail!("document receipt head kept changing; original attempt remains reconcilable")
    }
}

#[async_trait]
impl NativeTaskHandler for DocumentHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "document.parse".into(), definition_version: 1,
            implementation_ref: ImplementationRef { plugin_id: Self::plugin_id(), artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 },
            role_revision: "native-document-parse/1".into(), authority_refs: vec!["document.parse/v1".into()],
            prompt_section: Some("document.parse/v1 是已配置的文档解析 child。输入 source_ref（本地 CAS 的完整 ObjectRef）及 request（schema_version:1、source_digest、source_name、pages、closure_id:oxr/1.0、scope）。source_digest 必须等于 source_ref.digest；pages 留空表示全文，页索引从零开始。只向绑定的 OXR 部署上传一次，结果保留页结构及来源；unknown/unavailable 不表示成功。取消或等待超时不证明远端任务已结束，恢复查询原任务，不重新上传。".into()),
        }
    }

    fn effect_contracts(&self) -> Vec<EffectContract> {
        let descriptor = self.descriptor();
        vec![EffectContractBuilder::new(Self::plugin_id(), descriptor.implementation(), OPERATION, 1)
            .resource("document_source", "frozen CAS source digest", ResourceAccess::Read)
            .resource("oxr_task", "bound endpoint/account/scope and original task handle", ResourceAccess::Write)
            .action(EffectActionKind::Cancel).action(EffectActionKind::Restore)
            .idempotency(IdempotencyGuarantee::None)
            .external_effect(ExternalEffect { kind: "document_upload".into(),
                description: "Uploads the frozen source once; remote parsing and account usage may continue after a local cancellation or lost response".into(),
                reversible: false, compensation_possible: false })
            .cancel(CancelContract { can_cancel_before_start: true, can_cancel_in_flight: false,
                cancel_confirms_stopped: false, may_have_partial_effect: true })
            .checkpoint(CheckpointContract { capturable: vec!["original attempt binding, accepted task handle/deadline and page receipts in existing CAS/E1 history".into()],
                excluded: vec!["remote engine memory and remote task termination".into()], coordinated: false,
                consistency: "durable arm before upload; immutable acceptance before polling; unknown is not replayable".into() })
            .basis(GuaranteeBasis::RuntimeConstrained).build()]
    }

    fn capabilities(&self) -> Vec<NativeCapabilityDeclaration> {
        vec![NativeCapabilityDeclaration { capability_id: "native.document.parse".into(), version: "1".into(),
            description: "Parse a frozen document with the configured OXR deployment, retaining page provenance and unknown effects".into(),
            usage: Some("delegate:document.parse:1 {source_ref:<CAS ObjectRef>,request:<ParseRequest v1>}".into()) }]
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        ensure!(
            invocation.task.kind == "document.parse" && invocation.task.definition_version == 1,
            "document task identity mismatch"
        );
        let input: DocumentInput = serde_json::from_value(invocation.input.clone())?;
        let source = self.source(&input)?;
        let frozen = FrozenInput {
            schema_version: 1,
            input: input.clone(),
            provider: self.provider_binding(),
        };
        let input_ref = seal(&self.store, &frozen, "document-frozen-input")?;
        let (operation, attempt) = self.operation_parts(&invocation, input_ref)?;
        let mut bridge = StoreReceipts::new(self.store.clone(), operation, attempt);
        bridge.invocation = Some(invocation.task_invocation);
        let bridge = Arc::new(bridge);
        self.parse(&input, &source, self.provider(bridge.clone()), &bridge)
            .await
    }

    async fn reconcile(
        &self,
        task: &TaskRecord,
        operation_id: EntityId,
    ) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && operation.operation_name == OPERATION,
            "original document operation mismatch"
        );
        if operation.state == OperationState::Resolved || operation.outcome_conflict {
            return Ok(operation);
        }
        let frozen: FrozenInput = read(&self.store, &operation.final_input_ref)?;
        ensure!(
            frozen.schema_version == 1
                && operation.input_digest == operation.final_input_ref.digest,
            "document frozen input schema mismatch"
        );
        ensure!(
            frozen.provider == self.provider_binding(),
            "original OXR deployment/account/model configuration is unavailable"
        );
        let source = self.source(&frozen.input)?;
        let attempt = self.store.external_operation_attempt(operation_id)?;
        let bridge = Arc::new(StoreReceipts::new(self.store.clone(), operation, attempt));
        // No invocation can arm or dispatch. Even a missing acceptance receipt
        // remains Unknown, rather than recreating the original external effect.
        self.parse(
            &frozen.input,
            &source,
            self.provider(bridge.clone()),
            &bridge,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::{
        agent::CallContext,
        store::WriteBatch,
        subject::{
            GenesisRef, SubjectBranchHead, SubjectGenesis, SubjectMode, SubjectStep,
            SubjectStepState,
        },
        task::{DispatchEligibility, RunMode, TaskOwner, TaskState},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::Notify,
    };

    #[derive(Default)]
    struct WireState {
        uploads: AtomicUsize,
        checks: AtomicUsize,
        upload_received: Notify,
        release_upload: Notify,
        probes: AtomicUsize,
        first_probe: Notify,
        release_probe: Notify,
        first_check: Notify,
        slow_body: Notify,
        success_waiting: Notify,
        conflict_waiting: Notify,
        release_slow: Notify,
        release_success: Notify,
        release_conflict: Notify,
    }

    struct Wire {
        endpoint: String,
        state: Arc<WireState>,
        acceptor: tokio::task::JoinHandle<()>,
    }

    impl Drop for Wire {
        fn drop(&mut self) {
            self.acceptor.abort();
        }
    }

    fn success(text: &str) -> String {
        serde_json::json!({"code":0,"data":{"task_id":"original-task","status":"success",
        "metadata":{"page_num":1},"result":{"outputs":{"json":{"pages":[
            {"page_num":0,"blocks":[{"idx":0,"content":text,"type":"Text","format":"text"}]}
        ]}}}}})
        .to_string()
    }

    async fn wire(hold_first_probe: bool) -> Result<Wire> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}/v1", listener.local_addr()?);
        let state = Arc::new(WireState::default());
        let shared = state.clone();
        let acceptor = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).await.is_err() {
                            return Ok::<(), anyhow::Error>(());
                        }
                        head.push(byte[0]);
                        if head.len() > 16 * 1024 {
                            return Ok::<(), anyhow::Error>(());
                        }
                    }
                    let head = String::from_utf8(head)?;
                    let target = head
                        .lines()
                        .next()
                        .context("missing HTTP request line")?
                        .split_whitespace()
                        .nth(1)
                        .context("missing HTTP target")?;
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    let mut body = vec![0; length];
                    if stream.read_exact(&mut body).await.is_err() {
                        return Ok::<(), anyhow::Error>(());
                    }
                    let mut held_body = false;
                    let answer = if target.ends_with("/parse/sync") {
                        state.uploads.fetch_add(1, Ordering::SeqCst);
                        state.upload_received.notify_one();
                        state.release_upload.notified().await;
                        // A nominally successful envelope with no pages array
                        // cannot establish an empty completed document.
                        serde_json::json!({"code":0,"data":{"metadata":{"page_num":1},
                            "result":{"outputs":{"json":{}}}}})
                        .to_string()
                    } else if target.ends_with("/parse/async") {
                        state.uploads.fetch_add(1, Ordering::SeqCst);
                        serde_json::json!({"code":0,"data":{"task_id":"original-task"}}).to_string()
                    } else if target.ends_with("/parse/async_check") {
                        assert!(String::from_utf8_lossy(&body).contains("original-task"));
                        match state.checks.fetch_add(1, Ordering::SeqCst) {
                            0 => {
                                state.first_check.notify_one();
                                state.release_slow.notified().await;
                                success("original")
                            }
                            1 => {
                                held_body = true;
                                success("original")
                            }
                            2 => {
                                state.success_waiting.notify_one();
                                state.release_success.notified().await;
                                success("original")
                            }
                            _ => {
                                state.conflict_waiting.notify_one();
                                state.release_conflict.notified().await;
                                success("contradictory late observation")
                            }
                        }
                    } else {
                        if hold_first_probe && state.probes.fetch_add(1, Ordering::SeqCst) == 0 {
                            state.first_probe.notify_one();
                            state.release_probe.notified().await;
                        }
                        "{}".into()
                    };
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        answer.len()
                    );
                    if stream.write_all(headers.as_bytes()).await.is_err() {
                        return Ok::<(), anyhow::Error>(());
                    }
                    if held_body {
                        state.slow_body.notify_one();
                        state.release_slow.notified().await;
                    }
                    let _ = stream.write_all(answer.as_bytes()).await;
                    Ok::<(), anyhow::Error>(())
                });
            }
        });
        Ok(Wire {
            endpoint,
            state,
            acceptor,
        })
    }

    struct Fixture {
        dir: std::path::PathBuf,
        invocation: TaskInvocation,
        task: TaskRecord,
        operation_id: EntityId,
        authority: AuthorityRef,
        input: serde_json::Value,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl Fixture {
        fn open(&self) -> Result<Arc<Store>> {
            Ok(Arc::new(Store::open(
                self.dir.join("shenko.db"),
                self.dir.join("cas"),
            )?))
        }

        fn call(&self) -> NativeTaskInvocation {
            let mut context = CallContext::new_system(RuntimeGeneration::default());
            context.task_id = Some(self.task.task_id);
            context.invocation_id = Some(self.invocation.invocation_id);
            context.controller_epoch = self.invocation.epoch;
            NativeTaskInvocation {
                context,
                task_invocation: self.invocation.clone(),
                task: self.task.clone(),
                operation_id: self.operation_id,
                authority: self.authority.clone(),
                input: self.input.clone(),
                child_tools: None,
            }
        }
    }

    fn handler(store: Arc<Store>, endpoint: &str, transport: OxrTransport) -> DocumentHandler {
        DocumentHandler {
            store,
            account: "loopback-account".into(),
            config: OxrProviderConfig {
                endpoint: Some(endpoint.into()),
                model_identity: Some("loopback-protocol-fixture".into()),
                scope: Some("fixture-scope".into()),
                credential: Some(CredentialHandle::new("fixture-key")),
                transport,
                request_timeout: Some(Duration::from_secs(10)),
                poll_timeout: Some(Duration::from_secs(2)),
                poll_interval: Some(Duration::from_millis(5)),
                ..OxrProviderConfig::default()
            },
        }
    }

    fn fixture() -> Result<(Fixture, Arc<Store>)> {
        let dir = std::env::temp_dir().join(format!("shenko-document-{}", EntityId::new()));
        std::fs::create_dir_all(&dir)?;
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas"))?);
        let source = store
            .cas
            .seal_object(
                b"%PDF fixture",
                "application/pdf",
                "document-fixture",
                "source",
                1,
            )?
            .object;
        let input = serde_json::json!({ "source_ref": source, "request": {
            "schema_version":1, "source_digest":source.digest, "source_name":"fixture.pdf",
            "pages":[], "closure_id":"oxr/1.0", "scope":"fixture-scope"
        }});
        let input_ref = seal(&store, &input, "document-input")?;
        let task_id = EntityId::new();
        let agent_id = EntityId::new();
        let invocation_id = EntityId::new();
        let epoch = ControllerEpoch(1);
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let root_id = EntityId::new();
        let task = TaskRecord {
            record_schema_version: 1,
            task_id,
            task_revision: TaskRevision(0),
            record_revision: RecordRevision(0),
            agent_id,
            conversation_id: agent_id,
            owner: TaskOwner::Task { id: root_id },
            background: true,
            kind: "document.parse".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: DocumentHandler::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            input_ref: input_ref.clone(),
            state: TaskState::Running {
                checkpoint_ref: None,
                invocation_id,
                mode: RunMode::Run,
                epoch,
            },
            abort_request: None,
            memos: serde_json::json!({"native_child":true})
                .as_object()
                .context("fixture memos must be an object")?
                .clone(),
            residual_case_ids: vec![],
            created_by_step_id: None,
            dispatch_eligibility: DispatchEligibility::Ready,
            no_progress_crashes: 0,
            first_run_at: None,
            terminal_at: None,
            created_commit: zero,
            updated_commit: zero,
        };
        let genesis_id = EntityId::new();
        let first_step_id = EntityId::new();
        let genesis = SubjectGenesis {
            genesis_id,
            subject_id: agent_id,
            bootstrap_id: EntityId::new(),
            launch_id: EntityId::new(),
            seed_manifest_ref: input_ref.clone(),
            initial_profile_ref: input_ref.clone(),
            home_manifest_ref: input_ref.clone(),
            bootstrap_event_ref: input_ref,
            created_commit: zero,
        };
        let head = SubjectBranchHead {
            agent_id,
            branch_id: BranchId::new(),
            subject_root_task_id: root_id,
            controller_epoch: epoch,
            software_generation: RuntimeGeneration::default(),
            genesis_ref: GenesisRef {
                ledger_id: store.ledger.ledger_id(),
                genesis_id,
            },
            committed_step_no: 0,
            committed_step_id: None,
            committed_at: zero,
            next_step_id: first_step_id,
            mode: SubjectMode::Active,
            mode_reason_ref: None,
            record_revision: RecordRevision(0),
        };
        let first_step = SubjectStep {
            step_id: first_step_id,
            branch_id: head.branch_id.clone(),
            step_no: 1,
            expected_parent_commit: zero,
            controller_epoch: epoch,
            software_generation: RuntimeGeneration::default(),
            state: SubjectStepState::Planned,
            input_plan_ref: None,
            context_revision: None,
            active_request_id: None,
            active_attempt_id: None,
            model_result_ref: None,
            model_outcome_ref: None,
            decision_ref: None,
            commit_ref: None,
            retry_reason_ref: None,
            record_revision: RecordRevision(0),
            created_by_step_id: None,
        };
        let mut root = task.clone();
        root.task_id = root_id;
        root.owner = TaskOwner::Task { id: root_id };
        root.kind = "subject.root".into();
        root.memos.clear();
        store.initialize_genesis(&genesis, &head, &first_step, &root)?;
        store.start_subject_dispatcher()?;
        let mut batch = WriteBatch::new();
        batch.task_upserts.push(task.clone());
        store.commit(batch)?;
        let invocation = TaskInvocation {
            task_id,
            task_revision: task.task_revision,
            record_revision: task.record_revision,
            invocation_id,
            epoch,
            software_generation: RuntimeGeneration::default(),
            worker_id: EntityId::new(),
            node_id: EntityId::new(),
        };
        Ok((
            Fixture {
                dir,
                invocation,
                task,
                operation_id: EntityId::new(),
                input,
                authority: AuthorityRef {
                    delegation_id: task_id,
                    delegation_revision: RecordRevision(0),
                    decision_ref: EntityId::new(),
                },
            },
            store,
        ))
    }

    /// One malformed upload journey: concurrent callers and reconstructed
    /// whole/page callers all retain Unknown from the original Store attempt.
    #[test]
    fn malformed_document_survives_reopen_without_another_upload() -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let wire = wire(true).await?;
                let (fixture, store) = fixture()?;
                let initial = Arc::new(handler(store.clone(), &wire.endpoint, OxrTransport::Sync));
                let call = fixture.call();
                let first = {
                    let initial = initial.clone();
                    tokio::spawn(async move { initial.execute(call).await })
                };
                tokio::time::timeout(Duration::from_secs(3), wire.state.first_probe.notified())
                    .await?;
                assert!(
                    store
                        .find_external_operation(fixture.operation_id)?
                        .is_none(),
                    "preflight must not leave a half-armed attempt"
                );
                let second_call = fixture.call();
                let second = {
                    let initial = initial.clone();
                    tokio::spawn(async move { initial.execute(second_call).await })
                };
                tokio::time::timeout(
                    Duration::from_secs(3),
                    wire.state.upload_received.notified(),
                )
                .await?;
                let dispatched = store.external_task(fixture.task.task_id)?;
                let attempt = store.external_operation_attempt(fixture.operation_id)?;
                let marker = dispatched
                    .memos
                    .get(shenko_store::child::NATIVE_DISPATCH)
                    .context("upload requires durable native dispatch marker")?;
                let dispatch_commit: CommitRef = serde_json::from_value(
                    marker["attempts"][attempt.attempt_id.to_string()]["commit"].clone(),
                )?;
                assert_eq!(dispatch_commit, attempt.armed_commit);
                assert_eq!(
                    store
                        .external_operation(fixture.operation_id)?
                        .prepared_commit,
                    attempt.armed_commit
                );
                wire.state.release_probe.notify_one();
                assert!(first.await??.outcome.is_none());
                wire.state.release_upload.notify_one();
                assert!(second.await??.outcome.is_none());
                let originals = store.external_operation_evidence(fixture.operation_id)?;
                let run: ParseRun = read(
                    &store,
                    &originals
                        .iter()
                        .rev()
                        .find(|r| r.source_id == RESULT_RECEIPT)
                        .context("missing parse result receipt")?
                        .raw_ref,
                )?;
                assert_eq!(run.state, ParseState::Running);
                assert!(run.document_unknown_outcome.is_some());
                assert!(run.checkpoints.is_empty());
                drop(initial);
                drop(store);

                let reopened = fixture.open()?;
                let recovered = handler(reopened.clone(), &wire.endpoint, OxrTransport::Sync);
                let task = reopened.external_task(fixture.task.task_id)?;
                let operation = recovered.reconcile(&task, fixture.operation_id).await?;
                assert_eq!(operation.state, OperationState::Reconciling);
                assert_eq!(operation.effect_status, EffectStatus::Unknown);
                assert!(operation.outcome.is_none());
                let attempt = reopened.external_operation_attempt(fixture.operation_id)?;
                let bridge = Arc::new(StoreReceipts::new(reopened.clone(), operation, attempt));
                let provider = recovered.provider(bridge);
                let input: DocumentInput = serde_json::from_value(fixture.input.clone())?;
                let source = recovered.source(&input)?;
                let (page0, page1) = tokio::join!(
                    provider.parse_page(
                        &source,
                        shenko_document::PageRef { page_index: 0 },
                        &input.request
                    ),
                    provider.parse_page(
                        &source,
                        shenko_document::PageRef { page_index: 1 },
                        &input.request
                    )
                );
                assert!(matches!(page0?, shenko_document::PageOutcome::Unknown(_)));
                assert!(matches!(page1?, shenko_document::PageOutcome::Unknown(_)));
                assert_eq!(wire.state.uploads.load(Ordering::SeqCst), 1);
                assert_eq!(
                    reopened
                        .external_operation_attempts(fixture.operation_id)?
                        .len(),
                    1
                );
                for original in originals {
                    assert!(reopened.cas.read_object(&original.raw_ref.digest).is_ok());
                }
                Ok(())
            })
    }

    /// One accepted async task journey: cancel the caller during a poll,
    /// reopen Store, bound a slow body by the original deadline, then accept a
    /// late result and preserve a racing contradictory result as sticky Unknown.
    #[test]
    fn accepted_task_reconciles_after_cancel_deadline_and_late_conflict() -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let wire = wire(false).await?;
                let (fixture, store) = fixture()?;
                let initial = Arc::new(handler(store.clone(), &wire.endpoint, OxrTransport::Async));
                let call = fixture.call();
                let worker = {
                    let initial = initial.clone();
                    tokio::spawn(async move { initial.execute(call).await })
                };
                tokio::time::timeout(Duration::from_secs(3), wire.state.first_check.notified())
                    .await?;
                let accepted = store.external_operation_evidence(fixture.operation_id)?;
                let accepted: TransportObservation = read(
                    &store,
                    &accepted
                        .iter()
                        .find(|r| r.source_id == TRANSPORT_RECEIPT)
                        .context("missing transport receipt")?
                        .raw_ref,
                )?;
                let original_task = accepted.receipt.task.context("missing accepted task")?;
                worker.abort();
                assert!(
                    worker
                        .await
                        .err()
                        .context("caller was not cancelled")?
                        .is_cancelled()
                );
                drop(initial);
                drop(store);

                let reopened = fixture.open()?;
                let recovered = Arc::new(handler(
                    reopened.clone(),
                    &wire.endpoint,
                    OxrTransport::Async,
                ));
                let task = reopened.external_task(fixture.task.task_id)?;
                assert!(chrono::Utc::now() < original_task.poll_deadline);
                let unknown = tokio::time::timeout(
                    Duration::from_secs(3),
                    recovered.reconcile(&task, fixture.operation_id),
                )
                .await??;
                assert_eq!(wire.state.checks.load(Ordering::SeqCst), 2);
                assert_eq!(unknown.state, OperationState::Reconciling);
                assert!(unknown.outcome.is_none());
                let evidence = reopened.external_operation_evidence(fixture.operation_id)?;
                let last_transport: TransportObservation = read(
                    &reopened,
                    &evidence
                        .iter()
                        .rev()
                        .find(|r| r.source_id == TRANSPORT_RECEIPT)
                        .context("missing transport receipt")?
                        .raw_ref,
                )?;
                assert_eq!(last_transport.receipt.task, Some(original_task.clone()));
                assert!(matches!(
                    last_transport.receipt.outcome,
                    Some(Err(shenko_document::OxrError::PollTimeout(_)))
                ));

                let operation_id = fixture.operation_id;
                let operation = reopened.external_operation(operation_id)?;
                let attempt = reopened.external_operation_attempt(operation_id)?;
                let binding: OxrAttemptBinding = read(
                    &reopened,
                    operation
                        .checkpoint_scope_ref
                        .as_ref()
                        .context("missing attempt checkpoint")?,
                )?;
                let bridge = Arc::new(StoreReceipts::new(reopened.clone(), operation, attempt));
                let client = OxrClient::new(OxrClientConfig::new(
                    &wire.endpoint,
                    CredentialHandle::new("fixture-key"),
                ))?;
                let first = {
                    let bridge = bridge.clone();
                    let task = original_task.clone();
                    tokio::spawn(async move {
                        let document = client
                            .check_document(&task)
                            .await?
                            .context("task did not complete")?;
                        bridge.append(
                            &binding,
                            &OxrAttemptReceipt {
                                task: Some(task),
                                outcome: Some(Ok(document)),
                            },
                        )?;
                        Ok::<(), anyhow::Error>(())
                    })
                };
                tokio::time::timeout(
                    Duration::from_secs(3),
                    wire.state.success_waiting.notified(),
                )
                .await?;
                let late = {
                    let recovered = recovered.clone();
                    let task = task.clone();
                    tokio::spawn(async move { recovered.reconcile(&task, operation_id).await })
                };
                tokio::time::timeout(
                    Duration::from_secs(3),
                    wire.state.conflict_waiting.notified(),
                )
                .await?;
                wire.state.release_success.notify_one();
                first.await??;
                // Crash point: the terminal transport receipt exists, but no final
                // ParseRun/page manifest does. The endpoint now stops accepting new
                // connections; decoding committed success must not probe it.
                wire.acceptor.abort();
                let pending = reopened.external_operation(operation_id)?;
                assert_eq!(pending.state, OperationState::Reconciling);
                let succeeded = recovered.reconcile(&task, operation_id).await?;
                assert_eq!(succeeded.outcome, Some(OperationOutcome::Succeeded));
                // A changed page selection shares the transport binding, but
                // is not the original frozen native request. It must not be
                // silently answered with that request's settled success.
                let mut changed_replay = fixture.call();
                changed_replay.input["request"]["pages"] = serde_json::json!([{"page_index":0}]);
                let rejection = recovered
                    .execute(changed_replay)
                    .await
                    .err()
                    .context("changed frozen native request was accepted")?;
                assert!(
                    rejection
                        .to_string()
                        .contains("original frozen input or contract changed")
                );
                let before_conflict = reopened.external_operation_evidence(operation_id)?;
                wire.state.release_conflict.notify_one();
                let conflicted = late.await??;
                assert!(conflicted.outcome_conflict);
                assert!(conflicted.outcome.is_none());
                assert_eq!(conflicted.effect_status, EffectStatus::Unknown);
                assert_eq!(wire.state.uploads.load(Ordering::SeqCst), 1);
                assert_eq!(reopened.external_operation_attempts(operation_id)?.len(), 1);
                let retained = reopened.external_operation_evidence(operation_id)?;
                assert!(retained.len() > before_conflict.len());
                for original in before_conflict {
                    assert!(retained.iter().any(|r| r.receipt_id == original.receipt_id));
                }
                assert!(
                    recovered
                        .reconcile(&task, operation_id)
                        .await?
                        .outcome_conflict
                );
                Ok(())
            })
    }
}
