//! Native SSH child installed from explicit node-local capabilities.
//! Selection records intent; the installer supplies the real endpoint and
//! signer. T1 precedes authentication/dispatch. Recovery only queries the
//! original sealed operation; neither a completed channel nor a released local
//! connection slot proves arbitrary remote effects complete.
use anyhow::{Context as _, Result, ensure};
use async_trait::async_trait;
use serde::Deserialize;
use shenko_exec_ssh::{NativeSshFileTargetConfig, NativeSshInstallation,
    SelectedSshOutcome, SshAction, SshTaskRunner, native_transport::NativeCommandLimits};
use shenko_node::Node;
use shenko_store::Store;
use shenko_kernel::task::{NativeTaskDescriptor, NativeTaskHandler, NativeTaskInvocation};
use shenko_types::{ids::EntityId, object::Digest, operation::OperationRecord,
    task::{ImplementationRef, TaskRecord}};
use std::{collections::BTreeMap, path::Path, sync::{Arc, Mutex}, time::Duration};

const CONTRACT: &[u8] = b"shenko.ssh.native/v1: explicit installed target and signer; selected node is intent, not grant; durable T1 before connection/authentication; generation/owner fencing before exec; recovery queries original sealed operation without replay";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SshInput {
    target_id: String,
    decision_id: EntityId,
    action: SshAction,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}
fn default_timeout() -> u64 { 30 }


pub struct NativeSshTaskHandler {
    store: Arc<Store>,
    node: Mutex<Node>,
    targets: BTreeMap<String, NativeSshInstallation>,
    // Local transport capacity only, never a remote-effect/resource lease.
    slots: tokio::sync::Semaphore,
}

impl NativeSshTaskHandler {
    pub fn new(store: Arc<Store>, node: Node, targets: Vec<NativeSshInstallation>) -> Result<Self> {
        ensure!(!targets.is_empty() && targets.len() <= 128, "ssh_installation_target_count_invalid");
        let mut installed = BTreeMap::new();
        for target in targets {
            ensure!(target.seal().ledger_id == store.ledger.ledger_id(), "ssh_installation_ledger_mismatch");
            ensure!(installed.insert(target.target_id().to_owned(), target).is_none(), "ssh_installation_duplicate_target");
        }
        Ok(Self { store, node: Mutex::new(node), targets: installed, slots: tokio::sync::Semaphore::new(4) })
    }

    /// Runtime-owner-selected file only: no HOME/env/key enumeration. Config
    /// contains public identities and explicit local file locations, not keys.
    /// Loading it does not authenticate, connect, or certify a remote Node ID.
    pub fn from_file(store: Arc<Store>, node: Node, path: &Path) -> Result<Self> {
        let configs = NativeSshFileTargetConfig::read_installation_file(path)?;
        let targets = configs.into_iter().map(|target| target.install(&store)).collect::<Result<Vec<_>>>()?;
        Self::new(store, node, targets)
    }

    fn plugin_id() -> EntityId {
        "4d275d62-f194-4d28-9338-ceb58b90a2b0".parse().expect("fixed native SSH plugin UUID")
    }
    fn limits(timeout_secs: u64) -> Result<NativeCommandLimits> {
        ensure!((1..=3600).contains(&timeout_secs), "ssh_transport_deadline_invalid");
        Ok(NativeCommandLimits { deadline: Duration::from_secs(timeout_secs),
            shutdown_grace: Duration::from_millis(100), stdout_bytes: 2 * 1024 * 1024,
            stderr_bytes: 1024 * 1024 })
    }
    fn ensure_task(&self, task: &TaskRecord) -> Result<()> {
        ensure!(task.kind == "ssh.native" && task.definition_version == 1
            && task.implementation_ref.plugin_id == Self::plugin_id()
            && task.implementation_ref.artifact_digest == Digest::blake3(CONTRACT),
            "ssh_task_implementation_mismatch");
        let subject = self.store.installation_binding()?.subject.context("ssh_subject_not_initialized")?;
        let node = self.node.lock().map_err(|_| anyhow::anyhow!("ssh_node_observation_poisoned"))?;
        let identity = node.local_identity()?.context("ssh_node_subject_missing")?;
        ensure!(subject.agent_id == task.agent_id && identity.agent_id == subject.agent_id
            && identity.branch_id == subject.branch_id && identity.origin_proof == subject.origin_proof,
            "ssh_node_subject_binding_stale");
        Ok(())
    }
}

#[async_trait]
impl NativeTaskHandler for NativeSshTaskHandler {
    fn descriptor(&self) -> NativeTaskDescriptor {
        let names = self.targets.iter().map(|(id, target)| format!("{id} → node {}", target.seal().registered_node_id)).collect::<Vec<_>>().join(", ");
        NativeTaskDescriptor { kind: "ssh.native".into(), definition_version: 1,
            implementation_ref: ImplementationRef { plugin_id: Self::plugin_id(), artifact_digest: Digest::blake3(CONTRACT), registry_revision: 0 },
            role_revision: "native-russh-installer/1".into(),
            // Existing task/delegation evidence, not a synthetic policy grant.
            authority_refs: vec!["ssh.native.installed_capability/v1".into()],
            prompt_section: Some(format!("ssh.native/v1 已安装目标: {names}。使用 [delegate:ssh.native:1] {{\"target_id\":\"已安装目标\",\"decision_id\":\"node.selection.record 的真实 decision_id\",\"action\":{{\"kind\":\"exec\",\"argv\":[\"命令\"],\"stdin_b64\":\"\"}},\"timeout_secs\":30}}。action 也支持 upload(path,data_b64) 或 download(path)。执行前自行评估风险和收益，不传 host、account、私钥、路径授权或 provider。真实 OS/账号能力决定结果；选择记录不是授权或资源预约。超时/断线/未知结果只查询原操作，不盲目重复效果。传输结束或退出码不能证明任意命令业务成功。")) }
    }

    async fn execute(&self, invocation: NativeTaskInvocation) -> Result<OperationRecord> {
        // Match the existing native handler contract rather than trusting a
        // detached task snapshot with a different live invocation/context.
        let ctx = &invocation.task_invocation;
        let task = self.store.external_task(ctx.task_id)?;
        self.ensure_task(&task)?;
        ensure!(invocation.task.task_id == task.task_id
            && invocation.task.task_revision == task.task_revision
            && invocation.context.task_id == Some(ctx.task_id)
            && invocation.context.expected_task_revision == Some(ctx.task_revision.0)
            && invocation.context.invocation_id == Some(ctx.invocation_id)
            && invocation.context.controller_epoch == ctx.epoch
            && invocation.context.software_generation == ctx.software_generation,
            "ssh_trusted_invocation_mismatch");
        let input: SshInput = serde_json::from_value(invocation.input)
            .map_err(|_| anyhow::anyhow!("ssh_task_input_invalid"))?;
        let installation = self.targets.get(&input.target_id).context("ssh_target_not_installed")?;
        let limits = Self::limits(input.timeout_secs)?;
        // No queue extends the operation deadline and no hidden fallback target.
        let _slot = self.slots.try_acquire().map_err(|_| anyhow::anyhow!("ssh_local_transport_capacity_unavailable"))?;
        let runner = SshTaskRunner { store: &self.store };
        match runner.execute_selected_installed(&self.node, &invocation.task_invocation,
            invocation.operation_id, "native-task", invocation.authority, input.decision_id,
            installation, input.action, limits).await? {
            SelectedSshOutcome::Observed(observed) => Ok(observed.operation),
            SelectedSshOutcome::Unavailable(reason) => anyhow::bail!("ssh_selected_target_unavailable:{reason:?}"),
        }
    }

    async fn reconcile(&self, task: &TaskRecord, operation_id: EntityId) -> Result<OperationRecord> {
        self.ensure_task(task)?;
        let runner = SshTaskRunner { store: &self.store };
        let operation = self.store.external_operation(operation_id)?;
        ensure!(operation.task_id == task.task_id, "ssh_original_task_mismatch");
        if operation.state == shenko_types::operation::OperationState::Resolved {
            return Ok(operation);
        }
        let target = runner.original_target_id(operation_id)?;
        let installation = self.targets.get(&target).context("ssh_original_target_not_installed")?;
        let _slot = self.slots.try_acquire().map_err(|_| anyhow::anyhow!("ssh_local_transport_capacity_unavailable"))?;
        Ok(runner.query_installed(operation_id, installation, Self::limits(default_timeout())?).await?.operation)
    }
}
