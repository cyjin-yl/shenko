//! A deliberately narrow deterministic native definition. This is a real child
//! process, not a general shell tool and not a claim to run an arbitrary LLM child.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    commit::CommitRef,
    evidence::{DecodeStatus, EvidenceReceipt, ObservationLevel},
    ids::*,
    object::*,
    operation::*,
    task::TaskRecord,
};
use std::{process::Stdio, sync::Arc, time::Duration};

const MAX_TEXT_BYTES: usize = 64 * 1024;
const PRINTF: &str = "/usr/bin/printf";
const CONTRACT: &[u8] = b"shenko.host.printf/v1: exit 0 and exact frozen UTF-8 stdout; no shell";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostPrintfInput {
    text: String,
    timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    text: String,
    timeout_secs: u64,
}

#[derive(Debug, Serialize)]
struct HostObservation {
    operation_id: EntityId,
    exit_code: Option<i32>,
    stdout_b64: Option<String>,
    stderr_b64: Option<String>,
    exact_postcondition: bool,
    transport_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HostPrintfResult {
    exit_code: Option<i32>,
    stdout_utf8: Option<String>,
    stderr_utf8: Option<String>,
    exact_postcondition: bool,
    transport_error: Option<String>,
}

pub struct HostPrintfHandler {
    store: Arc<Store>,
}

impl HostPrintfHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    fn plugin_id() -> EntityId {
        "6d5d0c16-7f7f-4f6f-9f57-90d8aa30f0a1"
            .parse()
            .expect("fixed built-in plugin UUID")
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "host-native-effects",
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

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let input: HostPrintfInput = serde_json::from_value(invocation.input)?;
        ensure!(
            input.text.len() <= MAX_TEXT_BYTES,
            "printf input exceeds 64KiB"
        );
        ensure!(!input.text.contains('\0'), "printf input contains NUL");
        let timeout_secs = input.timeout_secs.unwrap_or(5);
        ensure!(
            (1..=30).contains(&timeout_secs),
            "timeout must be 1..=30 seconds"
        );
        let frozen = FrozenInput {
            text: input.text,
            timeout_secs,
        };
        let input_ref = self.seal(&frozen, "host-printf-frozen-input")?;
        let bindings = ResourceBindings {
            provider: "host.printf.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "no_declared_external_effects".into(),
            extra: serde_json::json!({"executable":PRINTF,"argv":["%s","<frozen text>"]}),
        };
        let resource = self.seal(&bindings, "host-printf-binding")?;
        let acceptance = self.seal(
            &"process exited 0 and stdout exactly equals frozen text",
            "host-printf-acceptance",
        )?;
        let retry = self.seal(
            &"no automatic replay; an armed process without evidence remains unknown",
            "host-printf-retry",
        )?;
        let contract_digest = Digest::blake3(CONTRACT);
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:host-printf-v1".into(),
            },
            symbol: "host.printf.execute".into(),
            schema: SchemaRef {
                name: "host-printf-observation-v1".into(),
                major: 1,
                digest: contract_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "host.printf.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: invocation.authority,
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
            prepared_commit: zero,
            evidence_through: None,
        };
        let mut attempt = AttemptRecord {
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
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }

        // The T1 commit above precedes process creation. No shell is involved;
        // fixed argv bounds output to the frozen UTF-8 byte string.
        let result = tokio::time::timeout(
            Duration::from_secs(frozen.timeout_secs),
            tokio::process::Command::new(PRINTF)
                .arg("%s")
                .arg(&frozen.text)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await;
        let (output, transport_error) = match result {
            Ok(Ok(output)) => (Some(output), None),
            Ok(Err(error)) => (None, Some(error.to_string())),
            Err(_) => (
                None,
                Some("deadline elapsed; process-stop evidence is not assumed".into()),
            ),
        };
        let exact_postcondition = output.as_ref().is_some_and(|output| {
            output.status.code() == Some(0)
                && output.stdout == frozen.text.as_bytes()
                && output.stderr.is_empty()
        });
        let outcome = output.as_ref().map(|output| {
            if exact_postcondition {
                OperationOutcome::Succeeded
            } else if output.status.code().is_some() {
                OperationOutcome::Failed
            } else {
                OperationOutcome::Interrupted
            }
        });
        let observation = HostObservation {
            operation_id: invocation.operation_id,
            exit_code: output.as_ref().and_then(|output| output.status.code()),
            stdout_b64: output.as_ref().map(|output| B64.encode(&output.stdout)),
            stderr_b64: output.as_ref().map(|output| B64.encode(&output.stderr)),
            exact_postcondition,
            transport_error,
        };
        let raw = self.seal(&observation, "host-printf-observation")?;
        let normalized = self.seal(
            &HostPrintfResult {
                exit_code: output.as_ref().and_then(|output| output.status.code()),
                stdout_utf8: output
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
                stderr_utf8: output
                    .as_ref()
                    .map(|output| String::from_utf8_lossy(&output.stderr).into_owned()),
                exact_postcondition,
                transport_error: observation.transport_error.clone(),
            },
            "host-printf-result",
        )?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "host.printf.v1".into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: if output.is_some() {
                DecodeStatus::Decoded
            } else {
                DecodeStatus::Uninterpreted
            },
            observation_level: output.as_ref().map(|_| {
                if exact_postcondition {
                    ObservationLevel::PostconditionVerified
                } else {
                    ObservationLevel::RemoteCompleted
                }
            }),
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            invocation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            if output.is_some() {
                EffectStatus::None
            } else {
                EffectStatus::Unknown
            },
        )
    }
}

#[async_trait]
impl NativeTaskHandler for HostPrintfHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "host.printf".into(),
            definition_version: 1,
            implementation_ref: shenko_types::task::ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-child/1".into(),
            authority_refs: vec!["host.process.printf.no_effects/v1".into()],
        }
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
            operation.task_id == task.task_id && operation.operation_name == "host.printf.v1",
            "original_operation_contract_mismatch"
        );
        // A host printf has no durable provider handle. Loss of the controller
        // after T1 is not evidence that the process never ran. Preserve that
        // uncertainty as E1; never execute a second attempt during reconcile.
        if operation.state == OperationState::InFlight {
            let attempt = self.store.external_operation_attempt(operation_id)?;
            if attempt.dispatch_stage != AttemptDispatchStage::Ended {
                let raw = self.seal(
                    &serde_json::json!({
                        "operation_id": operation_id,
                        "attempt_id": attempt.attempt_id,
                        "observation": "controller recovered without a conclusive process result",
                        "process_stop_proven": false
                    }),
                    "host-printf-recovery",
                )?;
                let evidence = EvidenceReceipt {
                    record_schema_version: 1,
                    receipt_id: EntityId::new(),
                    operation_id: Some(operation_id),
                    attempt_id: Some(attempt.attempt_id),
                    source_id: "host.printf.recovery".into(),
                    source_instance_id: self.store.capabilities.incarnation.to_string(),
                    source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
                    producer_epoch: None,
                    producer_schema_version: Some("1".into()),
                    raw_ref: raw,
                    decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
                    decode_status: DecodeStatus::Decoded,
                    observation_level: None,
                    normalized_ref: None,
                    observed_at: Some(chrono::Utc::now()),
                    received_commit: operation.prepared_commit,
                };
                return self.store.record_external_evidence(
                    operation_id,
                    attempt.attempt_id,
                    evidence,
                    None,
                    EffectStatus::Unknown,
                );
            }
        }
        // Resolved E1 is reusable; an already reconciling operation remains
        // unresolved until original evidence arrives, with no blind replay.
        Ok(operation)
    }
}
