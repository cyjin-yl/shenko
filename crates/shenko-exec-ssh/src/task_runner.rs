//! Task-bound native SSH effects. One armed admission, never blind replay.
use crate::{ExecOutcome, ExecResult, SshExecutor, SshTarget};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde::{Deserialize, Serialize};
use shenko_store::{Store, TaskInvocation};
use shenko_types::{commit::CommitRef, evidence::*, ids::*, object::*, operation::*};

const HELPER: &str = include_str!("remote_helper.py");
// PR33 727b964 helper artifact: query-only compatibility for its v1 receipts.
// Never use this compatibility path to dispatch or replay the old operation.
const LEGACY_RECEIPT_V1: &str =
    "blake3:3db899dc077c398ecc459e86565ed432e31097335af46291b579c6c3e0eaf9c6";
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshAction {
    Exec {
        argv: Vec<String>,
        stdin_b64: String,
    },
    Upload {
        path: String,
        data_b64: String,
    },
    Download {
        path: String,
    },
}

impl SshAction {
    pub fn exec(argv: Vec<String>, stdin: &[u8]) -> Self {
        Self::Exec {
            argv,
            stdin_b64: B64.encode(stdin),
        }
    }
    pub fn upload(path: String, data: &[u8]) -> Self {
        Self::Upload {
            path,
            data_b64: B64.encode(data),
        }
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Exec { argv, stdin_b64 } => {
                ensure!(
                    !argv.is_empty() && argv.iter().all(|s| !s.contains('\0')),
                    "invalid argv"
                );
                ensure!(
                    B64.decode(stdin_b64)?.len() <= MAX_BYTES,
                    "stdin exceeds 1MiB"
                );
            }
            Self::Upload { path, data_b64 } => {
                ensure!(
                    path.starts_with('/') && !path.contains('\0'),
                    "remote path must be absolute"
                );
                ensure!(
                    B64.decode(data_b64)?.len() <= MAX_BYTES,
                    "upload exceeds 1MiB"
                );
            }
            Self::Download { path } => ensure!(
                path.starts_with('/') && !path.contains('\0'),
                "remote path must be absolute"
            ),
        }
        Ok(())
    }
}

/// Registered native target. Only secret handles/paths, never private key bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshBinding {
    pub target_id: String,
    pub target: SshTarget,
    /// Private, account-owned absolute directory on the managed target.
    pub receipt_root: String,
    /// Public trust bytes sealed in CAS, so recovery cannot silently trust a changed file.
    pub known_hosts: ObjectRef,
}

impl SshBinding {
    pub fn register(
        store: &Store,
        target_id: String,
        mut target: SshTarget,
        receipt_root: String,
    ) -> Result<Self> {
        ensure!(
            target.password.is_none(),
            "password authentication is unsupported"
        );
        ensure!(
            target.key_path.is_some() || target.use_agent,
            "explicit key or agent required"
        );
        if let Some(path) = &target.key_path {
            ensure!(path.is_absolute(), "managed key path must be absolute");
        }
        ensure!(
            !target_id.is_empty() && receipt_root.starts_with('/') && !receipt_root.contains('\0'),
            "invalid target binding"
        );
        let path = target
            .known_hosts_path
            .take()
            .context("managed operations require explicit known_hosts")?;
        let bytes = std::fs::read(path)?;
        ensure!(!bytes.is_empty(), "empty known_hosts");
        let known_hosts = store
            .cas
            .seal_object(
                &bytes,
                "text/plain",
                "ssh-public-trust",
                "openssh-known-hosts",
                1,
            )?
            .object;
        Ok(Self {
            target_id,
            target,
            receipt_root,
            known_hosts,
        })
    }
    fn executor(&self, store: &Store) -> Result<SshExecutor> {
        // CAS locators are historical hints: recovery may relocate the store.
        // Verify and resolve the immutable trust object in the current CAS.
        let path = store.cas.verified_object_path(&self.known_hosts)?;
        let mut target = self.target.clone();
        target.known_hosts_path = Some(path);
        Ok(SshExecutor::over_ssh(target))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FrozenInput {
    binding: SshBinding,
    action: SshAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshReceipt {
    pub operation_id: EntityId,
    pub schema: Option<u32>,
    pub seal: String,
    pub state: String,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout_b64: String,
    #[serde(default)]
    pub stderr_b64: String,
    #[serde(default)]
    pub stdout_truncated: bool,
    #[serde(default)]
    pub stderr_truncated: bool,
    pub sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SshObservation {
    pub operation: OperationRecord,
    pub receipt: Option<SshReceipt>,
    pub transport: Option<ExecResult>,
    pub diagnostic: Option<String>,
}

/// Trusted child-side API. This is not a model-facing authority/registry API.
/// T1 validates the persisted invocation. E1 only appends facts; task terminal
/// settlement remains the current scheduler's responsibility.
pub struct SshTaskRunner<'a> {
    pub store: &'a Store,
}
impl SshTaskRunner<'_> {
    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "ssh-effects",
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

    pub async fn execute(
        &self,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        logical_key: &str,
        authority: AuthorityRef,
        binding: SshBinding,
        action: SshAction,
        timeout_secs: u64,
    ) -> Result<SshObservation> {
        action.validate()?;
        ensure!(
            !logical_key.is_empty() && logical_key.len() <= 128,
            "invalid logical key"
        );
        let frozen = FrozenInput { binding, action };
        let input = self.seal(&frozen, "ssh-frozen-input")?;
        let resource = self.seal(&frozen.binding, "ssh-binding")?;
        let contract_digest = Digest::blake3(HELPER.as_bytes());
        let schema = SchemaRef {
            name: "ssh-receipt-v1".into(),
            major: 1,
            digest: contract_digest.clone(),
        };
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:openssh-python3-v1".into(),
            },
            symbol: "receipt".into(),
            schema,
        };
        let acceptance = self.seal(
            &"remote helper completed; exit code is not application task success",
            "ssh-acceptance",
        )?;
        let retry = self.seal(
            &"query original operation; no automatic dispatch replay",
            "ssh-retry",
        )?;
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut op = OperationRecord {
            record_schema_version: 1,
            operation_id,
            record_revision: RecordRevision(0),
            task_id: ctx.task_id,
            logical_key: logical_key.into(),
            operation_name: "ssh.native.v1".into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input.clone(),
            input_digest: input.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: EntityId::nil(),
                artifact_digest: contract_digest.clone(),
                contract_digest,
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: authority,
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
        let mut att = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id,
            attempt_no: 1,
            invocation_id: ctx.invocation_id,
            worker_id: ctx.worker_id,
            node_id: ctx.node_id,
            epoch: ctx.epoch,
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
        if let Some(existing) = self.store.arm_external_operation(ctx, &mut op, &mut att)? {
            // Even an armed-but-never-sent record is uncertain after recovery.
            return self.query(existing.operation_id, timeout_secs).await;
        }
        self.observe(op, att, frozen, "dispatch", timeout_secs)
            .await
    }

    pub async fn query(&self, id: EntityId, timeout_secs: u64) -> Result<SshObservation> {
        let op = self.store.external_operation(id)?;
        let att = self.store.external_operation_attempt(id)?;
        ensure!(
            op.operation_name == "ssh.native.v1"
                && (op.contract_ref.contract_digest == Digest::blake3(HELPER.as_bytes())
                    || op.contract_ref.contract_digest.0 == LEGACY_RECEIPT_V1),
            "unsupported receipt reader version"
        );
        let bytes = self.store.cas.read_object(&op.final_input_ref.digest)?;
        ensure!(
            Digest::blake3(&bytes) == op.input_digest,
            "frozen input corrupted"
        );
        let frozen = serde_json::from_slice(&bytes)?;
        self.observe(op, att, frozen, "query", timeout_secs).await
    }

    async fn observe(
        &self,
        op: OperationRecord,
        att: AttemptRecord,
        frozen: FrozenInput,
        mode: &str,
        timeout: u64,
    ) -> Result<SshObservation> {
        fn quote(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        let command = format!(
            "python3 -I -c {} {} {} {} {}",
            quote(HELPER),
            quote(&frozen.binding.receipt_root),
            quote(&op.operation_id.to_string()),
            quote(&op.input_digest.0),
            quote(mode)
        );
        let payload = if mode == "dispatch" {
            serde_json::to_vec(&frozen.action)?
        } else {
            vec![]
        };
        let result = match frozen.binding.executor(self.store) {
            Ok(executor) => executor.run_with_input(&command, &payload, timeout).await,
            Err(e) => Err(e),
        };
        // Spawn/trust/read errors after T1 remain armed/unknown, not retryable failure.
        let (transport, dispatch_error) = match result {
            Ok(result) => (Some(result), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let receipt = transport
            .as_ref()
            .filter(|r| r.outcome == Some(ExecOutcome::Exited) && r.exit_code == 0)
            .and_then(|r| serde_json::from_str::<SshReceipt>(&r.stdout).ok())
            .filter(|r| r.operation_id == op.operation_id && r.seal == op.input_digest.0);
        let completed = receipt
            .as_ref()
            .filter(|r| r.schema == Some(1) && r.state == "completed" && r.exit_code.is_some());
        let outcome = completed.map(|r| {
            if r.exit_code == Some(0) {
                OperationOutcome::Succeeded
            } else {
                OperationOutcome::Failed
            }
        });
        // Arbitrary exec/nonzero effects are not proven by process exit. Read-only
        // downloads have no external mutation; successful uploads have sealed receipt.
        let effect = match (&frozen.action, completed) {
            (SshAction::Download { .. }, Some(_)) => EffectStatus::None,
            (SshAction::Upload { .. }, Some(r)) if r.exit_code == Some(0) && r.sha256.is_some() => {
                EffectStatus::Complete
            }
            _ => EffectStatus::Unknown,
        };
        let raw = self.seal(
            &(
                receipt.as_ref(),
                transport.as_ref(),
                dispatch_error.as_ref(),
            ),
            "ssh-observation",
        )?;
        let ev = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(op.operation_id),
            attempt_id: Some(att.attempt_id),
            source_id: "ssh.native.v1".into(),
            source_instance_id: frozen.binding.target_id,
            source_event_key: None,
            producer_epoch: Some(att.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(op.contract_ref.receipt_reader_ref.clone()),
            decode_status: if receipt.is_some() {
                DecodeStatus::Decoded
            } else {
                DecodeStatus::Uninterpreted
            },
            observation_level: completed.map(|_| ObservationLevel::RemoteCompleted),
            normalized_ref: None,
            observed_at: Some(chrono::Utc::now()),
            received_commit: op.prepared_commit,
        };
        let operation = self.store.record_external_evidence(
            op.operation_id,
            att.attempt_id,
            ev,
            outcome,
            effect,
        )?;
        Ok(SshObservation {
            operation,
            receipt,
            transport,
            diagnostic: dispatch_error,
        })
    }
}
