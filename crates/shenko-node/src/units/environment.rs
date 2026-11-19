//! Locked environment generations, selector compare-and-swap and rollback.
//!
//! Generations freeze dependency choices and declarations without adding a
//! second installation store or changing historical closure digest formulas.

use anyhow::{Context, Result, bail, ensure};
use crate::Node;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_types::object::Digest;
use std::cmp::Ordering;
use super::capabilities::{
    HostRequirement, PackageDeclaration, declaration_document, declared_package_capabilities,
};
use super::installation::{InstalledUnit, now};
use super::resolution::{
    DependencyStrength, EdgeStatus, ResolvedDependency, compare_versions, edge_matches_spec,
    selection_matches, selection_of,
};

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

pub(super) fn read_environment_selector(conn: &Connection) -> Result<Option<EnvironmentSelector>> {
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

/// A refusal that names every problem once, in a stable order, instead of one
/// arbitrary first failure.
fn environment_problems(mut problems: Vec<String>) -> String {
    problems.sort();
    problems.dedup();
    problems.join("; ")
}

impl Node {
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
    pub(super) fn prepare_environment(&mut self, roots: &[EnvironmentRoot]) -> Result<Value> {
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
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
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
    pub(super) fn activate_environment(&mut self, generation: u64, expected_revision: u64) -> Result<Value> {
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
        let tx = crate::installation::begin_write(&mut self.conn, self.installation.as_ref())?;
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
    pub(super) fn rollback_environment(&mut self, expected_revision: u64) -> Result<Value> {
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
}
