//! Real membership, election, quorum and fencing for one Node.
//!
//! SPEC §5.4.1–§5.4.3, §22: the current primary of a connected domain is
//! decided by a committed *member configuration* plus a quorum of verified vote
//! receipts. The number of nodes this process happens to see is never the
//! quorum, a heartbeat is never an election, and one replicated agent process
//! is never a second vote.
//!
//! What this module deliberately does not do:
//! - it does not measure peer liveness or guess reachability from a timeout;
//! - it does not copy credentials, provider grants, model weights or any
//!   secret handle between nodes;
//! - it does not require a member to run a model, a local LLM or a particular
//!   operating system (SPEC §5.4.1: an Android node reaching an external API
//!   is a capable member; a local LLM is not an eligibility gate);
//! - it does not merge parallel experience by last-write-wins: conflicting
//!   facts are retained side by side for the subject to judge (SPEC §5.4.2).
//!
//! Every committed decision carries a monotonic fencing epoch. The resource end
//! checks the epoch before accepting an authority-bearing write, so an instance
//! that lost the election cannot keep acting as primary once a newer epoch is
//! committed locally.

use super::{Event, Identity, Manifest, Node, ResultReceipt, digest, get, number, set, text};
use crate::cognition::{CognitionComparison, CognitionOrigin};
use anyhow::{Context as _, Result, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::{ids::EntityId, object::Digest};
use std::collections::BTreeSet;

/// Tables owned by this module; additive node storage schema 6.
pub(crate) const ELECTION_SCHEMA: &str = "
CREATE TABLE node_election_state(k TEXT PRIMARY KEY,v TEXT NOT NULL);
CREATE TABLE node_control_log(idx INTEGER PRIMARY KEY,term INTEGER NOT NULL,kind TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE node_member_configs(revision INTEGER PRIMARY KEY,term INTEGER NOT NULL,data TEXT NOT NULL);
CREATE TABLE node_votes(term INTEGER NOT NULL,scope TEXT NOT NULL,voter TEXT NOT NULL,candidate TEXT NOT NULL,data TEXT NOT NULL,received_at TEXT NOT NULL,PRIMARY KEY(term,scope,voter));
CREATE TABLE node_branches(branch_id TEXT PRIMARY KEY,data TEXT NOT NULL);
CREATE TABLE node_reconciliations(seq INTEGER PRIMARY KEY AUTOINCREMENT,data TEXT NOT NULL);
CREATE TABLE node_assignment_fences(task TEXT NOT NULL,epoch TEXT NOT NULL,election_term TEXT NOT NULL,election_epoch TEXT NOT NULL,PRIMARY KEY(task,epoch));
CREATE TABLE node_observation_fences(observation_id TEXT PRIMARY KEY,data TEXT NOT NULL);";

/// Additive node storage schema 8: the divergent control-log entries a
/// follower refused, kept side by side as evidence. Each refused append
/// records two rows under one conflict identity: the record this node
/// committed (`committed`) and the record the leader offered for the same
/// index (`offered`). Neither side is ever deleted or overwritten.
pub(crate) const CONTROL_CONFLICT_SCHEMA: &str = "
CREATE TABLE node_control_log_conflicts(seq INTEGER PRIMARY KEY AUTOINCREMENT,conflict_id TEXT NOT NULL,idx INTEGER NOT NULL,side TEXT NOT NULL,data TEXT NOT NULL,UNIQUE(conflict_id,side));";

/// Wire schema of the election control plane. Independent of the closure
/// replication framing: a vote is a control fact, not a memory transfer.
pub const ELECTION_WIRE_SCHEMA: u32 = 1;

/// One voting identity. Replicating the same identity does not add a vote.
pub type VoterId = EntityId;

/// A committed membership configuration. `members` is the current voting set;
/// `joint_new` is the configuration being transitioned to (joint consensus).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemberConfiguration {
    pub revision: u64,
    pub term: u64,
    pub members: Vec<VoterId>,
    pub joint_new: Option<Vec<VoterId>>,
    pub digest: Digest,
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

/// The quorum a decision needs, always derived from the committed
/// configuration and never from the peers this process can currently see.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Quorum {
    pub members: usize,
    pub required: usize,
    pub joint: Option<JointQuorum>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JointQuorum {
    pub old_members: usize,
    pub old_required: usize,
    pub new_members: usize,
    pub new_required: usize,
}

fn counted(members: &[VoterId], votes: &BTreeSet<VoterId>) -> usize {
    members.iter().filter(|id| votes.contains(id)).count()
}

fn majority(members: &[VoterId]) -> usize {
    members.len() / 2 + 1
}

impl MemberConfiguration {
    /// Every identity that may vote on this configuration (old ∪ new while a
    /// joint transition is in force).
    pub fn voters(&self) -> BTreeSet<VoterId> {
        let mut set: BTreeSet<VoterId> = self.members.iter().copied().collect();
        if let Some(new) = &self.joint_new {
            set.extend(new.iter().copied());
        }
        set
    }

    pub fn is_member(&self, id: VoterId) -> bool {
        self.voters().contains(&id)
    }

    pub fn quorum(&self) -> Quorum {
        let members = self.members.len();
        let required = majority(&self.members);
        let joint = self.joint_new.as_ref().map(|new| JointQuorum {
            old_members: members,
            old_required: required,
            new_members: new.len(),
            new_required: majority(new),
        });
        Quorum {
            members,
            required,
            joint,
        }
    }

    /// A majority of every configured set. Votes from identities outside the
    /// configuration never count.
    pub fn has_quorum(&self, votes: &BTreeSet<VoterId>) -> bool {
        if counted(&self.members, votes) < majority(&self.members) {
            return false;
        }
        match &self.joint_new {
            None => true,
            Some(new) => counted(new, votes) >= majority(new),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ElectionRole {
    Follower,
    Candidate,
    Leader,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Primary,
    MemberConfiguration,
}

/// The committed primary of a domain. Read by the upgrade/selector path and
/// by the resource-side fence; it exists only after a verified quorum.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PrimaryClaim {
    pub term: u64,
    pub epoch: u64,
    pub leader_node_id: String,
    pub config_revision: u64,
    pub committed_seq: u64,
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

/// The fencing token a writer presents to the resource end.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FencingToken {
    pub term: u64,
    pub epoch: u64,
    pub leader_id: VoterId,
    pub config_revision: u64,
    pub committed_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuorumCertificate {
    pub term: u64,
    pub voters: Vec<VoterId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrimaryEntry {
    pub leader_id: VoterId,
    pub epoch: u64,
    pub config_revision: u64,
    pub config_digest: Digest,
    pub members: Vec<VoterId>,
    pub certificate: QuorumCertificate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationEntry {
    pub revision: u64,
    pub members: Vec<VoterId>,
    pub joint_new: Option<Vec<VoterId>>,
    pub digest: Digest,
    pub certificate: QuorumCertificate,
    pub proposal_old: Vec<VoterId>,
    pub proposal_new: Vec<VoterId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlEntry {
    pub index: u64,
    pub term: u64,
    pub kind: ControlKind,
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProposalKind {
    Joint,
    Final,
}

/// A membership change proposal. The content travels with the vote request so
/// a peer can check the certificate against the exact sets it approves.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberProposal {
    pub kind: ProposalKind,
    pub base_revision: u64,
    pub base_digest: Digest,
    pub old_members: Vec<VoterId>,
    pub new_members: Vec<VoterId>,
    pub digest: Digest,
}

impl MemberProposal {
    /// Joint consensus: a majority of the old set *and* a majority of the new
    /// set. No member can be removed or added without that arithmetic.
    pub fn has_quorum(&self, votes: &BTreeSet<VoterId>) -> bool {
        counted(&self.old_members, votes) >= majority(&self.old_members)
            && counted(&self.new_members, votes) >= majority(&self.new_members)
    }

    pub fn scope(&self) -> String {
        format!("proposal:{}", self.digest.0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteRequest {
    pub schema: u32,
    pub request_id: EntityId,
    pub term: u64,
    pub candidate_id: VoterId,
    pub config_revision: u64,
    pub config_digest: Digest,
    pub last_log_index: u64,
    pub last_log_term: u64,
    pub proposal: Option<MemberProposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteResponse {
    pub schema: u32,
    pub request_id: EntityId,
    pub responder_id: VoterId,
    pub term: u64,
    pub vote_granted: bool,
    pub config_digest: Digest,
    pub proposal_digest: Option<Digest>,
    pub reason: String,
}

/// Leader → follower log replication, including the committed quorum
/// certificate that lets the follower verify the decision itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendRequest {
    pub schema: u32,
    pub leader_id: VoterId,
    pub prev_index: u64,
    pub prev_term: u64,
    pub entries: Vec<ControlEntry>,
    pub proposal: Option<MemberProposal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendOutcome {
    pub accepted: usize,
    pub commit_index: u64,
    pub reason: String,
}

/// Which side of a control-log divergence one evidence row retains.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControlConflictSide {
    /// The record this node committed at the index.
    Committed,
    /// The record a leader offered for the same index.
    Offered,
}

/// One retained side of a control-log divergence: an offered entry that
/// names an index this node already committed with a different term or a
/// different decision. Both sides of a divergence are kept so the fact is
/// diagnosable later; the committed record is never deleted or overwritten.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlLogConflict {
    /// Content-derived identity shared by both sides of one divergence.
    pub conflict_id: Digest,
    pub index: u64,
    pub side: ControlConflictSide,
    pub entry: ControlEntry,
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberChangeRequest {
    pub operation_id: EntityId,
    pub expected_revision: u64,
    pub add: Vec<VoterId>,
    pub remove: Vec<VoterId>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberChangeRound {
    pub phase: ProposalKind,
    pub proposal: MemberProposal,
    pub request: VoteRequest,
    /// Members that must be asked for a vote in this phase.
    pub members: Vec<VoterId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberChangeOutcome {
    pub phase: ProposalKind,
    pub quorum_met: bool,
    pub votes: Vec<VoterId>,
    pub committed: Option<MemberConfiguration>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Campaign {
    pub term: u64,
    pub candidate_id: VoterId,
    pub request: VoteRequest,
    /// Configured members that must be asked for a vote.
    pub members: Vec<VoterId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ElectionOutcome {
    pub term: u64,
    pub candidate_id: VoterId,
    pub quorum: Quorum,
    pub votes: Vec<VoterId>,
    pub elected: bool,
    pub claim: Option<PrimaryClaim>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoteOutcome {
    pub voter: VoterId,
    pub outcome: String,
    pub reason: String,
}

/// A branch this node actually used. A partition branch keeps its own
/// experience; it is never erased by an election.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    Primary,
    Availability,
    AdoptedExperience,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperienceCounts {
    pub tasks: u64,
    pub events: u64,
    pub work: u64,
    pub receipts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchFact {
    pub branch_id: EntityId,
    pub kind: BranchKind,
    pub subject_id: Option<EntityId>,
    pub reason: String,
    /// An availability branch is never a global quorum claim.
    pub global_quorum: bool,
    pub retained_experience: ExperienceCounts,
    pub opened_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// Present once this partition branch was merged back into the domain.
    /// The branch then produces no new decisions and is retained as an
    /// archived branch with this fact, never reset (SPEC §5.4.2). `kind` keeps
    /// saying which role the branch played; `global_quorum` still follows the
    /// committed claim, so an archived branch without a claim stays an
    /// availability branch rather than becoming an authority.
    #[serde(default)]
    pub merged: Option<BranchMerge>,
    /// The reconciliation journal sequence that already existed when this
    /// branch last became a partition branch. A merge must be recorded after
    /// this point: an older reconciliation does not cover the branch.
    #[serde(default)]
    pub reconciliation_floor: u64,
}

/// What ended a partition branch: the reconciliation that merged its
/// experience and the committed facts that make the domain's authority
/// explicit again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BranchMerge {
    pub reconciliation_id: EntityId,
    pub peer_node_id: VoterId,
    /// The peer's event watermark covered by the merge; `None` when the peer
    /// exported no events.
    pub peer_high_water: Option<String>,
    /// The committed primary claim of the domain after the merge. `None` means
    /// the domain has no committed claim: the merge does not invent a quorum.
    pub primary: Option<PrimaryClaim>,
    pub note: String,
    pub merged_at: chrono::DateTime<chrono::Utc>,
}

/// Adoption of an existing self: the local independent identity stops
/// producing new decisions, its experience is retained, and the fact is
/// recorded with its provenance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptRequest {
    pub operation_id: EntityId,
    pub trusted_identity: Identity,
    pub trusted_node_id: VoterId,
    /// How the trusted identity was actually verified (authenticated channel).
    pub provenance: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdoptionRecord {
    pub adoption_id: EntityId,
    pub operation_id: EntityId,
    pub adopted: Identity,
    pub retired: Option<Identity>,
    pub trusted_node_id: VoterId,
    pub provenance: String,
    pub reason: String,
    pub retained_experience: ExperienceCounts,
    pub adopted_at: chrono::DateTime<chrono::Utc>,
}

/// One peer's verified experience offered for reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerExperience {
    pub peer_node_id: VoterId,
    pub peer_branch_id: Option<EntityId>,
    pub events: Vec<Event>,
    pub receipts: Vec<ResultReceipt>,
    /// The peer installation's own Store observation: the snapshot
    /// fingerprint and the ledger watermark it covered. `None` means the peer
    /// published no observation, which is unknown — not "unchanged", not
    /// "empty" and not evidence about the peer's liveness or capacity.
    pub cognition_fingerprint: Option<Digest>,
    pub cognition_high_water: Option<u64>,
    /// Scope, revision and observation instant of the report above. Required
    /// whenever `cognition_fingerprint` is present: a fingerprint without the
    /// coverage it describes is not comparable to anything.
    #[serde(default)]
    pub cognition_origin: Option<CognitionOrigin>,
    /// The peer's event watermark this experience was exported through. It is
    /// the source's real progress fact, never a claim about its reachability.
    #[serde(default)]
    pub peer_high_water: Option<String>,
    /// The manifest the peer's process actually ran with. A heterogeneous peer
    /// (another OS/arch/artifact generation) is retained as such instead of
    /// being adopted as this node's own capability fact (SPEC §5.4.4).
    #[serde(default)]
    pub peer_manifest: Option<Manifest>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParallelFact {
    pub event_id: EntityId,
    pub local_digest: Digest,
    pub remote_digest: Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectReconciliation {
    pub task_id: EntityId,
    pub classification: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationRecord {
    pub reconciliation_id: EntityId,
    pub peer_node_id: VoterId,
    pub peer_branch_id: Option<EntityId>,
    pub imported_events: u64,
    pub duplicate_events: u64,
    pub parallel_facts: Vec<ParallelFact>,
    pub effects: Vec<EffectReconciliation>,
    pub cognition_fingerprint: Option<Digest>,
    pub cognition_high_water: Option<u64>,
    /// Scope, revision and observation instant of the merged peer's
    /// cognition report. `None` when the peer published no observation.
    #[serde(default)]
    pub cognition_origin: Option<CognitionOrigin>,
    /// How the peer's snapshot relates to this node's own published one. The
    /// comparison says two installations hold the same replicated snapshot or
    /// not; it is not evidence of liveness, capacity or capability equivalence.
    /// `None` only for records written before this field existed.
    #[serde(default)]
    pub cognition_comparison: Option<CognitionComparison>,
    /// The peer watermark this merge covered. A later rejoin can state what it
    /// already merged instead of re-deriving it from the event journal.
    #[serde(default)]
    pub peer_high_water: Option<String>,
    /// The manifest of the process that produced the merged facts.
    #[serde(default)]
    pub peer_manifest: Option<Manifest>,
    /// `Some(true)` when the peer ran a different artifact/OS/arch/schema
    /// generation than this node; `None` when that was not observable.
    #[serde(default)]
    pub heterogeneous: Option<bool>,
    /// Events the peer offered whose digest did not match their own content.
    /// They are not merged, and the count is the honest report of that.
    #[serde(default)]
    pub rejected_events: u64,
    /// Parallel facts retained in this node's conflict journal after the
    /// merge. Conflicting facts are kept side by side, never last-write-wins.
    #[serde(default)]
    pub retained_conflicts: u64,
    pub note: String,
    pub reconciled_at: chrono::DateTime<chrono::Utc>,
}

fn config_digest(members: &[VoterId], joint_new: Option<&[VoterId]>) -> Result<Digest> {
    let payload = json!({"members": members, "joint_new": joint_new});
    Ok(Digest::blake3(&serde_json::to_vec(&payload)?))
}

fn proposal_digest(proposal: &MemberProposal) -> Result<Digest> {
    let payload = json!({
        "kind": proposal.kind,
        "base_revision": proposal.base_revision,
        "base_digest": proposal.base_digest,
        "old_members": proposal.old_members,
        "new_members": proposal.new_members,
    });
    Ok(Digest::blake3(&serde_json::to_vec(&payload)?))
}

fn state_number(conn: &Connection, key: &str) -> Result<u64> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT v FROM node_election_state WHERE k=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    match raw {
        // The state table stores JSON, so a numeric state is a JSON string of
        // its canonical decimal form. Reading it as a bare decimal made every
        // election term and commit index unreadable.
        Some(value) => number(&serde_json::from_str::<String>(&value).unwrap_or(value)),
        None => Ok(0),
    }
}

fn set_state<T: Serialize>(tx: &Transaction<'_>, key: &str, value: &T) -> Result<()> {
    tx.execute(
        "INSERT INTO node_election_state(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        params![key, text(value)?],
    )?;
    Ok(())
}

pub(crate) fn get_state<T: for<'a> Deserialize<'a>>(conn: &Connection, key: &str) -> Result<Option<T>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT v FROM node_election_state WHERE k=?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
        .transpose()
}

fn sorted_unique(values: &[VoterId]) -> Vec<VoterId> {
    let mut set: BTreeSet<VoterId> = values.iter().copied().collect();
    set.into_iter().collect()
}

impl Node {
    fn committed_config(conn: &Connection) -> Result<Option<MemberConfiguration>> {
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM node_member_configs ORDER BY revision DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let config: MemberConfiguration = serde_json::from_str(&raw)?;
        let expected = config_digest(&config.members, config.joint_new.as_deref())?;
        ensure!(
            expected == config.digest,
            "member configuration digest does not match its own content"
        );
        Ok(Some(config))
    }

    fn config_at(conn: &Connection, revision: u64) -> Result<Option<MemberConfiguration>> {
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM node_member_configs WHERE revision=?1",
                [revision],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
            .transpose()
    }

    fn last_log_position(conn: &Connection) -> Result<(u64, u64)> {
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT idx,term FROM node_control_log ORDER BY idx DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((index, term)) => (
                u64::try_from(index).context("control log index out of range")?,
                u64::try_from(term).context("control log term out of range")?,
            ),
            None => (0, 0),
        })
    }

    fn control_entry(conn: &Connection, index: u64) -> Result<Option<ControlEntry>> {
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM node_control_log WHERE idx=?1",
                [i64::try_from(index).context("control log index out of range")?],
                |r| r.get(0),
            )
            .optional()?;
        raw.map(|value| serde_json::from_str(&value).map_err(anyhow::Error::from))
            .transpose()
    }

    /// The highest fencing epoch already committed in this node's control log.
    ///
    /// A rejoined branch carries the entries it adopted from the other
    /// branch, so this is the real high-water mark the next claim must exceed.
    fn logged_max_epoch(conn: &Connection) -> Result<u64> {
        let mut statement =
            conn.prepare("SELECT data FROM node_control_log WHERE kind='primary'")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut max = 0u64;
        for row in rows {
            let entry: ControlEntry = serde_json::from_str(&row?)?;
            let payload: PrimaryEntry = serde_json::from_value(entry.payload)?;
            max = max.max(payload.epoch);
        }
        Ok(max)
    }

    fn votes_for(conn: &Connection, term: u64, scope: &str, candidate: VoterId) -> Result<BTreeSet<VoterId>> {
        let mut statement = conn.prepare(
            "SELECT voter FROM node_votes WHERE term=?1 AND scope=?2 AND candidate=?3",
        )?;
        let rows = statement.query_map(
            params![
                i64::try_from(term).context("election term out of range")?,
                scope,
                candidate.to_string()
            ],
            |r| r.get::<_, String>(0),
        )?;
        let mut votes = BTreeSet::new();
        for row in rows {
            let voter = row?;
            votes.insert(
                voter
                    .parse::<EntityId>()
                    .context("stored election voter is not a node identity")?,
            );
        }
        Ok(votes)
    }

    /// The committed member configuration of this domain, if any.
    pub fn member_configuration(&self) -> Result<Option<MemberConfiguration>> {
        Self::committed_config(&self.conn)
    }

    /// The committed primary claim. It exists only after a verified quorum.
    pub fn committed_primary_claim(&self) -> Result<Option<PrimaryClaim>> {
        get_state(&self.conn, "primary")
    }

    pub fn current_term(&self) -> Result<u64> {
        state_number(&self.conn, "term")
    }

    pub fn election_role(&self) -> Result<ElectionRole> {
        Ok(match get_state::<ElectionRole>(&self.conn, "role")? {
            Some(role) => role,
            None => ElectionRole::Follower,
        })
    }

    /// The fencing token of the currently committed primary.
    pub fn fencing_token(&self) -> Result<Option<FencingToken>> {
        self.committed_primary_claim()?.map(|claim| {
            Ok(FencingToken {
                term: claim.term,
                epoch: claim.epoch,
                leader_id: claim
                    .leader_node_id
                    .parse::<EntityId>()
                    .context("committed primary leader is not a node identity")?,
                config_revision: claim.config_revision,
                committed_seq: claim.committed_seq,
            })
        }).transpose()
    }

    /// Resource-side fence: a token is accepted only when it is exactly the
    /// committed claim. An older epoch is refused, never silently upgraded.
    pub fn check_fencing(&self, token: &FencingToken) -> Result<()> {
        let claim = self
            .committed_primary_claim()?
            .context("no committed primary claim; this node holds no primary authority")?;
        ensure!(
            token.term == claim.term
                && token.epoch == claim.epoch
                && token.committed_seq == claim.committed_seq
                && token.leader_id.to_string() == claim.leader_node_id,
            "stale fencing token: committed epoch {} at term {}, presented epoch {} at term {}",
            claim.epoch,
            claim.term,
            token.epoch,
            token.term
        );
        Ok(())
    }

    /// True when this node currently holds the committed primary authority.
    pub fn is_primary(&self) -> Result<bool> {
        let Some(claim) = self.committed_primary_claim()? else {
            return Ok(false);
        };
        let node_id = self.local_node_id()?;
        Ok(node_id.is_some_and(|id| id.to_string() == claim.leader_node_id))
    }

    /// Establish the membership of a domain that has no configuration yet.
    ///
    /// A fresh installation is a one-member domain: its quorum is one, and the
    /// self vote is a real quorum certificate. When the domain already has a
    /// configuration (pushed by an existing member) this never overwrites it.
    pub fn ensure_member_configuration(&mut self) -> Result<MemberConfiguration> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(config) = Self::committed_config(&tx)? {
            tx.commit()?;
            return Ok(config);
        }
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let term = state_number(&tx, "term")?.max(1);
        let members = vec![node_id];
        let digest = config_digest(&members, None)?;
        let config = MemberConfiguration {
            revision: 1,
            term,
            members: members.clone(),
            joint_new: None,
            digest: digest.clone(),
            committed_at: chrono::Utc::now(),
        };
        let certificate = QuorumCertificate {
            term,
            voters: members.clone(),
        };
        let entry = ConfigurationEntry {
            revision: config.revision,
            members: members.clone(),
            joint_new: None,
            digest: digest.clone(),
            certificate,
            proposal_old: members.clone(),
            proposal_new: members.clone(),
        };
        let control = ControlEntry {
            index: 1,
            term,
            kind: ControlKind::MemberConfiguration,
            payload: serde_json::to_value(&entry)?,
        };
        tx.execute(
            "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4)",
            params![
                i64::try_from(control.index)?,
                i64::try_from(control.term)?,
                "member_configuration",
                text(&control)?
            ],
        )?;
        tx.execute(
            "INSERT INTO node_member_configs(revision,term,data) VALUES(?1,?2,?3)",
            params![
                i64::try_from(config.revision)?,
                i64::try_from(config.term)?,
                text(&config)?
            ],
        )?;
        set_state(&tx, "term", &term.to_string())?;
        set_state(&tx, "commit_index", &1u64.to_string())?;
        set_state(&tx, "role", &ElectionRole::Follower)?;
        tx.commit()?;
        Ok(config)
    }

    /// Adopt a configuration published by an existing member of the domain.
    ///
    /// This is how a newly enrolled node learns the domain before it may vote
    /// in a membership change: the node never trusts a base configuration that
    /// it has not seen.
    pub fn adopt_member_configuration(
        &mut self,
        config: &MemberConfiguration,
        entries: &[ControlEntry],
    ) -> Result<MemberConfiguration> {
        let expected = config_digest(&config.members, config.joint_new.as_deref())?;
        ensure!(
            expected == config.digest,
            "pushed member configuration digest does not match its content"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let first = entries.first().context("pushed control log must contain the full committed prefix")?;
        ensure!(first.index == 1, "pushed control log must start at index 1");
        ensure!(entries.windows(2).all(|pair| pair[0].index.checked_add(1) == Some(pair[1].index)),
            "pushed control log is not continuous");
        let end = entries.last().unwrap().index;
        let commit_index = state_number(&tx, "commit_index")?;
        let (last_index, _) = Self::last_log_position(&tx)?;
        ensure!(end >= commit_index && end >= last_index,
            "pushed control log does not cover the retained prefix; histories must be reconciled");
        let prior_config = Self::committed_config(&tx)?;
        let learns_configuration = prior_config.as_ref().is_none_or(|existing| existing.revision < config.revision);
        if let Some(existing) = prior_config {
            ensure!(
                existing.revision <= config.revision,
                "pushed member configuration is older than the committed one"
            );
            if existing.revision == config.revision {
                ensure!(
                    existing.digest == config.digest,
                    "pushed member configuration conflicts with the committed one"
                );
            }
        }
        let mut configuration_in_log = false;
        let mut pushed_term = 0;
        for entry in entries {
            ensure!(entry.term >= pushed_term, "pushed control log terms are not monotonic");
            pushed_term = entry.term;
            if let Some(payload) = Self::verify_control_entry(entry)? {
                configuration_in_log |= payload.revision == config.revision
                    && payload.digest == config.digest && entry.term == config.term;
            }
            // An index this node already committed carries one decision. A
            // different entry at the same index is a diverged branch, not a
            // newer fact: it is refused instead of silently replacing what
            // this node already committed (SPEC §5.4.2).
            if let Some(existing) = Self::control_entry(&tx, entry.index)? {
                ensure!(
                    existing.term == entry.term
                        && existing.kind == entry.kind
                        && existing.payload == entry.payload,
                    "control log entry {} differs from the entry this node committed; the branches must be reconciled, not overwritten",
                    entry.index
                );
            } else {
                ensure!(entry.index > commit_index,
                    "the retained committed prefix has a missing entry; histories must be reconciled");
            }
        }
        ensure!(configuration_in_log,
            "the pushed control log does not commit the advertised configuration");
        {
            let mut insert = tx.prepare(
                "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4) ON CONFLICT(idx) DO NOTHING",
            )?;
            for entry in entries {
                if entry.index <= commit_index {
                    continue;
                }
                if insert.execute(params![i64::try_from(entry.index)?, i64::try_from(entry.term)?,
                    control_kind_name(entry.kind), text(entry)?])? == 1
                {
                    Self::apply_control_entry(&tx, entry)?;
                }
            }
        }
        // Only verified log entries can create configuration facts. Inserting
        // the advertised config first would make this check vacuous.
        let mut landed = Self::committed_config(&tx)?.context("pushed log has no committed configuration")?;
        ensure!(
            landed.revision == config.revision && landed.term == config.term && landed.digest == config.digest,
            "the pushed control log commits a different configuration than the one pushed"
        );
        if learns_configuration {
            // Preserve the originating commit time only after the log has
            // authenticated the new revision. This row is new in this
            // transaction; previously retained metadata is never rewritten.
            tx.execute("UPDATE node_member_configs SET data=?2 WHERE revision=?1",
                params![i64::try_from(config.revision)?, text(config)?])?;
            landed.committed_at = config.committed_at;
        }
        set_state(&tx, "commit_index", &end.to_string())?;
        if pushed_term > state_number(&tx, "term")? {
            set_state(&tx, "term", &pushed_term.to_string())?;
        }
        tx.commit()?;
        Ok(landed)
    }

    /// Return already-verified configuration data when this entry commits one.
    fn verify_control_entry(entry: &ControlEntry) -> Result<Option<ConfigurationEntry>> {
        match entry.kind {
            ControlKind::Primary => {
                let payload = PrimaryEntry::deserialize(&entry.payload)?;
                let members: BTreeSet<VoterId> = payload.members.iter().copied().collect();
                let voters: BTreeSet<VoterId> = payload.certificate.voters.iter().copied().collect();
                for voter in &payload.certificate.voters {
                    ensure!(
                        members.contains(voter),
                        "primary certificate names a voter outside the configuration"
                    );
                }
                ensure!(
                    counted(&payload.members, &voters) >= majority(&payload.members),
                    "primary entry does not carry a quorum certificate"
                );
                Ok(None)
            }
            ControlKind::MemberConfiguration => {
                let payload = ConfigurationEntry::deserialize(&entry.payload)?;
                let voters: BTreeSet<VoterId> = payload.certificate.voters.iter().copied().collect();
                ensure!(
                    counted(&payload.proposal_old, &voters) >= majority(&payload.proposal_old)
                        && counted(&payload.proposal_new, &voters)
                            >= majority(&payload.proposal_new),
                    "member configuration entry does not carry a joint quorum certificate"
                );
                let expected = config_digest(&payload.members, payload.joint_new.as_deref())?;
                ensure!(
                    expected == payload.digest,
                    "member configuration entry digest does not match its content"
                );
                Ok(Some(payload))
            }
        }
    }

    /// Start an election round for this node.
    pub fn campaign(&mut self) -> Result<Campaign> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; this node cannot campaign")?;
        ensure!(
            config.is_member(node_id),
            "this node is not a member of the committed configuration"
        );
        let term = state_number(&tx, "term")?
            .checked_add(1)
            .context("election term exhausted")?;
        let (index, last_term) = Self::last_log_position(&tx)?;
        set_state(&tx, "term", &term.to_string())?;
        set_state(&tx, "voted_for", &Some(node_id))?;
        set_state(&tx, "role", &ElectionRole::Candidate)?;
        let request = VoteRequest {
            schema: ELECTION_WIRE_SCHEMA,
            request_id: EntityId::new(),
            term,
            candidate_id: node_id,
            config_revision: config.revision,
            config_digest: config.digest.clone(),
            last_log_index: index,
            last_log_term: last_term,
            proposal: None,
        };
        tx.execute(
            "INSERT INTO node_votes(term,scope,voter,candidate,data,received_at) VALUES(?1,'election',?2,?3,?4,?5)",
            params![
                i64::try_from(term)?,
                node_id.to_string(),
                node_id.to_string(),
                text(&request)?,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        let members: Vec<VoterId> = config
            .voters()
            .into_iter()
            .filter(|id| *id != node_id)
            .collect();
        Ok(Campaign {
            term,
            candidate_id: node_id,
            request,
            members,
        })
    }

    /// Answer a vote request from another member.
    pub fn handle_vote_request(&mut self, request: &VoteRequest) -> Result<VoteResponse> {
        ensure!(
            request.schema == ELECTION_WIRE_SCHEMA,
            "unsupported election wire schema"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let current_term = state_number(&tx, "term")?;
        let mut term = current_term;
        let mut granted = false;
        let mut reason = String::new();
        let config = Self::committed_config(&tx)?;
        if request.term > current_term {
            term = request.term;
            set_state(&tx, "term", &term.to_string())?;
            set_state(&tx, "voted_for", &Option::<VoterId>::None)?;
            set_state(&tx, "role", &ElectionRole::Follower)?;
        }
        if request.term < term {
            reason = "stale_term".into();
        } else {
            let Some(config) = config else {
                reason = "no_member_configuration; request a configuration push first".into();
                let response = VoteResponse {
                    schema: ELECTION_WIRE_SCHEMA,
                    request_id: request.request_id,
                    responder_id: node_id,
                    term,
                    vote_granted: false,
                    config_digest: Digest::blake3(b"no-configuration"),
                    proposal_digest: request.proposal.as_ref().map(|p| p.digest.clone()),
                    reason,
                };
                tx.commit()?;
                return Ok(response);
            };
            if !config.is_member(request.candidate_id) {
                reason = "candidate_not_member".into();
            } else if request.config_revision != config.revision
                || request.config_digest != config.digest
            {
                reason = "config_mismatch".into();
            } else if let Some(proposal) = &request.proposal {
                if proposal.base_revision != config.revision
                    || proposal.base_digest != config.digest
                    || proposal_digest(proposal)? != proposal.digest
                {
                    reason = "proposal_base_mismatch".into();
                } else if !proposal
                    .old_members
                    .iter()
                    .chain(proposal.new_members.iter())
                    .any(|id| *id == node_id)
                {
                    reason = "not_a_voter_of_this_proposal".into();
                } else {
                    granted = true;
                }
            } else {
                granted = true;
            }
            if granted {
                // A proposal vote is counted in the proposal's own scope so a
                // member may vote for a leader and for a membership change in
                // the same term without double counting.
                let vote_scope = request
                    .proposal
                    .as_ref()
                    .map(|proposal| proposal.scope())
                    .unwrap_or_else(|| "election".to_string());
                let voted: Option<String> = tx
                    .query_row(
                        "SELECT candidate FROM node_votes WHERE term=?1 AND scope=?2 AND voter=?3",
                        params![i64::try_from(term)?, vote_scope, node_id.to_string()],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(voted) = voted {
                    if voted != request.candidate_id.to_string() {
                        granted = false;
                        reason = "already_voted_in_this_term".into();
                    }
                }
            }
            if granted {
                let (index, last_term) = Self::last_log_position(&tx)?;
                if (request.last_log_term, request.last_log_index) < (last_term, index) {
                    granted = false;
                    reason = "candidate_log_not_up_to_date".into();
                }
            }
            if granted {
                let vote_scope = request
                    .proposal
                    .as_ref()
                    .map(|proposal| proposal.scope())
                    .unwrap_or_else(|| "election".to_string());
                tx.execute(
                    "INSERT OR REPLACE INTO node_votes(term,scope,voter,candidate,data,received_at) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        i64::try_from(term)?,
                        vote_scope,
                        node_id.to_string(),
                        request.candidate_id.to_string(),
                        text(request)?,
                        chrono::Utc::now().to_rfc3339()
                    ],
                )?;
                set_state(&tx, "voted_for", &Some(request.candidate_id))?;
                reason = "granted".into();
            }
        }
        let config_digest = Self::committed_config(&tx)?
            .map(|config| config.digest)
            .unwrap_or_else(|| Digest::blake3(b"no-configuration"));
        let response = VoteResponse {
            schema: ELECTION_WIRE_SCHEMA,
            request_id: request.request_id,
            responder_id: node_id,
            term,
            vote_granted: granted,
            config_digest,
            proposal_digest: request.proposal.as_ref().map(|p| p.digest.clone()),
            reason,
        };
        tx.commit()?;
        Ok(response)
    }

    /// Record a peer's vote for this node's campaign or membership proposal.
    pub fn record_vote_response(
        &mut self,
        request: &VoteRequest,
        response: &VoteResponse,
    ) -> Result<VoteOutcome> {
        ensure!(
            response.schema == ELECTION_WIRE_SCHEMA,
            "unsupported election wire schema"
        );
        ensure!(
            response.request_id == request.request_id,
            "vote response does not answer this request"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if response.term > request.term {
            // A newer term exists: this node steps down and forgets its vote.
            set_state(&tx, "term", &response.term.to_string())?;
            set_state(&tx, "voted_for", &Option::<VoterId>::None)?;
            set_state(&tx, "role", &ElectionRole::Follower)?;
            tx.commit()?;
            return Ok(VoteOutcome {
                voter: response.responder_id,
                outcome: "lost_election".into(),
                reason: "peer reported a higher term".into(),
            });
        }
        ensure!(
            response.term == request.term,
            "vote response term does not match the campaign"
        );
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; votes cannot be counted")?;
        // A membership proposal is decided by the voters of its own sets, so a
        // node being added may vote on it before it is a member of the
        // committed configuration. Its vote is recorded in the proposal's
        // scope and only `tally_member_change` applies the joint arithmetic;
        // it never counts as an election vote.
        let allowed_voter = match &request.proposal {
            Some(proposal) => {
                proposal.old_members.contains(&response.responder_id)
                    || proposal.new_members.contains(&response.responder_id)
            }
            None => config.is_member(response.responder_id),
        };
        if !allowed_voter {
            tx.commit()?;
            return Ok(VoteOutcome {
                voter: response.responder_id,
                outcome: "rejected".into(),
                reason: "voter is not a member of the committed configuration or of this proposal".into(),
            });
        }
        if response.config_digest != config.digest {
            tx.commit()?;
            return Ok(VoteOutcome {
                voter: response.responder_id,
                outcome: "rejected".into(),
                reason: "vote was given against a different configuration".into(),
            });
        }
        let scope = match &request.proposal {
            Some(proposal) => proposal.scope(),
            None => "election".to_string(),
        };
        if let Some(proposal) = &request.proposal {
            if response.proposal_digest.as_ref() != Some(&proposal.digest) {
                tx.commit()?;
                return Ok(VoteOutcome {
                    voter: response.responder_id,
                    outcome: "rejected".into(),
                    reason: "vote was given against a different proposal".into(),
                });
            }
        }
        if !response.vote_granted {
            tx.commit()?;
            return Ok(VoteOutcome {
                voter: response.responder_id,
                outcome: "declined".into(),
                reason: response.reason.clone(),
            });
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT candidate FROM node_votes WHERE term=?1 AND scope=?2 AND voter=?3",
                params![
                    i64::try_from(request.term)?,
                    scope,
                    response.responder_id.to_string()
                ],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != request.candidate_id.to_string() {
                tx.commit()?;
                return Ok(VoteOutcome {
                    voter: response.responder_id,
                    outcome: "conflict".into(),
                    reason: "voter granted two different votes in one term".into(),
                });
            }
            tx.commit()?;
            return Ok(VoteOutcome {
                voter: response.responder_id,
                outcome: "duplicate".into(),
                reason: "vote already recorded".into(),
            });
        }
        tx.execute(
            "INSERT INTO node_votes(term,scope,voter,candidate,data,received_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                i64::try_from(request.term)?,
                scope,
                response.responder_id.to_string(),
                request.candidate_id.to_string(),
                text(response)?,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        Ok(VoteOutcome {
            voter: response.responder_id,
            outcome: "accepted".into(),
            reason: response.reason.clone(),
        })
    }

    /// Count the recorded votes of a term and commit the primary when the
    /// committed configuration's quorum is satisfied.
    pub fn tally(&mut self, term: u64) -> Result<ElectionOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; votes cannot be counted")?;
        let voted: Option<String> = tx
            .query_row(
                "SELECT candidate FROM node_votes WHERE term=?1 AND scope='election' AND voter=?2",
                params![i64::try_from(term)?, node_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let candidate = match voted {
            Some(candidate) => candidate
                .parse::<EntityId>()
                .context("stored vote candidate is not a node identity")?,
            None => {
                tx.commit()?;
                return Ok(ElectionOutcome {
                    term,
                    candidate_id: node_id,
                    quorum: config.quorum(),
                    votes: vec![],
                    elected: false,
                    claim: None,
                    reason: "no vote of this node in this term".into(),
                });
            }
        };
        let votes = Self::votes_for(&tx, term, "election", candidate)?;
        let quorum = config.quorum();
        if !config.has_quorum(&votes) {
            tx.commit()?;
            return Ok(ElectionOutcome {
                term,
                candidate_id: candidate,
                quorum,
                votes: votes.into_iter().collect(),
                elected: false,
                claim: None,
                reason: "no quorum of the committed configuration".into(),
            });
        }
        let claim = Self::commit_primary(&tx, term, candidate, &config, &votes)?;
        tx.commit()?;
        Ok(ElectionOutcome {
            term,
            candidate_id: candidate,
            quorum,
            votes: votes.into_iter().collect(),
            elected: true,
            claim: Some(claim),
            reason: "quorum of the committed configuration".into(),
        })
    }

    fn commit_primary(
        tx: &Transaction<'_>,
        term: u64,
        leader: VoterId,
        config: &MemberConfiguration,
        votes: &BTreeSet<VoterId>,
    ) -> Result<PrimaryClaim> {
        let previous = get_state::<PrimaryClaim>(tx, "primary")?;
        let epoch = previous
            .as_ref()
            .map(|claim| claim.epoch)
            .unwrap_or(0)
            // A branch that rejoined has adopted the other branch's control
            // log. The next epoch must dominate every epoch already committed
            // in this node's log, otherwise two separated branches could both
            // commit the same epoch under different leaders and the resource
            // fence could not tell them apart (SPEC §5.4.1/§5.4.3).
            .max(Self::logged_max_epoch(tx)?)
            .checked_add(1)
            .context("fencing epoch exhausted")?;
        let index = state_number(tx, "commit_index")?
            .checked_add(1)
            .context("control log exhausted")?;
        let certificate = QuorumCertificate {
            term,
            voters: votes.iter().copied().collect(),
        };
        let payload = PrimaryEntry {
            leader_id: leader,
            epoch,
            config_revision: config.revision,
            config_digest: config.digest.clone(),
            members: config.members.clone(),
            certificate,
        };
        let entry = ControlEntry {
            index,
            term,
            kind: ControlKind::Primary,
            payload: serde_json::to_value(&payload)?,
        };
        let _ = Self::verify_control_entry(&entry)?;
        tx.execute(
            "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4)",
            params![
                i64::try_from(index)?,
                i64::try_from(term)?,
                "primary",
                text(&entry)?
            ],
        )?;
        let claim = PrimaryClaim {
            term,
            epoch,
            leader_node_id: leader.to_string(),
            config_revision: config.revision,
            committed_seq: index,
            committed_at: chrono::Utc::now(),
        };
        set_state(tx, "commit_index", &index.to_string())?;
        set_state(tx, "primary", &claim)?;
        set_state(tx, "role", &ElectionRole::Leader)?;
        Ok(claim)
    }

    /// Replicate committed control entries to this node (follower side).
    ///
    /// The follower verifies the quorum certificate itself: it does not trust
    /// the leader's word that a decision was committed.
    ///
    /// The request is accepted as a whole or refused as a whole. Three
    /// relations must hold before anything is applied:
    ///
    /// - the committed prefix: `prev_index`/`prev_term` must name an entry
    ///   this node already committed;
    /// - the input suffix: its first entry must continue the predecessor
    ///   (`prev_index + 1`) and every later entry must continue the one
    ///   before it — no index may be skipped;
    /// - each offered entry either extends the committed prefix by exactly
    ///   one or replays an already-committed entry with identical content.
    ///
    /// A suffix that skips an index is a `log_mismatch`: nothing is applied,
    /// the commit index does not move, and no primary or configuration fact
    /// changes. An entry that names an index this node already committed
    /// with a different term or a different decision is a conflict: both the
    /// committed record and the offered record are retained as evidence, and
    /// the committed record is never overwritten. An offered entry that is
    /// identical to the committed one is an idempotent replay, not a
    /// conflict.
    pub fn append_control_entries(&mut self, request: &AppendRequest) -> Result<AppendOutcome> {
        ensure!(
            request.schema == ELECTION_WIRE_SCHEMA,
            "unsupported election wire schema"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::stored_node_id(&tx)?.context("node not initialized")?;
        let commit_index = state_number(&tx, "commit_index")?;
        let node_term = state_number(&tx, "term")?;
        macro_rules! refuse {
            ($reason:expr) => {{
                return Ok(AppendOutcome {
                    accepted: 0,
                    commit_index,
                    reason: $reason.into(),
                });
            }};
        }
        if request.prev_index > 0 {
            let previous = Self::control_entry(&tx, request.prev_index)?;
            let Some(previous) = previous else {
                refuse!("log_mismatch: predecessor entry is missing");
            };
            if previous.term != request.prev_term {
                refuse!("log_mismatch: predecessor term differs");
            }
            if request.prev_index > commit_index {
                refuse!("log_mismatch: the predecessor is beyond the committed prefix");
            }
        }
        // The input suffix must continue the predecessor without a gap: the
        // first entry is exactly `prev_index + 1` and every later entry is
        // exactly one more than the one before it.
        let Some(expected_first) = request.prev_index.checked_add(1) else {
            refuse!("log_mismatch: the predecessor index is exhausted");
        };
        if let Some(first) = request.entries.first() {
            if first.index != expected_first {
                refuse!("log_mismatch: the first entry does not continue the predecessor");
            }
            for pair in request.entries.windows(2) {
                if Some(pair[1].index) != pair[0].index.checked_add(1) {
                    refuse!("log_mismatch: the appended entries are not continuous");
                }
            }
        }
        // Nothing is applied until the whole suffix has been checked. A stale
        // term or an unverified certificate refuses the request as a whole.
        let mut batch_term = node_term;
        for entry in &request.entries {
            if entry.term < batch_term {
                refuse!("stale_term: entry predates the current term");
            }
            if let Err(error) = Self::verify_control_entry(entry) {
                refuse!(format!("unverified_entry: {error:#}"));
            }
            batch_term = batch_term.max(entry.term);
            // An index this node already committed carries one decision. The
            // offered entry must be exactly that decision; a divergence is a
            // conflict and both records are retained as evidence.
            let Some(existing) = Self::control_entry(&tx, entry.index)? else {
                continue;
            };
            let differs = existing != *entry;
            if entry.index > commit_index && differs {
                refuse!("log_mismatch: a divergent stored record is beyond the committed prefix");
            }
            if existing.term != entry.term {
                Self::record_control_conflict(&tx, entry.index, &existing, entry)?;
                tx.commit()?;
                return Ok(AppendOutcome {
                    accepted: 0,
                    commit_index,
                    reason: format!(
                        "log_conflict: the entry at index {} carries a different term",
                        entry.index
                    ),
                });
            }
            if differs {
                Self::record_control_conflict(&tx, entry.index, &existing, entry)?;
                tx.commit()?;
                return Ok(AppendOutcome {
                    accepted: 0,
                    commit_index,
                    reason: format!(
                        "log_conflict: the entry at index {} carries a different decision",
                        entry.index
                    ),
                });
            }
        }
        // Apply the suffix: a new entry must extend the contiguous committed
        // prefix by exactly one; an identical entry is an idempotent replay.
        // Physical presence above the recorded commit frontier is not proof
        // of commitment. Legacy configuration pushes could leave such rows;
        // a replay must not silently promote them or report an index that was
        // never written back to state. Inconsistent histories need explicit
        // reconciliation, not implicit backfill of already-applied decisions.
        let mut end = commit_index;
        let mut next = end.checked_add(1).context("control log index exhausted")?;
        let mut accepted = 0usize;
        for entry in &request.entries {
            if Self::control_entry(&tx, entry.index)?.is_some() {
                continue;
            }
            if entry.index != next {
                refuse!("log_mismatch: the appended entries do not extend the committed prefix");
            }
            tx.execute(
                "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4)",
                params![
                    i64::try_from(entry.index)?,
                    i64::try_from(entry.term)?,
                    control_kind_name(entry.kind),
                    text(entry)?
                ],
            )?;
            Self::apply_control_entry(&tx, entry)?;
            accepted += 1;
            end = entry.index;
            next = end.checked_add(1).context("control log index exhausted")?;
        }
        if accepted > 0 {
            set_state(&tx, "commit_index", &end.to_string())?;
            set_state(&tx, "term", &batch_term.to_string())?;
        }
        tx.commit()?;
        Ok(AppendOutcome {
            accepted,
            commit_index: end,
            reason: "accepted".into(),
        })
    }

    /// Retain both sides of a control-log divergence under one content-derived
    /// conflict identity. The committed record is read back unchanged; the
    /// offered record is the one the leader sent. Re-observing the same
    /// divergence reuses the identity, so the evidence is recorded once.
    fn record_control_conflict(
        tx: &Transaction<'_>,
        index: u64,
        committed: &ControlEntry,
        offered: &ControlEntry,
    ) -> Result<Digest> {
        let conflict_id = digest(&(index, committed, offered))?;
        for (side, entry) in [
            (ControlConflictSide::Committed, committed),
            (ControlConflictSide::Offered, offered),
        ] {
            let record = ControlLogConflict {
                conflict_id: conflict_id.clone(),
                index,
                side,
                entry: entry.clone(),
                detected_at: chrono::Utc::now(),
            };
            tx.execute(
                "INSERT OR IGNORE INTO node_control_log_conflicts(conflict_id,idx,side,data) VALUES(?1,?2,?3,?4)",
                params![
                    conflict_id.0,
                    i64::try_from(index).context("control log index out of range")?,
                    side_name(side),
                    text(&record)?
                ],
            )?;
        }
        Ok(conflict_id)
    }

    fn apply_control_entry(tx: &Transaction<'_>, entry: &ControlEntry) -> Result<()> {
        match entry.kind {
            ControlKind::Primary => {
                let payload: PrimaryEntry = serde_json::from_value(entry.payload.clone())?;
                let current = get_state::<PrimaryClaim>(tx, "primary")?;
                let newer = match &current {
                    None => true,
                    Some(claim) => (entry.term, payload.epoch) > (claim.term, claim.epoch),
                };
                if newer {
                    let claim = PrimaryClaim {
                        term: entry.term,
                        epoch: payload.epoch,
                        leader_node_id: payload.leader_id.to_string(),
                        config_revision: payload.config_revision,
                        committed_seq: entry.index,
                        committed_at: chrono::Utc::now(),
                    };
                    set_state(tx, "primary", &claim)?;
                }
                Ok(())
            }
            ControlKind::MemberConfiguration => {
                let payload: ConfigurationEntry = serde_json::from_value(entry.payload.clone())?;
                let existing = Self::config_at(tx, payload.revision)?;
                match existing {
                    Some(config) => {
                        ensure!(
                            config.digest == payload.digest,
                            "member configuration revision conflicts with the committed one"
                        );
                    }
                    None => {
                        let config = MemberConfiguration {
                            revision: payload.revision,
                            term: entry.term,
                            members: payload.members.clone(),
                            joint_new: payload.joint_new.clone(),
                            digest: payload.digest.clone(),
                            committed_at: chrono::Utc::now(),
                        };
                        tx.execute(
                            "INSERT INTO node_member_configs(revision,term,data) VALUES(?1,?2,?3)",
                            params![
                                i64::try_from(config.revision)?,
                                i64::try_from(config.term)?,
                                text(&config)?
                            ],
                        )?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Propose a membership change. The first phase installs the joint
    /// configuration; the second phase commits the final configuration.
    pub fn propose_member_change(
        &mut self,
        request: MemberChangeRequest,
    ) -> Result<MemberChangeRound> {
        ensure!(
            !request.reason.trim().is_empty(),
            "membership change requires a reason"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; membership is unknown")?;
        ensure!(
            config.revision == request.expected_revision,
            "member configuration revision conflict"
        );
        let mut new_members = config.members.clone();
        new_members.extend(request.add.iter().copied());
        new_members.retain(|id| !request.remove.contains(id));
        let new_members = sorted_unique(&new_members);
        ensure!(!new_members.is_empty(), "membership change would empty the domain");
        ensure!(
            new_members.contains(&node_id),
            "removing this node requires an explicit handoff, not a membership change"
        );
        let proposal = MemberProposal {
            kind: ProposalKind::Joint,
            base_revision: config.revision,
            base_digest: config.digest.clone(),
            old_members: config.members.clone(),
            new_members: new_members.clone(),
            digest: Digest::blake3(b""),
        };
        let proposal = MemberProposal {
            digest: proposal_digest(&proposal)?,
            ..proposal
        };
        let (index, last_term) = Self::last_log_position(&tx)?;
        let term = state_number(&tx, "term")?.max(1);
        let vote_request = VoteRequest {
            schema: ELECTION_WIRE_SCHEMA,
            request_id: EntityId::new(),
            term,
            candidate_id: node_id,
            config_revision: config.revision,
            config_digest: config.digest.clone(),
            last_log_index: index,
            last_log_term: last_term,
            proposal: Some(proposal.clone()),
        };
        tx.execute(
            "INSERT OR REPLACE INTO node_votes(term,scope,voter,candidate,data,received_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                i64::try_from(term)?,
                proposal.scope(),
                node_id.to_string(),
                node_id.to_string(),
                text(&vote_request)?,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        let members = sorted_unique(
            &proposal
                .old_members
                .iter()
                .chain(proposal.new_members.iter())
                .copied()
                .collect::<Vec<_>>(),
        );
        Ok(MemberChangeRound {
            phase: ProposalKind::Joint,
            proposal,
            request: vote_request,
            members,
        })
    }

    /// The second phase of a membership change: the final configuration is
    /// approved against the joint configuration that phase one committed.
    pub fn propose_final_member_change(
        &mut self,
        joint: &MemberProposal,
    ) -> Result<MemberChangeRound> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; membership is unknown")?;
        ensure!(
            config.joint_new.is_some(),
            "the joint configuration was not committed; the final phase cannot start"
        );
        let proposal = MemberProposal {
            kind: ProposalKind::Final,
            base_revision: config.revision,
            base_digest: config.digest.clone(),
            old_members: joint.old_members.clone(),
            new_members: joint.new_members.clone(),
            digest: Digest::blake3(b""),
        };
        let proposal = MemberProposal {
            digest: proposal_digest(&proposal)?,
            ..proposal
        };
        let (index, last_term) = Self::last_log_position(&tx)?;
        let term = state_number(&tx, "term")?.max(1);
        let vote_request = VoteRequest {
            schema: ELECTION_WIRE_SCHEMA,
            request_id: EntityId::new(),
            term,
            candidate_id: node_id,
            config_revision: config.revision,
            config_digest: config.digest.clone(),
            last_log_index: index,
            last_log_term: last_term,
            proposal: Some(proposal.clone()),
        };
        tx.execute(
            "INSERT OR REPLACE INTO node_votes(term,scope,voter,candidate,data,received_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                i64::try_from(term)?,
                proposal.scope(),
                node_id.to_string(),
                node_id.to_string(),
                text(&vote_request)?,
                chrono::Utc::now().to_rfc3339()
            ],
        )?;
        tx.commit()?;
        let members = sorted_unique(
            &proposal
                .old_members
                .iter()
                .chain(proposal.new_members.iter())
                .copied()
                .collect::<Vec<_>>(),
        );
        Ok(MemberChangeRound {
            phase: ProposalKind::Final,
            proposal,
            request: vote_request,
            members,
        })
    }

    /// Count the votes of a membership proposal and commit the configuration
    /// when the joint quorum is satisfied.
    pub fn tally_member_change(
        &mut self,
        proposal: &MemberProposal,
    ) -> Result<MemberChangeOutcome> {
        ensure!(
            proposal_digest(proposal)? == proposal.digest,
            "membership proposal digest does not match its content"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let config = Self::committed_config(&tx)?
            .context("no committed member configuration; membership is unknown")?;
        // Proposal votes are recorded under the term in which the proposal
        // was made; a later proposal for the same content advances that term.
        let vote_term: i64 = tx.query_row(
            "SELECT COALESCE(MAX(term),0) FROM node_votes WHERE scope=?1 AND candidate=?2",
            params![proposal.scope(), node_id.to_string()],
            |r| r.get(0),
        )?;
        ensure!(
            vote_term > 0,
            "no votes recorded for this membership proposal"
        );
        let votes = Self::votes_for(&tx, u64::try_from(vote_term)?, &proposal.scope(), node_id)?;
        let quorum = proposal.has_quorum(&votes);
        if !quorum {
            tx.commit()?;
            return Ok(MemberChangeOutcome {
                phase: proposal.kind,
                quorum_met: false,
                votes: votes.into_iter().collect(),
                committed: None,
                reason: "no joint quorum of the proposed configuration".into(),
            });
        }
        let members = match proposal.kind {
            ProposalKind::Joint => proposal.old_members.clone(),
            ProposalKind::Final => proposal.new_members.clone(),
        };
        let joint_new = match proposal.kind {
            ProposalKind::Joint => Some(proposal.new_members.clone()),
            ProposalKind::Final => None,
        };
        let digest = config_digest(&members, joint_new.as_deref())?;
        let revision = config.revision + 1;
        let term = state_number(&tx, "term")?.max(1);
        let certificate = QuorumCertificate {
            term,
            voters: votes.iter().copied().collect(),
        };
        let payload = ConfigurationEntry {
            revision,
            members: members.clone(),
            joint_new: joint_new.clone(),
            digest: digest.clone(),
            certificate,
            proposal_old: proposal.old_members.clone(),
            proposal_new: proposal.new_members.clone(),
        };
        let index = state_number(&tx, "commit_index")?
            .checked_add(1)
            .context("control log exhausted")?;
        let entry = ControlEntry {
            index,
            term,
            kind: ControlKind::MemberConfiguration,
            payload: serde_json::to_value(&payload)?,
        };
        let _ = Self::verify_control_entry(&entry)?;
        tx.execute(
            "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4)",
            params![
                i64::try_from(index)?,
                i64::try_from(term)?,
                "member_configuration",
                text(&entry)?
            ],
        )?;
        let committed = MemberConfiguration {
            revision,
            term,
            members,
            joint_new,
            digest,
            committed_at: chrono::Utc::now(),
        };
        tx.execute(
            "INSERT INTO node_member_configs(revision,term,data) VALUES(?1,?2,?3)",
            params![
                i64::try_from(revision)?,
                i64::try_from(term)?,
                text(&committed)?
            ],
        )?;
        set_state(&tx, "commit_index", &index.to_string())?;
        tx.commit()?;
        Ok(MemberChangeOutcome {
            phase: proposal.kind,
            quorum_met: true,
            votes: votes.into_iter().collect(),
            committed: Some(committed),
            reason: "joint quorum of the proposed configuration".into(),
        })
    }

    /// Record the election epoch an assignment was made under, so the
    /// resource end can refuse a result that a fenced writer produced.
    pub fn record_assignment_fence(&mut self, task: EntityId, epoch: &str) -> Result<()> {
        let claim = self.committed_primary_claim()?;
        let (term, election_epoch) = match claim {
            Some(claim) => (claim.term.to_string(), claim.epoch.to_string()),
            None => ("0".into(), "0".into()),
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO node_assignment_fences(task,epoch,election_term,election_epoch) VALUES(?1,?2,?3,?4)",
            params![task.to_string(), epoch, term, election_epoch],
        )?;
        Ok(())
    }

    /// The election epoch an assignment was made under.
    pub fn assignment_fence(&self, task: EntityId, epoch: &str) -> Result<Option<(u64, u64)>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT election_term,election_epoch FROM node_assignment_fences WHERE task=?1 AND epoch=?2",
                params![task.to_string(), epoch],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((term, epoch)) => Ok(Some((number(&term)?, number(&epoch)?))),
            None => Ok(None),
        }
    }

    /// Open an explicit availability branch: this node keeps thinking inside
    /// its connected domain without claiming a global quorum (SPEC §5.4.1).
    pub fn open_availability_branch(
        &mut self,
        branch_id: EntityId,
        subject_id: Option<EntityId>,
        reason: &str,
    ) -> Result<BranchFact> {
        ensure!(
            !reason.trim().is_empty(),
            "an availability branch requires a reason"
        );
        let retained = self.experience_counts()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT data FROM node_branches WHERE branch_id=?1",
                [branch_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let now = chrono::Utc::now();
        let floor = Self::last_reconciliation_seq(&tx)?;
        let fact = match existing {
            Some(raw) => {
                let mut fact: BranchFact = serde_json::from_str(&raw)?;
                fact.kind = BranchKind::Availability;
                fact.reason = reason.to_string();
                fact.global_quorum = false;
                fact.retained_experience = retained;
                fact.updated_at = now;
                // A new partition of the same branch supersedes an earlier
                // merge: this branch is partitioned again, so it is not
                // merged until a newer reconciliation closes it.
                fact.merged = None;
                fact.reconciliation_floor = floor;
                fact
            }
            None => BranchFact {
                branch_id,
                kind: BranchKind::Availability,
                subject_id,
                reason: reason.to_string(),
                global_quorum: false,
                retained_experience: retained,
                opened_at: now,
                updated_at: now,
                merged: None,
                reconciliation_floor: floor,
            },
        };
        tx.execute(
            "INSERT INTO node_branches(branch_id,data) VALUES(?1,?2) ON CONFLICT(branch_id) DO UPDATE SET data=excluded.data",
            params![branch_id.to_string(), text(&fact)?],
        )?;
        tx.commit()?;
        Ok(fact)
    }

    /// The branches this node actually used.
    pub fn branches(&self) -> Result<Vec<BranchFact>> {
        let mut statement = self
            .conn
            .prepare("SELECT data FROM node_branches ORDER BY branch_id")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut facts = Vec::new();
        for row in rows {
            facts.push(serde_json::from_str(&row?)?);
        }
        Ok(facts)
    }

    /// Close a partition branch once its experience was merged.
    ///
    /// The merge is bound to a reconciliation that this node actually recorded
    /// *after* the branch became a partition branch: a stale reconciliation
    /// does not cover the branch's diverged experience, and a missing one means
    /// nothing was merged. The branch is archived with the merge fact instead
    /// of being reset, and it never claims a quorum of its own: `global_quorum`
    /// follows the committed primary claim exactly.
    pub fn merge_availability_branch(
        &mut self,
        branch_id: EntityId,
        reconciliation_id: EntityId,
        note: &str,
    ) -> Result<BranchFact> {
        ensure!(
            !note.trim().is_empty(),
            "a branch merge requires a note saying what was merged"
        );
        let retained = self.experience_counts()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let raw: Option<String> = tx
            .query_row(
                "SELECT data FROM node_branches WHERE branch_id=?1",
                [branch_id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        let mut fact: BranchFact = serde_json::from_str(
            &raw.with_context(|| format!("no branch {branch_id} on this node"))?,
        )?;
        ensure!(
            fact.kind == BranchKind::Availability,
            "only a partition branch is merged back into the domain"
        );
        if let Some(merged) = &fact.merged {
            ensure!(
                merged.reconciliation_id == reconciliation_id,
                "branch {branch_id} was already merged by reconciliation {}; a later merge needs its own branch",
                merged.reconciliation_id
            );
            // Replaying the same rejoin is not a new merge.
            tx.commit()?;
            return Ok(fact);
        }
        let seq = Self::reconciliation_seq(&tx, reconciliation_id)?.with_context(|| {
            format!("no reconciliation {reconciliation_id} was recorded by this node")
        })?;
        ensure!(
            seq > i64::try_from(fact.reconciliation_floor).context("branch floor out of range")?,
            "reconciliation {reconciliation_id} predates the partition branch; it does not cover its experience"
        );
        let record: ReconciliationRecord =
            serde_json::from_str(&tx.query_row(
                "SELECT data FROM node_reconciliations WHERE seq=?1",
                [seq],
                |r| r.get::<_, String>(0),
            )?)?;
        let claim = get_state::<PrimaryClaim>(&tx, "primary")?;
        let now = chrono::Utc::now();
        fact.reason = note.to_string();
        fact.retained_experience = retained;
        fact.updated_at = now;
        fact.global_quorum = claim.is_some();
        fact.merged = Some(BranchMerge {
            reconciliation_id,
            peer_node_id: record.peer_node_id,
            peer_high_water: record.peer_high_water.clone(),
            primary: claim,
            note: note.to_string(),
            merged_at: now,
        });
        tx.execute(
            "INSERT INTO node_branches(branch_id,data) VALUES(?1,?2) ON CONFLICT(branch_id) DO UPDATE SET data=excluded.data",
            params![branch_id.to_string(), text(&fact)?],
        )?;
        tx.commit()?;
        Ok(fact)
    }

    /// The highest reconciliation journal sequence, or zero when nothing was
    /// reconciled yet. Journal order is used instead of wall clock time.
    fn last_reconciliation_seq(tx: &Transaction<'_>) -> Result<u64> {
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM node_reconciliations",
            [],
            |r| r.get(0),
        )?;
        u64::try_from(seq).context("reconciliation sequence out of range")
    }

    fn reconciliation_seq(tx: &Transaction<'_>, id: EntityId) -> Result<Option<i64>> {
        let mut statement = tx.prepare("SELECT seq,data FROM node_reconciliations")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (seq, data) = row?;
            let record: ReconciliationRecord = serde_json::from_str(&data)?;
            if record.reconciliation_id == id {
                return Ok(Some(seq));
            }
        }
        Ok(None)
    }

    fn experience_counts(&self) -> Result<ExperienceCounts> {
        let count = |sql: &str| -> Result<u64> {
            let value: i64 = self.conn.query_row(sql, [], |r| r.get(0))?;
            u64::try_from(value).context("experience count out of range")
        };
        Ok(ExperienceCounts {
            tasks: count("SELECT count(*) FROM tasks")?,
            events: count("SELECT count(*) FROM events")?,
            work: count("SELECT count(*) FROM work")?,
            receipts: count("SELECT count(*) FROM receipts")?,
        })
    }

    /// Adopt an existing self: the local independent identity stops producing
    /// new decisions while its experience is retained (SPEC §5.4).
    pub fn adopt_identity(&mut self, request: AdoptRequest) -> Result<AdoptionRecord> {
        ensure!(
            !request.provenance.trim().is_empty(),
            "adoption requires the provenance of the verified identity"
        );
        ensure!(
            !request.reason.trim().is_empty(),
            "adoption requires a reason"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<Identity> = get(&tx, "identity")?;
        ensure!(
            current.as_ref() != Some(&request.trusted_identity),
            "this node already carries the trusted identity"
        );
        let retained = {
            let count = |sql: &str| -> Result<u64> {
                let value: i64 = tx.query_row(sql, [], |r| r.get(0))?;
                u64::try_from(value).context("experience count out of range")
            };
            ExperienceCounts {
                tasks: count("SELECT count(*) FROM tasks")?,
                events: count("SELECT count(*) FROM events")?,
                work: count("SELECT count(*) FROM work")?,
                receipts: count("SELECT count(*) FROM receipts")?,
            }
        };
        let mut retired: Vec<Identity> = get(&tx, "retired_identities")?;
        if let Some(previous) = current.clone() {
            retired.push(previous);
        }
        set(&tx, "retired_identities", &retired)?;
        set(&tx, "identity", &Some(&request.trusted_identity))?;
        set(&tx, "joined", &request.trusted_identity)?;
        let record = AdoptionRecord {
            adoption_id: EntityId::new(),
            operation_id: request.operation_id,
            adopted: request.trusted_identity.clone(),
            retired: current,
            trusted_node_id: request.trusted_node_id,
            provenance: request.provenance,
            reason: request.reason,
            retained_experience: retained.clone(),
            adopted_at: chrono::Utc::now(),
        };
        // The retained experience stays in place; it is recorded as its own
        // branch so a later reconciliation can merge it instead of erasing it.
        let branch_id = record
            .retired
            .as_ref()
            .map(|identity| identity.branch_id)
            .unwrap_or_else(EntityId::new);
        let now = chrono::Utc::now();
        let branch = BranchFact {
            branch_id,
            kind: BranchKind::AdoptedExperience,
            subject_id: record
                .retired
                .as_ref()
                .map(|identity| identity.agent_id),
            reason: format!("adopted an existing self: {}", record.reason),
            global_quorum: false,
            retained_experience: retained,
            opened_at: now,
            updated_at: now,
            merged: None,
            reconciliation_floor: Self::last_reconciliation_seq(&tx)?,
        };
        tx.execute(
            "INSERT INTO node_branches(branch_id,data) VALUES(?1,?2) ON CONFLICT(branch_id) DO UPDATE SET data=excluded.data",
            params![branch_id.to_string(), text(&branch)?],
        )?;
        tx.commit()?;
        Ok(record)
    }

    /// Merge a peer's verified experience into this node's facts.
    ///
    /// Only an event whose digest matches its own content is merged: a peer's
    /// receipt is evidence about that peer, not a licence to store unverified
    /// content. Events are deduplicated by identity; conflicting events are
    /// retained side by side as parallel facts. A settled task is never
    /// un-settled and an unknown external effect is never re-sent
    /// (SPEC §5.4.2/§5.4.3).
    pub fn reconcile_peer_experience(
        &mut self,
        peer: &PeerExperience,
    ) -> Result<ReconciliationRecord> {
        // A cognition report is a fingerprint *of* a scope: an experience that
        // carries one without the others states nothing observable, so it is
        // refused instead of being stored as half a report.
        ensure!(
            peer.cognition_fingerprint.is_some() == peer.cognition_high_water.is_some()
                && peer.cognition_fingerprint.is_some() == peer.cognition_origin.is_some(),
            "a peer cognition report must carry its fingerprint, high water, and the scope/revision provenance together"
        );
        // This node's own published observation, if its installation ever
        // published one. Absence keeps the comparison `unobserved`.
        let local_cognition = self.cognition_report()?;
        let cognition_comparison = CognitionComparison::between(
            peer.cognition_fingerprint.as_ref(),
            peer.cognition_origin.as_ref().map(|origin| &origin.scope),
            local_cognition.as_ref(),
        );
        let mut imported = 0u64;
        let mut duplicates = 0u64;
        let mut rejected = 0u64;
        let mut parallel = Vec::new();
        let mut effects = Vec::new();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for event in &peer.events {
            if event.raw.schema != super::SCHEMA || digest(&event.raw)? != event.digest {
                rejected += 1;
                continue;
            }
            let outcome = super::append_event(&tx, event)?;
            match outcome {
                "new" => imported += 1,
                "duplicate" => duplicates += 1,
                _ => {
                    let local: Option<String> = tx
                        .query_row(
                            "SELECT digest FROM events WHERE id=?1",
                            [event.raw.event_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()?;
                    parallel.push(ParallelFact {
                        event_id: event.raw.event_id,
                        local_digest: local
                            .map(Digest)
                            .unwrap_or_else(|| Digest::blake3(b"absent")),
                        remote_digest: event.digest.clone(),
                    });
                }
            }
        }
        for receipt in &peer.receipts {
            let task_id = receipt.ticket.task_id;
            let data = text(receipt)?;
            let receipt_digest = Digest::blake3(data.as_bytes());
            // Keep the complete incoming evidence in the existing receipt
            // journal, atomically with the reconciliation below. Merely
            // classifying it does not retain its ticket, result or event.
            // The peer is its transport provenance, not settlement authority.
            // A replay must neither duplicate nor downgrade an existing row.
            tx.execute(
                "INSERT OR IGNORE INTO receipts(source,event_id,digest,task,data,status,reason) \
                 VALUES(?1,?2,?3,?4,?5,'evidence_only','peer_reconciliation')",
                params![peer.peer_node_id.to_string(), receipt.event.raw.event_id.to_string(),
                    receipt_digest.0, task_id.to_string(), data],
            )?;
            let local: Option<String> = tx
                .query_row(
                    "SELECT data FROM results WHERE task=?1",
                    [task_id.to_string()],
                    |r| r.get(0),
                )
                .optional()?;
            match local {
                Some(raw) => {
                    let local_receipt: ResultReceipt = serde_json::from_str(&raw)?;
                    let same = digest(&local_receipt)? == receipt_digest;
                    effects.push(EffectReconciliation {
                        task_id,
                        classification: if same {
                            "already_settled".into()
                        } else {
                            "parallel_terminal_fact".into()
                        },
                        detail: if same {
                            "the same result is already settled here".into()
                        } else {
                            "a different result exists for this task; both facts are retained".into()
                        },
                    });
                }
                None => {
                    effects.push(EffectReconciliation {
                        task_id,
                        classification: "evidence_only".into(),
                        detail: "the remote result is retained as evidence; it does not settle this task".into(),
                    });
                }
            }
        }
        let local_manifest = super::running_manifest().ok();
        let record = ReconciliationRecord {
            reconciliation_id: EntityId::new(),
            peer_node_id: peer.peer_node_id,
            peer_branch_id: peer.peer_branch_id,
            imported_events: imported,
            duplicate_events: duplicates,
            parallel_facts: parallel,
            effects,
            cognition_fingerprint: peer.cognition_fingerprint.clone(),
            cognition_high_water: peer.cognition_high_water,
            cognition_origin: peer.cognition_origin.clone(),
            cognition_comparison: Some(cognition_comparison),
            // The merge names the peer's real watermark and the artifact it
            // ran. A heterogeneous peer's own capability facts stay its own:
            // this record says the generations differ, it does not adopt them.
            heterogeneous: match (&peer.peer_manifest, &local_manifest) {
                (Some(peer_manifest), Some(local)) => Some(peer_manifest != local),
                _ => None,
            },
            rejected_events: rejected,
            peer_high_water: peer.peer_high_water.clone(),
            peer_manifest: peer.peer_manifest.clone(),
            retained_conflicts: u64::try_from(tx.query_row(
                "SELECT count(*) FROM conflicts",
                [],
                |r| r.get::<_, i64>(0),
            )?)
            .context("retained parallel fact count out of range")?,
            note: peer.note.clone(),
            reconciled_at: chrono::Utc::now(),
        };
        tx.execute(
            "INSERT INTO node_reconciliations(data) VALUES(?1)",
            [text(&record)?],
        )?;
        tx.commit()?;
        Ok(record)
    }

    /// The reconciliation facts recorded so far.
    pub fn reconciliation_records(&self) -> Result<Vec<ReconciliationRecord>> {
        let mut statement = self
            .conn
            .prepare("SELECT data FROM node_reconciliations ORDER BY seq")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(records)
    }

    /// The committed control log entries, used to replicate decisions to
    /// peers so they learn the current configuration and fencing epoch.
    pub fn control_log(&self) -> Result<Vec<ControlEntry>> {
        let mut statement = self
            .conn
           .prepare("SELECT data FROM node_control_log ORDER BY idx")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(serde_json::from_str(&row?)?);
        }
        Ok(entries)
    }

    /// The control-log divergences this node refused and retained as
    /// evidence: one `committed` row (the record this node committed) and
    /// one `offered` row (the record a leader sent) per divergence.
    pub fn control_log_conflicts(&self) -> Result<Vec<ControlLogConflict>> {
        let mut statement = self
            .conn
            .prepare("SELECT data FROM node_control_log_conflicts ORDER BY seq")?;
        let rows = statement.query_map([], |r| r.get::<_, String>(0))?;
        let mut records = Vec::new();
        for row in rows {
            records.push(serde_json::from_str(&row?)?);
        }
        Ok(records)
    }

    /// The election, membership and authority facts of this node.
    pub fn election_status(&self) -> Result<Value> {
        let config = self.member_configuration()?;
        let claim = self.committed_primary_claim()?;
        let token = self.fencing_token()?;
        let role = self.election_role()?;
        let term = self.current_term()?;
        let branches = self.branches()?;
        let mut votes = Vec::new();
        if let Some(config) = &config {
            let mut statement = self.conn.prepare(
                "SELECT term,voter,candidate FROM node_votes WHERE scope='election' ORDER BY term,voter",
            )?;
            let rows = statement.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (term, voter, candidate) = row?;
                votes.push(json!({
                    "term": term.to_string(),
                    "voter": voter,
                    "candidate": candidate,
                }));
            }
        }
        let quorum = config.as_ref().map(|config| config.quorum());
        let parallel_facts: i64 =
            self.conn
                .query_row("SELECT count(*) FROM conflicts", [], |r| r.get(0))?;
        let retained_conflicts: i64 = self.conn.query_row(
            "SELECT count(*) FROM node_control_log_conflicts",
            [],
            |r| r.get(0),
        )?;
        Ok(json!({
            "node_id": self.local_node_id()?,
            "term": term.to_string(),
            "role": role,
            "member_configuration": config,
            "quorum": quorum,
            "committed_primary_claim": claim,
            "fencing_token": token,
            "this_node_is_primary": self.is_primary()?,
            "recorded_election_votes": votes,
            "branches": branches,
            "parallel_facts_retained": parallel_facts.to_string(),
            "control_log_conflicts_retained": retained_conflicts.to_string(),
            "control_log_conflicts_are_evidence": true,
            "parallel_facts_are_evidence": true,
            "is_election": true,
            "quorum_basis": "committed_member_configuration",
            "visible_nodes_are_not_the_quorum": true,
            "current_peer_liveness": "not_measured",
            "model_required_for_membership": false,
            "credentials_exported": false,
        }))
    }
}

fn control_kind_name(kind: ControlKind) -> &'static str {
    match kind {
        ControlKind::Primary => "primary",
        ControlKind::MemberConfiguration => "member_configuration",
    }
}

fn side_name(side: ControlConflictSide) -> &'static str {
    match side {
        ControlConflictSide::Committed => "committed",
        ControlConflictSide::Offered => "offered",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Request;

    /// A private node fixture: no network, no credentials, no peer route. All
    /// facts are read back from the node's own durable store.
    fn node(dir: &std::path::Path) -> Node {
        let mut node = Node::open(dir).unwrap();
        node.handle(Request::Initialize {
            node_id: EntityId::new(),
            identity: None,
        })
        .unwrap();
        node
    }

    fn node_id(node: &Node) -> EntityId {
        node.local_node_id().unwrap().unwrap()
    }

    /// A real, independently verifiable primary entry: the certificate's
    /// voters are drawn from the members it names, so a one-voter fixture
    /// carries a real quorum certificate.
    fn primary_entry(node: &Node, index: u64, term: u64, epoch: u64) -> ControlEntry {
        let id = node_id(node);
        let config = node.member_configuration().unwrap().unwrap();
        ControlEntry {
            index,
            term,
            kind: ControlKind::Primary,
            payload: serde_json::to_value(PrimaryEntry {
                leader_id: id,
                epoch,
                config_revision: config.revision,
                config_digest: config.digest.clone(),
                members: vec![id],
                certificate: QuorumCertificate {
                    term,
                    voters: vec![id],
                },
            })
            .unwrap(),
        }
    }

    fn append(node: &mut Node, prev_index: u64, prev_term: u64, entries: Vec<ControlEntry>) -> AppendOutcome {
        node.append_control_entries(&AppendRequest {
            schema: ELECTION_WIRE_SCHEMA,
            leader_id: node_id(node),
            prev_index,
            prev_term,
            entries,
            proposal: None,
        })
        .unwrap()
    }

    #[test]
    fn an_append_that_skips_the_predecessor_is_a_log_mismatch_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = node(&dir.path().join("node"));
        node.ensure_member_configuration().unwrap();

        let skipped = primary_entry(&node, 3, 1, 1);
        let outcome = append(&mut node, 1, 1, vec![skipped]);
        assert_eq!(outcome.accepted, 0);
        assert!(outcome.reason.starts_with("log_mismatch"), "{}", outcome.reason);
        assert_eq!(outcome.commit_index, 1);

        // Nothing was applied: no hole was crossed, no fact was advanced.
        assert_eq!(Node::control_entry(&node.conn, 3).unwrap(), None);
        assert_eq!(node.control_log().unwrap().len(), 1);
        assert_eq!(node.committed_primary_claim().unwrap(), None);
        assert_eq!(node.current_term().unwrap(), 1);
        assert!(node.control_log_conflicts().unwrap().is_empty());
        let status = node.election_status().unwrap();
        assert_eq!(status["this_node_is_primary"], json!(false));
        assert_eq!(status["member_configuration"]["revision"], json!(1));
    }

    #[test]
    fn a_divergent_entry_is_refused_and_both_records_are_retained() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = node(&dir.path().join("node"));
        node.ensure_member_configuration().unwrap();

        let committed = primary_entry(&node, 2, 1, 1);
        let first = append(&mut node, 1, 1, vec![committed.clone()]);
        assert_eq!(first.accepted, 1);

        // The same index and term but a different decision is a conflict, not
        // a silent no-op.
        let divergent = primary_entry(&node, 2, 1, 9);
        let outcome = append(&mut node, 1, 1, vec![divergent.clone()]);
        assert_eq!(outcome.accepted, 0);
        assert!(outcome.reason.starts_with("log_conflict"), "{}", outcome.reason);
        assert_eq!(outcome.commit_index, 2);

        // The committed decision was not deleted or overwritten.
        assert_eq!(Node::control_entry(&node.conn, 2).unwrap().unwrap(), committed);
        assert_eq!(node.committed_primary_claim().unwrap().unwrap().epoch, 1);
        assert_eq!(node.control_log().unwrap().len(), 2);

        // Both sides of the divergence are retained as durable evidence.
        let conflicts = node.control_log_conflicts().unwrap();
        assert_eq!(conflicts.len(), 2);
        let committed_record = conflicts
            .iter()
            .find(|record| record.side == ControlConflictSide::Committed)
            .unwrap();
        let offered_record = conflicts
            .iter()
            .find(|record| record.side == ControlConflictSide::Offered)
            .unwrap();
        assert_eq!(committed_record.entry, committed);
        assert_eq!(offered_record.entry, divergent);
        assert_eq!(committed_record.conflict_id, offered_record.conflict_id);

        // Replaying the same divergent request does not duplicate evidence.
        let replay = append(&mut node, 1, 1, vec![divergent]);
        assert_eq!(replay.accepted, 0);
        assert_eq!(node.control_log_conflicts().unwrap().len(), 2);
    }

    #[test]
    fn an_identical_replay_is_idempotent_not_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = node(&dir.path().join("node"));
        node.ensure_member_configuration().unwrap();

        let entry = primary_entry(&node, 2, 1, 1);
        let first = append(&mut node, 1, 1, vec![entry.clone()]);
        assert_eq!(first.accepted, 1);
        assert_eq!(first.commit_index, 2);

        let replay = append(&mut node, 1, 1, vec![entry]);
        assert_eq!(replay.accepted, 0);
        assert_eq!(replay.reason, "accepted");
        assert_eq!(replay.commit_index, 2);
        assert_eq!(node.control_log().unwrap().len(), 2);
        assert!(node.control_log_conflicts().unwrap().is_empty());
        assert_eq!(node.committed_primary_claim().unwrap().unwrap().epoch, 1);
    }

    #[test]
    fn a_replay_does_not_promote_rows_above_the_recorded_commit_frontier() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = node(&dir.path().join("node"));
        node.ensure_member_configuration().unwrap();
        let entry = primary_entry(&node, 2, 1, 1);
        assert_eq!(append(&mut node, 1, 1, vec![entry.clone()]).commit_index, 2);
        // A legacy PushConfig can regress this field while leaving rows in
        // place. Presence alone cannot certify those rows as committed.
        let tx = node.conn.transaction().unwrap();
        set_state(&tx, "commit_index", &1u64.to_string()).unwrap();
        tx.commit().unwrap();
        let replay = append(&mut node, 1, 1, vec![entry.clone()]);
        assert_eq!(replay.accepted, 0);
        assert_eq!(replay.commit_index, 1);
        assert_eq!(state_number(&node.conn, "commit_index").unwrap(), 1);
        let third = primary_entry(&node, 3, 1, 2);
        let refused = append(&mut node, 2, 1, vec![third]);
        assert!(refused.reason.starts_with("log_mismatch"));
        assert_eq!(refused.commit_index, 1);
        assert!(Node::control_entry(&node.conn, 3).unwrap().is_none());
        let tail = primary_entry(&node, 3, 1, 2);
        let covered = append(&mut node, 1, 1, vec![entry, tail]);
        assert!(covered.reason.starts_with("log_mismatch"));
        assert_eq!(covered.commit_index, 1);
        assert_eq!(node.control_log().unwrap().len(), 2);
        assert!(Node::control_entry(&node.conn, 3).unwrap().is_none());
        assert_eq!(node.committed_primary_claim().unwrap().unwrap().epoch, 1);
    }

    #[test]
    fn a_configuration_push_cannot_certify_a_gapped_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = node(&dir.path().join("source"));
        let config = source.ensure_member_configuration().unwrap();
        let mut target = node(&dir.path().join("target"));
        let skipped = primary_entry(&source, 2, 2, 1);
        assert!(target.adopt_member_configuration(&config, &[skipped]).is_err());
        assert!(target.control_log().unwrap().is_empty());
        assert!(target.member_configuration().unwrap().is_none());
        assert!(target.committed_primary_claim().unwrap().is_none());
        assert_eq!(state_number(&target.conn, "commit_index").unwrap(), 0);
        assert_eq!(target.current_term().unwrap(), 0);
    }

    #[test]
    fn a_configuration_push_requires_its_configuration_to_be_committed_in_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = node(&dir.path().join("source"));
        let mut advertised = source.ensure_member_configuration().unwrap();
        advertised.revision += 1;
        let mut entries = source.control_log().unwrap();
        entries.push(primary_entry(&source, 2, 2, 1));
        let mut target = node(&dir.path().join("target"));
        assert!(target.adopt_member_configuration(&advertised, &entries).is_err());
        assert!(target.control_log().unwrap().is_empty());
        assert!(target.member_configuration().unwrap().is_none());
        assert!(target.committed_primary_claim().unwrap().is_none());
        assert_eq!(state_number(&target.conn, "commit_index").unwrap(), 0);
    }

    #[test]
    fn a_full_configuration_push_catches_up_without_replaying_or_rewinding_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = node(&dir.path().join("source"));
        let config = source.ensure_member_configuration().unwrap();
        let prefix = source.control_log().unwrap();
        let mut target = node(&dir.path().join("target"));
        target.adopt_member_configuration(&config, &prefix).unwrap();
        assert_eq!(target.member_configuration().unwrap().unwrap().committed_at, config.committed_at);
        let mut full = prefix.clone();
        full.push(primary_entry(&source, 2, 3, 7));
        // Equal configuration revision does not mean an equal committed log.
        target.adopt_member_configuration(&config, &full).unwrap();
        let claim = target.committed_primary_claim().unwrap().unwrap();
        assert_eq!(claim.epoch, 7);
        assert_eq!(target.current_term().unwrap(), 3);
        assert_eq!(state_number(&target.conn, "commit_index").unwrap(), 2);
        target.adopt_member_configuration(&config, &full).unwrap();
        assert_eq!(target.committed_primary_claim().unwrap().unwrap().committed_at, claim.committed_at);
        assert!(target.adopt_member_configuration(&config, &prefix).is_err());
        assert_eq!(target.control_log().unwrap(), full);
        assert_eq!(state_number(&target.conn, "commit_index").unwrap(), 2);
        assert_eq!(target.current_term().unwrap(), 3);
        assert_eq!(target.committed_primary_claim().unwrap().unwrap().epoch, 7);
    }

    #[test]
    fn a_contiguous_suffix_is_accepted_and_the_commit_index_advances() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = node(&dir.path().join("node"));
        node.ensure_member_configuration().unwrap();

        let second = primary_entry(&node, 2, 1, 1);
        let third = primary_entry(&node, 3, 1, 2);
        let outcome = append(&mut node, 1, 1, vec![second, third]);
        assert_eq!(outcome.accepted, 2);
        assert_eq!(outcome.commit_index, 3);
        assert_eq!(node.control_log().unwrap().len(), 3);
        assert_eq!(node.committed_primary_claim().unwrap().unwrap().epoch, 2);
    }
}
