//! Availability-component agreement, separate from the global control log.
//!
//! A component requires the exact observed member set to acknowledge one
//! coordinator. The set never replaces the configured global electorate.
//! Changing a local component promise fences its former branch immediately;
//! other nodes learn that change through authenticated confirmation/recovery.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{ids::EntityId, object::Digest};
use std::collections::BTreeSet;

use crate::{Identity, Node, digest, election, meta, set, text};

#[derive(Debug)]
pub struct ComponentUnavailable(pub String);
impl std::fmt::Display for ComponentUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "component agreement unavailable: {}", self.0)
    }
}
impl std::error::Error for ComponentUnavailable {}

fn available(condition: bool, reason: &str) -> Result<()> {
    if !condition {
        return Err(ComponentUnavailable(reason.into()).into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentPreference {
    /// A supplied comparison of actual model evidence, not a parameter-count
    /// or operating-system ranking invented by the election mechanism.
    pub rank: i64,
    pub model_evidence: Digest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentCandidate {
    pub runtime_id: EntityId,
    pub profile_digest: Digest,
    /// None means no comparative intelligence evidence is available. Equal
    /// or unknown preferences use Node identity only as a coordination tie.
    pub preference: Option<ComponentPreference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentPresence {
    pub node_id: EntityId,
    pub identity: Identity,
    pub term: u64,
    pub configuration_revision: u64,
    pub configuration_digest: Digest,
    pub revision: u64,
    pub runtime_id: Option<EntityId>,
    pub candidate: Option<ComponentCandidate>,
    pub committed: Option<ComponentCertificate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentObservation {
    pub presence: ComponentPresence,
    /// Every authenticated configured member that answered this observation,
    /// including refusals. An unusable response prevents agreement.
    pub reachable: Vec<EntityId>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentProposal {
    pub decision_id: EntityId,
    pub branch_id: EntityId,
    pub coordinator: EntityId,
    pub observations: Vec<ComponentObservation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentAcknowledgement {
    pub node_id: EntityId,
    pub decision_id: EntityId,
    pub proposal_digest: Digest,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentCertificate {
    pub proposal: Box<ComponentProposal>,
    pub acknowledgements: Vec<ComponentAcknowledgement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ComponentState {
    revision: u64,
    pending: Option<ComponentAcknowledgement>,
    committed: Option<ComponentCertificate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CandidateObservation {
    runtime_id: EntityId,
    candidate: Option<ComponentCandidate>,
    observed_at: chrono::DateTime<chrono::Utc>,
}

fn state(conn: &Connection) -> Result<ComponentState> {
    let raw: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k='component_state'", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

impl ComponentProposal {
    pub fn members(&self) -> Vec<EntityId> {
        self.observations
            .iter()
            .map(|o| o.presence.node_id)
            .collect()
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            (2..=256).contains(&self.observations.len()),
            "component requires 2..=256 members"
        );
        let members = self.members();
        ensure!(
            members.windows(2).all(|pair| pair[0] < pair[1]),
            "component members must be unique and sorted"
        );
        let first = &self.observations[0].presence;
        for observation in &self.observations {
            let presence = &observation.presence;
            ensure!(
                observation.complete && observation.reachable == members,
                "component view is incomplete, asymmetric or omits an observed member"
            );
            ensure!(
                presence.identity.agent_id == first.identity.agent_id
                    && presence.identity.origin_proof == first.identity.origin_proof
                    && presence.configuration_revision == first.configuration_revision
                    && presence.configuration_digest == first.configuration_digest,
                "component participants differ in subject or committed configuration"
            );
            // Historical certificates are not recursively carried into every
            // next proposal, which would otherwise grow the wire without bound.
            ensure!(
                presence.committed.is_none(),
                "proposal observations must omit historical certificates"
            );
            ensure!(
                presence
                    .candidate
                    .as_ref()
                    .is_none_or(|candidate| { presence.runtime_id == Some(candidate.runtime_id) }),
                "component candidate is not bound to its observed runtime"
            );
        }
        let selected = self
            .observations
            .iter()
            .filter_map(|observation| {
                observation.presence.candidate.as_ref().map(|candidate| {
                    (
                        candidate.preference.as_ref().map(|p| p.rank),
                        observation.presence.node_id,
                    )
                })
            })
            .max_by(|(a_rank, a_id), (b_rank, b_id)| {
                a_rank.cmp(b_rank).then_with(|| b_id.cmp(a_id))
            })
            .context("component has no resident candidate")?
            .1;
        ensure!(
            self.coordinator == selected,
            "component coordinator differs from the agreed preference and stable tie break"
        );
        ensure!(
            self.branch_id
                == EntityId::derived(
                    "availability-component-branch-v1",
                    self.decision_id.to_string().as_bytes()
                ),
            "component branch must identify this exact decision"
        );
        Ok(())
    }
}

impl ComponentCertificate {
    pub fn validate(&self) -> Result<()> {
        self.proposal.validate()?;
        let digest = digest(&self.proposal)?;
        let members = self.proposal.members();
        ensure!(
            self.acknowledgements.len() == members.len(),
            "component certificate requires every member acknowledgement"
        );
        for (observation, acknowledgement) in self
            .proposal
            .observations
            .iter()
            .zip(&self.acknowledgements)
        {
            ensure!(
                acknowledgement.node_id == observation.presence.node_id
                    && acknowledgement.decision_id == self.proposal.decision_id
                    && acknowledgement.proposal_digest == digest
                    && observation.presence.revision.checked_add(1)
                        == Some(acknowledgement.revision),
                "component acknowledgement does not bind its observed revision and exact proposal"
            );
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<Digest> {
        digest(self)
    }
}

impl Node {
    pub fn advertise_component_candidate(&mut self, candidate: ComponentCandidate) -> Result<()> {
        self.advertise_component_runtime(candidate.runtime_id, Some(candidate))
    }

    pub fn advertise_component_runtime(
        &mut self,
        runtime_id: EntityId,
        candidate: Option<ComponentCandidate>,
    ) -> Result<()> {
        self.observe_component_runtime(runtime_id, candidate, true)
    }

    /// Renewal cannot replace a newer owned runtime's observation. A delayed
    /// blocking task from a superseded process therefore cannot revive it.
    pub fn refresh_component_runtime(
        &mut self,
        runtime_id: EntityId,
        candidate: Option<ComponentCandidate>,
    ) -> Result<()> {
        self.observe_component_runtime(runtime_id, candidate, false)
    }

    fn observe_component_runtime(
        &mut self,
        runtime_id: EntityId,
        candidate: Option<ComponentCandidate>,
        register: bool,
    ) -> Result<()> {
        ensure!(
            candidate
                .as_ref()
                .is_none_or(|candidate| candidate.runtime_id == runtime_id),
            "candidate differs from its resident runtime"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !register {
            let original: Option<CandidateObservation> = meta(&tx, "component_candidate")?
                .map(|raw| serde_json::from_str(&raw))
                .transpose()?;
            available(
                original
                    .as_ref()
                    .is_some_and(|original| original.runtime_id == runtime_id),
                "runtime observation was superseded",
            )?;
        }
        set(
            &tx,
            "component_candidate",
            &CandidateObservation {
                runtime_id,
                candidate,
                observed_at: chrono::Utc::now(),
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Expiry is only local candidate evidence. It is NOT an authority lease
    /// and never certifies that an older process or external effect stopped.
    pub fn component_presence(&self) -> Result<ComponentPresence> {
        let tx = self.conn.unchecked_transaction()?;
        let snapshot = Self::authority_snapshot_conn(&tx)?;
        let configuration = snapshot
            .configuration
            .context("component requires committed membership")?;
        let candidate: Option<CandidateObservation> = meta(&tx, "component_candidate")?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()?;
        let candidate = candidate.filter(|candidate| {
            let age = chrono::Utc::now()
                .signed_duration_since(candidate.observed_at)
                .num_seconds();
            (0..=30).contains(&age)
        });
        let presence = ComponentPresence {
            node_id: snapshot.node_id,
            identity: snapshot.identity,
            term: snapshot.term,
            configuration_revision: configuration.revision,
            configuration_digest: configuration.digest,
            revision: snapshot.component_revision,
            runtime_id: candidate.as_ref().map(|observation| observation.runtime_id),
            candidate: candidate.and_then(|observation| observation.candidate),
            committed: snapshot.component,
        };
        tx.commit()?;
        Ok(presence)
    }

    pub(crate) fn component_state_conn(
        conn: &Connection,
    ) -> Result<(u64, Option<ComponentCertificate>)> {
        let state = state(conn)?;
        let node_id = Self::stored_node_id(conn)?;
        let identity = Self::stored_identity(conn)?;
        let term = election::state_number(conn, "term")?;
        let configuration = Self::committed_config(conn)?;
        let fence = Self::fencing_token_conn(conn)?;
        if let Some(certificate) = &state.committed {
            certificate.validate()?;
        }
        let committed = state.committed.filter(|certificate| {
            let Some(observation) = certificate
                .proposal
                .observations
                .iter()
                .find(|o| Some(o.presence.node_id) == node_id)
            else {
                return false;
            };
            term == observation.presence.term
                && identity.as_ref().is_some_and(|identity| {
                    identity.agent_id == observation.presence.identity.agent_id
                        && identity.origin_proof == observation.presence.identity.origin_proof
                        && (identity.branch_id == observation.presence.identity.branch_id
                            || (Some(certificate.proposal.coordinator) == node_id
                                && identity.branch_id == certificate.proposal.branch_id))
                })
                && configuration.as_ref().is_some_and(|config| {
                    config.revision == observation.presence.configuration_revision
                        && config.digest == observation.presence.configuration_digest
                })
                && fence.is_none()
                && certificate.acknowledgements.iter().any(|ack| {
                    Some(ack.node_id) == node_id
                        && ack.revision == state.revision
                        && state.pending.as_ref() == Some(ack)
                })
        });
        Ok((state.revision, committed))
    }

    /// Called only after the production adapter has freshly probed this
    /// member's authenticated routes; supplied view contents cannot replace
    /// that independently observed member set.
    pub fn prepare_component(
        &mut self,
        proposal: &ComponentProposal,
        observed: &ComponentObservation,
    ) -> Result<ComponentAcknowledgement> {
        proposal.validate()?;
        let proposal_digest = digest(proposal)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = state(&tx)?;
        let snapshot = Self::authority_snapshot_conn(&tx)?;
        let own = proposal
            .observations
            .iter()
            .find(|o| o.presence.node_id == snapshot.node_id)
            .context("local member omitted from component proposal")?;
        let mut observed = observed.clone();
        observed.presence.committed = None;
        if let Some(acknowledgement) = &state.pending
            && acknowledgement.decision_id == proposal.decision_id
            && acknowledgement.proposal_digest == proposal_digest
        {
            available(
                state.revision == acknowledgement.revision,
                "component retry was superseded",
            )?;
            return Ok(acknowledgement.clone());
        }
        available(
            &observed == own
                && state.revision == own.presence.revision
                && snapshot.term == own.presence.term
                && snapshot.identity == own.presence.identity
                && snapshot.fence.is_none()
                && snapshot.configuration.as_ref().is_some_and(|config| {
                    config.revision == own.presence.configuration_revision
                        && config.digest == own.presence.configuration_digest
                }),
            "component observation changed before acknowledgement",
        )?;
        let configured = snapshot
            .configuration
            .as_ref()
            .context("missing configuration")?
            .voters();
        ensure!(
            proposal
                .members()
                .iter()
                .all(|member| configured.contains(member)),
            "component contains an unconfigured voter"
        );
        available(
            !snapshot
                .configuration
                .as_ref()
                .context("missing configuration")?
                .has_quorum(&proposal.members().into_iter().collect::<BTreeSet<_>>()),
            "a reachable global quorum must use global control election",
        )?;
        state.revision = state
            .revision
            .checked_add(1)
            .context("component revision exhausted")?;
        let acknowledgement = ComponentAcknowledgement {
            node_id: snapshot.node_id,
            decision_id: proposal.decision_id,
            proposal_digest,
            revision: state.revision,
        };
        state.pending = Some(acknowledgement.clone());
        state.committed = None;
        // Existing command history retains superseded acknowledgements. Its
        // current projection is separate and cannot be restored by old replay.
        let command_id = EntityId::derived(
            "component-ack-v1",
            format!("{}:{}", proposal.decision_id, snapshot.node_id).as_bytes(),
        );
        let result = text(&serde_json::json!({
            "proposal": proposal,
            "acknowledgement": &acknowledgement,
        }))?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT seal,result FROM commands WHERE id=?1",
                [command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        ensure!(
            old.is_none(),
            "superseded component acknowledgement cannot be renewed"
        );
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2,?3)",
            params![
                command_id.to_string(),
                acknowledgement.proposal_digest.0,
                result
            ],
        )?;
        set(&tx, "component_state", &state)?;
        tx.commit()?;
        Ok(acknowledgement)
    }

    pub fn commit_component(
        &mut self,
        certificate: &ComponentCertificate,
    ) -> Result<ComponentCertificate> {
        certificate.validate()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut state = state(&tx)?;
        let snapshot = Self::authority_snapshot_conn(&tx)?;
        let acknowledgement = certificate
            .acknowledgements
            .iter()
            .find(|ack| ack.node_id == snapshot.node_id)
            .context("local member omitted from component certificate")?;
        let observation = certificate
            .proposal
            .observations
            .iter()
            .find(|o| o.presence.node_id == snapshot.node_id)
            .context("local observation missing")?;
        available(
            state.pending.as_ref() == Some(acknowledgement)
                && state.revision == acknowledgement.revision
                && snapshot.term == observation.presence.term
                && snapshot.identity.agent_id == observation.presence.identity.agent_id
                && snapshot.identity.origin_proof == observation.presence.identity.origin_proof
                && (snapshot.identity.branch_id == observation.presence.identity.branch_id
                    || (certificate.proposal.coordinator == snapshot.node_id
                        && snapshot.identity.branch_id == certificate.proposal.branch_id))
                && snapshot.fence.is_none()
                && snapshot.configuration.as_ref().is_some_and(|config| {
                    config.revision == observation.presence.configuration_revision
                        && config.digest == observation.presence.configuration_digest
                }),
            "component acknowledgement was superseded or global authority changed",
        )?;
        let command_id = EntityId::derived(
            "component-commit-v1",
            format!("{}:{}", certificate.proposal.decision_id, snapshot.node_id).as_bytes(),
        );
        let certificate_digest = certificate.digest()?;
        let certificate_bytes = text(certificate)?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT seal,result FROM commands WHERE id=?1",
                [command_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((seal, result)) = old {
            ensure!(
                seal == certificate_digest.0 && result == certificate_bytes,
                "component decision conflicts with its retained original certificate"
            );
        } else {
            tx.execute(
                "INSERT INTO commands VALUES(?1,?2,?3)",
                params![
                    command_id.to_string(),
                    certificate_digest.0,
                    certificate_bytes
                ],
            )?;
        }
        state.committed = Some(certificate.clone());
        set(&tx, "component_state", &state)?;
        tx.commit()?;
        Ok(certificate.clone())
    }

    /// A coordinator with a blocked original transition must negotiate a new
    /// branch instead of silently changing the branch named by its certificate.
    /// Retiring only this exact local promise makes later confirmation fail;
    /// original physical effects and all agreement evidence remain retained.
    pub fn relinquish_component_if_current(
        &mut self,
        certificate: &ComponentCertificate,
        reason: &str,
    ) -> Result<()> {
        ensure!(
            !reason.trim().is_empty(),
            "component retirement needs a reason"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot = Self::authority_snapshot_conn(&tx)?;
        available(
            snapshot.component.as_ref() == Some(certificate)
                && certificate.proposal.coordinator == snapshot.node_id,
            "component changed before coordinator retirement",
        )?;
        let mut state = state(&tx)?;
        state.revision = state
            .revision
            .checked_add(1)
            .context("component revision exhausted")?;
        state.pending = None;
        state.committed = None;
        let fact = serde_json::json!({
            "kind": "component.retired",
            "node_id": snapshot.node_id,
            "revision": state.revision,
            "certificate": certificate,
            "reason": reason,
        });
        let command_id = EntityId::derived(
            "component-retirement-v1",
            format!("{}:{}", certificate.proposal.decision_id, snapshot.node_id).as_bytes(),
        );
        tx.execute(
            "INSERT INTO commands VALUES(?1,?2,?3)",
            params![command_id.to_string(), digest(&fact)?.0, text(&fact)?],
        )?;
        set(&tx, "component_state", &state)?;
        tx.commit()?;
        Ok(())
    }
}
