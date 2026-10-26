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
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::{ids::EntityId, object::Digest};
use std::cmp::Ordering;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Wire and storage schema of the installation-unit records.
pub const UNIT_SCHEMA: u32 = 2;

/// Tables owned by this module. The integration owner adds this DDL to the
/// node create batch and to the installation fingerprint, and migrates an
/// older installation by creating these tables and bumping the storage
/// schema; existing rows are never rewritten.
pub const UNIT_TABLES: &str = "
CREATE TABLE unit_versions(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,imported_at TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_current(unit_id TEXT PRIMARY KEY,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,enabled INTEGER NOT NULL,updated_at TEXT NOT NULL);
CREATE TABLE unit_edges(unit_id TEXT NOT NULL,revision TEXT NOT NULL,edge_id TEXT NOT NULL,data TEXT NOT NULL,PRIMARY KEY(unit_id,revision,edge_id));
CREATE TABLE unit_materializations(unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,root TEXT NOT NULL,path TEXT NOT NULL,state TEXT NOT NULL,detail TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(unit_id,revision,artifact_digest));
CREATE TABLE unit_attempts(attempt_id TEXT PRIMARY KEY,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,state TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);
CREATE TABLE unit_trials(seq INTEGER PRIMARY KEY AUTOINCREMENT,attempt_id TEXT NOT NULL,unit_id TEXT NOT NULL,revision TEXT NOT NULL,artifact_digest TEXT NOT NULL,environment_digest TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,outcome TEXT NOT NULL,artifact_bound INTEGER NOT NULL,artifact_verified INTEGER NOT NULL,owner_pid INTEGER NOT NULL,owner_runtime TEXT NOT NULL,data TEXT NOT NULL);";

/// Bounded limits. Every one of them bounds memory or time; none of them is
/// a product permission decision.
pub const MAX_ARTIFACT_BYTES: usize = 1 << 20;
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

fn b64_encode(data: &[u8]) -> String {
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

fn b64_decode(text: &str) -> Result<Vec<u8>> {
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
}

impl UnitKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnitKind::Perception => "perception",
            UnitKind::Awareness => "awareness",
            UnitKind::Skill => "skill",
            UnitKind::Code => "code",
        }
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "match", rename_all = "snake_case", deny_unknown_fields)]
pub enum DependencySelector {
    Exact {
        unit_id: String,
        revision_requirement: Option<String>,
        artifact_digest: Option<Digest>,
    },
    Abstract {
        capability_ref: Option<String>,
        topic_ref: Option<String>,
    },
}

/// One declared dependency edge of a unit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
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

/// One unit inside a transferable package: descriptor, actual bytes and the
/// origin node's safe trial provenance. Raw arguments, environment details,
/// paths and captured output never leave the node's local evidence log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UnitPackage {
    pub unit: UnitDescriptor,
    pub artifact_base64: String,
    pub evidence: Vec<TrialProvenance>,
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
}

/// A resolved edge with the evidence for the choice.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResolvedDependency {
    pub edge_id: String,
    pub strength: DependencyStrength,
    pub selector: DependencySelector,
    pub status: EdgeStatus,
    pub selected_unit_id: Option<String>,
    pub selected_revision: Option<String>,
    pub selected_artifact_digest: Option<Digest>,
    pub reason: String,
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

/// The explicit, node-safe projection of local trial evidence that may travel
/// with a package. This is deliberately a separate type rather than a set of
/// redaction heuristics: arbitrary arguments, environment probes, local paths,
/// free-form trial descriptions and captured stdout/stderr cannot be exported
/// through this interface.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TrialProvenance {
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

impl From<&TrialEvidence> for TrialProvenance {
    fn from(evidence: &TrialEvidence) -> Self {
        Self {
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
        }
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

#[derive(Default)]
struct RequiredClosure {
    unresolved: Vec<String>,
    unmaterialized: Vec<String>,
    root_materialized: bool,
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

    fn unit_import(
        &mut self,
        export: UnitExport,
        package_root: &str,
        enable: Option<bool>,
    ) -> Result<Value> {
        ensure!(export.schema == UNIT_SCHEMA, "unit export schema mismatch");
        ensure!(
            !export.packages.is_empty() && export.packages.len() <= MAX_PACKAGES,
            "unit package count out of bounds"
        );
        let root = normalize_root(package_root)?;
        // Verify every member before touching storage: a package with a wrong
        // digest is rejected as a whole.
        let mut members: Vec<(UnitDescriptor, Vec<u8>, Vec<TrialProvenance>)> = Vec::new();
        for package in &export.packages {
            let bytes = b64_decode(&package.artifact_base64)?;
            ensure!(
                bytes.len() <= MAX_ARTIFACT_BYTES,
                "unit artifact exceeds the size bound"
            );
            ensure!(
                Digest::blake3(&bytes) == package.unit.artifact_digest,
                "unit artifact digest mismatch"
            );
            ensure!(
                package.unit.artifact_digest.algorithm() == "blake3",
                "unsupported artifact digest algorithm"
            );
            validate_descriptor(&package.unit)?;
            // The declared configuration is copied as the package authored it.
            // This module adds no gate on its content: actual local bindings
            // (environment values, secret files, provider grants) live outside
            // the unit configuration and are never read into a package.
            members.push((package.unit.clone(), bytes, package.evidence.clone()));
        }
        // Available candidates: this package plus what is already installed.
        let mut available: Vec<(String, String, Digest, Vec<String>)> = Vec::new();
        for (descriptor, _, _) in &members {
            available.push((
                descriptor.unit_id.clone(),
                descriptor.revision.clone(),
                descriptor.artifact_digest.clone(),
                descriptor.provides.clone(),
            ));
        }
        for unit in self.installed_units()? {
            available.push((
                unit.unit_id.clone(),
                unit.revision.clone(),
                unit.artifact_digest.clone(),
                unit.descriptor.provides.clone(),
            ));
        }
        // Resolve every edge against the whole candidate set.
        let mut resolutions: Vec<(String, String, Vec<ResolvedDependency>)> = Vec::new();
        for (descriptor, _, _) in &members {
            let mut edges = Vec::new();
            for spec in &descriptor.dependencies {
                edges.push(resolve_edge(spec, &available));
            }
            resolutions.push((
                descriptor.unit_id.clone(),
                descriptor.revision.clone(),
                edges,
            ));
        }
        // The previous enable state is read before the write transaction, so
        // the transaction never needs a second borrow of the connection.
        let mut previous_enabled: Vec<bool> = Vec::new();
        for (descriptor, _, _) in &members {
            let existing: Option<i64> = self
                .conn
                .query_row(
                    "SELECT enabled FROM unit_current WHERE unit_id=?1",
                    [&descriptor.unit_id],
                    |r| r.get(0),
                )
                .optional()?;
            previous_enabled.push(existing.map(|value| value != 0).unwrap_or(false));
        }
        let imported_at = now();
        {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            for (index, ((descriptor, bytes, _evidence), (_, revision, edges))) in
                members.iter().zip(resolutions.iter()).enumerate()
            {
                // Package bytes reuse the existing inline object store.
                let stored = crate::seal(&tx, bytes, "application/octet-stream", "unit-package")?;
                ensure!(
                    stored.digest == descriptor.artifact_digest,
                    "stored artifact digest mismatch"
                );
                let record = StoredVersion {
                    unit: descriptor.clone(),
                    remote_evidence: _evidence
                        .iter()
                        .map(serde_json::to_value)
                        .collect::<serde_json::Result<Vec<_>>>()?,
                    imported_at: imported_at.clone(),
                    origin_node: export.origin_node.clone(),
                };
                tx.execute(
                    "INSERT INTO unit_versions(unit_id,revision,artifact_digest,imported_at,data) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id,revision,artifact_digest) DO UPDATE SET \
                     data=excluded.data,imported_at=excluded.imported_at",
                    params![
                        descriptor.unit_id,
                        revision,
                        descriptor.artifact_digest.0,
                        imported_at,
                        serde_json::to_string(&record)?
                    ],
                )?;
                let enabled = if descriptor.unit_id == export.root_unit_id {
                    enable.unwrap_or(previous_enabled[index])
                } else {
                    previous_enabled[index]
                };
                tx.execute(
                    "INSERT INTO unit_current(unit_id,revision,artifact_digest,enabled,updated_at) \
                     VALUES(?1,?2,?3,?4,?5) ON CONFLICT(unit_id) DO UPDATE SET \
                     revision=excluded.revision,artifact_digest=excluded.artifact_digest,\
                     enabled=excluded.enabled,updated_at=excluded.updated_at",
                    params![
                        descriptor.unit_id,
                        revision,
                        descriptor.artifact_digest.0,
                        i64::from(enabled),
                        imported_at
                    ],
                )?;
                for edge in edges {
                    tx.execute(
                        "INSERT INTO unit_edges(unit_id,revision,edge_id,data) VALUES(?1,?2,?3,?4) \
                         ON CONFLICT(unit_id,revision,edge_id) DO UPDATE SET data=excluded.data",
                        params![
                            descriptor.unit_id,
                            revision,
                            edge.edge_id,
                            serde_json::to_string(&edge)?
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
                        revision,
                        descriptor.artifact_digest.0,
                        imported_at
                    ],
                )?;
            }
            tx.commit()?;
        }
        // Materialization happens outside the database transaction; a partial
        // materialization leaves the unit not ready and is retryable.
        let mut statuses = Vec::new();
        for (descriptor, _, _) in &members {
            let status = self.materialize_unit(&descriptor.unit_id, &root)?;
            statuses.push(status);
        }
        Ok(json!({
            "imported": statuses.len(),
            "package_root": root,
            "readiness_basis": "local_trials_only",
            "note": "imported packages are not ready here; only a local trial can make them ready",
            "units": statuses,
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

    /// Write the actual package bytes into the caller-scoped package root.
    fn materialize_unit(&mut self, unit_id: &str, root: &str) -> Result<Value> {
        let unit = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let bytes = self.object_bytes(&unit.artifact_digest)?;
        ensure!(
            Digest::blake3(&bytes) == unit.artifact_digest,
            "stored artifact corruption"
        );
        let directory = Path::new(root)
            .join("units")
            .join(safe_component(&unit.unit_id)?)
            .join(safe_component(&unit.revision)?);
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

    /// Export a unit and the dependency closure this node can actually offer.
    fn unit_export(&self, unit_id: &str) -> Result<Value> {
        let root = self
            .installed_unit(unit_id)?
            .context("unit is not installed on this node")?;
        let export = self.export_unit_closure(&root)?;
        Ok(json!({
            "export": export,
            "unresolved_edges": unresolved_edges(&root.edges),
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
            if seen.contains(&current) || packages.len() >= MAX_PACKAGES {
                continue;
            }
            seen.push(current.clone());
            let Some(unit) =
                self.installed_unit_version(&current.0, &current.1, &current.2)?
            else {
                continue;
            };
            let bytes = self.object_bytes(&unit.artifact_digest)?;
            packages.push(UnitPackage {
                unit: unit.descriptor.clone(),
                artifact_base64: b64_encode(&bytes),
                evidence: self.local_provenance(&unit.unit_id, &unit.artifact_digest)?,
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
            if status.is_some()
                && group_finished
                && out_result.is_some()
                && err_result.is_some()
            {
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
            out_result = Some(
                out_rx
                    .recv_timeout(READ_TIMEOUT)
                    .unwrap_or((Vec::new(), false, true)),
            );
        }
        if err_result.is_none() {
            err_result = Some(
                err_rx
                    .recv_timeout(READ_TIMEOUT)
                    .unwrap_or((Vec::new(), false, true)),
            );
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
        let edges: Vec<ResolvedDependency> = self
            .conn
            .prepare(
                "SELECT data FROM unit_edges WHERE unit_id=?1 AND revision=?2 ORDER BY edge_id",
            )?
            .query_map(params![unit_id, revision], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|value| serde_json::from_str::<ResolvedDependency>(&value))
            .collect::<serde_json::Result<Vec<_>>>()?;
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
        let rows: Vec<String> = self
            .conn
            .prepare(
                "SELECT data FROM unit_trials WHERE unit_id=?1 AND artifact_digest=?2 ORDER BY seq",
            )?
            .query_map(params![unit_id, digest.0], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .map(|value| {
                serde_json::from_str::<TrialEvidence>(&value)
                    .map(|evidence| TrialProvenance::from(&evidence))
            })
            .collect::<serde_json::Result<Vec<_>>>()?)
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
            if visited.contains(&selected) {
                continue;
            }
            let Some(dependency) =
                self.installed_unit_version(&selected.0, &selected.1, &selected.2)?
            else {
                closure.unresolved.push(format!(
                    "{} revision {} edge {} selects a version that is not retained",
                    unit.unit_id, unit.revision, edge.edge_id
                ));
                continue;
            };
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
                format!("a required dependency is not materialized with its selected bytes: {problem}"),
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
                "the last local trial's effective working directory is no longer available"
                    .into(),
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

fn ensure_unit_schema(conn: &rusqlite::Connection) -> Result<()> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
         ('unit_versions','unit_current','unit_edges','unit_materializations','unit_attempts','unit_trials')",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        count == 6,
        "node storage schema lacks the installation-unit tables; migrate the node storage schema first"
    );
    Ok(())
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
            DependencySelector::Exact {
                unit_id,
                revision_requirement,
                artifact_digest,
            } => {
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
        reason: String::new(),
    };
    match &spec.selector {
        DependencySelector::Exact {
            unit_id,
            revision_requirement,
            artifact_digest,
        } => {
            let candidates: Vec<&(String, String, Digest, Vec<String>)> = available
                .iter()
                .filter(|candidate| &candidate.0 == unit_id)
                .collect();
            if candidates.is_empty() {
                resolved.reason = format!("no unit named {unit_id} is available here");
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
        "edges": edges,
    })
}

fn unresolved_edges(edges: &[ResolvedDependency]) -> Value {
    json!(
        edges
            .iter()
            .filter(|edge| edge.status == EdgeStatus::Unresolved)
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
fn unit_remote_evidence(
    conn: &rusqlite::Connection,
    unit: &InstalledUnit,
) -> Result<Vec<Value>> {
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
    unit.materialization.as_ref().is_some_and(|materialization| {
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
        }
    }

    fn export(packages: Vec<UnitPackage>) -> UnitExport {
        UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: packages[0].unit.unit_id.clone(),
            origin_node: Some("node-a".into()),
            enabled: false,
            packages,
        }
    }

    /// The unit tables belong to the integration owner. The tests create them
    /// when they are not part of the node schema yet, and do nothing when
    /// they already are.
    fn install_unit_tables(node: &mut Node) {
        node.conn
            .execute_batch(&UNIT_TABLES.replace("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS "))
            .unwrap();
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

    #[test]
    fn package_transfers_between_nodes_without_inheriting_readiness() {
        let dir_a = scratch("node-a");
        let dir_b = scratch("node-b");
        let root_a = scratch("packages-a");
        let root_b = scratch("packages-b");
        let mut a = Node::open(&dir_a).unwrap();
        install_unit_tables(&mut a);
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
            export: export(vec![root, dependency]),
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
        assert_eq!(proven["readiness"], json!("ready"));
        let exported = a
            .handle_unit_request(UnitRequest::Export {
                unit_id: "demo.skill.root".into(),
            })
            .unwrap();
        let export: UnitExport = serde_json::from_value(exported["export"].clone()).unwrap();

        let mut b = Node::open(&dir_b).unwrap();
        install_unit_tables(&mut b);
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
        install_unit_tables(&mut node);
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
        install_unit_tables(&mut node);
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
