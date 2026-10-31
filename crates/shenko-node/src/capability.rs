//! Verified node capability facts and deterministic coordination candidacy.
//!
//! A `NodeCapabilityProfile` is a bounded, self-declared bundle of *facts* a
//! Node can honestly advertise on an already-authenticated channel: its real
//! node/self identity, running artifact manifest, persisted role assignment
//! (intent), latest runtime observation (what actually started), the count of
//! runtime observations that ever carried a live subject, and the capability
//! names declared by currently enabled installation units. It is not a
//! liveness claim, a model-reachability proof, or an election vote.
//!
//! Advertisements carry a strictly increasing per-node `advertisement_seq`.
//! A receiving Node stores at most the newest fact per source: an older or
//! conflicting same-sequence replay can never overwrite a newer verified fact
//! (generation safety), while every arrival is kept in an append-only
//! observation log with its provenance.
//!
//! `coordination_candidates` deterministically orders the known member set
//! (this Node plus its configured sync peers) from these verified facts. It is
//! candidacy input for a future ElectionProvider and does not itself elect,
//! fence, or measure current reachability; an island must not read it as a
//! global quorum decision.

use super::{Identity, Manifest, Node, digest, meta, number, running_manifest, set, text};
use crate::peers::SyncPeerState;
use crate::roles::{RoleAssignment, RuntimeRoleObservation};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

/// Tables owned by this module; additive node storage schema 5.
pub(crate) const CAPABILITY_SCHEMA: &str = "
CREATE TABLE node_capability_facts(node TEXT PRIMARY KEY,latest_seq TEXT NOT NULL,digest TEXT NOT NULL,data TEXT NOT NULL,provenance TEXT NOT NULL,received_at TEXT NOT NULL);
CREATE TABLE node_capability_observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,node TEXT NOT NULL,advertisement_seq TEXT NOT NULL,digest TEXT NOT NULL,outcome TEXT NOT NULL,provenance TEXT NOT NULL,received_at TEXT NOT NULL);";

/// One self-declared capability advertisement. Every field is a fact about
/// this Node's durable state or running artifact, computed at send time; the
/// receiver re-derives all ranking inputs from it and never trusts a derived
/// claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCapabilityProfile {
    pub node_id: EntityId,
    /// Subject self this Node is bound to, if already joined.
    pub identity: Option<Identity>,
    /// Running executable identity; `source_revision` may be `unbound` when the
    /// artifact was not built with SHENKO_SOURCE_REVISION.
    pub manifest: Manifest,
    /// Per-node strictly increasing advertisement generation, canonical
    /// unsigned decimal. Content-identical re-advertisements reuse the same
    /// generation; any content change advances it.
    pub advertisement_seq: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    /// Persisted role *intent* (mutable assignment), if any.
    pub role_assignment: Option<RoleAssignment>,
    /// Latest observed runtime roles, if any; not liveness.
    pub latest_runtime_observation: Option<RuntimeRoleObservation>,
    /// How many runtime observations ever reported a bound subject. Zero means
    /// this node has never evidenced a running subject or model path here.
    pub subject_observations: u64,
    /// Capability names declared by currently enabled units; declarations,
    /// not readiness or model evidence.
    pub provides: Vec<String>,
}

/// The newest verified fact this Node holds about one source.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerCapabilityFact {
    pub node_id: EntityId,
    pub latest_seq: String,
    pub profile_digest: Digest,
    pub profile: NodeCapabilityProfile,
    /// How this fact actually arrived (e.g. verified replication receipt).
    pub provenance: String,
    pub received_at: chrono::DateTime<chrono::Utc>,
}

struct CapabilityParts {
    identity: Option<Identity>,
    role_assignment: Option<RoleAssignment>,
    latest_runtime_observation: Option<RuntimeRoleObservation>,
    subject_observations: u64,
    provides: Vec<String>,
}

fn primary_evidence(profile: &NodeCapabilityProfile) -> bool {
    profile
        .latest_runtime_observation
        .as_ref()
        .is_some_and(|o| o.subject_id.is_some() && o.roles.contains(&crate::roles::NodeRole::Primary))
}

fn subject_evidence(profile: &NodeCapabilityProfile) -> bool {
    profile.subject_observations > 0
}

impl Node {
    /// Read every advertised durable field on one connection. Callers that
    /// advance the generation pass their write transaction so the captured
    /// snapshot can never postdate the generation it is published under.
    fn capability_parts(conn: &Connection) -> Result<CapabilityParts> {
        let mut statement = conn
            .prepare("SELECT data FROM node_runtime_observations ORDER BY seq")?;
        let rows = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut subject_observations = 0u64;
        for row in &rows {
            let observation: RuntimeRoleObservation = serde_json::from_str(row)?;
            if observation.subject_id.is_some() {
                subject_observations = subject_observations
                    .checked_add(1)
                    .context("subject observation count exhausted")?;
            }
        }
        Ok(CapabilityParts {
            identity: Self::stored_identity(conn)?,
            role_assignment: Self::latest_assignment(conn)?,
            latest_runtime_observation: Self::latest_observation(conn)?,
            subject_observations,
            provides: Self::enabled_provides(conn)?,
        })
    }

    /// Build this Node's current self-declared capability advertisement.
    ///
    /// The generation advances only when the advertised content changes, so a
    /// replayed identical profile keeps its generation while every real change
    /// is distinguishable from a stale replay at the receiver. All durable
    /// inputs are read inside the generation transaction so generation order
    /// cannot reverse the order of the snapshots it describes, and an
    /// unchanged generation returns the exact stored profile bytes (same
    /// `generated_at`, same serialization) so receivers and sync fingerprint
    /// comparisons see a stable advertisement.
    pub fn capability_profile(&mut self) -> Result<NodeCapabilityProfile> {
        // Process identity, not durable state: safe to compute before the
        // generation transaction starts.
        let manifest = running_manifest()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let parts = Self::capability_parts(&tx)?;
        let content = json!({
            "node_id": node_id,
            "identity": parts.identity,
            "manifest": manifest,
            "role_assignment": parts.role_assignment,
            "latest_runtime_observation": parts.latest_runtime_observation,
            "subject_observations": parts.subject_observations,
            "provides": parts.provides,
        });
        let content_digest = Digest::blake3(&serde_json::to_vec(&content)?);
        let prior: Option<Value> = meta(&tx, "capability_advertisement")?
            .map(|s| serde_json::from_str(&s))
            .transpose()?;
        let prior = prior.as_ref();
        let prior_seq = prior
            .and_then(|p| p.get("seq"))
            .and_then(Value::as_str)
            .map(number)
            .transpose()?
            .unwrap_or(0);
        let unchanged = prior
            .and_then(|p| p.get("content_digest"))
            .and_then(Value::as_str)
            == Some(content_digest.0.as_str());
        let profile = if unchanged && prior_seq > 0 {
            // Content-identical re-advertisement: return the stored profile
            // verbatim so the bytes receivers and header fingerprints compare
            // are identical at the reused generation.
            let stored = prior.and_then(|p| p.get("profile")).cloned();
            match stored {
                Some(stored) => serde_json::from_value(stored)
                    .context("stored capability profile unreadable")?,
                None => {
                    // A generation persisted before the profile itself was
                    // retained: rebuild it at the same generation and store
                    // the result so subsequent calls are byte-identical.
                    let rebuilt = NodeCapabilityProfile {
                        node_id,
                        identity: parts.identity,
                        manifest,
                        advertisement_seq: prior_seq.to_string(),
                        generated_at: chrono::Utc::now(),
                        role_assignment: parts.role_assignment,
                        latest_runtime_observation: parts.latest_runtime_observation,
                        subject_observations: parts.subject_observations,
                        provides: parts.provides,
                    };
                    set(
                        &tx,
                        "capability_advertisement",
                        &json!({"seq": prior_seq.to_string(), "content_digest": content_digest.0, "profile": rebuilt}),
                    )?;
                    rebuilt
                }
            }
        } else {
            let seq = prior_seq
                .checked_add(1)
                .context("capability advertisement generation exhausted")?;
            let profile = NodeCapabilityProfile {
                node_id,
                identity: parts.identity,
                manifest,
                advertisement_seq: seq.to_string(),
                generated_at: chrono::Utc::now(),
                role_assignment: parts.role_assignment,
                latest_runtime_observation: parts.latest_runtime_observation,
                subject_observations: parts.subject_observations,
                provides: parts.provides,
            };
            set(
                &tx,
                "capability_advertisement",
                &json!({"seq": seq.to_string(), "content_digest": content_digest.0, "profile": profile}),
            )?;
            profile
        };
        tx.commit()?;
        Ok(profile)
    }

    /// Store a capability advertisement that arrived over an already
    /// authenticated/verified channel. Generation safety: only a strictly
    /// newer `advertisement_seq` replaces the retained fact; an identical
    /// replay is a duplicate, a same-sequence different-bytes arrival is a
    /// recorded conflict, and an older generation is stale. Every arrival is
    /// appended to the observation log either way.
    pub fn record_peer_capability(
        &mut self,
        profile: &NodeCapabilityProfile,
        provenance: &str,
    ) -> Result<Value> {
        ensure!(
            !provenance.trim().is_empty(),
            "capability provenance required"
        );
        let seq = number(&profile.advertisement_seq)
            .context("capability advertisement sequence")?;
        ensure!(
            seq > 0,
            "capability advertisement sequence must be positive"
        );
        ensure!(
            i64::try_from(seq).is_ok(),
            "capability advertisement sequence exceeds SQLite INTEGER"
        );
        let data = text(profile)?;
        let profile_digest = digest(profile)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT latest_seq,data FROM node_capability_facts WHERE node=?1",
                [profile.node_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let outcome = match existing {
            None => {
                tx.execute(
                    "INSERT INTO node_capability_facts VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        profile.node_id.to_string(),
                        seq.to_string(),
                        profile_digest.0,
                        data,
                        provenance,
                        chrono::Utc::now().to_rfc3339(),
                    ],
                )?;
                "accepted"
            }
            Some((old_seq, old_data)) => {
                let old_seq = number(&old_seq)?;
                if seq > old_seq {
                    tx.execute(
                        "UPDATE node_capability_facts SET latest_seq=?2,digest=?3,data=?4,provenance=?5,received_at=?6 WHERE node=?1",
                        params![
                            profile.node_id.to_string(),
                            seq.to_string(),
                            profile_digest.0,
                            data,
                            provenance,
                            chrono::Utc::now().to_rfc3339(),
                        ],
                    )?;
                    "accepted"
                } else if seq == old_seq && old_data == data {
                    "duplicate"
                } else if seq == old_seq {
                    "conflict"
                } else {
                    "stale"
                }
            }
        };
        tx.execute(
            "INSERT INTO node_capability_observations(node,advertisement_seq,digest,outcome,provenance,received_at) VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                profile.node_id.to_string(),
                seq.to_string(),
                profile_digest.0,
                outcome,
                provenance,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "node_id": profile.node_id,
            "outcome": outcome,
            "advertisement_seq": seq.to_string(),
            "accepted_as_latest": matches!(outcome, "accepted" | "duplicate"),
        }))
    }

    /// Newest verified capability fact per known source, including a
    /// self-generated fact recorded on send.
    pub fn capability_facts(&self) -> Result<Vec<PeerCapabilityFact>> {
        let mut statement = self.conn.prepare(
            "SELECT node,latest_seq,digest,data,provenance,received_at FROM node_capability_facts ORDER BY node",
        )?;
        let rows = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(node, latest_seq, pdigest, data, provenance, received_at)| {
                Ok(PeerCapabilityFact {
                    node_id: node
                        .parse::<EntityId>()
                        .context("stored capability fact node ID")?,
                    latest_seq,
                    profile_digest: Digest(pdigest),
                    profile: serde_json::from_str(&data)?,
                    provenance,
                    received_at: chrono::DateTime::parse_from_rfc3339(&received_at)?
                        .with_timezone(&chrono::Utc),
                })
            })
            .collect()
    }

    /// One deterministic ordering of the known member set for coordination
    /// candidacy: this Node plus its configured sync peers, ranked by
    /// observed-primary evidence, then any bound-subject evidence, then
    /// verified reachability of its recovery closure (confirmed sync for
    /// peers, local durability for self), then newest advertisement
    /// generation, then a stable node_id tiebreak.
    ///
    /// This is *not* an election: no member configuration, no quorum, no
    /// fencing and no liveness probe is implied. A partitioned component must
    /// still run the §5.4.1 election protocol before any takeover; this
    /// ordering only supplies the evidence the SPEC says candidacy may
    /// consider (versioned capability facts and actual evidence, not names or
    /// static claims).
    pub fn coordination_candidates(&self) -> Result<Value> {
        let self_id = self.local_node_id()?.context("node not initialized")?;
        let mut entries: Vec<Value> = Vec::new();
        let mut keys: Vec<(bool, bool, bool, u64, EntityId)> = Vec::new();

        let self_parts = Self::capability_parts(&self.conn)?;
        let last_adv: Option<String> = self
            .conn
            .query_row("SELECT v FROM meta WHERE k='capability_advertisement'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|s| {
                serde_json::from_str::<Value>(&s)
                    .ok()
                    .and_then(|v| v.get("seq")?.as_str().map(str::to_owned))
            });
        let self_seq = last_adv
            .as_deref()
            .map(number)
            .transpose()?
            .unwrap_or(0);
        let self_primary = self_parts
            .latest_runtime_observation
            .as_ref()
            .is_some_and(|o| {
                o.subject_id.is_some() && o.roles.contains(&crate::roles::NodeRole::Primary)
            });
        let self_subject = self_parts.subject_observations > 0;
        keys.push((
            self_primary,
            self_subject,
            true, // local durable facts are verified by definition
            self_seq,
            self_id,
        ));
        entries.push(json!({
            "node_id": self_id,
            "self": true,
            "verified": "local_durable",
            "primary_evidence": self_primary,
            "subject_evidence": self_subject,
            "role_assignment": self_parts.role_assignment,
            "latest_runtime_observation": self_parts.latest_runtime_observation,
            "provides": self_parts.provides,
            "advertisement_seq": last_adv,
        }));

        let facts: std::collections::BTreeMap<String, PeerCapabilityFact> = self
            .capability_facts()?
            .into_iter()
            .map(|f| (f.node_id.to_string(), f))
            .collect();
        for SyncPeerState {
            peer,
            confirmation,
            failure,
        } in self.sync_peers()?
        {
            ensure!(
                peer.node_id != self_id,
                "sync peer must not be this same Node"
            );
            let fact = facts.get(&peer.node_id.to_string());
            let primary = fact.is_some_and(|f| primary_evidence(&f.profile));
            let subject = fact.is_some_and(|f| subject_evidence(&f.profile));
            let confirmed = confirmation.is_some();
            let seq = fact
                .map(|f| number(&f.latest_seq))
                .transpose()?
                .unwrap_or(0);
            let verified = if confirmed {
                "confirmed_sync"
            } else if fact.is_some() {
                "capability_fact"
            } else {
                "enrolled_no_facts"
            };
            keys.push((primary, subject, confirmed, seq, peer.node_id));
            entries.push(json!({
                "node_id": peer.node_id,
                "self": false,
                "verified": verified,
                "primary_evidence": primary,
                "subject_evidence": subject,
                "sync_confirmed": confirmed,
                "sync_failure": failure,
                "capability_fact": fact.map(|f| json!({
                    "latest_seq": f.latest_seq,
                    "profile_digest": f.profile_digest,
                    "provenance": f.provenance,
                    "received_at": f.received_at,
                    "role_assignment": f.profile.role_assignment,
                    "latest_runtime_observation": f.profile.latest_runtime_observation,
                    "subject_observations": f.profile.subject_observations,
                    "provides": f.profile.provides,
                    "manifest": f.profile.manifest,
                })),
            }));
        }

        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by(|a, b| keys[*b].cmp(&keys[*a]));
        let mut candidates = Vec::with_capacity(entries.len());
        for (rank, index) in order.into_iter().enumerate() {
            let mut entry = entries[index].clone();
            entry["rank"] = json!(rank + 1);
            candidates.push(entry);
        }
        Ok(json!({
            "basis": "verified_capability_facts_and_sync_confirmations",
            "is_election": false,
            "global_quorum": false,
            "member_configuration": "not_installed",
            "current_peer_liveness": "not_measured",
            "candidates": candidates,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SyncPeer;
    use crate::peers::SyncConfirmation;
    use crate::roles::{NodeRole, RoleAssignmentRequest};
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        // Unique per call; tests never delete data directories.
        std::env::temp_dir().join(format!(
            "shenko-capability-{}-{name}-{}",
            std::process::id(),
            EntityId::new()
        ))
    }

    fn init(dir: &std::path::Path, node_id: EntityId) -> Node {
        let mut node = Node::open(dir).unwrap();
        node.handle(crate::Request::Initialize {
            node_id,
            identity: None,
        })
        .unwrap();
        node
    }

    #[test]
    fn generation_safe_capability_facts_drive_candidacy() {
        let a_id = EntityId::new();
        let b_id = EntityId::new();
        let dir_a = scratch("a");
        let mut a = init(&dir_a, a_id);
        let mut b = init(&scratch("b"), b_id);

        // B declares intent, then its actual runtime evidences a bound
        // subject; both travel as facts inside the advertisement.
        b.assign_roles(RoleAssignmentRequest {
            operation_id: EntityId::new(),
            expected_revision: "0".into(),
            roles: vec![NodeRole::Primary, NodeRole::Executor],
            reason: "this node reaches a working subject".into(),
        })
        .unwrap();
        b.observe_runtime_roles(
            EntityId::new(),
            vec![NodeRole::Primary, NodeRole::Executor],
            Some(EntityId::new()),
            "subject actually ran here".into(),
        )
        .unwrap();
        let profile_b = b.capability_profile().unwrap();
        assert_eq!(profile_b.advertisement_seq, "1");
        assert_eq!(profile_b.node_id, b_id);
        // Unchanged content reuses the generation; a real change advances it.
        let same = b.capability_profile().unwrap();
        assert_eq!(same.advertisement_seq, profile_b.advertisement_seq);
        // The reused generation repeats the exact stored profile bytes so the
        // sync header fingerprint and receiver deduplication stay stable.
        assert_eq!(text(&same).unwrap(), text(&profile_b).unwrap());
        b.observe_runtime_roles(
            EntityId::new(),
            vec![NodeRole::Executor],
            None,
            "subject moved off this node".into(),
        )
        .unwrap();
        let profile_b2 = b.capability_profile().unwrap();
        assert_eq!(profile_b2.advertisement_seq, "2");

        // Stale and conflicting replays never overwrite the newer fact;
        // every arrival is still recorded in the observation log.
        assert_eq!(
            a.record_peer_capability(&profile_b2, "verified receipt")
                .unwrap()["outcome"],
            json!("accepted")
        );
        assert_eq!(
            a.record_peer_capability(&profile_b, "replayed earlier receipt")
                .unwrap()["outcome"],
            json!("stale")
        );
        let mut conflicting = profile_b2.clone();
        conflicting.subject_observations = 99;
        assert_eq!(
            a.record_peer_capability(&conflicting, "forged replay")
                .unwrap()["outcome"],
            json!("conflict")
        );
        assert_eq!(
            a.record_peer_capability(&profile_b2, "same verified receipt")
                .unwrap()["outcome"],
            json!("duplicate")
        );
        let log: i64 = a
            .conn
            .query_row("SELECT count(*) FROM node_capability_observations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(log, 4);

        // Candidacy uses the verified fact: B's latest observation revoked
        // primary, so it carries no primary evidence, but its bound-subject
        // history plus a confirmed sync still outranks this node's lack of
        // evidence.
        a.register_sync_peer(&SyncPeer {
            node_id: b_id,
            ssh_target: "b.example".into(),
            remote_binary: "shenko".into(),
            remote_data_dir: PathBuf::from("/remote"),
            workspaces: vec![],
            remote_workspace_root: None,
        })
        .unwrap();
        a.confirm_sync_peer(
            &SyncPeer {
                node_id: b_id,
                ssh_target: "b.example".into(),
                remote_binary: "shenko".into(),
                remote_data_dir: PathBuf::from("/remote"),
                workspaces: vec![],
                remote_workspace_root: None,
            },
            &SyncConfirmation {
                cognition_fingerprint: Digest::blake3(b"closure"),
                units_fingerprint: Digest::blake3(b"units"),
                confirmed_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        let status = a.coordination_candidates().unwrap();
        assert_eq!(status["is_election"], false);
        assert_eq!(status["global_quorum"], false);
        let candidates = status["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["node_id"], json!(b_id));
        assert_eq!(candidates[0]["verified"], json!("confirmed_sync"));
        assert_eq!(candidates[0]["primary_evidence"], json!(false));
        assert_eq!(candidates[0]["subject_evidence"], json!(true));
        assert_eq!(candidates[1]["node_id"], json!(a_id));
        assert_eq!(candidates[1]["primary_evidence"], json!(false));

        // Facts and generation ordering survive reopen of the original
        // directory; the test never deletes the installation it reopens.
        drop(a);
        let a = Node::open(&dir_a).unwrap();
        let facts = a.capability_facts().unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].node_id, b_id);
        assert_eq!(facts[0].latest_seq, "2");
        // The conflicting replay never replaced the verified fact.
        assert_eq!(facts[0].profile.subject_observations, 1);
        let status = a.coordination_candidates().unwrap();
        assert_eq!(status["candidates"][0]["node_id"], json!(b_id));
    }

    #[test]
    fn malformed_advertisement_and_unverified_peer_have_no_authority() {
        let a_id = EntityId::new();
        let b_id = EntityId::new();
        let ghost_id = EntityId::new();
        let mut a = init(&scratch("a2"), a_id);
        let mut b = init(&scratch("b2"), b_id);
        let mut profile = b.capability_profile().unwrap();
        profile.advertisement_seq = "0".into();
        assert!(a.record_peer_capability(&profile, "verified receipt").is_err());

        // An enrolled peer with no facts stays in the member set but ranks
        // below verified durable evidence and never becomes an election.
        a.register_sync_peer(&SyncPeer {
            node_id: ghost_id,
            ssh_target: "ghost.example".into(),
            remote_binary: "shenko".into(),
            remote_data_dir: PathBuf::from("/remote"),
            workspaces: vec![],
            remote_workspace_root: None,
        })
        .unwrap();
        let status = a.coordination_candidates().unwrap();
        let candidates = status["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["node_id"], json!(a_id));
        assert_eq!(candidates[1]["node_id"], json!(ghost_id));
        assert_eq!(candidates[1]["verified"], json!("enrolled_no_facts"));
        assert_eq!(candidates[1]["sync_confirmed"], json!(false));
    }
}
