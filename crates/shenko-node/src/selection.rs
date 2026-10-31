//! Observation and durable choice, not a router or execution authorization.
//!
//! A trusted adapter may expose these narrow operations to a subject or its
//! delegated scheduling agent. It MUST authenticate that caller separately;
//! no model-supplied field here establishes authority. The administrative Node
//! RPC is deliberately not extended. Receipts never reserve resources, fence
//! executors, or dispatch work. An execution bridge must independently enforce
//! task authority, current permissions, resource admission and effect recovery.

use super::{Identity, Node, NodeCapabilityProfile, digest, text};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{ids::EntityId, object::Digest};

const MAX_PAGE: usize = 32;
const MAX_PROFILE_BYTES: i64 = 16 * 1024;
const MAX_REASON_BYTES: usize = 4096;
const RECEIPT_SCHEMA: &str = "node.selection.v1";

/// A projection of an advertisement, not a readiness or permission claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityObservation {
    pub profile_digest: Digest,
    pub advertisement_seq: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub provides: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityKnowledge {
    Observed { observation: CapabilityObservation },
    Unknown,
    Oversized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeChoiceCandidate {
    pub node_id: EntityId,
    pub knowledge: CapabilityKnowledge,
}

/// Each page is a single SQLite read snapshot. Pagination is NOT a frozen
/// multi-page view; concurrent enrollment can change subsequent pages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeChoiceSnapshot {
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub candidates: Vec<NodeChoiceCandidate>,
    /// Exclusive node-ID cursor, never a recommendation or preference rank.
    pub next_after: Option<EntityId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSelection {
    /// Stable ID of this exact decision. A changed decision needs a new ID.
    pub decision_id: EntityId,
    pub node_id: EntityId,
    pub observed_profile_digest: Digest,
    /// The agent's explanation, retained verbatim; not an authorization claim.
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum NodeSelectionOutcome {
    Recorded,
    Unavailable { reason: SelectionUnavailable },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionUnavailable {
    NotEnrolled,
    ObservationUnknown,
    ObservationOversized,
    ObservationChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSelectionReceipt {
    pub schema: String,
    pub selection: NodeSelection,
    pub outcome: NodeSelectionOutcome,
    /// Current observation when recording, including on mismatch. A retry of
    /// the same decision returns this historical observation, not a new one.
    pub observation: Option<CapabilityObservation>,
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    pub dispatch: SelectionDispatch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionDispatch {
    NotDispatched,
}

fn knowledge(conn: &Connection, node_id: EntityId, local_id: EntityId) -> Result<CapabilityKnowledge> {
    // Bound bytes in SQL before allocating/decoding untrusted advertisements.
    // No SSH routes, filesystem paths, provenance text or administrative
    // configuration is projected to the caller.
    let row: Option<(i64, Option<String>)> = if node_id == local_id {
        conn.query_row(
            "SELECT length(CAST(v AS BLOB)), CASE WHEN length(CAST(v AS BLOB))<=?1 THEN v END FROM meta WHERE k='capability_advertisement'",
            [MAX_PROFILE_BYTES], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?
    } else {
        conn.query_row(
            "SELECT length(CAST(data AS BLOB)), CASE WHEN length(CAST(data AS BLOB))<=?1 THEN data END FROM node_capability_facts WHERE node=?2",
            params![MAX_PROFILE_BYTES, node_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?
    };
    let Some((_, data)) = row else { return Ok(CapabilityKnowledge::Unknown) };
    let Some(data) = data else { return Ok(CapabilityKnowledge::Oversized) };
    let profile: NodeCapabilityProfile = if node_id == local_id {
        let envelope: serde_json::Value = serde_json::from_str(&data)?;
        serde_json::from_value(envelope.get("profile").context("local advertisement has no profile")?.clone())?
    } else {
        serde_json::from_str(&data)?
    };
    ensure!(profile.node_id == node_id, "capability observation node mismatch");
    Ok(CapabilityKnowledge::Observed { observation: CapabilityObservation {
        profile_digest: digest(&profile)?,
        advertisement_seq: profile.advertisement_seq,
        generated_at: profile.generated_at,
        provides: profile.provides,
    } })
}

impl Node {
    /// Read one enrolled candidate without ranking or scanning every page.
    /// Like the page API this reads cached observations, not current readiness.
    pub fn node_choice_candidate(&mut self, node_id: EntityId) -> Result<Option<NodeChoiceCandidate>> {
        ensure!(!self.failed, "node requires reopen after storage failure");
        let tx = self.conn.transaction()?;
        let local_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let enrolled = node_id == local_id || tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM node_sync_peers WHERE node_id=?1)",
            [node_id.to_string()], |r| r.get::<_, bool>(0),
        )?;
        let candidate = if enrolled {
            Some(NodeChoiceCandidate { node_id, knowledge: knowledge(&tx, node_id, local_id)? })
        } else { None };
        tx.commit()?;
        Ok(candidate)
    }

    /// Recover an existing receipt without re-planning or re-validating current
    /// conditions. `None` means no such command; a different command kind or
    /// unsupported schema is an error, never interpreted as a selection.
    pub fn node_selection_receipt(&self, decision_id: EntityId) -> Result<Option<NodeSelectionReceipt>> {
        ensure!(!self.failed, "node requires reopen after storage failure");
        let result: Option<String> = self.conn.query_row(
            "SELECT result FROM commands WHERE id=?1", [decision_id.to_string()], |r| r.get(0),
        ).optional()?;
        result.map(|result| {
            let receipt: NodeSelectionReceipt = serde_json::from_str(&result)?;
            ensure!(receipt.schema == RECEIPT_SCHEMA, "unsupported selection receipt schema");
            ensure!(receipt.selection.decision_id == decision_id, "selection receipt ID mismatch");
            Ok(receipt)
        }).transpose()
    }

    /// Read cached advertisements for self and enrolled sync peers, including
    /// peers with unknown facts. No ranking, filtering by inferred suitability,
    /// refresh, network probe or fallback. Call `capability_profile` separately
    /// to publish fresh local facts. Advertisements can be arbitrarily old.
    pub fn node_choice_snapshot(&mut self, after: Option<EntityId>, limit: usize) -> Result<NodeChoiceSnapshot> {
        self.node_choice_snapshot_bound(after, limit, None)
    }

    /// Read for an already authenticated subject adapter. Identity and facts
    /// are checked in the SAME SQLite snapshot; another connection may adopt
    /// an identity concurrently. This check does not grant execution rights.
    pub fn node_choice_snapshot_for_subject(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
        expected_node: EntityId,
        expected_subject: &Identity,
    ) -> Result<NodeChoiceSnapshot> {
        self.node_choice_snapshot_bound(after, limit, Some((expected_node, expected_subject)))
    }

    fn node_choice_snapshot_bound(
        &mut self,
        after: Option<EntityId>,
        limit: usize,
        binding: Option<(EntityId, &Identity)>,
    ) -> Result<NodeChoiceSnapshot> {
        ensure!(!self.failed, "node requires reopen after storage failure");
        ensure!((1..=MAX_PAGE).contains(&limit), "page limit must be 1..=32");
        let tx = self.conn.transaction()?;
        let local_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        if let Some((expected_node, expected_subject)) = binding {
            ensure!(
                local_id == expected_node && Self::stored_identity(&tx)?.as_ref() == Some(expected_subject),
                "node_subject_binding_stale"
            );
        }
        let ids = {
            let mut statement = tx.prepare(
                "SELECT node_id FROM (SELECT node_id FROM node_sync_peers UNION SELECT ?1 AS node_id) WHERE node_id>?2 ORDER BY node_id LIMIT ?3"
            )?;
            statement.query_map(params![local_id.to_string(), after.map(|id| id.to_string()).unwrap_or_default(), (limit + 1) as i64], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let has_more = ids.len() > limit;
        let mut candidates = Vec::new();
        for id in ids.into_iter().take(limit) {
            let node_id: EntityId = id.parse()?;
            candidates.push(NodeChoiceCandidate { node_id, knowledge: knowledge(&tx, node_id, local_id)? });
        }
        let next_after = if has_more { candidates.last().map(|c| c.node_id) } else { None };
        let snapshot = NodeChoiceSnapshot { observed_at: chrono::Utc::now(), candidates, next_after };
        tx.commit()?;
        Ok(snapshot)
    }

    /// Atomically validate an explicit choice against retained observations and
    /// persist its receipt. A mismatch returns facts for agent re-decision; it
    /// NEVER selects a different node. The existing command journal supplies
    /// restart-safe idempotency without introducing a second dispatch ledger.
    pub fn record_node_selection(&mut self, selection: NodeSelection) -> Result<NodeSelectionReceipt> {
        ensure!(!self.failed, "node requires reopen after storage failure");
        ensure!(!selection.reason.trim().is_empty() && selection.reason.len() <= MAX_REASON_BYTES, "selection reason must be 1..=4096 bytes");
        let hex = selection.observed_profile_digest.0.strip_prefix("blake3:").context("invalid profile digest algorithm")?;
        ensure!(hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "invalid profile digest");
        let seal = digest(&(RECEIPT_SCHEMA, &selection))?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior: Option<(String, String)> = tx.query_row(
            "SELECT seal,result FROM commands WHERE id=?1", [selection.decision_id.to_string()], |r| Ok((r.get(0)?, r.get(1)?)),
        ).optional()?;
        if let Some((old_seal, result)) = prior {
            ensure!(old_seal == seal.0, "decision ID already sealed to a different command");
            let receipt: NodeSelectionReceipt = serde_json::from_str(&result)?;
            ensure!(receipt.schema == RECEIPT_SCHEMA, "unsupported selection receipt schema");
            tx.commit()?;
            return Ok(receipt);
        }
        let local_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let enrolled = selection.node_id == local_id || tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM node_sync_peers WHERE node_id=?1)", [selection.node_id.to_string()], |r| r.get::<_, bool>(0),
        )?;
        let (outcome, observation) = if !enrolled {
            (NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::NotEnrolled }, None)
        } else {
            match knowledge(&tx, selection.node_id, local_id)? {
                CapabilityKnowledge::Unknown => (NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationUnknown }, None),
                CapabilityKnowledge::Oversized => (NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationOversized }, None),
                CapabilityKnowledge::Observed { observation } => {
                    let outcome = if observation.profile_digest == selection.observed_profile_digest {
                        NodeSelectionOutcome::Recorded
                    } else {
                        NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationChanged }
                    };
                    (outcome, Some(observation))
                }
            }
        };
        let receipt = NodeSelectionReceipt { schema: RECEIPT_SCHEMA.into(), selection, outcome, observation, recorded_at: chrono::Utc::now(), dispatch: SelectionDispatch::NotDispatched };
        tx.execute("INSERT INTO commands(id,seal,result) VALUES(?1,?2,?3)", params![receipt.selection.decision_id.to_string(), seal.0, text(&receipt)?])?;
        tx.commit()?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Request, SyncPeer};
    use std::path::Path;

    fn init(dir: &Path) -> Node {
        let mut node = Node::open(dir).unwrap();
        node.handle(Request::Initialize { node_id: EntityId::new(), identity: None }).unwrap();
        node
    }

    fn enroll(node: &mut Node, id: EntityId) {
        node.register_sync_peer(&SyncPeer {
            node_id: id,
            ssh_target: "private-route-not-for-model".into(),
            remote_binary: "/private/binary".into(),
            remote_data_dir: "/private/data".into(),
            workspaces: vec![],
            remote_workspace_root: None,
        }).unwrap();
    }

    fn selection(profile: &NodeCapabilityProfile) -> NodeSelection {
        NodeSelection {
            decision_id: EntityId::new(), node_id: profile.node_id,
            observed_profile_digest: digest(profile).unwrap(),
            reason: "I choose this node for the task; this is not a permission grant.".into(),
        }
    }

    #[test]
    fn snapshot_is_bounded_non_ranked_and_keeps_unknown_without_routes() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = init(&dir.path().join("node"));
        let local = node.capability_profile().unwrap();
        let peer = EntityId::new();
        enroll(&mut node, peer);
        let first = node.node_choice_snapshot(None, 1).unwrap();
        let second = node.node_choice_snapshot(first.next_after, 1).unwrap();
        assert_eq!(first.candidates.len(), 1);
        assert_eq!(second.candidates.len(), 1);
        assert!(first.candidates[0].node_id.to_string() < second.candidates[0].node_id.to_string());
        assert!(second.next_after.is_none());
        let all = node.node_choice_snapshot(None, 32).unwrap();
        assert!(all.candidates.iter().any(|c| c.node_id == peer && matches!(c.knowledge, CapabilityKnowledge::Unknown)));
        assert!(all.candidates.iter().any(|c| c.node_id == local.node_id && matches!(c.knowledge, CapabilityKnowledge::Observed { .. })));
        assert!(!text(&all).unwrap().contains("private"));
        assert!(node.node_choice_snapshot(None, 33).is_err());
    }

    #[test]
    fn explicit_choice_survives_reopen_and_replay_never_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node");
        let mut node = init(&path);
        let profile = node.capability_profile().unwrap();
        let choice = selection(&profile);
        let receipt = node.record_node_selection(choice.clone()).unwrap();
        assert!(matches!(receipt.outcome, NodeSelectionOutcome::Recorded));
        assert!(matches!(receipt.dispatch, SelectionDispatch::NotDispatched));
        drop(node);
        let mut reopened = Node::open(&path).unwrap();
        assert_eq!(text(&receipt).unwrap(), text(&reopened.node_selection_receipt(choice.decision_id).unwrap().unwrap()).unwrap());
        assert_eq!(text(&receipt).unwrap(), text(&reopened.record_node_selection(choice.clone()).unwrap()).unwrap());
        let mut changed = choice;
        changed.reason = "different decision under reused ID".into();
        assert!(reopened.record_node_selection(changed).is_err());
        let tasks: i64 = reopened.conn.query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0)).unwrap();
        assert_eq!(tasks, 0);
    }

    #[test]
    fn changed_observation_returns_reason_and_never_substitutes() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = init(&dir.path().join("node"));
        let mut peer = init(&dir.path().join("peer"));
        let mut profile = peer.capability_profile().unwrap();
        enroll(&mut node, profile.node_id);
        node.record_peer_capability(&profile, "authenticated test fixture").unwrap();
        let choice = selection(&profile);
        profile.advertisement_seq = "2".into();
        profile.provides.push("new.fact".into());
        node.record_peer_capability(&profile, "authenticated test fixture").unwrap();
        let receipt = node.record_node_selection(choice.clone()).unwrap();
        assert!(matches!(receipt.outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationChanged }));
        assert_eq!(receipt.selection.node_id, choice.node_id);
        assert_eq!(receipt.observation.as_ref().unwrap().profile_digest, digest(&profile).unwrap());
        let new_choice = selection(&profile);
        assert!(matches!(node.record_node_selection(new_choice).unwrap().outcome, NodeSelectionOutcome::Recorded));
        // The original unavailable decision remains immutable even after a
        // subsequent successful choice; recovery does not silently re-plan.
        assert_eq!(text(&receipt).unwrap(), text(&node.record_node_selection(choice).unwrap()).unwrap());
    }

    #[test]
    fn absent_observation_and_unenrolled_target_are_explained_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = init(&dir.path().join("node"));
        let mut peer = init(&dir.path().join("peer"));
        let profile = peer.capability_profile().unwrap();
        let absent = node.record_node_selection(selection(&profile)).unwrap();
        assert!(matches!(absent.outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::NotEnrolled }));
        enroll(&mut node, profile.node_id);
        let unknown = node.record_node_selection(selection(&profile)).unwrap();
        assert!(matches!(unknown.outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationUnknown }));
        node.record_peer_capability(&profile, "authenticated test fixture").unwrap();
        node.conn.execute("DELETE FROM node_sync_peers WHERE node_id=?1", [profile.node_id.to_string()]).unwrap();
        let removed = node.record_node_selection(selection(&profile)).unwrap();
        assert!(matches!(removed.outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::NotEnrolled }));
    }

    #[test]
    fn oversized_advertisement_is_explicit_and_cannot_be_recorded_as_ready() {
        let dir = tempfile::tempdir().unwrap();
        let mut node = init(&dir.path().join("node"));
        let mut peer = init(&dir.path().join("peer"));
        let mut profile = peer.capability_profile().unwrap();
        profile.provides = vec!["x".repeat(MAX_PROFILE_BYTES as usize)];
        enroll(&mut node, profile.node_id);
        node.record_peer_capability(&profile, "authenticated test fixture").unwrap();
        let snapshot = node.node_choice_snapshot(None, 32).unwrap();
        assert!(snapshot.candidates.iter().any(|c| c.node_id == profile.node_id && matches!(c.knowledge, CapabilityKnowledge::Oversized)));
        assert!(matches!(node.record_node_selection(selection(&profile)).unwrap().outcome, NodeSelectionOutcome::Unavailable { reason: SelectionUnavailable::ObservationOversized }));
    }
}
