//! Installed native task authority for SSH exec/PTY and seat observation —
//! docs/specs/remote-seats.md#spec-11, docs/specs/remote-seats.md#spec-17, docs/specs/remote-seats.md#spec-35-5, docs/specs/remote-seats.md#spec-41-11.
//!
//! Seven native children, all wired to the same runtime the binary installs:
//!
//! | kind | handler | what it really does |
//! | --- | --- | --- |
//! | `ssh.exec` | [`SshExecHandler`] | one exact remote command over the PR127 native transport |
//! | `ssh.pty` | [`SshPtyHandler`] | opens a live PTY seat, registers it durably, attaches it to the [`SeatRuntime`] |
//! | `omp.seat` | [`OmpSeatHandler`] | spawns a managed OMP seat, registers it at its session anchor, attaches it to the [`SeatRuntime`] |
//! | `claude.seat` | [`ClaudeSeatHandler`] | spawns a managed Claude Code seat, registers it at its session anchor, attaches it to the [`SeatRuntime`] |
//! | `seat.observe` | [`SeatObserveHandler`] | observes one attached seat; questions become durable before the observation returns |
//! | `seat.respond` | [`SeatRespondHandler`] | answers one durable question with lease preflight + compare-and-swap decision |
//! | `seat.fleet` | [`SeatFleetHandler`] | bounded read-only inventory of durable seats and observed transports |
//!
//! Every handler follows the same at-most-once contract as the other native
//! children: freeze the input to CAS, arm the exact operation id, perform
//! the effect once, record the observation as evidence. Reconcile is
//! query-only; a crash after T1 never re-sends a command, re-opens a PTY or
//! re-answers a question.
//!
//! The frozen input carries the complete `SshBinding` (host, port, user,
//! absolute key path, CAS-sealed known_hosts reference). A binding is a
//! secret *handle*, not secret *bytes*: the private key file is read locally
//! by this installer at effect time and is never sealed into any receipt.

use crate::native_transport::{
    NativeCommandLimits, NativeCommandObservation, NativeInterruption, NativePtyProgram,
    NativePtyRequest, NativeSshTarget,
};
use crate::seat::runtime::{SeatAttachment, SeatRespondRequest, SeatRuntime};
use crate::seat::{SeatAdapter, SeatAdapterId, SeatAnswer, SeatHandle};
use crate::SshBinding;
use anyhow::{Result, ensure};
use async_trait::async_trait;
use shenko_store::{SeatAnswerDecisionState, SeatLeaseRequest, Store};
use shenko_subject::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{
    ids::*,
    object::*,
    operation::{EffectStatus, OperationOutcome, OperationRecord},
    task::{ImplementationRef, TaskRecord},
};
use std::sync::Arc;
use std::time::Duration;
mod support;

const EXEC_CONTRACT: &[u8] = b"shenko.ssh.exec/v1: one exact remote command over the registered-CAS trust binding and an owned stream; the installer resolves the exact selected key locally; remote exit255 is a real status here; recovery queries only the original observation, never resends";
const PTY_CONTRACT: &[u8] = b"shenko.ssh.pty/v1: one live PTY seat over the registered-CAS trust binding; the seat record, the v2 writer lease and the runtime attachment are recorded under the trusted operation id; recovery reads the durable seat facts, never re-opens the PTY";
const OBSERVE_CONTRACT: &[u8] = b"shenko.seat.observe/v1: one observation of an attached seat; every reported question is durable before the observation returns; recovery re-observes live seats or reads durable question facts only";
const FLEET_CONTRACT: &[u8] = b"shenko.seat.fleet/v1: at most 32 seats with owning task identities and an optional task filter plus an explicit truncation flag; durable records and open-question counts share one query; transport liveness is observed without attach or writer leases; unknown is distinct from stopped; recovery may repeat this read-only observation";
const RESPOND_CONTRACT: &[u8] = b"shenko.seat.respond/v2: one durable answer decision for one open seat question, in the SPEC 41.11 order: the decision is committed first as a compare-and-swap on the observed question revision, seat incarnation, task revision and writer epoch; the exact effect bytes are planned and re-checked against the live state; the effect is armed with its digest before anything is written; the transport runs outside any transaction; the closing evidence decides answered, answer_unknown, not_sent or withdrawn. A refused or stale decision never writes, and an unknown effect is never replayed";

fn plugin_id(constant: &str) -> EntityId {
    constant.parse().expect("fixed seat plugin UUID")
}

const EXEC_PLUGIN: &str = "7a1d3f6e-2b8c-4e5a-9d0f-6c1b3e5a7d9f";
const PTY_PLUGIN: &str = "8b2e4a7f-3c9d-4f6b-8e1a-7d2c4f6b8e1a";
const OBSERVE_PLUGIN: &str = "9c3f5b8a-4d0e-4a7c-9f2b-8e3d5a7c9f2b";
const FLEET_PLUGIN: &str = "7db78fa0-4919-40f2-90a6-097c8b79c349";
const RESPOND_PLUGIN: &str = "0d4a6c9b-5e1f-4b8d-8a3c-9f4e6b8d0a3c";

// ---------------------------------------------------------------------------
// shared input pieces
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecLimitsInput {
    deadline_ms: Option<u64>,
    shutdown_grace_ms: Option<u64>,
    stdout_bytes: Option<usize>,
    stderr_bytes: Option<usize>,
}

// ---------------------------------------------------------------------------
// ssh.exec
// ---------------------------------------------------------------------------

/// Installed `ssh.exec` authority — one exact remote command per operation.
pub struct SshExecHandler {
    store: Arc<Store>,
}

impl SshExecHandler {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecInput {
    binding: SshBinding,
    command: String,
    #[serde(default)]
    stdin_b64: Option<String>,
    #[serde(default)]
    limits: Option<ExecLimitsInput>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecFrozen {
    schema_version: u32,
    binding: SshBinding,
    command: String,
    stdin_b64: String,
    deadline_ms: u64,
    shutdown_grace_ms: u64,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

enum ExecObservation {
    Executed(Result<NativeCommandObservation, crate::native_transport::NativeSshError>),
    Refused { error: String },
}

#[async_trait]
impl NativeTaskHandler for SshExecHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(EXEC_CONTRACT);
        NativeTaskDescriptor {
            kind: "ssh.exec".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(EXEC_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-ssh-exec/1".into(),
            authority_refs: vec!["ssh.exec.one_exact_command/v1".into()],
            prompt_section: Some(
                "ssh.exec/v1 是已安装的一次一命令远程执行 child。用 [delegate:ssh.exec:1] {\"binding\":{...已注册绑定...},\"command\":\"精确的远程命令\",\"stdin_b64\":可选,\"limits\":{可选}} 执行；binding 必须来自已注册的 CAS 信任，command 非空、不含 NUL。远程 exit255 在这里是真实退出状态，不是本地 ssh 进程失败；observation 的 exit 是远程自报状态，不等于应用后置条件通过。连接/认证/绑定的 typed 错误没有远端效果；execution 之后 unknown 只按原回执查询，绝不重发。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(&self.store, &invocation, "ssh.exec", 1, plugin_id(EXEC_PLUGIN))?;
        let input: ExecInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = freeze_exec(&self.store, input)?;
        let input_ref = support::seal(&self.store, &frozen, "ssh-exec-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(EXEC_PLUGIN),
            Digest::blake3(EXEC_CONTRACT),
            "ssh.exec.v1",
            input_ref,
            serde_json::json!({
                "provider": "ssh.native-transport.v1",
                "target_id": frozen.binding.target_id,
                "account_identity": frozen.binding.target.user,
                "trust_digest": frozen.binding.known_hosts.digest,
            }),
            "one exact remote command executed over the pinned trust binding; complete means an accepted, closed exchange reported a remote exit; missing or contradictory completion remains unknown, and a reported exit does not prove application postconditions",
            "query only the original observation receipt under its frozen operation; unknown stays unknown, never resend the command",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        // One effect site. The key file is read locally by this installer;
        // no private bytes are sealed into any receipt.
        let observation = match support::connect_session(&self.store, &frozen.binding, Duration::from_millis(frozen.deadline_ms)).await {
            Ok(session) => {
                let command = frozen.command.clone().into_bytes();
                let stdin = support::decode_b64(&frozen.stdin_b64)?;
                let limits = NativeCommandLimits {
                    deadline: Duration::from_millis(frozen.deadline_ms),
                    shutdown_grace: Duration::from_millis(frozen.shutdown_grace_ms),
                    stdout_bytes: frozen.stdout_bytes,
                    stderr_bytes: frozen.stderr_bytes,
                };
                ExecObservation::Executed(session.execute(&command, &stdin, limits).await)
            }
            Err(error) => ExecObservation::Refused {
                error: format!("{error:?}"),
            },
        };
        let (payload, outcome, effect) = exec_payload(&frozen, &observation);
        support::observe_record(
            &self.store,
            &operation,
            &attempt,
            "ssh.exec.v1",
            &payload,
            outcome,
            effect,
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "ssh.exec",
            1,
            plugin_id(EXEC_PLUGIN),
            "ssh.exec.v1",
            Digest::blake3(EXEC_CONTRACT),
            "ssh-exec-recovery",
            "controller recovered without a conclusive remote result; the command may or may not have run — query only the original observation, never resend",
        )
    }
}

fn freeze_exec(store: &Store, input: ExecInput) -> Result<ExecFrozen> {
    ensure!(
        !input.command.is_empty() && input.command.len() <= 64 * 1024 && !input.command.contains('\0'),
        "ssh.exec command must be non-empty, <= 64KiB, without NUL"
    );
    let limits = input.limits.unwrap_or(ExecLimitsInput {
        deadline_ms: None,
        shutdown_grace_ms: None,
        stdout_bytes: None,
        stderr_bytes: None,
    });
    let deadline_ms = limits.deadline_ms.unwrap_or(30_000).clamp(1_000, 300_000);
    let shutdown_grace_ms = limits.shutdown_grace_ms.unwrap_or(500).clamp(100, 2_000);
    let stdout_bytes = limits.stdout_bytes.unwrap_or(256 * 1024).clamp(1_024, 2 * 1024 * 1024);
    let stderr_bytes = limits.stderr_bytes.unwrap_or(64 * 1024).clamp(1_024, 2 * 1024 * 1024);
    // Verify the exact trust binding now; recovery re-reads the frozen bytes.
    let _ = NativeSshTarget::from_binding(store, &input.binding)?;
    ensure!(
        input.binding.target.key_path.is_some(),
        "native ssh.exec requires an explicit key_path binding (agent-backed signers are not admitted here)"
    );
    ensure!(
        input.binding.target.use_agent == false,
        "native ssh.exec does not admit use_agent bindings"
    );
    Ok(ExecFrozen {
        schema_version: 1,
        binding: input.binding,
        command: input.command,
        stdin_b64: input.stdin_b64.unwrap_or_default(),
        deadline_ms,
        shutdown_grace_ms,
        stdout_bytes,
        stderr_bytes,
    })
}

fn exec_payload(
    frozen: &ExecFrozen,
    observation: &ExecObservation,
) -> (serde_json::Value, Option<OperationOutcome>, EffectStatus) {
    match observation {
        ExecObservation::Executed(Ok(observation)) => {
            // execute() returns a transport observation even after a deadline,
            // disconnect or protocol failure. Local session shutdown is not
            // evidence that an accepted remote command stopped. Only a coherent
            // accepted/closed exchange with a remote exit is conclusive here.
            // OutputLimit retains that exit after draining; it does not erase it.
            let conclusive = observation.exec_request_queued
                && observation.exec_accepted
                && observation.channel_closed
                && observation.exit.is_some()
                && matches!(observation.interruption.as_ref(),
                    None | Some(NativeInterruption::OutputLimit));
            (
                serde_json::json!({
                    "schema_version": 1,
                    "status": if conclusive { "executed" } else { "unknown" },
                    "target_id": frozen.binding.target_id,
                    "trust_digest": frozen.binding.known_hosts.digest,
                    "observation": observation,
                }),
                conclusive.then_some(OperationOutcome::Succeeded),
                if conclusive { EffectStatus::Complete } else { EffectStatus::Unknown },
            )
        },
        ExecObservation::Executed(Err(error)) => (
            serde_json::json!({
                "schema_version": 1,
                "status": "refused",
                "target_id": frozen.binding.target_id,
                "error": format!("{error:?}"),
                "note": "the command was never started; a lost result after a start must query the original operation, never resend",
            }),
            Some(OperationOutcome::Failed),
            EffectStatus::None,
        ),
        ExecObservation::Refused { error } => (
            serde_json::json!({
                "schema_version": 1,
                "status": "refused",
                "target_id": frozen.binding.target_id,
                "error": error,
                "note": "connect/authentication refused before any remote effect",
            }),
            Some(OperationOutcome::Failed),
            EffectStatus::None,
        ),
    }
}

// ---------------------------------------------------------------------------
// ssh.pty
// ---------------------------------------------------------------------------

/// Installed `ssh.pty` authority — one live PTY seat per operation.
pub struct SshPtyHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl SshPtyHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }

    async fn open_seat(&self, frozen: &PtyFrozen, task_id: EntityId) -> Result<SeatHandle> {
        let deadline = Duration::from_millis(frozen.handshake_ms);
        let session =
            support::connect_session(&self.store, &frozen.binding, deadline).await?;
        let program = match &frozen.program {
            PtyProgramFrozen::Exec { command } => NativePtyProgram::Exec {
                command: command.clone().into_bytes(),
            },
            PtyProgramFrozen::Shell => NativePtyProgram::Shell,
        };
        let pty_session = session
            .open_pty(
                NativePtyRequest {
                    term: frozen.term.clone(),
                    cols: frozen.cols,
                    rows: frozen.rows,
                    program,
                },
                deadline,
            )
            .await
            .map_err(|error| anyhow::anyhow!("native PTY handshake failed: {error:?}"))?;
        // Durable bindings first: the seat record and the v2 writer lease
        // commit before the live adapter may persist any question.
        let location = shenko_store::SeatLocation::SshPty {
            host: frozen.binding.target.host.clone(),
            port: frozen.binding.target.port,
            user: frozen.binding.target.user.clone(),
            program_b64: support::encode_b64(
                &serde_json::to_vec(&frozen.program).unwrap_or_default(),
            ),
        };
        self.store.register_seat(
            frozen.seat_id,
            task_id,
            SeatAdapterId::SshPty.as_str(),
            crate::seat::ssh_pty::SSH_PTY_ADAPTER_VERSION,
            location,
        )?;
        self.store.acquire_seat_lease(SeatLeaseRequest {
            lease_id: frozen.lease_id,
            seat_id: frozen.seat_id,
            holder_task_id: task_id,
            lease_epoch: frozen.lease_epoch,
            fence_capability: frozen.fence_capability.clone(),
        })?;
        let location_id = format!(
            "ssh-pty://{}@{}:{}",
            frozen.binding.target.user, frozen.binding.target.host, frozen.binding.target.port
        );
        let handle = SeatHandle {
            location_id,
            adapter: SeatAdapterId::SshPty,
            adapter_version: crate::seat::ssh_pty::SSH_PTY_ADAPTER_VERSION.into(),
            host: frozen.binding.target.host.clone(),
            cwd: None,
            remote_session_id: None,
            current_command: Some(match &frozen.program {
                PtyProgramFrozen::Exec { command } => command.clone(),
                PtyProgramFrozen::Shell => "<remote shell>".into(),
            }),
            process_identity: None,
        };
        let seat = crate::seat::ssh_pty::SshPtySeat::spawn(
            pty_session,
            handle.clone(),
            frozen.cols as u16,
            frozen.rows as u16,
        );
        self.runtime
            .attach(
                Arc::new(seat),
                SeatAttachment { task_id, seat_record_id: frozen.seat_id, lease_id: frozen.lease_id },
            )
            .await
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PtyInput {
    binding: SshBinding,
    term: String,
    cols: u32,
    rows: u32,
    program: PtyProgramInput,
    #[serde(default)]
    handshake_ms: Option<u64>,
    seat_id: EntityId,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum PtyProgramInput {
    Exec { command: String },
    Shell,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum PtyProgramFrozen {
    Exec { command: String },
    Shell,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PtyFrozen {
    schema_version: u32,
    binding: SshBinding,
    term: String,
    cols: u32,
    rows: u32,
    program: PtyProgramFrozen,
    handshake_ms: u64,
    seat_id: EntityId,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
}

#[async_trait]
impl NativeTaskHandler for SshPtyHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(PTY_CONTRACT);
        NativeTaskDescriptor {
            kind: "ssh.pty".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(PTY_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-ssh-pty/1".into(),
            authority_refs: vec!["ssh.pty.live_seat/v1".into()],
            prompt_section: Some(
                "ssh.pty/v1 是已安装的远程 PTY seat 开启 child。用 [delegate:ssh.pty:1] {\"binding\":{...},\"term\":\"xterm-256color\",\"cols\":120,\"rows\":32,\"program\":{\"mode\":\"exec\",\"command\":\"...\"}|{\"mode\":\"shell\"},\"seat_id\":\"预选的 seat 记录 UUID\",\"lease_id\":\"预选的租约 UUID\",\"lease_epoch\":1,\"fence_capability\":\"ssh.pty:user@host:port\"} 开启一个被本运行时管理的远程 seat。seat_id/lease_id 必须在尝试前选好并持久保存：崩溃后按同一身份恢复。成功只说明 PTY 打开且 seat 已挂接，不代表远程代理完成了任何工作；unknown 读取持久 seat 事实，绝不自动重开 PTY。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(&self.store, &invocation, "ssh.pty", 1, plugin_id(PTY_PLUGIN))?;
        let task_id = invocation.task.task_id;
        let input: PtyInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = freeze_pty(&self.store, input)?;
        let input_ref = support::seal(&self.store, &frozen, "ssh-pty-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(PTY_PLUGIN),
            Digest::blake3(PTY_CONTRACT),
            "ssh.pty.v1",
            input_ref,
            serde_json::json!({
                "provider": "ssh.native-pty.v1",
                "target_id": frozen.binding.target_id,
                "account_identity": frozen.binding.target.user,
                "trust_digest": frozen.binding.known_hosts.digest,
                "seat_id": frozen.seat_id,
                "lease_id": frozen.lease_id,
            }),
            "one live PTY seat opened, durably registered and attached to this runtime; complete means the PTY handshake succeeded and the seat is observable, not that the remote agent did any work",
            "read the durable seat record and lease facts; a live PTY is never re-opened automatically — reopening is a new explicit operation",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        let attach = self.open_seat(&frozen, task_id).await;
        let (payload, outcome, effect) = match attach {
            Ok(attached) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "attached",
                    "target_id": frozen.binding.target_id,
                    "seat_id": frozen.seat_id,
                    "lease_id": frozen.lease_id,
                    "location_id": attached.location_id,
                    "adapter": attached.adapter.as_str(),
                    "adapter_version": attached.adapter_version,
                }),
                Some(OperationOutcome::Succeeded),
                EffectStatus::Complete,
            ),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "refused",
                    "target_id": frozen.binding.target_id,
                    "error": format!("{error:#}"),
                    "note": "the PTY was not opened; no remote seat exists for this operation",
                }),
                Some(OperationOutcome::Failed),
                EffectStatus::None,
            ),
        };
        support::observe_record(
            &self.store,
            &operation,
            &attempt,
            "ssh.pty.v1",
            &payload,
            outcome,
            effect,
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "ssh.pty",
            1,
            plugin_id(PTY_PLUGIN),
            "ssh.pty.v1",
            Digest::blake3(PTY_CONTRACT),
            "ssh-pty-recovery",
            "controller recovered; the durable seat/lease facts are the only authority — a live PTY is never re-opened by reconcile",
        )
    }
}

fn freeze_pty(store: &Store, input: PtyInput) -> Result<PtyFrozen> {
    ensure!(
        !input.term.is_empty() && input.term.len() <= 64,
        "ssh.pty term must be non-empty and <= 64 bytes"
    );
    ensure!(
        input.cols > 0 && input.cols <= 512 && input.rows > 0 && input.rows <= 128,
        "ssh.pty cols/rows out of range"
    );
    ensure!(input.lease_epoch > 0, "ssh.pty lease_epoch must be > 0");
    ensure!(
        !input.fence_capability.is_empty() && input.fence_capability.len() <= 256,
        "ssh.pty fence_capability must be non-empty and <= 256 bytes"
    );
    let _ = NativeSshTarget::from_binding(store, &input.binding)?;
    ensure!(
        input.binding.target.key_path.is_some(),
        "native ssh.pty requires an explicit key_path binding (agent-backed signers are not admitted here)"
    );
    ensure!(
        !input.binding.target.use_agent,
        "native ssh.pty does not admit use_agent bindings"
    );
    let program = match input.program {
        PtyProgramInput::Exec { command } => {
            ensure!(
                !command.is_empty() && command.len() <= 64 * 1024 && !command.contains('\0'),
                "ssh.pty exec command must be non-empty, <= 64KiB, without NUL"
            );
            PtyProgramFrozen::Exec { command }
        }
        PtyProgramInput::Shell => PtyProgramFrozen::Shell,
    };
    Ok(PtyFrozen {
        schema_version: 1,
        binding: input.binding,
        term: input.term,
        cols: input.cols,
        rows: input.rows,
        program,
        handshake_ms: input.handshake_ms.unwrap_or(10_000).clamp(1_000, 60_000),
        seat_id: input.seat_id,
        lease_id: input.lease_id,
        lease_epoch: input.lease_epoch,
        fence_capability: input.fence_capability,
    })
}

// ---------------------------------------------------------------------------
// seat.observe
// ---------------------------------------------------------------------------

/// Installed `seat.observe` authority — one observation of one seat.
pub struct SeatObserveHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl SeatObserveHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveInput {
    location_id: String,
    seat_id: EntityId,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveFrozen {
    schema_version: u32,
    location_id: String,
    seat_id: EntityId,
}

#[async_trait]
impl NativeTaskHandler for SeatObserveHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(OBSERVE_CONTRACT);
        NativeTaskDescriptor {
            kind: "seat.observe".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(OBSERVE_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-seat-observe/1".into(),
            authority_refs: vec!["seat.observe.attached_seat/v1".into()],
            prompt_section: Some(
                "seat.observe/v1 是已安装的 seat 观测 child。用 [delegate:seat.observe:1] {\"location_id\":\"ssh.pty://user@host:22\",\"seat_id\":\"seat 记录 UUID\"} 读取一个已挂接 seat 的画面与问题；每个问题在返回前已经持久。seat 未挂接时返回 durable_only：只有持久问题事实，没有画面。observation 的画面是证据，不是答案已被接受或远程代理仍在运行的证明。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(
            &self.store,
            &invocation,
            "seat.observe",
            1,
            plugin_id(OBSERVE_PLUGIN),
        )?;
        let input: ObserveInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = ObserveFrozen {
            schema_version: 1,
            location_id: input.location_id,
            seat_id: input.seat_id,
        };
        let input_ref = support::seal(&self.store, &frozen, "seat-observe-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(OBSERVE_PLUGIN),
            Digest::blake3(OBSERVE_CONTRACT),
            "seat.observe.v1",
            input_ref,
            serde_json::json!({
                "provider": "seat.runtime.v1",
                "location_id": frozen.location_id,
                "seat_id": frozen.seat_id,
            }),
            "one observation of the named seat; every reported question is durable before the observation returns; a durable_only status means no live seat is attached in this process",
            "re-observe live seats, or read the durable question facts only; never fabricate a screen from durable state",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        let (payload, outcome, effect) = match self.runtime.observe(&frozen.location_id).await {
            Ok(observation) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "observed",
                    "location_id": frozen.location_id,
                    "seat_id": frozen.seat_id,
                    "observation_revision": observation.observation_revision,
                    "screen_text": observation.screen_text,
                    "questions": observation.questions,
                    "unknown": observation.unknown,
                }),
                Some(OperationOutcome::Succeeded),
                EffectStatus::Complete,
            ),
            Err(live_error) => {
                let durable: Vec<shenko_store::SeatQuestionRecord> =
                    self.store.seat_questions(frozen.seat_id).unwrap_or_default();
                (
                    serde_json::json!({
                        "schema_version": 1,
                        "status": "durable_only",
                        "location_id": frozen.location_id,
                        "seat_id": frozen.seat_id,
                        "live_error": format!("{live_error:#}"),
                        "questions": durable,
                    }),
                    Some(OperationOutcome::Succeeded),
                    EffectStatus::Complete,
                )
            }
        };
        support::observe_record(
            &self.store,
            &operation,
            &attempt,
            "seat.observe.v1",
            &payload,
            outcome,
            effect,
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "seat.observe",
            1,
            plugin_id(OBSERVE_PLUGIN),
            "seat.observe.v1",
            Digest::blake3(OBSERVE_CONTRACT),
            "seat-observe-recovery",
            "controller recovered; re-observation is a new explicit operation, recovery only re-reads the original receipt",
        )
    }
}

// ---------------------------------------------------------------------------
// seat.fleet
// ---------------------------------------------------------------------------

/// Installed read-only inventory; never creates or attaches a remote seat.
pub struct SeatFleetHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl SeatFleetHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }

    async fn record_snapshot(
        &self,
        operation: &OperationRecord,
        attempt: &shenko_types::operation::AttemptRecord,
        input: &FleetInput,
    ) -> Result<OperationRecord> {
        let (seats, truncated) = self.runtime.fleet_view(input.task_id).await?;
        support::observe_record(
            &self.store, operation, attempt, "seat.fleet.v1",
            &serde_json::json!({
                "schema_version": 1,
                "task_id": input.task_id,
                "seats": seats,
                "truncated": truncated,
            }),
            Some(OperationOutcome::Succeeded), EffectStatus::None,
        )
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FleetInput {
    #[serde(default)]
    task_id: Option<EntityId>,
}

#[async_trait]
impl NativeTaskHandler for SeatFleetHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        NativeTaskDescriptor {
            kind: "seat.fleet".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(FLEET_PLUGIN),
                artifact_digest: Digest::blake3(FLEET_CONTRACT),
                registry_revision: 0,
            },
            role_revision: "native-seat-fleet/1".into(),
            authority_refs: vec!["seat.fleet.read_only/v1".into()],
            prompt_section: Some(
                "seat.fleet/v1 只读查看已登记的外部 Agent 工作位及其所属任务、传输状态、待决定问题数。用 [delegate:seat.fleet:1] {} 发现工作位；已知任务时可传 {\"task_id\":\"任务 UUID\"} 筛选。最多 32 条，truncated 明示剩余。transport_live 为 null 表示无法确认，false 表示本地没有活传输，不证明远端进程死亡；不新建、不挂接、不获取写租约。进程活着不代表协议已就绪或任务完成。".into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(&self.store, &invocation, "seat.fleet", 1, plugin_id(FLEET_PLUGIN))?;
        let input: FleetInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let input_ref = support::seal(&self.store, &input, "seat-fleet-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store, &invocation, plugin_id(FLEET_PLUGIN), Digest::blake3(FLEET_CONTRACT),
            "seat.fleet.v1", input_ref,
            serde_json::json!({"task_id": input.task_id, "provider": "seat.runtime.v1"}),
            "one bounded read-only inventory with transport uncertainty retained",
            "read again from the original frozen task selector; never attach or write to a seat",
        )?;
        if let Some(existing) = self.store.arm_external_operation(
            &invocation.task_invocation, &mut operation, &mut attempt,
        )? {
            return Ok(existing);
        }
        self.record_snapshot(&operation, &attempt, &input).await
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        let operation = self.store.external_operation(operation_id)?;
        ensure!(
            operation.task_id == task.task_id && task.kind == "seat.fleet"
                && task.definition_version == 1 && operation.operation_name == "seat.fleet.v1"
                && operation.contract_ref.plugin_id == plugin_id(FLEET_PLUGIN)
                && operation.contract_ref.contract_digest == Digest::blake3(FLEET_CONTRACT),
            "original_operation_contract_mismatch"
        );
        if operation.state == shenko_types::operation::OperationState::Resolved || operation.outcome_conflict {
            return Ok(operation);
        }
        if !matches!(operation.state,
            shenko_types::operation::OperationState::InFlight
                | shenko_types::operation::OperationState::Reconciling)
        {
            return Ok(operation);
        }
        let input = support::frozen_read(&self.store, &operation, "seat-fleet-frozen-input")?;
        let attempt = self.store.external_operation_attempt(operation_id)?;
        self.record_snapshot(&operation, &attempt, &input).await
    }
}

// ---------------------------------------------------------------------------
// seat.respond
// ---------------------------------------------------------------------------

/// Installed `seat.respond` authority — one durable answer decision.
pub struct SeatRespondHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl SeatRespondHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondInput {
    location_id: String,
    seat_id: EntityId,
    question_id: String,
    answer: SeatAnswerInput,
    /// Who decided. The default is the subject itself; a delegated child
    /// names itself so the decision records its own scope.
    #[serde(default)]
    deciding_actor: Option<String>,
    /// The delegation that gave the decider its decision scope.
    #[serde(default)]
    delegation_grant_ref: Option<String>,
    /// The authority revision the decision was made under.
    #[serde(default)]
    authority_revision: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SeatAnswerInput {
    #[serde(default)]
    option_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    allow: Option<bool>,
    reason: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondFrozen {
    schema_version: u32,
    location_id: String,
    seat_id: EntityId,
    question_id: String,
    answer: SeatAnswer,
    deciding_actor: String,
    delegation_grant_ref: Option<String>,
    authority_revision: Option<u64>,
}

#[async_trait]
impl NativeTaskHandler for SeatRespondHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(RESPOND_CONTRACT);
        NativeTaskDescriptor {
            kind: "seat.respond".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(RESPOND_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-seat-respond/1".into(),
            authority_refs: vec!["seat.respond.durable_decision/v1".into()],
            prompt_section: Some(
                "seat.respond/v1 是已安装的 seat 回答 child。用 [delegate:seat.respond:1] {\"location_id\":\"...\",\"seat_id\":\"...\",\"question_id\":\"观测返回的语义问题 id\",\"answer\":{\"option_id\":\"可选\",\"text\":\"可选\",\"allow\":可选,\"reason\":\"理由\"},\"deciding_actor\":\"可选，默认 subject\"} 回答一个 open 的持久问题。先观测再回答；stale_question/stale_task/stale_authority 表示事实已变，必须重新观测，绝不凭旧画面重发。决定先落盘（compare-and-swap），再复核现场，最后才写入；写入前任何一步被拒都不会碰到远端。返回的 status 区分 answered（远端确认）、answer_unknown（已写但未确认，绝不重发）、not_sent（未写入，可重新决定）和 rejected（远端拒绝）。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(
            &self.store,
            &invocation,
            "seat.respond",
            1,
            plugin_id(RESPOND_PLUGIN),
        )?;
        let input: RespondInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = RespondFrozen {
            schema_version: 1,
            location_id: input.location_id,
            seat_id: input.seat_id,
            question_id: input.question_id,
            answer: SeatAnswer {
                option_id: input.answer.option_id,
                text: input.answer.text,
                allow: input.answer.allow,
                reason: input.answer.reason,
            },
            deciding_actor: input
                .deciding_actor
                .unwrap_or_else(|| "subject".to_string()),
            delegation_grant_ref: input.delegation_grant_ref,
            authority_revision: input.authority_revision,
        };
        let input_ref = support::seal(&self.store, &frozen, "seat-respond-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(RESPOND_PLUGIN),
            Digest::blake3(RESPOND_CONTRACT),
            "seat.respond.v1",
            input_ref,
            serde_json::json!({
                "provider": "seat.runtime.v1",
                "location_id": frozen.location_id,
                "seat_id": frozen.seat_id,
                "question_id": frozen.question_id,
            }),
            "one durable answer decision for one open question; the decision is committed before anything is written, the exact effect is armed with its digest, and the closing evidence decides answered / answer_unknown / not_sent / withdrawn",
            "re-observe the seat and read the durable decision facts; an unknown or lost effect is never re-answered automatically",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        let request = SeatRespondRequest {
            location_id: frozen.location_id.clone(),
            semantic_question_id: frozen.question_id.clone(),
            answer: frozen.answer.clone(),
            deciding_actor: frozen.deciding_actor.clone(),
            delegation_grant_ref: frozen.delegation_grant_ref.clone(),
            authority_revision: frozen.authority_revision,
            operation_id: invocation.operation_id,
        };
        let (payload, outcome, effect) = match self.runtime.respond(request).await {
            Ok(result) => {
                let status = match result.decision_state {
                    SeatAnswerDecisionState::Answered => "answered",
                    SeatAnswerDecisionState::AnswerUnknown => "answer_unknown",
                    SeatAnswerDecisionState::NotSent => "not_sent",
                    SeatAnswerDecisionState::Rejected => "rejected",
                    SeatAnswerDecisionState::Committed => "decision_committed",
                    SeatAnswerDecisionState::Armed => "responding",
                };
                let effect = match result.decision_state {
                    SeatAnswerDecisionState::Answered => EffectStatus::Complete,
                    // The effect may have reached the remote side: the
                    // receipt says so instead of claiming nothing happened.
                    SeatAnswerDecisionState::AnswerUnknown | SeatAnswerDecisionState::Armed => {
                        EffectStatus::Unknown
                    }
                    _ => EffectStatus::None,
                };
                (
                    serde_json::json!({
                        "schema_version": 2,
                        "status": status,
                        "location_id": frozen.location_id,
                        "seat_id": frozen.seat_id,
                        "semantic_question_id": frozen.question_id,
                        "durable_question_id": result.question_id.to_string(),
                        "decision_id": result.decision_id.to_string(),
                        "decision_state": result.decision_state.as_str(),
                        "question_state": result.question_state.as_str(),
                        "effect_input_digest": result.effect_input_digest,
                        "refusal": result.refusal,
                        "response": result.response,
                    }),
                    Some(OperationOutcome::Succeeded),
                    effect,
                )
            }
            Err(error) => (
                serde_json::json!({
                    "schema_version": 2,
                    "status": "refused",
                    "location_id": frozen.location_id,
                    "seat_id": frozen.seat_id,
                    "question_id": frozen.question_id,
                    "error": format!("{error:#}"),
                }),
                Some(OperationOutcome::Failed),
                EffectStatus::None,
            ),
        };
        support::observe_record(
            &self.store,
            &operation,
            &attempt,
            "seat.respond.v1",
            &payload,
            outcome,
            effect,
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "seat.respond",
            1,
            plugin_id(RESPOND_PLUGIN),
            "seat.respond.v1",
            Digest::blake3(RESPOND_CONTRACT),
            "seat-respond-recovery",
            "controller recovered; the typed input effect is not provably undone — read the durable seat decision facts (decision_state / question_state), never re-answer automatically",
        )
    }
}

// ---------------------------------------------------------------------------
// managed agent seats (omp.seat / claude.seat) — docs/specs/remote-seats.md#spec-17-1, docs/specs/remote-seats.md#spec-35-5, R069
// ---------------------------------------------------------------------------

const OMP_SEAT_CONTRACT: &[u8] = b"shenko.omp.seat/v1: one managed OMP JSONL-RPC seat per operation; the durable seat record binds the adapter's session anchor (the exact spawn command line), the v2 writer lease and the runtime attachment are recorded under the trusted operation id; recovery reads the durable seat facts, never re-spawns the process";
const CLAUDE_SEAT_CONTRACT: &[u8] = b"shenko.claude.seat/v1: one managed Claude Code control-protocol seat per operation; the durable seat record binds the durable session anchor (the caller's resume anchor, the remote-published session id, or the exact spawn command line), the v2 writer lease and the runtime attachment are recorded under the trusted operation id; recovery reads the durable seat facts, never re-spawns the process";

const OMP_SEAT_PLUGIN: &str = "1e5b7d0c-6f2a-4c9e-b4d6-0a5f7c1e3b5d";
const CLAUDE_SEAT_PLUGIN: &str = "2f6c8e1d-7a3b-4d0f-c5e7-1b6a8d2f4c6e";

/// The fields both managed seat openers take. `deny_unknown_fields` cannot
/// be combined with serde `flatten`, so each input struct repeats the
/// fields literally and converts into this shape.
struct ManagedSeatBase {
    /// The exact spawn command; argv[0] plus its arguments.
    command: Vec<String>,
    cwd: Option<String>,
    /// The pre-chosen seat record id. Required for a new seat; forbidden
    /// with `supersedes_seat_id`, which mints the successor record id.
    seat_id: Option<EntityId>,
    /// When set, the new record supersedes this predecessor and claims the
    /// same anchor — the relocate path for a respawned session.
    supersedes_seat_id: Option<EntityId>,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OmpSeatInput {
    command: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    seat_id: Option<EntityId>,
    #[serde(default)]
    supersedes_seat_id: Option<EntityId>,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
}

impl OmpSeatInput {
    fn into_base(self) -> ManagedSeatBase {
        ManagedSeatBase {
            command: self.command,
            cwd: self.cwd,
            seat_id: self.seat_id,
            supersedes_seat_id: self.supersedes_seat_id,
            lease_id: self.lease_id,
            lease_epoch: self.lease_epoch,
            fence_capability: self.fence_capability,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaudeSeatInput {
    command: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    seat_id: Option<EntityId>,
    #[serde(default)]
    supersedes_seat_id: Option<EntityId>,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
    /// The durable session anchor to bind: a real remote session id this
    /// seat resumes. When set and the command does not already carry
    /// `--resume`/`--session-id`, the handler appends `--resume <id>` so the
    /// anchor the store binds is the session the remote side re-enters.
    #[serde(default)]
    remote_session_id: Option<String>,
}

impl ClaudeSeatInput {
    fn into_parts(self) -> (ManagedSeatBase, Option<String>) {
        let base = ManagedSeatBase {
            command: self.command,
            cwd: self.cwd,
            seat_id: self.seat_id,
            supersedes_seat_id: self.supersedes_seat_id,
            lease_id: self.lease_id,
            lease_epoch: self.lease_epoch,
            fence_capability: self.fence_capability,
        };
        (base, self.remote_session_id)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedSeatFrozen {
    schema_version: u32,
    command: Vec<String>,
    cwd: Option<String>,
    seat_id: Option<EntityId>,
    supersedes_seat_id: Option<EntityId>,
    lease_id: EntityId,
    lease_epoch: u64,
    fence_capability: String,
    /// The session anchor the caller named, if any. When absent the record
    /// binds the command line (OMP) or the remote-published session id with
    /// the command line as fallback (Claude).
    remote_session_id: String,
    /// `remote_session_id` came from the caller, not the command fallback.
    anchor_named: bool,
}

fn freeze_managed_seat(
    base: ManagedSeatBase,
    remote_session_id: Option<String>,
) -> Result<ManagedSeatFrozen> {
    ensure!(
        !base.command.is_empty()
            && base.command.iter().all(|arg| !arg.is_empty() && !arg.contains('\0')),
        "managed seat command must be non-empty argv without NUL bytes"
    );
    ensure!(
        base.command.iter().map(|arg| arg.len()).sum::<usize>() <= 64 * 1024,
        "managed seat command exceeds 64KiB"
    );
    if let Some(cwd) = &base.cwd {
        ensure!(
            !cwd.is_empty() && cwd.len() <= 4096 && !cwd.contains('\0'),
            "managed seat cwd must be non-empty, <= 4KiB, without NUL"
        );
    }
    ensure!(
        base.seat_id.is_some() != base.supersedes_seat_id.is_some(),
        "exactly one of seat_id (new seat) or supersedes_seat_id (relocate) is required"
    );
    ensure!(base.lease_epoch > 0, "seat lease_epoch must be > 0");
    ensure!(
        !base.fence_capability.is_empty() && base.fence_capability.len() <= 256,
        "seat fence_capability must be non-empty and <= 256 bytes"
    );
    let anchor_named = remote_session_id.is_some();
    let anchor = match remote_session_id {
        Some(id) => {
            ensure!(
                !id.is_empty() && id.len() <= 512 && !id.contains('\0'),
                "remote_session_id must be non-empty and <= 512 bytes"
            );
            id
        }
        None => base.command.join(" "),
    };
    Ok(ManagedSeatFrozen {
        schema_version: 1,
        command: base.command,
        cwd: base.cwd,
        seat_id: base.seat_id,
        supersedes_seat_id: base.supersedes_seat_id,
        lease_id: base.lease_id,
        lease_epoch: base.lease_epoch,
        fence_capability: base.fence_capability,
        remote_session_id: anchor,
        anchor_named,
    })
}

/// Register (or relocate) the durable seat record + acquire the writer
/// lease, then attach the live adapter. Shared by both managed adapters;
/// the only difference is which `SeatLocation` variant the record binds.
/// Returns the committed seat record id and the attached handle.
async fn bind_managed_seat(
    store: &Store,
    runtime: &SeatRuntime,
    adapter: Arc<dyn SeatAdapter>,
    location: shenko_store::SeatLocation,
    frozen: &ManagedSeatFrozen,
    task_id: EntityId,
) -> Result<(EntityId, SeatHandle)> {
    // Durable bindings first: the seat record and the v2 writer lease
    // commit before the live adapter may persist any question.
    let seat_record_id = match (frozen.seat_id, frozen.supersedes_seat_id) {
        (Some(seat_id), None) => {
            store.register_seat(
                seat_id,
                task_id,
                adapter.adapter_id().as_str(),
                adapter.adapter_version(),
                location.clone(),
            )?;
            seat_id
        }
        (None, Some(previous)) => {
            let record = store.relocate_seat(
                previous,
                task_id,
                adapter.adapter_id().as_str(),
                adapter.adapter_version(),
                location.clone(),
            )?;
            record.seat_id
        }
        _ => unreachable!("freeze guarantees exactly one seat identity"),
    };
    store.acquire_seat_lease(SeatLeaseRequest {
        lease_id: frozen.lease_id,
        seat_id: seat_record_id,
        holder_task_id: task_id,
        lease_epoch: frozen.lease_epoch,
        fence_capability: frozen.fence_capability.clone(),
    })?;
    let handle = runtime
        .attach(
            adapter,
            SeatAttachment { task_id, seat_record_id, lease_id: frozen.lease_id },
        )
        .await?;
    Ok((seat_record_id, handle))
}

/// Installed `omp.seat` authority — one managed OMP seat per operation.
pub struct OmpSeatHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl OmpSeatHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }

    /// Spawn the managed OMP process, then bind it durably and attach it.
    /// The OMP adapter's session anchor is the exact spawn command line —
    /// the same value it exposes as `remote_session_id` — so the record
    /// binds `frozen.remote_session_id` (which `freeze_managed_seat` sets
    /// to that command line for OMP).
    async fn open_seat(
        &self,
        frozen: &ManagedSeatFrozen,
        task_id: EntityId,
    ) -> Result<(EntityId, SeatHandle)> {
        let seat = crate::seat::omp_rpc::OmpRpcSeat::spawn(
            frozen.command.clone(),
            frozen.cwd.clone().map(std::path::PathBuf::from),
        )
        .await?;
        let seat = Arc::new(seat);
        let result = bind_managed_seat(
            &self.store,
            &self.runtime,
            seat.clone(),
            shenko_store::SeatLocation::OmpRpc {
                host: "local".into(),
                remote_session_id: frozen.remote_session_id.clone(),
            },
            frozen,
            task_id,
        )
        .await;
        if result.is_err() {
            // Binding failed after the process started: stop our side so a
            // refused operation never leaves a live agent behind.
            seat.close().await;
        }
        result
    }
}

/// Installed `claude.seat` authority — one managed Claude Code seat per
/// operation.
pub struct ClaudeSeatHandler {
    store: Arc<Store>,
    runtime: Arc<SeatRuntime>,
}

impl ClaudeSeatHandler {
    pub fn new(store: Arc<Store>, runtime: Arc<SeatRuntime>) -> Self {
        Self { store, runtime }
    }

    /// Spawn the managed Claude process, resolve the durable session anchor,
    /// then bind it durably and attach it. A caller-named anchor is used
    /// verbatim; otherwise the anchor is the session id the remote side
    /// publishes (the adapter's `remote_session_id`) within a bounded wait,
    /// falling back to the exact spawn command line — the same fallback
    /// `ClaudeSdkSeat::discover` uses for `location_id`.
    async fn open_seat(
        &self,
        frozen: &ManagedSeatFrozen,
        task_id: EntityId,
    ) -> Result<(EntityId, SeatHandle)> {
        let seat = crate::seat::claude_sdk::ClaudeSdkSeat::spawn(
            frozen.command.clone(),
            frozen.cwd.clone().map(std::path::PathBuf::from),
        )
        .await?;
        let anchor = match frozen.anchor_named {
            true => frozen.remote_session_id.clone(),
            false => {
                let deadline = tokio::time::Instant::now() + Duration::from_millis(5_000);
                let mut anchor = seat.session_id().await;
                while anchor.is_none() && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    anchor = seat.session_id().await;
                }
                anchor.unwrap_or_else(|| frozen.remote_session_id.clone())
            }
        };
        let seat = Arc::new(seat);
        let result = bind_managed_seat(
            &self.store,
            &self.runtime,
            seat.clone(),
            shenko_store::SeatLocation::ClaudeSdk {
                host: "local".into(),
                remote_session_id: anchor,
            },
            frozen,
            task_id,
        )
        .await;
        if result.is_err() {
            // Binding failed after the process started: stop our side so a
            // refused operation never leaves a live agent behind.
            seat.close().await;
        }
        result
    }
}

#[async_trait]
impl NativeTaskHandler for OmpSeatHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(OMP_SEAT_CONTRACT);
        NativeTaskDescriptor {
            kind: "omp.seat".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(OMP_SEAT_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-omp-seat/1".into(),
            authority_refs: vec!["omp.seat.live_seat/v1".into()],
            prompt_section: Some(
                "omp.seat/v1 是已安装的托管 OMP seat 开启 child。用 [delegate:omp.seat:1] {\"command\":[\"omp\",\"--mode\",\"rpc\"],\"seat_id\":\"预选的 seat 记录 UUID\",\"lease_id\":\"预选的租约 UUID\",\"lease_epoch\":1,\"fence_capability\":\"omp.rpc:<session-anchor>\"} 启动并挂接一个托管 OMP 会话；用 \"supersedes_seat_id\"（代替 seat_id）在同一锚点上重开以延续前身。seat_id/lease_id 必须在尝试前选好并持久保存：崩溃后按同一身份恢复。持久锚点是 spawn 命令行本身（OMP 暴露的会话标识），同一锚点只允许一个活跃 seat。成功只说明进程已启动且 seat 已挂接；unknown 读取持久 seat 事实，绝不自动重开。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(&self.store, &invocation, "omp.seat", 1, plugin_id(OMP_SEAT_PLUGIN))?;
        let task_id = invocation.task.task_id;
        let input: OmpSeatInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let frozen = freeze_managed_seat(input.into_base(), None)?;
        let input_ref = support::seal(&self.store, &frozen, "omp-seat-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(OMP_SEAT_PLUGIN),
            Digest::blake3(OMP_SEAT_CONTRACT),
            "omp.seat.v1",
            input_ref,
            serde_json::json!({
                "provider": "omp.rpc.v1",
                "remote_session_id": frozen.remote_session_id,
                "seat_id": frozen.seat_id,
                "supersedes_seat_id": frozen.supersedes_seat_id,
                "lease_id": frozen.lease_id,
            }),
            "one managed OMP seat spawned, durably registered at its session anchor and attached to this runtime; complete means the process is observable, not that the agent did any work",
            "read the durable seat record and lease facts; a managed OMP process is never re-spawned automatically — reopening is a new explicit operation or a supersedes",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        let (payload, outcome, effect) = match self.open_seat(&frozen, task_id).await {
            Ok((seat_record_id, attached)) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "attached",
                    "seat_id": seat_record_id,
                    "lease_id": frozen.lease_id,
                    "location_id": attached.location_id,
                    "remote_session_id": attached.remote_session_id,
                    "adapter": attached.adapter.as_str(),
                    "adapter_version": attached.adapter_version,
                }),
                Some(OperationOutcome::Succeeded),
                EffectStatus::Complete,
            ),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "refused",
                    "error": format!("{error:#}"),
                    "note": "the managed OMP process was not started or not bound; no live seat exists for this operation",
                }),
                Some(OperationOutcome::Failed),
                EffectStatus::None,
            ),
        };
        support::observe_record(&self.store, &operation, &attempt, "omp.seat.v1", &payload, outcome, effect)
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "omp.seat",
            1,
            plugin_id(OMP_SEAT_PLUGIN),
            "omp.seat.v1",
            Digest::blake3(OMP_SEAT_CONTRACT),
            "omp-seat-recovery",
            "controller recovered; the durable seat/lease facts are the only authority — a managed process is never re-spawned by reconcile",
        )
    }
}

#[async_trait]
impl NativeTaskHandler for ClaudeSeatHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let contract_digest = Digest::blake3(CLAUDE_SEAT_CONTRACT);
        NativeTaskDescriptor {
            kind: "claude.seat".into(),
            definition_version: 1,
            implementation_ref: ImplementationRef {
                plugin_id: plugin_id(CLAUDE_SEAT_PLUGIN),
                artifact_digest: contract_digest,
                registry_revision: 0,
            },
            role_revision: "native-claude-seat/1".into(),
            authority_refs: vec!["claude.seat.live_seat/v1".into()],
            prompt_section: Some(
                "claude.seat/v1 是已安装的托管 Claude Code seat 开启 child。用 [delegate:claude.seat:1] {\"command\":[\"claude\",\"-p\",\"...\"],\"seat_id\":\"预选的 seat 记录 UUID\",\"lease_id\":\"预选的租约 UUID\",\"lease_epoch\":1,\"fence_capability\":\"claude.control:<session-anchor>\",\"remote_session_id\":\"可选，要续接的会话 id\"} 启动并挂接一个托管 Claude Code 会话；给 remote_session_id 时命令会追加 --resume 续接该会话，持久锚点就是该会话 id；不给则锚定远端发布的 session_id（未发布则退回 spawn 命令行）。用 \"supersedes_seat_id\"（代替 seat_id）在同一锚点上重开以延续前身。seat_id/lease_id 必须在尝试前选好并持久保存。成功只说明进程已启动且 seat 已挂接；unknown 读取持久 seat 事实，绝不自动重开。"
                    .into(),
            ),
        }
    }

    async fn execute(&self, mut invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        support::verify_wiring(
            &self.store,
            &invocation,
            "claude.seat",
            1,
            plugin_id(CLAUDE_SEAT_PLUGIN),
        )?;
        let task_id = invocation.task.task_id;
        let input: ClaudeSeatInput = serde_json::from_value(std::mem::take(&mut invocation.input))?;
        let (mut base, remote_session_id) = input.into_parts();
        // A caller-named anchor is the session to re-enter: inject --resume
        // unless the command already names one; a command that names a
        // *different* session contradicts the anchor and is refused.
        if let Some(resume) = remote_session_id.as_deref() {
            let session_arg = base
                .command
                .windows(2)
                .find(|w| w[0] == "--resume" || w[0] == "--session-id")
                .map(|w| w[1].as_str());
            ensure!(
                session_arg.is_none() || session_arg == Some(resume),
                "claude.seat remote_session_id contradicts the command's own --resume/--session-id"
            );
            if session_arg.is_none()
                && !base.command.iter().any(|arg| arg == "--continue")
            {
                base.command.push("--resume".into());
                base.command.push(resume.to_string());
            }
        }
        let frozen = freeze_managed_seat(base, remote_session_id)?;
        let input_ref = support::seal(&self.store, &frozen, "claude-seat-frozen-input")?;
        let (mut operation, mut attempt) = support::operation_parts(
            &self.store,
            &invocation,
            plugin_id(CLAUDE_SEAT_PLUGIN),
            Digest::blake3(CLAUDE_SEAT_CONTRACT),
            "claude.seat.v1",
            input_ref,
            serde_json::json!({
                "provider": "claude.control.v1",
                "remote_session_id": frozen.remote_session_id,
                "seat_id": frozen.seat_id,
                "supersedes_seat_id": frozen.supersedes_seat_id,
                "lease_id": frozen.lease_id,
            }),
            "one managed Claude Code seat spawned, durably registered at its session anchor and attached to this runtime; complete means the process is observable, not that the agent did any work",
            "read the durable seat record and lease facts; a managed Claude process is never re-spawned automatically — reopening is a new explicit operation or a supersedes",
        )?;
        if let Some(existing) = self
            .store
            .arm_external_operation(&invocation.task_invocation, &mut operation, &mut attempt)?
        {
            return Ok(existing);
        }
        let (payload, outcome, effect) = match self.open_seat(&frozen, task_id).await {
            Ok((seat_record_id, attached)) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "attached",
                    "seat_id": seat_record_id,
                    "lease_id": frozen.lease_id,
                    "location_id": attached.location_id,
                    "remote_session_id": attached.remote_session_id,
                    "adapter": attached.adapter.as_str(),
                    "adapter_version": attached.adapter_version,
                }),
                Some(OperationOutcome::Succeeded),
                EffectStatus::Complete,
            ),
            Err(error) => (
                serde_json::json!({
                    "schema_version": 1,
                    "status": "refused",
                    "error": format!("{error:#}"),
                    "note": "the managed Claude process was not started or not bound; no live seat exists for this operation",
                }),
                Some(OperationOutcome::Failed),
                EffectStatus::None,
            ),
        };
        support::observe_record(
            &self.store,
            &operation,
            &attempt,
            "claude.seat.v1",
            &payload,
            outcome,
            effect,
        )
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        support::reconcile_query_only(
            &self.store,
            task,
            operation_id,
            "claude.seat",
            1,
            plugin_id(CLAUDE_SEAT_PLUGIN),
            "claude.seat.v1",
            Digest::blake3(CLAUDE_SEAT_CONTRACT),
            "claude-seat-recovery",
            "controller recovered; the durable seat/lease facts are the only authority — a managed process is never re-spawned by reconcile",
        )
    }
}

#[cfg(test)]
mod exec_observation_tests {
    use super::*;
    use crate::native_transport::NativeExit;

    // Only receipt projection is under test. This synthetic binding is never
    // admitted, opened or connected, and contains no credential or real path.
    fn frozen() -> ExecFrozen {
        let digest = Digest("0".repeat(64));
        ExecFrozen {
            schema_version: 1,
            binding: SshBinding {
                target_id: "projection-fixture".into(),
                target: crate::SshTarget::default(),
                receipt_root: "/unused-projection-fixture".into(),
                known_hosts: ObjectRef {
                    digest: digest.clone(), bytes: 0, media_type: "text/plain".into(),
                    store_namespace: "fixture-only".into(), locator: "not-admitted".into(),
                    codec: SchemaRef { name: "fixture".into(), major: 1, digest },
                },
            },
            command: "never-executed".into(), stdin_b64: String::new(),
            deadline_ms: 100, shutdown_grace_ms: 100, stdout_bytes: 32, stderr_bytes: 32,
        }
    }

    #[test]
    fn accepted_deadline_without_exit_remains_unknown() {
        let observation = NativeCommandObservation {
            schema: "ssh.native-transport.v1", stdout: b"partial".to_vec(), stderr: vec![],
            exit: None, exec_request_queued: true, exec_accepted: true,
            channel_closed: false, interruption: Some(NativeInterruption::Deadline),
            session_end_observed: true,
        };
        let (payload, outcome, effect) = exec_payload(&frozen(), &ExecObservation::Executed(Ok(observation)));
        assert_eq!(outcome, None, "a local protocol end does not prove the remote command ended");
        assert_eq!(effect, EffectStatus::Unknown);
        assert_eq!(payload["observation"]["interruption"], "deadline");
        assert!(payload["observation"]["exit"].is_null());
        assert_eq!(payload["observation"]["stdout"], serde_json::json!(b"partial".to_vec()));
    }

    #[test]
    fn bounded_output_with_exit_255_retains_conclusive_transport_result() {
        let observation = NativeCommandObservation {
            schema: "ssh.native-transport.v1", stdout: b"bounded".to_vec(), stderr: vec![],
            exit: Some(NativeExit::Status { code: 255 }),
            exec_request_queued: true, exec_accepted: true, channel_closed: true,
            interruption: Some(NativeInterruption::OutputLimit), session_end_observed: true,
        };
        let (payload, outcome, effect) = exec_payload(&frozen(), &ExecObservation::Executed(Ok(observation)));
        assert_eq!(outcome, Some(OperationOutcome::Succeeded));
        assert_eq!(effect, EffectStatus::Complete);
        assert_eq!(payload["observation"]["exit"]["code"], 255);
        assert_eq!(payload["observation"]["interruption"], "output_limit");
    }
}
