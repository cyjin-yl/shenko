//! Installation-to-node cognition fact boundary.
//!
//! A Node never opens the installation's Store: the standalone worker is a
//! one-request process over its own durable database. Only the installation
//! that owns the Store can compute what that Store actually holds, so the
//! installation publishes one bounded observation — the Store's own cognition
//! snapshot fingerprint and the ledger watermark it covered, with the scope it
//! covered — and the Node keeps it as a node-local fact. The Node assigns the
//! fact's identity (its own node ID and a strictly increasing generation) and
//! reports it on its authenticated peer route, so a peer reads a fact the
//! source's installation really produced instead of a claim about it.
//!
//! What a report is: one Store-origin observation of one scope, pointing at the
//! immutable snapshot digest of that content and at the ledger watermark the
//! observation covered. What it is not, under SPEC §51.1: cognitive content, an
//! attitude, a knowledge kind, a confidence, or the adoption of anything. It is
//! also not liveness of that installation, a probe of its model or provider, a
//! capability advertisement, an election or quorum input, or a recoverability
//! claim for the closure. Recoverability stays with the replication ledger,
//! which knows the gaps; election authority stays with the committed control
//! log. A reader that cannot observe a report keeps `None`, and absence is never
//! turned into a default. If the canonical content/attitude store later wants to
//! project this snapshot, that projection is its own; this node keeps a
//! mechanism report, not a second content ledger.
//!
//! Revision identity follows §51.1's dedup rule: the revision is the content
//! digest over its declared coverage. Re-observing identical content is the
//! same revision, not a new one, so `observed_at` is the instant this content
//! revision was first observed on this node. It says how long the content has
//! been current; it never says the installation is alive now.

use super::{Node, meta, set};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use std::path::PathBuf;

/// The `meta` key this module owns. `Status` reports the same report explicitly,
/// so a peer never has to know this key.
const COGNITION_REPORT_KEY: &str = "cognition_report";

/// A report is bounded by construction: one scope description and one
/// provenance sentence. A publication outside these bounds is refused instead
/// of being stored and re-served to every peer.
const MAX_WORKSPACE_ROOTS: usize = 64;
const MAX_PATH_LEN: usize = 4096;
const MAX_PROVENANCE_LEN: usize = 1024;

/// What one observation covered.
///
/// The roots are the paths the *publishing installation* read. They identify
/// which root an observation belongs to and are never compared across
/// installations; only the coverage shape is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionScope {
    /// The managed memory root, when the observation covered memory files.
    pub memory_root: Option<PathBuf>,
    /// Opt-in workspace roots, in the order the installation passed them.
    pub workspace_roots: Vec<PathBuf>,
    /// Whether the memory repository history was part of the observation.
    pub include_git_history: bool,
    /// Whether the caller guaranteed a preserved local subject boundary. Peer
    /// replication never sets this, so a peer fact cannot borrow it.
    pub local_boundary_preserved: bool,
}

impl CognitionScope {
    /// Whether two observations cover the same origins.
    ///
    /// Two installations keep different absolute paths for the same origins,
    /// so the paths themselves are not comparable; what is comparable — and
    /// what a fingerprint comparison needs — is which origins were covered.
    pub fn same_coverage(&self, other: &Self) -> bool {
        self.memory_root.is_some() == other.memory_root.is_some()
            && self.workspace_roots.len() == other.workspace_roots.len()
            && self.include_git_history == other.include_git_history
            && self.local_boundary_preserved == other.local_boundary_preserved
    }

    fn bounded(&self) -> Result<()> {
        ensure!(
            self.workspace_roots.len() <= MAX_WORKSPACE_ROOTS,
            "a cognition report scope may cover at most {MAX_WORKSPACE_ROOTS} workspace roots"
        );
        for root in self.memory_root.iter().chain(self.workspace_roots.iter()) {
            let rendered = root.to_string_lossy();
            ensure!(
                rendered.len() <= MAX_PATH_LEN,
                "a cognition report scope path exceeds {MAX_PATH_LEN} bytes"
            );
        }
        Ok(())
    }
}

/// What an installation observed about its own Store.
///
/// This is the input side of the boundary: the installation computes it from
/// its Store and is the only writer. The Node never derives it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionObservation {
    /// The Store's cognition snapshot fingerprint over `scope`.
    pub fingerprint: Digest,
    /// The Store's ledger sequence the snapshot was taken at.
    pub high_water: u64,
    /// What the fingerprint covers.
    pub scope: CognitionScope,
    /// When the Store was observed; never the time the Node received it.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    /// How the installation produced this observation.
    pub provenance: String,
}

/// Provenance of a cognition report as it travelled with a peer's experience.
///
/// A fingerprint never travels without it: without the scope, the generation
/// and the observation instant, a digest is not comparable to anything and
/// would silently become a claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionOrigin {
    pub scope: CognitionScope,
    /// The source node's revision of this content at the time it was read.
    /// A re-observation of unchanged content keeps it, per §51.1 dedup.
    pub generation: u64,
    /// When that revision was first observed on the source node.
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub provenance: String,
}

/// One node-local cognition snapshot report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CognitionSnapshotReport {
    /// The node whose installation produced this observation. Assigned by the
    /// Node from its own durable identity, so a fact copied out of another
    /// node's database is recognizable and refused.
    pub node_id: EntityId,
    pub fingerprint: Digest,
    pub high_water: u64,
    pub scope: CognitionScope,
    /// Strictly increasing per observed content on this node. Re-observing
    /// identical content bytes keeps this revision and its exact bytes (§51.1).
    pub generation: u64,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub provenance: String,
}

impl CognitionSnapshotReport {
    /// This report's provenance as it travels with a peer's experience.
    pub fn origin(&self) -> CognitionOrigin {
        CognitionOrigin {
            scope: self.scope.clone(),
            generation: self.generation,
            observed_at: self.observed_at,
            provenance: self.provenance.clone(),
        }
    }

    /// Whether the observation states the same content as this revision.
    ///
    /// Content is the digest and the ledger watermark it covers, over the
    /// coverage it declares. The observation instant and its provenance
    /// sentence are metadata of a revision, not content: §51.1 dedup means
    /// unchanged bytes never open a new revision.
    fn same_content(&self, observation: &CognitionObservation) -> bool {
        self.fingerprint == observation.fingerprint
            && self.high_water == observation.high_water
            && self.scope == observation.scope
    }
}

/// How a peer's cognition fact relates to this node's own published one.
///
/// The comparison is between two Store-origin snapshots. Equal fingerprints
/// mean the two installations hold the same replicated snapshot over the same
/// coverage; they say nothing about either installation being alive, reachable,
/// capable, or able to serve a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CognitionComparison {
    /// Both sides published a fact over the same coverage and the snapshots
    /// are byte-identical.
    SameSnapshot,
    /// Both sides published a fact over the same coverage and the snapshots
    /// differ.
    DifferentSnapshot,
    /// Both sides published a fact, but they cover different origins, so the
    /// fingerprints are not comparable.
    CoverageDiffers,
    /// At least one side published no fact: the relation is unknown, not
    /// equal and not different.
    Unobserved,
}

impl CognitionComparison {
    /// The relation between a peer's published fact and this node's own fact,
    /// computed from what each side actually published. The peer side is given
    /// as the fingerprint and scope it travelled with, so a caller cannot
    /// invent a fact for a side that published none.
    pub fn between(
        peer_fingerprint: Option<&Digest>,
        peer_scope: Option<&CognitionScope>,
        local: Option<&CognitionSnapshotReport>,
    ) -> Self {
        match (peer_fingerprint, peer_scope, local) {
            (Some(_), Some(scope), Some(local)) if !scope.same_coverage(&local.scope) => {
                Self::CoverageDiffers
            }
            (Some(peer), Some(_), Some(local)) if *peer == local.fingerprint => {
                Self::SameSnapshot
            }
            (Some(_), Some(_), Some(_)) => Self::DifferentSnapshot,
            _ => Self::Unobserved,
        }
    }
}

impl Node {
    /// Keep this installation's observation of its own Store.
    ///
    /// The observation must be produced by the installation that owns the
    /// Store; the Node cannot check the digest itself, so it enforces what it
    /// can: the report is bounded, it is bound to this node's identity, and a
    /// stale observation can never replace a newer one (a replayed publish
    /// from another process cannot walk the report backwards).
    ///
    /// Unchanged content is one revision (SPEC §51.1): re-observing the same
    /// digest, watermark and coverage returns the stored report byte for byte
    /// and writes nothing, so peers and comparison readers keep seeing stable
    /// bytes at a stable revision. `observed_at` therefore stays the instant
    /// this content was first observed here.
    pub fn publish_cognition_report(
        &mut self,
        observation: &CognitionObservation,
    ) -> Result<CognitionSnapshotReport> {
        observation.scope.bounded()?;
        ensure!(
            observation.provenance.len() <= MAX_PROVENANCE_LEN,
            "a cognition report provenance exceeds {MAX_PROVENANCE_LEN} bytes"
        );
        ensure!(
            !observation.provenance.trim().is_empty(),
            "a cognition report states how the installation observed it"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let node_id = Self::stored_node_id(&tx)?.context("node not initialized")?;
        let stored: Option<CognitionSnapshotReport> = meta(&tx, COGNITION_REPORT_KEY)?
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .context("stored cognition report unreadable")?;
        let report = match stored {
            Some(stored) => {
                ensure!(
                    stored.node_id == node_id,
                    "the stored cognition report names node {} but this installation is {node_id}",
                    stored.node_id
                );
                if stored.same_content(observation) {
                    // Unchanged content is the same revision, not a new one:
                    // the transaction is dropped without a write.
                    return Ok(stored);
                }
                ensure!(
                    observation.observed_at >= stored.observed_at,
                    "a cognition observation from {} cannot replace the newer stored observation from {}",
                    observation.observed_at,
                    stored.observed_at
                );
                CognitionSnapshotReport {
                    node_id,
                    fingerprint: observation.fingerprint.clone(),
                    high_water: observation.high_water,
                    scope: observation.scope.clone(),
                    generation: stored
                        .generation
                        .checked_add(1)
                        .context("cognition report revision exhausted")?,
                    observed_at: observation.observed_at,
                    provenance: observation.provenance.clone(),
                }
            }
            None => CognitionSnapshotReport {
                node_id,
                fingerprint: observation.fingerprint.clone(),
                high_water: observation.high_water,
                scope: observation.scope.clone(),
                generation: 1,
                observed_at: observation.observed_at,
                provenance: observation.provenance.clone(),
            },
        };
        set(&tx, COGNITION_REPORT_KEY, &report)?;
        tx.commit()?;
        Ok(report)
    }

    /// The report this node last kept, if an installation ever published one.
    ///
    /// `None` is the honest answer when the installation has not been observed
    /// yet. A stored report that names a different node, or that cannot be read
    /// as one, is an error: it is not silently downgraded to "unknown".
    pub fn cognition_report(&self) -> Result<Option<CognitionSnapshotReport>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT v FROM meta WHERE k=?1",
                [COGNITION_REPORT_KEY],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let report: CognitionSnapshotReport =
            serde_json::from_str(&raw).context("stored cognition report unreadable")?;
        if let Some(node_id) = Self::stored_node_id(&self.conn)? {
            ensure!(
                report.node_id == node_id,
                "the stored cognition report names node {} but this installation is {node_id}",
                report.node_id
            );
        }
        Ok(Some(report))
    }
}
