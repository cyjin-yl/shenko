//! Composition adapters: use the real Node authority and native worker drain.
use std::{path::PathBuf, sync::Arc};
use anyhow::{Context, Result, ensure};
use shenko_store::Store;
use shenko_types::{activation::PrimaryClaim, ids::EntityId};
use shenko_update::handoff::{PrimaryClaimSource, HandoffParticipant, HandoffParticipantReceipt};

pub struct NodeAuthority {
    pub data_dir: PathBuf,
    pub store: Arc<Store>,
    pub agent_id: EntityId,
}

impl PrimaryClaimSource for NodeAuthority {
    fn committed_primary_claim(&self) -> Result<Option<PrimaryClaim>> {
        let node = crate::node_sync::open_node(&self.data_dir, &self.store)?;
        if !node.is_primary()? { return Ok(None); }
        let claim = node.committed_primary_claim()?.context("local primary has no committed claim")?;
        // The node's own resource-side validation is authoritative; don't
        // infer a lease merely from a truthful subject identity binding.
        node.check_fencing(&node.fencing_token()?.context("primary fencing token missing")?)?;
        Ok(Some(PrimaryClaim {
            term: claim.term, epoch: claim.epoch,
            leader_node_id: claim.leader_node_id,
            config_revision: claim.config_revision,
            committed_seq: claim.committed_seq,
        }))
    }

    fn local_branch_authority(&self) -> Result<Option<serde_json::Value>> {
        let node = crate::node_sync::open_node(&self.data_dir, &self.store)?;
        if node.is_primary()? { return Ok(None); }
        let (head, _) = self.store.load_subject(self.agent_id)?;
        if node.member_configuration()?.is_none() && node.sync_peers()?.is_empty() {
            return Ok(Some(serde_json::json!({
                "kind": "unconfigured_local_installation", "global_quorum": false,
                "node_id": node.local_node_id()?, "subject_id": self.agent_id,
                "branch_id": head.branch_id, "controller_epoch": head.controller_epoch,
                "scope": "this installation's local slot and retained facts only"
            })));
        }
        let branch = node.branches()?.into_iter().find(|branch| {
            branch.kind == shenko_node::BranchKind::Availability
                && !branch.global_quorum
                && branch.subject_id == Some(self.agent_id)
                && branch.branch_id.to_string() == head.branch_id.0.to_string()
        });
        let Some(branch) = branch else { return Ok(None); };
        let identity = node.local_identity()?.context("availability branch lacks local identity")?;
        ensure!(identity.agent_id == self.agent_id
            && identity.branch_id.to_string() == branch.branch_id.to_string(),
            "availability branch does not own this subject head");
        Ok(Some(serde_json::json!({
            "kind": "local_availability_branch", "global_quorum": false,
            "node_id": node.local_node_id()?, "branch_id": branch.branch_id,
            "subject_id": self.agent_id, "controller_epoch": head.controller_epoch,
            "scope": "this installation's local slot and retained facts only"
        })))
    }
}

pub struct NativeWorkers(pub Arc<shenko_subject::NativeChildService>);
#[async_trait::async_trait]
impl HandoffParticipant for NativeWorkers {
    fn name(&self) -> &str { "native-children" }
    async fn quiesce(&self) -> Result<HandoffParticipantReceipt> {
        // Orchestration runs outside a native worker, so no worker needs an
        // exemption from this drain. Original armed effects remain debt.
        let details = self.0.quiesce_for_handoff(&[]).await?;
        Ok(HandoffParticipantReceipt {
            participant: self.name().into(), stopped: Vec::new(), details,
        })
    }
    async fn resume(&self) -> Result<()> {
        self.0.resume_after_handoff_abort().await
    }
}

pub struct PlaygroundServices(pub Arc<shenko_ui::playground_backend::ArtifactServiceController>);
#[async_trait::async_trait]
impl HandoffParticipant for PlaygroundServices {
    fn name(&self) -> &str { "playground-services" }
    async fn quiesce(&self) -> Result<HandoffParticipantReceipt> {
        let stopped = self.0.quiesce_for_handoff()?;
        Ok(HandoffParticipantReceipt { participant: self.name().into(),
            stopped: stopped.iter().map(|identity| format!("{}:{}:{}", identity.pid,
                identity.process_start_token, identity.executable_digest)).collect(),
            details: serde_json::to_value(stopped)?, })
    }
    async fn resume(&self) -> Result<()> { self.0.resume_after_abandoned_activation() }
}

pub struct ReplSessions(pub Arc<shenko_repl::task::SessionStore>);
#[async_trait::async_trait]
impl HandoffParticipant for ReplSessions {
    fn name(&self) -> &str { "repl-interpreters" }
    async fn quiesce(&self) -> Result<HandoffParticipantReceipt> {
        let details = serde_json::to_value(self.0.quiesce_for_upgrade()?)?;
        Ok(HandoffParticipantReceipt { participant: self.name().into(), stopped: Vec::new(), details })
    }
    async fn resume(&self) -> Result<()> { self.0.resume_after_upgrade_abort() }
}

pub struct InferenceDomain(pub shenko_broker::broker::BrokerHandle);
#[async_trait::async_trait]
impl HandoffParticipant for InferenceDomain {
    fn name(&self) -> &str { "physical-inference-domain" }
    async fn quiesce(&self) -> Result<HandoffParticipantReceipt> {
        let details = self.0.quiesce_for_handoff().await?;
        Ok(HandoffParticipantReceipt { participant: self.name().into(), stopped: Vec::new(), details })
    }
    async fn resume(&self) -> Result<()> { self.0.resume_after_handoff_abort(); Ok(()) }
}
