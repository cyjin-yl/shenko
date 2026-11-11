//! docs/specs/cognition-units.md#spec-51-4: append-node rollback of the owned versioned root.
//!
//! Rollback is not a branch pointer moving backwards. It selects a historical
//! root node and appends a **new** node whose content equals that node, so the
//! chain and the ledger log beside it only grow (`docs/specs/self-update.md#spec-9-5`). Content-addressed
//! objects are reused, never rewritten; the subject's `agent_id` keeps its
//! pre-switch value (the subject did not change, only the failure lane did).
//!
//! The rolled-back range is one root: home files, core and plugin software
//! slots, the `unit_current` cognition selectors, and the WASM linear
//! memories, execution stacks and interpreter object graphs. External effects
//! are the one class that cannot be withdrawn: they are reconciled once and
//! recorded as not undone (`docs/specs/durable-state.md#spec-10-1`). A file/state rollback never claims that a
//! sent message or a remote write was reverted.
//!
//! # Ownership boundary
//!
//! The durable versioned-root manifest, its schema, the selector and the
//! chain storage belong to the versioned-root controller. This module defines
//! **no** second root, manifest or selector: it reads one node's composition
//! through [`RootChain`], appends through the same seam, and records a
//! mechanism receipt of what it verified. The controller reports the state
//! classes a node actually carries; a class it cannot capture is a reported
//! gap ([`RollbackOutcome::missing_classes`]), never silently removed from the
//! goal and never counted as a complete `docs/specs/cognition-units.md#spec-51-4` rollback.
//!
//! A `HomeCheckpoint` that only declares `FilesAndMetadata` is exactly such a
//! gap: the file/metadata class may exist while linear memory, execution stack
//! and interpreter object graph do not. Callers must read
//! [`RollbackOutcome::full_state`] instead of assuming the rollback covered
//! them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

/// Schema of the sealed rollback receipt.
pub const RECEIPT_SCHEMA_VERSION: u32 = 1;
/// CAS namespace of rollback receipts. A subset of `activation`; receipts are
/// mechanism records, not cognition content.
pub const RECEIPT_NAMESPACE: &str = "rollback";
/// CAS codec name of a rollback receipt.
pub const RECEIPT_CODEC: &str = "append-node-receipt";

/// One class of state that returns together with a home version (`docs/specs/cognition-units.md#spec-51-4`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateClass {
    /// Home directory files and their metadata.
    HomeFiles,
    /// Core and implemented-plugin software slots.
    SoftwareSlots,
    /// `unit_current`: the current awareness and perception unit versions.
    CognitionSelectors,
    /// Guest WASM linear memories.
    WasmLinearMemory,
    /// Guest execution stacks.
    ExecutionStack,
    /// Guest interpreter object graph.
    InterpreterObjectGraph,
}

impl StateClass {
    /// Every class one rollback root must carry to be a `docs/specs/cognition-units.md#spec-51-4` rollback.
    ///
    /// This is the normative set. It is not a menu: a root that lacks an entry
    /// is reported as a gap, not redefined as a narrower goal.
    pub const MANDATORY: [StateClass; 6] = [
        StateClass::HomeFiles,
        StateClass::SoftwareSlots,
        StateClass::CognitionSelectors,
        StateClass::WasmLinearMemory,
        StateClass::ExecutionStack,
        StateClass::InterpreterObjectGraph,
    ];

    /// The wire name, matching the serialized form.
    pub fn as_str(self) -> &'static str {
        match self {
            StateClass::HomeFiles => "home_files",
            StateClass::SoftwareSlots => "software_slots",
            StateClass::CognitionSelectors => "cognition_selectors",
            StateClass::WasmLinearMemory => "wasm_linear_memory",
            StateClass::ExecutionStack => "execution_stack",
            StateClass::InterpreterObjectGraph => "interpreter_object_graph",
        }
    }
}

/// The composition of one root node, as the controller reads it back.
///
/// `classes` names the state classes the node carries content for; `objects`
/// are the content-addressed objects its root references. Both are reported by
/// the controller's own manifest. Nothing in this module re-derives them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootComposition {
    /// The node this composition was read from.
    pub node: Digest,
    /// The subject the node belongs to.
    pub agent_id: EntityId,
    /// State classes the node carries content for.
    pub classes: BTreeSet<StateClass>,
    /// Content objects the node's root references.
    pub objects: Vec<Digest>,
}

/// What the controller reports after appending one node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendedNode {
    /// The newly appended node. Never an existing node: the chain grows.
    pub node: Digest,
    /// Objects the append resolved from the CAS instead of writing again.
    pub reused_objects: u64,
}

/// The versioned-root contract this module consumes.
///
/// The controller owns every durable structure behind it. The seam is
/// deliberately narrow so that a rollback cannot invent a second root:
///
/// * [`RootChain::head`] is the node the chain is currently at.
/// * [`RootChain::composition`] reports one node's declared state classes and
///   referenced content objects, or `None` when the chain has no such node.
/// * [`RootChain::append_rollback_node`] appends a **new** node whose root
///   content equals `target` and reuses the target's content-addressed
///   objects. It returns only after the appended node is the durable head;
///   it must never truncate, delete or rewind the chain or its log.
pub trait RootChain: Send + Sync {
    /// The subject this chain belongs to.
    fn agent_id(&self) -> anyhow::Result<EntityId>;
    /// The node the append-only chain is currently at.
    fn head(&self) -> anyhow::Result<Option<Digest>>;
    /// One node's declared composition, or `None` if the chain has no such node.
    fn composition(&self, node: &Digest) -> anyhow::Result<Option<RootComposition>>;
    /// Append a new node whose content equals `target`, reusing its objects.
    fn append_rollback_node(&self, target: &RootComposition) -> anyhow::Result<AppendedNode>;
}

/// One external effect the mechanism ledger still shows, recorded as it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnretractedEffect {
    /// The mechanism that owns the record (for example an operation name).
    pub mechanism: String,
    /// The record identity inside that mechanism.
    pub record: String,
    /// The effect status the ledger reports. `unknown` stays unknown; this
    /// module never upgrades an unresolved effect into a finished one.
    pub effect: String,
}

/// The reconciliation of external effects at rollback time.
///
/// Rollback cannot withdraw an effect that already left the process. It reads
/// the effect ledger once and records what was not undone. When no ledger
/// source is available the record says so: `Unobserved` is a gap in the
/// evidence, never evidence that nothing happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExternalEffectReconciliation {
    /// The ledger was read; these records exist and remain not undone.
    Observed { items: Vec<UnretractedEffect> },
    /// No ledger source was installed; nothing is claimed either way.
    Unobserved { detail: String },
}

impl ExternalEffectReconciliation {
    /// How many effect records were actually read.
    pub fn observed_len(&self) -> usize {
        match self {
            ExternalEffectReconciliation::Observed { items } => items.len(),
            ExternalEffectReconciliation::Unobserved { .. } => 0,
        }
    }
}

/// The mechanism that owns the external-effect ledger.
///
/// Rollback reads it; it never appends, retries or replays there. Implemented
/// by whoever owns the effect records, so the rollback receipt quotes the
/// ledger instead of guessing at it.
pub trait ExternalEffects: Send + Sync {
    /// Effect records for `agent_id` that remain external and unresolved.
    fn unreconciled(&self, agent_id: EntityId) -> anyhow::Result<Vec<UnretractedEffect>>;
}

/// One rollback selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackRequest {
    /// The historical root node the appended node's content must equal.
    pub target: Digest,
    /// The head this selection was made against. A chain that moved since then
    /// refuses, so a rollback never silently absorbs a state it was not
    /// selected against.
    pub expected_head: Option<Digest>,
    /// The software activation this rollback accompanies. Recorded only: a
    /// root rollback does not change the selected generation by itself.
    pub activation: Option<EntityId>,
}

/// The verified result of one append-node rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackOutcome {
    /// Receipt schema; [`RECEIPT_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// The selected historical node.
    pub target: Digest,
    /// The node appended for it. New identity, equal content.
    pub new_node: Digest,
    /// The head the selection was made against.
    pub head_before: Option<Digest>,
    /// The head after the append. Always `new_node`.
    pub head_after: Digest,
    /// The subject. Its value before and after the append was compared.
    pub agent_id: EntityId,
    /// State classes the appended node carries.
    pub classes: BTreeSet<StateClass>,
    /// Mandatory classes the appended node does not carry.
    pub missing_classes: Vec<StateClass>,
    /// True only when every mandatory class is present.
    pub full_state: bool,
    /// Content objects the CAS already held when the append ran (measured
    /// here, by digest, not taken from the controller's word).
    pub reused_objects: u64,
    /// Bytes those reused objects cover, as reported by the CAS.
    pub reused_bytes: u64,
    /// External effects reconciled as not undone.
    pub external_effects: ExternalEffectReconciliation,
    /// The activation this rollback accompanied, if any.
    pub activation: Option<EntityId>,
    /// The sealed receipt of this rollback.
    pub receipt: ObjectRef,
    /// When the receipt was sealed (unix milliseconds).
    pub created_at_ms: i64,
}

/// Why an append-node rollback did not happen, or did not verify.
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    /// A precondition of the append-only contract was not met. The message
    /// names the exact condition; it is the durable refusal reason.
    #[error("rollback refused: {0}")]
    Refused(String),
    /// A store, CAS or controller call failed. The controller's own
    /// persistence rules decide what is durable; nothing is inferred here.
    #[error(transparent)]
    Contract(#[from] anyhow::Error),
}

/// Parse a node digest as the canonical `blake3:<64 lowercase hex>` identity.
///
/// A CLI-selected node is an identity, not a substring or a prefix, so a
/// non-canonical digest is refused instead of resolved.
pub fn parse_node_digest(text: &str) -> Result<Digest, RollbackError> {
    let digest = Digest(text.trim().to_owned());
    if digest.canonical_blake3_hex().is_none() {
        return Err(RollbackError::Refused(format!(
            "not a canonical blake3 node identity: {text:?} (expected blake3:<64 lowercase hex>)"
        )));
    }
    Ok(digest)
}

/// Append one rollback node and verify the result.
///
/// Order of the checks is part of the contract:
///
/// 1. The store has a subject and the chain belongs to it.
/// 2. The chain is still at the head the selection was made against.
/// 3. The selected node exists, belongs to the same subject, and every content
///    object its root references is present in the CAS. A node whose objects
///    are gone cannot be restored faithfully, so nothing is appended.
/// 4. The append returns a node that is not the current head, and that node is
///    the durable head afterwards.
/// 5. The appended node's composition equals the selected node's composition
///    (same classes, same object set), and the CAS reuse it reports covers the
///    objects the selected node references.
/// 6. The subject identity is unchanged.
///
/// Step 5 is what distinguishes an append-node rollback from a rewind: equal
/// content under a new identity. Missing mandatory classes are recorded, never
/// required into existence here — this module restores what the selected node
/// actually holds and reports the rest as a gap.
pub fn append_rollback(
    store: &Store,
    chain: &dyn RootChain,
    request: &RollbackRequest,
    external_effects: ExternalEffectReconciliation,
) -> Result<RollbackOutcome, RollbackError> {
    let agent_before = store
        .get_existing_agent_id()
        .map_err(anyhow::Error::from)?
        .ok_or_else(|| {
            RollbackError::Refused(
                "no local subject: a rollback restores one subject's own root, never a foreign installation's"
                    .to_owned(),
            )
        })?;
    let chain_agent = chain.agent_id()?;
    if chain_agent != agent_before {
        return Err(RollbackError::Refused(format!(
            "root-subject-mismatch: the chain owns subject {chain_agent} and this installation owns {agent_before}; \
             a rollback never installs another installation's root"
        )));
    }
    let head_before = chain.head()?;
    if head_before.as_ref() != request.expected_head.as_ref() {
        return Err(RollbackError::Refused(format!(
            "chain-moved: the selection was made at {} but the chain is at {}; select again from the current chain",
            describe_head(request.expected_head.as_ref()),
            describe_head(head_before.as_ref()),
        )));
    }
    let target = chain.composition(&request.target)?.ok_or_else(|| {
        RollbackError::Refused(format!(
            "unknown-root-node: the chain has no node {}",
            request.target
        ))
    })?;
    if target.node != request.target {
        return Err(RollbackError::Refused(format!(
            "root-node-mismatch: asked for {} and the chain reported {}",
            request.target, target.node
        )));
    }
    if target.agent_id != agent_before {
        return Err(RollbackError::Refused(format!(
            "root-subject-mismatch: node {} belongs to subject {} and this installation owns {agent_before}",
            target.node, target.agent_id
        )));
    }
    let mut missing_objects = Vec::new();
    let mut reused_bytes = 0_u64;
    for object in &target.objects {
        if !store.cas.object_exists(object) {
            missing_objects.push(object.to_string());
            continue;
        }
        reused_bytes = reused_bytes.saturating_add(store.cas.object_size(object).unwrap_or(0));
    }
    if !missing_objects.is_empty() {
        return Err(RollbackError::Refused(format!(
            "root-objects-missing: node {} references {} content objects the CAS does not hold ({}); \
             appending it would claim content that cannot be read back",
            target.node,
            missing_objects.len(),
            missing_objects.join(", ")
        )));
    }
    let missing_classes: Vec<StateClass> = StateClass::MANDATORY
        .iter()
        .copied()
        .filter(|class| !target.classes.contains(class))
        .collect();
    let appended = chain.append_rollback_node(&target)?;
    if head_before.as_ref() == Some(&appended.node) {
        // An append that returns the node the chain was already at is a
        // rewind wearing the append's name: the chain did not grow.
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            "chain-rewound: the append returned the node the chain was already at; docs/specs/cognition-units.md#spec-51-4 rollback has to append a new node",
        ));
    }
    let head_after = chain.head()?;
    if head_after.as_ref() != Some(&appended.node) {
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            &format!(
                "append-not-durable: the chain head reads {} after appending {}",
                describe_head(head_after.as_ref()),
                appended.node
            ),
        ));
    }
    let appended_view = chain.composition(&appended.node)?.ok_or_else(|| {
        RollbackError::Refused(format!(
            "append-unreadable: the chain just appended {} but has no composition for it",
            appended.node
        ))
    })?;
    if appended_view.classes != target.classes {
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            &format!(
                "append-content-mismatch: the appended node carries classes {:?} and the selected node carries {:?}",
                class_names(&appended_view.classes),
                class_names(&target.classes),
            ),
        ));
    }
    if !same_objects(&appended_view.objects, &target.objects) {
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            "append-content-mismatch: the appended node references different content objects than the selected node",
        ));
    }
    if appended.reused_objects < target.objects.len() as u64 {
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            &format!(
                "append-rewrote-objects: the append reported {} reused objects for {} referenced objects; \
                 docs/specs/cognition-units.md#spec-51-4 reuses content-addressed objects instead of writing them again",
                appended.reused_objects,
                target.objects.len(),
            ),
        ));
    }
    let agent_after = store
        .get_existing_agent_id()
        .map_err(anyhow::Error::from)?;
    if agent_after != Some(agent_before) {
        return Err(record_post_append_refusal(
            store,
            request,
            head_before.as_ref(),
            &appended.node,
            &format!(
                "subject-changed-during-rollback: the installation owned {agent_before} before the append and {:?} after it",
                agent_after
            ),
        ));
    }
    let created_at_ms = chrono::Utc::now().timestamp_millis();
    let receipt = RollbackReceipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        target: request.target.clone(),
        new_node: appended.node.clone(),
        head_before: head_before.clone(),
        head_after: appended.node.clone(),
        agent_id: agent_before,
        classes: class_names(&target.classes),
        missing_classes: missing_classes.iter().map(|class| class.as_str()).collect(),
        reused_objects: target.objects.len() as u64,
        reused_bytes,
        external_effects: &external_effects,
        activation: request.activation,
        created_at_ms,
    };
    let receipt_ref = seal_receipt(store, &receipt)?;
    let new_node = appended.node;
    Ok(RollbackOutcome {
        schema_version: RECEIPT_SCHEMA_VERSION,
        target: request.target.clone(),
        new_node: new_node.clone(),
        head_before,
        head_after: new_node,
        agent_id: agent_before,
        classes: target.classes,
        full_state: missing_classes.is_empty(),
        missing_classes,
        reused_objects: target.objects.len() as u64,
        reused_bytes,
        external_effects,
        activation: request.activation,
        receipt: receipt_ref,
        created_at_ms,
    })
}

/// The receipt body. `reused_objects`/`reused_bytes` are the measured values:
/// every referenced object was already in the CAS when the append ran.
#[derive(Debug, Serialize)]
struct RollbackReceipt<'a> {
    schema_version: u32,
    target: Digest,
    new_node: Digest,
    head_before: Option<Digest>,
    head_after: Digest,
    agent_id: EntityId,
    classes: Vec<&'static str>,
    missing_classes: Vec<&'static str>,
    reused_objects: u64,
    reused_bytes: u64,
    external_effects: &'a ExternalEffectReconciliation,
    activation: Option<EntityId>,
    created_at_ms: i64,
}

/// A rollback that appended and then failed verification. The chain grew and
/// that is not undone: the receipt records the append and the exact refusal.
#[derive(Debug, Serialize)]
struct RollbackRefusal<'a> {
    schema_version: u32,
    outcome: &'static str,
    target: Digest,
    head_before: Option<Digest>,
    appended_node: Digest,
    agent_id: EntityId,
    refusal: &'a str,
    created_at_ms: i64,
}

fn record_post_append_refusal(
    store: &Store,
    request: &RollbackRequest,
    head_before: Option<&Digest>,
    appended: &Digest,
    refusal: &str,
) -> RollbackError {
    let agent = store.get_existing_agent_id().ok().flatten();
    let record = RollbackRefusal {
        schema_version: RECEIPT_SCHEMA_VERSION,
        outcome: "refused_after_append",
        target: request.target.clone(),
        head_before: head_before.cloned(),
        appended_node: appended.clone(),
        agent_id: agent.unwrap_or_else(EntityId::nil),
        refusal,
        created_at_ms: chrono::Utc::now().timestamp_millis(),
    };
    match seal_receipt(store, &record) {
        Ok(receipt) => RollbackError::Refused(format!(
            "{refusal}; the chain already grew to {appended} and that append is not undone (receipt {})",
            receipt.digest
        )),
        Err(error) => RollbackError::Refused(format!(
            "{refusal}; the chain already grew to {appended} and the refusal receipt could not be sealed ({error})"
        )),
    }
}

fn seal_receipt(store: &Store, record: &impl Serialize) -> Result<ObjectRef, RollbackError> {
    let bytes = serde_json::to_vec(record).map_err(anyhow::Error::from)?;
    let sealed = store
        .cas
        .seal_object(
            &bytes,
            "application/json",
            RECEIPT_NAMESPACE,
            RECEIPT_CODEC,
            1,
        )
        .map_err(anyhow::Error::from)?;
    Ok(sealed.object)
}

fn describe_head(head: Option<&Digest>) -> String {
    match head {
        Some(digest) => digest.to_string(),
        None => "an empty chain".to_owned(),
    }
}

fn class_names(classes: &BTreeSet<StateClass>) -> Vec<&'static str> {
    classes.iter().map(|class| class.as_str()).collect()
}

/// Object order inside a composition is not part of its meaning.
fn same_objects(left: &[Digest], right: &[Digest]) -> bool {
    let mut left: Vec<&str> = left.iter().map(|digest| digest.0.as_str()).collect();
    let mut right: Vec<&str> = right.iter().map(|digest| digest.0.as_str()).collect();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}
