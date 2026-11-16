//! Exact predecessor facts for an ordered Store action.
//!
//! The ordinary cognition snapshot owns validation. This module
//! only makes its immutable rows and object closure shareable through CAS:
//! hash-partitioned pages retain unchanged subtrees across successive actions.
//! No second task database, execution loop or replacement merge policy lives
//! here. A recovered physical-send grant is still evidence, never permission
//! to repeat a send.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use shenko_types::{
    authority::CommitAdmission,
    error::ShenkoError,
    ids::EntityId,
    object::{Digest, ObjectRef},
    subject::{SubjectBranchHead, SubjectStep},
    task::TaskRecord,
};

use crate::{CognitionSnapshotOptions, Store};
use crate::cognition_archive::{CognitionDerivative, CognitionManifest, CognitionRow};

const PAGE_CAPACITY: usize = 32;
const PAGE_CODEC: &str = "store-control-page";
const ROOT_CODEC: &str = "store-control-predecessor";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    key: String,
    value: ObjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edge {
    nibble: u8,
    child: ObjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    schema_version: u32,
    kind: String,
    entries: Vec<Entry>,
    children: Vec<Edge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Predecessor {
    schema_version: u32,
    head: SubjectBranchHead,
    /// Only snapshot metadata. The arrays below are stored in immutable pages.
    manifest: CognitionManifest,
    row_count: usize,
    object_count: usize,
    root_count: usize,
    derivative_count: usize,
    rows: ObjectRef,
    roots: ObjectRef,
    derivatives: ObjectRef,
}

/// Retained evidence is not an installed state transition. Ordinary cognition
/// deliberately parks imported work and does not carry all local control rows;
/// a caller must not use this snapshot to re-execute S1 on another ledger.
#[derive(Debug)]
pub struct ControlPredecessorEvidence {
    pub source_ledger_id: EntityId,
    pub fingerprint: Digest,
    pub original_source: bool,
    pub original_head: SubjectBranchHead,
}

fn failure(error: anyhow::Error) -> ShenkoError {
    ShenkoError::conflict(format!("ordered action predecessor: {error:#}"))
}

impl Store {
    /// Capture one consistent read snapshot before consensus, and require the
    /// exact head the caller used to construct its action. Final row CAS still
    /// happens at the existing Store transaction boundary.
    pub fn prepare_control_predecessor(&self, head: &SubjectBranchHead) -> Result<ObjectRef, ShenkoError> {
        self.prepare_control_predecessor_for_admission(head, &[], &[])
    }

    /// Every expected task/ancestor and registered step must belong to the
    /// SAME SQLite snapshot as the head. Comparing the immutable extracted
    /// rows after the read transaction closes does not re-read live state.
    pub fn prepare_control_predecessor_for_admission(
        &self, head: &SubjectBranchHead, expected_tasks: &[TaskRecord], expected_steps: &[SubjectStep],
    ) -> Result<ObjectRef, ShenkoError> {
        self.prepare_control_predecessor_inner(head, expected_tasks, expected_steps).map_err(failure)
    }

    fn prepare_control_predecessor_inner(
        &self, head: &SubjectBranchHead, expected_tasks: &[TaskRecord], expected_steps: &[SubjectStep],
    ) -> Result<ObjectRef> {
        let export = self.export_cognition(&CognitionSnapshotOptions {
            include_git_history: false,
            ..Default::default()
        })?;
        let mut manifest = export.manifest;
        self.validate_control_manifest(&manifest)?;
        let actual = manifest.rows.iter().find(|row| row.table == "subject_branch_heads"
            && row.key == head.agent_id.to_string()).context("snapshot has no requested subject head")?;
        let actual: SubjectBranchHead = serde_json::from_str(actual.value("data").context("head body missing")?)?;
        ensure!(serde_json::to_value(&actual)? == serde_json::to_value(head)?, "subject predecessor changed before snapshot capture");
        for task in expected_tasks {
            let actual = manifest.rows.iter().find(|row| row.table == "tasks" && row.key == task.task_id.to_string())
                .and_then(|row| row.value("data")).context("expected task is absent from the frozen snapshot")?;
            ensure!(serde_json::from_str::<serde_json::Value>(actual)? == serde_json::to_value(task)?,
                "task or ancestor changed before snapshot capture");
        }
        for step in expected_steps {
            let actual = manifest.rows.iter().find(|row| row.table == "subject_steps" && row.key == step.step_id.to_string())
                .and_then(|row| row.value("data")).context("expected step is absent from the frozen snapshot")?;
            ensure!(serde_json::from_str::<serde_json::Value>(actual)? == serde_json::to_value(step)?,
                "registered step changed before snapshot capture");
        }

        let row_count = manifest.rows.len();
        let object_count = manifest.objects.len();
        let derivative_count = manifest.derivatives.len();
        // Index direct references, never the recursively expanded object
        // inventory. Re-indexing old control wrapper pages in each new
        // snapshot would turn the index itself into a growing workload.
        let mut roots = BTreeMap::new();
        for mut reference in self.cognition_object_roots(&manifest.rows).0 {
            reference.locator.clear();
            let key = Digest::blake3(&serde_json::to_vec(&reference)?).0;
            roots.insert(key.clone(), Entry { key, value: reference });
        }
        let roots = roots.into_values().collect::<Vec<_>>();
        let root_count = roots.len();
        let rows = std::mem::take(&mut manifest.rows).into_iter().map(|row| {
            Ok(Entry { key: format!("{}\u{1f}{}", row.table, row.key),
                value: self.seal_control_value(&row, "store-control-row")? })
        }).collect::<Result<Vec<_>>>()?;
        manifest.objects.clear();
        let derivatives = std::mem::take(&mut manifest.derivatives).into_iter().map(|frame| {
            Ok(Entry { key: format!("{}\u{1f}{}", frame.window_id, frame.idx),
                value: self.seal_control_value(&frame, "store-control-derivative")? })
        }).collect::<Result<Vec<_>>>()?;
        let root = Predecessor {
            schema_version: 1, head: head.clone(), manifest,
            row_count, object_count, root_count, derivative_count,
            rows: self.seal_control_tree("rows", rows, 0)?,
            roots: self.seal_control_tree("roots", roots, 0)?,
            derivatives: self.seal_control_tree("derivatives", derivatives, 0)?,
        };
        self.seal_control_value(&root, ROOT_CODEC)
    }

    /// Verify retained source evidence without changing any live Store rows.
    /// Exact original-source replay has separate action-specific preconditions;
    /// unavailable materialization requires a real availability branch and a
    /// reconciliation record, not an epoch/CommitRef rewrite of the old plan.
    pub fn inspect_control_predecessor(
        &self, reference: &ObjectRef, admission: &CommitAdmission,
    ) -> Result<ControlPredecessorEvidence, ShenkoError> {
        self.inspect_control_predecessor_inner(reference, admission, &[], &[]).map_err(failure)
    }

    /// A typed action which narrows a shared read range must still prove that
    /// its exact source task/ancestor and creator reads belong to the admitted
    /// predecessor. This reads immutable evidence, never installs those rows.
    pub(crate) fn inspect_control_predecessor_for_admission(
        &self, reference: &ObjectRef, admission: &CommitAdmission,
        expected_tasks: &[TaskRecord], expected_steps: &[SubjectStep],
    ) -> Result<ControlPredecessorEvidence, ShenkoError> {
        self.inspect_control_predecessor_inner(reference, admission, expected_tasks, expected_steps)
            .map_err(|error| ShenkoError::other(format!("invalid typed action predecessor: {error:#}")))
    }

    fn inspect_control_predecessor_inner(
        &self, reference: &ObjectRef, admission: &CommitAdmission,
        expected_tasks: &[TaskRecord], expected_steps: &[SubjectStep],
    ) -> Result<ControlPredecessorEvidence> {
        let root: Predecessor = self.read_control_value(reference, ROOT_CODEC)?;
        ensure!(root.schema_version == 1 && admission.schema_version == 1
            && root.head.agent_id == admission.subject_id && root.head.branch_id == admission.branch_id
            && root.head.controller_epoch == admission.controller_epoch
            && root.head.software_generation == admission.software_generation
            && admission.expected_parent.is_none_or(|parent| parent == root.head.committed_at)
            && admission.expected_head_revision.is_none_or(|revision| revision == root.head.record_revision),
            "snapshot does not bind the original ordered action");
        let mut manifest = root.manifest;
        ensure!(manifest.rows.is_empty() && manifest.objects.is_empty() && manifest.derivatives.is_empty()
            && manifest.files.is_empty() && manifest.file_origins.is_empty() && manifest.git_history.is_none(),
            "control snapshot has an inline or filesystem payload");
        manifest.rows = self.read_control_tree("rows", &root.rows, root.row_count)?.into_iter().map(|entry| {
            let row: CognitionRow = self.read_control_value(&entry.value, "store-control-row")?;
            ensure!(entry.key == format!("{}\u{1f}{}", row.table, row.key), "row tree key differs from its body");
            Ok(row)
        }).collect::<Result<Vec<_>>>()?;
        let roots = self.read_control_tree("roots", &root.roots, root.root_count)?.into_iter().map(|entry| {
            ensure!(entry.key == Digest::blake3(&serde_json::to_vec(&entry.value)?).0,
                "root tree key differs from its typed reference");
            Ok(entry.value)
        }).collect::<Result<Vec<_>>>()?;
        let (objects, missing) = self.collect_closure(&roots)?;
        ensure!(missing.is_empty() && objects.len() == root.object_count, "snapshot object closure is incomplete");
        manifest.objects = objects;
        manifest.derivatives = self.read_control_tree("derivatives", &root.derivatives, root.derivative_count)?.into_iter().map(|entry| {
            let frame: CognitionDerivative = self.read_control_value(&entry.value, "store-control-derivative")?;
            ensure!(entry.key == format!("{}\u{1f}{}", frame.window_id, frame.idx), "derivative tree key differs from its body");
            ensure!(Digest::blake3(&frame.bytes).0 == frame.blake3, "derivative bytes differ from their digest");
            Ok(frame)
        }).collect::<Result<Vec<_>>>()?;
        self.validate_control_manifest(&manifest)?;
        let actual: SubjectBranchHead = serde_json::from_str(manifest.rows.iter()
            .find(|row| row.table == "subject_branch_heads" && row.key == root.head.agent_id.to_string())
            .and_then(|row| row.value("data")).context("snapshot subject head is absent")?)?;
        ensure!(serde_json::to_value(&actual)? == serde_json::to_value(&root.head)?, "snapshot head differs from root binding");
        for task in expected_tasks {
            let actual = manifest.rows.iter().find(|row| row.table == "tasks" && row.key == task.task_id.to_string())
                .and_then(|row| row.value("data")).context("typed task is absent from the frozen snapshot")?;
            ensure!(serde_json::from_str::<serde_json::Value>(actual)? == serde_json::to_value(task)?,
                "typed task or ancestor differs from the frozen snapshot");
        }
        for step in expected_steps {
            let actual = manifest.rows.iter().find(|row| row.table == "subject_steps" && row.key == step.step_id.to_string())
                .and_then(|row| row.value("data")).context("typed creator is absent from the frozen snapshot")?;
            ensure!(serde_json::from_str::<serde_json::Value>(actual)? == serde_json::to_value(step)?,
                "typed creator differs from the frozen snapshot");
        }
        let source_ledger_id = manifest.source_ledger_id;
        let fingerprint = manifest.fingerprint.clone();
        Ok(ControlPredecessorEvidence { source_ledger_id, fingerprint,
            original_source: source_ledger_id == self.ledger.ledger_id(), original_head: root.head })
    }

    fn seal_control_value(&self, value: &impl Serialize, codec: &str) -> Result<ObjectRef> {
        Ok(self.cas.seal_object(&serde_json::to_vec(value)?, "application/json",
            "commit-authority", codec, 1)?.object)
    }

    fn verified_control_bytes(&self, reference: &ObjectRef) -> Result<Vec<u8>> {
        let bytes = self.cas.read_object(&reference.digest)?;
        ensure!(bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
            "snapshot object length or digest differs");
        Ok(bytes)
    }

    fn read_control_value<T: DeserializeOwned>(&self, reference: &ObjectRef, codec: &str) -> Result<T> {
        ensure!(reference.codec.name == codec && reference.codec.major == 1
            && reference.media_type == "application/json", "unsupported snapshot object type");
        Ok(serde_json::from_slice(&self.verified_control_bytes(reference)?)?)
    }

    fn seal_control_tree(&self, kind: &str, mut entries: Vec<Entry>, depth: usize) -> Result<ObjectRef> {
        entries.sort_by(|left, right| left.key.cmp(&right.key));
        ensure!(entries.windows(2).all(|pair| pair[0].key != pair[1].key), "duplicate snapshot item identity");
        let mut children = Vec::new();
        if entries.len() > PAGE_CAPACITY && depth < 64 {
            let mut groups: BTreeMap<u8, Vec<Entry>> = BTreeMap::new();
            for entry in std::mem::take(&mut entries) {
                groups.entry(key_path(&entry.key)[depth]).or_default().push(entry);
            }
            for (nibble, group) in groups {
                children.push(Edge { nibble, child: self.seal_control_tree(kind, group, depth + 1)? });
            }
        }
        self.seal_control_value(&Page { schema_version: 1, kind: kind.into(), entries, children }, PAGE_CODEC)
    }

    fn read_control_tree(&self, kind: &str, reference: &ObjectRef, count: usize) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        let mut keys = BTreeSet::new();
        let mut pages = BTreeSet::new();
        let mut pending = vec![(reference.clone(), Vec::<u8>::new())];
        while let Some((reference, path)) = pending.pop() {
            ensure!(pages.insert(reference.digest.0.clone()), "repeated snapshot page");
            let page: Page = self.read_control_value(&reference, PAGE_CODEC)?;
            ensure!(page.schema_version == 1 && page.kind == kind && path.len() <= 64
                && (page.children.is_empty() || page.entries.is_empty())
                && page.children.len() <= 16
                && (page.entries.len() <= PAGE_CAPACITY || path.len() == 64), "invalid snapshot page");
            ensure!(page.children.windows(2).all(|pair| pair[0].nibble < pair[1].nibble), "unordered snapshot children");
            for entry in page.entries {
                ensure!(out.len() < count && keys.insert(entry.key.clone())
                    && key_path(&entry.key).starts_with(&path), "snapshot count, key or path differs");
                out.push(entry);
            }
            for edge in page.children {
                ensure!(edge.nibble < 16 && path.len() < 64, "invalid snapshot edge");
                let mut child_path = path.clone();
                child_path.push(edge.nibble);
                pending.push((edge.child, child_path));
            }
        }
        ensure!(out.len() == count, "incomplete snapshot tree");
        Ok(out)
    }
}

fn key_path(key: &str) -> Vec<u8> {
    blake3::hash(key.as_bytes()).as_bytes().iter().flat_map(|byte| [byte >> 4, byte & 15]).collect()
}
