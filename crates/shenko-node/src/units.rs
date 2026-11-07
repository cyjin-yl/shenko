//! Installation units: cross-node package transfer, per-node installation
//! facts, and real component trials.
//!
//! SPEC §45.10 shapes the unit itself (identity, kind, revision, artifact,
//! dependencies, config, applicability). The rules this module keeps:
//!
//! * Copying a package never makes a unit ready *here*. Only a locally
//!   executed trial of the exact artifact digest in this environment can.
//! * Declared OS/arch/runtime labels are advisory. A foreign, incomplete or
//!   unknown label never blocks a trial; only a real failure does.
//! * A trial attempt is durable before the child starts. A crash leaves the
//!   attempt unresolved (unknown) and it is never replayed automatically.
//! * A partial import or a partial materialization never reports ready.
//! * Local bindings stay local. This module never reads an environment
//!   value, a secret file or a provider grant into a package, and it exports
//!   no such material. The package's own declared bytes and configuration are
//!   copied exactly as they are, so a package that declares a secret carries
//!   that secret with it.
//!
//! The module is a child of the crate root, so it may use the private
//! `Node::conn` and the existing inline `objects` table for package bytes.

use crate::Node;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::{ids::EntityId, object::Digest};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Transfer schema; the node storage schema is versioned independently.
pub const UNIT_SCHEMA: u32 = 3;

/// Exact v3 node-storage fingerprint, retained for admission before migration.
pub(crate) const UNIT_TABLES_V3: &str = "
CREATE TABLE unit_versions(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_current(unit_id TEXT PRIMARY KEY,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,enabled INTEGER NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,edge_id));
CREATE TABLE unit_materializations(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,root TEXT NOT NULL,path TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_attempts(attempt_id TEXT PRIMARY KEY,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,state TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_trials(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,outcome TEXT NOT NULL,artifact_bound INTEGER NOT NULL,artifact_verified INTEGER NOT NULL,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);";

const UNIT_EDGE_TABLE: &str = "CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest,edge_id));";
const UNIT_VERSION_HISTORY_TABLE: &str = "CREATE TABLE unit_version_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL);";
const UNIT_EDGE_HISTORY_TABLE: &str = "CREATE TABLE unit_edge_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL);";

/// Current node-storage fingerprint. The legacy table retains every old row;
/// it is evidence, not an authority for ambiguous version ownership.
pub const UNIT_TABLES: &str = "
CREATE TABLE unit_versions(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_version_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_current(unit_id TEXT PRIMARY KEY,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,enabled INTEGER NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE \"unit_edges_legacy\"(unit_id TEXT NOT NULL,revision TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,edge_id));
CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest,edge_id));
CREATE TABLE unit_edge_history(seq INTEGER PRIMARY KEY AUTOINCREMENT,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_materializations(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,root TEXT NOT NULL,path TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_attempts(attempt_id TEXT PRIMARY KEY,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,state TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_trials(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,outcome TEXT NOT NULL,artifact_bound INTEGER NOT NULL,artifact_verified INTEGER NOT NULL,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);";

/// Bounded limits. Every one of them bounds memory or time; none of them is
/// a product permission decision.
pub const MAX_ARTIFACT_BYTES: usize = 1 << 20;
/// Upper bound of an artifact registered out of line. The bytes are not
/// copied into the node database, so the bound only refuses an absurd
/// declared size; the installer still verifies the real bytes.
pub const MAX_EXTERNAL_ARTIFACT_BYTES: u64 = 1 << 32;
pub const MAX_PACKAGES: usize = 64;
pub const MAX_EDGES: usize = 64;
pub const MAX_PROVIDES: usize = 64;
pub const MAX_ARGV: usize = 64;
pub const MAX_ARG_BYTES: usize = 4096;
pub const MAX_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_TEXT_BYTES: usize = 4096;
pub const DEFAULT_TRIAL_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TRIAL_TIMEOUT_MS: u64 = 600_000;

// ---------------------------------------------------------------- base64 ---

const B64_ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode artifact bytes for a transferable package.
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Decode the artifact bytes of a transferable package.
pub fn b64_decode(text: &str) -> Result<Vec<u8>> {
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;
    let mut out: Vec<u8> = Vec::with_capacity(text.len() / 4 * 3);
    for ch in text.bytes() {
        if ch == b'\n' || ch == b'\r' || ch == b' ' || ch == b'\t' {
            continue;
        }
        if ch == b'=' {
            break;
        }
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 character"),
        } as u32;
        acc = (acc << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------ unit shape ---

/// The four independently installable unit kinds of SPEC §45.10.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    Perception,
    Awareness,
    Skill,
    Code,
    Metapackage,
    Distribution,
}

impl UnitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitKind::Perception => "perception",
            UnitKind::Awareness => "awareness",
            UnitKind::Skill => "skill",
            UnitKind::Code => "code",
            UnitKind::Metapackage => "metapackage",
            UnitKind::Distribution => "distribution",
        }
    }

    /// Whether this kind is cognitive *content*.
    ///
    /// Content joins a metapackage only with a separate adoption attitude
    /// (SPEC §51.1/.3); a code plugin is installed, not adopted, and a
    /// sub-metapackage is judged by the attitudes of its own content members.
    pub fn is_content(&self) -> bool {
        matches!(
            self,
            UnitKind::Perception | UnitKind::Awareness | UnitKind::Skill
        )
    }

    /// Whether installing this kind means running a component here.
    pub fn is_executable(&self) -> bool {
        matches!(self, UnitKind::Code)
    }
}

/// Dependency strength is a separate axis from the matching mode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyStrength {
    Required,
    Recommended,
}

/// Matching mode of a dependency edge. An exact selector names a unit and
/// pins at least one of revision or artifact digest; an abstract selector
/// names a capability or topic and is only satisfied by a unit that actually
/// declares providing it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencySelector {
    Exact {
        unit_id: String,
        revision_requirement: Option<String>,
        artifact_digest: Option<Digest>,
        /// The versioned contract the selected provider must declare, for an
        /// edge that also pins a service ABI or a code contract (SPEC §45.10).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contract_requirement: Option<CapabilityContract>,
    },
    Abstract {
        capability_ref: Option<String>,
        topic_ref: Option<String>,
    },
}

/// One declared dependency edge of a unit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DependencySpec {
    pub edge_id: String,
    pub strength: DependencyStrength,
    pub selector: DependencySelector,
    pub purpose: Option<String>,
}

/// Declared platform labels. They are facts about the package, not a verdict
/// about this node: a foreign or missing label stays advisory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Applicability {
    pub os: Option<String>,
    pub arch: Option<String>,
    pub runtime: Option<String>,
    pub notes: Option<String>,
}

/// The unit identity and metadata that travel with a package.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitDescriptor {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub provides: Vec<String>,
    pub dependencies: Vec<DependencySpec>,
    pub config: Value,
    pub applicability: Option<Applicability>,
}

impl UnitDescriptor {
    /// The typed capability contracts this version declares providing.
    ///
    /// `provides` keeps its historical string shape so every retained package
    /// still reads. An entry that begins with [`CAPABILITY_CONTRACT_PREFIX`] is
    /// a versioned capability contract (`cap.<name>/<major>`, SPEC §51.8) and
    /// is parsed here; every other entry is an opaque topic or capability name
    /// that can satisfy a content dependency only. A malformed contract refuses
    /// the version instead of quietly becoming a topic.
    pub fn contracts(&self) -> Result<Vec<CapabilityContract>> {
        let mut contracts = Vec::new();
        for reference in &self.provides {
            if !CapabilityContract::is_contract_ref(reference) {
                continue;
            }
            let contract = CapabilityContract::parse(reference).with_context(|| {
                format!(
                    "UNIT_CAPABILITY_INVALID: {} revision {} declares {reference:?}",
                    self.unit_id, self.revision
                )
            })?;
            if !contracts.contains(&contract) {
                contracts.push(contract);
            }
        }
        contracts.sort();
        Ok(contracts)
    }

    /// Whether this version declares providing exactly this capability
    /// contract. The major version is part of the identity, so a provider of
    /// `cap.forgetting/2` does not satisfy a `cap.forgetting/1` requirement.
    pub fn provides_contract(&self, contract: &CapabilityContract) -> bool {
        self.provides.iter().any(|reference| {
            CapabilityContract::parse(reference)
                .map(|provided| contract.matches(&provided))
                .unwrap_or(false)
        })
    }

    /// The declared topic or capability names: the `provides` entries that are
    /// not typed capability contracts.
    pub fn provides_topics(&self) -> Vec<&str> {
        self.provides
            .iter()
            .map(String::as_str)
            .filter(|reference| !CapabilityContract::is_contract_ref(reference))
            .collect()
    }

    /// The resource declaration this version authored. Content kinds carry
    /// none, and no version ever declares a conflict with another package.
    pub fn package_declaration(&self) -> Result<PackageDeclaration> {
        declared_package_capabilities(self)
    }
}

/// One unit inside a transferable package: descriptor, actual bytes and the
/// origin node's safe trial provenance. Raw arguments, environment details,
/// paths and captured output never leave the node's local evidence log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitPackage {
    pub unit: UnitDescriptor,
    pub artifact_base64: String,
    pub evidence: Vec<TrialProvenance>,
    /// A v3 export carries each version's own selections. An absent value
    /// denotes an authored/v2 package, whose declarations must be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_dependencies: Option<Vec<ResolvedDependency>>,
}

/// A transferable closure: the requested unit plus the dependency units this
/// node could actually resolve locally.
///
/// The configuration it carries is the configuration the package declared,
/// copied exactly as authored. This module never reads an environment value,
/// a secret file or a provider grant into a package and exports no such
/// material; a package that declares a secret carries that secret with it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitExport {
    pub schema: u32,
    pub root_unit_id: String,
    /// Schema 3 exports bind the root independently of package ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_artifact_digest: Option<Digest>,
    pub origin_node: Option<String>,
    /// The enable state of the root unit on the origin node. It travels as
    /// information; the receiving node decides whether to adopt it.
    pub enabled: bool,
    pub packages: Vec<UnitPackage>,
}

/// Resolution state of one declared edge.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EdgeStatus {
    Satisfied,
    Omitted,
    Unresolved,
    /// Old rows cannot be attributed to this digest. Resolve this exact
    /// version or reimport it as an authored package to make a new decision.
    LegacyAmbiguous,
}

/// A resolved edge with the evidence for the choice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedDependency {
    pub edge_id: String,
    pub strength: DependencyStrength,
    pub selector: DependencySelector,
    pub status: EdgeStatus,
    pub selected_unit_id: Option<String>,
    pub selected_revision: Option<String>,
    pub selected_artifact_digest: Option<Digest>,
    /// The capability contract the selected provider was chosen for, when the
    /// declaration asked for one. A provider that changes contract major
    /// version stops satisfying the edge; its selection stays recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_contract: Option<CapabilityContract>,
    /// Other installed providers of the same abstract capability that coexist
    /// with the selection, when their actually used atomic capabilities do not
    /// conflict (SPEC §51.8). They are enabled facts on this node, never a
    /// promise that they run here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coexisting_providers: Vec<ProviderSelection>,
    /// The atomic capabilities the selected provider declares using.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub used_capabilities: Vec<AtomicCapability>,
    pub reason: String,
}

/// Another provider of one abstract capability, as this node has it installed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProviderSelection {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    pub enabled: bool,
}

/// Local readiness of an installed unit on this node.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessState {
    /// No unit is installed under this identity.
    NotInstalled,
    /// Installed and complete, but no local trial has proven it runs here.
    Unverified,
    /// A local trial of this exact artifact succeeded in this environment.
    Ready,
    /// A local trial of this exact artifact failed here.
    Failed,
    /// A local attempt ended without a known outcome; never replayed blindly.
    Unknown,
    /// A required dependency is unresolved or the package is incomplete.
    Unresolved,
    /// The bytes are not (fully) materialized in a package root yet.
    Unmaterialized,
}

/// Result of a real trial.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TrialOutcome {
    Succeeded,
    Failed,
    Signaled,
    TimedOut,
    Unknown,
}

impl TrialOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TrialOutcome::Succeeded => "succeeded",
            TrialOutcome::Failed => "failed",
            TrialOutcome::Signaled => "signaled",
            TrialOutcome::TimedOut => "timed_out",
            TrialOutcome::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "succeeded" => TrialOutcome::Succeeded,
            "failed" => TrialOutcome::Failed,
            "signaled" => TrialOutcome::Signaled,
            "timed_out" => TrialOutcome::TimedOut,
            "unknown" => TrialOutcome::Unknown,
            other => bail!("unknown trial outcome {other}"),
        })
    }
}

/// One program probe of the local environment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProgramProbe {
    pub program: String,
    pub resolved: Option<String>,
    pub size: Option<u64>,
    pub modified_unix: Option<u64>,
}

/// The environment a trial was bound to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrialEnvironment {
    pub os: String,
    pub arch: String,
    pub kernel: Option<String>,
    pub programs: Vec<ProgramProbe>,
    pub digest: Digest,
}

/// Append-only trial evidence of one attempt.
///
/// A trial is either an investigation of the environment or a readiness proof
/// for the installed artifact. Only an artifact-bound trial, whose artifact
/// digest was verified before and after the run, can prove readiness.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrialEvidence {
    pub attempt_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: TrialOutcome,
    pub environment_digest: Digest,
    pub environment: TrialEnvironment,
    /// The command that actually ran, after the artifact placeholder was
    /// expanded.
    pub argv: Vec<String>,
    /// The command the caller asked for, before expansion.
    pub requested_argv: Vec<String>,
    pub cwd: Option<String>,
    pub risk: String,
    pub benefit: String,
    pub consequence: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub interrupted: bool,
    /// The run referenced the materialized artifact of this unit.
    pub artifact_bound: bool,
    pub artifact_path: Option<String>,
    /// The materialized artifact matched the installed digest before the run.
    pub artifact_verified_before: bool,
    /// The materialized artifact still matched the installed digest after it.
    pub artifact_verified_after: bool,
    /// The output could not be read completely, so the result is not trusted.
    pub reader_failed: bool,
    /// The whole process group was signalled on the deadline.
    pub group_signalled: bool,
    /// Descendants outside the signalled group may still be running.
    pub descendants_unknown: bool,
    pub owner_pid: i64,
    pub owner_runtime: String,
    pub reason: String,
}

/// Transferable evidence of a command or a runner-owned generic trial.
/// Command evidence keeps its original safe projection; generic trials carry
/// the caller's explicit plan and receipt objects, not captured command data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum TrialProvenance {
    Command(CommandTrialProvenance),
    Generic(GenericTrialProvenance),
}

/// Command arguments, paths, environment probes, descriptions and captured
/// stdout/stderr remain local rather than being redacted by key-name rules.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CommandTrialProvenance {
    pub attempt_id: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub outcome: TrialOutcome,
    pub environment_digest: Digest,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub interrupted: bool,
    pub artifact_bound: bool,
    pub artifact_verified_before: bool,
    pub artifact_verified_after: bool,
    pub reader_failed: bool,
    pub group_signalled: bool,
    pub descendants_unknown: bool,
}

/// A generic trial's original plan and terminal receipts. These remain foreign
/// provenance on import and never become local readiness or adoption facts.
/// Referenced digests do not imply that their object bodies travel here.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GenericTrialProvenance {
    pub attempt_id: String,
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub kind: String,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: TrialOutcome,
    pub plan_digest: Digest,
    pub plan: Value,
    pub receipts: Vec<Value>,
    pub receipt_count: usize,
    pub interrupted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl From<&TrialEvidence> for TrialProvenance {
    fn from(evidence: &TrialEvidence) -> Self {
        Self::Command(CommandTrialProvenance {
            attempt_id: evidence.attempt_id.clone(),
            started_at: evidence.started_at.clone(),
            finished_at: evidence.finished_at.clone(),
            outcome: evidence.outcome,
            environment_digest: evidence.environment_digest.clone(),
            exit_code: evidence.exit_code,
            signal: evidence.signal,
            stdout_truncated: evidence.stdout_truncated,
            stderr_truncated: evidence.stderr_truncated,
            interrupted: evidence.interrupted,
            artifact_bound: evidence.artifact_bound,
            artifact_verified_before: evidence.artifact_verified_before,
            artifact_verified_after: evidence.artifact_verified_after,
            reader_failed: evidence.reader_failed,
            group_signalled: evidence.group_signalled,
            descendants_unknown: evidence.descendants_unknown,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialPlan {
    kind: String,
    plan: Value,
    plan_digest: Digest,
    started_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialCompletion {
    kind: String,
    plan_digest: Digest,
    receipts: Vec<Value>,
    receipt_count: usize,
    finished_at: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GenericTrialInterruption {
    kind: String,
    reason: String,
}

struct StoredTrialProvenance {
    attempt_id: String,
    unit_id: String,
    revision: String,
    artifact_digest: Digest,
    environment_digest: Digest,
    started_at: String,
    finished_at: Option<String>,
    outcome: String,
    data: String,
    attempt_data: Option<String>,
    attempt_state: Option<String>,
    attempt_started_at: Option<String>,
    attempt_matches: Option<bool>,
}

impl StoredTrialProvenance {
    fn generic(self) -> Result<GenericTrialProvenance> {
        ensure!(
            self.attempt_matches == Some(true),
            "generic trial does not match its original attempt"
        );
        let plan: GenericTrialPlan = serde_json::from_str(
            self.attempt_data.as_deref().context("generic trial lost its original plan")?,
        )?;
        let mut plan_hasher = blake3::Hasher::new();
        serde_json::to_writer(&mut plan_hasher, &plan.plan)?;
        ensure!(
            plan.plan.is_object()
                && self.attempt_started_at.as_deref() == Some(plan.started_at.as_str())
                && self.environment_digest == plan.plan_digest
                && plan.plan_digest.canonical_blake3_hex().is_some_and(|expected|
                    plan_hasher.finalize().to_hex().as_str() == expected),
            "generic trial plan does not match its original binding"
        );
        let finished_at = self.finished_at.context("generic trial has no terminal time")?;
        let outcome = TrialOutcome::parse(&self.outcome)?;
        let (receipts, receipt_count, interrupted, reason) = match self.attempt_state.as_deref() {
            Some("finished") => {
                let completed: GenericTrialCompletion = serde_json::from_str(&self.data)?;
                ensure!(
                    completed.kind == plan.kind
                        && completed.plan_digest == plan.plan_digest
                        && completed.finished_at == finished_at
                        && self.started_at == plan.started_at
                        && completed.receipt_count == completed.receipts.len()
                        && completed.receipts.iter().all(|receipt| {
                            receipt.is_object()
                                && receipt.get("digest").and_then(Value::as_str).is_some()
                        }),
                    "generic trial completion does not match its original binding"
                );
                (completed.receipts, completed.receipt_count, false, None)
            }
            Some("unknown") => {
                let interrupted: GenericTrialInterruption = serde_json::from_str(&self.data)?;
                ensure!(
                    interrupted.kind == "unknown"
                        && outcome == TrialOutcome::Unknown
                        // Earlier retirement rows used their retirement time
                        // as started_at; the attempt retains the actual start.
                        && (self.started_at == plan.started_at || self.started_at == finished_at),
                    "interrupted generic trial does not match its original binding"
                );
                (Vec::new(), 0, true, Some(interrupted.reason))
            }
            _ => bail!("generic trial attempt is not terminal"),
        };
        Ok(GenericTrialProvenance {
            attempt_id: self.attempt_id,
            unit_id: self.unit_id,
            revision: self.revision,
            artifact_digest: self.artifact_digest,
            kind: plan.kind,
            started_at: plan.started_at,
            finished_at,
            outcome,
            plan_digest: plan.plan_digest,
            plan: plan.plan,
            receipts,
            receipt_count,
            interrupted,
            reason,
        })
    }
}

// ---------------------------------------------------------------- request ---

/// Request surface of the installation-unit handler.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum UnitRequest {
    /// Verify and store a package closure, then materialize its bytes.
    Import {
        export: UnitExport,
        package_root: String,
        enable: Option<bool>,
    },
    /// Retry materialization of an installed unit into a package root.
    Materialize {
        unit_id: String,
        package_root: String,
    },
    /// Make a new resolution decision for one retained version without
    /// changing which version is current or rerunning any component.
    ResolveVersion {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
    },
    /// Local facts of one installed unit.
    Status { unit_id: String },
    /// Local facts of every installed unit.
    List {},
    /// Export a unit and the dependency closure available on this node.
    Export { unit_id: String },
    /// Enable or disable a unit on this node.
    SetEnabled { unit_id: String, enabled: bool },
    /// Run a bounded, caller-selected command as the real trial of a unit.
    Trial {
        unit_id: String,
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
        risk: String,
        benefit: String,
        consequence: String,
    },
    /// Append-only trial evidence, optionally of one unit.
    Evidence {
        unit_id: Option<String>,
        after: String,
        limit: u32,
    },
    /// Register a unit version whose artifact bytes live outside this
    /// installation's inline object store.
    ///
    /// The node records the declared identity — digest, size and the
    /// installation-local locator — and never copies the bytes. The
    /// installer verifies the bytes before registering them and the
    /// executor re-verifies them at run time, so an artifact that does not
    /// match its descriptor still never executes.
    ImportExternal {
        unit: UnitDescriptor,
        artifact: ExternalArtifact,
        package_root: String,
        enable: Option<bool>,
    },
    /// Read the exact content of one installed unit version.
    ///
    /// The revision is mandatory: a unit keeps several retained versions and
    /// the caller has to name the one it wants.
    Read { unit_id: String, revision: String },
    /// Record how one unit version was judged, and on which evidence.
    ///
    /// The decision is a fact about this installation, never a conclusion
    /// drawn from the unit's text. The node stores the decision, the method
    /// and the evidence the caller actually observed.
    RecordAdoption {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        /// The evidence the judgement rests on. The node verifies every item
        /// itself: a trial attempt must exist in this node's own trial store
        /// for this exact unit version, and a stored observation must exist
        /// in this node's own object store and be bound to this version.
        /// An observation is never empirical acceptance.
        evidence: Vec<AdoptionEvidence>,
        notes: Option<String>,
        /// Enable the unit in the same transaction. A rejected or retired
        /// decision never enables it. Enabling is an installation fact and
        /// never requires evidence; empirical acceptance is a separate fact
        /// that the recorded evidence decides.
        enable: Option<bool>,
    },
    /// Open a durable trial attempt for one unit version.
    ///
    /// The attempt exists before anything runs, so a crash leaves an
    /// unresolved attempt instead of a missing fact. The plan records what
    /// the trial is bound to: the exact artifact, the scenarios, the
    /// candidate and control configuration and the model binding.
    BeginTrial {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        /// What kind of trial this is, e.g. `sandbox_comparison`.
        kind: String,
        /// The bound facts of the trial. The node digests it and keeps the
        /// digest with the attempt.
        plan: Value,
    },
    /// Close a trial attempt with the receipts the run produced.
    ///
    /// Every receipt is an independent terminal fact of one condition (one
    /// committed decision, one judge verdict). The node stores them with
    /// the attempt; it never derives them from a summary.
    FinishTrial {
        attempt_id: String,
        outcome: TrialOutcome,
        receipts: Vec<Value>,
    },
    /// Register an observation about one unit version.
    ///
    /// An observation is a recorded fact (a measurement, a report, a note).
    /// It is stored with its bytes so the node can re-verify them, and it
    /// is bound to this exact unit version. An observation never becomes
    /// empirical acceptance by itself.
    RegisterObservation {
        unit_id: String,
        revision: String,
        artifact_digest: Digest,
        kind: String,
        bytes: Vec<u8>,
    },
    /// The observations registered for one unit version.
    Observations { unit_id: String },
    /// The retained versions of one unit and the supersession relations
    /// between them.
    Lineage { unit_id: String },
    /// Resolve the transitive required closure of these roots against the
    /// versions retained here and record it as a locked generation.
    ///
    /// Nothing becomes current: preparation is a candidate. A closure that
    /// cannot be satisfied from retained, resolved edges is refused with every
    /// problem listed, so an operator never receives a half-resolved
    /// environment.
    PrepareEnvironment { roots: Vec<EnvironmentRoot> },
    /// Atomically select a prepared generation as this installation's current
    /// environment. `expected_revision` is the selector revision the caller
    /// read; the write is a compare-and-swap, so a concurrent activation
    /// fails loudly instead of being overwritten.
    ActivateEnvironment { generation: u64, expected_revision: u64 },
    /// Select the previous generation of the current environment. The same
    /// compare-and-swap as activation: the caller names the revision it read.
    RollbackEnvironment { expected_revision: u64 },
    /// The current environment selector, the recorded generations and the
    /// append-only activation log. Pure query: an installation that never
    /// activated a generation reports `pinned: false`.
    Environment { limit: u32 },
    /// Walk a metapackage's member list, resolve every member against
    /// installed/retained units, check adoption via the content/attitude
    /// ledger, and return the membership standing of each. Pure read.
    ///
    /// The metapackage must be installed on this node; membership never
    /// crosses nodes. A content member that lacks an authenticated adoption
    /// attitude is reported as unadopted, not omitted.
    QueryMetapackage { unit_id: String },
    /// Walk a distribution's preinstalled macros, target-access expectations,
    /// and capability-provider defaults. Pure read: declarations are facts
    /// about the distribution version, not grants of local readiness.
    QueryDistribution { unit_id: String },
    /// Resolve one named capability-contract slot across the installed
    /// providers on this node. Returns every admitted provider, the
    /// coexistence decision and the selected index (if any). Pure read.
    QuerySlot {
        macro_id: String,
        contract: String,
    },
}

/// How one unit version was judged on this installation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionDecision {
    /// Kept and usable.
    Adopted,
    /// Not kept; the reason and the evidence stay attached to the version.
    Rejected,
    /// Kept as history but no longer offered.
    Retired,
}

impl AdoptionDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdoptionDecision::Adopted => "adopted",
            AdoptionDecision::Rejected => "rejected",
            AdoptionDecision::Retired => "retired",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "adopted" => AdoptionDecision::Adopted,
            "rejected" => AdoptionDecision::Rejected,
            "retired" => AdoptionDecision::Retired,
            other => bail!("unknown adoption decision {other}"),
        })
    }
}

/// How the judgement about one unit version was reached.
///
/// The method says which kind of work produced the judgement. It never
/// replaces the evidence: a method the node cannot back with a durable
/// attempt of its own stays a claim.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptionMethod {
    /// A finite comparison against a control excluding every version of the
    /// candidate unit id, in a real sandbox with the caller's scenario and rubric.
    SandboxComparison,
    /// A local execution trial of the installed artifact in this node's own
    /// environment.
    LocalTrial,
    /// A statement by the owner or an operator. It records what was said; it
    /// is not an empirical measurement and never claims one.
    OperatorStatement,
}

impl AdoptionMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            AdoptionMethod::SandboxComparison => "sandbox_comparison",
            AdoptionMethod::LocalTrial => "local_trial",
            AdoptionMethod::OperatorStatement => "operator_statement",
        }
    }

    /// Whether this method can ever carry empirical acceptance.
    ///
    /// Only a method that runs something can. A statement records intent;
    /// it is not a measurement.
    pub fn is_empirical(&self) -> bool {
        match self {
            AdoptionMethod::SandboxComparison | AdoptionMethod::LocalTrial => true,
            AdoptionMethod::OperatorStatement => false,
        }
    }

    /// Parse the serialized method name; an unknown method is rejected so a
    /// misspelt method never becomes an unnamed judgement.
    pub fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "sandbox_comparison" => AdoptionMethod::SandboxComparison,
            "local_trial" => AdoptionMethod::LocalTrial,
            "operator_statement" => AdoptionMethod::OperatorStatement,
            other => bail!("unknown adoption method {other}"),
        })
    }
}

/// One piece of evidence for a recorded judgement.
///
/// The node verifies each item against its own durable state. A trial
/// attempt is a runner-owned record: the node created it before the run and
/// closed it with the run's receipts. A stored observation is bytes this
/// node holds and can re-hash — proof that the bytes exist, never proof
/// that a measurement produced them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdoptionEvidence {
    /// A trial attempt of this node, bound to this unit version.
    TrialAttempt { attempt_id: String },
    /// Bytes this node stores for this unit version.
    StoredObservation { digest: Digest },
}

/// An artifact whose bytes live outside the node's inline object store.
///
/// `store` names the object store that holds the bytes (`cas` for the
/// Shenko content-addressed store) and `locator` is the installation-local
/// address of the object. The digest stays the portable identity: a peer
/// receives the digest and re-registers the unit against its own bytes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExternalArtifact {
    pub digest: Digest,
    pub bytes: u64,
    pub store: String,
    pub locator: String,
}

/// Where the bytes of one artifact actually live.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum UnitArtifactSource {
    /// The bytes are in this installation's inline object store.
    Inline,
    /// The bytes live outside the inline store and are addressed by the
    /// installation-local locator.
    External {
        store: String,
        locator: String,
        bytes: u64,
    },
}

// --------------------------------------------------------------- records ---

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredVersion {
    unit: UnitDescriptor,
    /// Evidence produced on the origin node. It is retained as provenance and
    /// never counts as local readiness. Values also preserve schema-1 records
    /// whose raw captures remain node-local and are never projected on export.
    remote_evidence: Vec<Value>,
    imported_at: String,
    origin_node: Option<String>,
}

#[derive(Clone, Debug)]
struct Materialization {
    root: String,
    path: String,
    state: String,
    detail: String,
}

#[derive(Clone, Debug)]
struct InstalledUnit {
    unit_id: String,
    revision: String,
    artifact_digest: Digest,
    enabled: bool,
    descriptor: UnitDescriptor,
    edges: Vec<ResolvedDependency>,
    materialization: Option<Materialization>,
}

/// One version another version declares it replaces.
///
/// The relation is declared by the replacing version, so it travels with
/// the package and keeps its reason. It is never guessed from wall-clock
/// order or from the number of copies a node holds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SupersededVersion {
    pub revision: String,
    pub artifact_digest: Digest,
    pub reason: Option<String>,
}

/// The public, durable facts of one installed unit version.
///
/// This is the single installation authority for perception, awareness,
/// Skill and code units: version, enable state, dependencies, supersession
/// and the authored configuration. There is no second registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstalledUnitFact {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    /// Whether this version is the one this node currently offers.
    pub current: bool,
    /// Whether this version is enabled for use.
    pub enabled: bool,
    pub imported_at: String,
    pub origin_node: Option<String>,
    pub descriptor: UnitDescriptor,
    pub edges: Vec<ResolvedDependency>,
    /// Versions this version declares it replaces.
    pub supersedes: Vec<SupersededVersion>,
    /// Retained versions that declare they replace this version.
    pub superseded_by: Vec<SupersededVersion>,
    /// Where the artifact bytes live.
    pub artifact_source: UnitArtifactSource,
    /// The materialization record, when the bytes were copied into a
    /// package root on this node.
    pub materialization: Option<MaterializationFact>,
}

/// The materialization record of a unit version.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MaterializationFact {
    pub root: String,
    pub path: String,
    pub state: String,
    pub detail: String,
}

/// The durable facts of one trial attempt.
///
/// An attempt is opened before the trial runs and closed with the receipts
/// the run produced. Its plan digest binds the attempt to the exact unit
/// version, the scenarios and rubrics, the candidate and control
/// configuration and the model binding the trial used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrialAttemptFacts {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    /// `trial_open` while the trial runs, `finished` when it closed with
    /// receipts, `unknown` when the owning process disappeared.
    pub state: String,
    pub started_at: String,
    pub kind: Option<String>,
    pub plan_digest: Option<String>,
}

/// The exact content of one installed unit version.
///
/// `bytes` are the artifact bytes whose digest is `artifact_digest`; the
/// caller verifies the digest when it matters.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitContent {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub enabled: bool,
    pub current: bool,
    pub bytes: Vec<u8>,
}

/// Public read-only projection of an installed unit that declares itself a
/// named tool (`config.tool` manifest). Carries only durable unit facts; the
/// artifact bytes are fetched separately through `Node::unit_object_bytes`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUnitFact {
    pub unit_id: String,
    pub kind: UnitKind,
    pub revision: String,
    pub artifact_digest: Digest,
    pub enabled: bool,
    /// True when the installation's unit inventory currently names this
    /// version for its unit id (`unit_current`). It is a fact about the
    /// inventory, not about any environment generation.
    pub current: bool,
    /// True when the version this installation resolves for name-only use is
    /// this one: the active environment generation's pin when a generation is
    /// active, otherwise the `current` row. A version retained outside the
    /// active generation stays resolvable by exact name@version but is not
    /// selected.
    pub selected: bool,
    /// The active environment generation, when this version is its pin.
    pub environment: Option<EnvironmentPin>,
    /// The unit-authored `config.tool` object: `{name, abi, entrypoint,
    /// source, imports}` — recorded claims whose executable part
    /// (`abi`/`imports`) was verified at admission.
    pub tool: Value,
    /// Root this node's materialization was written to (e.g. `<data>/tools`).
    pub materialization_root: Option<String>,
    /// Exact materialized artifact path, when materialization completed.
    pub materialization_path: Option<String>,
}

// ------------------------------------------------------- environment ------

/// Storage schema of the environment-generation tables. They are created on
/// first use inside the writing transaction (the same pattern the ledger's
/// activation tables use), so a node database written by an older build opens
/// unchanged and a read-only inspection never creates them.
pub const UNIT_ENVIRONMENT_SCHEMA: u32 = 1;

/// Version of the closure-digest formula a newly prepared generation is
/// recorded with. Version 1 hashed only the pinned unit identities, so two
/// closures that differed only in their locked selections or declarations
/// hashed equal. Version 2 hashes both, which is what lets an environment keep
/// the exact dependency meaning it published.
pub const ENVIRONMENT_CLOSURE_VERSION: u32 = 2;

/// Serde default for a generation document written before the closure-digest
/// version was recorded: those documents are version 1.
fn environment_closure_version_v1() -> u32 {
    1
}

/// Bounds on one environment. Both bound memory and the closure walk; neither
/// is a product permission decision.
pub const MAX_ENVIRONMENT_ROOTS: usize = 64;
pub const MAX_ENVIRONMENT_MEMBERS: usize = 256;

/// Meta-document keys of the environment state.
///
/// The environment lives in the node's existing `meta` key/value documents,
/// the same durable place the node already keeps its identity, retired
/// identities and branch facts. Storing it there keeps one transactional
/// authority and needs no storage-schema change, so an installation written by
/// an older build keeps opening unchanged and no read path creates storage.
/// Generations and activations are append-only keys whose zero-padded numeric
/// suffix orders them exactly as `u64` ordering does.
const ENVIRONMENT_SELECTOR_KEY: &str = "unit_environment.selector";
const ENVIRONMENT_GENERATION_GLOB: &str = "unit_environment.generation.*";
const ENVIRONMENT_GENERATION_PREFIX: &str = "unit_environment.generation.";
const ENVIRONMENT_ACTIVATION_GLOB: &str = "unit_environment.activation.*";
const ENVIRONMENT_ACTIVATION_PREFIX: &str = "unit_environment.activation.";

fn environment_generation_key(generation: u64) -> String {
    format!("{ENVIRONMENT_GENERATION_PREFIX}{generation:020}")
}

fn environment_activation_key(revision: u64) -> String {
    format!("{ENVIRONMENT_ACTIVATION_PREFIX}{revision:020}")
}

// ------------------------------------------------- package declarations ---

/// One guest-visible filesystem requirement of a package — SPEC §9
/// "权限需求": a guest path the executor must preopen, and whether the
/// package writes under it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct FileRequirement {
    /// Absolute guest path, for example `/home` or `/home/work`.
    pub guest_path: String,
    /// True when the package writes under the path, false for read-only use.
    pub write: bool,
}

/// One socket operation a package needs.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkOperation {
    Connect {},
    Listen {},
    Accept {},
}

impl NetworkOperation {
    pub fn as_str(&self) -> &'static str {
        match self {
            NetworkOperation::Connect {} => "connect",
            NetworkOperation::Listen {} => "listen",
            NetworkOperation::Accept {} => "accept",
        }
    }
}

/// One network target a package declares it needs, in the same typed shape the
/// network transport admits. A requirement is matched against an admitted
/// target; the transport never invents one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct NetworkTarget {
    pub host: String,
    pub port: u16,
    /// True when the guest asks the host to resolve the name; false when it
    /// connects to the literal host.
    #[serde(default)]
    pub resolve: bool,
    /// The operations the package needs on this target.
    pub operations: Vec<NetworkOperation>,
}

/// A host facility a package can require beyond files, arguments, environment
/// variables and the three standard streams.
///
/// The variants are zero-field structs rather than unit variants so that
/// `deny_unknown_fields` rejects an unexpected key on every one of them
/// (serde routes internally tagged unit variants through an ignoring visitor).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "facility", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequirement {
    /// Wall-clock and monotonic clock calls.
    Clock {},
    /// Entropy (`random_get`).
    Random {},
    /// Socket access to these exact targets. A package that declares this and
    /// receives no admitted target set is unsupported, never run under an
    /// invented empty admission.
    Network { targets: Vec<NetworkTarget> },
    /// One named host import module other than `wasi_snapshot_preview1`.
    HostImport { module: String },
}

/// The capability requirements one package version declares. They are
/// requirements, never grants: a declaration is admitted against the resources
/// the runtime actually hands the guest, and a declaration is never permission
/// to reach anything outside that grant.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageCapabilities {
    #[serde(default)]
    pub filesystem: Vec<FileRequirement>,
    /// Environment variable names that must be supplied per invocation.
    #[serde(default)]
    pub environment: Vec<String>,
    #[serde(default)]
    pub host: Vec<HostRequirement>,
    /// The atomic capabilities this code version actually exercises at run
    /// time (hook points, event subscriptions, singleton services).
    ///
    /// They are what coexistence is judged on: providers may be installed and
    /// enabled together unless both need the same atom exclusively (SPEC
    /// §51.8). There is deliberately no package-level `conflicts` field.
    #[serde(default)]
    pub atoms: Vec<AtomicCapability>,
}

/// Whether one package version records its capability requirements.
///
/// A package written before declarations existed carries none. That absence is
/// not "needs nothing" and never a blanket grant: the runtime keeps the
/// profile's own grant and reports the declaration as unrecorded, so an old
/// artifact neither loses access it had nor gains any.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageDeclaration {
    /// `None` when the unit version was written before declarations existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<PackageCapabilities>,
}

impl PackageDeclaration {
    /// A version that records no declaration.
    pub fn unrecorded() -> Self {
        Self::default()
    }

    pub fn declared(capabilities: PackageCapabilities) -> Self {
        Self {
            declared: Some(capabilities),
        }
    }

    pub fn is_unrecorded(&self) -> bool {
        self.declared.is_none()
    }

    /// `"declared"` or `"unrecorded"`, for receipts and projections.
    pub fn state(&self) -> &'static str {
        if self.declared.is_some() {
            "declared"
        } else {
            "unrecorded"
        }
    }

    pub fn capabilities(&self) -> Option<&PackageCapabilities> {
        self.declared.as_ref()
    }

    /// The declaration in canonical order, so two equal declarations are one
    /// document for digests and divergence reports.
    pub fn normalized(&self) -> Self {
        match &self.declared {
            Some(capabilities) => Self::declared(capabilities.normalized()),
            None => Self::unrecorded(),
        }
    }
}

/// Bounds on one declaration. They bound memory and the admission walk; none
/// of them is a product permission decision.
pub const MAX_DECLARED_PATHS: usize = 32;
pub const MAX_DECLARED_ENVIRONMENT: usize = 32;
pub const MAX_DECLARED_HOST: usize = 16;
/// Bound on the targets one network requirement may name.
pub const MAX_DECLARED_NETWORK_TARGETS: usize = 16;
/// Guest path and environment-name length bound. Long enough for a real
/// managed-home layout, small enough to keep a generation document bounded.
pub const MAX_CAPABILITY_NAME_BYTES: usize = 256;

impl PackageCapabilities {
    /// Typed admission of the declaration itself: bounded counts, absolute
    /// normalized guest paths, environment variable names and host facilities.
    /// Nothing here consults a model, a human or a keyword list.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.filesystem.len() <= MAX_DECLARED_PATHS,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_PATHS} declared filesystem paths"
        );
        ensure!(
            self.environment.len() <= MAX_DECLARED_ENVIRONMENT,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_ENVIRONMENT} declared environment variables"
        );
        ensure!(
            self.host.len() <= MAX_DECLARED_HOST,
            "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_HOST} declared host facilities"
        );
        for requirement in &self.filesystem {
            validate_guest_path(&requirement.guest_path)?;
        }
        for name in &self.environment {
            ensure!(
                !name.is_empty()
                    && name.len() <= MAX_CAPABILITY_NAME_BYTES
                    && name
                        .chars()
                        .enumerate()
                        .all(|(index, c)| if index == 0 {
                            c.is_ascii_alphabetic() || c == '_'
                        } else {
                            c.is_ascii_alphanumeric() || c == '_'
                        }),
                "TOOL_CAPABILITY_INVALID: environment name {name:?} must match \
                 [A-Za-z_][A-Za-z0-9_]*"
            );
        }
        for requirement in &self.host {
            match requirement {
                HostRequirement::HostImport { module } => {
                    ensure!(
                        !module.is_empty()
                            && module.len() <= MAX_CAPABILITY_NAME_BYTES
                            && module != "wasi_snapshot_preview1",
                        "TOOL_CAPABILITY_INVALID: host import module {module:?} is not a \
                         declared requirement beyond wasi_snapshot_preview1"
                    );
                }
                HostRequirement::Network { targets } => {
                    ensure!(
                        !targets.is_empty(),
                        "TOOL_CAPABILITY_INVALID: a network requirement must name the \
                         targets it needs; there is no default target set"
                    );
                    ensure!(
                        targets.len() <= MAX_DECLARED_NETWORK_TARGETS,
                        "TOOL_CAPABILITY_INVALID: at most {MAX_DECLARED_NETWORK_TARGETS} \
                         declared network targets"
                    );
                    for target in targets {
                        ensure!(
                            !target.host.is_empty()
                                && target.host.len() <= 253
                                && !target
                                    .host
                                    .chars()
                                    .any(|c| c.is_whitespace() || c.is_control()),
                            "TOOL_CAPABILITY_INVALID: network target host {:?} is not a \
                             host name or address",
                            target.host
                        );
                        ensure!(
                            !target.operations.is_empty(),
                            "TOOL_CAPABILITY_INVALID: network target {}:{} declares no \
                             operation",
                            target.host,
                            target.port
                        );
                    }
                }
                HostRequirement::Clock {} | HostRequirement::Random {} => {}
            }
        }
        ensure!(
            self.atoms.len() <= MAX_ATOMIC_CAPABILITIES,
            "TOOL_CAPABILITY_INVALID: at most {MAX_ATOMIC_CAPABILITIES} declared atomic capabilities"
        );
        let mut atoms: Vec<&str> = Vec::new();
        for atom in &self.atoms {
            validate_atom_id(&atom.atom_id)?;
            ensure!(
                !atoms.contains(&atom.atom_id.as_str()),
                "TOOL_CAPABILITY_INVALID: atomic capability {} is declared twice",
                atom.atom_id
            );
            atoms.push(&atom.atom_id);
        }
        Ok(())
    }

    /// The declaration in one stable order, so two equal declarations produce
    /// equal documents and equal closure digests.
    pub fn normalized(&self) -> Self {
        let mut normalized = self.clone();
        normalized.filesystem.sort();
        normalized.filesystem.dedup();
        normalized.environment.sort();
        normalized.environment.dedup();
        for requirement in &mut normalized.host {
            if let HostRequirement::Network { targets } = requirement {
                for target in targets.iter_mut() {
                    target.operations.sort();
                    target.operations.dedup();
                }
                targets.sort();
                targets.dedup();
            }
        }
        normalized.host.sort();
        normalized.host.dedup();
        normalized.atoms.sort();
        normalized.atoms.dedup();
        normalized
    }
}

/// A guest path a package may name: absolute, normalized, bounded.
fn validate_guest_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && path.len() <= MAX_CAPABILITY_NAME_BYTES,
        "TOOL_CAPABILITY_INVALID: guest path {path:?} must be absolute and at most \
         {MAX_CAPABILITY_NAME_BYTES} bytes"
    );
    ensure!(
        !path.contains('\0'),
        "TOOL_CAPABILITY_INVALID: guest path contains NUL"
    );
    let body = path.trim_start_matches('/');
    if !body.is_empty() {
        ensure!(
            !body.ends_with('/'),
            "TOOL_CAPABILITY_INVALID: guest path {path:?} must not end with '/'"
        );
        for component in body.split('/') {
            ensure!(
                !component.is_empty() && component != "." && component != "..",
                "TOOL_CAPABILITY_INVALID: guest path {path:?} is not normalized"
            );
        }
    }
    Ok(())
}

// ------------------------------------ capability contracts (SPEC §51.8) ---

/// Reserved prefix of a capability contract identifier.
///
/// A capability contract and a unit id are two identifier spaces: a unit id may
/// never begin with this prefix, and a dependency written on a contract is
/// satisfied by whichever installed unit declares providing it (SPEC §51.8).
pub const CAPABILITY_CONTRACT_PREFIX: &str = "cap.";

/// Bounds on one metapackage or distribution declaration. They bound the
/// admission walk; none of them is a product permission decision.
pub const MAX_MACRO_MEMBERS: usize = 64;
/// Bound on nested metapackages while a macro closure is walked.
pub const MAX_MACRO_DEPTH: usize = 4;
/// Bound on the atomic capabilities one code version declares.
pub const MAX_ATOMIC_CAPABILITIES: usize = 32;
/// Bound on a capability contract name.
pub const MAX_CONTRACT_BYTES: usize = 128;

fn validate_dotted_name(name: &str, what: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= MAX_CONTRACT_BYTES,
        "UNIT_CAPABILITY_INVALID: {what} {name:?} must be 1..{MAX_CONTRACT_BYTES} bytes"
    );
    for segment in name.split('.') {
        ensure!(
            !segment.is_empty(),
            "UNIT_CAPABILITY_INVALID: {what} {name:?} has an empty segment"
        );
        ensure!(
            segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'),
            "UNIT_CAPABILITY_INVALID: {what} {name:?} must be lowercase ASCII letters, digits, '-' or '_' in dot-separated segments"
        );
    }
    Ok(())
}

/// One versioned capability contract, written `cap.<name>/<major>`.
///
/// The contract identity is the pair (name, major): a provider's own package
/// version may change freely, and only a different contract major version makes
/// a dependency unsatisfied (SPEC §51.8). The value serializes as its text form,
/// so an author writes one string and no field can drift from it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityContract {
    /// `cap.` followed by a dotted name, for example `cap.forgetting`.
    pub contract_id: String,
    /// Major version, at least 1.
    pub major: u32,
}

impl CapabilityContract {
    /// Parse the text form. A malformed contract is refused instead of being
    /// treated as an opaque topic.
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let Some(rest) = trimmed.strip_prefix(CAPABILITY_CONTRACT_PREFIX) else {
            bail!(
                "UNIT_CAPABILITY_INVALID: capability contract {text:?} must begin with {CAPABILITY_CONTRACT_PREFIX:?}"
            );
        };
        let Some((name, version)) = rest.rsplit_once('/') else {
            bail!(
                "UNIT_CAPABILITY_INVALID: capability contract {text:?} must be `cap.<name>/<major>`"
            );
        };
        validate_dotted_name(name, "capability contract name")?;
        ensure!(
            !version.is_empty()
                && version.len() <= 9
                && version.chars().all(|c| c.is_ascii_digit()),
            "UNIT_CAPABILITY_INVALID: capability contract {text:?} major version must be a decimal number"
        );
        let major: u32 = version.parse().with_context(|| {
            format!("UNIT_CAPABILITY_INVALID: capability contract {text:?} major version is out of range")
        })?;
        ensure!(
            major >= 1,
            "UNIT_CAPABILITY_INVALID: capability contract {text:?} starts at major version 1"
        );
        Ok(Self {
            contract_id: format!("{CAPABILITY_CONTRACT_PREFIX}{name}"),
            major,
        })
    }

    /// Build a contract from a name with or without the reserved prefix.
    pub fn new(name: &str, major: u32) -> Result<Self> {
        let text = if name.starts_with(CAPABILITY_CONTRACT_PREFIX) {
            format!("{name}/{major}")
        } else {
            format!("{CAPABILITY_CONTRACT_PREFIX}{name}/{major}")
        };
        Self::parse(&text)
    }

    /// The text form, `cap.<name>/<major>`.
    pub fn text(&self) -> String {
        format!("{}/{}", self.contract_id, self.major)
    }

    /// Whether a reference names a typed capability contract at all. Every
    /// other `provides` entry is a topic or capability name.
    pub fn is_contract_ref(text: &str) -> bool {
        text.starts_with(CAPABILITY_CONTRACT_PREFIX)
    }

    /// Whether a provider of `provided` satisfies this requirement. Same name
    /// and same major version; nothing else.
    pub fn matches(&self, provided: &Self) -> bool {
        self.contract_id == provided.contract_id && self.major == provided.major
    }
}

impl fmt::Display for CapabilityContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.text())
    }
}

impl Serialize for CapabilityContract {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for CapabilityContract {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Whether a provider needs an atomic capability to itself or shares it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityExclusivity {
    /// Several providers may hold this capability at the same time.
    #[default]
    Shared,
    /// Two enabled providers cannot both hold it.
    Exclusive,
}

/// One atomic capability a code version actually exercises at run time: a hook
/// point, an event subscription, a singleton service.
///
/// Coexistence is judged here, on what is actually used (SPEC §51.8). Packages
/// carry no `conflicts` field and there is no package-level exclusivity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct AtomicCapability {
    pub atom_id: String,
    #[serde(default)]
    pub exclusivity: CapabilityExclusivity,
}

fn validate_atom_id(atom_id: &str) -> Result<()> {
    ensure!(
        !atom_id.is_empty() && atom_id.len() <= MAX_CONTRACT_BYTES,
        "TOOL_CAPABILITY_INVALID: atomic capability {atom_id:?} must be 1..{MAX_CONTRACT_BYTES} bytes"
    );
    ensure!(
        atom_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')),
        "TOOL_CAPABILITY_INVALID: atomic capability {atom_id:?} must be ASCII letters, digits, '.', '_', '-' or ':'"
    );
    Ok(())
}

/// The atomic capability two enabled providers both need exclusively, if any.
///
/// Providers that only share capabilities coexist. Nothing here consults a
/// model, a human or a package-level declaration.
pub fn atomic_conflict(left: &[AtomicCapability], right: &[AtomicCapability]) -> Option<String> {
    for atom in left
        .iter()
        .filter(|atom| atom.exclusivity == CapabilityExclusivity::Exclusive)
    {
        if right.iter().any(|other| {
            other.exclusivity == CapabilityExclusivity::Exclusive && other.atom_id == atom.atom_id
        }) {
            return Some(atom.atom_id.clone());
        }
    }
    None
}

/// How one metapackage member's admission is judged.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemberAdoption {
    /// The member's own kind decides: content needs an authenticated adoption
    /// attitude, code needs installation only. This is the default and it can
    /// never be used to waive the content requirement.
    #[default]
    Required,
    /// The author additionally requires an adoption attitude for a code or
    /// metapackage member.
    Attested,
}

/// One declared member of a metapackage.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MacroMemberSpec {
    /// The member's unit id. A unit id, never a capability contract.
    pub member_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_requirement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<Digest>,
    /// A named slot: the member must itself declare providing this exact
    /// capability contract, so a consumer resolves a role — a revision
    /// projection rule, an awareness host — without naming a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    #[serde(default)]
    pub adoption: MemberAdoption,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
}

/// The member list one metapackage version authors.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MetapackageBody {
    #[serde(default)]
    pub members: Vec<MacroMemberSpec>,
}

impl MetapackageBody {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.members.len() <= MAX_MACRO_MEMBERS,
            "UNIT_METAPACKAGE_INVALID: at most {MAX_MACRO_MEMBERS} members"
        );
        let mut ids: Vec<&str> = Vec::new();
        for member in &self.members {
            ensure!(
                !member.member_id.is_empty() && member.member_id.len() <= 128,
                "UNIT_METAPACKAGE_INVALID: invalid member identity {:?}",
                member.member_id
            );
            ensure!(
                !member.member_id.starts_with(CAPABILITY_CONTRACT_PREFIX),
                "UNIT_METAPACKAGE_INVALID: member {:?} uses the reserved capability namespace",
                member.member_id
            );
            ensure!(
                !ids.contains(&member.member_id.as_str()),
                "UNIT_METAPACKAGE_INVALID: duplicate member identity {:?}",
                member.member_id
            );
            ids.push(&member.member_id);
            if let Some(requirement) = &member.revision_requirement {
                ensure!(
                    !requirement.trim().is_empty(),
                    "UNIT_METAPACKAGE_INVALID: member {:?} declares an empty revision requirement",
                    member.member_id
                );
            }
            if let Some(purpose) = &member.purpose {
                ensure!(
                    purpose.len() <= MAX_TEXT_BYTES,
                    "UNIT_METAPACKAGE_INVALID: member {:?} purpose exceeds the text bound",
                    member.member_id
                );
            }
        }
        Ok(())
    }

    pub fn member(&self, member_id: &str) -> Option<&MacroMemberSpec> {
        self.members.iter().find(|member| member.member_id == member_id)
    }
}

/// The member list a version declares, or `None` for a version that declares
/// none. A metapackage without a member declaration, or a non-metapackage that
/// declares members, is refused: a macro that silently installs nothing is not
/// a macro.
pub fn declared_metapackage(descriptor: &UnitDescriptor) -> Result<Option<MetapackageBody>> {
    let Some(value) = descriptor.config.get("metapackage") else {
        ensure!(
            descriptor.kind != UnitKind::Metapackage,
            "UNIT_METAPACKAGE_INVALID: {} revision {} is a metapackage without a member declaration",
            descriptor.unit_id,
            descriptor.revision
        );
        return Ok(None);
    };
    ensure!(
        descriptor.kind == UnitKind::Metapackage,
        "UNIT_METAPACKAGE_INVALID: {} revision {} declares members but its kind is {}",
        descriptor.unit_id,
        descriptor.revision,
        descriptor.kind.as_str()
    );
    let body: MetapackageBody = serde_json::from_value(value.clone()).with_context(|| {
        format!(
            "UNIT_METAPACKAGE_INVALID: {} revision {} declares unreadable members",
            descriptor.unit_id, descriptor.revision
        )
    })?;
    body.validate()?;
    Ok(Some(body))
}

/// What the installation's distribution declares about one named target's
/// expected access.
///
/// It is installable content (SPEC §51.3), not a kernel constant: the kernel
/// never decides which access a deployment expects for a target. `expected`
/// uses the same typed requirement shape a package declares for itself, so an
/// expectation is compared with a real grant instead of matched as a keyword.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TargetAccessExpectation {
    pub target_id: String,
    pub expected: PackageCapabilities,
    /// Requirements this runtime's mechanism does not provide at all (no
    /// process spawn, no raw sockets on this build). They are facts about the
    /// mechanism that the expectation was written against, not a product
    /// permission decision.
    #[serde(default)]
    pub unavailable_requirements: Vec<String>,
}

/// One metapackage a distribution preinstalls.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PreinstalledMacro {
    pub unit_id: String,
    /// Parallel macros are peers; none contains another (SPEC §51.3).
    #[serde(default)]
    pub parallel: bool,
    /// The distribution is designed to depend on this macro. It records a
    /// closure fact; it is not a boot identity gate.
    #[serde(default)]
    pub design_dependency: bool,
}

/// The unit a distribution uses by default for one capability contract. It is
/// replaceable data: another unit may provide the same contract instead.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProviderDefault {
    pub contract: CapabilityContract,
    pub unit_id: String,
}

/// The distribution data one distribution version authors.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DistributionBody {
    #[serde(default)]
    pub targets: Vec<TargetAccessExpectation>,
    #[serde(default)]
    pub macros: Vec<PreinstalledMacro>,
    #[serde(default)]
    pub capability_providers: Vec<CapabilityProviderDefault>,
}

impl DistributionBody {
    pub fn validate(&self) -> Result<()> {
        let mut targets: Vec<&str> = Vec::new();
        for target in &self.targets {
            ensure!(
                !target.target_id.is_empty() && target.target_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid target identity {:?}",
                target.target_id
            );
            ensure!(
                !targets.contains(&target.target_id.as_str()),
                "UNIT_DISTRIBUTION_INVALID: target {:?} is declared twice",
                target.target_id
            );
            targets.push(&target.target_id);
            target.expected.validate()?;
            ensure!(
                target.unavailable_requirements.len() <= MAX_DECLARED_HOST,
                "UNIT_DISTRIBUTION_INVALID: target {:?} declares too many unavailable requirements",
                target.target_id
            );
        }
        let mut macros: Vec<&str> = Vec::new();
        for name in &self.macros {
            ensure!(
                !name.unit_id.is_empty() && name.unit_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid preinstalled metapackage identity {:?}",
                name.unit_id
            );
            ensure!(
                !name.unit_id.starts_with(CAPABILITY_CONTRACT_PREFIX),
                "UNIT_DISTRIBUTION_INVALID: preinstalled metapackage {:?} uses the reserved capability namespace",
                name.unit_id
            );
            ensure!(
                !macros.contains(&name.unit_id.as_str()),
                "UNIT_DISTRIBUTION_INVALID: metapackage {:?} is preinstalled twice",
                name.unit_id
            );
            macros.push(&name.unit_id);
        }
        let mut contracts: Vec<&CapabilityContract> = Vec::new();
        for provider in &self.capability_providers {
            ensure!(
                !provider.unit_id.is_empty() && provider.unit_id.len() <= 128,
                "UNIT_DISTRIBUTION_INVALID: invalid provider default identity {:?}",
                provider.unit_id
            );
            ensure!(
                !contracts.iter().any(|contract| **contract == provider.contract),
                "UNIT_DISTRIBUTION_INVALID: capability contract {} has two default providers",
                provider.contract
            );
            contracts.push(&provider.contract);
        }
        Ok(())
    }

    pub fn provider_for(&self, contract: &CapabilityContract) -> Option<&str> {
        self.capability_providers
            .iter()
            .find(|provider| provider.contract.matches(contract))
            .map(|provider| provider.unit_id.as_str())
    }
}

pub fn declared_distribution(descriptor: &UnitDescriptor) -> Result<Option<DistributionBody>> {
    let Some(value) = descriptor.config.get("distribution") else {
        ensure!(
            descriptor.kind != UnitKind::Distribution,
            "UNIT_DISTRIBUTION_INVALID: {} revision {} is a distribution without a declaration",
            descriptor.unit_id,
            descriptor.revision
        );
        return Ok(None);
    };
    ensure!(
        descriptor.kind == UnitKind::Distribution,
        "UNIT_DISTRIBUTION_INVALID: {} revision {} declares distribution data but its kind is {}",
        descriptor.unit_id,
        descriptor.revision,
        descriptor.kind.as_str()
    );
    let body: DistributionBody = serde_json::from_value(value.clone()).with_context(|| {
        format!(
            "UNIT_DISTRIBUTION_INVALID: {} revision {} declares unreadable distribution data",
            descriptor.unit_id, descriptor.revision
        )
    })?;
    body.validate()?;
    Ok(Some(body))
}

/// Which unit version a distribution declaration came from. A declaration is
/// read with its provenance, so a projection can name the content that carried
/// it instead of presenting it as a kernel table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredBy {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
}

/// One target expectation with the content that declared it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeclaredTargetAccess {
    pub expectation: TargetAccessExpectation,
    pub declared_by: DeclaredBy,
}

/// Where one metapackage member stands on this node.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MembershipStatus {
    /// A content member with an authenticated adoption attitude: it joined.
    Joined,
    /// A code member that is installed here.
    CodeInstalled,
    /// The member version is installed but its adoption attitude is missing,
    /// undetermined locally, or refuted locally. It has not joined.
    Unadopted,
    /// No installed version matches the member declaration.
    Absent,
    /// A version matched the identity but not the declaration (a named slot
    /// whose contract it does not provide, or a retained version that no longer
    /// satisfies the requirement), or its own closure is unresolved.
    Unresolved,
}

impl MembershipStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MembershipStatus::Joined => "joined",
            MembershipStatus::CodeInstalled => "code_installed",
            MembershipStatus::Unadopted => "unadopted",
            MembershipStatus::Absent => "absent",
            MembershipStatus::Unresolved => "unresolved",
        }
    }

    /// Whether the member is part of the macro on this node.
    pub fn joined(&self) -> bool {
        matches!(self, MembershipStatus::Joined | MembershipStatus::CodeInstalled)
    }
}

/// Tri-state result of verifying an adoption attitude's reason pointers.
///
/// It is a local verification result, never a property of the record: an
/// imported, unrelated or unexecuted reason pointer is `Undetermined`, and
/// `Undetermined` is reported as unresolved instead of being collapsed into a
/// claim of either verification or refutation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContentAdoptionEvidence {
    /// Every reason pointer was re-verified against this installation.
    VerifiedLocal,
    /// At least one reason pointer no longer holds here.
    RefutedLocal,
    /// Nothing local establishes the pointers either way.
    Undetermined,
}

impl ContentAdoptionEvidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            ContentAdoptionEvidence::VerifiedLocal => "verified_local",
            ContentAdoptionEvidence::RefutedLocal => "refuted_local",
            ContentAdoptionEvidence::Undetermined => "undetermined",
        }
    }
}

/// The adoption attitude a content member carries, as read from the
/// installation that holds it.
///
/// This module never stores an adoption attitude: the canonical content and
/// attitude ledger owns them. What arrives here is a read projection, and the
/// membership decision needs exactly three things — the attitude points at this
/// exact content digest, it reads as adopted, and its reason pointers verify
/// locally.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContentAdoption {
    pub attitude_id: String,
    pub content_digest: Digest,
    pub adopted: bool,
    pub evidence: ContentAdoptionEvidence,
    /// Digest of a content reason pointer, when the attitude carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_digest: Option<Digest>,
    pub recorded_at: String,
}

impl ContentAdoption {
    /// Whether this attitude lets the exact content revision join a macro.
    pub fn accepts(&self, digest: &Digest) -> bool {
        self.adopted
            && self.evidence == ContentAdoptionEvidence::VerifiedLocal
            && &self.content_digest == digest
    }

    /// Why this attitude does not admit the content, when it does not.
    pub fn refusal(&self, digest: &Digest) -> Option<String> {
        if self.accepts(digest) {
            return None;
        }
        if &self.content_digest != digest {
            return Some(format!(
                "adoption attitude {} points at {} instead of this content revision",
                self.attitude_id, self.content_digest
            ));
        }
        if !self.adopted {
            return Some(format!(
                "adoption attitude {} does not read as adopted",
                self.attitude_id
            ));
        }
        Some(match self.evidence {
            ContentAdoptionEvidence::VerifiedLocal => format!(
                "adoption attitude {} verified locally but is not adopted",
                self.attitude_id
            ),
            ContentAdoptionEvidence::RefutedLocal => format!(
                "adoption attitude {} has a reason pointer that no longer holds on this node",
                self.attitude_id
            ),
            ContentAdoptionEvidence::Undetermined => format!(
                "adoption attitude {} has no locally verified reason pointer; the evidence is undetermined, not refuted",
                self.attitude_id
            ),
        })
    }
}

/// The port through which a metapackage asks the content/attitude ledger
/// whether one exact content revision carries an admissible adoption attitude.
///
/// The canonical ledger (content, attitudes and reason pointers) implements
/// this; this module offers [`UnitAdoptionProjection`] as the projection over
/// the installation adoption records it already keeps, so nothing is duplicated
/// and no second adoption store exists.
pub trait MacroContentAdoption {
    fn adoption(
        &self,
        member_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>>;
}

/// Read projection over this installation's own adoption records.
///
/// The record lives inside the exact retained unit version it judged: an
/// imported descriptor has its cognition section stripped on arrival, so a
/// judgement made on another node can never be read here as a local adoption.
/// Every reason pointer is re-verified now against this node's own trial and
/// observation store; a stored `verified` flag is not trusted on its own.
pub struct UnitAdoptionProjection<'a> {
    conn: &'a Connection,
}

impl<'a> UnitAdoptionProjection<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }
}

impl MacroContentAdoption for UnitAdoptionProjection<'_> {
    fn adoption(
        &self,
        member_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>> {
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![member_id, revision, digest.0],
                |row| row.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let stored: StoredVersion = serde_json::from_str(&data)?;
        let Some(record) = stored.unit.config.pointer("/cognition/adoption") else {
            return Ok(None);
        };
        let decision = record
            .get("decision")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let recorded_at = record
            .get("recorded_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut pointers = 0usize;
        let mut verified = 0usize;
        let mut refuted = false;
        let mut reason_digest: Option<Digest> = None;
        for item in record
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            pointers += 1;
            match item.get("kind").and_then(Value::as_str) {
                Some("trial_attempt") => {
                    let attempt_id = item
                        .get("attempt_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let holds = Node::trial_attempt_facts(self.conn, attempt_id)?.is_some_and(|facts| {
                        facts.unit_id == member_id
                            && facts.revision == revision
                            && facts.artifact_digest == *digest
                            && facts.state == "finished"
                            && facts.plan_digest.is_some()
                    });
                    if holds {
                        verified += 1;
                    } else {
                        refuted = true;
                    }
                }
                Some("stored_observation") => {
                    let Some(observed) = item.get("digest").and_then(Value::as_str) else {
                        refuted = true;
                        continue;
                    };
                    let observed = Digest(observed.to_string());
                    if Node::observation_bound(self.conn, &stored.unit, &observed)? {
                        verified += 1;
                        if reason_digest.is_none() {
                            reason_digest = Some(observed);
                        }
                    } else {
                        refuted = true;
                    }
                }
                _ => refuted = true,
            }
        }
        let evidence = if refuted {
            ContentAdoptionEvidence::RefutedLocal
        } else if pointers > 0 && verified == pointers {
            ContentAdoptionEvidence::VerifiedLocal
        } else {
            ContentAdoptionEvidence::Undetermined
        };
        Ok(Some(ContentAdoption {
            attitude_id: format!("unit-adoption:{member_id}@{revision}:{recorded_at}"),
            content_digest: digest.clone(),
            adopted: decision == AdoptionDecision::Adopted.as_str(),
            evidence,
            reason_digest,
            recorded_at,
        }))
    }
}

/// One metapackage member's standing on this node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MembershipResolution {
    pub macro_id: String,
    /// The metapackages that reached this member, outermost first. Empty for a
    /// direct member of `macro_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    pub member_id: String,
    /// The named slot this member fills, when the declaration names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<CapabilityContract>,
    pub status: MembershipStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_kind: Option<UnitKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_artifact_digest: Option<Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<ContentAdoption>,
    /// Local facts. An installed member that is not enabled here has not joined
    /// a running macro, and a member installed on a peer never joins locally.
    #[serde(default)]
    pub enabled: bool,
    pub readiness: ReadinessState,
    pub reason: String,
}

/// One provider of a metapackage slot, as installed on this node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemberSelection {
    pub unit_id: String,
    pub revision: String,
    pub artifact_digest: Digest,
    pub member_kind: UnitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adoption: Option<ContentAdoption>,
    pub enabled: bool,
    pub readiness: ReadinessState,
}

/// The providers a named metapackage slot has on this node.
///
/// `selected` is an index into `providers`, never a kernel default: when no
/// provider is enabled and admitted the slot is omitted here, and a consumer
/// reports that instead of claiming a rule was applied.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MacroSlot {
    pub macro_id: String,
    pub contract: CapabilityContract,
    #[serde(default)]
    pub providers: Vec<MemberSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected: Option<usize>,
    /// True when more than one admitted provider is enabled and their actually
    /// used atomic capabilities do not conflict.
    #[serde(default)]
    pub coexisting: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
}

/// Why one unit version is part of an environment closure.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentRole {
    /// Named by the caller as a root of the environment.
    Root,
    /// Reached through a dependency of a root or of another member.
    Dependency,
}

impl EnvironmentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            EnvironmentRole::Root => "root",
            EnvironmentRole::Dependency => "dependency",
        }
    }
}

/// One unit version an environment pins.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentMember {
    pub unit_id: String,
    pub revision: String,
    /// The *unit* artifact digest: the module for an inline tool, the pointer
    /// document for an out-of-line one. It is the identity the unit inventory
    /// keys on, so it is the identity an environment pins.
    pub artifact_digest: Digest,
    pub role: EnvironmentRole,
    /// The dependency selections this closure locks for this member.
    ///
    /// They are copied into the generation instead of read back from the unit
    /// record, because a retained unit version's selections can be re-resolved
    /// later (`ResolveVersion`). A generation that kept only pointers would
    /// silently change meaning — and an older generation could become
    /// unsatisfiable just because a newer decision was recorded — so the
    /// closure document carries the exact selections it locks.
    #[serde(default)]
    pub edges: Vec<ResolvedDependency>,
    /// The capability requirements this member declared when the closure was
    /// locked, copied out of the unit version's authored configuration.
    ///
    /// A reimport can replace a retained unit version's descriptor at the same
    /// artifact identity, so reading the declaration back from the live unit
    /// row would let the permission surface of an already-selected
    /// environment change behind its back. A generation recorded before
    /// declarations existed stays [`PackageDeclaration::unrecorded`] — never a
    /// fabricated empty declaration and never a blanket grant.
    #[serde(default, skip_serializing_if = "PackageDeclaration::is_unrecorded")]
    pub declarations: PackageDeclaration,
}

/// A locked environment closure.
///
/// Immutable once prepared: it names exact retained unit versions, so a later
/// import of another version changes neither this document nor what a running
/// invocation resolved. Selecting it is a separate, conditional write of
/// [`EnvironmentSelector`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentGeneration {
    pub schema: u32,
    /// Version of the closure-digest formula this generation was recorded
    /// with. Version 1 hashed only the pinned unit identities; version 2 also
    /// hashes the locked selections and the frozen declarations, so a closure
    /// whose edges or declarations changed is a different closure instead of
    /// "unchanged". Generations written before this field existed are
    /// version 1 and keep verifying against their own formula.
    #[serde(default = "environment_closure_version_v1")]
    pub closure_version: u32,
    pub generation: u64,
    /// blake3 over the canonical closure document (sorted roots and members).
    pub digest: Digest,
    pub roots: Vec<String>,
    /// Members ordered by unit id.
    pub members: Vec<EnvironmentMember>,
    pub created_at: String,
}

/// The durable environment selector — the linearization point of an
/// activation. Only a successful conditional update of `revision` publishes a
/// generation; a materialized file, a process or a printed list does not.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentSelector {
    pub schema: u32,
    pub generation: u64,
    pub generation_digest: Digest,
    pub revision: u64,
    pub previous_generation: Option<u64>,
    pub updated_at: String,
}

/// Where one unit version stands relative to the active environment.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentPin {
    pub generation: u64,
    pub generation_digest: Digest,
    pub selector_revision: u64,
    pub role: EnvironmentRole,
    /// The declarations the generation froze for this member.
    #[serde(default, skip_serializing_if = "PackageDeclaration::is_unrecorded")]
    pub declarations: PackageDeclaration,
}

/// One requested root: an exact retained version, or the unit's current one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRoot {
    pub unit_id: String,
    #[serde(default)]
    pub revision: Option<String>,
    #[serde(default)]
    pub artifact_digest: Option<Digest>,
}

#[derive(Default)]
struct RequiredClosure {
    unresolved: Vec<String>,
    unmaterialized: Vec<String>,
    root_materialized: bool,
}

fn edge_matches_spec(edge: &ResolvedDependency, spec: &DependencySpec) -> bool {
    edge.edge_id == spec.edge_id
        && edge.strength == spec.strength
        && edge.selector == spec.selector
}

/// The exact selection a retained edge records, or `None` when the inventory
/// never resolved it. An unresolved or ambiguous edge is not a candidate for
/// an environment closure: guessing here would publish a closure the node
/// cannot explain.
fn selection_of(edge: Option<&ResolvedDependency>) -> Option<(String, String, Digest)> {
    let edge = edge?;
    match (
        edge.status,
        edge.selected_unit_id.as_ref(),
        edge.selected_revision.as_ref(),
        edge.selected_artifact_digest.as_ref(),
    ) {
        (EdgeStatus::Satisfied, Some(unit_id), Some(revision), Some(digest)) => {
            Some((unit_id.clone(), revision.clone(), digest.clone()))
        }
        _ => None,
    }
}

/// The capability requirements one unit version authored, read from the
/// package description that travels with it.
///
/// A code package carries its description under `config.tool`; the
/// `capabilities` object there is the declaration SPEC §9 requires every
/// package description to record. A version without that object records no
/// declaration: that is [`PackageDeclaration::unrecorded`], which the runtime
/// reports as such — never a fabricated empty declaration (which would claim
/// the package needs nothing) and never a blanket grant.
///
/// A malformed declaration refuses the closure it belongs to, so an
/// environment never pins requirements the node cannot even parse.
pub fn declared_package_capabilities(descriptor: &UnitDescriptor) -> Result<PackageDeclaration> {
    if descriptor.kind != UnitKind::Code {
        return Ok(PackageDeclaration::unrecorded());
    }
    let Some(value) = descriptor.config.pointer("/tool/capabilities") else {
        return Ok(PackageDeclaration::unrecorded());
    };
    let declaration: PackageDeclaration =
        serde_json::from_value(value.clone()).with_context(|| {
            format!(
                "UNIT_PACKAGE_CAPABILITIES_INVALID: {} revision {} declares unreadable capabilities",
                descriptor.unit_id, descriptor.revision
            )
        })?;
    if let Some(capabilities) = declaration.capabilities() {
        capabilities.validate()?;
    }
    // The frozen copy is normalized so two equal declarations are one
    // document: the closure digest and the divergence report both compare
    // declarations, not authoring order.
    Ok(declaration.normalized())
}

fn legacy_ambiguous(spec: &DependencySpec, reason: &str) -> ResolvedDependency {
    ResolvedDependency {
        edge_id: spec.edge_id.clone(),
        strength: spec.strength,
        selector: spec.selector.clone(),
        status: EdgeStatus::LegacyAmbiguous,
        selected_unit_id: None,
        selected_revision: None,
        selected_artifact_digest: None,
        selected_contract: None,
        coexisting_providers: Vec::new(),
        used_capabilities: Vec::new(),
        reason: reason.to_string(),
    }
}

/// Migrate only attributable legacy choices. Multiple retained digests may
/// have overwritten each other's old rows, even if their declarations match.
/// The untouched table remains available for investigation and recovery.
pub(crate) fn migrate_v3_edges(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    tx.execute_batch("ALTER TABLE unit_edges RENAME TO unit_edges_legacy;")?;
    tx.execute_batch(UNIT_EDGE_TABLE)?;
    tx.execute_batch(UNIT_VERSION_HISTORY_TABLE)?;
    tx.execute_batch(UNIT_EDGE_HISTORY_TABLE)?;
    let versions: Vec<(String, String, String, String, i64)> = tx
        .prepare(
            "SELECT v.unit_id,v.revision,v.artifact_digest,v.data, \
             (SELECT count(*) FROM unit_versions AS n WHERE n.unit_id=v.unit_id AND n.revision=v.revision) \
             FROM unit_versions AS v ORDER BY v.unit_id,v.revision,v.artifact_digest",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (unit_id, revision, artifact_digest, data, version_count) in versions {
        let stored: StoredVersion = serde_json::from_str(&data)?;
        ensure!(
            stored.unit.unit_id == unit_id
                && stored.unit.revision == revision
                && stored.unit.artifact_digest.0 == artifact_digest,
            "stored unit version identity does not match its key"
        );
        validate_descriptor(&stored.unit)?;
        for spec in &stored.unit.dependencies {
            let old: Option<String> = tx
                .query_row(
                    "SELECT data FROM unit_edges_legacy WHERE unit_id=?1 AND revision=?2 AND edge_id=?3",
                    params![unit_id, revision, spec.edge_id],
                    |r| r.get(0),
                )
                .optional()?;
            let edge = if version_count == 1 {
                old.and_then(|value| serde_json::from_str::<ResolvedDependency>(&value).ok())
                    .filter(|edge| edge_matches_spec(edge, spec))
                    .unwrap_or_else(|| legacy_ambiguous(spec, "legacy edge is missing or differs from the retained declaration; use resolve_version for this exact version"))
            } else {
                legacy_ambiguous(spec, "legacy edge ownership is ambiguous across retained artifact digests; use resolve_version for this exact version")
            };
            tx.execute(
                "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5)",
                params![unit_id, revision, artifact_digest, spec.edge_id, serde_json::to_string(&edge)?],
            )?;
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- node ----

impl Node {
    /// Handle one installation-unit request.
    ///
    /// This is the only entry point for package transfer, per-node
    /// installation state and component trials.
    pub fn handle_unit_request(&mut self, request: UnitRequest) -> Result<Value> {
        let result = self.handle_unit_inner(request);
        if let Err(e) = &result {
            if e.is::<rusqlite::Error>() || e.is::<serde_json::Error>() || e.is::<std::io::Error>()
            {
                self.failed = true;
            }
        }
        result
    }

    fn handle_unit_inner(&mut self, request: UnitRequest) -> Result<Value> {
        ensure_unit_schema(&self.conn)?;
        // A crash between persisting an attempt and recording its result
        // leaves an unresolved attempt. It becomes unknown here and is never
        // replayed automatically.
        let reconciled = self.retire_unknown_trials()?;
        let mut result = match request {
            UnitRequest::Import {
                export,
                package_root,
                enable,
            } => self.unit_import(export, &package_root, enable)?,
            UnitRequest::Materialize {
                unit_id,
                package_root,
            } => self.unit_materialize(&unit_id, &package_root)?,
            UnitRequest::ResolveVersion { unit_id, revision, artifact_digest } => {
                self.unit_resolve_version(&unit_id, &revision, &artifact_digest)?
            }
            UnitRequest::Status { unit_id } => self.unit_status(&unit_id)?,
            UnitRequest::List {} => self.unit_list()?,
            UnitRequest::Export { unit_id } => self.unit_export(&unit_id)?,
            UnitRequest::SetEnabled { unit_id, enabled } => {
                self.unit_set_enabled(&unit_id, enabled)?
            }
            UnitRequest::Trial {
                unit_id,
                argv,
                cwd,
                timeout_ms,
                risk,
                benefit,
                consequence,
            } => self.unit_trial(
                &unit_id,
                &argv,
                cwd.as_deref(),
                timeout_ms,
                &risk,
                &benefit,
                &consequence,
            )?,
            UnitRequest::Evidence {
                unit_id,
                after,
                limit,
            } => self.unit_evidence(unit_id.as_deref(), &after, limit)?,
            UnitRequest::ImportExternal {
                unit,
                artifact,
                package_root,
                enable,
            } => self.unit_import_external(unit, artifact, &package_root, enable)?,
            UnitRequest::Read { unit_id, revision } => self.unit_read(&unit_id, &revision)?,
            UnitRequest::RecordAdoption {
                unit_id,
                revision,
                artifact_digest,
                decision,
                method,
                evidence,
                notes,
                enable,
            } => self.unit_record_adoption(
                &unit_id,
                &revision,
                &artifact_digest,
                decision,
                method,
                &evidence,
                notes.as_deref(),
                enable,
            )?,
            UnitRequest::BeginTrial {
                unit_id,
                revision,
                artifact_digest,
                kind,
                plan,
            } => self.unit_begin_trial(&unit_id, &revision, &artifact_digest, &kind, &plan)?,
            UnitRequest::FinishTrial {
                attempt_id,
                outcome,
                receipts,
            } => self.unit_finish_trial(&attempt_id, outcome, &receipts)?,
            UnitRequest::RegisterObservation {
                unit_id,
                revision,
                artifact_digest,
                kind,
                bytes,
            } => self.unit_register_observation(&unit_id, &revision, &artifact_digest, &kind, &bytes)?,
            UnitRequest::Observations { unit_id } => self.unit_observations(&unit_id)?,
            UnitRequest::Lineage { unit_id } => self.unit_lineage(&unit_id)?,
            UnitRequest::PrepareEnvironment { roots } => self.prepare_environment(&roots)?,
            UnitRequest::ActivateEnvironment {
                generation,
                expected_revision,
            } => self.activate_environment(generation, expected_revision)?,
            UnitRequest::RollbackEnvironment { expected_revision } => {
                self.rollback_environment(expected_revision)?
            }
            UnitRequest::Environment { limit } => self.environment_report(limit)?,
            UnitRequest::QueryMetapackage { unit_id } => self.query_metapackage(&unit_id)?,
            UnitRequest::QueryDistribution { unit_id } => self.query_distribution(&unit_id)?,
            UnitRequest::QuerySlot { macro_id, contract } => {
                self.query_slot(&macro_id, &contract)?
            }
        };
        if let Some(object) = result.as_object_mut() {
            object.insert("schema".into(), json!(UNIT_SCHEMA));
            if reconciled > 0 {
                object.insert("reconciled_attempts".into(), json!(reconciled));
            }
        }
        Ok(result)
    }

    // ---------------------------------------------------------- import -----

    /// Register a transferred package closure in this installation.
    ///
    /// A package import writes unit versions, their edges, the inventory's
    /// per-unit current pointer, materialization state and trial provenance.
    /// It never writes `unit_environment.*`: the environment selector and the
    /// generations it names are local authority, so a `sync`/`receive` import
    /// (which is exactly this request) cannot activate a foreign environment.
    /// With a generation active, `tool_unit_facts` selects by that
    /// generation's pins, so even an import that changes `unit_current` does
    /// not change what a name-only resolution selects.
    fn unit_import(
        &mut self,
        export: UnitExport,
        package_root: &str,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(matches!(export.schema, 2 | UNIT_SCHEMA), "unit export schema mismatch");
        ensure!(
            !export.packages.is_empty() && export.packages.len() <= MAX_PACKAGES,
            "unit package count out of bounds"
        );
        let root = normalize_root(package_root)?;
        // Verify every member before touching storage: a package with a wrong
        // digest is rejected as a whole.
        let mut members: Vec<(UnitDescriptor, Vec<u8>, Vec<TrialProvenance>, Option<Vec<ResolvedDependency>>)> = Vec::new();
        for package in &export.packages {
            let bytes = match external_artifact_of(&package.unit) {
                Some(external) => {
                    ensure!(
                        package.artifact_base64.is_empty(),
                        "an out-of-line artifact must not carry inline bytes"
                    );
                    ensure!(
                        external.digest == package.unit.artifact_digest,
                        "out-of-line artifact digest does not match the unit descriptor"
                    );
                    ensure!(
                        external.bytes <= MAX_EXTERNAL_ARTIFACT_BYTES,
                        "out-of-line artifact exceeds the size bound"
                    );
                    Vec::new()
                }
                None => {
                    let bytes = b64_decode(&package.artifact_base64)?;
                    ensure!(
                        bytes.len() <= MAX_ARTIFACT_BYTES,
                        "unit artifact exceeds the size bound"
                    );
                    ensure!(
                        Digest::blake3(&bytes) == package.unit.artifact_digest,
                        "unit artifact digest mismatch"
                    );
                    bytes
                }
            };
            ensure!(
                package.unit.artifact_digest.algorithm() == "blake3",
                "unsupported artifact digest algorithm"
            );
            validate_descriptor(&package.unit)?;
            ensure!(
                !members.iter().any(|(unit, _, _, _)| unit.unit_id == package.unit.unit_id
                    && unit.revision == package.unit.revision
                    && unit.artifact_digest == package.unit.artifact_digest),
                "duplicate unit version in package closure"
            );
            ensure!(export.schema != 2 || package.resolved_dependencies.is_none(), "schema-2 packages cannot carry selections");
            // The declared configuration is copied as the package authored it.
            // This module adds no gate on its content: actual local bindings
            // (environment values, secret files, provider grants) live outside
            // the unit configuration and are never read into a package.
            // A judgement recorded on another node and the observations bound
            // to it are local facts of that node: they never travel with the
            // package and never arrive as an acceptance here.
            let mut descriptor = package.unit.clone();
            strip_local_sections(&mut descriptor.config);
            members.push((descriptor, bytes, package.evidence.clone(), package.resolved_dependencies.clone()));
        }
        let root_index = match (&export.root_revision, &export.root_artifact_digest) {
            (Some(revision), Some(digest)) => {
                ensure!(export.schema == UNIT_SCHEMA, "schema-2 transfer cannot carry a digest-bound root");
                members.iter().position(|(unit, _, _, _)| unit.unit_id == export.root_unit_id
                    && &unit.revision == revision && &unit.artifact_digest == digest)
                    .context("selected root version is absent from the package closure")?
            }
            (None, None) => {
                let mut roots = members.iter().enumerate().filter(|(_, (unit, _, _, _))| unit.unit_id == export.root_unit_id);
                let first = roots.next().context("root unit is absent from the package closure")?.0;
                // Schema 2 exporters placed their root first. Schema 3 authored
                // closures with several root versions must identify one.
                ensure!(export.schema == 2 || roots.next().is_none(), "multiple root versions require a digest-bound root");
                first
            }
            _ => bail!("root version reference is incomplete"),
        };
        let imported_at = now();
        let resolutions: Vec<Vec<ResolvedDependency>>;
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Bind the candidates and existing pins to the same write-locked
            // database snapshot. Supplied versions take precedence only for
            // new selections, never for an already pinned local selection.
            let mut available = members.iter()
                .map(|(unit, _, _, _)| (unit.unit_id.clone(), unit.revision.clone(), unit.artifact_digest.clone(), unit.provides.clone()))
                .collect::<Vec<(String, String, Digest, Vec<String>)>>();
            // Incoming versions replace the exact same identity, not merely
            // take precedence. Their old retained descriptor would otherwise
            // keep declaring capabilities the package explicitly removed.
            let extra = resolution_candidates(&tx)?;
            for candidate in extra {
                let dominated = members.iter().any(|(unit, _, _, _)| unit.unit_id == candidate.unit_id
                    && unit.revision == candidate.revision && unit.artifact_digest == candidate.digest);
                if !dominated {
                    available.push((candidate.unit_id, candidate.revision, candidate.digest, candidate.provides));
                }
            }
            let mut resolved_versions = Vec::new();
            for (descriptor, _, _, carried) in &members {
                if let Some(carried) = carried {
                    ensure!(carried.len() == descriptor.dependencies.len(), "exported dependency selections do not match the declaration");
                    for edge in carried {
                        ensure!(descriptor.dependencies.iter().filter(|spec| edge_matches_spec(edge, spec)).count() == 1
                            && carried.iter().filter(|other| other.edge_id == edge.edge_id).count() == 1,
                            "exported dependency selection does not match the declaration");
                    }
                }
                let mut edges = Vec::new();
                for spec in &descriptor.dependencies {
                    let incoming = carried.as_ref().and_then(|edges| edges.iter().find(|edge| edge_matches_spec(edge, spec)));
                    let incoming = incoming.map(|edge| carry_edge(spec, edge, &available)).transpose()?;
                    let edge = if let Some(incoming) = incoming {
                        incoming
                    } else if export.schema == 2 {
                        legacy_ambiguous(spec, "schema-2 transfer did not carry a digest-bound dependency selection; use resolve_version for this exact version")
                    } else {
                        resolve_edge(spec, &available)
                    };
                    edges.push(edge);
                }
                resolved_versions.push(edges);
            }
            resolutions = resolved_versions;
            for (index, ((descriptor, bytes, evidence, carried), edges)) in
                members.iter().zip(resolutions.iter()).enumerate()
            {
                let mut descriptor = descriptor.clone();
                let old_version: Option<String> = tx.query_row(
                    "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                    params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0], |r| r.get(0),
                ).optional()?;
                let old_version: Option<StoredVersion> = old_version.map(|data| serde_json::from_str(&data)).transpose()?;
                // A recorded adoption is a fact about this installation, not
                // part of the authored package. Re-importing the same
                // artifact identity keeps the decision that was already
                // recorded instead of silently dropping it.
                if let Some(old) = old_version.as_ref() {
                    if let Some(adoption) = old.unit.config.pointer("/cognition/adoption").cloned() {
                        if descriptor.config.pointer("/cognition/adoption").is_none() {
                            set_config_section(&mut descriptor.config, "cognition", "adoption", adoption);
                        }
                    }
                    // The observations this installation recorded against
                    // this exact artifact identity are its own facts. A
                    // re-import of the same bytes must not drop them.
                    if let Some(observations) =
                        old.unit.config.pointer("/cognition/observations").cloned()
                    {
                        if descriptor.config.pointer("/cognition/observations").is_none() {
                            set_config_section(
                                &mut descriptor.config,
                                "cognition",
                                "observations",
                                observations,
                            );
                        }
                    }
                }
                // Package bytes reuse the existing inline object store.
                if external_artifact_of(&descriptor).is_none() {
                    let stored = crate::seal(&tx, bytes, "application/octet-stream", "unit-package")?;
                    ensure!(
                        stored.digest == descriptor.artifact_digest,
                        "stored artifact digest mismatch"
                    );
                }
                let record = StoredVersion {
                    unit: descriptor.clone(),
                    remote_evidence: evidence
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<serde_json::Result<Vec<_>>>()?,
                    imported_at: imported_at.clone(),
                    origin_node: export.origin_node.clone(),
                };
                // Reimport may update the descriptor or remote provenance at
                // the same artifact identity. Retain the exact prior record.
                tx.execute(
                    "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) \
                     SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions \
                     WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                    params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0],
                )?;
                tx.execute(
                    "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET \
                     data=excluded.data,imported_at=excluded.imported_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        imported_at,
                        serde_json::to_string(&record)?
                    ],
                )?;
                // The first package for an identity chooses its current
                // version; the declared root is explicitly first among root
                // versions. Other retained closure members do not overwrite it.
                let current_row: Option<(String, String, i64)> = tx.query_row(
                    "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                    params![descriptor.unit_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).optional()?;
                let already_held = old_version.is_some();
                let current_revision = current_row.as_ref().map(|row| row.0.clone()).unwrap_or_default();
                let current_digest = current_row.as_ref().map(|row| row.1.clone()).unwrap_or_default();
                let current_names_other = current_revision != descriptor.revision
                    || current_digest != descriptor.artifact_digest.0;
                let choose_current = (index == root_index || (descriptor.unit_id != export.root_unit_id
                    && !members[..index].iter().any(|(unit, _, _, _)| unit.unit_id == descriptor.unit_id)))
                    && !(already_held && current_names_other);
                if choose_current {
                    let previous_enabled = current_row.as_ref().map(|row| row.2 != 0).unwrap_or(false);
                    let enabled = if already_held {
                        previous_enabled
                    } else if index == root_index {
                        enable.unwrap_or(previous_enabled)
                    } else {
                        previous_enabled
                    };
                    tx.execute(
                    "INSERT INTO unit_current(unit_id,revision,artifact_digest,enabled,updated_at) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id) DO UPDATE SET \
                     revision=excluded.revision,artifact_digest=excluded.artifact_digest,\
                     enabled=excluded.enabled,updated_at=excluded.updated_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        i64::from(enabled),
                        imported_at
                    ],
                    )?;
                }
                for edge in edges {
                    let old_data: Option<String> = tx.query_row(
                        "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                        params![descriptor.unit_id, descriptor.revision, descriptor.artifact_digest.0, edge.edge_id], |r| r.get(0),
                    ).optional()?;
                    let old_edge: Option<ResolvedDependency> = old_data.as_deref().map(serde_json::from_str).transpose()?;
                    let old_pinned = old_edge.as_ref().filter(|old| {
                        old_version.as_ref().is_some_and(|version| version.unit.dependencies.iter().any(|spec| edge_matches_spec(old, spec)))
                            && old.edge_id == edge.edge_id && old.strength == edge.strength && old.selector == edge.selector
                            && (old.status == EdgeStatus::Satisfied || old.selected_unit_id.is_some()
                                || old.selected_revision.is_some() || old.selected_artifact_digest.is_some())
                    });
                    if let Some(old) = old_pinned {
                        if carried.is_some() && edge.selected_artifact_digest.is_some() {
                            ensure!(old.selected_unit_id == edge.selected_unit_id
                                && old.selected_revision == edge.selected_revision
                                && old.selected_artifact_digest == edge.selected_artifact_digest,
                                "conflicting digest-bound dependency selections; neither pinned choice was replaced");
                        }
                    }
                    let edge = old_pinned.unwrap_or(edge);
                    let data = serde_json::to_string(&edge)?;
                    if old_data.as_ref().is_some_and(|old| old != &data) {
                        archive_edge(&tx, &descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest.0, &edge.edge_id)?;
                    }
                    tx.execute(
                        "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5) \
                         ON CONFLICT(unit_id,revision,artifact_digest,edge_id) DO UPDATE SET data=excluded.data",
                        params![
                            descriptor.unit_id,
                            descriptor.revision,
                            descriptor.artifact_digest.0,
                            edge.edge_id,
                            data
                        ],
                    )?;
                }
                tx.execute(
                    "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) \
                     VALUES(?1,?2,?3,'','','pending','',?4) \
                     ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET \
                     root='',path='',state='pending',detail='',updated_at=excluded.updated_at",
                    params![
                        descriptor.unit_id,
                        descriptor.revision,
                        descriptor.artifact_digest.0,
                        imported_at
                    ],
                )?;
            }
            // A later package may complete an older unit's required closure.
            // Keep successful exact selections pinned to their retained bytes;
            // only retry unresolved/omitted edges against this import's facts.
            let pending_edges = tx
                .prepare(
                    "SELECT e.unit_id,e.revision,e.artifact_digest,e.edge_id,e.data,v.data FROM unit_edges AS e \
                     JOIN unit_versions AS v ON v.unit_id=e.unit_id AND v.revision=e.revision AND v.artifact_digest=e.artifact_digest \
                     WHERE json_extract(e.data,'$.status') NOT IN ('satisfied','legacy_ambiguous')",
                )?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (unit_id, revision, artifact_digest, edge_id, data, version_data) in pending_edges {
                let previous: ResolvedDependency = serde_json::from_str(&data)?;
                let stored: StoredVersion = serde_json::from_str(&version_data)?;
                let Some(spec) = stored.unit.dependencies.iter().find(|spec| edge_matches_spec(&previous, spec)) else { continue; };
                let updated = if previous.selected_artifact_digest.is_some() {
                    carry_edge(spec, &previous, &available)?
                } else { resolve_edge(spec, &available) };
                if updated != previous {
                    archive_edge(&tx, &unit_id, &revision, &artifact_digest, &edge_id)?;
                    tx.execute(
                        "UPDATE unit_edges SET data=?5 WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                        params![unit_id, revision, artifact_digest, edge_id, serde_json::to_string(&updated)?],
                    )?;
                }
            }
            tx.commit()?;
        }
        // Materialization happens outside the database transaction; a partial
        // materialization leaves the unit not ready and is retryable.
        let mut statuses = Vec::new();
        for (descriptor, _, _, _) in &members {
            let status = self.materialize_version(&descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest, &root)?;
            statuses.push(status);
        }
        let mut dependency_resolutions = Vec::new();
        for ((descriptor, _, _, _), offered) in members.iter().zip(resolutions.iter()) {
            let unit = self.installed_unit_version(&descriptor.unit_id, &descriptor.revision, &descriptor.artifact_digest)?
                .context("imported unit version is not retained")?;
            let closure = self.required_closure(&unit)?;
            let closure = closure_value(&unit.edges, &closure);
            dependency_resolutions.push(json!({
                "unit_id": descriptor.unit_id,
                "revision": descriptor.revision,
                "artifact_digest": descriptor.artifact_digest,
                "offered_edges": offered,
                "retained_edges": &unit.edges,
                "closure": closure,
            }));
        }
        Ok(json!({
            "imported": statuses.len(),
            "package_root": root,
            "readiness_basis": "local_trials_only",
            "note": "imported packages are not ready here; only a local trial can make them ready",
            "units": statuses,
            "dependency_resolutions": dependency_resolutions,
        }))
    }

    fn unit_materialize(&mut self, unit_id: &str, package_root: &str) -> Result<Value> {
        let root = normalize_root(package_root)?;
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let status = self.materialize_unit(unit_id, &root)?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "materialized": status.get("materialized").cloned().unwrap_or(json!(false)),
            "package_path": status.get("package_path").cloned().unwrap_or(Value::Null),
            "detail": status.get("detail").cloned().unwrap_or(json!("")),
        }))
    }

    fn unit_resolve_version(&mut self, unit_id: &str, revision: &str, artifact_digest: &Digest) -> Result<Value> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let available: Vec<(String, String, Digest, Vec<String>)> = resolution_candidates(&tx)?
            .into_iter()
            .map(|c| (c.unit_id, c.revision, c.digest, c.provides))
            .collect();
        let data: Option<String> = tx.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0], |r| r.get(0),
        ).optional()?;
        let stored: StoredVersion = serde_json::from_str(&data.context("unit version is not retained")?)?;
        ensure!(stored.unit.unit_id == unit_id && stored.unit.revision == revision && stored.unit.artifact_digest == *artifact_digest,
            "stored unit version identity does not match its key");
        for spec in &stored.unit.dependencies {
            let old_data: Option<String> = tx.query_row(
                "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
                params![unit_id, revision, artifact_digest.0, spec.edge_id], |r| r.get(0),
            ).optional()?;
            let old: Option<ResolvedDependency> = old_data.as_deref().map(serde_json::from_str).transpose()?;
            let old = old.filter(|edge| edge_matches_spec(edge, spec)).unwrap_or_else(|| {
                legacy_ambiguous(spec, "no matching digest-bound edge is retained; make a new resolution for this version")
            });
            let updated = if old.status == EdgeStatus::Satisfied {
                old
            } else if old.selected_artifact_digest.is_some() {
                carry_edge(spec, &old, &available)?
            } else {
                let candidate = resolve_edge(spec, &available);
                // A failed new attempt does not explain who owned the old row.
                if old.status == EdgeStatus::LegacyAmbiguous && candidate.status != EdgeStatus::Satisfied { old } else { candidate }
            };
            let data = serde_json::to_string(&updated)?;
            if old_data.as_ref().is_some_and(|old| old != &data) {
                archive_edge(&tx, unit_id, revision, &artifact_digest.0, &spec.edge_id)?;
            }
            tx.execute(
                "INSERT INTO unit_edges(unit_id,revision,artifact_digest,edge_id,data) VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(unit_id,revision,artifact_digest,edge_id) DO UPDATE SET data=excluded.data",
                params![unit_id, revision, artifact_digest.0, spec.edge_id, data],
            )?;
        }
        tx.commit()?;
        let unit = self.installed_unit_version(unit_id, revision, artifact_digest)?.context("unit version is not retained")?;
        let closure = self.required_closure(&unit)?;
        let current: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM unit_current WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3)",
            params![unit_id, revision, artifact_digest.0], |r| r.get(0),
        )?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest,
            "current": current,
            "closure": closure_value(&unit.edges, &closure),
            "note": "new resolution only; the current version and component activation were not changed",
        }))
    }

    /// Write the actual package bytes into the caller-scoped package root.
    fn materialize_unit(&mut self, unit_id: &str, root: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        self.materialize_version(&unit.unit_id, &unit.revision, &unit.artifact_digest, root)
    }

    /// Retained versions may share an id and revision; never overwrite their
    /// bytes while another version remains selected in a dependency closure.
    fn materialize_version(&mut self, unit_id: &str, revision: &str, artifact_digest: &Digest, root: &str) -> Result<Value> {
        let unit = self
            .installed_unit_version(unit_id, revision, artifact_digest)?
            .context("unit version is not retained on this node")?;
        if let Some(external) = external_artifact_of(&unit.descriptor) {
            // The bytes live outside this installation's package store. The
            // node records where they are; it does not claim a local copy
            // and it does not call the unit ready because of it.
            let detail = format!("{}:{}", external.store, external.locator);
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) VALUES(?1,?2,?3,?4,'',?5,?6,?7) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET root=excluded.root,path='',state=excluded.state,detail=excluded.detail,updated_at=excluded.updated_at",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0, root, "external", detail, now()],
            )?;
            tx.commit()?;
            return Ok(json!({
                "unit_id": unit.unit_id,
                "revision": unit.revision,
                "artifact_digest": unit.artifact_digest,
                "materialized": false,
                "state": "external",
                "package_path": Value::Null,
                "detail": format!("the artifact bytes live outside this installation: {detail}"),
            }));
        }
        let bytes = self.object_bytes(&unit.artifact_digest)?;
        ensure!(
            Digest::blake3(&bytes) == unit.artifact_digest,
            "stored artifact corruption"
        );
        let directory = Path::new(root)
            .join("units")
            .join(safe_component(&unit.unit_id)?)
            .join(safe_component(&unit.revision)?)
            .join(safe_component(&unit.artifact_digest.0)?);
        let outcome = write_package(&directory, &bytes, &unit.descriptor);
        let (state, path, detail) = match &outcome {
            Ok(path) => ("materialized", path.clone(), String::new()),
            Err(e) => ("failed", String::new(), format!("{e:#}")),
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO unit_materializations(unit_id,revision,artifact_digest,root,path,state,detail,updated_at) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8) \
             ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET root=excluded.root,path=excluded.path,\
             state=excluded.state,detail=excluded.detail,updated_at=excluded.updated_at",
            params![
                unit.unit_id,
                unit.revision,
                unit.artifact_digest.0,
                root,
                path,
                state,
                detail,
                now()
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "materialized": state == "materialized",
            "state": state,
            "package_path": if path.is_empty() { Value::Null } else { json!(path) },
            "detail": detail,
        }))
    }

    // ------------------------------------------------------------ facts ----

    fn unit_status(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        Ok(self.status_value(&unit)?)
    }

    fn unit_list(&self) -> Result<Value> {
        let mut units = Vec::new();
        for unit in self.installed_units()? {
            units.push(self.status_value(&unit)?);
        }
        Ok(json!({
            "units": units,
            "readiness_basis": "local_trials_only",
        }))
    }

    fn status_value(&self, unit: &InstalledUnit) -> Result<Value> {
        let closure = self.required_closure(unit)?;
        let (readiness, reason) = self.readiness_with_closure(unit, &closure)?;
        let local_trials = self.local_trial_count(&unit.unit_id, &unit.artifact_digest)?;
        let remote_evidence = unit_remote_evidence(&self.conn, unit)?.len() as u32;
        Ok(json!({
            "unit_id": unit.unit_id,
            "kind": unit.descriptor.kind,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "installed": true,
            "enabled": unit.enabled,
            "materialized": closure.root_materialized,
            "package_root": unit.materialization.as_ref().map(|m| m.root.clone()).unwrap_or_default(),
            "package_path": unit.materialization.as_ref().map(|m| m.path.clone()).unwrap_or_default(),
            "materialization_detail": unit.materialization.as_ref().map(|m| m.detail.clone()).unwrap_or_default(),
            "provides": unit.descriptor.provides,
            "config": unit.descriptor.config,
            "applicability": unit.descriptor.applicability,
            "advisory": advisory_notes(unit.descriptor.applicability.as_ref()),
            "closure": closure_value(&unit.edges, &closure),
            "readiness": {
                "state": readiness,
                "reason": reason,
                "basis": "local_trials_only",
            },
            "local_trials": local_trials,
            "remote_evidence": remote_evidence,
        }))
    }

    fn unit_set_enabled(&mut self, unit_id: &str, enabled: bool) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE unit_current SET enabled=?2,updated_at=?3 WHERE unit_id=?1",
            params![unit.unit_id, i64::from(enabled), now()],
        )?;
        tx.commit()?;
        let (readiness, reason) = self.readiness_of(&unit)?;
        Ok(json!({
            "unit_id": unit.unit_id,
            "enabled": enabled,
            "readiness": readiness,
            "reason": reason,
        }))
    }

    /// Typed export of every unit installed on this node.
    ///
    /// One entry per installed unit, each holding the unit itself and the
    /// dependency closure this node could resolve. The caller can import the
    /// result with `UnitRequest::Import` without a `serde_json::Value`
    /// roundtrip.
    pub fn export_installation_units(&self) -> Result<Vec<UnitExport>> {
        let mut exports = Vec::new();
        for unit in self.installed_units()? {
            exports.push(self.export_unit_closure(&unit)?);
        }
        Ok(exports)
    }

    /// Read-only facts of every installed unit carrying a `tool` manifest in
    /// its declared configuration (`kind` is `Code`). Used by the named-WASM
    /// tool surface (`shenko tool`, `wasi.exec`): installation authority,
    /// versioning and replication stay here; the tool layer adds name/ABI
    /// resolution on top of these facts. Pure query — no schema creation, no
    /// trial reconciliation, no materialization.
    ///
    /// `selected` is the only field a name-only lookup may use. When an
    /// environment generation is active it is that generation's pin; when no
    /// generation exists yet the installation is honestly unpinned and the
    /// unit-current row is the selection.
    pub fn tool_unit_facts(&self) -> Result<Vec<ToolUnitFact>> {
        let selector = read_environment_selector(&self.conn)?;
        let pins: Vec<EnvironmentMember> = match &selector {
            Some(selector) => self.environment_members(selector.generation)?,
            None => Vec::new(),
        };
        let rows: Vec<(String, bool, bool, Option<String>, Option<String>)> = self
            .conn
            .prepare(
                "SELECT v.data,c.enabled,(c.revision=v.revision AND c.artifact_digest=v.artifact_digest),m.root,m.path \
                 FROM unit_versions v JOIN unit_current c ON c.unit_id=v.unit_id \
                 LEFT JOIN unit_materializations m ON m.unit_id=v.unit_id AND m.revision=v.revision AND m.artifact_digest=v.artifact_digest \
                 ORDER BY v.unit_id,v.revision,v.artifact_digest"
            )?
            .query_map([], |r| Ok((
                r.get::<_, String>(0)?, r.get::<_, bool>(1)?, r.get::<_, bool>(2)?,
                r.get::<_, Option<String>>(3)?, r.get::<_, Option<String>>(4)?,
            )))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut facts = Vec::new();
        for (data, enabled, current, root, path) in rows {
            let stored: StoredVersion = serde_json::from_str(&data)?;
            if stored.unit.kind != UnitKind::Code {
                continue;
            }
            let Some(tool) = stored.unit.config.get("tool").cloned() else {
                continue;
            };
            let pin = match &selector {
                Some(selector) => pins
                    .iter()
                    .find(|member| {
                        member.unit_id == stored.unit.unit_id
                            && member.revision == stored.unit.revision
                            && member.artifact_digest == stored.unit.artifact_digest
                    })
                    .map(|member| EnvironmentPin {
                        generation: selector.generation,
                        generation_digest: selector.generation_digest.clone(),
                        selector_revision: selector.revision,
                        role: member.role,
                        declarations: member.declarations.clone(),
                    }),
                None => None,
            };
            let selected = if selector.is_some() { pin.is_some() } else { current };
            facts.push(ToolUnitFact {
                unit_id: stored.unit.unit_id,
                kind: stored.unit.kind,
                revision: stored.unit.revision,
                artifact_digest: stored.unit.artifact_digest,
                enabled,
                current,
                selected,
                environment: pin,
                tool,
                materialization_root: root,
                materialization_path: path,
            });
        }
        Ok(facts)
    }

    // ---------------------------------------------------- environment -----

    /// The active environment selector, if any. Read-only: an installation
    /// that never activated a generation reports `None` and is not mutated.
    pub fn environment_selector(&self) -> Result<Option<EnvironmentSelector>> {
        read_environment_selector(&self.conn)
    }

    /// One recorded generation, exactly as it was prepared.
    pub fn environment_generation(
        &self,
        generation: u64,
    ) -> Result<Option<EnvironmentGeneration>> {
        read_environment_generation(&self.conn, generation)
    }

    /// Recorded generations, oldest first.
    pub fn environment_generations(&self, limit: u32) -> Result<Vec<EnvironmentGeneration>> {
        ensure!((1..=512).contains(&limit), "environment limit 1..=512");
        environment_documents(&self.conn, ENVIRONMENT_GENERATION_GLOB, limit)?
            .into_iter()
            .map(|(_, data)| environment_generation_document(&data))
            .collect()
    }

    /// The members of one generation, ordered by unit id.
    pub fn environment_members(&self, generation: u64) -> Result<Vec<EnvironmentMember>> {
        Ok(self
            .environment_generation(generation)?
            .map(|record| record.members)
            .unwrap_or_default())
    }

    /// The append-only activation log, oldest first.
    pub fn environment_activations(&self, limit: u32) -> Result<Vec<Value>> {
        ensure!((1..=512).contains(&limit), "environment limit 1..=512");
        environment_documents(&self.conn, ENVIRONMENT_ACTIVATION_GLOB, limit)?
            .into_iter()
            .map(|(_, data)| Ok(serde_json::from_str::<Value>(&data)?))
            .collect()
    }

    /// Resolve the transitive closure of these roots against the versions and
    /// selections this node actually retains, and record it as a locked
    /// generation. Nothing is selected by preparing.
    fn prepare_environment(&mut self, roots: &[EnvironmentRoot]) -> Result<Value> {
        let (root_ids, members) = self.plan_environment(roots)?;
        let digest =
            environment_closure_digest(&root_ids, &members, ENVIRONMENT_CLOSURE_VERSION)?;
        // A closure identical to the active environment is not a new
        // candidate: the selector is not advanced and no generation row is
        // written, so repeating an install does not churn generations.
        if let Some(selector) = self.environment_selector()? {
            if selector.generation_digest == digest {
                let existing = self
                    .environment_generation(selector.generation)?
                    .context("the active environment generation is not recorded")?;
                return Ok(json!({
                    "generation": existing,
                    "unchanged": true,
                    "note": "this closure is already the active environment; the selector was not advanced",
                }));
            }
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Generations are append-only documents: the next number is one past
        // the highest recorded one, read inside this write transaction.
        let generation = latest_environment_generation(&tx)?.map_or(1, |latest| latest + 1);
        let record = EnvironmentGeneration {
            schema: UNIT_ENVIRONMENT_SCHEMA,
            closure_version: ENVIRONMENT_CLOSURE_VERSION,
            generation,
            digest,
            roots: root_ids,
            members,
            created_at: now(),
        };
        crate::set(&tx, &environment_generation_key(generation), &record)?;
        tx.commit()?;
        Ok(json!({
            "generation": record,
            "unchanged": false,
            "note": "prepared only; nothing becomes current until the selector is advanced",
        }))
    }

    /// Publish a prepared generation as the current environment through one
    /// conditional update of the selector revision.
    fn activate_environment(&mut self, generation: u64, expected_revision: u64) -> Result<Value> {
        let record = self.environment_generation(generation)?.with_context(|| {
            format!("UNIT_ENVIRONMENT_UNKNOWN: generation {generation} was never prepared on this node")
        })?;
        // A generation is immutable: its document must still hash to its own
        // digest. A disagreement is corruption, not a new closure.
        ensure!(
            environment_closure_digest(
                &record.roots,
                &record.members,
                record.closure_version
            )? == record.digest,
            "UNIT_ENVIRONMENT_CORRUPT: generation {generation} does not hash to its recorded digest"
        );
        // The closure must still be satisfiable here: every required
        // declaration lands on this environment's pin and every pinned unit's
        // own bytes are held. Activation never publishes a closure the node
        // cannot actually supply.
        let problems = self.environment_closure_problems(&record.members)?;
        ensure!(problems.is_empty(), "{}", environment_problems(problems));
        for member in &record.members {
            self.ensure_environment_member_bytes(member)?;
        }
        // Retained versions, resolved selections and stored unit bytes are
        // append-only facts, so the verification above cannot be invalidated
        // by a concurrent import before this write. The compare-and-swap is
        // the linearization point of the activation itself.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated_at = now();
        // The selector document is rewritten only when it still carries the
        // revision the caller read: read and write happen in one immediate
        // transaction, so a concurrent activation either wins the whole
        // transaction or is refused here. The activation log entry lands in
        // the same transaction as the selector it describes.
        let (previous, revision) = match read_environment_selector(&tx)? {
            Some(selector) => {
                ensure!(
                    selector.revision == expected_revision,
                    "UNIT_ENVIRONMENT_CONFLICT: the environment selector changed while activating (now revision {}, caller expected {expected_revision})",
                    selector.revision
                );
                ensure!(
                    selector.generation != generation,
                    "UNIT_ENVIRONMENT_ALREADY_ACTIVE: generation {generation} is the current environment; nothing was changed"
                );
                (Some(selector.generation), expected_revision + 1)
            }
            None => {
                ensure!(
                    expected_revision == 0,
                    "UNIT_ENVIRONMENT_CONFLICT: this installation has no environment selector; the expected revision must be 0"
                );
                (None, 1)
            }
        };
        let selector = EnvironmentSelector {
            schema: UNIT_ENVIRONMENT_SCHEMA,
            generation,
            generation_digest: record.digest.clone(),
            revision,
            previous_generation: previous,
            updated_at: updated_at.clone(),
        };
        crate::set(&tx, ENVIRONMENT_SELECTOR_KEY, &selector)?;
        crate::set(
            &tx,
            &environment_activation_key(revision),
            &json!({
                "revision": revision,
                "generation": generation,
                "generation_digest": &record.digest.0,
                "previous_generation": previous,
                "activated_at": updated_at,
            }),
        )?;
        // Publishing the environment publishes which version each unit id
        // currently names, in the same transaction: the selector and the
        // per-unit current pointers cannot disagree after an activation.
        // `enabled` stays an independent installation fact, so publishing a
        // selection never enables or disables anything.
        for member in &record.members {
            let enabled: Option<i64> = tx
                .query_row(
                    "SELECT enabled FROM unit_current WHERE unit_id=?1",
                    [&member.unit_id],
                    |r| r.get(0),
                )
                .optional()?;
            tx.execute(
                "INSERT INTO unit_current(unit_id,revision,artifact_digest,enabled,updated_at) \
                 VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id) DO UPDATE SET \
                 revision=excluded.revision,artifact_digest=excluded.artifact_digest,\
                 updated_at=excluded.updated_at",
                params![
                    &member.unit_id,
                    &member.revision,
                    &member.artifact_digest.0,
                    enabled.unwrap_or(1),
                    &updated_at
                ],
            )?;
        }
        tx.commit()?;
        Ok(json!({
            "selector": selector,
            "generation": record,
            "activated": true,
        }))
    }

    /// Select the previous generation of the current environment through the
    /// same compare-and-swap. Retained versions, earlier generations and every
    /// other installation fact stay untouched: a rollback changes the
    /// selection, never the history.
    fn rollback_environment(&mut self, expected_revision: u64) -> Result<Value> {
        let selector = self.environment_selector()?.context(
            "UNIT_ENVIRONMENT_NONE: this installation has no active environment; prepare and activate one first",
        )?;
        let previous = selector.previous_generation.context(
            "UNIT_ENVIRONMENT_NO_PREVIOUS: the current environment records no previous generation; activate the target generation explicitly",
        )?;
        let mut outcome = self.activate_environment(previous, expected_revision)?;
        if let Some(object) = outcome.as_object_mut() {
            object.insert("rolled_back_from".into(), json!(selector.generation));
            object.insert("rollback".into(), json!(true));
        }
        Ok(outcome)
    }

    /// The complete environment projection: the selector, the locked closure
    /// of the current generation (with the selections it froze beside the live
    /// unit rows), every recorded generation and the activation log. Pure
    /// read; a read-only inspection can call it.
    pub fn environment_report(&self, limit: u32) -> Result<Value> {
        let selector = self.environment_selector()?;
        let members = match &selector {
            Some(selector) => self.environment_members(selector.generation)?,
            None => Vec::new(),
        };
        // A retained unit version's selections can be re-resolved after a
        // generation was locked. The generation's own copies stay the
        // authority for its closure; reporting the live rows beside them keeps
        // that divergence visible instead of pretending the unit record
        // rewrote history.
        let mut closure = Vec::with_capacity(members.len());
        let mut diverged: Vec<String> = Vec::new();
        let mut host_requirements: Vec<HostRequirement> = Vec::new();
        for member in members {
            let live = self.installed_unit_version(
                &member.unit_id,
                &member.revision,
                &member.artifact_digest,
            )?;
            let live_declarations = match &live {
                Some(unit) => declared_package_capabilities(&unit.descriptor)?,
                None => PackageDeclaration::unrecorded(),
            };
            let edges_agree = live
                .as_ref()
                .is_some_and(|unit| unit.edges == member.edges);
            let declarations_agree = live_declarations == member.declarations;
            if !edges_agree || !declarations_agree {
                diverged.push(member.unit_id.clone());
            }
            if let Some(capabilities) = member.declarations.capabilities() {
                host_requirements.extend(capabilities.host.iter().cloned());
            }
            closure.push(json!({
                "unit_id": member.unit_id,
                "revision": member.revision,
                "artifact_digest": member.artifact_digest,
                "role": member.role.as_str(),
                "locked_edges": member.edges,
                "live_edges": live.as_ref().map(|unit| unit.edges.clone()).unwrap_or_default(),
                "live_edges_agree": edges_agree,
                "declarations": declaration_document(&member.declarations),
                "live_declarations": declaration_document(&live_declarations),
                "live_declarations_agree": declarations_agree,
            }));
        }
        host_requirements.sort();
        host_requirements.dedup();
        Ok(json!({
            "pinned": selector.is_some(),
            "selector": selector,
            "closure": closure,
            // Which copy of the closure decides what a run depends on: the
            // documents the generation froze. A reader that re-resolves
            // dependencies from live unit rows can silently change the meaning
            // of an environment that is already published.
            "execution_authority": "generation.locked_edges_and_declarations",
            "diverged": diverged,
            "host_requirements": host_requirements,
            "generations": self.environment_generations(limit)?,
            "activations": self.environment_activations(limit)?,
            "note": "the selector is the only authority for which generation is current; an activation or a rollback never deletes a retained version or a prior generation",
        }))
    }

    /// Follow one dependency declaration of one member *inside a locked
    /// generation*, using only the selections and members that generation
    /// froze.
    ///
    /// This is the execution authority for an environment's dependency
    /// meaning: a later `resolve_version` on the retained unit row changes
    /// neither this answer nor the version a selected environment pins. The
    /// returned member is the whole frozen identity — revision, artifact
    /// digest, locked edges and declarations — never a mutable live row.
    pub fn environment_locked_selection(
        &self,
        generation: u64,
        unit_id: &str,
        edge_id: &str,
    ) -> Result<Option<EnvironmentMember>> {
        let Some(record) = self.environment_generation(generation)? else {
            return Ok(None);
        };
        let Some(member) = record
            .members
            .iter()
            .find(|member| member.unit_id == unit_id)
        else {
            return Ok(None);
        };
        let Some(edge) = member
            .edges
            .iter()
            .find(|edge| edge.edge_id == edge_id)
        else {
            return Ok(None);
        };
        let Some((selected_unit, revision, digest)) = selection_of(Some(edge)) else {
            return Ok(None);
        };
        Ok(record.members.into_iter().find(|member| {
            member.unit_id == selected_unit
                && member.revision == revision
                && member.artifact_digest == digest
        }))
    }

    /// The member one *active* environment pins for this unit, if any. Read
    /// only from the generation the selector names; a version retained outside
    /// that generation is not a member and resolves to `None`.
    pub fn active_environment_member(&self, unit_id: &str) -> Result<Option<EnvironmentMember>> {
        let Some(selector) = self.environment_selector()? else {
            return Ok(None);
        };
        Ok(self
            .environment_members(selector.generation)?
            .into_iter()
            .find(|member| member.unit_id == unit_id))
    }

    // --------------------------------------------- metapackage/distribution queries -----

    /// Walk a metapackage's declared members, resolve each against installed
    /// units, and check adoption via the installation's own adoption records.
    ///
    /// A member that is content (perception, awareness, skill) **must** carry
    /// an authenticated adoption attitude whose evidence verifies locally
    /// (SPEC §51.1, §51.3, §51.8). Code members need installation only.
    /// A member installed on a peer never joins locally: declarations crossing
    /// nodes never imply local readiness.
    fn query_metapackage(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {unit_id} is not installed on this node"))?;
        let body = declared_metapackage(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_METAPACKAGE: {unit_id} does not declare a metapackage body"))?;

        let adoption_projection = UnitAdoptionProjection::new(&self.conn);
        let mut memberships: Vec<MembershipResolution> = Vec::new();
        let mut slots: Vec<MacroSlot> = Vec::new();
        let mut contract_providers: BTreeMap<String, Vec<MemberSelection>> = BTreeMap::new();

        for spec in &body.members {
            let resolution = self.resolve_macro_member(
                unit_id,
                spec,
                &adoption_projection,
            )?;
            // Track providers for named contract slots.
            if let Some(contract) = &spec.contract {
                if let (Some(revision), Some(digest)) = (
                    resolution.selected_revision.as_ref(),
                    resolution.selected_artifact_digest.as_ref(),
                ) {
                    let selection = MemberSelection {
                        unit_id: spec.member_id.clone(),
                        revision: revision.clone(),
                        artifact_digest: digest.clone(),
                        member_kind: resolution.member_kind.unwrap_or(UnitKind::Code),
                        adoption: resolution.adoption.clone(),
                        enabled: resolution.enabled,
                        readiness: resolution.readiness,
                    };
                    contract_providers
                        .entry(contract.text())
                        .or_default()
                        .push(selection);
                }
            }
            memberships.push(resolution);
        }

        // Build named slots from the collected providers.
        for (contract_text, providers) in &contract_providers {
            let contract = CapabilityContract::parse(contract_text)?;
            let slot = self.build_macro_slot(unit_id, &contract, providers)?;
            slots.push(slot);
        }

        Ok(json!({
            "unit_id": unit_id,
            "kind": "metapackage",
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "members": memberships,
            "slots": slots,
            "member_count": body.members.len(),
        }))
    }

    /// Resolve one metapackage member spec against installed/retained units.
    fn resolve_macro_member(
        &self,
        macro_id: &str,
        spec: &MacroMemberSpec,
        adoption: &dyn MacroContentAdoption,
    ) -> Result<MembershipResolution> {
        // Look up the member as an installed unit.
        let installed = self.installed_unit(&spec.member_id)?;
        let Some(unit) = installed else {
            return Ok(MembershipResolution {
                macro_id: macro_id.to_string(),
                path: vec![],
                member_id: spec.member_id.clone(),
                contract: spec.contract.clone(),
                status: MembershipStatus::Absent,
                member_kind: None,
                selected_revision: None,
                selected_artifact_digest: None,
                adoption: None,
                enabled: false,
                readiness: ReadinessState::NotInstalled,
                reason: format!("{} is not installed on this node", spec.member_id),
            });
        };

        // Check revision requirement if the spec pins one.
        if let Some(requirement) = &spec.revision_requirement {
            if !revision_matches(requirement, &unit.revision) {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: spec.contract.clone(),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "installed revision {} does not satisfy requirement {requirement}",
                        unit.revision
                    ),
                });
            }
        }

        // Check artifact digest if pinned.
        if let Some(expected) = &spec.artifact_digest {
            if &unit.artifact_digest != expected {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: spec.contract.clone(),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "installed artifact digest does not match the pinned value"
                    ),
                });
            }
        }

        // Check contract: if the spec names one, the member must declare
        // providing it.
        if let Some(contract) = &spec.contract {
            if !unit.descriptor.provides_contract(contract) {
                return Ok(MembershipResolution {
                    macro_id: macro_id.to_string(),
                    path: vec![],
                    member_id: spec.member_id.clone(),
                    contract: Some(contract.clone()),
                    status: MembershipStatus::Unresolved,
                    member_kind: Some(unit.descriptor.kind),
                    selected_revision: Some(unit.revision.clone()),
                    selected_artifact_digest: Some(unit.artifact_digest.clone()),
                    adoption: None,
                    enabled: unit.enabled,
                    readiness: ReadinessState::Unresolved,
                    reason: format!(
                        "{} does not declare providing {}",
                        spec.member_id,
                        contract
                    ),
                });
            }
        }

        // Adoption check: content kinds require authenticated adoption.
        let needs_adoption = unit.descriptor.kind.is_content()
            || spec.adoption == MemberAdoption::Attested;
        let adoption_result = if needs_adoption {
            adoption.adoption(
                &spec.member_id,
                &unit.revision,
                &unit.artifact_digest,
            )?
        } else {
            None
        };

        let (status, reason) = if needs_adoption {
            match &adoption_result {
                None => (
                    MembershipStatus::Unadopted,
                    format!(
                        "{} is content or attested; no adoption attitude found",
                        spec.member_id
                    ),
                ),
                Some(ca) if !ca.accepts(&unit.artifact_digest) => {
                    let refusal = ca
                        .refusal(&unit.artifact_digest)
                        .unwrap_or_else(|| "adoption does not accept this revision".into());
                    (MembershipStatus::Unadopted, refusal)
                }
                Some(_) => {
                    let (readiness, reason) = self.readiness_of(&unit)?;
                    if readiness == ReadinessState::Ready {
                        (
                            MembershipStatus::Joined,
                            format!("{} is adopted and ready", spec.member_id),
                        )
                    } else {
                        (
                            MembershipStatus::CodeInstalled,
                            format!("{} is adopted but readiness is {:?}: {reason}", spec.member_id, readiness),
                        )
                    }
                }
            }
        } else {
            // Code member: installation is sufficient, no adoption needed.
            let (readiness, reason) = self.readiness_of(&unit)?;
            if readiness == ReadinessState::Ready {
                (
                    MembershipStatus::Joined,
                    format!("{} is installed and ready", spec.member_id),
                )
            } else {
                (
                    MembershipStatus::CodeInstalled,
                    format!("{} is installed but readiness is {:?}: {reason}", spec.member_id, readiness),
                )
            }
        };

        let (readiness, _) = self.readiness_of(&unit)?;

        Ok(MembershipResolution {
            macro_id: macro_id.to_string(),
            path: vec![],
            member_id: spec.member_id.clone(),
            contract: spec.contract.clone(),
            status,
            member_kind: Some(unit.descriptor.kind),
            selected_revision: Some(unit.revision.clone()),
            selected_artifact_digest: Some(unit.artifact_digest.clone()),
            adoption: adoption_result,
            enabled: unit.enabled,
            readiness,
            reason,
        })
    }

    /// Build a named contract slot from the collected providers.
    ///
    /// Multiple admitted providers coexist when their actually used atomic
    /// capabilities do not conflict (SPEC §51.8). Selection: the first
    /// admitted and enabled provider is the selected one; a slot where no
    /// provider is admitted stays unselected, and a consumer reports that
    /// omission rather than claiming a rule was applied.
    fn build_macro_slot(
        &self,
        macro_id: &str,
        contract: &CapabilityContract,
        providers: &[MemberSelection],
    ) -> Result<MacroSlot> {
        let admitted: Vec<&MemberSelection> = providers
            .iter()
            .filter(|provider| {
                provider
                    .adoption
                    .as_ref()
                    .is_none_or(|ca| ca.accepts(&provider.artifact_digest))
            })
            .collect();

        // Check coexistence: admitted providers whose used atomic capabilities
        // conflict cannot coexist.
        let mut coexisting = admitted.len() > 1;
        let mut conflict: Option<String> = None;
        if admitted.len() > 1 {
            for (i, a) in admitted.iter().enumerate() {
                for b in admitted.iter().skip(i + 1) {
                    let a_atoms = self.provider_atoms(&a.unit_id, &a.revision, &a.artifact_digest)?;
                    let b_atoms = self.provider_atoms(&b.unit_id, &b.revision, &b.artifact_digest)?;
                    if let Some(reason) = atomic_conflict(&a_atoms, &b_atoms) {
                        coexisting = false;
                        conflict = Some(format!(
                            "{} and {} conflict: {reason}",
                            a.unit_id, b.unit_id
                        ));
                        break;
                    }
                }
                if conflict.is_some() {
                    break;
                }
            }
        }

        // Select: first admitted + enabled provider.
        let selected = providers.iter().position(|provider| {
            provider.enabled
                && provider
                    .adoption
                    .as_ref()
                    .is_none_or(|ca| ca.accepts(&provider.artifact_digest))
        });

        Ok(MacroSlot {
            macro_id: macro_id.to_string(),
            contract: contract.clone(),
            providers: providers.to_vec(),
            selected,
            coexisting,
            conflict,
        })
    }

    /// Read the atomic capabilities a version declares.
    fn provider_atoms(
        &self,
        unit_id: &str,
        revision: &str,
        digest: &Digest,
    ) -> Result<Vec<AtomicCapability>> {
        let fact = self.retained_unit_fact(unit_id, revision, digest)?;
        match fact {
            Some(f) => {
                let decl = declared_package_capabilities(&f.descriptor)?;
                Ok(decl
                    .capabilities()
                    .map(|c| c.normalized().atoms)
                    .unwrap_or_default())
            }
            None => Ok(vec![]),
        }
    }

    /// Walk a distribution's declarations and return the complete projection.
    ///
    /// Target-access expectations replace the kernel's `NAMED_TARGETS` const
    /// table (SPEC §51.3). Preinstalled macros name the metapackages the
    /// distribution ships by design. Capability-provider defaults name the
    /// replaceable provider selections. All are declarations — facts about the
    /// distribution version, never grants of local readiness.
    fn query_distribution(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {unit_id} is not installed on this node"))?;
        let body = declared_distribution(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_DISTRIBUTION: {unit_id} does not declare a distribution body"))?;

        let declared_by = DeclaredBy {
            unit_id: unit.unit_id.clone(),
            revision: unit.revision.clone(),
            artifact_digest: unit.artifact_digest.clone(),
        };

        // Each target-access expectation carries its provenance.
        let mut targets: Vec<Value> = Vec::new();
        for expectation in &body.targets {
            let access = DeclaredTargetAccess {
                expectation: expectation.clone(),
                declared_by: declared_by.clone(),
            };
            targets.push(serde_json::to_value(&access)?);
        }

        // Resolve preinstalled macros.
        let mut macros: Vec<Value> = Vec::new();
        for macro_spec in &body.macros {
            let installed = self.installed_unit(&macro_spec.unit_id)?.is_some();
            let is_metapackage = self
                .installed_unit(&macro_spec.unit_id)?
                .is_some_and(|u| u.descriptor.kind == UnitKind::Metapackage);
            macros.push(json!({
                "macro_id": macro_spec.unit_id,
                "parallel": macro_spec.parallel,
                "design_dependency": macro_spec.design_dependency,
                "installed": installed,
                "is_metapackage": is_metapackage,
            }));
        }

        // Resolve capability-provider defaults.
        let mut capability_defaults: Vec<Value> = Vec::new();
        for default in &body.capability_providers {
            let provider_installed = self.installed_unit(&default.unit_id)?.is_some();
            let provides = self
                .installed_unit(&default.unit_id)?
                .is_some_and(|u| u.descriptor.provides_contract(&default.contract));
            capability_defaults.push(json!({
                "contract": default.contract.text(),
                "provider_unit_id": default.unit_id,
                "installed": provider_installed,
                "declares_providing": provides,
            }));
        }

        Ok(json!({
            "unit_id": unit_id,
            "kind": "distribution",
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest,
            "targets": targets,
            "macros": macros,
            "capability_providers": capability_defaults,
        }))
    }

    /// Resolve one named capability-contract slot across installed providers.
    fn query_slot(&self, macro_id: &str, contract_text: &str) -> Result<Value> {
        let contract = CapabilityContract::parse(contract_text)?;
        let unit = self
            .installed_unit(macro_id)?
            .with_context(|| format!("UNIT_NOT_FOUND: {macro_id} is not installed on this node"))?;
        let body = declared_metapackage(&unit.descriptor)?
            .with_context(|| format!("UNIT_NOT_METAPACKAGE: {macro_id} does not declare a metapackage body"))?;

        // Collect members that name this contract.
        let adoption_projection = UnitAdoptionProjection::new(&self.conn);
        let mut providers: Vec<MemberSelection> = Vec::new();
        for spec in &body.members {
            if spec.contract.as_ref() != Some(&contract) {
                continue;
            }
            let installed = self.installed_unit(&spec.member_id)?;
            let Some(member_unit) = installed else {
                continue;
            };
            let needs_adoption = member_unit.descriptor.kind.is_content()
                || spec.adoption == MemberAdoption::Attested;
            let adoption_result = if needs_adoption {
                adoption_projection.adoption(
                    &spec.member_id,
                    &member_unit.revision,
                    &member_unit.artifact_digest,
                )?
            } else {
                None
            };
            let (readiness, _) = self.readiness_of(&member_unit)?;
            providers.push(MemberSelection {
                unit_id: spec.member_id.clone(),
                revision: member_unit.revision.clone(),
                artifact_digest: member_unit.artifact_digest.clone(),
                member_kind: member_unit.descriptor.kind,
                adoption: adoption_result,
                enabled: member_unit.enabled,
                readiness,
            });
        }

        let slot = self.build_macro_slot(macro_id, &contract, &providers)?;
        Ok(serde_json::to_value(slot)?)
    }

    /// Walk the retained, resolved dependency closure of the requested roots.
    ///
    /// Only selections this node actually recorded are followed: an edge the
    /// inventory left unresolved is a refusal, not something the planner
    /// guesses at. Recommended dependencies join the environment when they
    /// resolve and are dropped when they do not.
    fn plan_environment(
        &self,
        roots: &[EnvironmentRoot],
    ) -> Result<(Vec<String>, Vec<EnvironmentMember>)> {
        ensure!(
            !roots.is_empty(),
            "UNIT_ENVIRONMENT_EMPTY: an environment needs at least one root unit"
        );
        ensure!(
            roots.len() <= MAX_ENVIRONMENT_ROOTS,
            "UNIT_ENVIRONMENT_ROOTS: at most {MAX_ENVIRONMENT_ROOTS} roots"
        );
        let mut root_ids: Vec<String> = Vec::new();
        let mut queue: Vec<(InstalledUnit, EnvironmentRole)> = Vec::with_capacity(roots.len());
        for root in roots {
            let unit = self.root_environment_version(root)?;
            ensure!(
                !root_ids.contains(&unit.unit_id),
                "UNIT_ENVIRONMENT_DUPLICATE_ROOT: {} is requested twice",
                unit.unit_id
            );
            root_ids.push(unit.unit_id.clone());
            queue.push((unit, EnvironmentRole::Root));
        }
        root_ids.sort();
        let mut members: Vec<EnvironmentMember> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let mut index = 0usize;
        while index < queue.len() {
            ensure!(
                members.len() <= MAX_ENVIRONMENT_MEMBERS,
                "UNIT_ENVIRONMENT_TOO_LARGE: the closure exceeds {MAX_ENVIRONMENT_MEMBERS} members"
            );
            let (unit, role) = queue[index].clone();
            index += 1;
            // A unit reached twice is resolved by the second pass below: an
            // explicit root pin outranks an older recorded selection, and the
            // closure check afterwards is the authoritative verdict.
            if members.iter().any(|member| member.unit_id == unit.unit_id) {
                continue;
            }
            members.push(EnvironmentMember {
                unit_id: unit.unit_id.clone(),
                revision: unit.revision.clone(),
                artifact_digest: unit.artifact_digest.clone(),
                role,
                edges: unit.edges.clone(),
                declarations: declared_package_capabilities(&unit.descriptor)?,
            });
            for spec in &unit.descriptor.dependencies {
                let edge = unit.edges.iter().find(|edge| edge_matches_spec(edge, spec));
                let selected = selection_of(edge);
                let dependency = match selected {
                    Some((unit_id, revision, digest)) => {
                        self.installed_unit_version(&unit_id, &revision, &digest)?
                    }
                    None => None,
                };
                let Some(dependency) = dependency else {
                    if spec.strength == DependencyStrength::Required {
                        let reason = edge
                            .map(|edge| edge.reason.clone())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| {
                                "no retained, resolved selection exists for this declaration"
                                    .to_string()
                            });
                        problems.push(format!(
                            "TOOL_DEPENDENCY_MISSING: {} revision {} edge {}: {}; resolve this exact version first",
                            unit.unit_id, unit.revision, spec.edge_id, reason
                        ));
                    }
                    continue;
                };
                if !selection_matches(
                    spec,
                    &dependency.unit_id,
                    &dependency.revision,
                    &dependency.artifact_digest,
                    &dependency.descriptor.provides,
                ) {
                    if spec.strength == DependencyStrength::Required {
                        problems.push(format!(
                            "TOOL_DEPENDENCY_MISSING: {} revision {} edge {} keeps a selection that no longer satisfies its declaration",
                            unit.unit_id, unit.revision, spec.edge_id
                        ));
                    }
                    continue;
                }
                queue.push((dependency, EnvironmentRole::Dependency));
            }
        }
        // Second pass: an explicit root pin outranks a selection recorded
        // earlier, so a required declaration is re-solved onto the version
        // this environment actually pins — but only onto a member that still
        // satisfies the declaration. Nothing is invented: a declaration no
        // member satisfies keeps its recorded selection, and the closure check
        // below reports it.
        for index in 0..members.len() {
            let specs = {
                let member = &members[index];
                let Some(unit) = self.installed_unit_version(
                    &member.unit_id,
                    &member.revision,
                    &member.artifact_digest,
                )?
                else {
                    continue;
                };
                unit.descriptor.dependencies
            };
            for spec in specs
                .iter()
                .filter(|spec| spec.strength == DependencyStrength::Required)
            {
                let member = &members[index];
                let current = member
                    .edges
                    .iter()
                    .find(|edge| edge_matches_spec(edge, spec))
                    .and_then(|edge| selection_of(Some(edge)));
                let already_pinned = current.as_ref().is_some_and(
                    |(unit_id, revision, digest)| {
                        members.iter().any(|other| {
                            &other.unit_id == unit_id
                                && &other.revision == revision
                                && &other.artifact_digest == digest
                        })
                    },
                );
                if already_pinned {
                    continue;
                }
                let mut best: Option<(String, String, Digest)> = None;
                for candidate in &members {
                    let Some(candidate_unit) = self.installed_unit_version(
                        &candidate.unit_id,
                        &candidate.revision,
                        &candidate.artifact_digest,
                    )?
                    else {
                        continue;
                    };
                    if !selection_matches(
                        spec,
                        &candidate_unit.unit_id,
                        &candidate_unit.revision,
                        &candidate_unit.artifact_digest,
                        &candidate_unit.descriptor.provides,
                    ) {
                        continue;
                    }
                    let better = match &best {
                        None => true,
                        Some(previous) => {
                            compare_versions(&candidate.revision, &previous.1) == Ordering::Greater
                        }
                    };
                    if better {
                        best = Some((
                            candidate.unit_id.clone(),
                            candidate.revision.clone(),
                            candidate.artifact_digest.clone(),
                        ));
                    }
                }
                let Some((unit_id, revision, digest)) = best else {
                    continue;
                };
                let member = &mut members[index];
                if let Some(edge) = member
                    .edges
                    .iter_mut()
                    .find(|edge| edge_matches_spec(&**edge, spec))
                {
                    edge.status = EdgeStatus::Satisfied;
                    edge.selected_unit_id = Some(unit_id);
                    edge.selected_revision = Some(revision);
                    edge.selected_artifact_digest = Some(digest);
                    edge.reason =
                        "re-solved to the version this environment pins".to_string();
                }
            }
        }
        problems.extend(self.environment_closure_problems(&members)?);
        if !problems.is_empty() {
            bail!("{}", environment_problems(problems));
        }
        members.sort_by(|left, right| left.unit_id.cmp(&right.unit_id));
        Ok((root_ids, members))
    }

    fn root_environment_version(&self, root: &EnvironmentRoot) -> Result<InstalledUnit> {
        let unit = match (&root.revision, &root.artifact_digest) {
            (Some(revision), Some(digest)) => {
                self.installed_unit_version(&root.unit_id, revision, digest)?
            }
            (Some(revision), None) => {
                let retained: Vec<InstalledUnit> = self
                    .retained_unit_versions(&root.unit_id)?
                    .into_iter()
                    .filter(|unit| unit.revision.as_str() == revision.as_str())
                    .collect();
                ensure!(
                    retained.len() <= 1,
                    "UNIT_ENVIRONMENT_AMBIGUOUS: {} revision {} is retained with {} different artifact digests; pin one by digest",
                    root.unit_id,
                    revision,
                    retained.len()
                );
                retained.into_iter().next()
            }
            (None, None) => self.installed_unit(&root.unit_id)?,
            (None, Some(_)) => bail!(
                "UNIT_ENVIRONMENT_INVALID: {} pins an artifact digest without a revision",
                root.unit_id
            ),
        };
        unit.with_context(|| {
            format!(
                "UNIT_ENVIRONMENT_ROOT_MISSING: {} is not retained on this node",
                root.unit_id
            )
        })
    }

    /// Every required declaration of every member must land on exactly the pin
    /// this environment records. Preparation and activation share this
    /// predicate, so a prepared closure is never activated under a weaker
    /// check than the one that admitted it.
    fn environment_closure_problems(
        &self,
        members: &[EnvironmentMember],
    ) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        for member in members {
            // The frozen declaration is admitted here as well: preparation and
            // activation share this predicate, so a generation can never be
            // published under a weaker declaration check than the one that
            // prepared it, and a malformed declaration names its member
            // instead of surfacing later inside a guest run.
            if let Some(capabilities) = member.declarations.capabilities() {
                if let Err(error) = capabilities.validate() {
                    problems.push(format!(
                        "{} revision {}: {error:#}",
                        member.unit_id, member.revision
                    ));
                }
            }
            let Some(unit) = self.installed_unit_version(
                &member.unit_id,
                &member.revision,
                &member.artifact_digest,
            )? else {
                problems.push(format!(
                    "TOOL_GENERATION_STALE: {} revision {} ({}) is no longer retained on this node",
                    member.unit_id, member.revision, member.artifact_digest
                ));
                continue;
            };
            // The declarations are the unit version's own; the *selections*
            // are the ones this generation locked. A later re-resolution of
            // the retained unit version does not rewrite this closure.
            for spec in unit
                .descriptor
                .dependencies
                .iter()
                .filter(|spec| spec.strength == DependencyStrength::Required)
            {
                let edge = member.edges.iter().find(|edge| edge_matches_spec(edge, spec));
                let Some((unit_id, revision, digest)) = selection_of(edge) else {
                    problems.push(format!(
                        "TOOL_DEPENDENCY_MISSING: {} revision {} edge {} has no retained, satisfied selection",
                        member.unit_id, member.revision, spec.edge_id
                    ));
                    continue;
                };
                let pinned = members.iter().find(|other| other.unit_id == unit_id);
                match pinned {
                    Some(pinned)
                        if pinned.revision == revision && pinned.artifact_digest == digest => {}
                    Some(pinned) => {
                        problems.push(format!(
                            "TOOL_DEPENDENCY_CONFLICT: {} revision {} requires {} revision {} ({}) but the environment pins {} ({})",
                            member.unit_id,
                            member.revision,
                            unit_id,
                            revision,
                            digest,
                            pinned.revision,
                            pinned.artifact_digest
                        ));
                        continue;
                    }
                    None => {
                        problems.push(format!(
                            "TOOL_DEPENDENCY_MISSING: {} revision {} requires {} revision {} which the environment does not contain",
                            member.unit_id, member.revision, unit_id, revision
                        ));
                        continue;
                    }
                }
                // A capability selector must still be satisfied by the
                // dependency's actual `provides`, not only by its identity.
                let Some(dependency) = self.installed_unit_version(&unit_id, &revision, &digest)?
                else {
                    problems.push(format!(
                        "TOOL_GENERATION_STALE: {} revision {} ({}) is no longer retained on this node",
                        unit_id, revision, digest
                    ));
                    continue;
                };
                if !selection_matches(
                    spec,
                    &dependency.unit_id,
                    &dependency.revision,
                    &dependency.artifact_digest,
                    &dependency.descriptor.provides,
                ) {
                    problems.push(format!(
                        "TOOL_DEPENDENCY_MISSING: {} revision {} edge {} selects {} revision {}, which no longer satisfies its declaration",
                        member.unit_id, member.revision, spec.edge_id, unit_id, revision
                    ));
                }
            }
        }
        Ok(problems)
    }

    /// The unit's own bytes must be held before a generation that pins them is
    /// published. For an inline tool this object is the module; for an
    /// out-of-line tool it is the pointer document, and the module chunks live
    /// in the installation's content store, which the executor re-verifies per
    /// run.
    fn ensure_environment_member_bytes(&self, member: &EnvironmentMember) -> Result<()> {
        let present: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE digest=?1)",
            params![member.artifact_digest.0],
            |r| r.get(0),
        )?;
        ensure!(
            present,
            "TOOL_ARTIFACT_UNAVAILABLE: {} revision {}: this installation's object store does not hold the pinned unit bytes",
            member.unit_id, member.revision
        );
        Ok(())
    }

    /// Read-only access to a retained package's exact bytes, digest-verified.
    /// This is the authoritative artifact read for code-unit execution; the
    /// materialized file is a copy for transparency, not the trust root.
    pub fn unit_object_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        self.object_bytes(digest)
    }

    /// Every distinct materialization root this installation ever wrote units
    /// into. Bootstrap uses this to admit (or refuse to adopt) an existing
    /// tool/package directory before any new installation state is written.
    pub fn materialization_roots(&self) -> Result<Vec<String>> {
        let mut roots: Vec<String> = self
            .conn
            .prepare(
                "SELECT DISTINCT root FROM unit_materializations WHERE root<>'' ORDER BY root",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        roots.dedup();
        Ok(roots)
    }

    /// Export a unit and the dependency closure this node can actually offer.
    fn unit_export(&self, unit_id: &str) -> Result<Value> {
        let root = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let export = self.export_unit_closure(&root)?;
        let closure = self.required_closure(&root)?;
        Ok(json!({
            "export": export,
            "unresolved_edges": unresolved_edges(&root.edges),
            "closure": closure_value(&root.edges, &closure),
            "note": "exported evidence belongs to the origin node; importing it never makes the unit ready here",
        }))
    }

    /// Build the transferable closure of one installed unit.
    fn export_unit_closure(&self, root: &InstalledUnit) -> Result<UnitExport> {
        let mut packages: Vec<UnitPackage> = Vec::new();
        let mut seen: Vec<(String, String, Digest)> = Vec::new();
        let mut queue: Vec<(String, String, Digest)> = vec![(
            root.unit_id.clone(),
            root.revision.clone(),
            root.artifact_digest.clone(),
        )];
        while let Some(current) = queue.pop() {
            if seen.contains(&current) {
                continue;
            }
            ensure!(packages.len() < MAX_PACKAGES, "unit dependency closure exceeds the transferable package limit");
            seen.push(current.clone());
            let Some(unit) = self.installed_unit_version(&current.0, &current.1, &current.2)?
            else {
                continue;
            };
            // An out-of-line artifact is not re-inlined into an export: the
            // portable identity is its digest, the locator is
            // installation-local.
            let bytes = if external_artifact_of(&unit.descriptor).is_some() {
                Vec::new()
            } else {
                self.object_bytes(&unit.artifact_digest)?
            };
            // A judgement recorded on this node and the observations bound to
            // it are facts about this installation. They never travel with a
            // package: another node must reach its own conclusion.
            let mut descriptor = unit.descriptor.clone();
            strip_local_sections(&mut descriptor.config);
            packages.push(UnitPackage {
                unit: descriptor,
                artifact_base64: b64_encode(&bytes),
                evidence: self.local_provenance(&unit.unit_id, &unit.artifact_digest)?,
                resolved_dependencies: Some(unit.edges.clone()),
            });
            for edge in &unit.edges {
                if edge.status == EdgeStatus::Satisfied {
                    if let (Some(unit_id), Some(revision), Some(digest)) = (
                        &edge.selected_unit_id,
                        &edge.selected_revision,
                        &edge.selected_artifact_digest,
                    ) {
                        let selected = (unit_id.clone(), revision.clone(), digest.clone());
                        if !seen.contains(&selected) {
                            queue.push(selected);
                        }
                    }
                }
            }
        }
        let origin_node = self.local_node_id()?.map(|node| node.to_string());
        Ok(UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: root.unit_id.clone(),
            root_revision: Some(root.revision.clone()),
            root_artifact_digest: Some(root.artifact_digest.clone()),
            origin_node,
            enabled: root.enabled,
            packages,
        })
    }

    // ----------------------------------------------------------- trial -----

    /// Run a bounded, caller-selected command as the real trial of a unit.
    ///
    /// The command is executed directly, without a shell. There is no
    /// allowlist and no approval step: the caller states the risk, the
    /// expected benefit and the consequence, and the run is recorded with its
    /// real exit status and output.
    ///
    /// A run that references the materialized artifact of this unit, either by
    /// passing its path or by using the `{artifact}` placeholder, is a
    /// readiness proof: the artifact digest is verified before and after the
    /// run. Any other command is an investigation of the environment. It is
    /// allowed, it is recorded, and it never promotes readiness.
    fn unit_trial(
        &mut self,
        unit_id: &str,
        argv: &[String],
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
        risk: &str,
        benefit: &str,
        consequence: &str,
    ) -> Result<Value> {
        ensure!(
            !argv.is_empty() && argv.len() <= MAX_ARGV,
            "trial argv must hold 1..=64 entries"
        );
        for argument in argv {
            ensure!(
                argument.len() <= MAX_ARG_BYTES,
                "trial argument exceeds the size bound"
            );
            ensure!(!argument.contains('\0'), "trial argument contains NUL");
        }
        ensure!(
            risk.trim().len() >= 4 && risk.len() <= MAX_TEXT_BYTES,
            "the trial must state its risk"
        );
        ensure!(
            benefit.trim().len() >= 4 && benefit.len() <= MAX_TEXT_BYTES,
            "the trial must state its expected benefit"
        );
        ensure!(
            consequence.trim().len() >= 4 && consequence.len() <= MAX_TEXT_BYTES,
            "the trial must state its consequence"
        );
        let timeout = timeout_ms
            .unwrap_or(DEFAULT_TRIAL_TIMEOUT_MS)
            .min(MAX_TRIAL_TIMEOUT_MS);
        ensure!(timeout > 0, "trial timeout must be positive");
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let artifact = unit
            .materialization
            .as_ref()
            .filter(|m| m.state == "materialized")
            .map(|m| m.path.clone())
            .filter(|path| !path.is_empty());
        let expanded = expand_artifact(argv, artifact.as_deref())?;
        let artifact_bound = match &artifact {
            Some(path) => references_artifact(&expanded, path),
            None => false,
        };
        let workdir = trial_workdir(cwd, &unit)?;
        let workdir_text = workdir.to_string_lossy().into_owned();
        let environment = probe_environment(&expanded[0], &workdir)?;
        let attempt_id = EntityId::new().to_string();
        let started_at = now();
        let owner_pid = i64::from(std::process::id());
        let owner_runtime = runtime_id(&self.conn)?;
        let attempt_data = json!({
            "argv": expanded,
            "requested_argv": argv,
            "cwd": workdir_text,
            "environment": environment,
            "risk": risk,
            "benefit": benefit,
            "consequence": consequence,
            "artifact_bound": artifact_bound,
            "artifact_path": artifact,
            "timeout_ms": timeout,
        });
        // The attempt is durable before any child can exist.
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,'prepared',?6,NULL,?7,?8,?9)",
                params![
                    attempt_id,
                    unit.unit_id,
                    unit.revision,
                    unit.artifact_digest.0,
                    environment.digest.0,
                    started_at,
                    owner_pid,
                    owner_runtime,
                    serde_json::to_string(&attempt_data)?
                ],
            )?;
            tx.commit()?;
        }
        // From here a child may exist, so the attempt is already unresolved.
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE unit_attempts SET state='spawning' WHERE attempt_id=?1",
                params![attempt_id],
            )?;
            tx.commit()?;
        }
        // A readiness proof only counts when the materialized artifact really
        // is the installed artifact.
        let artifact_verified_before = match (&artifact, artifact_bound) {
            (Some(path), true) => verify_artifact(path, &unit.artifact_digest),
            _ => false,
        };
        let mut command = Command::new(&expanded[0]);
        command
            .args(&expanded[1..])
            .current_dir(&workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // The child leads its own process group, so the deadline can stop the
        // whole tree instead of only the direct child.
        command.process_group(0);
        let spawn = command.spawn();
        let mut child = match spawn {
            Ok(child) => child,
            Err(e) => {
                let evidence = TrialEvidence {
                    attempt_id: attempt_id.clone(),
                    started_at: started_at.clone(),
                    finished_at: Some(now()),
                    outcome: TrialOutcome::Failed,
                    environment_digest: environment.digest.clone(),
                    environment: environment.clone(),
                    argv: expanded.clone(),
                    requested_argv: argv.to_vec(),
                    cwd: Some(workdir_text.clone()),
                    risk: risk.to_string(),
                    benefit: benefit.to_string(),
                    consequence: consequence.to_string(),
                    exit_code: None,
                    signal: None,
                    stdout: String::new(),
                    stderr: format!("{e}"),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    interrupted: false,
                    artifact_bound,
                    artifact_path: artifact.clone(),
                    artifact_verified_before,
                    artifact_verified_after: false,
                    reader_failed: false,
                    group_signalled: false,
                    descendants_unknown: false,
                    owner_pid,
                    owner_runtime: owner_runtime.clone(),
                    reason: "the trial command could not be started".to_string(),
                };
                self.record_trial(&attempt_id, &unit, started_at, &evidence, "failed")?;
                let (readiness, reason) = self.readiness_of(&unit)?;
                return Ok(json!({
                    "attempt_id": attempt_id,
                    "outcome": "failed",
                    "error": format!("{e}"),
                    "artifact_bound": artifact_bound,
                    "readiness_proof": false,
                    "readiness": readiness,
                    "reason": reason,
                }));
            }
        };
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE unit_attempts SET state='running' WHERE attempt_id=?1",
                params![attempt_id],
            )?;
            tx.commit()?;
        }
        let stdout = child.stdout.take().context("trial stdout pipe missing")?;
        let stderr = child.stderr.take().context("trial stderr pipe missing")?;
        let (out_tx, out_rx) = mpsc::channel();
        let (err_tx, err_rx) = mpsc::channel();
        let _out_reader = std::thread::spawn(move || {
            let result = drain(stdout, MAX_OUTPUT_BYTES);
            let _ = out_tx.send(result);
        });
        let _err_reader = std::thread::spawn(move || {
            let result = drain(stderr, MAX_OUTPUT_BYTES);
            let _ = err_tx.send(result);
        });
        let deadline = Instant::now() + Duration::from_millis(timeout);
        let mut timed_out = false;
        let mut group_signalled = false;
        let mut wait_error: Option<String> = None;
        let mut status = None;
        let mut out_result = None;
        let mut err_result = None;
        loop {
            if status.is_none() {
                match child.try_wait() {
                    Ok(Some(done)) => status = Some(done),
                    Ok(None) => {}
                    Err(e) => {
                        wait_error = Some(format!("{e}"));
                        group_signalled = signal_process_group(child.id());
                        let _ = child.kill();
                        status = child.wait().ok();
                        break;
                    }
                }
            }
            if out_result.is_none() {
                match out_rx.try_recv() {
                    Ok(result) => out_result = Some(result),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        out_result = Some((Vec::new(), false, true));
                    }
                }
            }
            if err_result.is_none() {
                match err_rx.try_recv() {
                    Ok(result) => err_result = Some(result),
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => {
                        err_result = Some((Vec::new(), false, true));
                    }
                }
            }
            // A departed group leader is not completion while a descendant
            // remains or a reader is still waiting for captured output.
            let group_finished = status.is_some() && process_group_gone(child.id());
            if status.is_some() && group_finished && out_result.is_some() && err_result.is_some() {
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                group_signalled = signal_process_group(child.id());
                if status.is_none() {
                    let _ = child.kill();
                    status = child.wait().ok();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // After a group stop, reader teardown remains bounded. An escaped
        // descendant can still hold a pipe, but its result is unknown rather
        // than an unbounded wait or a successful proof.
        if out_result.is_none() {
            out_result = Some(out_rx.recv_timeout(READ_TIMEOUT).unwrap_or((
                Vec::new(),
                false,
                true,
            )));
        }
        if err_result.is_none() {
            err_result = Some(err_rx.recv_timeout(READ_TIMEOUT).unwrap_or((
                Vec::new(),
                false,
                true,
            )));
        }
        let (out_bytes, out_truncated, out_failed) =
            out_result.unwrap_or((Vec::new(), false, true));
        let (err_bytes, err_truncated, err_failed) =
            err_result.unwrap_or((Vec::new(), false, true));
        let reader_failed = out_failed || err_failed;
        let descendants_unknown = timed_out || group_signalled || reader_failed;
        let artifact_verified_after = match (&artifact, artifact_bound) {
            (Some(path), true) => verify_artifact(path, &unit.artifact_digest),
            _ => false,
        };
        let (outcome, exit_code, signal) = match &status {
            Some(status) if timed_out => (TrialOutcome::TimedOut, status.code(), status.signal()),
            Some(status) if status.code() == Some(0) && reader_failed => {
                (TrialOutcome::Unknown, Some(0), None)
            }
            Some(status) if status.code() == Some(0) => (TrialOutcome::Succeeded, Some(0), None),
            Some(status) => match status.code() {
                Some(code) => (TrialOutcome::Failed, Some(code), None),
                None => (TrialOutcome::Signaled, None, status.signal()),
            },
            None => (TrialOutcome::Failed, None, None),
        };
        let stdout_text = String::from_utf8_lossy(&out_bytes).into_owned();
        let mut stderr_text = String::from_utf8_lossy(&err_bytes).into_owned();
        if let Some(error) = &wait_error {
            stderr_text.push_str(&format!("\nwait error: {error}"));
        }
        let reason = match outcome {
            TrialOutcome::Succeeded => "the trial command ran and reported success",
            TrialOutcome::Failed => "the trial command ran and reported failure",
            TrialOutcome::Signaled => "the trial command was terminated by a signal",
            TrialOutcome::TimedOut => {
                "the trial exceeded its bounded deadline while waiting for the command, descendants, or captured output"
            }
            TrialOutcome::Unknown => {
                "the trial result could not be read completely, so its outcome is unknown"
            }
        };
        let evidence = TrialEvidence {
            attempt_id: attempt_id.clone(),
            started_at: started_at.clone(),
            finished_at: Some(now()),
            outcome,
            environment_digest: environment.digest.clone(),
            environment: environment.clone(),
            argv: expanded.clone(),
            requested_argv: argv.to_vec(),
            cwd: Some(workdir_text.clone()),
            risk: risk.to_string(),
            benefit: benefit.to_string(),
            consequence: consequence.to_string(),
            exit_code,
            signal,
            stdout: stdout_text.clone(),
            stderr: stderr_text.clone(),
            stdout_truncated: out_truncated,
            stderr_truncated: err_truncated,
            interrupted: timed_out,
            artifact_bound,
            artifact_path: artifact.clone(),
            artifact_verified_before,
            artifact_verified_after,
            reader_failed,
            group_signalled,
            descendants_unknown,
            owner_pid,
            owner_runtime: owner_runtime.clone(),
            reason: reason.to_string(),
        };
        let readiness_proof = artifact_bound
            && artifact_verified_before
            && artifact_verified_after
            && outcome == TrialOutcome::Succeeded;
        self.record_trial(&attempt_id, &unit, started_at, &evidence, outcome.as_str())?;
        let (readiness, readiness_reason) = self.readiness_of(&unit)?;
        Ok(json!({
            "attempt_id": attempt_id,
            "outcome": outcome.as_str(),
            "exit_code": exit_code,
            "signal": signal,
            "argv": expanded,
            "cwd": workdir_text,
            "artifact_bound": artifact_bound,
            "artifact_path": artifact,
            "artifact_verified_before": artifact_verified_before,
            "artifact_verified_after": artifact_verified_after,
            "readiness_proof": readiness_proof,
            "reader_failed": reader_failed,
            "group_signalled": group_signalled,
            "descendants_unknown": descendants_unknown,
            "stdout": stdout_text,
            "stderr": stderr_text,
            "stdout_truncated": out_truncated,
            "stderr_truncated": err_truncated,
            "interrupted": timed_out,
            "environment": environment,
            "readiness": readiness,
            "reason": readiness_reason,
        }))
    }

    /// Retire attempts whose owning process is provably gone.
    ///
    /// An attempt that another live command still owns is left untouched: a
    /// concurrent command on the same installation is not a crash, and its
    /// attempt must not be relabelled as one.
    pub fn retire_unknown_trials(&mut self) -> Result<u32> {
        let my_runtime = runtime_id(&self.conn)?;
        let my_pid = i64::from(std::process::id());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Vec<(String, String, String, String, String, String, String, i64, String)> = tx
            .prepare(
                "SELECT attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,data,owner_pid,owner_runtime \
                 FROM unit_attempts WHERE state IN ('prepared','spawning','running') ORDER BY started_at",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if pending.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        let finished = now();
        let mut retired = 0u32;
        for (
            attempt_id,
            unit_id,
            revision,
            digest,
            environment,
            started_at,
            data,
            owner_pid,
            owner_runtime,
        ) in pending
        {
            let mine = owner_runtime == my_runtime && owner_pid == my_pid;
            if !mine && !process_gone(owner_pid, &owner_runtime) {
                // Another command may still be running this attempt.
                continue;
            }
            let stored: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
            let environment_digest = Digest(environment.clone());
            let evidence = TrialEvidence {
                attempt_id: attempt_id.clone(),
                started_at: started_at.clone(),
                finished_at: Some(finished.clone()),
                outcome: TrialOutcome::Unknown,
                environment_digest: environment_digest.clone(),
                environment: stored
                    .get("environment")
                    .and_then(|value| serde_json::from_value(value.clone()).ok())
                    .unwrap_or_else(|| fallback_environment(environment_digest)),
                argv: string_list(stored.get("argv")),
                requested_argv: string_list(stored.get("requested_argv")),
                cwd: stored
                    .get("cwd")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                risk: stored
                    .get("risk")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                benefit: stored
                    .get("benefit")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                consequence: stored
                    .get("consequence")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                exit_code: None,
                signal: None,
                stdout: String::new(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                interrupted: true,
                artifact_bound: stored
                    .get("artifact_bound")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false),
                artifact_path: stored
                    .get("artifact_path")
                    .and_then(|value| value.as_str())
                    .map(|value| value.to_string()),
                artifact_verified_before: false,
                artifact_verified_after: false,
                reader_failed: false,
                group_signalled: false,
                descendants_unknown: true,
                owner_pid,
                owner_runtime: owner_runtime.clone(),
                reason: "the owning command ended before this attempt was resolved; a child may have run, so the outcome is unknown and is never replayed automatically"
                    .to_string(),
            };
            tx.execute(
                "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'unknown',?8,0,?9,?10,?11)",
                params![
                    attempt_id,
                    unit_id,
                    revision,
                    digest,
                    environment,
                    started_at,
                    finished,
                    i64::from(evidence.artifact_bound),
                    owner_pid,
                    owner_runtime,
                    serde_json::to_string(&evidence)?
                ],
            )?;
            tx.execute(
                "UPDATE unit_attempts SET state='unknown',finished_at=?2 WHERE attempt_id=?1",
                params![attempt_id, finished],
            )?;
            retired += 1;
        }
        tx.commit()?;
        Ok(retired)
    }

    fn record_trial(
        &mut self,
        attempt_id: &str,
        unit: &InstalledUnit,
        started_at: String,
        evidence: &TrialEvidence,
        state: &str,
    ) -> Result<()> {
        let finished = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                attempt_id,
                unit.unit_id,
                unit.revision,
                unit.artifact_digest.0,
                evidence.environment_digest.0,
                started_at,
                finished,
                state,
                i64::from(evidence.artifact_bound),
                i64::from(
                    evidence.artifact_verified_before && evidence.artifact_verified_after
                ),
                evidence.owner_pid,
                evidence.owner_runtime,
                serde_json::to_string(evidence)?
            ],
        )?;
        tx.execute(
            "UPDATE unit_attempts SET state=?2,finished_at=?3 WHERE attempt_id=?1",
            params![attempt_id, state, finished],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn unit_evidence(&self, unit_id: Option<&str>, after: &str, limit: u32) -> Result<Value> {
        ensure!((1..=128).contains(&limit), "evidence limit 1..128");
        let after = crate::number(after)?;
        let after_seq = i64::try_from(after)?;
        let rows: Vec<(
            i64,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            String,
        )> = match unit_id {
            Some(id) => self
                .conn
                .prepare(
                    "SELECT seq,attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,data \
                     FROM unit_trials WHERE seq>?1 AND unit_id=?2 ORDER BY seq LIMIT ?3",
                )?
                .query_map(params![after_seq, id, limit], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => self
                .conn
                .prepare(
                    "SELECT seq,attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,data \
                     FROM unit_trials WHERE seq>?1 ORDER BY seq LIMIT ?2",
                )?
                .query_map(params![after_seq, limit], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        let mut entries = Vec::new();
        let mut cursor = after;
        for (seq, attempt, unit, revision, digest, environment, started, finished, outcome, data) in
            rows
        {
            cursor = seq as u64;
            entries.push(json!({
                "seq": seq.to_string(),
                "attempt_id": attempt,
                "unit_id": unit,
                "revision": revision,
                "artifact_digest": digest,
                "environment_digest": environment,
                "started_at": started,
                "finished_at": finished,
                "outcome": outcome,
                "evidence": serde_json::from_str::<Value>(&data)?,
            }));
        }
        Ok(json!({
            "evidence": entries,
            "through": cursor.to_string(),
            "append_only": true,
        }))
    }

    // ------------------------------------------------- installation facts ---

    /// The current version of one unit, without enumerating unrelated units
    /// or loading every retained historical version.
    pub fn installed_unit_fact(&self, unit_id: &str) -> Result<Option<InstalledUnitFact>> {
        self.installed_unit(unit_id)?
            .as_ref()
            .map(|unit| self.unit_fact(unit, true))
            .transpose()
    }

    /// One exact retained version, with its actual current/enabled projection.
    pub fn retained_unit_fact(&self, unit_id: &str, revision: &str, digest: &Digest) -> Result<Option<InstalledUnitFact>> {
        let current = self.installed_unit(unit_id)?;
        if let Some(unit) = current.as_ref().filter(|unit| unit.revision == revision && unit.artifact_digest == *digest) {
            return self.unit_fact(unit, true).map(Some);
        }
        self.load_installed_unit(unit_id, revision, digest, false)?
            .as_ref()
            .map(|unit| self.unit_fact_with(unit, false, false))
            .transpose()
    }

    /// The durable facts of every unit this node currently offers.
    pub fn installed_unit_facts(&self) -> Result<Vec<InstalledUnitFact>> {
        let mut facts = Vec::new();
        for unit in self.installed_units()? {
            facts.push(self.unit_fact(&unit, true)?);
        }
        Ok(facts)
    }

    /// The durable facts of every retained version of one unit.
    ///
    /// Superseded versions stay visible with the relation that replaced
    /// them, so a revision history is a fact rather than a guess.
    pub fn retained_unit_facts(&self, unit_id: &str) -> Result<Vec<InstalledUnitFact>> {
        let current = self.installed_unit(unit_id)?;
        let mut facts = Vec::new();
        for unit in self.retained_unit_versions(unit_id)? {
            let is_current = current.as_ref().is_some_and(|current| {
                current.revision == unit.revision && current.artifact_digest == unit.artifact_digest
            });
            let enabled = is_current && current.as_ref().is_some_and(|current| current.enabled);
            facts.push(self.unit_fact_with(&unit, is_current, enabled)?);
        }
        Ok(facts)
    }

    /// The exact content of one installed unit version.
    ///
    /// A revision can have several retained artifact digests; the caller
    /// names one when the revision alone is ambiguous.
    pub fn unit_content(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: Option<&Digest>,
    ) -> Result<UnitContent> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare("SELECT revision,artifact_digest FROM unit_versions WHERE unit_id=?1 AND revision=?2 ORDER BY artifact_digest")?
            .query_map(params![unit_id, revision], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(!rows.is_empty(), "unit revision is not retained on this node");
        let selected = match artifact_digest {
            Some(digest) => rows
                .iter()
                .find(|(_, stored)| stored == &digest.0)
                .cloned()
                .context("unit revision does not retain the requested artifact digest")?,
            None => {
                ensure!(
                    rows.len() == 1,
                    "unit revision retains several artifacts; name the artifact digest"
                );
                rows[0].clone()
            }
        };
        let digest = Digest(selected.1);
        let unit = self
            .installed_unit_version(unit_id, revision, &digest)?
            .context("unit version is not retained on this node")?;
        let bytes = self.object_bytes(&digest)?;
        ensure!(
            Digest::blake3(&bytes) == digest,
            "stored artifact corruption"
        );
        let current = self.installed_unit(unit_id)?.is_some_and(|current| {
            current.revision == revision && current.artifact_digest == digest
        });
        Ok(UnitContent {
            unit_id: unit_id.to_string(),
            kind: unit.descriptor.kind,
            revision: revision.to_string(),
            artifact_digest: digest,
            enabled: unit.enabled,
            current,
            bytes,
        })
    }

    /// Where the bytes of one artifact actually live.
    ///
    /// An artifact registered out of line is not in this installation's
    /// inline object store; the returned locator says where it is, and the
    /// executor re-verifies the digest before it runs.
    pub fn unit_artifact_source(&self, digest: &Digest) -> Result<UnitArtifactSource> {
        let versions: Vec<String> = self
            .conn
            .prepare("SELECT data FROM unit_versions ORDER BY unit_id,revision,artifact_digest")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for data in versions {
            let stored: StoredVersion = serde_json::from_str(&data)?;
            if stored.unit.artifact_digest != *digest {
                continue;
            }
            if let Some(external) = external_artifact_of(&stored.unit) {
                return Ok(UnitArtifactSource::External {
                    store: external.store,
                    locator: external.locator,
                    bytes: external.bytes,
                });
            }
        }
        let retained: bool = self
            .conn
            .query_row("SELECT 1 FROM objects WHERE digest=?1", [&digest.0], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some();
        ensure!(retained, "artifact is not retained on this node");
        Ok(UnitArtifactSource::Inline)
    }

    /// Register a unit version whose artifact bytes live outside this
    /// installation's inline object store.
    ///
    /// The bytes are never copied into the node database. The declared
    /// digest is the portable identity: a peer receives it and registers the
    /// unit against its own bytes.
    fn unit_import_external(
        &mut self,
        unit: UnitDescriptor,
        artifact: ExternalArtifact,
        package_root: &str,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(
            artifact.digest.algorithm() == "blake3",
            "unsupported artifact digest algorithm"
        );
        ensure!(
            artifact.digest == unit.artifact_digest,
            "external artifact digest does not match the unit descriptor"
        );
        ensure!(
            artifact.bytes <= MAX_EXTERNAL_ARTIFACT_BYTES,
            "external artifact exceeds the size bound"
        );
        ensure!(!artifact.locator.trim().is_empty(), "external artifact locator is empty");
        ensure!(!artifact.store.trim().is_empty(), "external artifact store is empty");
        validate_descriptor(&unit)?;
        let root = normalize_root(package_root)?;
        let mut descriptor = unit;
        descriptor.config["external_artifact"] = json!({
            "digest": artifact.digest,
            "bytes": artifact.bytes,
            "store": artifact.store,
            "locator": artifact.locator,
        });
        let export = UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: descriptor.unit_id.clone(),
            root_revision: Some(descriptor.revision.clone()),
            root_artifact_digest: Some(descriptor.artifact_digest.clone()),
            origin_node: None,
            enabled: enable.unwrap_or(false),
            packages: vec![UnitPackage {
                unit: descriptor,
                artifact_base64: String::new(),
                evidence: vec![],
                resolved_dependencies: None,
            }],
        };
        let mut outcome = self.unit_import(export, &root, enable)?;
        if let Some(object) = outcome.as_object_mut() {
            object.insert(
                "artifact_source".into(),
                json!({
                    "source": "external",
                    "store": artifact.store,
                    "locator": artifact.locator,
                    "bytes": artifact.bytes,
                }),
            );
        }
        Ok(outcome)
    }

    /// Read the exact content of one installed unit version.
    fn unit_read(&self, unit_id: &str, revision: &str) -> Result<Value> {
        let content = self.unit_content(unit_id, revision, None)?;
        let text = String::from_utf8(content.bytes.clone()).ok();
        Ok(json!({
            "unit_id": content.unit_id,
            "kind": content.kind,
            "revision": content.revision,
            "artifact_digest": content.artifact_digest,
            "enabled": content.enabled,
            "current": content.current,
            "bytes": content.bytes.len(),
            "content_base64": b64_encode(&content.bytes),
            "content_text": text,
        }))
    }

    /// Record how one unit version was judged, and on which evidence.
    ///
    /// The decision is stored under the unit's own configuration as a
    /// recorded fact. The node never derives a decision from the unit's
    /// text and never requires one: an unjudged unit stays unjudged.
    fn unit_record_adoption(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        decision: AdoptionDecision,
        method: AdoptionMethod,
        evidence: &[AdoptionEvidence],
        notes: Option<&str>,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(
            evidence.len() <= MAX_EDGES,
            "adoption evidence count out of bounds"
        );
        if let Some(notes) = notes {
            ensure!(notes.len() <= MAX_TEXT_BYTES, "adoption notes exceed the text bound");
        }
        if enable == Some(true) {
            ensure!(
                decision == AdoptionDecision::Adopted,
                "only an adopted unit can be enabled"
            );
        }
        let recorded_at = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            bail!("unit version is not retained on this node");
        };
        let mut stored: StoredVersion = serde_json::from_str(&data)?;
        // The node verifies every evidence item against its own durable
        // state. A trial attempt is runner-owned: this node opened it before
        // the run and closed it with the run's receipts. A stored observation
        // is bytes this node holds; it proves the bytes exist, never that a
        // measurement produced them, so it can never carry empirical
        // acceptance.
        let mut resolved: Vec<Value> = Vec::new();
        let mut trial_backed = false;
        for item in evidence {
            match item {
                AdoptionEvidence::TrialAttempt { attempt_id } => {
                    let attempt_id = attempt_id.trim();
                    ensure!(
                        !attempt_id.is_empty() && attempt_id.len() <= MAX_TEXT_BYTES,
                        "adoption trial attempt id is empty or too long"
                    );
                    let facts = Self::trial_attempt_facts(&tx, attempt_id)?;
                    let Some(facts) = facts else {
                        bail!("adoption evidence names a trial attempt this node never opened");
                    };
                    ensure!(
                        facts.unit_id == unit_id
                            && facts.revision == revision
                            && facts.artifact_digest == *artifact_digest,
                        "adoption evidence names a trial attempt of a different unit version"
                    );
                    ensure!(
                        facts.state == "finished",
                        "adoption evidence names a trial attempt that never reached a terminal state"
                    );
                    let plan_digest = match facts.plan_digest {
                        Some(digest) => digest,
                        None => bail!("the trial attempt carries no bound plan"),
                    };
                    trial_backed = true;
                    resolved.push(json!({
                        "kind": "trial_attempt",
                        "attempt_id": attempt_id,
                        "verified": true,
                        "plan_digest": plan_digest,
                        "bound_unit_id": facts.unit_id,
                        "bound_revision": facts.revision,
                        "bound_artifact_digest": facts.artifact_digest,
                    }));
                }
                AdoptionEvidence::StoredObservation { digest } => {
                    ensure!(
                        Self::observation_bound(&tx, &stored.unit, digest)?,
                        "adoption evidence names an observation this node does not hold for this unit version"
                    );
                    resolved.push(json!({
                        "kind": "stored_observation",
                        "digest": digest.0,
                        "verified": true,
                        "empirical": false,
                    }));
                }
            }
        }
        // The empirical flag says what the evidence can carry. An operator
        // statement, or an observation without a trial attempt, records what
        // was said; it never claims a measurement happened.
        let empirical = trial_backed && method.is_empirical();
        let adoption = json!({
            "decision": decision.as_str(),
            "method": method.as_str(),
            "evidence": resolved,
            "evidence_verified": resolved
                .iter()
                .all(|item| item.get("verified").and_then(Value::as_bool) == Some(true)),
            "empirical": empirical,
            "notes": notes,
            "recorded_at": recorded_at,
        });
        set_config_section(&mut stored.unit.config, "cognition", "adoption", adoption.clone());
        tx.execute(
            "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0],
        )?;
        tx.execute(
            "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET data=excluded.data,imported_at=excluded.imported_at",
            params![
                unit_id,
                revision,
                artifact_digest.0,
                stored.imported_at.clone(),
                serde_json::to_string(&stored)?
            ],
        )?;
        if let Some(enable) = enable {
            tx.execute(
                "UPDATE unit_current SET enabled=?2,updated_at=?3 WHERE unit_id=?1",
                params![unit_id, i64::from(enable), recorded_at],
            )?;
        }
        tx.commit()?;
        let facts = self.retained_unit_facts(unit_id)?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest,
            "adoption": adoption,
            "enabled": enable,
            "versions": facts,
        }))
    }

    /// The durable facts of one trial attempt.
    ///
    /// A trial attempt is runner-owned: this node opened it before the run
    /// started and closes it with the receipts the run produced. A caller
    /// cannot write an attempt row directly, so a report cannot promote
    /// itself into a measurement.
    fn trial_attempt_facts(conn: &Connection, attempt_id: &str) -> Result<Option<TrialAttemptFacts>> {
        let row: Option<(String, String, String, String, String, String)> = conn
            .prepare(
                "SELECT unit_id,revision,artifact_digest,state,started_at,data FROM unit_attempts WHERE attempt_id=?1",
            )?
            .query_row(params![attempt_id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .optional()?;
        let Some((unit_id, revision, artifact_digest, state, started_at, data)) = row else {
            return Ok(None);
        };
        let stored: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
        Ok(Some(TrialAttemptFacts {
            unit_id,
            revision,
            artifact_digest: Digest(artifact_digest),
            state,
            started_at,
            kind: stored.get("kind").and_then(Value::as_str).map(str::to_string),
            plan_digest: stored
                .get("plan_digest")
                .and_then(Value::as_str)
                .map(str::to_string),
        }))
    }

    /// Whether this node holds the observation bytes and binds them to this
    /// exact unit version.
    fn observation_bound(
        conn: &Connection,
        unit: &UnitDescriptor,
        digest: &Digest,
    ) -> Result<bool> {
        let bound = unit
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries.iter().any(|entry| {
                    entry.get("digest").and_then(Value::as_str) == Some(digest.0.as_str())
                })
            });
        if !bound {
            return Ok(false);
        }
        let bytes: Option<Vec<u8>> = conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&digest.0],
            |r| r.get(0),
        ).optional()?;
        Ok(bytes.is_some_and(|bytes| Digest::blake3(&bytes) == *digest))
    }

    /// Open a durable trial attempt for one unit version.
    ///
    /// The attempt exists before anything runs, so a crash leaves an
    /// unresolved attempt instead of a missing fact. The plan records what
    /// the trial is bound to: the exact artifact, the scenarios and rubrics,
    /// the candidate and control configuration and the model binding.
    fn unit_begin_trial(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        plan: &Value,
    ) -> Result<Value> {
        let kind = kind.trim();
        ensure!(
            !kind.is_empty() && kind.len() <= MAX_TEXT_BYTES,
            "the trial kind is empty or too long"
        );
        ensure!(plan.is_object(), "the trial plan must be an object");
        let plan_bytes = serde_json::to_vec(plan)?;
        ensure!(
            plan_bytes.len() <= MAX_TEXT_BYTES * 8,
            "the trial plan exceeds the size bound"
        );
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        ensure!(
            unit.revision == revision && unit.artifact_digest == *artifact_digest,
            "the trial must bind the current version of the unit"
        );
        self.retire_open_trials()?;
        let plan_digest = Digest::blake3(&plan_bytes);
        let attempt_id = EntityId::new().to_string();
        let started_at = now();
        let owner_pid = i64::from(std::process::id());
        let owner_runtime = runtime_id(&self.conn)?;
        let data = json!({
            "kind": kind,
            "plan": plan,
            "plan_digest": plan_digest.0,
            "started_at": started_at,
        });
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,'trial_open',?6,NULL,?7,?8,?9)",
            params![
                attempt_id,
                unit_id,
                revision,
                artifact_digest.0,
                plan_digest.0,
                started_at,
                owner_pid,
                owner_runtime,
                serde_json::to_string(&data)?
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "attempt_id": attempt_id,
            "kind": kind,
            "plan_digest": plan_digest.0,
            "state": "trial_open",
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest.0,
        }))
    }

    /// Close a trial attempt with the receipts the run produced.
    ///
    /// Every receipt is an independent terminal fact of one condition. The
    /// node stores them with the attempt and never derives a measurement from
    /// a summary the caller wrote.
    fn unit_finish_trial(
        &mut self,
        attempt_id: &str,
        outcome: TrialOutcome,
        receipts: &[Value],
    ) -> Result<Value> {
        ensure!(
            receipts.len() <= MAX_EDGES * 4,
            "too many trial receipts"
        );
        for receipt in receipts {
            ensure!(receipt.is_object(), "a trial receipt must be an object");
            ensure!(
                receipt.get("digest").and_then(Value::as_str).is_some(),
                "a trial receipt must name the digest of the fact it carries"
            );
        }
        let facts = Self::trial_attempt_facts(&self.conn, attempt_id)?
            .context("this node never opened that trial attempt")?;
        ensure!(
            facts.state == "trial_open",
            "the trial attempt is not open; it was already closed"
        );
        let finished_at = now();
        let data = json!({
            "kind": facts.kind,
            "plan_digest": facts.plan_digest,
            "receipts": receipts,
            "receipt_count": receipts.len(),
            "finished_at": finished_at,
        });
        let owner_runtime = runtime_id(&self.conn)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,0,?9,?10,?11)",
            params![
                attempt_id,
                facts.unit_id,
                facts.revision,
                facts.artifact_digest.0,
                facts.plan_digest.clone().unwrap_or_default(),
                facts.started_at,
                finished_at,
                outcome.as_str(),
                i64::from(std::process::id()),
                owner_runtime,
                serde_json::to_string(&data)?
            ],
        )?;
        tx.execute(
            "UPDATE unit_attempts SET state='finished',finished_at=?2 WHERE attempt_id=?1",
            params![attempt_id, finished_at],
        )?;
        tx.commit()?;
        Ok(json!({
            "attempt_id": attempt_id,
            "state": "finished",
            "outcome": outcome.as_str(),
            "receipt_count": receipts.len(),
            "unit_id": facts.unit_id,
            "revision": facts.revision,
            "artifact_digest": facts.artifact_digest.0,
        }))
    }

    /// Register an observation about one unit version.
    ///
    /// The bytes are stored with their digest so the node can re-verify them,
    /// and they are bound to this exact unit version. An observation is a
    /// recorded fact; it never becomes empirical acceptance by itself.
    fn unit_register_observation(
        &mut self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        kind: &str,
        bytes: &[u8],
    ) -> Result<Value> {
        let kind = kind.trim();
        ensure!(
            !kind.is_empty() && kind.len() <= MAX_TEXT_BYTES,
            "the observation kind is empty or too long"
        );
        ensure!(
            bytes.len() <= MAX_ARTIFACT_BYTES,
            "an observation exceeds the inline object bound"
        );
        let digest = Digest::blake3(bytes);
        let recorded_at = now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let data: Option<String> = tx
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            bail!("unit version is not retained on this node");
        };
        let mut stored: StoredVersion = serde_json::from_str(&data)?;
        tx.execute(
            "INSERT OR IGNORE INTO objects(digest,bytes) VALUES(?1,?2)",
            params![digest.0, bytes],
        )?;
        let mut entries = stored
            .unit
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        entries.retain(|entry| {
            entry.get("digest").and_then(Value::as_str) != Some(digest.0.as_str())
        });
        entries.push(json!({
            "digest": digest.0,
            "kind": kind,
            "bytes": bytes.len(),
            "recorded_at": recorded_at,
        }));
        set_config_section(
            &mut stored.unit.config,
            "cognition",
            "observations",
            Value::Array(entries.clone()),
        );
        tx.execute(
            "INSERT INTO unit_version_history(unit_id,revision,artifact_digest,imported_at,data) SELECT unit_id,revision,artifact_digest,imported_at,data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![unit_id, revision, artifact_digest.0],
        )?;
        tx.execute(
            "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET data=excluded.data,imported_at=excluded.imported_at",
            params![
                unit_id,
                revision,
                artifact_digest.0,
                stored.imported_at.clone(),
                serde_json::to_string(&stored)?
            ],
        )?;
        tx.commit()?;
        Ok(json!({
            "unit_id": unit_id,
            "revision": revision,
            "artifact_digest": artifact_digest.0,
            "digest": digest.0,
            "kind": kind,
            "bytes": bytes.len(),
            "observations": entries,
        }))
    }

    /// The observations registered for one unit version.
    fn unit_observations(&self, unit_id: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let version = self
            .installed_unit_version(&unit.unit_id, &unit.revision, &unit.artifact_digest)?
            .context("the current unit version is not retained")?;
        let entries = version
            .descriptor
            .config
            .pointer("/cognition/observations")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let observations: Vec<Value> = entries
            .iter()
            .map(|entry| {
                let digest = entry
                    .get("digest")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let held = self
                    .object_bytes(&Digest(digest.clone()))
                    .map(|bytes| Digest::blake3(&bytes) == Digest(digest.clone()))
                    .unwrap_or(false);
                let mut entry = entry.clone();
                if let Some(object) = entry.as_object_mut() {
                    object.insert("bytes_held".into(), json!(held));
                }
                entry
            })
            .collect();
        Ok(json!({
            "unit_id": unit.unit_id,
            "revision": unit.revision,
            "artifact_digest": unit.artifact_digest.0,
            "observations": observations,
        }))
    }

    /// Close trial attempts whose owning process is gone.
    ///
    /// A trial that never finished is unknown, never a measurement. The
    /// attempt keeps its plan so a reader can see what was bound.
    fn retire_open_trials(&mut self) -> Result<u32> {
        let my_runtime = runtime_id(&self.conn)?;
        let my_pid = i64::from(std::process::id());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending: Vec<(String, String, String, String, String, i64, String)> = tx
            .prepare(
                "SELECT attempt_id,unit_id,revision,artifact_digest,environment_digest,owner_pid,owner_runtime FROM unit_attempts WHERE state='trial_open' ORDER BY started_at",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if pending.is_empty() {
            tx.commit()?;
            return Ok(0);
        }
        let finished = now();
        let mut retired = 0u32;
        for (
            attempt_id,
            unit_id,
            revision,
            digest,
            environment,
            owner_pid,
            owner_runtime,
        ) in pending
        {
            let mine = owner_runtime == my_runtime && owner_pid == my_pid;
            if !mine && !process_gone(owner_pid, &owner_runtime) {
                continue;
            }
            tx.execute(
                "INSERT INTO unit_trials(attempt_id,unit_id,revision,artifact_digest,environment_digest,started_at,finished_at,outcome,artifact_bound,artifact_verified,owner_pid,owner_runtime,data) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,'unknown',0,0,?8,?9,?10)",
                params![
                    attempt_id,
                    unit_id,
                    revision,
                    digest,
                    environment,
                    finished,
                    finished,
                    owner_pid,
                    owner_runtime,
                    json!({"kind": "unknown", "reason": "the trial attempt was never closed"}).to_string()
                ],
            )?;
            tx.execute(
                "UPDATE unit_attempts SET state='unknown',finished_at=?2 WHERE attempt_id=?1",
                params![attempt_id, finished],
            )?;
            retired += 1;
        }
        tx.commit()?;
        Ok(retired)
    }

    /// The retained versions of one unit and their supersession relations.
    fn unit_lineage(&self, unit_id: &str) -> Result<Value> {
        let facts = self.retained_unit_facts(unit_id)?;
        ensure!(!facts.is_empty(), "unit is not installed on this node");
        Ok(json!({
            "unit_id": unit_id,
            "current": facts
                .iter()
                .find(|fact| fact.current)
                .map(|fact| json!({
                    "revision": fact.revision,
                    "artifact_digest": fact.artifact_digest,
                    "enabled": fact.enabled,
                })),
            "versions": facts,
        }))
    }

    /// Every retained version of one unit.
    fn retained_unit_versions(&self, unit_id: &str) -> Result<Vec<InstalledUnit>> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare("SELECT revision,artifact_digest FROM unit_versions WHERE unit_id=?1 ORDER BY revision,artifact_digest")?
            .query_map(params![unit_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut units = Vec::new();
        for (revision, digest) in rows {
            if let Some(unit) = self.load_installed_unit(unit_id, &revision, &Digest(digest), false)? {
                units.push(unit);
            }
        }
        Ok(units)
    }

    /// The public projection of one installed version.
    fn unit_fact(&self, unit: &InstalledUnit, current: bool) -> Result<InstalledUnitFact> {
        self.unit_fact_with(unit, current, unit.enabled)
    }

    fn unit_fact_with(
        &self,
        unit: &InstalledUnit,
        current: bool,
        enabled: bool,
    ) -> Result<InstalledUnitFact> {
        let supersedes = declared_supersedes(&unit.descriptor);
        let mut superseded_by: Vec<SupersededVersion> = Vec::new();
        for other in self.retained_unit_versions(&unit.unit_id)? {
            if other.revision == unit.revision && other.artifact_digest == unit.artifact_digest {
                continue;
            }
            for relation in declared_supersedes(&other.descriptor) {
                if relation.revision == unit.revision && relation.artifact_digest == unit.artifact_digest {
                    superseded_by.push(SupersededVersion {
                        revision: other.revision.clone(),
                        artifact_digest: other.artifact_digest.clone(),
                        reason: relation.reason,
                    });
                }
            }
        }
        let artifact_source = match external_artifact_of(&unit.descriptor) {
            Some(external) => UnitArtifactSource::External {
                store: external.store,
                locator: external.locator,
                bytes: external.bytes,
            },
            None => UnitArtifactSource::Inline,
        };
        let imported_at = self
            .conn
            .query_row(
                "SELECT imported_at FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default();
        let origin_node = self
            .conn
            .query_row(
                "SELECT json_extract(data,'$.origin_node') FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit.unit_id, unit.revision, unit.artifact_digest.0],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        Ok(InstalledUnitFact {
            unit_id: unit.unit_id.clone(),
            kind: unit.descriptor.kind,
            revision: unit.revision.clone(),
            artifact_digest: unit.artifact_digest.clone(),
            current,
            enabled,
            imported_at,
            origin_node,
            descriptor: unit.descriptor.clone(),
            edges: unit.edges.clone(),
            supersedes,
            superseded_by,
            artifact_source,
            materialization: unit.materialization.as_ref().map(|materialization| {
                MaterializationFact {
                    root: materialization.root.clone(),
                    path: materialization.path.clone(),
                    state: materialization.state.clone(),
                    detail: materialization.detail.clone(),
                }
            }),
        })
    }

    // ------------------------------------------------------- local facts ---

    fn installed_units(&self) -> Result<Vec<InstalledUnit>> {
        let ids: Vec<String> = self
            .conn
            .prepare("SELECT unit_id FROM unit_current ORDER BY unit_id")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut units = Vec::new();
        for id in ids {
            if let Some(unit) = self.installed_unit(&id)? {
                units.push(unit);
            }
        }
        Ok(units)
    }

    /// Capability names declared by currently enabled units on this node.
    /// These are declarations for coordination candidacy, not readiness or
    /// model-evidence claims. Reads on the caller's connection so a
    /// generation transaction can bind them to its own durable snapshot.
    pub(crate) fn enabled_provides(conn: &Connection) -> Result<Vec<String>> {
        let mut provides = Vec::new();
        let rows: Vec<(String, i64)> = conn
            .prepare(
                "SELECT v.data,c.enabled FROM unit_current c JOIN unit_versions v ON v.unit_id=c.unit_id AND v.revision=c.revision AND v.artifact_digest=c.artifact_digest ORDER BY c.unit_id",
            )?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (data, enabled) in rows {
            if enabled == 0 {
                continue;
            }
            let stored: StoredVersion = serde_json::from_str(&data)?;
            provides.extend(stored.unit.provides);
        }
        provides.sort();
        provides.dedup();
        Ok(provides)
    }


    fn installed_unit(&self, unit_id: &str) -> Result<Option<InstalledUnit>> {
        let row: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT revision,artifact_digest,enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((revision, digest, enabled)) = row else {
            return Ok(None);
        };
        let unit = self
            .load_installed_unit(unit_id, &revision, &Digest(digest), enabled != 0)?
            .context("current unit version is not retained")?;
        Ok(Some(unit))
    }

    fn installed_unit_version(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
    ) -> Result<Option<InstalledUnit>> {
        let enabled = self
            .conn
            .query_row(
                "SELECT enabled FROM unit_current WHERE unit_id=?1",
                [unit_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0;
        self.load_installed_unit(unit_id, revision, artifact_digest, enabled)
    }

    fn load_installed_unit(
        &self,
        unit_id: &str,
        revision: &str,
        artifact_digest: &Digest,
        enabled: bool,
    ) -> Result<Option<InstalledUnit>> {
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        let stored: StoredVersion = serde_json::from_str(&data)?;
        let stored_edges: Vec<ResolvedDependency> = self
            .conn
            .prepare(
                "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 ORDER BY edge_id",
            )?
            .query_map(params![unit_id, revision, artifact_digest.0], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|value| serde_json::from_str::<ResolvedDependency>(&value))
            .collect::<serde_json::Result<Vec<_>>>()?;
        // Old descriptors can be updated under the same artifact identity.
        // Stale edge rows must not become dependencies of the new descriptor,
        // and a missing row must never make its required closure look complete.
        let mut edges: Vec<ResolvedDependency> = stored.unit.dependencies.iter().map(|spec| {
            stored_edges.iter().find(|edge| edge_matches_spec(edge, spec)).cloned().unwrap_or_else(|| {
                legacy_ambiguous(spec, "no matching digest-bound edge is retained; use resolve_version for this exact version")
            })
        }).collect();
        edges.sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
        let materialization: Option<Materialization> = self
            .conn
            .query_row(
                "SELECT root,path,state,detail FROM unit_materializations WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
                params![unit_id, revision, artifact_digest.0],
                |r| {
                    Ok(Materialization {
                        root: r.get(0)?,
                        path: r.get(1)?,
                        state: r.get(2)?,
                        detail: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(Some(InstalledUnit {
            unit_id: unit_id.to_string(),
            revision: revision.to_string(),
            artifact_digest: artifact_digest.clone(),
            enabled,
            descriptor: stored.unit,
            edges,
            materialization,
        }))
    }

    fn object_bytes(&self, digest: &Digest) -> Result<Vec<u8>> {
        let bytes: Vec<u8> = self.conn.query_row(
            "SELECT bytes FROM objects WHERE digest=?1",
            [&digest.0],
            |r| r.get(0),
        )?;
        ensure!(
            Digest::blake3(&bytes) == *digest,
            "stored object corruption"
        );
        Ok(bytes)
    }

    fn local_trial_count(&self, unit_id: &str, digest: &Digest) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM unit_trials WHERE unit_id=?1 AND artifact_digest=?2",
            params![unit_id, digest.0],
            |r| r.get(0),
        )?;
        Ok(count as u32)
    }

    fn local_provenance(&self, unit_id: &str, digest: &Digest) -> Result<Vec<TrialProvenance>> {
        let mut statement = self.conn.prepare(
            "SELECT t.attempt_id,t.unit_id,t.revision,t.artifact_digest,t.environment_digest,t.started_at,t.finished_at,t.outcome,t.data,\
             a.data,a.state,a.started_at,\
             a.unit_id=t.unit_id AND a.revision=t.revision AND a.artifact_digest=t.artifact_digest \
             AND a.environment_digest=t.environment_digest AND a.finished_at=t.finished_at \
             FROM unit_trials t LEFT JOIN unit_attempts a ON a.attempt_id=t.attempt_id \
             WHERE t.unit_id=?1 AND t.artifact_digest=?2 ORDER BY t.seq",
        )?;
        let rows = statement.query_map(params![unit_id, digest.0], |r| {
            Ok(StoredTrialProvenance {
                attempt_id: r.get(0)?,
                unit_id: r.get(1)?,
                revision: r.get(2)?,
                artifact_digest: Digest(r.get(3)?),
                environment_digest: Digest(r.get(4)?),
                started_at: r.get(5)?,
                finished_at: r.get(6)?,
                outcome: r.get(7)?,
                data: r.get(8)?,
                attempt_data: r.get(9)?,
                attempt_state: r.get(10)?,
                attempt_started_at: r.get(11)?,
                attempt_matches: r.get(12)?,
            })
        })?;
        rows.map(|row| {
            let row = row?;
            match serde_json::from_str::<TrialEvidence>(&row.data) {
                Ok(command) => {
                    ensure!(
                        command.attempt_id == row.attempt_id
                            && command.environment_digest == row.environment_digest
                            && command.outcome.as_str() == row.outcome,
                        "command trial does not match its stored identity"
                    );
                    Ok(TrialProvenance::from(&command))
                }
                Err(command_error) => row.generic()
                    .map(TrialProvenance::Generic)
                    .with_context(|| format!("invalid local trial evidence; command decoding: {command_error}")),
            }
            })
            .collect()
    }

    /// Readiness of an installed unit on this node.
    ///
    /// Only local trial evidence of the exact artifact digest counts. Remote
    /// evidence and imported success never make a unit ready here.
    fn readiness_of(&self, unit: &InstalledUnit) -> Result<(ReadinessState, String)> {
        let closure = self.required_closure(unit)?;
        self.readiness_with_closure(unit, &closure)
    }

    fn required_closure(&self, root: &InstalledUnit) -> Result<RequiredClosure> {
        let root_materialized = materialization_matches(root);
        let mut closure = RequiredClosure {
            root_materialized,
            ..RequiredClosure::default()
        };
        if !root_materialized {
            closure.unmaterialized.push(format!(
                "{} revision {} does not have its selected artifact bytes",
                root.unit_id, root.revision
            ));
        }
        let mut visited = Vec::new();
        self.inspect_required_dependencies(root, &mut visited, &mut closure)?;
        Ok(closure)
    }

    fn inspect_required_dependencies(
        &self,
        unit: &InstalledUnit,
        visited: &mut Vec<(String, String, Digest)>,
        closure: &mut RequiredClosure,
    ) -> Result<()> {
        let current = (
            unit.unit_id.clone(),
            unit.revision.clone(),
            unit.artifact_digest.clone(),
        );
        if visited.contains(&current) {
            return Ok(());
        }
        visited.push(current);
        for edge in unit
            .edges
            .iter()
            .filter(|edge| edge.strength == DependencyStrength::Required)
        {
            if edge.status != EdgeStatus::Satisfied {
                closure.unresolved.push(format!(
                    "{} revision {} edge {}: {}",
                    unit.unit_id, unit.revision, edge.edge_id, edge.reason
                ));
                continue;
            }
            let selected = match (
                &edge.selected_unit_id,
                &edge.selected_revision,
                &edge.selected_artifact_digest,
            ) {
                (Some(unit_id), Some(revision), Some(digest)) => {
                    (unit_id.clone(), revision.clone(), digest.clone())
                }
                _ => {
                    closure.unresolved.push(format!(
                        "{} revision {} edge {} has an incomplete selected reference",
                        unit.unit_id, unit.revision, edge.edge_id
                    ));
                    continue;
                }
            };
            let Some(dependency) =
                self.installed_unit_version(&selected.0, &selected.1, &selected.2)?
            else {
                closure.unresolved.push(format!(
                    "{} revision {} edge {} selects a version that is not retained",
                    unit.unit_id, unit.revision, edge.edge_id
                ));
                continue;
            };
            let spec = unit.descriptor.dependencies.iter().find(|spec| edge_matches_spec(edge, spec))
                .context("stored dependency edge has no matching declaration")?;
            if !selection_matches(spec, &dependency.unit_id, &dependency.revision,
                &dependency.artifact_digest, &dependency.descriptor.provides) {
                closure.unresolved.push(format!(
                    "{} revision {} edge {} retains a selection that no longer satisfies its declaration",
                    unit.unit_id, unit.revision, edge.edge_id
                ));
                continue;
            }
            if visited.contains(&selected) {
                continue;
            }
            if !materialization_matches(&dependency) {
                closure.unmaterialized.push(format!(
                    "{} revision {} does not have its selected artifact bytes",
                    dependency.unit_id, dependency.revision
                ));
            }
            self.inspect_required_dependencies(&dependency, visited, closure)?;
        }
        Ok(())
    }

    fn readiness_with_closure(
        &self,
        unit: &InstalledUnit,
        closure: &RequiredClosure,
    ) -> Result<(ReadinessState, String)> {
        if let Some(problem) = closure.unresolved.first() {
            return Ok((
                ReadinessState::Unresolved,
                format!("a required dependency is unresolved on this node: {problem}"),
            ));
        }
        if !closure.root_materialized {
            let detail = unit
                .materialization
                .as_ref()
                .map(|m| m.detail.clone())
                .unwrap_or_default();
            return Ok((
                ReadinessState::Unmaterialized,
                if detail.is_empty() {
                    "the current package bytes are missing or do not match the installed artifact digest"
                        .to_string()
                } else {
                    format!("the package bytes are not materialized: {detail}")
                },
            ));
        }
        if let Some(problem) = closure.unmaterialized.first() {
            return Ok((
                ReadinessState::Unmaterialized,
                format!(
                    "a required dependency is not materialized with its selected bytes: {problem}"
                ),
            ));
        }
        // Only a trial that ran the materialized artifact is a readiness
        // proof; an unbound investigation is evidence, not proof.
        let last: Option<(String, String, i64, String)> = self
            .conn
            .query_row(
                "SELECT environment_digest,outcome,artifact_verified,data FROM unit_trials \
                 WHERE unit_id=?1 AND artifact_digest=?2 AND artifact_bound=1 ORDER BY seq DESC LIMIT 1",
                params![unit.unit_id, unit.artifact_digest.0],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((environment_digest, outcome, artifact_verified, data)) = last else {
            let investigations: i64 = self
                .conn
                .query_row(
                    "SELECT count(*) FROM unit_trials WHERE unit_id=?1 AND artifact_digest=?2 AND artifact_bound=0",
                    params![unit.unit_id, unit.artifact_digest.0],
                    |r| r.get(0),
                )?;
            return Ok((
                ReadinessState::Unverified,
                if investigations > 0 {
                    "only unbound investigation commands have run here; they are not a readiness proof"
                        .to_string()
                } else {
                    "no local trial of the materialized artifact has run on this node yet"
                        .to_string()
                },
            ));
        };
        let evidence: TrialEvidence = match serde_json::from_str(&data) {
            Ok(value) => value,
            Err(_) => {
                return Ok((
                    ReadinessState::Unknown,
                    "the last local trial evidence could not be read".into(),
                ));
            }
        };
        // The environment is part of the proof: a changed interpreter, OS or
        // architecture does not inherit the previous result.
        let Some(workdir) = evidence_workdir(&evidence) else {
            return Ok((
                ReadinessState::Unverified,
                "the last local trial did not retain an absolute effective working directory"
                    .into(),
            ));
        };
        if !workdir.is_dir() {
            return Ok((
                ReadinessState::Unverified,
                "the last local trial's effective working directory is no longer available".into(),
            ));
        }
        let current = probe_environment(
            evidence.argv.first().map(|v| v.as_str()).unwrap_or(""),
            &workdir,
        )?;
        if current.digest != Digest(environment_digest) {
            return Ok((
                ReadinessState::Unverified,
                "the environment changed since the last local trial; the previous result does not carry over"
                    .into(),
            ));
        }
        let outcome = TrialOutcome::parse(&outcome)?;
        Ok(match outcome {
            TrialOutcome::Succeeded if artifact_verified != 0 => (
                ReadinessState::Ready,
                "a local trial of the materialized artifact succeeded in this environment".into(),
            ),
            TrialOutcome::Succeeded => (
                ReadinessState::Failed,
                "the materialized artifact did not match the installed digest during the trial"
                    .into(),
            ),
            TrialOutcome::Failed | TrialOutcome::Signaled | TrialOutcome::TimedOut => (
                ReadinessState::Failed,
                format!("the last local trial ended as {}", outcome.as_str()),
            ),
            TrialOutcome::Unknown => (
                ReadinessState::Unknown,
                "the last local attempt has an unknown outcome; it is never replayed automatically"
                    .into(),
            ),
        })
    }
}

// ------------------------------------------------------------- helpers -----

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Set one key inside a named section of a unit's own configuration.
///
/// The configuration is authored by the package; this helper only adds a
/// node-recorded fact under its own section and never rewrites an authored
/// value.
fn set_config_section(config: &mut Value, section: &str, key: &str, value: Value) {
    if !config.is_object() {
        *config = json!({});
    }
    let root = config.as_object_mut().expect("config is an object");
    let slot = root.entry(section.to_string()).or_insert_with(|| json!({}));
    if !slot.is_object() {
        *slot = json!({});
    }
    slot.as_object_mut()
        .expect("section is an object")
        .insert(key.to_string(), value);
}

/// Remove the sections that are facts about one node only.
///
/// A recorded judgement and the observations bound to it describe what
/// happened on this installation. They are stripped from every package so a
/// peer never inherits an acceptance it did not measure.
fn strip_local_sections(config: &mut Value) {
    let empty = {
        let Some(section) = config.get_mut("cognition").and_then(Value::as_object_mut) else {
            return;
        };
        section.remove("adoption");
        section.remove("observations");
        section.is_empty()
    };
    if empty {
        if let Some(root) = config.as_object_mut() {
            root.remove("cognition");
        }
    }
}

/// The out-of-line artifact a unit version declares, when it declares one.
fn external_artifact_of(descriptor: &UnitDescriptor) -> Option<ExternalArtifact> {
    let value = descriptor.config.get("external_artifact")?;
    let digest = Digest(value.get("digest")?.as_str()?.to_string());
    let bytes = value.get("bytes")?.as_u64()?;
    let store = value.get("store")?.as_str()?.to_string();
    let locator = value.get("locator")?.as_str()?.to_string();
    Some(ExternalArtifact {
        digest,
        bytes,
        store,
        locator,
    })
}

/// The versions a unit version declares it replaces.
///
/// The relation is declared, never inferred: a node that holds two
/// versions of a unit does not decide by itself which one replaced which.
fn declared_supersedes(descriptor: &UnitDescriptor) -> Vec<SupersededVersion> {
    let Some(Value::Array(items)) = descriptor.config.pointer("/cognition/supersedes") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let revision = item.get("revision")?.as_str()?.to_string();
            let artifact_digest = Digest(item.get("artifact_digest")?.as_str()?.to_string());
            let reason = item
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some(SupersededVersion {
                revision,
                artifact_digest,
                reason,
            })
        })
        .collect()
}

fn ensure_unit_schema(conn: &rusqlite::Connection) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
         ('unit_versions','unit_version_history','unit_current','unit_edges','unit_edge_history','unit_edges_legacy','unit_materializations','unit_attempts','unit_trials')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        count == 9,
        "node storage schema lacks the installation-unit tables; migrate the node storage schema first"
    );
    Ok(())
}

fn read_environment_document(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT v FROM meta WHERE k=?1", [key], |r| r.get(0))
        .optional()?)
}

/// The documents stored under one environment key family, in key order — that
/// is, in ascending generation or selector-revision order.
fn environment_documents(
    conn: &Connection,
    pattern: &str,
    limit: u32,
) -> Result<Vec<(String, String)>> {
    Ok(conn
        .prepare("SELECT k,v FROM meta WHERE k GLOB ?1 ORDER BY k LIMIT ?2")?
        .query_map(params![pattern, i64::from(limit)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn read_environment_selector(conn: &Connection) -> Result<Option<EnvironmentSelector>> {
    read_environment_document(conn, ENVIRONMENT_SELECTOR_KEY)?
        .map(|data| {
            serde_json::from_str(&data).context("stored environment selector is not readable")
        })
        .transpose()
}

fn read_environment_generation(
    conn: &Connection,
    generation: u64,
) -> Result<Option<EnvironmentGeneration>> {
    read_environment_document(conn, &environment_generation_key(generation))?
        .map(|data| environment_generation_document(&data))
        .transpose()
}

fn environment_generation_document(data: &str) -> Result<EnvironmentGeneration> {
    serde_json::from_str(data).context("stored environment generation is not readable")
}

/// The highest generation this installation recorded, if any.
fn latest_environment_generation(conn: &Connection) -> Result<Option<u64>> {
    let key: Option<String> = conn
        .query_row(
            "SELECT k FROM meta WHERE k GLOB ?1 ORDER BY k DESC LIMIT 1",
            [ENVIRONMENT_GENERATION_GLOB],
            |r| r.get(0),
        )
        .optional()?;
    key.map(|key| {
        key.strip_prefix(ENVIRONMENT_GENERATION_PREFIX)
            .and_then(|suffix| suffix.parse::<u64>().ok())
            .context("recorded environment generation key is not readable")
    })
    .transpose()
}

/// The closure identity: blake3 over the canonical document (sorted roots and
/// members). Two environments that pin the same versions have the same digest,
/// whatever order the caller listed them in.
fn environment_closure_digest(
    roots: &[String],
    members: &[EnvironmentMember],
    version: u32,
) -> Result<Digest> {
    ensure!(
        (1..=ENVIRONMENT_CLOSURE_VERSION).contains(&version),
        "UNIT_ENVIRONMENT_CORRUPT: unknown closure digest version {version}"
    );
    let mut roots = roots.to_vec();
    roots.sort();
    let mut members = members.to_vec();
    members.sort_by(|left, right| left.unit_id.cmp(&right.unit_id));
    let mut documents = Vec::with_capacity(members.len());
    for member in &members {
        let mut document = json!({
            "unit_id": member.unit_id,
            "revision": member.revision,
            "artifact_digest": member.artifact_digest.0,
            "role": member.role.as_str(),
        });
        if version >= 2 {
            let mut edges = member.edges.clone();
            edges.sort_by(|left, right| left.edge_id.cmp(&right.edge_id));
            let object = document
                .as_object_mut()
                .context("closure member document is not an object")?;
            object.insert("edges".into(), serde_json::to_value(&edges)?);
            object.insert(
                "declarations".into(),
                declaration_document(&member.declarations),
            );
        }
        documents.push(document);
    }
    let mut document = json!({
        "schema": UNIT_ENVIRONMENT_SCHEMA,
        "roots": roots,
        "members": documents,
    });
    if version >= 2 {
        document
            .as_object_mut()
            .context("closure document is not an object")?
            .insert("closure_version".into(), json!(version));
    }
    Ok(Digest::blake3(&serde_json::to_vec(&document)?))
}

/// The canonical JSON one declaration is hashed and reported as. An
/// unrecorded declaration is its own state, not an empty declaration.
pub fn declaration_document(declarations: &PackageDeclaration) -> Value {
    match declarations.capabilities() {
        Some(capabilities) => json!({
            "state": "declared",
            "capabilities": capabilities.normalized(),
        }),
        None => json!({"state": "unrecorded"}),
    }
}

/// A refusal that names every problem once, in a stable order, instead of one
/// arbitrary first failure.
fn environment_problems(mut problems: Vec<String>) -> String {
    problems.sort();
    problems.dedup();
    problems.join("; ")
}

fn archive_edge(tx: &rusqlite::Transaction<'_>, unit_id: &str, revision: &str, digest: &str, edge_id: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO unit_edge_history(unit_id,revision,artifact_digest,edge_id,data) \
         SELECT unit_id,revision,artifact_digest,edge_id,data FROM unit_edges \
         WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3 AND edge_id=?4",
        params![unit_id, revision, digest, edge_id],
    )?;
    Ok(())
}

/// One installed unit version a dependency declaration can resolve against.
///
/// A candidate carries what resolution actually decides on: the identity, the
/// declared `provides` entries (topic names and typed capability contracts) and
/// the atomic capabilities the version uses. `enabled` is a local fact of this
/// node and never travels with a package.
#[derive(Clone, Debug)]
struct Candidate {
    unit_id: String,
    revision: String,
    digest: Digest,
    provides: Vec<String>,
    enabled: bool,
    atoms: Vec<AtomicCapability>,
}

impl Candidate {
    fn of(descriptor: &UnitDescriptor, enabled: bool) -> Result<Self> {
        Ok(Self {
            unit_id: descriptor.unit_id.clone(),
            revision: descriptor.revision.clone(),
            digest: descriptor.artifact_digest.clone(),
            provides: descriptor.provides.clone(),
            enabled,
            atoms: descriptor
                .package_declaration()?
                .capabilities()
                .map(|capabilities| capabilities.normalized().atoms)
                .unwrap_or_default(),
        })
    }
}

/// Current versions come first; retained non-current versions remain valid
/// exact-digest candidates. This runs inside the import/resolution transaction.
fn resolution_candidates(conn: &rusqlite::Connection) -> Result<Vec<Candidate>> {
    let current: Vec<(String, String, String, Option<String>, i64)> = conn.prepare(
        "SELECT c.unit_id,c.revision,c.artifact_digest,v.data,c.enabled FROM unit_current AS c \
         LEFT JOIN unit_versions AS v ON v.unit_id=c.unit_id AND v.revision=c.revision AND v.artifact_digest=c.artifact_digest \
         ORDER BY c.unit_id"
    )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let retained: Vec<(String, String, String, Option<String>, i64)> = conn.prepare(
        "SELECT v.unit_id,v.revision,v.artifact_digest,v.data,COALESCE(c.enabled,0) FROM unit_versions AS v \
         LEFT JOIN unit_current AS c ON c.unit_id=v.unit_id AND c.revision=v.revision AND c.artifact_digest=v.artifact_digest \
         ORDER BY v.unit_id,v.revision,v.artifact_digest"
    )?.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut available: Vec<Candidate> = Vec::new();
    for (unit_id, revision, digest, data, enabled) in current.into_iter().chain(retained) {
        let stored: StoredVersion = serde_json::from_str(&data.context("current unit version is not retained")?)?;
        ensure!(stored.unit.unit_id == unit_id && stored.unit.revision == revision && stored.unit.artifact_digest.0 == digest,
            "stored unit version identity does not match its key");
        if !available.iter().any(|candidate| candidate.unit_id == unit_id
            && candidate.revision == revision && candidate.digest == stored.unit.artifact_digest) {
            available.push(Candidate::of(&stored.unit, enabled != 0)?);
        }
    }
    Ok(available)
}

fn validate_descriptor(unit: &UnitDescriptor) -> Result<()> {
    ensure!(
        !unit.unit_id.is_empty() && unit.unit_id.len() <= 128,
        "invalid unit identity"
    );
    ensure!(
        !unit.revision.is_empty() && unit.revision.len() <= 64,
        "invalid unit revision"
    );
    ensure!(
        unit.dependencies.len() <= MAX_EDGES,
        "too many declared dependencies"
    );
    ensure!(
        unit.provides.len() <= MAX_PROVIDES,
        "too many declared capabilities"
    );
    let mut edge_ids: Vec<&str> = Vec::new();
    for edge in &unit.dependencies {
        ensure!(
            !edge.edge_id.is_empty() && edge.edge_id.len() <= 128,
            "invalid dependency edge identity"
        );
        ensure!(
            !edge_ids.contains(&edge.edge_id.as_str()),
            "duplicate dependency edge identity"
        );
        edge_ids.push(&edge.edge_id);
        match &edge.selector {
            DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } => {
                ensure!(
                    !unit_id.is_empty() && unit_id.len() <= 128,
                    "invalid exact dependency target"
                );
                ensure!(
                    revision_requirement.is_some() || artifact_digest.is_some(),
                    "an exact dependency must pin a revision requirement or an artifact digest"
                );
            }
            DependencySelector::Abstract {
                capability_ref,
                topic_ref,
            } => {
                ensure!(
                    capability_ref.is_some() || topic_ref.is_some(),
                    "an abstract dependency must name a capability or a topic"
                );
            }
        }
    }
    Ok(())
}

/// A transferred pin is a historical selection, not permission to pick a
/// different candidate. If its target is absent, retain the reference and
/// retry only that target when it arrives.
fn carry_edge(
    spec: &DependencySpec,
    edge: &ResolvedDependency,
    available: &[(String, String, Digest, Vec<String>)],
) -> Result<ResolvedDependency> {
    ensure!(edge_matches_spec(edge, spec), "dependency selection does not match its declaration");
    let selected = match (&edge.selected_unit_id, &edge.selected_revision, &edge.selected_artifact_digest) {
        (None, None, None) => None,
        (Some(id), Some(revision), Some(digest)) => Some((id, revision, digest)),
        _ => bail!("dependency selection has an incomplete pinned reference"),
    };
    if selected.is_none() {
        ensure!(edge.status != EdgeStatus::Satisfied, "satisfied dependency has no selected reference");
        ensure!(edge.status != EdgeStatus::Omitted || spec.strength == DependencyStrength::Recommended,
            "required dependency cannot be omitted");
        return Ok(edge.clone());
    }
    ensure!(matches!(edge.status, EdgeStatus::Satisfied | EdgeStatus::Unresolved),
        "dependency selection conflicts with its status");
    let (id, revision, digest) = selected.unwrap();
    if let DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } = &spec.selector {
        ensure!(id == unit_id
            && revision_requirement.as_ref().is_none_or(|requirement| revision_matches(requirement, revision))
            && artifact_digest.as_ref().is_none_or(|expected| expected == digest),
            "pinned dependency does not satisfy its exact selector");
    }
    let candidate = available.iter().find(|candidate| &candidate.0 == id && &candidate.1 == revision && &candidate.2 == digest);
    let mut result = edge.clone();
    if let Some(candidate) = candidate {
        let checked = resolve_edge(spec, std::slice::from_ref(candidate));
        if checked.status != EdgeStatus::Satisfied {
            result.status = EdgeStatus::Unresolved;
            result.reason = "the pinned dependency no longer satisfies its declaration; its exact selection is retained".into();
        } else if result.status != EdgeStatus::Satisfied {
            result.status = EdgeStatus::Satisfied;
            result.reason = checked.reason;
        }
    } else {
        result.status = EdgeStatus::Unresolved;
        result.reason = "the pinned dependency version is not retained; its selection is preserved until that exact version arrives".into();
    }
    Ok(result)
}

fn selection_matches(spec: &DependencySpec, unit_id: &str, revision: &str, digest: &Digest, provides: &[String]) -> bool {
    match &spec.selector {
        DependencySelector::Exact { unit_id: expected, revision_requirement, artifact_digest, contract_requirement: _ } => {
            unit_id == expected
                && revision_requirement.as_ref().is_none_or(|requirement| revision_matches(requirement, revision))
                && artifact_digest.as_ref().is_none_or(|expected| expected == digest)
        }
        DependencySelector::Abstract { capability_ref, topic_ref } => {
            capability_ref.as_ref().is_none_or(|reference| provides.contains(reference))
                && topic_ref.as_ref().is_none_or(|reference| provides.contains(reference))
        }
    }
}

/// Resolve one declared edge against the available candidate set.
fn resolve_edge(
    spec: &DependencySpec,
    available: &[(String, String, Digest, Vec<String>)],
) -> ResolvedDependency {
    let mut resolved = ResolvedDependency {
        edge_id: spec.edge_id.clone(),
        strength: spec.strength,
        selector: spec.selector.clone(),
        status: EdgeStatus::Unresolved,
        selected_unit_id: None,
        selected_revision: None,
        selected_artifact_digest: None,
        selected_contract: None,
        coexisting_providers: Vec::new(),
        used_capabilities: Vec::new(),
        reason: String::new(),
    };
    match &spec.selector {
        DependencySelector::Exact { unit_id, revision_requirement, artifact_digest, contract_requirement: _ } => {
            let candidates: Vec<&(String, String, Digest, Vec<String>)> = available
                .iter()
                .filter(|candidate| &candidate.0 == unit_id)
                .collect();
            if candidates.is_empty() {
                resolved.reason = format!("no unit named {unit_id} is available here");
                if spec.strength == DependencyStrength::Recommended {
                    resolved.status = EdgeStatus::Omitted;
                    resolved.reason = format!("recommended dependency omitted: {}", resolved.reason);
                }
                return resolved;
            }
            let mut mismatch: Option<String> = None;
            for candidate in candidates {
                if let Some(requirement) = revision_requirement {
                    if !revision_matches(requirement, &candidate.1) {
                        mismatch = Some(format!(
                            "revision {} does not satisfy {requirement}",
                            candidate.1
                        ));
                        continue;
                    }
                }
                if let Some(expected) = artifact_digest {
                    if &candidate.2 != expected {
                        mismatch = Some("artifact digest does not match the pinned value".into());
                        continue;
                    }
                }
                resolved.status = EdgeStatus::Satisfied;
                resolved.selected_unit_id = Some(candidate.0.clone());
                resolved.selected_revision = Some(candidate.1.clone());
                resolved.selected_artifact_digest = Some(candidate.2.clone());
                resolved.reason =
                    format!("resolved to unit {} revision {}", candidate.0, candidate.1);
                return resolved;
            }
            resolved.reason =
                mismatch.unwrap_or_else(|| "no candidate satisfies the selector".into());
        }
        DependencySelector::Abstract {
            capability_ref,
            topic_ref,
        } => {
            for candidate in available {
                let capability_ok = match capability_ref {
                    Some(reference) => candidate.3.iter().any(|value| value == reference),
                    None => true,
                };
                let topic_ok = match topic_ref {
                    Some(reference) => candidate.3.iter().any(|value| value == reference),
                    None => true,
                };
                if capability_ok && topic_ok {
                    resolved.status = EdgeStatus::Satisfied;
                    resolved.selected_unit_id = Some(candidate.0.clone());
                    resolved.selected_revision = Some(candidate.1.clone());
                    resolved.selected_artifact_digest = Some(candidate.2.clone());
                    resolved.reason = format!(
                        "unit {} declares providing the requested capability or topic",
                        candidate.0
                    );
                    return resolved;
                }
            }
            resolved.reason =
                "no unit declares providing the requested capability or topic; similarity is not a match"
                    .to_string();
        }
    }
    if spec.strength == DependencyStrength::Recommended {
        // A missing recommendation is omitted, not a failure of the closure.
        resolved.status = EdgeStatus::Omitted;
        resolved.reason = format!("recommended dependency omitted: {}", resolved.reason);
    }
    resolved
}

/// Compare a revision against a simple requirement such as ">=1.0.0 <2.0.0".
fn revision_matches(requirement: &str, revision: &str) -> bool {
    for part in requirement.split_whitespace() {
        let (operator, rest) = if let Some(rest) = part.strip_prefix(">=") {
            (">=", rest)
        } else if let Some(rest) = part.strip_prefix("<=") {
            ("<=", rest)
        } else if let Some(rest) = part.strip_prefix('>') {
            (">", rest)
        } else if let Some(rest) = part.strip_prefix('<') {
            ("<", rest)
        } else if let Some(rest) = part.strip_prefix('=') {
            ("=", rest)
        } else {
            ("=", part)
        };
        let order = compare_versions(revision, rest);
        let ok = match operator {
            ">=" => order != Ordering::Less,
            "<=" => order != Ordering::Greater,
            ">" => order == Ordering::Greater,
            "<" => order == Ordering::Less,
            _ => order == Ordering::Equal,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn compare_versions(left: &str, right: &str) -> Ordering {
    let left: Vec<&str> = left.split(['.', '-', '+']).collect();
    let right: Vec<&str> = right.split(['.', '-', '+']).collect();
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or("0");
        let b = right.get(index).copied().unwrap_or("0");
        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
            (Err(_), Err(_)) => a.cmp(b),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

fn closure_value(edges: &[ResolvedDependency], closure: &RequiredClosure) -> Value {
    let complete = closure.unresolved.is_empty() && closure.unmaterialized.is_empty();
    json!({
        "state": if complete { "complete" } else { "incomplete" },
        "unresolved_required": closure.unresolved.len(),
        "unmaterialized_required": closure.unmaterialized.len(),
        "unresolved_details": &closure.unresolved,
        "unmaterialized_details": &closure.unmaterialized,
        "edges": edges,
    })
}

fn unresolved_edges(edges: &[ResolvedDependency]) -> Value {
    json!(
        edges
            .iter()
            .filter(|edge| matches!(edge.status, EdgeStatus::Unresolved | EdgeStatus::LegacyAmbiguous))
            .collect::<Vec<_>>()
    )
}

fn advisory_notes(applicability: Option<&Applicability>) -> Value {
    let mut notes: Vec<String> = Vec::new();
    match applicability {
        None => notes.push(
            "no applicability was declared; unknown labels stay advisory and never block a trial"
                .to_string(),
        ),
        Some(labels) => {
            match &labels.os {
                Some(os) if os != std::env::consts::OS => notes.push(format!(
                    "declared operating system {os} differs from this node's {}; the label is advisory and a trial is still permitted",
                    std::env::consts::OS
                )),
                Some(_) => notes.push("the declared operating system matches this node".to_string()),
                None => notes.push(
                    "no operating system was declared; the missing label is advisory and a trial is still permitted"
                        .to_string(),
                ),
            }
            match &labels.arch {
                Some(arch) if arch != std::env::consts::ARCH => notes.push(format!(
                    "declared architecture {arch} differs from this node's {}; the label is advisory and a trial is still permitted",
                    std::env::consts::ARCH
                )),
                Some(_) => notes.push("the declared architecture matches this node".to_string()),
                None => notes.push(
                    "no architecture was declared; the missing label is advisory and a trial is still permitted"
                        .to_string(),
                ),
            }
            if let Some(runtime) = &labels.runtime {
                notes.push(format!(
                    "declared runtime {runtime} is a claim to check by trial, not a verified fact"
                ));
            }
        }
    }
    json!(notes)
}

/// Remote evidence retained for provenance. It never counts as local
/// readiness.
fn unit_remote_evidence(conn: &rusqlite::Connection, unit: &InstalledUnit) -> Result<Vec<Value>> {
    let data: String = conn.query_row(
        "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
        params![unit.unit_id, unit.revision, unit.artifact_digest.0],
        |r| r.get(0),
    )?;
    let stored: StoredVersion = serde_json::from_str(&data)?;
    Ok(stored.remote_evidence)
}

fn safe_component(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty() && name.len() <= 128,
        "invalid name component"
    );
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized == name && !sanitized.starts_with('.') {
        Ok(sanitized)
    } else {
        let hex = Digest::blake3(name.as_bytes()).hex().to_string();
        Ok(format!("{sanitized}.{}", &hex[..8]))
    }
}

/// The package root is chosen by the caller and stays private to it.
fn normalize_root(root: &str) -> Result<String> {
    ensure!(!root.trim().is_empty(), "a package root is required");
    let path = PathBuf::from(root);
    if path.exists() {
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "the package root must be a real directory, not a symlink"
        );
    } else {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)?;
    }
    let path = std::fs::canonicalize(path)?;
    Ok(path.to_string_lossy().into_owned())
}

/// Write the actual package bytes and a readable manifest into the root.
fn write_package(directory: &Path, bytes: &[u8], descriptor: &UnitDescriptor) -> Result<String> {
    std::fs::create_dir_all(directory)?;
    let artifact = directory.join("package.bin");
    let temporary = directory.join("package.bin.partial");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, &artifact)?;
    let manifest = serde_json::to_vec_pretty(&json!({
        "schema": UNIT_SCHEMA,
        "unit_id": descriptor.unit_id,
        "kind": descriptor.kind,
        "revision": descriptor.revision,
        "artifact_digest": descriptor.artifact_digest,
        "provides": descriptor.provides,
        "dependencies": descriptor.dependencies,
        "config": descriptor.config,
        "applicability": descriptor.applicability,
    }))?;
    let manifest_path = directory.join("manifest.json");
    let manifest_temporary = directory.join("manifest.json.partial");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&manifest_temporary)?;
        file.write_all(&manifest)?;
        file.sync_all()?;
    }
    std::fs::rename(&manifest_temporary, &manifest_path)?;
    Ok(artifact.to_string_lossy().into_owned())
}

/// Resolve the program a trial will run in its actual child working directory.
fn resolve_program(program: &str, cwd: &Path) -> Option<String> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let path = Path::new(program);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        return if path.is_file() {
            Some(path.to_string_lossy().into_owned())
        } else {
            None
        };
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            cwd.join(candidate)
        };
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// The environment a trial is bound to. It changes when the interpreter, the
/// operating system or the architecture changes, so a previous result cannot
/// be inherited silently.
fn probe_environment(program: &str, cwd: &Path) -> Result<TrialEnvironment> {
    let resolved = resolve_program(program, cwd);
    let mut probe = ProgramProbe {
        program: program.to_string(),
        resolved: resolved.clone(),
        size: None,
        modified_unix: None,
    };
    if let Some(path) = &resolved {
        if let Ok(metadata) = std::fs::metadata(path) {
            probe.size = Some(metadata.len());
            probe.modified_unix = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|delta| delta.as_secs());
        }
    }
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|value| value.trim().to_string());
    let digest = Digest::blake3(&serde_json::to_vec(&json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "kernel": kernel,
        "programs": [&probe],
    }))?);
    Ok(TrialEnvironment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel,
        programs: vec![probe],
        digest,
    })
}

/// Read at most `cap` bytes. The third value reports a failed read, which
/// makes the captured output untrustworthy.
fn drain<R: Read>(mut reader: R, cap: usize) -> (Vec<u8>, bool, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    let mut failed = false;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let room = cap.saturating_sub(kept.len());
                if room > 0 {
                    let take = room.min(count);
                    kept.extend_from_slice(&chunk[..take]);
                    if take < count {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    (kept, truncated, failed)
}

/// How long a trial waits for its captured output before it stops trusting it.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Resolve the effective child working directory before probing or spawning.
/// Persisting this canonical path makes later environment rechecks independent
/// of the Node process's subsequent working directory.
fn trial_workdir(cwd: Option<&str>, unit: &InstalledUnit) -> Result<PathBuf> {
    let path = match cwd {
        Some(value) => PathBuf::from(value),
        None => unit
            .materialization
            .as_ref()
            .map(|materialization| PathBuf::from(&materialization.path))
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone())
            })
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = std::fs::canonicalize(path)?;
    ensure!(path.is_dir(), "trial working directory is not a directory");
    Ok(path)
}

/// Recover the effective directory retained with an older default-cwd trial.
/// A relative historical value is not guessed against a later Node process.
fn evidence_workdir(evidence: &TrialEvidence) -> Option<PathBuf> {
    let path = match &evidence.cwd {
        Some(cwd) => PathBuf::from(cwd),
        None => evidence
            .artifact_path
            .as_ref()
            .and_then(|artifact| Path::new(artifact).parent().map(Path::to_path_buf))?,
    };
    path.is_absolute().then_some(path)
}

/// Expand the `{artifact}` placeholder to the materialized package path.
fn expand_artifact(argv: &[String], artifact: Option<&str>) -> Result<Vec<String>> {
    let mut expanded = Vec::with_capacity(argv.len());
    for argument in argv {
        if argument.contains("{artifact}") {
            let path = artifact.context(
                "the unit is not materialized in a package root, so {artifact} cannot be expanded",
            )?;
            expanded.push(argument.replace("{artifact}", path));
        } else {
            expanded.push(argument.clone());
        }
    }
    Ok(expanded)
}

/// Whether a command references the materialized artifact of a unit.
fn references_artifact(argv: &[String], artifact: &str) -> bool {
    argv.iter()
        .any(|argument| argument.split_whitespace().any(|token| token == artifact))
}

/// Whether the materialized file still is the installed artifact.
fn verify_artifact(path: &str, expected: &Digest) -> bool {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() as usize <= MAX_ARTIFACT_BYTES => {}
        _ => return false,
    }
    match std::fs::read(path) {
        Ok(bytes) => Digest::blake3(&bytes) == *expected,
        Err(_) => false,
    }
}

fn materialization_matches(unit: &InstalledUnit) -> bool {
    unit.materialization
        .as_ref()
        .is_some_and(|materialization| {
            materialization.state == "materialized"
                && !materialization.path.is_empty()
                && verify_artifact(&materialization.path, &unit.artifact_digest)
        })
}

/// Stop a whole process group. A descendant that left the group may survive.
fn signal_process_group(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) == 0 }
}

fn process_group_gone(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    let rc = unsafe { libc::kill(-(pid as libc::pid_t), 0) };
    if rc == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
}

struct OwnerIncarnation<'a> {
    pid: i64,
    start_ticks: u64,
    boot_id: Option<&'a str>,
}

fn parse_owner_incarnation(runtime: &str) -> Option<OwnerIncarnation<'_>> {
    // New identities append a boot id. Schema-1 identities are still parsed
    // by pid/start ticks so a live pre-upgrade command is not retired merely
    // for having the older representation.
    let parts: Vec<&str> = runtime.rsplitn(4, ':').collect();
    if parts.len() == 4 {
        Some(OwnerIncarnation {
            pid: parts[2].parse().ok()?,
            start_ticks: parts[1].parse().ok()?,
            boot_id: Some(parts[0]),
        })
    } else if parts.len() == 3 {
        Some(OwnerIncarnation {
            pid: parts[1].parse().ok()?,
            start_ticks: parts[0].parse().ok()?,
            boot_id: None,
        })
    } else {
        None
    }
}

/// Whether the persisted owner incarnation, rather than merely its numeric
/// pid, is provably gone. EPERM or an unavailable /proc record leaves a live
/// candidate untouched because it cannot prove crash recovery is safe.
fn process_gone(pid: i64, owner_runtime: &str) -> bool {
    if pid <= 0 {
        return true;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return true;
    }
    let Some(expected) = parse_owner_incarnation(owner_runtime) else {
        // A malformed owner coordinate cannot keep an attempt pending forever.
        return true;
    };
    if expected.pid != pid {
        return true;
    }
    if let Some(expected_boot) = expected.boot_id.filter(|boot| *boot != "unknown") {
        let Some(actual_boot) = boot_id() else {
            return false;
        };
        if actual_boot != expected_boot {
            return true;
        }
    }
    if expected.start_ticks == 0 {
        return false;
    }
    match process_start_ticks_for(pid) {
        Some(actual) => actual != expected.start_ticks,
        None => false,
    }
}

/// Identity of the running command. It stays the same for the lifetime of the
/// process, so a reused process id cannot be mistaken for the same command.
fn runtime_id(conn: &rusqlite::Connection) -> Result<String> {
    runtime_id_for_process(conn, i64::from(std::process::id()))
}

fn runtime_id_for_process(conn: &rusqlite::Connection, pid: i64) -> Result<String> {
    let installation: Option<String> = conn
        .query_row("SELECT v FROM meta WHERE k='installation'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(format!(
        "{}:{}:{}:{}",
        installation.unwrap_or_else(|| "uninitialized".to_string()),
        pid,
        process_start_ticks_for(pid).unwrap_or(0),
        boot_id().unwrap_or_else(|| "unknown".to_string())
    ))
}

/// The start time of this process in clock ticks since boot.
fn process_start_ticks_for(pid: i64) -> Option<u64> {
    if pid <= 0 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(") ")?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    fields.get(19)?.parse().ok()
}

fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
}

/// A minimal environment record for evidence whose original was lost.
fn fallback_environment(digest: Digest) -> TrialEnvironment {
    TrialEnvironment {
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        kernel: None,
        programs: vec![],
        digest,
    }
}

/// Read a list of strings from a stored value.
fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(|text| text.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;

    fn scratch(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("shenko-units-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&path)
            .unwrap();
        path
    }

    fn package(
        unit_id: &str,
        revision: &str,
        bytes: &[u8],
        deps: Vec<DependencySpec>,
    ) -> UnitPackage {
        UnitPackage {
            unit: UnitDescriptor {
                unit_id: unit_id.into(),
                kind: UnitKind::Skill,
                revision: revision.into(),
                artifact_digest: Digest::blake3(bytes),
                provides: vec![],
                dependencies: deps,
                config: json!({"mode": "test"}),
                // A foreign platform label stays advisory.
                applicability: Some(Applicability {
                    os: Some("plan9".into()),
                    arch: None,
                    runtime: None,
                    notes: None,
                }),
            },
            artifact_base64: b64_encode(bytes),
            evidence: vec![],
            resolved_dependencies: None,
        }
    }

    fn export(packages: Vec<UnitPackage>) -> UnitExport {
        UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: packages[0].unit.unit_id.clone(),
            root_revision: Some(packages[0].unit.revision.clone()),
            root_artifact_digest: Some(packages[0].unit.artifact_digest.clone()),
            origin_node: Some("node-a".into()),
            enabled: false,
            packages,
        }
    }

    fn trial(unit_id: &str, argv: Vec<&str>) -> UnitRequest {
        UnitRequest::Trial {
            unit_id: unit_id.into(),
            argv: argv.into_iter().map(|value| value.to_string()).collect(),
            cwd: None,
            timeout_ms: Some(10_000),
            risk: "runs a trivial interpreter command".into(),
            benefit: "proves the unit can run in this environment".into(),
            consequence: "a failed command leaves the unit unready here".into(),
        }
    }

    fn exact(edge_id: &str, target: &str, digest: Option<Digest>) -> DependencySpec {
        DependencySpec {
            edge_id: edge_id.into(),
            strength: DependencyStrength::Required,
            selector: DependencySelector::Exact {
                unit_id: target.into(),
                revision_requirement: Some("1.0.0".into()),
                artifact_digest: digest,
            },
            purpose: None,
        }
    }

    fn import(node: &mut Node, root: &Path, packages: Vec<UnitPackage>) -> Value {
        node.handle_unit_request(UnitRequest::Import {
            export: export(packages),
            package_root: root.to_string_lossy().into_owned(),
            enable: None,
        }).unwrap()
    }

    #[test]
    fn digest_versions_keep_independent_edges_and_re_resolve() {
        let dir = scratch("digest-node");
        let root = scratch("digest-packages");
        let mut node = Node::open(&dir).unwrap();
        let first = package("root", "1.0.0", b"root-one", vec![exact("edge", "first", None)]);
        let second = package("root", "1.0.0", b"root-two", vec![exact("edge", "second", None)]);
        import(&mut node, &root, vec![first.clone()]);
        import(&mut node, &root, vec![second.clone()]);
        import(&mut node, &root, vec![package("first", "1.0.0", b"first", vec![])]);
        let first_version = node.installed_unit_version("root", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        let second_version = node.installed_unit_version("root", "1.0.0", &second.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(first_version.edges[0].status, EdgeStatus::Satisfied);
        assert_eq!(first_version.edges[0].selected_unit_id.as_deref(), Some("first"));
        assert_eq!(second_version.edges[0].status, EdgeStatus::Unresolved);
        assert!(node.required_closure(&first_version).unwrap().unresolved.is_empty());
        assert_eq!(node.required_closure(&second_version).unwrap().unresolved.len(), 1);
        assert_ne!(first_version.materialization.as_ref().unwrap().path, second_version.materialization.as_ref().unwrap().path);
        import(&mut node, &root, vec![package("second", "1.0.0", b"second", vec![])]);
        let resolved = node.installed_unit_version("root", "1.0.0", &second.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(resolved.edges[0].selected_unit_id.as_deref(), Some("second"));
        assert!(node.required_closure(&resolved).unwrap().unresolved.is_empty());
    }

    #[test]
    fn incoming_descriptor_supersedes_its_stale_retained_candidate() {
        let mut node = Node::open(&scratch("capability-node")).unwrap();
        let root = scratch("capability-packages");
        let mut provider = package("provider", "1.0.0", b"provider", vec![]);
        provider.unit.provides.push("capability/example".into());
        import(&mut node, &root, vec![provider.clone()]);
        let mut replacement = package("provider", "1.0.0", b"provider-two", vec![]);
        replacement.unit.provides.push("capability/other".into());
        import(&mut node, &root, vec![replacement]);

        // The same retained digest is supplied with a new declaration that no
        // longer provides the capability, together with a new consumer.
        provider.unit.provides.clear();
        let consumer = package("consumer", "1.0.0", b"consumer", vec![DependencySpec {
            edge_id: "provider".into(), strength: DependencyStrength::Required,
            selector: DependencySelector::Abstract { capability_ref: Some("capability/example".into()), topic_ref: None },
            purpose: None,
        }]);
        import(&mut node, &root, vec![consumer, provider]);
        let status = node.unit_status("consumer").unwrap();
        assert_eq!(status["closure"]["state"], json!("incomplete"));
        assert_eq!(status["closure"]["edges"][0]["status"], json!("unresolved"));
        assert_eq!(status["closure"]["edges"][0]["selected_artifact_digest"], Value::Null);
        let mut valid = package("provider-new", "1.0.0", b"provider-new", vec![]);
        valid.unit.provides.push("capability/example".into());
        import(&mut node, &root, vec![valid]);
        assert_eq!(node.unit_status("consumer").unwrap()["closure"]["state"], json!("complete"));
        assert_eq!(node.unit_status("consumer").unwrap()["closure"]["edges"][0]["selected_unit_id"], json!("provider-new"));
    }

    #[test]
    fn exported_root_digest_is_independent_of_package_order() {
        let mut source = Node::open(&scratch("root-binding-source")).unwrap();
        let source_root = scratch("root-binding-source-packages");
        let old = package("self", "1.0.0", b"self-old", vec![]);
        let new = package("self", "1.0.0", b"self-new", vec![
            exact("old", "self", Some(old.unit.artifact_digest.clone())),
        ]);
        import(&mut source, &source_root, vec![old.clone()]);
        import(&mut source, &source_root, vec![new.clone()]);
        let mut transfer = source.export_unit_closure(&source.installed_unit("self").unwrap().unwrap()).unwrap();
        assert_eq!(transfer.root_artifact_digest, Some(new.unit.artifact_digest.clone()));
        assert_eq!(transfer.packages.len(), 2);
        transfer.packages.reverse();
        let mut receiver = Node::open(&scratch("root-binding-receiver")).unwrap();
        let receiver_root = scratch("root-binding-receiver-packages");
        receiver.handle_unit_request(UnitRequest::Import {
            export: transfer, package_root: receiver_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        let status = receiver.unit_status("self").unwrap();
        assert_eq!(status["artifact_digest"], json!(&new.unit.artifact_digest.0));
        assert_eq!(status["closure"]["state"], json!("complete"));
        let retained = receiver.installed_unit_version("self", "1.0.0", &old.unit.artifact_digest).unwrap().unwrap();
        assert!(materialization_matches(&retained));
    }

    #[test]
    fn export_retains_distinct_pins_and_recovers_a_missing_selected_version() {
        let dir = scratch("pin-origin");
        let root = scratch("pin-origin-packages");
        let mut origin = Node::open(&dir).unwrap();
        let one = package("target", "1.0.0", b"target-one", vec![]);
        let two = package("target", "1.0.0", b"target-two", vec![]);
        let a = package("middle", "1.0.0", b"middle-one", vec![exact("target", "target", None)]);
        let b = package("middle", "1.0.0", b"middle-two", vec![exact("target", "target", None)]);
        import(&mut origin, &root, vec![one.clone()]);
        import(&mut origin, &root, vec![a.clone()]);
        import(&mut origin, &root, vec![two.clone()]);
        import(&mut origin, &root, vec![b.clone()]);
        let top = package("top", "1.0.0", b"top", vec![
            exact("a", "middle", Some(a.unit.artifact_digest.clone())),
            exact("b", "middle", Some(b.unit.artifact_digest.clone())),
        ]);
        import(&mut origin, &root, vec![top]);
        let transfer = origin.export_unit_closure(&origin.installed_unit("top").unwrap().unwrap()).unwrap();
        assert_eq!(transfer.packages.len(), 5);

        let mut incomplete = transfer;
        incomplete.packages.retain(|p| p.unit.artifact_digest != one.unit.artifact_digest);
        let mut recovering = Node::open(&scratch("pin-recovery")).unwrap();
        let recovery_root = scratch("pin-recovery-packages");
        recovering.handle_unit_request(UnitRequest::Import {
            export: incomplete, package_root: recovery_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        let pending = recovering.installed_unit_version("middle", "1.0.0", &a.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(pending.edges[0].status, EdgeStatus::Unresolved);
        assert_eq!(pending.edges[0].selected_artifact_digest, Some(one.unit.artifact_digest.clone()));
        assert_eq!(recovering.unit_status("top").unwrap()["closure"]["state"], json!("incomplete"));
        import(&mut recovering, &recovery_root, vec![one]);
        assert_eq!(recovering.unit_status("top").unwrap()["closure"]["state"], json!("complete"));
    }

    #[test]
    fn v3_migration_preserves_identity_history_and_ambiguous_legacy_rows() {
        let dir = scratch("legacy-v3");
        let package_root = scratch("legacy-v3-packages");
        let target = package("target", "1.0.0", b"old-target", vec![]);
        let unique = package("unique", "1.0.0", b"old-unique", vec![exact("edge", "target", None)]);
        let first = package("multi", "1.0.0", b"old-one", vec![exact("edge", "target", None)]);
        let second = package("multi", "1.0.0", b"old-two", vec![exact("edge", "target", None)]);
        let node_id = EntityId::new();
        let ledger = EntityId::new();
        let installation = EntityId::new();
        let old_choice = resolve_edge(&first.unit.dependencies[0], &[(
            target.unit.unit_id.clone(), target.unit.revision.clone(), target.unit.artifact_digest.clone(), vec![],
        )]);
        let old_json = serde_json::to_string(&old_choice).unwrap();
        {
            let mut db = rusqlite::Connection::open(dir.join("node.sqlite3")).unwrap();
            let tx = db.transaction().unwrap();
            tx.execute_batch(crate::NODE_TABLES).unwrap();
            tx.execute_batch(crate::RECEIPT_TABLE).unwrap();
            tx.execute_batch(crate::roles::ROLE_SCHEMA).unwrap();
            tx.execute_batch(crate::peers::SYNC_SCHEMA).unwrap();
            tx.execute_batch(UNIT_TABLES_V3).unwrap();
            crate::set(&tx, "installation", &installation).unwrap();
            crate::set(&tx, "node", &node_id).unwrap();
            crate::set(&tx, "ledger", &ledger).unwrap();
            crate::set(&tx, "identity", &Option::<crate::Identity>::None).unwrap();
            crate::set(&tx, "retired_identities", &Vec::<crate::Identity>::new()).unwrap();
            for package in [&target, &unique, &first, &second] {
                let bytes = b64_decode(&package.artifact_base64).unwrap();
                crate::seal(&tx, &bytes, "application/octet-stream", "unit-package").unwrap();
                let stored = StoredVersion { unit: package.unit.clone(), remote_evidence: vec![json!({"historical": true})], imported_at: "old".into(), origin_node: Some("origin".into()) };
                tx.execute("INSERT INTO unit_versions VALUES(?1,?2,?3,'old',?4)",
                    params![package.unit.unit_id, package.unit.revision, package.unit.artifact_digest.0, serde_json::to_string(&stored).unwrap()]).unwrap();
                tx.execute("INSERT OR IGNORE INTO unit_current VALUES(?1,?2,?3,1,'old')",
                    params![package.unit.unit_id, package.unit.revision, package.unit.artifact_digest.0]).unwrap();
            }
            for id in ["unique", "multi"] {
                tx.execute("INSERT INTO unit_edges VALUES(?1,'1.0.0','edge',?2)", params![id, old_json]).unwrap();
            }
            tx.execute("INSERT INTO unit_edges VALUES('multi','1.0.0','stale','unparsed historical row')", []).unwrap();
            tx.execute("INSERT INTO events(id,source,digest,data) VALUES('historic','source','digest','event bytes')", []).unwrap();
            tx.pragma_update(None, "application_id", crate::APPLICATION_ID).unwrap();
            tx.pragma_update(None, "user_version", 3).unwrap();
            tx.commit().unwrap();
        }
        let mut node = Node::open(&dir).unwrap();
        assert_eq!(node.local_node_id().unwrap(), Some(node_id));
        let retained_installation: String = node.conn.query_row("SELECT v FROM meta WHERE k='installation'", [], |r| r.get(0)).unwrap();
        let retained_ledger: String = node.conn.query_row("SELECT v FROM meta WHERE k='ledger'", [], |r| r.get(0)).unwrap();
        assert_eq!(retained_installation, serde_json::to_string(&installation).unwrap());
        assert_eq!(retained_ledger, serde_json::to_string(&ledger).unwrap());
        let event: (i64, String) = node.conn.query_row("SELECT seq,data FROM events WHERE id='historic'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(event, (1, "event bytes".into()));
        let legacy: Vec<(String, String, String)> = node.conn.prepare("SELECT unit_id,edge_id,data FROM unit_edges_legacy ORDER BY unit_id,edge_id").unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(legacy.len(), 3);
        assert!(legacy.iter().any(|row| row.1 == "stale" && row.2 == "unparsed historical row"));
        let unique_version = node.installed_unit_version("unique", "1.0.0", &unique.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(unique_version.edges[0], old_choice);
        let ambiguous = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(ambiguous.edges[0].status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(ambiguous.edges[0].selected_artifact_digest, None);
        import(&mut node, &package_root, vec![target.clone()]);
        let still_unknown = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(still_unknown.edges[0].status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(node.unit_status("multi").unwrap()["readiness"]["state"], json!("unresolved"));
        // An authored schema-3 reimport is an explicit new decision for this
        // digest, not an attribution of the old shared row.
        import(&mut node, &package_root, vec![first.clone()]);
        let recovered = node.installed_unit_version("multi", "1.0.0", &first.unit.artifact_digest).unwrap().unwrap();
        assert_eq!(recovered.edges[0].status, EdgeStatus::Satisfied);
        assert_eq!(recovered.edges[0].selected_artifact_digest, Some(target.unit.artifact_digest));
        let saved: String = node.conn.query_row(
            "SELECT data FROM unit_version_history WHERE unit_id='multi' AND revision='1.0.0' AND artifact_digest=?1 ORDER BY seq LIMIT 1",
            [&first.unit.artifact_digest.0], |r| r.get(0),
        ).unwrap();
        let saved: StoredVersion = serde_json::from_str(&saved).unwrap();
        assert_eq!(saved.imported_at, "old");
        assert_eq!(saved.origin_node.as_deref(), Some("origin"));
        assert_eq!(saved.remote_evidence, vec![json!({"historical": true})]);
        let saved_edge: String = node.conn.query_row(
            "SELECT data FROM unit_edge_history WHERE unit_id='multi' AND revision='1.0.0' AND artifact_digest=?1 AND edge_id='edge' ORDER BY seq LIMIT 1",
            [&first.unit.artifact_digest.0], |r| r.get(0),
        ).unwrap();
        let saved_edge: ResolvedDependency = serde_json::from_str(&saved_edge).unwrap();
        assert_eq!(saved_edge.status, EdgeStatus::LegacyAmbiguous);
        assert_eq!(saved_edge.selected_artifact_digest, None);
        drop(node);
        let reopened = Node::open(&dir).unwrap();
        assert_eq!(reopened.local_node_id().unwrap(), Some(node_id));
        assert_eq!(reopened.conn.query_row("SELECT count(*) FROM unit_edges_legacy", [], |r| r.get::<_, i64>(0)).unwrap(), 3);
    }

    #[test]
    fn schema_two_transfer_does_not_invent_an_old_selection() {
        let mut node = Node::open(&scratch("old-wire-node")).unwrap();
        let root = scratch("old-wire-packages");
        let dependency = package("target", "1.0.0", b"wire-target", vec![]);
        let source = package("source", "1.0.0", b"wire-source", vec![exact("edge", "target", None)]);
        let mut old_transfer = export(vec![source.clone(), dependency.clone()]);
        old_transfer.schema = 2;
        old_transfer.root_revision = None;
        old_transfer.root_artifact_digest = None;
        let imported = node.handle_unit_request(UnitRequest::Import {
            export: old_transfer, package_root: root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        assert_eq!(imported["dependency_resolutions"][0]["offered_edges"][0]["status"], json!("legacy_ambiguous"));
        import(&mut node, &root, vec![dependency]);
        let transfer = node.export_unit_closure(&node.installed_unit("source").unwrap().unwrap()).unwrap();
        let mut receiver = Node::open(&scratch("old-wire-receiver")).unwrap();
        let receiver_root = scratch("old-wire-receiver-packages");
        receiver.handle_unit_request(UnitRequest::Import {
            export: transfer, package_root: receiver_root.to_string_lossy().into_owned(), enable: None,
        }).unwrap();
        assert_eq!(receiver.unit_status("source").unwrap()["closure"]["edges"][0]["status"], json!("legacy_ambiguous"));
    }

    #[test]
    fn generic_trials_export_with_original_plans_without_inheriting_readiness() {
        let base = std::env::var_os("SHENKO_COMPAT_EVIDENCE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/unit-provenance"));
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&base).unwrap();
        let dir = base.join(format!("generic-provenance-{}", EntityId::new()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let source_root = dir.join("source-packages");
        let receiver_root = dir.join("receiver-packages");
        std::fs::create_dir(&source_root).unwrap();
        std::fs::create_dir(&receiver_root).unwrap();
        let mut source = Node::open(&dir.join("source")).unwrap();
        let package = package("trial.generic", "1.0.0", b"owned trial artifact\n", vec![]);
        let identity = package.unit.clone();
        import(&mut source, &source_root, vec![package]);

        let plan = json!({
            "candidate": {"unit_id": identity.unit_id, "artifact_digest": identity.artifact_digest},
            "conditions": [{"world": "one", "ordinal": 1}, {"world": "two", "ordinal": 2}],
            "numeric_fact": 51.248178375505404_f64,
            "scope": "native provenance mechanics, no model or efficacy claim"
        });
        let begun = source.handle_unit_request(UnitRequest::BeginTrial {
            unit_id: identity.unit_id.clone(),
            revision: identity.revision.clone(),
            artifact_digest: identity.artifact_digest.clone(),
            kind: "sandbox_comparison".into(),
            plan: plan.clone(),
        }).unwrap();
        let attempt_id = begun["attempt_id"].as_str().unwrap().to_string();
        let receipts = vec![
            json!({"digest": identity.artifact_digest, "fact": {"kind": "artifact", "ordinal": 1,
                "numeric_fact": 51.248178375505404_f64}}),
            json!({"digest": begun["plan_digest"], "fact": {"kind": "plan", "ordinal": 2}}),
        ];
        source.handle_unit_request(UnitRequest::FinishTrial {
            attempt_id: attempt_id.clone(),
            outcome: TrialOutcome::Succeeded,
            receipts: receipts.clone(),
        }).unwrap();
        assert_eq!(source.unit_status(&identity.unit_id).unwrap()["readiness"]["state"], json!("unverified"));

        let command = source.handle_unit_request(trial(
            &identity.unit_id,
            vec!["/bin/sh", "-c", "cat \"$1\"; printf '\\nprivate-native-trial-output\\n'", "trial-reader", "{artifact}"],
        )).unwrap();
        assert_eq!(command["outcome"], json!("succeeded"));
        assert!(command["stdout"].as_str().unwrap().contains("private-native-trial-output"));
        assert_eq!(source.unit_status(&identity.unit_id).unwrap()["readiness"]["state"], json!("ready"));

        let interrupted_plan = json!({"scope": "opened but never completed", "artifact": identity.artifact_digest});
        let interrupted = source.handle_unit_request(UnitRequest::BeginTrial {
            unit_id: identity.unit_id.clone(),
            revision: identity.revision.clone(),
            artifact_digest: identity.artifact_digest.clone(),
            kind: "observation_capture".into(),
            plan: interrupted_plan.clone(),
        }).unwrap();
        assert_eq!(source.retire_open_trials().unwrap(), 1);

        let exported = source.handle_unit_request(UnitRequest::Export {
            unit_id: identity.unit_id.clone(),
        }).unwrap();
        let wire = &exported["export"];
        let evidence = wire["packages"][0]["evidence"].as_array().unwrap();
        assert_eq!(evidence.len(), 3);
        let completed = evidence.iter().find(|row| row["attempt_id"] == begun["attempt_id"]).unwrap();
        assert_eq!(completed["kind"], json!("sandbox_comparison"));
        assert_eq!(completed["plan_digest"], begun["plan_digest"]);
        assert_eq!(completed["plan"], plan);
        assert_eq!(completed["receipts"], json!(receipts));
        assert_eq!(completed["outcome"], json!("succeeded"));
        assert_eq!(completed["interrupted"], json!(false));
        let unresolved = evidence.iter().find(|row| row["attempt_id"] == interrupted["attempt_id"]).unwrap();
        assert_eq!(unresolved["kind"], json!("observation_capture"));
        assert_eq!(unresolved["plan"], interrupted_plan);
        assert_eq!(unresolved["outcome"], json!("unknown"));
        assert_eq!(unresolved["interrupted"], json!(true));
        assert_eq!(unresolved["receipts"], json!([]));
        let command_provenance = evidence.iter().find(|row| row["exit_code"] == json!(0)).unwrap();
        assert_eq!(command_provenance["artifact_bound"], json!(true));
        assert!(command_provenance.get("kind").is_none());
        assert!(command_provenance.get("stdout").is_none());
        assert!(!serde_json::to_string(wire).unwrap().contains("private-native-trial-output"));

        let mut receiver = Node::open(&dir.join("receiver")).unwrap();
        receiver.handle_unit_request(UnitRequest::Import {
            export: serde_json::from_value(wire.clone()).unwrap(),
            package_root: receiver_root.to_string_lossy().into_owned(),
            enable: Some(false),
        }).unwrap();
        let received = receiver.unit_status(&identity.unit_id).unwrap();
        assert_eq!(received["readiness"]["state"], json!("unverified"));
        assert_eq!(received["local_trials"], json!(0));
        assert_eq!(received["remote_evidence"], json!(3));
        let stored: String = receiver.conn.query_row(
            "SELECT data FROM unit_versions WHERE unit_id=?1 AND revision=?2 AND artifact_digest=?3",
            params![identity.unit_id, identity.revision, identity.artifact_digest.0],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap()["remote_evidence"], json!(evidence));

        let original: String = source.conn.query_row(
            "SELECT data FROM unit_attempts WHERE attempt_id=?1", [&attempt_id], |row| row.get(0),
        ).unwrap();
        let mut changed: Value = serde_json::from_str(&original).unwrap();
        changed["plan"]["conditions"][0]["world"] = json!("different-plan");
        source.conn.execute(
            "UPDATE unit_attempts SET data=?2 WHERE attempt_id=?1",
            params![attempt_id, serde_json::to_string(&changed).unwrap()],
        ).unwrap();
        assert!(source.handle_unit_request(UnitRequest::Export { unit_id: identity.unit_id.clone() }).is_err());
        source.conn.execute(
            "UPDATE unit_attempts SET data=?2,revision='different-version' WHERE attempt_id=?1",
            params![attempt_id, original],
        ).unwrap();
        assert!(source.handle_unit_request(UnitRequest::Export { unit_id: identity.unit_id.clone() }).is_err());
        source.conn.execute(
            "UPDATE unit_attempts SET revision=?2 WHERE attempt_id=?1",
            params![attempt_id, identity.revision],
        ).unwrap();
    }

    #[test]
    fn package_transfers_between_nodes_without_inheriting_readiness() {
        let dir_a = scratch("node-a");
        let dir_b = scratch("node-b");
        let root_a = scratch("packages-a");
        let root_b = scratch("packages-b");
        let mut a = Node::open(&dir_a).unwrap();
        let dependency = package("demo.skill.base", "1.0.0", b"base-bytes", vec![]);
        let root = package(
            "demo.skill.root",
            "2.1.0",
            b"root-bytes",
            vec![DependencySpec {
                edge_id: "base".into(),
                strength: DependencyStrength::Required,
                selector: DependencySelector::Exact {
                    unit_id: "demo.skill.base".into(),
                    revision_requirement: Some(">=1.0.0 <2.0.0".into()),
                    artifact_digest: None,
                },
                purpose: None,
            }],
        );
        a.handle_unit_request(UnitRequest::Import {
            export: export(vec![root]),
            package_root: root_a.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        // The origin node proves the unit by running the materialized artifact.
        let proven = a
            .handle_unit_request(trial(
                "demo.skill.root",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(proven["outcome"], json!("succeeded"));
        assert_eq!(proven["artifact_bound"], json!(true));
        assert_eq!(proven["readiness_proof"], json!(true));
        assert_eq!(proven["readiness"], json!("unresolved"));
        a.handle_unit_request(UnitRequest::Import {
            export: export(vec![dependency]),
            package_root: root_a.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let resolved = a
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        assert_eq!(resolved["closure"]["state"], json!("complete"));
        assert_eq!(resolved["readiness"]["state"], json!("ready"));
        let exported = a
            .handle_unit_request(UnitRequest::Export {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        let export: UnitExport = serde_json::from_value(exported["export"].clone()).unwrap();

        let mut b = Node::open(&dir_b).unwrap();
        b.handle_unit_request(UnitRequest::Import {
            export,
            package_root: root_b.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let status = b
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        // The receiving node holds the same unit, but it is not ready here.
        assert_eq!(status["unit_id"], json!("demo.skill.root"));
        assert_eq!(status["revision"], json!("2.1.0"));
        assert_eq!(
            status["artifact_digest"],
            json!(Digest::blake3(b"root-bytes").0)
        );
        assert_eq!(status["config"], json!({"mode": "test"}));
        assert_eq!(status["readiness"]["state"], json!("unverified"));
        assert_eq!(status["local_trials"], json!(0));
        assert!(status["remote_evidence"].as_u64().unwrap() >= 1);
        assert_eq!(status["closure"]["state"], json!("complete"));
        assert_eq!(status["materialized"], json!(true));
        // The advisory label does not deny the trial.
        let notes = status["advisory"].as_array().unwrap();
        assert!(notes.iter().any(|n| n.as_str().unwrap().contains("plan9")));
        // An unbound investigation is allowed and recorded, but it is not a
        // readiness proof.
        let investigation = b
            .handle_unit_request(trial("demo.skill.root", vec!["/bin/sh", "-c", "exit 0"]))
            .unwrap();
        assert_eq!(investigation["outcome"], json!("succeeded"));
        assert_eq!(investigation["artifact_bound"], json!(false));
        assert_eq!(investigation["readiness_proof"], json!(false));
        assert_eq!(investigation["readiness"], json!("unverified"));
        // The local proof of the artifact makes this node ready.
        let proven = b
            .handle_unit_request(trial(
                "demo.skill.root",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(proven["outcome"], json!("succeeded"));
        assert_eq!(proven["readiness_proof"], json!(true));
        assert_eq!(proven["readiness"], json!("ready"));
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn trial_records_real_success_and_real_failure() {
        let dir = scratch("trial-node");
        let root = scratch("trial-packages");
        let mut node = Node::open(&dir).unwrap();
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![package(
                "demo.skill.trial",
                "1.0.0",
                b"trial-bytes",
                vec![],
            )]),
            package_root: root.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let good = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/bin/sh", "-c", "cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(good["outcome"], json!("succeeded"));
        assert_eq!(good["readiness_proof"], json!(true));
        assert_eq!(good["readiness"], json!("ready"));

        let bad = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/bin/sh", "-c", "exit 3; cat {artifact}"],
            ))
            .unwrap();
        assert_eq!(bad["outcome"], json!("failed"));
        assert_eq!(bad["exit_code"], json!(3));
        assert_eq!(bad["readiness"], json!("failed"));

        let missing = node
            .handle_unit_request(trial(
                "demo.skill.trial",
                vec!["/nonexistent/shenko-units-probe"],
            ))
            .unwrap();
        assert_eq!(missing["outcome"], json!("failed"));
        assert_eq!(missing["artifact_bound"], json!(false));
        // An unbound failure does not change the readiness of the artifact.
        assert_eq!(missing["readiness"], json!("failed"));

        let evidence = node
            .handle_unit_request(UnitRequest::Evidence {
                unit_id: Some("demo.skill.trial".into()),
                after: "0".into(),
                limit: 32,
            })
            .unwrap();
        let rows = evidence["evidence"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["outcome"], json!("succeeded"));
        assert_eq!(rows[1]["outcome"], json!("failed"));
        assert_eq!(rows[2]["outcome"], json!("failed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unresolved_attempt_becomes_unknown_and_is_never_replayed() {
        let dir = scratch("crash-node");
        let root = scratch("crash-packages");
        let mut node = Node::open(&dir).unwrap();
        node.handle_unit_request(UnitRequest::Import {
            export: export(vec![package(
                "demo.skill.crash",
                "1.0.0",
                b"crash-bytes",
                vec![],
            )]),
            package_root: root.to_string_lossy().into_owned(),
            enable: None,
        })
        .unwrap();
        let environment = probe_environment("/bin/sh", Path::new("/")).unwrap();
        let digest = Digest::blake3(b"crash-bytes");
        let artifact_path = node
            .installed_unit("demo.skill.crash")
            .unwrap()
            .unwrap()
            .materialization
            .unwrap()
            .path;
        let runtime = runtime_id(&node.conn).unwrap();
        let foreign_runtime = runtime_id_for_process(&node.conn, 1).unwrap();
        let mine = EntityId::new().to_string();
        let foreign = EntityId::new().to_string();
        let data = serde_json::to_string(&json!({
            "argv": ["/bin/sh", "-c", "cat {artifact}"],
            "requested_argv": ["/bin/sh", "-c", "cat {artifact}"],
            "cwd": Value::Null,
            "environment": environment,
            "risk": "runs a trivial interpreter command",
            "benefit": "proves the unit can run in this environment",
            "consequence": "a failed command leaves the unit unready here",
            "artifact_bound": true,
            "artifact_path": artifact_path,
        }))
        .unwrap();
        // An attempt of this command, whose child may have run.
        node.conn
            .execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,'demo.skill.crash','1.0.0',?2,?3,'spawning',?4,NULL,?5,?6,?7)",
                params![
                    mine,
                    digest.0,
                    environment.digest.0,
                    now(),
                    i64::from(std::process::id()),
                    runtime,
                    data
                ],
            )
            .unwrap();
        // An attempt of another command that is still alive.
        node.conn
            .execute(
                "INSERT INTO unit_attempts(attempt_id,unit_id,revision,artifact_digest,environment_digest,state,started_at,finished_at,owner_pid,owner_runtime,data) \
                 VALUES(?1,'demo.skill.crash','1.0.0',?2,?3,'spawning',?4,NULL,1,?5,?6)",
                params![
                    foreign,
                    digest.0,
                    environment.digest.0,
                    now(),
                    foreign_runtime,
                    data
                ],
            )
            .unwrap();
        // The next request retires only the attempt whose owner is gone.
        let status = node
            .handle_unit_request(UnitRequest::Status {
                unit_id: "demo.skill.crash".into(),
            })
            .unwrap();
        assert_eq!(status["readiness"]["state"], json!("unknown"));
        assert_eq!(status["local_trials"], json!(1));
        let evidence = node
            .handle_unit_request(UnitRequest::Evidence {
                unit_id: Some("demo.skill.crash".into()),
                after: "0".into(),
                limit: 32,
            })
            .unwrap();
        let rows = evidence["evidence"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["outcome"], json!("unknown"));
        assert_eq!(rows[0]["attempt_id"], json!(mine));
        // Nothing was replayed, and the live attempt of the other command was
        // not relabelled as a crash.
        let attempts: i64 = node
            .conn
            .query_row("SELECT count(*) FROM unit_attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(attempts, 2);
        let pending: i64 = node
            .conn
            .query_row(
                "SELECT count(*) FROM unit_attempts WHERE state='spawning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        // The facts are durable in the node's own store.
        let store = rusqlite::Connection::open(dir.join("node.sqlite3")).unwrap();
        let unknown: i64 = store
            .query_row(
                "SELECT count(*) FROM unit_trials WHERE outcome='unknown'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unknown, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
