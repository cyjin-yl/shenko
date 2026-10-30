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
use shenko_node::Node;
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
use shenko_wasm::{
    InputFile, MountMode, MountSpec, OutputFile, WasiCancellation, WasiCommand, WasiRunControl, WasiRunOutcome,
    ToolMountProfile, WasmRuntime, resolve_tool, tool_module,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

const MAX_ARG_BYTES: usize = 4096;
const MAX_ARGS: usize = 64;
const MAX_ENV: usize = 64;
const MAX_STDIN_BYTES: usize = 64 * 1024;
/// The normalized result must fit the native settlement limit
/// (`shenko-store` reads normalized objects under a 512 KiB cap). Inline
/// previews allow worst-case JSON escaping (six bytes per input byte).
const MAX_STREAM_PREVIEW_BYTES: usize = 16 * 1024;
const MAX_TIMEOUT_SECS: u64 = 300;
const CONTRACT: &[u8] =
    b"shenko.wasi.exec/v2: execute digest-verified WASI preview1 bytes under the actual frozen \
      mount closure and explicit managed_home/read_only profile, preserve cancellation and \
      deadline outcomes, and retain unknown RW effects without replay or false settlement";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WasiExecInput {
    /// Tool name, or name@version when several versions are installed.
    tool: String,
    /// Explicit capability profile. The default keeps the managed home's
    /// declared permissions; read_only actually withdraws write capabilities.
    #[serde(default)]
    mount_profile: ToolMountProfile,
    /// Arguments after argv[0]; argv[0] is the installed tool's entry name.
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Base64 stdin bytes; omitted means an empty stream.
    #[serde(default)]
    stdin_b64: Option<String>,
    /// Guest execution deadline (C05 `WasiRunControl::deadline`): the guest
    /// is interrupted at a wasmtime epoch checkpoint after this budget. A
    /// guest blocked inside a blocking host call can outlive it — the
    /// deadline bounds guest-executed work, never claims it stopped early.
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
    mount_profile: ToolMountProfile,
    mounts: Vec<MountSpec>,
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

/// Bounded evidence of one guest stream: full size + full digest computed by
/// streaming the file, plus a bounded head preview (the only bytes ever
/// resident in memory).
#[derive(Debug, Clone)]
struct StreamEvidence {
    object: ObjectRef,
    preview: Vec<u8>,
    truncated: bool,
}

/// Raw guest observation produced on the execution thread.
#[derive(Debug)]
struct GuestRun {
    outcome: Option<WasiRunOutcome>,
    exit_code: Option<i32>,
    stdout: Option<StreamEvidence>,
    stderr: Option<StreamEvidence>,
    transport_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StreamReport {
    object: ObjectRef,
    /// Base64 head of the stream, capped at MAX_STREAM_PREVIEW_BYTES.
    preview_b64: String,
    preview_bytes: u64,
    truncated: bool,
}

impl StreamReport {
    fn from_evidence(evidence: &StreamEvidence) -> Self {
        Self {
            object: evidence.object.clone(),
            preview_b64: B64.encode(&evidence.preview),
            preview_bytes: evidence.preview.len() as u64,
            truncated: evidence.truncated,
        }
    }
}

#[derive(Debug, Serialize)]
struct WasiObservation {
    operation_id: EntityId,
    tool: ResolvedTool,
    mount_profile: ToolMountProfile,
    outcome: Option<String>,
    exit_code: Option<i32>,
    stdout: Option<StreamReport>,
    stderr: Option<StreamReport>,
    transport_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct WasiExecResult {
    tool: String,
    mount_profile: ToolMountProfile,
    version: String,
    digest: String,
    outcome: Option<String>,
    exit_code: Option<i32>,
    stdout_utf8: Option<String>,
    stderr_utf8: Option<String>,
    stdout_bytes: Option<u64>,
    stderr_bytes: Option<u64>,
    stdout_ref: Option<ObjectRef>,
    stderr_ref: Option<ObjectRef>,
    truncated: bool,
    /// Explicit TOOL_* / WASI_* reason when resolution or the guest failed.
    error: Option<String>,
}

/// Runs installed WASI preview1 tools against the managed home/mount closure.
/// Constructed with the same mounts the home manifest declares; the tool
/// inventory is reopened per execution so newly installed tools are visible,
/// every resolution binds this installation's own unit records and every
/// artifact digest is re-verified before it runs.
pub struct WasiExecHandler {
    store: Arc<Store>,
    mounts: Vec<MountSpec>,
    data_dir: PathBuf,
}

impl WasiExecHandler {
    pub fn new(store: Arc<Store>, mounts: Vec<MountSpec>, data_dir: PathBuf) -> Self {
        Self {
            store,
            mounts,
            data_dir,
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
            2,
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
        // never a silent fallback to a host binary. Resolution reads only
        // this installation's own node inventory — a dropped foreign tools/
        // directory has no effect because the records are unit facts. A
        // missing inventory resolves against zero installed tools, which
        // still produces the honest TOOL_UNSUPPORTED/TOOL_UNAVAILABLE answers.
        let node = Node::open_inspect(&self.data_dir.join("node")).map_err(PrepareError::Fault)?;
        let facts = match &node {
            Some(node) => node.tool_unit_facts().map_err(PrepareError::Fault)?,
            None => Vec::new(),
        };
        let (record, _) = resolve_tool(&facts, &input.tool)
            .map_err(|error| PrepareError::Refusal(error.to_string()))?;
        let node = node.ok_or_else(|| {
            PrepareError::Fault(anyhow::anyhow!(
                "no node installation at {}/node; run `shenko start` once",
                self.data_dir.display()
            ))
        })?;
        // Installation binding: the node must belong to this subject, the same
        // check node_sync::open_node enforces on the writable path. A
        // transplanted sibling installation's node/ cannot lend its tool
        // records into this home's mounts.
        let expected = self.expected_node_identity().map_err(PrepareError::Fault)?;
        let actual = node.local_identity().map_err(PrepareError::Fault)?;
        if actual != expected {
            return Err(PrepareError::Refusal(
                "INSTALLATION_IDENTITY_MISMATCH: node identity does not match \
                 this installation's subject binding; reconcile before exec"
                    .into(),
            ));
        }
        // Re-verify the exact artifact bytes against the retained digest on
        // every execution: the materialized copy is never trusted.
        // tool_module returns the *actual* inspection — its imports are what
        // the sealed execution record reports, not the authored descriptor.
        let (bytes, inspection) = tool_module(&node, &record).map_err(PrepareError::Fault)?;
        let resolved_tool = ResolvedTool {
            name: record.name,
            version: record.version,
            abi: record.abi.name().into(),
            digest: record.digest,
            bytes: bytes.len() as u64,
            entrypoint: record.entrypoint,
            imports: inspection.imports,
        };
        let frozen = FrozenInput {
            tool: input.tool,
            mount_profile: input.mount_profile,
            mounts: input.mount_profile.resolve(&self.mounts),
            args: input.args,
            env: input.env,
            stdin_len: stdin.len(),
            stdin_digest: Digest::blake3(&stdin),
            timeout_secs: input.timeout_secs.unwrap_or(60),
            resolved_tool,
        };
        Ok((frozen, bytes, stdin))
    }

    /// The subject-derived identity this installation's node must carry — the
    /// exact derivation `node_sync::open_node` uses for Initialize/Join.
    fn expected_node_identity(&self) -> Result<Option<shenko_node::Identity>> {
        let live = self.store.installation_binding()?;
        let current = Store::inspect_installation_binding(&self.data_dir.join("shenko.db"))?;
        ensure!(live == current,
            "INSTALLATION_IDENTITY_MISMATCH: managed home database no longer matches the live Store");
        Ok(live.subject.map(|subject| shenko_node::Identity {
            agent_id: subject.agent_id,
            branch_id: subject.branch_id,
            origin_proof: subject.origin_proof,
        }))
    }

    /// Retain every byte in CAS by streaming, plus a bounded text preview.
    fn stream_evidence(store: &Store, path: &PathBuf) -> Result<StreamEvidence> {
        let mut file = fs::File::open(path)?;
        let sealed = match store.cas.seal_reader(
            &mut file, "application/octet-stream", "wasi-exec-streams", "wasi-stream", 1,
        ) {
            Ok(sealed) => sealed,
            Err(error) => {
                store.close_external_writer();
                return Err(error.into());
            }
        };
        file.seek(SeekFrom::Start(0))?;
        let mut preview = Vec::new();
        file.take(MAX_STREAM_PREVIEW_BYTES as u64).read_to_end(&mut preview)?;
        Ok(StreamEvidence {
            truncated: sealed.object.bytes > preview.len() as u64,
            object: sealed.object,
            preview,
        })
    }

    /// The blocking WASI execution itself: temp files carry the streams so a
    /// bounded, streaming read-back reports exact sizes and digests without
    /// ever loading a full stream. The deadline is the real C05 guest control:
    /// `run_wasi_module` returns `WasiRunOutcome::DeadlineExceeded` after
    /// interrupting the guest at an epoch checkpoint, so this worker finishes
    /// and is always joined by the caller — no detached guest ownership.
    fn execute_blocking(
        store: Arc<Store>,
        bytes: Vec<u8>,
        frozen: FrozenInput,
        stdin: Vec<u8>,
        io_dir: PathBuf,
        cancel: WasiCancellation,
    ) -> Result<GuestRun> {
        let stdin_path = io_dir.join("stdin.bin");
        let stdout_path = io_dir.join("stdout.bin");
        let stderr_path = io_dir.join("stderr.bin");
        let run = (|| -> Result<WasiRunOutcome> {
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
            command.control = WasiRunControl {
                deadline: Some(Duration::from_secs(frozen.timeout_secs)),
                cancel: Some(cancel),
            };
            let mut runtime = WasmRuntime::with_mounts(frozen.mounts)?;
            runtime.run_wasi_module(&bytes, command)
        })();
        let stdout = Some(Self::stream_evidence(&store, &stdout_path)?);
        let stderr = Some(Self::stream_evidence(&store, &stderr_path)?);
        let (outcome, exit_code, transport_error) = match run {
            Ok(outcome) => {
                let exit_code = match outcome {
                    WasiRunOutcome::Exited(code) => Some(code),
                    _ => None,
                };
                (Some(outcome), exit_code, None)
            }
            Err(error) => (None, None, Some(error.to_string())),
        };
        Ok(GuestRun {
            outcome,
            exit_code,
            stdout,
            stderr,
            transport_error,
        })
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
            operation_name: "wasi.exec.v2".into(),
            input_schema_version: 2,
            output_schema_version: 2,
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
                locator: "builtin:wasi-exec-v2".into(),
            },
            symbol: "wasi.exec.execute".into(),
            schema: SchemaRef {
                name: "wasi-exec-observation-v2".into(),
                major: 2,
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
        let mount_profile = frozen.mount_profile;
        let all_read_only = frozen.mounts.iter().all(|mount| mount.mode == MountMode::ReadOnly);

        let input_ref = self.seal(&frozen, "wasi-exec-frozen-input")?;
        let bindings = ResourceBindings {
            provider: "wasi.exec.v2".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "managed_home_mounts_and_declared_streams".into(),
            extra: serde_json::json!({
                "tool": resolved_tool.name,
                "version": resolved_tool.version,
                "artifact_digest": resolved_tool.digest,
                "abi": resolved_tool.abi,
                "mount_profile": mount_profile,
                "mounts": frozen.mounts,
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
        // on its own thread inside the real WasmRuntime under the real C05
        // deadline control; the worker therefore always finishes and is
        // always joined — ownership of a finished guest is never abandoned
        // and the stream directory is removed only after the join.
        let io_dir = std::env::temp_dir().join(format!("shenko-wasi-io-{}", EntityId::new()));
        fs::create_dir_all(&io_dir)?;
        let stream_store = Arc::clone(&self.store);
        let io_dir_run = io_dir.clone();
        // C05's interruptible token: durable abort_request propagates into the
        // running guest's epoch checkpoints; the worker is still always
        // joined before the io dir is removed.
        let cancel = WasiCancellation::default();
        let worker_cancel = cancel.clone();
        let blocking = tokio::task::spawn_blocking(move || {
            Self::execute_blocking(stream_store, bytes, frozen, stdin, io_dir_run, worker_cancel)
        });
        let mut cancellation_read_error = None;
        let join_result = {
            loop {
                if blocking.is_finished() {
                    break blocking.await;
                }
                // Poll the durable task record: a committed abort_request
                // reaches the guest through the cancellation token.
                match self.store.external_task(invocation.task.task_id) {
                    Ok(task) if task.abort_request.is_some() => cancel.cancel(),
                    Ok(_) => {}
                    Err(error) => {
                        // Loss of durable cancellation authority must not be
                        // silently treated as permission to keep running.
                        // Request a stop, join, then preserve this primary
                        // error; the armed operation remains unreplayed.
                        cancellation_read_error = Some(error);
                        cancel.cancel();
                        break blocking.await;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        if let Some(error) = cancellation_read_error {
            return Err(error);
        }
        let guest = match join_result {
            Ok(Ok(guest)) => Some(guest),
            Ok(Err(error)) => return Err(error),
            Err(join) => Some(GuestRun {
                outcome: None,
                exit_code: None,
                stdout: None,
                stderr: None,
                transport_error: Some(format!("guest thread failed: {join}")),
            }),
        };
        let _ = fs::remove_dir_all(&io_dir);

        let outcome = guest.as_ref().and_then(|guest| guest.outcome).map(|outcome| match outcome {
            WasiRunOutcome::Exited(0) => OperationOutcome::Succeeded,
            WasiRunOutcome::Exited(_) => OperationOutcome::Failed,
            WasiRunOutcome::Cancelled => OperationOutcome::Cancelled,
            WasiRunOutcome::DeadlineExceeded => OperationOutcome::Interrupted,
        });
        let normalized = self.seal(
            &WasiExecResult {
                tool: resolved_tool.name.clone(),
                mount_profile,
                version: resolved_tool.version.clone(),
                digest: resolved_tool.digest.to_string(),
                outcome: guest.as_ref().and_then(|o| o.outcome.map(|v| format!("{v:?}"))),
                exit_code: guest.as_ref().and_then(|o| o.exit_code),
                stdout_utf8: guest.as_ref().and_then(|o| o.stdout.as_ref())
                    .map(|s| String::from_utf8_lossy(&s.preview).into_owned()),
                stderr_utf8: guest.as_ref().and_then(|o| o.stderr.as_ref())
                    .map(|s| String::from_utf8_lossy(&s.preview).into_owned()),
                stdout_bytes: guest.as_ref().and_then(|o| o.stdout.as_ref()).map(|s| s.object.bytes),
                stderr_bytes: guest.as_ref().and_then(|o| o.stderr.as_ref()).map(|s| s.object.bytes),
                stdout_ref: guest.as_ref().and_then(|o| o.stdout.as_ref()).map(|s| s.object.clone()),
                stderr_ref: guest.as_ref().and_then(|o| o.stderr.as_ref()).map(|s| s.object.clone()),
                truncated: guest.as_ref().is_some_and(|o| {
                    o.stdout.as_ref().is_some_and(|s| s.truncated)
                        || o.stderr.as_ref().is_some_and(|s| s.truncated)
                }),
                error: guest.as_ref().and_then(|o| o.transport_error.as_ref())
                    .map(|error| error.chars().take(1024).collect()),
            },
            "wasi-exec-result",
        )?;
        let observation = guest.map(|guest| WasiObservation {
            operation_id: invocation.operation_id,
            tool: resolved_tool.clone(),
            mount_profile,
            outcome: guest.outcome.map(|outcome| format!("{outcome:?}")),
            exit_code: guest.exit_code,
            stdout: guest.stdout.as_ref().map(StreamReport::from_evidence),
            stderr: guest.stderr.as_ref().map(StreamReport::from_evidence),
            transport_error: guest.transport_error,
        });
        let raw = self.seal(&observation, "wasi-exec-observation")?;
        let evidence = EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(invocation.operation_id),
            attempt_id: Some(attempt.attempt_id),
            source_id: "wasi.exec.v2".into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("2".into()),
            raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: if observation.is_some() {
                DecodeStatus::Decoded
            } else {
                DecodeStatus::Uninterpreted
            },
            observation_level: observation
                .as_ref()
                .and_then(|o| o.outcome.as_ref())
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
            // Process completion is not an effect assessment. Only a real
            // terminal observation under an entirely read-only capability
            // closure proves there was no external writable surface.
            if observation.as_ref().is_some_and(|o| o.outcome.is_some())
                && all_read_only
            {
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
        let refused: WasiExecInput = serde_json::from_value(invocation.input.clone())?;
        let tool = refused.tool;
        let mount_profile = refused.mount_profile;
        let frozen = FrozenInput {
            tool: tool.clone(),
            mount_profile,
            // No guest received capabilities after resolution was refused.
            mounts: vec![],
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
            provider: "wasi.exec.v2".into(),
            account_identity: String::new(),
            runtime_node: invocation.task_invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "tool_resolution".into(),
            extra: serde_json::json!({"tool": tool, "mount_profile": mount_profile, "mounts": []}),
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
                mount_profile,
                version: String::new(),
                digest: String::new(),
                outcome: None,
                exit_code: None,
                stdout_utf8: None,
                stderr_utf8: None,
                stdout_bytes: None,
                stderr_bytes: None,
                stdout_ref: None,
                stderr_ref: None,
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
            source_id: "wasi.exec.v2".into(),
            source_instance_id: invocation.task_invocation.node_id.to_string(),
            source_event_key: None,
            producer_epoch: Some(invocation.task_invocation.epoch.0),
            producer_schema_version: Some("2".into()),
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
            definition_version: 2,
            implementation_ref: ImplementationRef {
                plugin_id: Self::plugin_id(),
                artifact_digest: Digest::blake3(CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-wasi-exec/2".into(),
            authority_refs: vec!["wasm.managed_home.execute_installed_tool/v2".into()],
            prompt_section: Some(
                "wasi.exec/v2 运行已安装的 WASI preview1 工具（真实 WASM 模块在受管 \
                 挂载闭包内执行）：[delegate:wasi.exec:2] \
                 {\"tool\":\"名字\"或\"名字@版本\",\"args\":[...],\"env\":{\"K\":\"V\"},\
                 \"stdin_b64\":\"...\",\"timeout_secs\":60,\"mount_profile\":\"read_only\"}。\
                 read_only 会真实撤销所有挂载写权限，不按工具名猜测；省略此字段默认 \
                 managed_home 保留声明权限，RW effects 仍未知且不能 T2/自动重放。\
                 argv[0] 是已安装工具名；\
                 未安装的名字返回明确 TOOL_UNAVAILABLE，已声明但未移植的具名目标返回 \
                 TOOL_UNSUPPORTED，宿主工具不会被冒名顶替。截止/取消在 guest epoch \
                 checkpoint 生效；阻塞 host call 不承诺硬期限，worker 总会 join。"
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
            operation.task_id == task.task_id && operation.operation_name == "wasi.exec.v2",
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
                    source_id: "wasi.exec.recovery.v2".into(),
                    source_instance_id: self.store.capabilities.incarnation.to_string(),
                    source_event_key: Some(format!("controller-recovery:{}", attempt.attempt_id)),
                    producer_epoch: None,
                    producer_schema_version: Some("2".into()),
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
    use shenko_types::agent::CallContext;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shenko-wasi-task-{tag}-{}", EntityId::new()))
    }

    fn invocation(input: serde_json::Value) -> NativeTaskInvocation {
        let task_id = EntityId::new();
        NativeTaskInvocation {
            context: CallContext {
                caller_instance_id: EntityId::new(),
                role_revision: "native-wasi-exec/2".into(),
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
                software_generation: RuntimeGeneration::default(),
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
                definition_version: 2,
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
        let store = Arc::new(shenko_store::store::Store::open(
            dir.join("shenko.db"),
            dir.join("cas"),
        )?);
        let handler = WasiExecHandler::new(store, vec![], dir.clone());

        match handler.prepare(&invocation(serde_json::json!({"tool": "toybox"}))) {
            Err(PrepareError::Refusal(detail)) => {
                assert!(detail.contains("TOOL_UNSUPPORTED:toybox"), "{detail}")
            }
            other => panic!("expected TOOL_UNSUPPORTED refusal, got {other:?}"),
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
