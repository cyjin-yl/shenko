//! Sequential, durable consensus slots for the Node control plane.
//!
//! Election votes are phase-one promises, not replication. A value becomes a
//! committed control fact only after phase-two acceptors have durably stored
//! that exact value and prefix. A new proposer recovers the highest accepted
//! value reported by its prepare quorum before it may choose a new value.
//! This is the sequential state-machine construction from Paxos Made Simple,
//! not a Raft implementation. Experiences live outside this control log.

use super::{Node, Result, digest, text};
use crate::control_transport::*;
use crate::election::*;
use anyhow::{Context as _, ensure};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{ids::EntityId, object::Digest};
use std::collections::BTreeSet;

pub(crate) const CONTROL_DURABILITY_SCHEMA: &str = "
CREATE TABLE node_control_promises(term INTEGER NOT NULL,voter TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(term,voter));
CREATE TABLE node_control_acceptances(idx INTEGER NOT NULL,term INTEGER NOT NULL,proposer TEXT NOT NULL,digest TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(idx,term,proposer));
CREATE TABLE node_control_rounds(idx INTEGER NOT NULL,term INTEGER NOT NULL,proposer TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(idx,term,proposer));
CREATE TABLE node_control_admissions(conflict_key TEXT PRIMARY KEY,action_id TEXT UNIQUE NOT NULL,idx INTEGER NOT NULL,data TEXT NOT NULL);
CREATE TABLE node_control_acknowledgements(receipt TEXT PRIMARY KEY,idx INTEGER NOT NULL,term INTEGER NOT NULL,proposer TEXT NOT NULL,voter TEXT NOT NULL,data TEXT NOT NULL);";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct ControlBallot {
    pub term: u64,
    pub proposer: EntityId,
}

/// An accepted value survives process death even when no learner knows that a
/// majority accepted it. Its value identity excludes the later commit proof.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AcceptedControl {
    pub ballot: ControlBallot,
    pub configuration_revision: u64,
    pub configuration_digest: Digest,
    pub prefix_digest: Digest,
    pub entry: ControlEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlAcknowledgement {
    pub schema: u32,
    pub responder_id: EntityId,
    pub ballot: ControlBallot,
    pub index: u64,
    pub entry_digest: Digest,
    pub prefix_digest: Digest,
    pub configuration_revision: u64,
    pub configuration_digest: Digest,
    pub accepted: bool,
    pub observed_term: u64,
    pub reason: String,
}

/// These are actual storage responses, not election voter names. The trusted
/// authenticated transport binds each response to its configured identity;
/// this is crash-fault consensus, not a Byzantine signature certificate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DurableControlCommit {
    pub ballot: ControlBallot,
    pub acknowledgements: Vec<ControlAcknowledgement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPrepareProof {
    pub request: VoteRequest,
    pub responses: Vec<ControlPreparePromise>,
    pub recovery: Option<AcceptedControl>,
}

/// Phase-one witnesses name accepted values by digest. Their original full
/// replies remain in the receipt table; the replication proof includes only
/// the single highest accepted value, avoiding N copies of a large payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPreparePromise {
    pub responder_id: EntityId,
    pub request_id: EntityId,
    pub term: u64,
    pub config_digest: Digest,
    pub committed: ControlFrontier,
    pub accepted: Option<ControlAcceptanceStamp>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlAcceptanceStamp {
    pub ballot: ControlBallot,
    pub index: u64,
    pub entry_digest: Digest,
    pub prefix_digest: Digest,
    pub configuration_revision: u64,
    pub configuration_digest: Digest,
}

impl ControlAcceptanceStamp {
    fn of(accepted: &AcceptedControl) -> Result<Self> {
        Ok(Self {
            ballot: accepted.ballot.clone(),
            index: accepted.entry.index,
            entry_digest: control_value_digest(&accepted.entry)?,
            prefix_digest: accepted.prefix_digest.clone(),
            configuration_revision: accepted.configuration_revision,
            configuration_digest: accepted.configuration_digest.clone(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlReplication {
    pub schema: u32,
    pub prepare: ControlPrepareProof,
    pub accepted: AcceptedControl,
    pub predecessor: ControlFrontier,
    pub configuration: MemberConfiguration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlCommitOutcome {
    pub committed: bool,
    pub index: u64,
    pub entry: Option<ControlEntry>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPromise {
    request: VoteRequest,
    response: VoteResponse,
}

pub(crate) fn control_value_digest(entry: &ControlEntry) -> Result<Digest> {
    digest(&(entry.index, entry.term, entry.kind, &entry.payload))
}

fn entry_quorum(
    config: &MemberConfiguration,
    entry: &ControlEntry,
    voters: &BTreeSet<EntityId>,
) -> Result<bool> {
    match entry.kind {
        ControlKind::MemberConfiguration => {
            let payload: ConfigurationEntry = serde_json::from_value(entry.payload.clone())?;
            Ok(payload
                .proposal_old
                .iter()
                .filter(|id| voters.contains(id))
                .count()
                > payload.proposal_old.len() / 2
                && payload
                    .proposal_new
                    .iter()
                    .filter(|id| voters.contains(id))
                    .count()
                    > payload.proposal_new.len() / 2)
        }
        _ => Ok(config.has_quorum(voters)),
    }
}

fn entry_voters(config: &MemberConfiguration, entry: &ControlEntry) -> Result<BTreeSet<EntityId>> {
    if entry.kind == ControlKind::MemberConfiguration {
        let payload: ConfigurationEntry = serde_json::from_value(entry.payload.clone())?;
        Ok(payload
            .proposal_old
            .into_iter()
            .chain(payload.proposal_new)
            .collect())
    } else {
        Ok(config.voters())
    }
}

fn ack_matches(ack: &ControlAcknowledgement, accepted: &AcceptedControl) -> Result<bool> {
    Ok((2..=ELECTION_WIRE_SCHEMA).contains(&ack.schema)
        && ack.accepted
        && ack.reason == "durably_accepted"
        && ack.ballot == accepted.ballot
        && ack.observed_term == accepted.ballot.term
        && ack.index == accepted.entry.index
        && ack.entry_digest == control_value_digest(&accepted.entry)?
        && ack.prefix_digest == accepted.prefix_digest
        && ack.configuration_revision == accepted.configuration_revision
        && ack.configuration_digest == accepted.configuration_digest)
}

impl Node {
    pub(crate) fn control_log_conn(conn: &Connection) -> Result<Vec<ControlEntry>> {
        let mut statement =
            conn.prepare("SELECT data FROM node_control_log WHERE idx<=?1 ORDER BY idx")?;
        let rows = statement.query_map(
            [i64::try_from(state_number(conn, "commit_index")?)?],
            |row| row.get::<_, String>(0),
        )?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub(crate) fn accepted_control_conn(conn: &Connection) -> Result<Option<AcceptedControl>> {
        let index = state_number(conn, "commit_index")?
            .checked_add(1)
            .context("control log exhausted")?;
        let raw: Option<String> = conn.query_row(
            "SELECT data FROM node_control_acceptances WHERE idx=?1 ORDER BY term DESC,proposer DESC LIMIT 1",
            [i64::try_from(index)?], |row| row.get(0),
        ).optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub(crate) fn record_control_promise(
        tx: &Transaction<'_>,
        request: &VoteRequest,
        response: &VoteResponse,
    ) -> Result<()> {
        if request.proposal.is_some() || !response.vote_granted {
            return Ok(());
        }
        let stored = StoredPromise {
            request: request.clone(),
            response: response.clone(),
        };
        let old: Option<String> = tx
            .query_row(
                "SELECT data FROM node_control_promises WHERE term=?1 AND voter=?2",
                params![
                    i64::try_from(request.term)?,
                    response.responder_id.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: StoredPromise = serde_json::from_str(&old)?;
            ensure!(
                old.request.request_id == request.request_id
                    && old.request.candidate_id == request.candidate_id
                    && old.request.prefix_digest == request.prefix_digest,
                "one prepare promise cannot be rebound to another campaign"
            );
            // The original promise snapshot is immutable. Later accepts do
            // not retroactively alter the information this phase one saw.
            return Ok(());
        }
        tx.execute(
            "INSERT INTO node_control_promises(term,voter,data) VALUES(?1,?2,?3)",
            params![
                i64::try_from(request.term)?,
                response.responder_id.to_string(),
                text(&stored)?
            ],
        )?;
        Ok(())
    }

    pub(crate) fn retained_control_promise(
        conn: &Connection,
        request: &VoteRequest,
        voter: EntityId,
    ) -> Result<Option<VoteResponse>> {
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM node_control_promises WHERE term=?1 AND voter=?2",
                params![i64::try_from(request.term)?, voter.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(raw) = raw else { return Ok(None) };
        let promise: StoredPromise = serde_json::from_str(&raw)?;
        Ok((promise.request.request_id == request.request_id).then_some(promise.response))
    }

    fn prepare_proof(
        conn: &Connection,
        term: u64,
        proposer: EntityId,
    ) -> Result<ControlPrepareProof> {
        let mut statement =
            conn.prepare("SELECT data FROM node_control_promises WHERE term=?1 ORDER BY voter")?;
        let rows = statement.query_map([i64::try_from(term)?], |row| row.get::<_, String>(0))?;
        let mut request = None;
        let mut responses = Vec::new();
        let mut recovery: Option<AcceptedControl> = None;
        for row in rows {
            let promise: StoredPromise = serde_json::from_str(&row?)?;
            if promise.request.candidate_id != proposer {
                continue;
            }
            if let Some(first) = &request {
                ensure!(
                    text(first)? == text(&promise.request)?,
                    "prepare responses name different campaigns"
                );
            } else {
                request = Some(promise.request);
            }
            let response = promise.response;
            ensure!(
                response.schema == ELECTION_WIRE_SCHEMA
                    && response.vote_granted
                    && response.proposal_digest.is_none(),
                "retained control promise is not a current election response"
            );
            if let Some(accepted) = &response.accepted_control
                && recovery
                    .as_ref()
                    .is_none_or(|old| accepted.ballot > old.ballot)
            {
                recovery = Some(accepted.clone());
            }
            responses.push(ControlPreparePromise {
                responder_id: response.responder_id,
                request_id: response.request_id,
                term: response.term,
                config_digest: response.config_digest,
                committed: response.committed,
                accepted: response
                    .accepted_control
                    .as_ref()
                    .map(ControlAcceptanceStamp::of)
                    .transpose()?,
            });
        }
        Ok(ControlPrepareProof {
            request: request.context("no retained prepare promises for this proposer")?,
            responses,
            recovery,
        })
    }

    /// Check phase one against its pinned predecessor configuration. A
    /// higher-ballot proposer must preserve the highest accepted value,
    /// including another leader's primary or membership command.
    fn verify_prepare(
        proof: &ControlPrepareProof,
        config: &MemberConfiguration,
    ) -> Result<Option<AcceptedControl>> {
        let request = &proof.request;
        ensure!(
            request.schema == ELECTION_WIRE_SCHEMA
                && request.proposal.is_none()
                && request.term > 0
                && request.config_revision == config.revision
                && request.config_digest == config.digest
                && config.is_member(request.candidate_id),
            "control prepare does not bind the governing configuration"
        );
        let mut voters = BTreeSet::new();
        let mut highest: Option<ControlAcceptanceStamp> = None;
        for response in &proof.responses {
            ensure!(
                response.term == request.term
                    && response.request_id == request.request_id
                    && response.config_digest == request.config_digest
                    && config.is_member(response.responder_id)
                    && voters.insert(response.responder_id),
                "invalid or repeated prepare responder"
            );
            ensure!(
                response.committed.index == request.last_log_index
                    && response.committed.term == request.last_log_term
                    && response.committed.digest == request.prefix_digest,
                "prepare quorum did not promise against one committed prefix"
            );
            if let Some(accepted) = &response.accepted {
                ensure!(
                    accepted.index
                        == request
                            .last_log_index
                            .checked_add(1)
                            .context("control log exhausted")?
                        && accepted.prefix_digest == request.prefix_digest
                        && accepted.configuration_revision == config.revision
                        && accepted.configuration_digest == config.digest
                        && accepted.ballot.term <= request.term,
                    "prepare response has an acceptance from another slot or configuration"
                );
                if let Some(prior) = &highest
                    && prior.ballot == accepted.ballot
                {
                    ensure!(
                        prior.entry_digest == accepted.entry_digest,
                        "one accepted ballot carries conflicting control values"
                    );
                }
                if highest
                    .as_ref()
                    .is_none_or(|old| accepted.ballot > old.ballot)
                {
                    highest = Some(accepted.clone());
                }
            }
        }
        ensure!(
            config.has_quorum(&voters),
            "control prepare has no configured quorum"
        );
        ensure!(
            highest
                == proof
                    .recovery
                    .as_ref()
                    .map(ControlAcceptanceStamp::of)
                    .transpose()?,
            "prepare recovery value is not the highest accepted value reported by its quorum"
        );
        Ok(proof.recovery.clone())
    }

    fn validate_replication(
        conn: &Connection,
        request: &ControlReplication,
    ) -> Result<MemberConfiguration> {
        ensure!(
            request.schema == ELECTION_WIRE_SCHEMA,
            "unsupported control durability wire schema"
        );
        let accepted = &request.accepted;
        let config = Self::configuration_at_frontier(conn, request.predecessor.index)?
            .context("accepted control value has no committed configuration")?;
        ensure!(
            config.revision == request.configuration.revision
                && config.digest == request.configuration.digest
                && config.members == request.configuration.members
                && config.joint_new == request.configuration.joint_new,
            "offered governing configuration differs from the retained predecessor"
        );
        ensure!(
            Self::control_frontier_at(conn, request.predecessor.index)? == request.predecessor
                && accepted.configuration_revision == config.revision
                && accepted.configuration_digest == config.digest
                && accepted.prefix_digest == request.predecessor.digest
                && Some(accepted.entry.index) == request.predecessor.index.checked_add(1)
                && accepted.ballot.term == request.prepare.request.term
                && accepted.ballot.proposer == request.prepare.request.candidate_id
                && accepted.entry.durability.is_none(),
            "accepted control value does not bind its exact predecessor and ballot"
        );
        let recovered = Self::verify_prepare(&request.prepare, &config)?;
        let prepare_index = request.prepare.request.last_log_index;
        ensure!(
            prepare_index < accepted.entry.index
                && Self::prepare_covers_index(
                    conn,
                    &request.prepare.request,
                    request.predecessor.index
                )?,
            "control prepare does not cover the offered committed prefix"
        );
        let recovering = accepted.entry.index == prepare_index + 1 && recovered.is_some();
        if accepted.entry.index == prepare_index + 1
            && let Some(recovered) = recovered
        {
            ensure!(
                control_value_digest(&recovered.entry)? == control_value_digest(&accepted.entry)?,
                "higher ballot must recover the highest accepted control value"
            );
        }
        if !recovering {
            ensure!(
                accepted.entry.term == accepted.ballot.term,
                "a fresh control value must originate in its accepting ballot"
            );
            if accepted.entry.kind == ControlKind::Primary {
                let primary: PrimaryEntry = serde_json::from_value(accepted.entry.payload.clone())?;
                ensure!(
                    primary.leader_id == accepted.ballot.proposer,
                    "fresh primary value does not name its prepared proposer"
                );
            } else {
                let primary =
                    Self::last_control_kind(conn, request.predecessor.index, ControlKind::Primary)?
                        .context("fresh control command has no committed primary")?;
                let payload: PrimaryEntry = serde_json::from_value(primary.payload)?;
                ensure!(
                    primary.term == accepted.ballot.term
                        && payload.leader_id == accepted.ballot.proposer
                        && payload.config_revision == config.revision,
                    "fresh control command is not issued by the current primary and configuration"
                );
            }
        }
        if accepted.entry.kind == ControlKind::Primary {
            let payload: PrimaryEntry = serde_json::from_value(accepted.entry.payload.clone())?;
            if let Some(previous) =
                Self::last_control_kind(conn, request.predecessor.index, ControlKind::Primary)?
            {
                let old: PrimaryEntry = serde_json::from_value(previous.payload)?;
                ensure!(
                    payload.epoch > old.epoch
                        && (previous.term != accepted.entry.term
                            || old.leader_id == payload.leader_id),
                    "accepted primary must advance its predecessor epoch without changing leaders in one term"
                );
            } else {
                ensure!(payload.epoch > 0, "initial primary epoch must be positive");
            }
        }
        Self::verify_control_entry(&accepted.entry, Some(&config))?;
        ensure!(
            accepted.entry.term >= request.predecessor.term
                && accepted.entry.term <= accepted.ballot.term,
            "control value term does not fit its predecessor and accepting ballot"
        );
        Self::check_control_frame(request, &config)?;
        Ok(config)
    }

    // Bound the eventual committed value before ANY acceptor acknowledges it.
    // The worst-case certificate includes every eligible voter. CAS plan bytes
    // are transferred separately and never make this control frame grow.
    fn check_control_frame(
        request: &ControlReplication,
        config: &MemberConfiguration,
    ) -> Result<()> {
        let mut entry = request.accepted.entry.clone();
        let acknowledgements = entry_voters(config, &entry)?
            .into_iter()
            .map(|voter| {
                Self::acknowledge(
                    &request.accepted,
                    voter,
                    request.accepted.ballot.term,
                    true,
                    "durably_accepted",
                )
            })
            .collect::<Result<Vec<_>>>()?;
        entry.durability = Some(DurableControlCommit {
            ballot: request.accepted.ballot.clone(),
            acknowledgements,
        });
        ensure!(
            serde_json::to_vec(&entry)?.len() <= MAX_CONTROL_ENTRY_BYTES,
            "control metadata and full persistence certificate exceed the bounded entry frame"
        );
        ensure!(
            serde_json::to_vec(request)?.len() <= crate::MAX_NODE_REQUEST / 4,
            "control prepare proof exceeds the bounded replication frame"
        );
        Ok(())
    }

    pub(crate) fn verify_durable_control(
        entry: &ControlEntry,
        config: &MemberConfiguration,
        prefix_digest: &Digest,
    ) -> Result<()> {
        let proof = entry
            .durability
            .as_ref()
            .context("control entry has no durable quorum proof")?;
        let accepted = AcceptedControl {
            ballot: proof.ballot.clone(),
            configuration_revision: config.revision,
            configuration_digest: config.digest.clone(),
            prefix_digest: prefix_digest.clone(),
            entry: entry.clone(),
        };
        ensure!(
            proof.ballot.term >= entry.term && config.is_member(proof.ballot.proposer),
            "durable proof ballot is not governed by the predecessor configuration"
        );
        let allowed = entry_voters(config, entry)?;
        let mut voters = BTreeSet::new();
        for ack in &proof.acknowledgements {
            ensure!(
                ack_matches(ack, &accepted)?
                    && allowed.contains(&ack.responder_id)
                    && voters.insert(ack.responder_id),
                "durable control proof contains an invalid or repeated persistence response"
            );
        }
        ensure!(
            entry_quorum(config, entry, &voters)?,
            "control value lacks a durable configured quorum"
        );
        Ok(())
    }

    fn acknowledge(
        accepted: &AcceptedControl,
        voter: EntityId,
        term: u64,
        ok: bool,
        reason: &str,
    ) -> Result<ControlAcknowledgement> {
        Ok(ControlAcknowledgement {
            schema: ELECTION_WIRE_SCHEMA,
            responder_id: voter,
            ballot: accepted.ballot.clone(),
            index: accepted.entry.index,
            entry_digest: control_value_digest(&accepted.entry)?,
            prefix_digest: accepted.prefix_digest.clone(),
            configuration_revision: accepted.configuration_revision,
            configuration_digest: accepted.configuration_digest.clone(),
            accepted: ok,
            observed_term: term,
            reason: reason.into(),
        })
    }

    fn save_ack(tx: &Transaction<'_>, ack: &ControlAcknowledgement) -> Result<()> {
        tx.execute("INSERT OR IGNORE INTO node_control_acknowledgements(receipt,idx,term,proposer,voter,data) VALUES(?1,?2,?3,?4,?5,?6)",
            params![digest(ack)?.0, i64::try_from(ack.index)?, i64::try_from(ack.ballot.term)?,
                ack.ballot.proposer.to_string(), ack.responder_id.to_string(), text(ack)?])?;
        Ok(())
    }

    fn accept_control_tx(
        tx: &Transaction<'_>,
        request: &ControlReplication,
    ) -> Result<ControlAcknowledgement> {
        let accepted = &request.accepted;
        let own = Self::stored_node_id(tx)?.context("node not initialized")?;
        let term = state_number(tx, "term")?;
        ensure!(
            request.schema == ELECTION_WIRE_SCHEMA,
            "unsupported control durability wire schema"
        );
        // A stale request needs no fresh permission to receive a refusal. The
        // same immutable predecessor may have been learned with another valid
        // recovery certificate, so validate its live prepare only after this
        // fence. Negative receipts never count toward a commit.
        if accepted.ballot.term < term {
            return Self::acknowledge(accepted, own, term, false, "promised_a_higher_ballot");
        }
        let config = Self::validate_replication(tx, request)?;
        let prefix = Self::control_frontier_conn(tx)?;
        let local_config =
            Self::committed_config(tx)?.context("no committed member configuration")?;
        let refuse = |reason: &str| Self::acknowledge(accepted, own, term, false, reason);
        if local_config.revision != config.revision
            || local_config.digest != config.digest
            || prefix.digest != accepted.prefix_digest
            || prefix.index + 1 != accepted.entry.index
        {
            return refuse("committed_prefix_mismatch");
        }
        if !entry_voters(&config, &accepted.entry)?.contains(&own) {
            return refuse("not_a_control_acceptor");
        }
        let voted: Option<EntityId> = get_state::<Option<EntityId>>(tx, "voted_for")?.flatten();
        if accepted.ballot.term == term
            && voted.is_some_and(|voter| voter != accepted.ballot.proposer)
        {
            return refuse("promised_another_proposer_in_this_term");
        }
        if accepted.entry.kind == ControlKind::Admission {
            let payload: ControlAdmissionPayload =
                serde_json::from_value(accepted.entry.payload.clone())?;
            ensure!(
                Self::admission_conflict_index(tx, &payload.admission)?.is_none(),
                "admission position, predecessor or action already committed; replay the original grant instead of accepting another slot"
            );
            Self::validate_admission_reads(tx, &payload.admission, Some(accepted.entry.index))?;
            Self::store_admission_objects_tx(tx, &payload.objects)?;
            Self::verify_admission_closure_tx(tx, &payload.admission.payload_ref)?;
        }
        // Promise and accepted bytes commit together before an acknowledgement
        // can leave this process. All older accepted values remain evidence.
        set_state(tx, "term", &accepted.ballot.term.to_string())?;
        set_state(tx, "voted_for", &Some(accepted.ballot.proposer))?;
        if own != accepted.ballot.proposer {
            set_state(tx, "role", &ElectionRole::Follower)?;
        }
        let value_digest = control_value_digest(&accepted.entry)?;
        let existing: Option<String> = tx.query_row(
            "SELECT digest FROM node_control_acceptances WHERE idx=?1 AND term=?2 AND proposer=?3",
            params![i64::try_from(accepted.entry.index)?, i64::try_from(accepted.ballot.term)?, accepted.ballot.proposer.to_string()],
            |row| row.get(0),
        ).optional()?;
        ensure!(
            existing.as_ref().is_none_or(|old| *old == value_digest.0),
            "a proposer attempted to change an accepted value within one ballot"
        );
        tx.execute("INSERT OR IGNORE INTO node_control_acceptances(idx,term,proposer,digest,data) VALUES(?1,?2,?3,?4,?5)",
            params![i64::try_from(accepted.entry.index)?, i64::try_from(accepted.ballot.term)?,
                accepted.ballot.proposer.to_string(), value_digest.0, text(accepted)?])?;
        let ack = Self::acknowledge(
            accepted,
            own,
            accepted.ballot.term,
            true,
            "durably_accepted",
        )?;
        Self::save_ack(tx, &ack)?;
        Ok(ack)
    }

    /// Persist a proposed control value. Learning the certified predecessor is
    /// factual catch-up; it grants no permission to accept under a stale term.
    pub fn accept_control(
        &mut self,
        request: &ControlReplication,
    ) -> Result<ControlAcknowledgement> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ack = Self::accept_control_tx(&tx, request)?;
        tx.commit()?;
        Ok(ack)
    }

    pub(crate) fn stage_control_value(
        tx: &Transaction<'_>,
        desired: ControlEntry,
    ) -> Result<ControlReplication> {
        let proposer = Self::stored_node_id(tx)?.context("node not initialized")?;
        let term = state_number(tx, "term")?;
        let index = state_number(tx, "commit_index")?
            .checked_add(1)
            .context("control log exhausted")?;
        ensure!(
            desired.index == index,
            "control proposal is not the next uncommitted slot"
        );
        let existing: Option<String> = tx
            .query_row(
                "SELECT data FROM node_control_rounds WHERE idx=?1 AND term=?2 AND proposer=?3",
                params![
                    i64::try_from(index)?,
                    i64::try_from(term)?,
                    proposer.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(raw) = existing {
            return Ok(serde_json::from_str(&raw)?);
        }
        let config = Self::committed_config(tx)?.context("no committed member configuration")?;
        let prepare = Self::prepare_proof(tx, term, proposer)?;
        let recovered = Self::verify_prepare(&prepare, &config)?;
        let mut entry = if index == prepare.request.last_log_index + 1 {
            recovered.map_or(desired, |accepted| accepted.entry)
        } else {
            desired
        };
        entry.durability = None;
        let predecessor = Self::control_frontier_conn(tx)?;
        let accepted = AcceptedControl {
            ballot: ControlBallot { term, proposer },
            configuration_revision: config.revision,
            configuration_digest: config.digest.clone(),
            prefix_digest: predecessor.digest.clone(),
            entry,
        };
        let request = ControlReplication {
            schema: ELECTION_WIRE_SCHEMA,
            prepare,
            accepted,
            predecessor,
            configuration: config,
        };
        let ack = Self::accept_control_tx(tx, &request)?;
        ensure!(
            ack.accepted,
            "local control acceptance refused: {}",
            ack.reason
        );
        tx.execute(
            "INSERT INTO node_control_rounds(idx,term,proposer,data) VALUES(?1,?2,?3,?4)",
            params![
                i64::try_from(index)?,
                i64::try_from(term)?,
                proposer.to_string(),
                text(&request)?
            ],
        )?;
        Ok(request)
    }

    /// The exact durable pending request, reusable after a lost response or a
    /// process restart. A new ballot must run phase one instead of reusing it.
    pub fn pending_control_replication(&self) -> Result<Option<ControlReplication>> {
        let index = state_number(&self.conn, "commit_index")?
            .checked_add(1)
            .context("control log exhausted")?;
        let term = state_number(&self.conn, "term")?;
        let own = Self::stored_node_id(&self.conn)?.context("node not initialized")?;
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM node_control_rounds WHERE idx=?1 AND term=?2 AND proposer=?3",
                params![i64::try_from(index)?, i64::try_from(term)?, own.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub fn control_replication_voters(request: &ControlReplication) -> Result<Vec<EntityId>> {
        let config = &request.configuration;
        Ok(entry_voters(config, &request.accepted.entry)?
            .into_iter()
            .collect())
    }

    /// Store an authenticated peer response, preserving late receipts as facts
    /// without granting a superseded proposer any new authority.
    pub fn record_control_acknowledgement(
        &mut self,
        request: &ControlReplication,
        ack: &ControlAcknowledgement,
    ) -> Result<()> {
        let config = Self::validate_replication(&self.conn, request)?;
        ensure!(
            ack.schema == ELECTION_WIRE_SCHEMA
                && ack.ballot == request.accepted.ballot
                && ack.index == request.accepted.entry.index
                && entry_voters(&config, &request.accepted.entry)?.contains(&ack.responder_id),
            "control acknowledgement does not name this request and configured responder"
        );
        if ack.accepted {
            ensure!(
                ack_matches(ack, &request.accepted)?,
                "persistence response changed the accepted value"
            )
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::save_ack(&tx, ack)?;
        let current_voter = if let Some(current) = Self::committed_config(&tx)? {
            current.is_member(ack.responder_id)
                || (current.revision == config.revision
                    && current.digest == config.digest
                    && entry_voters(&current, &request.accepted.entry)?.contains(&ack.responder_id))
        } else {
            false
        };
        if current_voter && ack.observed_term > state_number(&tx, "term")? {
            set_state(&tx, "term", &ack.observed_term.to_string())?;
            set_state(&tx, "voted_for", &Option::<EntityId>::None)?;
            set_state(&tx, "role", &ElectionRole::Follower)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn commit_control_tx(
        tx: &Transaction<'_>,
        request: &ControlReplication,
    ) -> Result<ControlCommitOutcome> {
        let accepted = &request.accepted;
        if let Some(existing) = Self::control_entry(tx, accepted.entry.index)? {
            ensure!(
                control_value_digest(&existing)? == control_value_digest(&accepted.entry)?,
                "committed control slot contains another value"
            );
            return Ok(ControlCommitOutcome {
                committed: true,
                index: existing.index,
                entry: Some(existing),
                reason: "already_committed".into(),
            });
        }
        let config = Self::validate_replication(tx, request)?;
        let term = state_number(tx, "term")?;
        // Learning an already chosen value is factual even after a higher
        // promise. Its original term remains intact and cannot restore the
        // former primary's usable fence.
        let local_config = Self::committed_config(tx)?.context("no committed configuration")?;
        ensure!(
            local_config.revision == config.revision
                && local_config.digest == config.digest
                && Self::control_frontier_conn(tx)?.digest == accepted.prefix_digest,
            "control commit predecessor changed"
        );
        let mut statement = tx.prepare("SELECT data FROM node_control_acknowledgements WHERE idx=?1 AND term=?2 AND proposer=?3 ORDER BY voter,receipt")?;
        let rows = statement.query_map(
            params![
                i64::try_from(accepted.entry.index)?,
                i64::try_from(accepted.ballot.term)?,
                accepted.ballot.proposer.to_string()
            ],
            |row| row.get::<_, String>(0),
        )?;
        let allowed = entry_voters(&config, &accepted.entry)?;
        let mut voters = BTreeSet::new();
        let mut acknowledgements = Vec::new();
        for row in rows {
            let ack: ControlAcknowledgement = serde_json::from_str(&row?)?;
            if ack_matches(&ack, accepted)?
                && allowed.contains(&ack.responder_id)
                && voters.insert(ack.responder_id)
            {
                acknowledgements.push(ack);
            }
        }
        if !entry_quorum(&config, &accepted.entry, &voters)? {
            return Ok(ControlCommitOutcome {
                committed: false,
                index: accepted.entry.index,
                entry: None,
                reason: "no_durable_control_quorum".into(),
            });
        }
        let mut committed = accepted.entry.clone();
        committed.durability = Some(DurableControlCommit {
            ballot: accepted.ballot.clone(),
            acknowledgements,
        });
        Self::verify_durable_control(&committed, &config, &request.predecessor.digest)?;
        tx.execute(
            "INSERT INTO node_control_log(idx,term,kind,data) VALUES(?1,?2,?3,?4)",
            params![
                i64::try_from(committed.index)?,
                i64::try_from(committed.term)?,
                control_kind_name(committed.kind),
                text(&committed)?
            ],
        )?;
        Self::apply_control_entry(tx, &committed)?;
        set_state(tx, "commit_index", &committed.index.to_string())?;
        if let Some(claim) = get_state::<PrimaryClaim>(tx, "primary")?
            && claim.term == term
            && claim.leader_node_id == accepted.ballot.proposer.to_string()
            && Self::stored_node_id(tx)? == Some(accepted.ballot.proposer)
        {
            set_state(tx, "role", &ElectionRole::Leader)?;
        }
        Ok(ControlCommitOutcome {
            committed: true,
            index: committed.index,
            entry: Some(committed),
            reason: "durable_control_quorum".into(),
        })
    }

    pub fn commit_prepared_control(
        &mut self,
        request: &ControlReplication,
    ) -> Result<ControlCommitOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = Self::commit_control_tx(&tx, request)?;
        tx.commit()?;
        Ok(outcome)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlAdmissionPayload {
    pub admission: shenko_types::authority::CommitAdmission,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objects: Vec<shenko_types::authority::CommitAdmissionObject>,
}

impl ControlAdmissionPayload {
    pub(crate) fn verify(&self) -> Result<()> {
        ensure!(
            self.admission.schema_version == 1 && !self.admission.conflict_key.trim().is_empty(),
            "invalid control admission descriptor"
        );
        let mut keys = BTreeSet::from([self.admission.conflict_key.as_str()]);
        for alias in &self.admission.conflict_aliases {
            ensure!(
                !alias.trim().is_empty() && keys.insert(alias),
                "control admission aliases must be nonempty distinct positions"
            );
        }
        let mut reads = BTreeSet::new();
        ensure!(
            self.admission
                .required_unconsumed
                .iter()
                .all(|key| !key.trim().is_empty() && reads.insert(key)),
            "control admission read fences must be nonempty distinct positions"
        );
        if self.objects.is_empty() {
            return Ok(());
        }
        let mut seen = std::collections::BTreeMap::new();
        for object in &self.objects {
            ensure!(
                object.reference.digest == Digest::blake3(&object.bytes)
                    && object.reference.bytes == u64::try_from(object.bytes.len())?,
                "admission closure object digest or length mismatch"
            );
            if let Some(previous) = seen.insert(object.reference.digest.clone(), &object.bytes) {
                ensure!(
                    previous == &object.bytes,
                    "admission closure repeats a digest with different bytes"
                );
            }
        }
        let mut pending = vec![self.admission.payload_ref.clone()];
        let mut json_visited = BTreeSet::new();
        while let Some(reference) = pending.pop() {
            let bytes = seen
                .get(&reference.digest)
                .context("admission control value is missing its frozen plan object closure")?;
            ensure!(
                reference.bytes == u64::try_from(bytes.len())?,
                "admission closure reference has a different byte length"
            );
            // Match the existing Store closure rule: opaque bytes never gain
            // graph authority merely because they happen to parse as JSON.
            // A later typed alias of the same bytes still gets traversed.
            if (reference.media_type == "application/json"
                || reference.media_type.ends_with("+json"))
                && json_visited.insert(reference.digest)
            {
                let value: serde_json::Value = serde_json::from_slice(bytes)?;
                collect_admission_references(&value, &mut pending);
            }
        }
        Ok(())
    }
}

pub(crate) fn collect_admission_references(
    value: &serde_json::Value,
    references: &mut Vec<shenko_types::object::ObjectRef>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if map.contains_key("digest")
                && map.contains_key("bytes")
                && map.contains_key("codec")
                && let Ok(reference) =
                    serde_json::from_value::<shenko_types::object::ObjectRef>(value.clone())
            {
                references.push(reference);
                return;
            }
            for nested in map.values() {
                collect_admission_references(nested, references);
            }
        }
        serde_json::Value::Array(values) => {
            for nested in values {
                collect_admission_references(nested, references);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlAdmissionOutcome {
    pub grant: Option<shenko_types::authority::CommitAuthorityGrant>,
    pub pending: bool,
    /// True only for the caller that durably created this local admission
    /// round. A restart/retry never renews the original dispatch opportunity.
    pub freshly_prepared: bool,
}

impl Node {
    pub(crate) fn admission_conflict_index(
        conn: &Connection,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> Result<Option<u64>> {
        let mut first: Option<u64> = conn
            .query_row(
                "SELECT idx FROM node_control_admissions WHERE action_id=?1",
                [admission.action_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        for key in std::iter::once(&admission.conflict_key).chain(&admission.conflict_aliases) {
            let index: Option<u64> = conn
                .query_row(
                    "SELECT idx FROM node_control_admission_keys WHERE conflict_key=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(index) = index {
                first = Some(first.map_or(index, |old| old.min(index)));
            }
        }
        Ok(first)
    }

    pub(crate) fn validate_admission_reads(
        conn: &Connection,
        admission: &shenko_types::authority::CommitAdmission,
        prior_to: Option<u64>,
    ) -> Result<()> {
        for key in &admission.required_unconsumed {
            let consumed: Option<u64> = conn
                .query_row(
                    "SELECT idx FROM node_control_admission_keys WHERE conflict_key=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(index) = consumed
                && prior_to.is_none_or(|frontier| index < frontier)
            {
                return Err(ControlAdmissionError::AuthorityUnavailable(format!(
                    "read predecessor {key} was consumed at control index {index}"
                ))
                .into());
            }
        }
        Ok(())
    }

    /// Recheck a fresh grant under the owned Node reservation. Historical
    /// grant lookup remains available even after a later cancellation; it is
    /// evidence for ordered recovery, never a renewed dispatch permit.
    pub(crate) fn validate_admission_current_conn(
        conn: &Connection,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> Result<()> {
        Self::committed_admission_conn(conn, admission)?
            .context("admission has no committed grant")?;
        let index = Self::admission_conflict_index(conn, admission)?
            .context("committed admission index missing")?;
        for key in &admission.required_unconsumed {
            let consumed: Option<u64> = conn
                .query_row(
                    "SELECT idx FROM node_control_admission_keys WHERE conflict_key=?1",
                    [key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(consumed) = consumed
                && consumed != index
            {
                return Err(ControlAdmissionError::AuthorityUnavailable(format!(
                    "read predecessor {key} was consumed at control index {consumed}"
                ))
                .into());
            }
        }
        Ok(())
    }

    pub(crate) fn apply_control_admission(
        tx: &Transaction<'_>,
        entry: &ControlEntry,
    ) -> Result<()> {
        let payload: ControlAdmissionPayload = serde_json::from_value(entry.payload.clone())?;
        payload.verify()?;
        Self::store_admission_objects_tx(tx, &payload.objects)?;
        Self::verify_admission_closure_tx(tx, &payload.admission.payload_ref)?;
        Self::validate_admission_reads(tx, &payload.admission, Some(entry.index))?;
        let admission = payload.admission;
        if let Some(index) = Self::admission_conflict_index(tx, &admission)? {
            let raw: String = tx.query_row(
                "SELECT data FROM node_control_admissions WHERE idx=?1",
                [i64::try_from(index)?],
                |row| row.get(0),
            )?;
            ensure!(
                index == entry.index
                    && serde_json::from_str::<shenko_types::authority::CommitAdmission>(&raw)?
                        == admission,
                "an admission position, predecessor or action already has another committed grant"
            );
            return Ok(());
        }
        for key in std::iter::once(&admission.conflict_key).chain(&admission.conflict_aliases) {
            tx.execute(
                "INSERT INTO node_control_admission_keys(conflict_key,idx) VALUES(?1,?2)",
                params![key, i64::try_from(entry.index)?],
            )?;
        }
        tx.execute("INSERT INTO node_control_admissions(conflict_key,action_id,idx,data) VALUES(?1,?2,?3,?4)",
            params![admission.conflict_key, admission.action_id.to_string(), i64::try_from(entry.index)?, text(&admission)?])?;
        Ok(())
    }

    pub(crate) fn committed_admission_conn(
        conn: &Connection,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> Result<Option<shenko_types::authority::CommitAuthorityGrant>> {
        let Some(index) = Self::admission_conflict_index(conn, admission)? else {
            return Ok(None);
        };
        let raw: String = conn.query_row(
            "SELECT data FROM node_control_admissions WHERE idx=?1",
            [i64::try_from(index)?],
            |row| row.get(0),
        )?;
        let original: shenko_types::authority::CommitAdmission = serde_json::from_str(&raw)?;

        let entry = Self::control_entry(conn, index)?
            .context("admission projection is missing its committed control value")?;
        ensure!(
            entry.durability.is_some(),
            "legacy control data is not a quorum admission"
        );
        let grant = shenko_types::authority::CommitAuthorityGrant {
            admission: original,
            grant_id: format!("control:{}:{}", index, control_value_digest(&entry)?.0),
            proof: serde_json::to_value(&entry)?,
            replayed: true,
        };
        if grant.admission != *admission {
            return Err(ControlAdmissionError::ExistingAdmission {
                original: Box::new(grant),
            }
            .into());
        }
        Ok(Some(grant))
    }

    pub fn committed_admission(
        &self,
        admission: &shenko_types::authority::CommitAdmission,
    ) -> Result<Option<shenko_types::authority::CommitAuthorityGrant>> {
        Self::committed_admission_conn(&self.conn, admission)
    }

    /// Ordered immutable grants, including their frozen plan/object closure.
    /// Startup recovery must apply or reconcile these original plans before
    /// proposing replacements for the same decision positions.
    pub fn committed_admissions(
        &self,
        after_index: u64,
    ) -> Result<Vec<shenko_types::authority::CommitAuthorityGrant>> {
        let mut statement = self
            .conn
            .prepare("SELECT data FROM node_control_admissions WHERE idx>?1 ORDER BY idx")?;
        let rows =
            statement.query_map([i64::try_from(after_index)?], |row| row.get::<_, String>(0))?;
        let mut grants = Vec::new();
        for raw in rows {
            let admission = serde_json::from_str(&raw?)?;
            grants.push(
                Self::committed_admission_conn(&self.conn, &admission)?
                    .context("committed admission projection vanished")?,
            );
        }
        Ok(grants)
    }

    /// Propose one frozen Store admission. The conflict key spans controller
    /// changes; an exact replay returns the original grant, and changed content
    /// cannot obtain a second successor or a new physical dispatch permit.
    pub fn propose_admission(
        &mut self,
        admission: &shenko_types::authority::CommitAdmission,
        objects: &[shenko_types::authority::CommitAdmissionObject],
    ) -> Result<ControlAdmissionOutcome> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(grant) = Self::committed_admission_conn(&tx, admission)? {
            tx.commit()?;
            return Ok(ControlAdmissionOutcome {
                grant: Some(grant),
                pending: false,
                freshly_prepared: false,
            });
        }
        Self::validate_admission_reads(&tx, admission, None)?;
        let own = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let token = Self::fencing_token_conn(&tx)?.ok_or_else(|| {
            ControlAdmissionError::AuthorityUnavailable(
                "no current durable primary for this admission".into(),
            )
        })?;
        if token.leader_id != own {
            return Err(ControlAdmissionError::AuthorityUnavailable(
                "another node is the current primary".into(),
            )
            .into());
        }
        let payload = ControlAdmissionPayload {
            admission: admission.clone(),
            objects: Vec::new(),
        };
        payload.verify()?;
        Self::store_admission_objects_tx(&tx, objects)?;
        Self::verify_admission_closure_tx(&tx, &payload.admission.payload_ref)?;
        let entry = ControlEntry {
            schema: ELECTION_WIRE_SCHEMA,
            index: state_number(&tx, "commit_index")?
                .checked_add(1)
                .context("control log exhausted")?,
            term: token.term,
            kind: ControlKind::Admission,
            payload: serde_json::to_value(&payload)?,
            durability: None,
        };
        let existing_round: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM node_control_rounds WHERE idx=?1 AND term=?2 AND proposer=?3)",
            params![i64::try_from(entry.index)?, i64::try_from(entry.term)?, own.to_string()], |row| row.get(0),
        )?;
        let freshly_prepared = !existing_round;
        let prepared = Self::stage_control_value(&tx, entry)?;
        if prepared.accepted.entry.kind != ControlKind::Admission
            || serde_json::from_value::<ControlAdmissionPayload>(
                prepared.accepted.entry.payload.clone(),
            )?
            .admission
                != *admission
        {
            return Err(ControlAdmissionError::RecoveryRequired {
                value: prepared.accepted.entry,
            }
            .into());
        }
        let outcome = Self::commit_control_tx(&tx, &prepared)?;
        let mut grant = Self::committed_admission_conn(&tx, admission)?;
        if let Some(grant) = &mut grant {
            grant.replayed = !freshly_prepared;
        }
        tx.commit()?;
        Ok(ControlAdmissionOutcome {
            grant,
            pending: !outcome.committed,
            freshly_prepared,
        })
    }
}

/// Recoverable authority outcomes are distinct from malformed plans or local
/// storage errors. Runtime supervision may restart on these outcomes; it must
/// not hide an invalid descriptor or corrupt database behind a quorum retry.
pub enum ControlAdmissionError {
    AuthorityUnavailable(String),
    QuorumUnavailable(String),
    ExistingAdmission {
        original: Box<shenko_types::authority::CommitAuthorityGrant>,
    },
    RecoveryRequired {
        value: ControlEntry,
    },
}

impl std::fmt::Display for ControlAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AuthorityUnavailable(reason) => {
                write!(formatter, "control authority unavailable: {reason}")
            }
            Self::QuorumUnavailable(reason) => {
                write!(formatter, "durable control quorum unavailable: {reason}")
            }
            Self::ExistingAdmission { original } => write!(
                formatter,
                "recover original admission {} before replacing its decision position",
                original.grant_id
            ),
            Self::RecoveryRequired { value } => write!(
                formatter,
                "recover accepted control value at index {} before proposing another action",
                value.index
            ),
        }
    }
}

impl std::fmt::Debug for ControlAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, formatter)
    }
}
impl std::error::Error for ControlAdmissionError {}

impl Node {
    /// Retrieve the exact original plan closure from a surviving control
    /// replica. Transport and persistence layout do not leak into Store.
    pub fn admission_objects(
        &self,
        root: &shenko_types::object::ObjectRef,
    ) -> Result<Vec<shenko_types::authority::CommitAdmissionObject>> {
        Self::admission_objects_conn(&self.conn, root)
    }
}
