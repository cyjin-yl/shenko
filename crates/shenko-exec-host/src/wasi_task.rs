//! `wasi.exec` — run an installed named WASI tool through the real WasmRuntime.
//!
//! The host process performs I/O plumbing and ledger bookkeeping; the actual
//! tool logic executes inside the WASM module, which is the honest execution
//! boundary required by SPEC §8.2/R008 (an H-level host relay never counts).
//! Uninstalled or unresolved named targets produce an explicit
//! `TOOL_UNSUPPORTED`/`TOOL_UNAVAILABLE` result rather than a silent miss.
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
    task::*,
};
use shenko_wasm::{InputFile, MountSpec, OutputFile, ToolStore, WasiCommand, WasmRuntime};
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Arc, time::Duration};

const MAX_ARG_BYTES: usize = 4096;
const MAX_ARGS: usize = 64;
const MAX_ENV: usize = 64;
const MAX_STDIN_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_TIMEOUT_SECS: u64 = 300;
const CONTRACT: &[u8] =
    b"shenko.wasi.exec/v1: resolve an installed WASI preview1 tool record, execute its exact \
      bytes under declared mounts, and report the true exit code";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WasiExecInput {
    /// Tool name, or name@version when several versions are installed.
    tool: String,
    /// Arguments after argv[0]; argv[0] is the installed tool's entry name.
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Base64 stdin bytes; omitted means an empty stream.
    #[serde(default)]
    stdin_b64: Option<String>,
    /// Host-side observation deadline. A guest that outlives it is UNKNOWN,
    /// not failed: the host cannot prove the guest stopped (C05 run controls
    /// land separately).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Resolution refused the tool before any arming — an explicit, recorded
/// answer for the model. Distinct from genuine store/IO faults which are
/// handler errors worth parking for.
#[derive(Debug)]
enum PrepareError {
    Refusal(String),
    Fault(anyhow::Error),
}

#[derive(Debug, Serialize)]
struct FrozenInput {
    tool: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    stdin_len: usize,
    stdin_digest: Digest,
    timeout_secs: u64,
    resolved_tool: ResolvedTool,
}

/// The exact artifact identity the operation bound to, sealed into the
/// operation's final input so a later registry change cannot rewrite what ran.
#[derive(Debug, Clone, Serialize)]
struct ResolvedTool {
    name: String,
    version: String,
    abi: String,
    digest: Digest,
    bytes: u64,
    entrypoint: String,
    imports: Vec<String>,
}

/// Raw guest observation produced on the execution thread.
#[derive(Debug)]
struct GuestRun {
    exit_code: Option<i32>,
    stdout_b64: Option<String>,
    stderr_b64: Option<String>,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
    truncated: bool,
    transport_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WasiObservation {
    operation_id: EntityId,
    tool: ResolvedTool,
    exit_code: Option<i32>,
    stdout_b64: Option<String>,
    stderr_b64: Option<String>,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
    truncated: bool,
    transport_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WasiExecResult {
    tool: String,
    version: String,
    digest: String,
    exit_code: Option<i32>,
    stdout_utf8: Option<String>,
    stderr_utf8: Option<String>,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
    truncated: bool,
    /// Explicit TOOL_* / WASI_* reason when resolution or the guest failed.
    error: Option<String>,
}

/// Runs installed WASI preview1 tools against the managed home/mount closure.
/// Constructed with the same mounts the home manifest declares; the tool
/// store is reopened per execution so newly installed tools are visible and
/// every artifact digest is re-verified before it runs.
pub struct WasiExecHandler {
    store: Arc<Store>,
    mounts: Vec<MountSpec>,
    tools_dir: PathBuf,
}

impl WasiExecHandler {
    pub fn new(store: Arc<Store>, mounts: Vec<MountSpec>, tools_dir: PathBuf) -> Self {
        Self {
            store,
            mounts,
            tools_dir,
        }
    }

    fn plugin_id() -> EntityId {
        "19c7bf88-f365-468d-ae79-92f60021ceb5"
            .parse()
            .expect("fixed built-in plugin UUID")
    }

    fn seal<T: Serialize>(&self, value: &T, schema: &str) -> Result<ObjectRef> {
        match self.store.cas.seal_object(
            &serde_json::to_vec(value)?,
            "application/json",
            "wasi-exec-effects",
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

    /// Validate the invocation input, resolve the tool record and return the
    /// frozen input plus the module bytes. Resolution failures are `Refusal`
    /// (recorded as the task's explicit result); store/IO faults are `Fault`.
    fn prepare(
        &self,
        invocation: &NativeTaskInvocation,
    ) -> std::result::Result<(FrozenInput, Vec<u8>, Vec<u8>), PrepareError> {
        let parsed = (|| -> Result<(WasiExecInput, Vec<u8>)> {
            let input: WasiExecInput = serde_json::from_value(invocation.input.clone())?;
            ensure!(!input.tool.is_empty(), "wasi.exec input requires `tool`");
            ensure!(input.args.len() <= MAX_ARGS, "wasi.exec args exceed {MAX_ARGS}");
            for arg in &input.args {
                ensure!(
                    arg.len() <= MAX_ARG_BYTES,
                    "wasi.exec arg exceeds {MAX_ARG_BYTES} bytes"
                );
            }
            ensure!(
                input.env.len() <= MAX_ENV,
                "wasi.exec env exceeds {MAX_ENV} entries"
            );
            let stdin = match &input.stdin_b64 {
                Some(b64) => B64.decode(b64).map_err(|e| anyhow::anyhow!("stdin_b64: {e}"))?,
                None => Vec::new(),
            };
            ensure!(
                stdin.len() <= MAX_STDIN_BYTES,
                "wasi.exec stdin exceeds {MAX_STDIN_BYTES}"
            );
            let timeout_secs = input.timeout_secs.unwrap_or(60);
            ensure!(
                (1..=MAX_TIMEOUT_SECS).contains(&timeout_secs),
                "timeout must be 1..={MAX_TIMEOUT_SECS} seconds"
            );
            Ok((input, stdin))
        })();
        let (input, stdin) = parsed.map_err(PrepareError::Fault)?;

        // Resolution errors are explicit and recorded as the task's result,
        // never a silent fallback to a host binary.
        let tools = ToolStore::open(&self.tools_dir).map_err(PrepareError::Fault)?;
        let (record, bytes) = tools
            .resolve(&input.tool)
            .map_err(|error| PrepareError::Refusal(error.to_string()))?;
        let resolved_tool = ResolvedTool {
            name: record.name,
            version: record.version,
            abi: record.abi.name().into(),
            digest: record.digest,
            bytes: record.bytes,
            entrypoint: record.entrypoint,
            imports: record.imports,
        };
        let frozen = FrozenInput {
            tool: input.tool,
            args: input.args,
            env: input.env,
            stdin_len: stdin.len(),
            stdin_digest: Digest::blake3(&stdin),
            timeout_secs: input.timeout_secs.unwrap_or(60),
            resolved_tool,
        };
        Ok((frozen, bytes, stdin))
    }

    /// The blocking WASI execution itself: temp files carry the streams so a
    /// bounded read-back can report exact byte counts and truncation.
    fn execute_blocking(
        mounts: Vec<MountSpec>,
        bytes: Vec<u8>,
        frozen: &FrozenInput,
        stdin: Vec<u8>,
        io_dir: PathBuf,
    ) -> GuestRun {
        let stdin_path = io_dir.join("stdin.bin");
        let stdout_path = io_dir.join("stdout.bin");
        let stderr_path = io_dir.join("stderr.bin");
        let run = (|| -> Result<i32> {
            fs::write(&stdin_path, &stdin)?;
            let stdout_file = fs::File::create(&stdout_path)?;
            let stderr_file = fs::File::create(&stderr_path)?;
            let mut command = WasiCommand::new(
                InputFile::new(fs::File::open(&stdin_path)?),
                OutputFile::new(stdout_file),
                OutputFile::new(stderr_file),
            );
            let mut arguments = Vec::with_capacity(frozen.args.len() + 1);
            arguments.push(frozen.resolved_tool.name.clone());
            arguments.extend(frozen.args.iter().cloned());
            command.arguments = arguments;
            command.environment = frozen
                .env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let mut runtime = WasmRuntime::with_mounts(mounts)?;
            runtime.run_wasi_module(&bytes, command)
        })();
        let read = |path: &PathBuf| -> (Option<String>, Option<u64>, bool) {
            match fs::read(path) {
                Ok(data) => {
                    let total = data.len() as u64;
                    let truncated = total > MAX_OUTPUT_BYTES as u64;
                    let kept = if truncated {
                        &data[..MAX_OUTPUT_BYTES]
                    } else {
                        &data[..]
                    };
                    (Some(B64.encode(kept)), Some(total), truncated)
                }
                Err(_) => (None, None, false),
            }
        };
        let (stdout_b64, stdout_bytes, out_trunc) = read(&stdout_path);
        let (stderr_b64, stderr_bytes, err_trunc) = read(&stderr_path);
        let (exit_code, transport_error) = match run {
            Ok(code) => (Some(code), None),
            Err(error) => (None, Some(error.to_string())),
        };
        GuestRun {
            exit_code,
            stdout_b64,
            stderr_b64,
            stdout_bytes,
            stderr_bytes,
            truncated: out_trunc || err_trunc,
            transport_error,
        }
    }

    fn new_operation(
        &self,
        invocation: &NativeTaskInvocation,
        input_ref: ObjectRef,
        resource: ObjectRef,
        acceptance: ObjectRef,
        retry: ObjectRef,
        version: &VersionRef,
    ) -> (OperationRecord, AttemptRecord) {
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let contract_digest = Digest::blake3(CONTRACT);
        let operation = OperationRecord {
            record_schema_version: 1,
            operation_id: invocation.operation_id,
            record_revision: RecordRevision(0),
            task_id: invocation.task_invocation.task_id,
            logical_key: "native-handler".into(),
            operation_name: "wasi.exec.v1".into(),
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
            authority_ref: invocation.authority.clone(),
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
        let attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: invocation.operation_id,
            attempt_no: 1,
            invocation_id: invocation.task_invocation.invocation_id,
            worker_id: invocation.task_invocation.worker_id,
            node_id: invocation.task_invocation.node_id,
            epoch: invocation.task_invocation.epoch,
            implementation_ref: version.clone(),
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        (operation, attempt)
    }

    fn version_ref() -> VersionRef {
        let contract_digest = Digest::blake3(CONTRACT);
        VersionRef {
            artifact: ArtifactRef {
                digest: contract_digest.clone(),
                locator: "builtin:wasi-exec-v1".into(),
            },
            symbol: "wasi.exec.execute".into(),
            schema: SchemaRef {
                name: "wasi-exec-observation-v1".into(),
                major: 1,
                digest: contract_digest,
            },
        }
    }

    async fn run(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        let (frozen, bytes, stdin) = match self.prepare(&invocation) {
            Ok(parts) => parts,
            Err(PrepareError::Refusal(detail)) => {
                return self.resolve_failure(invocation, detail).await;
            }
            Err(PrepareError::Fault(error)) => return Err(error),
        };
        let resolved_tool = frozen.resolved_tool.clone();

        let input_ref = self.seal(&frozen, "wasi-exec-frozen-input")?;
        let bindings = ResourceBindings {
            provider: "wasi.exec.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "managed_home_mounts_and_declared_streams".into(),
            extra: serde_json::json!({
                "tool": resolved_tool.name,
                "version": resolved_tool.version,
                "artifact_digest": resolved_tool.digest,
                "abi": resolved_tool.abi,
                "mounts": self.mounts.iter().map(|mount| serde_json::json!({
                    "guest_path": mount.guest_path,
                    "mode": format!("{:?}", mount.mode),
                })).collect::<Vec<_>>(),
            }),
        };
        let resource = self.seal(&bindings, "wasi-exec-binding")?;
        let acceptance = self.seal(
            &"guest returned an exit code under the declared mounts; \
              resolution refusals carry an explicit TOOL_* reason",
            "wasi-exec-acceptance",
        )?;
        let retry = self.seal(
            &"an armed guest run without evidence remains unknown; no automatic replay",
            "wasi-exec-retry",
        )?;
        let version = Self::version_ref();
        let (mut operation, mut attempt) = self.new_operation(
            &invocation,
            input_ref,
            resource,
            acceptance,
            retry,
            &version,
        );
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }

        // T1 commit precedes guest execution, like host.printf. The guest runs
        // on its own thread inside the real WasmRuntime; the deadline bounds
        // host observation only and does not claim the guest stopped.
        let io_dir = std::env::temp_dir().join(format!("shenko-wasi-io-{}", EntityId::new()));
        fs::create_dir_all(&io_dir)?;
        let mounts = self.mounts.clone();
        let timeout_secs = frozen.timeout_secs;
        let io_dir_run = io_dir.clone();
        let blocking = tokio::task::spawn_blocking(move || {
            Self::execute_blocking(mounts, bytes, &frozen, stdin, io_dir_run)
        });
        let outcome_result =
            tokio::time::timeout(Duration::from_secs(timeout_secs), blocking).await;

        // A join failure is a host-side fault we did observe; an elapsed
        // deadline is genuine UNKNOWN — the guest may still be running.
        let guest = match outcome_result {
            Ok(Ok(guest)) => Some(guest),
            Ok(Err(join)) => Some(GuestRun {
                exit_code: None,
                stdout_b64: None,
                stderr_b64: None,
                stdout_bytes: None,
                stderr_bytes: None,
                truncated: false,
                transport_error: Some(format!("guest thread failed: {join}")),
            }),
            Err(_) => None,
        };
        let _ = fs::remove_dir_all(&io_dir);

        let observation = guest.map(|guest| WasiObservation {
            operation_id: invocation.operation_id,
            tool: resolved_tool.clone(),
            exit_code: guest.exit_code,
            stdout_b64: guest.stdout_b64,
            stderr_b64: guest.stderr_b64,
            stdout_bytes: guest.stdout_bytes,
            stderr_bytes: guest.stderr_bytes,
            truncated: guest.truncated,
            transport_error: guest.transport_error,
        });
        let outcome = observation.as_ref().map(|obs| {
            if obs.exit_code == Some(0) {
                OperationOutcome::Succeeded
            } else {
                OperationOutcome::Failed
            }
        });
        let raw = self.seal(&observation, "wasi-exec-observation")?;
        let normalized = self.seal(
            &WasiExecResult {
                tool: resolved_tool.name.clone(),
                version: resolved_tool.version.clone(),
                digest: resolved_tool.digest.to_string(),
                exit_code: observation.as_ref().and_then(|o| o.exit_code),
                stdout_utf8: observation.as_ref().and_then(|o| {
                    o.stdout_b64
                        .as_deref()
                        .and_then(|b| B64.decode(b).ok())
                        .map(|v| String::from_utf8_lossy(&v).into_owned())
                }),
                stderr_utf8: observation.as_ref().and_then(|o| {
                    o.stderr_b64
                        .as_deref()
                        .and_then(|b| B64.decode(b).ok())
                        .map(|v| String::from_utf8_lossy(&v).into_owned())
                }),
                stdout_bytes: observation.as_ref().and_then(|o| o.stdout_bytes),
                stderr_bytes: observation.as_ref().and_then(|o| o.stderr_bytes),
                truncated: observation.as_ref().is_some_and(|o| o.truncated),
                error: observation
                    .as_ref()
                    .and_then(|o| o.transport_error.clone()),
            },
            "wasi-exec-result",
        )?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "wasi.exec.v1".into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: if observation.is_some() {
                DecodeStatus::Decoded
            } else {
                DecodeStatus::Uninterpreted
            },
            observation_level: observation
                .as_ref()
                .map(|_| ObservationLevel::RemoteCompleted),
            normalized_ref: Some(normalized),
            observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        self.store.record_external_evidence(
            invocation.operation_id,
            attempt.attempt_id,
            evidence,
            outcome,
            if observation.is_some() {
                EffectStatus::None
            } else {
                EffectStatus::Unknown
            },
        )
    }

    /// Record an explicit `TOOL_*` resolution refusal as the task's real
    /// outcome. The model learns the tool is unavailable/unsupported instead
    /// of the task silently parking forever.
    async fn resolve_failure(
        &self,
        invocation: NativeTaskInvocation,
        detail: String,
    ) -> Result<OperationRecord> {
        let tool = serde_json::from_value::<serde_json::Value>(invocation.input.clone())
            .ok()
            .and_then(|value| value.get("tool").and_then(|t| t.as_str()).map(String::from))
            .unwrap_or_else(|| "(unparsed)".into());
        let frozen = FrozenInput {
            tool: tool.clone(),
            args: vec![],
            env: BTreeMap::new(),
            stdin_len: 0,
            stdin_digest: Digest::blake3(b""),
            timeout_secs: 0,
            resolved_tool: ResolvedTool {
                name: tool.clone(),
                version: String::new(),
                abi: String::new(),
                digest: Digest::blake3(b""),
                bytes: 0,
                entrypoint: String::new(),
                imports: vec![],
            },
        };
        let input_ref = self.seal(&frozen, "wasi-exec-frozen-input")?;
        let bindings = ResourceBindings {
            provider: "wasi.exec.v1".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "tool_resolution".into(),
            extra: serde_json::json!({"tool": tool}),
        };
        let resource = self.seal(&bindings, "wasi-exec-binding")?;
        let acceptance = self.seal(&"explicit resolution refusal", "wasi-exec-acceptance")?;
        let retry = self.seal(&"no execution attempted", "wasi-exec-retry")?;
        let version = Self::version_ref();
        let (mut operation, mut attempt) = self.new_operation(
            &invocation,
            input_ref,
            resource,
            acceptance,
            retry,
            &version,
        );
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation,
            &mut operation,
            &mut attempt,
        )? {
            return Ok(existing);
        }
        let raw = self.seal(
            &serde_json::json!({
                "operation_id": invocation.operation_id,
                "tool": tool,
                "error": detail,
                "executed": false,
            }),
            "wasi-exec-observation",
        )?;
        let normalized = self.seal(
            &WasiExecResult {
                tool: tool.clone(),
                version: String::new(),
                digest: String::new(),
                exit_code: None,
                stdout_utf8: None,
                stderr_utf8: None,
                stdout_bytes: None,
                stderr_bytes: None,
                truncated: false,
                error: Some(detail),
            },
            "wasi-exec-result",
        )?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "wasi.exec.v1".into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
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
            invocation.operation_id,
            attempt.attempt_id,
            evidence,
            Some(OperationOutcome::Failed),
            EffectStatus::None,
        )
    }
}

#[async_trait]
impl NativeTaskHandler for WasiExecHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "wasi.exec".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-wasi-exec/1".into(),
            authority_refs: vec!["wasm.managed_home.execute_installed_tool/v1".into()],
            prompt_section: Some(
                "wasi.exec/v1 运行已安装的 WASI preview1 工具（真实 WASM 模块在受管 \
                 挂载闭包内执行）：[delegate:wasi.exec:1] \
                 {\"tool\":\"名字\"或\"名字@版本\",\"args\":[...],\"env\":{\"K\":\"V\"},\
                 \"stdin_b64\":\"...\",\"timeout_secs\":60}。argv[0] 是已安装工具名；\
                 未安装的名字返回明确 TOOL_UNAVAILABLE，已声明但未移植的具名目标返回 \
                 TOOL_UNSUPPORTED，宿主工具不会被冒名顶替。宿主观察截止只证明 \
                 host 侧观察超时，不证明 guest 停止。"
                    .into(),
            ),
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
            operation.task_id == task.task_id && operation.operation_name == "wasi.exec.v1",
            "original_operation_contract_mismatch"
        );
        // A guest run has no durable provider handle. A controller restart
        // mid-attempt is not proof the guest never ran: keep the uncertainty
        // as E1 and never execute a second attempt during reconcile.
        if operation.state == OperationState::InFlight {
            let attempt = self.store.external_operation_attempt(operation_id)?;
            if attempt.dispatch_stage != AttemptDispatchStage::Ended {
                let raw = self.seal(
                    &serde_json::json!({
                        "operation_id": operation_id,
                        "attempt_id": attempt.attempt_id,
                        "observation": "controller recovered without a conclusive guest result",
                        "guest_stop_proven": false
                    }),
                    "wasi-exec-recovery",
                )?;
                let evidence = EvidenceReceipt {
                    record_schema_version: 1,
                    receipt_id: EntityId::new(),
                    operation_id: Some(operation_id),
                    attempt_id: Some(attempt.attempt_id),
                    source_id: "wasi.exec.recovery".into(),
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
        Ok(operation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_store::store::Store;
    use shenko_types::agent::CallContext;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shenko-wasi-task-{tag}-{}", EntityId::new()))
    }

    fn invocation(input: serde_json::Value) -> NativeTaskInvocation {
        let task_id = EntityId::new();
        NativeTaskInvocation {
            context: CallContext {
                caller_instance_id: EntityId::new(),
                role_revision: "native-wasi-exec/1".into(),
                task_id: Some(task_id),
                expected_task_revision: Some(1),
                invocation_id: Some(EntityId::new()),
                controller_epoch: ControllerEpoch(1),
                software_generation: RuntimeGeneration::default(),
                authority_refs: vec![],
                trace_id: EntityId::new(),
            },
            task_invocation: shenko_store::TaskInvocation {
                task_id,
                task_revision: TaskRevision(1),
                record_revision: RecordRevision(1),
                invocation_id: EntityId::new(),
                epoch: ControllerEpoch(1),
                worker_id: EntityId::new(),
                node_id: EntityId::new(),
            },
            task: TaskRecord {
                record_schema_version: 1,
                task_id,
                task_revision: TaskRevision(1),
                record_revision: RecordRevision(1),
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                owner: TaskOwner::Conversation {
                    id: EntityId::new(),
                },
                background: false,
                kind: "wasi.exec".into(),
                definition_version: 1,
                implementation_ref: ImplementationRef {
                    plugin_id: EntityId::new(),
                    artifact_digest: Digest::blake3(CONTRACT),
                    registry_revision: 1,
                },
                input_ref: ObjectRef {
                    digest: Digest::blake3(b"input"),
                    bytes: 5,
                    media_type: "application/json".into(),
                    store_namespace: "test".into(),
                    locator: "fixture".into(),
                    codec: SchemaRef {
                        name: "fixture".into(),
                        major: 1,
                        digest: Digest::blake3(b"codec"),
                    },
                },
                state: TaskState::Pending {
                    checkpoint_ref: None,
                },
                abort_request: None,
                memos: serde_json::Map::new(),
                residual_case_ids: vec![],
                created_by_step_id: None,
                dispatch_eligibility: DispatchEligibility::Ready,
                no_progress_crashes: 0,
                first_run_at: None,
                terminal_at: None,
                created_commit: CommitRef::new(EntityId::new(), LedgerSeq::ZERO),
                updated_commit: CommitRef::new(EntityId::new(), LedgerSeq::ZERO),
            },
            operation_id: EntityId::new(),
            authority: AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(1),
                decision_ref: EntityId::new(),
            },
            input,
        }
    }

    /// Resolution failures are classified as explicit TOOL_* refusals (the
    /// model's real answer), while malformed inputs are faults: a declared
    /// named target with no artifact reports TOOL_UNSUPPORTED; oversized argv
    /// and undecodable stdin fail validation before any execution.
    #[test]
    fn prepare_classifies_tool_refusal_versus_input_faults() -> Result<()> {
        let dir = temp_dir("prepare");
        fs::create_dir_all(&dir)?;
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas"))?);
        let handler = WasiExecHandler::new(store, vec![], dir.join("tools"));

        match handler.prepare(&invocation(serde_json::json!({"tool": "toybox"}))) {
            Err(PrepareError::Refusal(detail)) => {
                assert!(detail.contains("TOOL_UNSUPPORTED:toybox"), "{detail}")
            }
            other => panic!("expected TOOL_UNSUPPORTED refusal, got {other:?}"),
        }
        match handler.prepare(&invocation(serde_json::json!({"tool": "nonesuch"}))) {
            Err(PrepareError::Refusal(detail)) => {
                assert!(detail.contains("TOOL_UNAVAILABLE"), "{detail}")
            }
            other => panic!("expected TOOL_UNAVAILABLE refusal, got {other:?}"),
        }
        let oversized = "x".repeat(MAX_ARG_BYTES + 1);
        match handler.prepare(&invocation(serde_json::json!({
            "tool": "fixture", "args": [oversized]
        }))) {
            Err(PrepareError::Fault(_)) => {}
            other => panic!("expected bounds fault, got {other:?}"),
        }
        match handler.prepare(&invocation(serde_json::json!({
            "tool": "fixture", "stdin_b64": "!!!"
        }))) {
            Err(PrepareError::Fault(_)) => {}
            other => panic!("expected base64 fault, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }
}
