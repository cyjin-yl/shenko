//! Replaceable commit authority, independent of a concrete election provider.
//!
//! The provider must reserve its actual durable authority while the returned
//! guard lives. A cached observation or a polling task does not implement this
//! contract. Store holds the guard until its admission transaction has ended.
use serde::{Deserialize, Serialize};

use crate::{error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError},
    commit::CommitRef,
    ids::{BranchId, ControllerEpoch, EntityId, RecordRevision, RuntimeGeneration},
    object::{Digest, ObjectRef}};

/// The immutable action a provider orders before a Store admission. The
/// conflict key identifies the decision position, so changing controller epoch
/// cannot manufacture another successor or another send for the same attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAdmission {
    pub schema_version: u32,
    pub subject_id: EntityId,
    pub branch_id: BranchId,
    pub conflict_key: String,
    /// Additional immutable decision positions consumed by this same action,
    /// such as a task predecessor revision shared by claim/cancel/T1/T2.
    /// The provider must reserve all keys atomically, before acknowledging the
    /// value. An empty field preserves retained descriptor bytes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_aliases: Vec<String>,
    /// Read dependencies such as an ancestor task's still-live revision.
    /// Reads do not consume a position; a later chosen writer does. A fresh
    /// dispatch must recheck them under the provider's final reservation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_unconsumed: Vec<String>,
    pub action_id: EntityId,
    pub expected_parent: Option<CommitRef>,
    pub expected_head_revision: Option<RecordRevision>,
    pub controller_epoch: ControllerEpoch,
    pub software_generation: RuntimeGeneration,
    pub operation_id: Option<EntityId>,
    pub attempt_id: Option<EntityId>,
    pub payload_ref: ObjectRef,
}

/// Verified content retained with a chosen admission. The descriptor alone
/// cannot recover a decision whose source disappears before Store applies it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAdmissionObject {
    pub reference: ObjectRef,
    pub bytes: Vec<u8>,
}

/// Provider-specific proof of an ordered admission. The provider verifies
/// `proof` under its actual reservation; the Store only binds it to the exact
/// descriptor. A replay is historical permission, never a new physical send.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAuthorityGrant {
    pub admission: CommitAdmission,
    pub grant_id: String,
    pub proof: serde_json::Value,
    pub replayed: bool,
}

/// An opaque, source-bound grant for one actual subject branch. Its revision
/// identifies the provider's complete commitment, not a wall-clock timestamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitAuthorityStamp {
    pub source_id: String,
    pub holder_id: EntityId,
    pub subject_id: EntityId,
    pub branch_id: BranchId,
    pub revision: Digest,
}

pub trait CommitAuthorityGuard {
    /// None means that this installation currently has no grant. Such a guard
    /// may still be held to serialize a deliberate recovery transition.
    fn stamp(&self) -> Option<&CommitAuthorityStamp>;

    fn installation_permit(&self) -> Option<&crate::installation::InstallationPermit> { None }
}

struct InstalledAuthorityGuard {
    // Drop the Node/source reservation before releasing physical exclusion.
    source: Box<dyn CommitAuthorityGuard>,
    permit: crate::installation::InstallationPermit,
}
impl CommitAuthorityGuard for InstalledAuthorityGuard {
    fn stamp(&self) -> Option<&CommitAuthorityStamp> { self.source.stamp() }
    fn installation_permit(&self) -> Option<&crate::installation::InstallationPermit> { Some(&self.permit) }
}

pub trait CommitAuthorityProvider: Send + Sync {
    /// Stable identity of the authority source. It must survive process and
    /// software replacement; another source cannot silently replace it.
    fn source_id(&self) -> &str;

    /// Order this immutable action before taking any Node/Store reservation.
    /// Connected-domain implementations acquire a real quorum-log grant.
    /// Explicit isolated branch implementations may return None: their local
    /// Store transaction orders the action, without claiming global quorum.
    fn prepare(&self, admission: &CommitAdmission)
        -> Result<Option<CommitAuthorityGrant>, ShenkoError>;

    /// A chosen action left its Store boundary without an applied marker.
    /// This notification must not block or acquire a database lock: it can run
    /// while the source reservation is still held. Runtime adapters use it to
    /// request their ordinary coherent recovery/rebuild, never to resend.
    fn action_unmaterialized(&self, _grant: &CommitAuthorityGrant) {}

    /// Acquire the source reservation BEFORE acquiring the Store write lock.
    /// Dropping the guard releases it. No external effect runs while held.
    fn acquire(&self, grant: Option<&CommitAuthorityGrant>)
        -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError>;

    /// Transfer the original physical permit into the source guard. An
    /// installed provider with its own database must override this method and
    /// borrow the permit for initialization/reservation, never reacquire it.
    fn acquire_installed(&self, grant: Option<&CommitAuthorityGrant>,
        permit: crate::installation::InstallationPermit)
        -> Result<Box<dyn CommitAuthorityGuard>, ShenkoError> {
        let source = self.acquire(grant)?;
        Ok(Box::new(InstalledAuthorityGuard { source, permit }))
    }
}

pub fn authority_fenced(reason: impl Into<String>) -> ShenkoError {
    ProtocolError::new("runtime_authority_fenced", ErrorScope::Writer,
        RetryStrategy::AfterReconcile)
        .with_details(serde_json::json!({"reason": reason.into()})).into()
}

pub fn is_authority_fenced(error: &ShenkoError) -> bool {
    matches!(error, ShenkoError::Protocol(error) if error.code == "runtime_authority_fenced")
}
