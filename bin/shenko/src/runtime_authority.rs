//! Node-backed runtime authority at the Store's actual admission boundary.
//!
//! Consensus runs before either database is reserved. The final Node RESERVED
//! lock is held across the Store transaction; a quorum grant binds one exact
//! action, while an availability branch has only its explicit local authority.
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use shenko_node::{ControlAdmissionError, ControlEntry, Node, NodeAuthorityGuard};
use shenko_store::Store;
use shenko_types::{
    authority::{
        CommitAdmission, CommitAuthorityGrant, CommitAuthorityGuard, CommitAuthorityProvider,
        CommitAuthorityStamp, authority_fenced,
    },
    error::ShenkoError,
    ids::{BranchId, EntityId},
    object::Digest,
};
use tokio_util::sync::CancellationToken;

pub struct RuntimeAuthority {
    data_dir: PathBuf,
    store: Weak<Store>,
    source_id: String,
    cancel: CancellationToken,
    active: AtomicBool,
    recovery: Mutex<Option<String>>,
}

#[derive(Debug)]
pub struct AuthorityRecoveryRequired(pub String);

impl std::fmt::Display for AuthorityRecoveryRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime authority requires reconstruction: {}", self.0)
    }
}
impl std::error::Error for AuthorityRecoveryRequired {}

fn has_storage_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ShenkoError>()
        .is_some_and(|error| matches!(error, ShenkoError::Storage(_)))
        || error.chain().any(|cause| {
            cause
                .downcast_ref::<ShenkoError>()
                .is_some_and(|error| matches!(error, ShenkoError::Storage(_)))
        })
}

pub(crate) fn requires_reconstruction(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AuthorityRecoveryRequired>().is_some() && !has_storage_failure(error)
}

impl RuntimeAuthority {
    pub fn attach(
        data_dir: &Path,
        store: &Arc<Store>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>> {
        let node = crate::node_sync::open_node(data_dir, store)?;
        let node_id = node
            .local_node_id()?
            .context("runtime Node is not initialized")?;
        let provider = Arc::new(Self {
            data_dir: data_dir.to_owned(),
            store: Arc::downgrade(store),
            source_id: format!("node:{node_id}"),
            cancel,
            active: AtomicBool::new(false),
            recovery: Mutex::new(None),
        });
        store.attach_commit_authority(provider.clone())?;
        Ok(provider)
    }

    /// Run before constructing any branch-scoped consumer. A chosen foreign
    /// plan is evidence, not permission to replay it into a different ledger.
    pub fn recover_before_consumers(&self) -> Result<bool> {
        let store = self
            .store
            .upgrade()
            .context("authority Store was disposed")?;
        let node = crate::node_sync::open_node(&self.data_dir, &store)?;
        let guard = node.reserve_authority()?;
        let snapshot = guard.snapshot().clone();
        let selected_here = !is_availability(&snapshot)
            && snapshot
                .fence
                .as_ref()
                .is_some_and(|fence| fence.leader_id == snapshot.node_id);
        drop(guard);
        let mut node = crate::node_sync::open_node(&self.data_dir, &store)?;
        let mut unresolved = None;
        for grant in node.committed_admissions(0)? {
            if grant.admission.subject_id != snapshot.identity.agent_id
                || store.control_admission_applied_here(&grant.admission)?
            {
                continue;
            }
            let current_branch = grant.admission.branch_id.0 == snapshot.identity.branch_id.0;
            let objects = node.admission_objects(&grant.admission.payload_ref)?;
            if store.recover_control_admission(&grant, &objects, selected_here && current_branch)?
                == shenko_store::ControlRecoveryOutcome::ReconciliationRequired
                && current_branch
            {
                unresolved.get_or_insert(grant.grant_id);
            }
        }
        if let Some(grant_id) = unresolved.filter(|_| selected_here) {
            // Stable across a crash between the Node fact, Store fork and
            // Node identity projection. Root's exact receipt repair finishes
            // only this parent→child transition on reopen.
            let branch_id = EntityId::derived(
                "unmaterialized-control-branch-v1",
                format!("{}:{grant_id}", snapshot.identity.branch_id).as_bytes(),
            );
            let reason = format!("selected-plan recovery unavailable: {grant_id}");
            node.open_availability_branch_if_current(
                branch_id,
                Some(snapshot.identity.agent_id),
                &reason,
                &snapshot,
            )?;
            drop(node);
            let reservation = Node::open(&self.data_dir.join("node"))?.reserve_authority()?;
            let current = reservation.snapshot();
            if current.identity != snapshot.identity
                || current.fence != snapshot.fence
                || current
                    .fence
                    .as_ref()
                    .is_none_or(|fence| fence.leader_id != current.node_id)
            {
                return Err(AuthorityRecoveryRequired(
                    "selected primary changed before its recovery fork".into(),
                )
                .into());
            }
            reservation.validate_availability_branch(branch_id)?;
            let (parent, _) = store.load_subject(snapshot.identity.agent_id)?;
            anyhow::ensure!(
                parent.branch_id.0 == snapshot.identity.branch_id.0,
                "Store branch changed before selected-plan recovery"
            );
            let fork = store.fork_subject_branch(&parent, BranchId(branch_id.0), &reason)?;
            let head = store.load_subject(snapshot.identity.agent_id)?.0;
            reservation.project_subject_branch(&shenko_node::SubjectBranchProjection {
                parent: snapshot.identity.clone(),
                branch_id,
                store_commit: fork.created_commit,
                fork_receipt: head
                    .branch_origin
                    .context("recovery fork receipt missing")?,
            })?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn activate(&self) -> Result<()> {
        let store = self
            .store
            .upgrade()
            .context("authority Store was disposed")?;
        let agent = store
            .get_existing_agent_id()?
            .context("authority has no subject")?;
        let (head, _) = store.load_subject(agent)?;
        store.activate_runtime_authority(&head)?;
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    /// Called only after the ordinary host cleanup. The next owned Start
    /// reconstructs every branch-scoped service from durable facts.
    pub fn recovery_required(&self) -> Option<AuthorityRecoveryRequired> {
        self.recovery.lock().clone().map(AuthorityRecoveryRequired)
    }

    pub fn finish_result(&self, result: Result<()>) -> Result<()> {
        // A storage failure remains a storage failure; restarting authority
        // must not hide a closed or corrupt ledger behind a network retry.
        if self
            .store
            .upgrade()
            .is_some_and(|store| !store.ledger.writer_is_open())
        {
            let failure = ShenkoError::storage(
                "the Store writer closed during runtime work; its original failure requires recovery",
            );
            return match result {
                Ok(()) => Err(failure.into()),
                Err(error) => Err(error.context(failure)),
            };
        }
        if result.as_ref().err().is_some_and(has_storage_failure) {
            return result;
        }
        match self.recovery_required() {
            Some(recovery) => match result {
                Ok(()) => Err(recovery.into()),
                Err(error) => Err(error.context(recovery)),
            },
            None => result,
        }
    }

    fn request_recovery(&self, reason: impl Into<String>) {
        if self.active.load(Ordering::Acquire) {
            self.recovery.lock().get_or_insert_with(|| reason.into());
            self.cancel.cancel();
        }
    }

    fn failure(&self, error: anyhow::Error, term: u64) -> ShenkoError {
        let recover = match error.downcast_ref::<ControlAdmissionError>() {
            Some(
                ControlAdmissionError::AuthorityUnavailable(_)
                | ControlAdmissionError::QuorumUnavailable(_)
                | ControlAdmissionError::RecoveryRequired { .. },
            ) => true,
            Some(ControlAdmissionError::ExistingAdmission { original }) => {
                // A preceding authority's chosen action must be recovered.
                // Conflicting payloads produced by this same authority remain
                // errors: restarting must not hide a malformed proposal.
                serde_json::from_value::<ControlEntry>(original.proof.clone())
                    .is_ok_and(|entry| entry.term < term)
            }
            None => false,
        };
        if recover {
            let reason = error.to_string();
            self.request_recovery(reason.clone());
            authority_fenced(reason)
        } else {
            ShenkoError::other(format!(
                "commit authority refused the immutable action: {error:#}"
            ))
        }
    }
}

struct ReservedAuthority {
    _reservation: NodeAuthorityGuard,
    stamp: Option<CommitAuthorityStamp>,
}

impl CommitAuthorityGuard for ReservedAuthority {
    fn stamp(&self) -> Option<&CommitAuthorityStamp> {
        self.stamp.as_ref()
    }
}

fn is_availability(snapshot: &shenko_node::NodeAuthoritySnapshot) -> bool {
    snapshot.is_current_availability()
}

impl CommitAuthorityProvider for RuntimeAuthority {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn prepare(
        &self,
        admission: &CommitAdmission,
    ) -> Result<Option<CommitAuthorityGrant>, ShenkoError> {
        if self.active.load(Ordering::Acquire) && self.cancel.is_cancelled() {
            return Err(authority_fenced("runtime admission is canceled"));
        }
        let node =
            Node::open(&self.data_dir.join("node")).map_err(|error| self.failure(error, 0))?;
        let reservation = node
            .reserve_authority()
            .map_err(|error| self.failure(error, 0))?;
        let snapshot = reservation.snapshot();
        let term = snapshot.term;
        let isolated = is_availability(snapshot);
        let matches = admission.subject_id == snapshot.identity.agent_id
            && admission.branch_id.0 == snapshot.identity.branch_id.0;
        drop(reservation);
        if !matches {
            return Err(authority_fenced(
                "admission belongs to another actual Node branch",
            ));
        }
        if isolated {
            return Ok(None);
        }
        let store = self
            .store
            .upgrade()
            .ok_or_else(|| authority_fenced("authority Store was disposed"))?;
        let objects = store
            .admission_objects(&admission.payload_ref)
            .map_err(|error| self.failure(error, term))?;
        let grant = crate::node_sync::commit_admission(
            &self.data_dir,
            &store,
            admission,
            &objects,
            &self.cancel,
        )
        .map_err(|error| self.failure(error, term))?;
        if grant.replayed && self.active.load(Ordering::Acquire) {
            self.request_recovery(
                "an original chosen action must be recovered before new runtime work",
            );
            return Err(authority_fenced(
                "a replayed grant cannot renew a live dispatch",
            ));
        }
        Ok(Some(grant))
    }

    fn action_unmaterialized(&self, grant: &CommitAuthorityGrant) {
        self.request_recovery(format!(
            "chosen action {} has no local applied marker",
            grant.grant_id
        ));
    }

    fn acquire(
        &self,
        grant: Option<&CommitAuthorityGrant>,
    ) -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        let reservation = Node::open(&self.data_dir.join("node"))
            .and_then(Node::reserve_authority)
            .map_err(|error| self.failure(error, 0))?;
        let snapshot = reservation.snapshot();
        if let Some(grant) = grant {
            reservation
                .validate_admission_grant(grant)
                .map_err(|error| self.failure(error, snapshot.term))?;
        }
        let source = if is_availability(snapshot) {
            let store = self
                .store
                .upgrade()
                .ok_or_else(|| authority_fenced("authority Store was disposed"))?;
            let (head, _) = store.load_subject(snapshot.identity.agent_id)?;
            if head.branch_id.0 != snapshot.identity.branch_id.0 || head.branch_origin.is_none() {
                return Err(authority_fenced(
                    "availability authority has no actual retained Store fork",
                ));
            }
            let branch = snapshot
                .branch
                .as_ref()
                .ok_or_else(|| authority_fenced("availability branch missing"))?;
            Some(
                serde_json::json!({"mode": "availability", "identity": snapshot.identity,
                "node_id": snapshot.node_id, "opened_at": branch.opened_at,
                "opening_authority": branch.availability_authority,
                "reconciliation_floor": branch.reconciliation_floor}),
            )
        } else {
            snapshot.fence.as_ref().filter(|fence| fence.leader_id == snapshot.node_id)
                .map(|fence| serde_json::json!({"mode": "quorum", "identity": snapshot.identity,
                    "node_id": snapshot.node_id, "fence": fence, "configuration": snapshot.configuration}))
        };
        let mut stamp = source
            .map(|source| -> Result<CommitAuthorityStamp, ShenkoError> {
                Ok(CommitAuthorityStamp {
                    source_id: self.source_id.clone(),
                    holder_id: snapshot.node_id,
                    subject_id: snapshot.identity.agent_id,
                    branch_id: BranchId(snapshot.identity.branch_id.0),
                    revision: Digest::blake3(&serde_json::to_vec(&source)?),
                })
            })
            .transpose()?;
        if self.active.load(Ordering::Acquire) && !self.cancel.is_cancelled() {
            let store = self
                .store
                .upgrade()
                .ok_or_else(|| authority_fenced("authority Store was disposed"))?;
            let binding = store.subject_authority_binding(snapshot.identity.agent_id)?;
            if stamp.as_ref().is_none_or(|stamp| {
                binding.as_ref().is_none_or(|binding| {
                    binding.stamp != *stamp
                        || binding.writer_incarnation != store.capabilities().incarnation
                })
            }) {
                self.request_recovery(
                    "the reserved Node authority or local writer binding changed",
                );
            }
        }
        if self.active.load(Ordering::Acquire) && self.cancel.is_cancelled() {
            stamp = None;
        }
        Ok(Box::new(ReservedAuthority {
            _reservation: reservation,
            stamp,
        }))
    }
}

/// Capture only Start's inputs; credentials stay in memory and are never
/// serialized into the recovery journal or printed as command arguments.
pub fn restart_command(command: &crate::Command) -> Option<crate::Command> {
    let crate::Command::Start {
        port,
        name,
        napcat,
        napcat_account,
        napcat_access_token,
        napcat_admin_qq,
        napcat_file_root,
        napcat_file_host,
        napcat_cli_url,
        channels_config,
        ssh_targets,
    } = command
    else {
        return None;
    };
    Some(crate::Command::Start {
        port: *port,
        name: name.clone(),
        napcat: napcat.clone(),
        napcat_account: *napcat_account,
        napcat_access_token: napcat_access_token.clone(),
        napcat_admin_qq: napcat_admin_qq.clone(),
        napcat_file_root: napcat_file_root.clone(),
        napcat_file_host: napcat_file_host.clone(),
        napcat_cli_url: napcat_cli_url.clone(),
        channels_config: channels_config.clone(),
        ssh_targets: ssh_targets.clone(),
    })
}

#[cfg(test)]
mod tests {
    include!("runtime_authority_tests.rs");
}
