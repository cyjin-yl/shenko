//! Task-bound native SSH effects. One armed admission, never blind replay.
use crate::{ExecOutcome, ExecResult, SshExecutor, SshTarget};
use crate::native_transport::{NativeCommandLimits, NativeCommandObservation, NativeExit, NativeSshSession};
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
// Preserve the exact PR127 helper artifact as data for query-only recovery.
// It is never dispatched; new effects use HELPER and its new contract digest.
const PR127_RECEIPT_V1: &str = include_str!("remote_helper_pr127_v1.py");
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
    pub(super) fn validate(&self) -> Result<()> {
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
        // OpenSSH requires an absolute path even when this Store was opened
        // with a relative CAS root.
        let path = store.cas.verified_object_path(&self.known_hosts)?.canonicalize()?;
        let mut target = self.target.clone();
        target.known_hosts_path = Some(path);
        Ok(SshExecutor::over_ssh(target))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct FrozenInput {
    pub(super) binding: SshBinding,
    pub(super) action: SshAction,
    // None serializes exactly like the original two-field input. Existing
    // operations and their receipt readers keep their original input digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) selection: Option<crate::selected_task::TaskBoundNodeSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) installation: Option<crate::NativeInstallationSeal>,
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
    /// Native protocol evidence is separate from historical host-process status.
    /// In particular an old OpenSSH exit255 is never retyped as remote exit255.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_transport: Option<NativeCommandObservation>,
    pub diagnostic: Option<String>,
}

enum ObservedTransport {
    OpenSsh(Result<ExecResult, String>),
    Native(Result<NativeCommandObservation, String>),
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
        self.execute_frozen(ctx, operation_id, logical_key, authority,
            FrozenInput { binding, action, selection: None, installation: None }, timeout_secs).await
    }

    pub(super) async fn execute_frozen(
        &self,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        logical_key: &str,
        authority: AuthorityRef,
        frozen: FrozenInput,
        timeout_secs: u64,
    ) -> Result<SshObservation> {
        let (op, att, frozen, fresh) = self.prepare_frozen(
            ctx, operation_id, logical_key, authority, frozen)?;
        self.observe(op, att, frozen, if fresh { "dispatch" } else { "query" }, timeout_secs).await
    }

    fn prepare_frozen(
        &self,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        logical_key: &str,
        authority: AuthorityRef,
        frozen: FrozenInput,
    ) -> Result<(OperationRecord, AttemptRecord, FrozenInput, bool)> {
        if let Some(selected) = &frozen.selection {
            selected.ensure_fresh()?;
        }
        frozen.action.validate()?;
        ensure!(
            !logical_key.is_empty() && logical_key.len() <= 128,
            "invalid logical key"
        );
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
            let (op, att, frozen) = self.query_input(existing.operation_id)?;
            return Ok((op, att, frozen, false));
        }
        Ok((op, att, frozen, true))
    }

    pub(super) async fn execute_frozen_installed(
        &self, ctx: &TaskInvocation, operation_id: EntityId, logical_key: &str,
        authority: AuthorityRef, mut frozen: FrozenInput,
        installation: &crate::NativeSshInstallation, limits: NativeCommandLimits,
    ) -> Result<SshObservation> {
        ensure!(installation.matches_original(self.store, &frozen.binding, Some(installation.seal())),
            "native SSH installation binding mismatch");
        // The native transport's already established bound includes the JSON
        // envelope/base64, not merely decoded action stdin/upload bytes. Reject
        // oversized fresh input before T1, never as an unknown sent operation.
        ensure!(serde_json::to_vec(&frozen.action)?.len() <= 1024 * 1024,
            "native SSH action exceeds 1MiB encoded transport input");
        frozen.installation = Some(installation.seal().clone());
        // All connection/signing code is below this durable T1. No permission
        // verdict or metadata descriptor is converted into an invented grant.
        let (op, att, frozen, fresh) = self.prepare_frozen(
            ctx, operation_id, logical_key, authority, frozen)?;
        self.observe_installed(op, att, frozen, installation, limits,
            if fresh { Some(ctx) } else { None }).await
    }

    /// Installed handler recovery. The target comes from original frozen data,
    /// not a newly supplied model target; credentials remain local to installer.
    pub fn original_target_id(&self, id: EntityId) -> Result<String> {
        Ok(self.query_input(id)?.2.binding.target_id)
    }

    pub async fn query_installed(&self, id: EntityId,
        installation: &crate::NativeSshInstallation, limits: NativeCommandLimits,
    ) -> Result<SshObservation> {
        let (op, att, frozen) = self.query_input(id)?;
        self.observe_installed(op, att, frozen, installation, limits, None).await
    }

    async fn observe_installed(&self, op: OperationRecord, att: AttemptRecord,
        frozen: FrozenInput, installation: &crate::NativeSshInstallation,
        mut limits: NativeCommandLimits, fresh: Option<&TaskInvocation>,
    ) -> Result<SshObservation> {
        use crate::native_transport::NativeSshError;
        let mode = if fresh.is_some() { "dispatch" } else { "query" };
        let command = Self::helper_command(&op, &frozen.binding, mode);
        let payload = if fresh.is_some() { serde_json::to_vec(&frozen.action)? } else { Vec::new() };
        let end = tokio::time::Instant::now().checked_add(limits.deadline)
            .filter(|_| !limits.deadline.is_zero());
        let observed = async {
            let end = end.ok_or(NativeSshError::InvalidLimits)?;
            if let Some(ctx) = fresh {
                if self.store.native_invocation_current(ctx).ok() != Some(true) {
                    return Err(NativeSshError::InvocationChanged);
                }
            }
            let session = tokio::time::timeout_at(end, installation.open_original(self.store,
                &frozen.binding, frozen.installation.as_ref(), limits.deadline)).await
                .map_err(|_| NativeSshError::Deadline)??;
            // No Store or Node lock crosses the opener/authentication await.
            // Durable cancellation/ancestor/generation change is observed
            // before a NEW exec; original recovery only queries an old receipt.
            if let Some(ctx) = fresh {
                if self.store.native_invocation_current(ctx).ok() != Some(true) {
                    return Err(NativeSshError::InvocationChanged);
                }
            }
            limits.deadline = end.saturating_duration_since(tokio::time::Instant::now());
            session.execute(command.as_bytes(), &payload, limits).await
        }.await.map_err(|error: NativeSshError| error.to_string());
        // Failed connection/credential/fencing after T1 is armed/unknown.
        // No false NeverDispatched and no implicit retry or alternate identity.
        self.record_observation(op, att, frozen, ObservedTransport::Native(observed))
    }

    pub async fn query(&self, id: EntityId, timeout_secs: u64) -> Result<SshObservation> {
        let (op, att, frozen) = self.query_input(id)?;
        self.observe(op, att, frozen, "query", timeout_secs).await
    }

    /// Reconcile the ORIGINAL already-armed operation over one explicitly
    /// authenticated native session. Trusted child/recovery API, not a model
    /// tool or permission grant: the caller authorizes this query, the stream
    /// acquisition and the exact signer. No action payload or T1/retry is sent.
    pub async fn query_native(
        &self, id: EntityId, session: NativeSshSession, limits: NativeCommandLimits,
    ) -> Result<SshObservation> {
        let (op, att, frozen) = self.query_input(id)?;
        ensure!(session.matches_binding(&frozen.binding)?, "native session binding mismatch");
        let command = Self::helper_command(&op, &frozen.binding, "query");
        // Native errors contain fixed, non-secret diagnostics only.
        let observed = session.execute(command.as_bytes(), &[], limits).await
            .map_err(|error| error.to_string());
        self.record_observation(op, att, frozen, ObservedTransport::Native(observed))
    }

    fn query_input(&self, id: EntityId) -> Result<(OperationRecord, AttemptRecord, FrozenInput)> {
        let op = self.store.external_operation(id)?;
        let att = self.store.external_operation_attempt(id)?;
        ensure!(
            op.operation_name == "ssh.native.v1"
                && (op.contract_ref.contract_digest == Digest::blake3(HELPER.as_bytes())
                    || op.contract_ref.contract_digest.0 == LEGACY_RECEIPT_V1
                    || op.contract_ref.contract_digest == Digest::blake3(PR127_RECEIPT_V1.as_bytes())),
            "unsupported receipt reader version"
        );
        let bytes = self.store.cas.read_object(&op.final_input_ref.digest)?;
        ensure!(
            Digest::blake3(&bytes) == op.input_digest,
            "frozen input corrupted"
        );
        let frozen: FrozenInput = serde_json::from_slice(&bytes)?;
        if let Some(selected) = &frozen.selection {
            selected.ensure_readable()?;
        }
        Ok((op, att, frozen))
    }

    fn helper_command(op: &OperationRecord, binding: &SshBinding, mode: &str) -> String {
        fn quote(s: &str) -> String {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
        format!("python3 -I -c {} {} {} {} {}", quote(HELPER),
            quote(&binding.receipt_root), quote(&op.operation_id.to_string()),
            quote(&op.input_digest.0), quote(mode))
    }

    async fn observe(
        &self,
        op: OperationRecord,
        att: AttemptRecord,
        frozen: FrozenInput,
        mode: &str,
        timeout: u64,
    ) -> Result<SshObservation> {
        let command = Self::helper_command(&op, &frozen.binding, mode);
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
        self.record_observation(op, att, frozen,
            ObservedTransport::OpenSsh(result.map_err(|error| error.to_string())))
    }

    fn record_observation(
        &self, op: OperationRecord, att: AttemptRecord, frozen: FrozenInput,
        observed: ObservedTransport,
    ) -> Result<SshObservation> {
        let (transport, native_transport, dispatch_error, is_native) = match observed {
            ObservedTransport::OpenSsh(Ok(value)) => (Some(value), None, None, false),
            ObservedTransport::OpenSsh(Err(error)) => (None, None, Some(error), false),
            ObservedTransport::Native(Ok(value)) => (None, Some(value), None, true),
            ObservedTransport::Native(Err(error)) => (None, None, Some(error), true),
        };
        let receipt = if let Some(native) = native_transport.as_ref() {
            // Drain reaches the ordered channel Close, not necessarily EOF:
            // RFC4254 permits Close without EOF. A complete JSON object, exact
            // identity and no interruption are required. Transport append
            // overflow is sticky and produces OutputLimit (or another error),
            // so a truncated prefix that happens to be valid JSON cannot pass.
            // This differs from receipt.stdout_truncated: that field describes
            // the original action output, not truncation of this helper reply.
            (native.interruption.is_none() && native.exec_accepted && native.channel_closed
                && native.exit == Some(NativeExit::Status { code: 0 }))
                .then(|| serde_json::from_slice::<SshReceipt>(&native.stdout).ok()).flatten()
        } else {
            transport.as_ref()
                .filter(|r| r.outcome == Some(ExecOutcome::Exited) && r.exit_code == 0)
                .and_then(|r| serde_json::from_str::<SshReceipt>(&r.stdout).ok())
        }.filter(|r| r.operation_id == op.operation_id && r.seal == op.input_digest.0);
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
        let raw = if is_native {
            self.seal(&(receipt.as_ref(), native_transport.as_ref(), dispatch_error.as_ref()),
                "ssh-native-observation-v1")?
        } else {
            // Preserve the historical observation encoding for the old path.
            self.seal(&(receipt.as_ref(), transport.as_ref(), dispatch_error.as_ref()),
                "ssh-observation")?
        };
        // The installed child exposes a bounded result plus a CAS reference to
        // the complete receipt. It does not recast an arbitrary exec as known
        // effects. Historical codecs/frozen inputs retain their old shape.
        let normalized = if frozen.installation.is_some() {
            let receipt_ref = receipt.as_ref().map(|receipt|
                self.seal(receipt, "ssh-native-helper-receipt-v1")).transpose()?;
            Some(self.seal(&serde_json::json!({
                "schema": "ssh.native-task-result.v1", "operation_id": op.operation_id,
                "receipt_ref": receipt_ref, "effect_status": effect,
                "state": receipt.as_ref().map(|r| &r.state),
                "exit_code": receipt.as_ref().and_then(|r| r.exit_code),
                "stdout_preview_b64": receipt.as_ref().map(|r| r.stdout_b64.chars().take(8192).collect::<String>()),
                "stderr_preview_b64": receipt.as_ref().map(|r| r.stderr_b64.chars().take(8192).collect::<String>()),
                "stdout_preview_truncated": receipt.as_ref().map(|r| r.stdout_truncated || r.stdout_b64.len() > 8192),
                "stderr_preview_truncated": receipt.as_ref().map(|r| r.stderr_truncated || r.stderr_b64.len() > 8192),
            }), "ssh-native-task-result-v1")?)
        } else { None };
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
            normalized_ref: normalized,
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
            native_transport,
            diagnostic: dispatch_error,
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn relative_cas_trust_reaches_ssh_after_store_reopen() {
        const CHILD: &str = "SHENKO_RELATIVE_CAS_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let cwd = std::env::current_dir().unwrap();
            let trust = cwd.join("known_hosts");
            std::fs::write(&trust, b"fixture public trust\n").unwrap();
            let store = Store::open("fixture.db".into(), "relative-cas".into()).unwrap();
            let binding = SshBinding::register(
                &store, "fixture".into(),
                SshTarget { host: "127.0.0.1".into(), port: 2222, user: "fixture".into(),
                    key_path: Some(cwd.join("unused-key")), known_hosts_path: Some(trust),
                    ..SshTarget::default() },
                "/fixture-receipts".into(),
            ).unwrap();
            drop(store);
            let reopened = Store::open("fixture.db".into(), "relative-cas".into()).unwrap();
            let result = binding.executor(&reopened).unwrap().run_with_input("ignored", b"", 5).await.unwrap();
            assert_eq!(result.outcome, Some(crate::executor::ExecOutcome::Unknown));
            return;
        }
        let root = std::env::temp_dir().join(format!("shenko-relative-cas-{}", EntityId::new()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh = bin.join("ssh");
        std::fs::write(&ssh, "#!/bin/sh\nprintf called >> \"$SHENKO_RELATIVE_CAS_MARKER\"\nexit 255\n").unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let marker = root.join("marker");
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "task_runner::tests::relative_cas_trust_reaches_ssh_after_store_reopen", "--nocapture"])
            .current_dir(&root).env(CHILD, "1").env("SHENKO_RELATIVE_CAS_MARKER", &marker)
            .env("PATH", &bin).output().unwrap();
        assert!(child.status.success(), "child failed: {}", String::from_utf8_lossy(&child.stderr));
        assert_eq!(std::fs::read(&marker).unwrap(), b"called");
    }
}

#[cfg(test)]
#[path = "native_query_tests.rs"]
mod native_query_tests;
