//! Effect contracts — SPEC §36.
//!
//! One canonical machine-readable declaration of what an operation touches,
//! what can be reset/restored/replayed/cancelled/compensated, and on what
//! basis the guarantee is claimed. The contract travels with the provider
//! registration, is bound at call time to the actual implementation, and is
//! readable by the subject, the UI and the recovery planner without any of
//! them depending on the plugin's crate.
//!
//! §36.2: reset / restore / replay / cancel / compensate are five different
//! actions and never share one "rollback" label. Read-only, idempotently
//! queryable and externally-visible writes are therefore declared separately.

use crate::ids::EntityId;
use crate::object::VersionedRef;
use serde::{Deserialize, Serialize};

/// The five actions of §36.2. They are not interchangeable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectActionKind {
    /// Clear or rebuild the declared temporary compute/work state.
    Reset,
    /// Restore state captured by a checkpoint. Never rewrites history outside
    /// the declared scope.
    Restore,
    /// Continue from a checkpoint. Re-executing an external call still obeys
    /// that call's retry contract.
    Replay,
    /// Revoke work that has not started, or request that running work stops.
    /// Confirmed stop, partial completion and unknown stay distinct.
    Cancel,
    /// A NEW operation that tries to repair an earlier effect. It may fail or
    /// only partly work; it never rewrites the original effect as "never
    /// happened".
    Compensate,
}

impl EffectActionKind {
    /// Every action kind of the standard profile.
    pub const ALL: [EffectActionKind; 5] = [
        EffectActionKind::Reset,
        EffectActionKind::Restore,
        EffectActionKind::Replay,
        EffectActionKind::Cancel,
        EffectActionKind::Compensate,
    ];
}

/// How an operation reaches a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    Read,
    Write,
    ReadWrite,
    Execute,
    /// The real reach is not known; the declaration must say so rather than
    /// pretend a narrow scope.
    Unknown,
}

/// One resource an operation may touch, and how it is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceScope {
    /// Resource family, e.g. "file", "qq_message", "remote_process".
    pub kind: String,
    /// The selector inside the family. The binding is resolved at call time.
    pub selector: String,
    pub access: ResourceAccess,
}

/// Whether and how a repeated call is safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum IdempotencyGuarantee {
    /// Repeating the call under the same key is safe for the given window.
    Strict {
        key_namespace: String,
        max_age_secs: u64,
    },
    /// Repeating is usually safe but not guaranteed.
    BestEffort,
    /// Repeating is not safe.
    None,
}

/// One possible externally visible effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalEffect {
    /// Effect family, e.g. "message_sent", "file_written", "api_mutation".
    pub kind: String,
    pub description: String,
    /// Whether an inverse operation exists.
    pub reversible: bool,
    /// Whether a compensation operation can be implemented for it.
    pub compensation_possible: bool,
}

/// What cancelling can and cannot guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelContract {
    pub can_cancel_before_start: bool,
    pub can_cancel_in_flight: bool,
    /// True only when a receipt proves the work actually stopped.
    pub cancel_confirms_stopped: bool,
    /// True when a cancelled call may still have produced part of its effect.
    pub may_have_partial_effect: bool,
}

/// How an effect could be compensated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompensationContract {
    /// The operation that performs the compensation.
    pub compensation_operation: String,
    pub preconditions: Vec<String>,
    /// The compensation itself may fail.
    pub may_fail: bool,
    /// Traces of the original effect remain visible after compensation.
    pub leaves_traces: bool,
}

/// What a checkpoint can capture for this operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointContract {
    /// Resource families a checkpoint may capture for this operation.
    pub capturable: Vec<String>,
    /// Explicitly excluded from capture (host mounts, remote machines, …).
    pub excluded: Vec<String>,
    /// True when capture/restore is coordinated across participants.
    pub coordinated: bool,
    /// The consistency level a restore may claim.
    pub consistency: String,
}

/// What happens to in-flight work when the plugin is upgraded, removed or
/// crashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleContract {
    /// What an upgrade does to in-flight calls of this operation.
    pub on_upgrade: String,
    /// What an uninstall does to in-flight calls, connections, spools, cursors
    /// and leases.
    pub on_uninstall: String,
    /// What a crash leaves behind and how it is reconciled.
    pub on_crash: String,
    /// Whether an unknown result stays queryable after the plugin is gone.
    pub receipt_readable_after_removal: bool,
}

/// On what basis the guarantee is claimed — §36.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GuaranteeBasis {
    /// Only declared.
    ClaimOnly,
    /// Constrained at runtime by the provider.
    RuntimeConstrained,
    /// Covered by tests under the stated conditions.
    Tested {
        conditions: String,
        evidence_ref: Option<String>,
    },
    /// Not known.
    Unknown,
}

/// The machine-readable effect contract of ONE operation.
///
/// The declaration helps the subject plan and recover; it never becomes a
/// per-item human approval, and `Unknown` never means "nothing can be undone".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectContract {
    /// Stable identity of this contract.
    pub contract_id: EntityId,
    /// The plugin that owns the operation.
    pub plugin_id: EntityId,
    /// The exact implementation the contract was declared for. A different
    /// implementation is a different contract.
    pub implementation: VersionedRef,
    /// The operation this contract describes.
    pub operation_name: String,
    /// Input/output schema version of the operation.
    pub operation_version: u32,
    /// Resources this operation may read or write.
    pub resource_scope: Vec<ResourceScope>,
    /// Which of the five actions this operation supports.
    pub supported_actions: Vec<EffectActionKind>,
    /// What a checkpoint can capture.
    pub checkpoint: CheckpointContract,
    /// Whether repeating the call is safe.
    pub idempotency: IdempotencyGuarantee,
    /// Possible externally visible effects.
    pub external_effects: Vec<ExternalEffect>,
    /// Cancellation semantics.
    pub cancel_contract: Option<CancelContract>,
    /// Compensation semantics.
    pub compensation: Option<CompensationContract>,
    /// Upgrade / uninstall / crash semantics.
    pub lifecycle: LifecycleContract,
    /// The basis of the guarantee.
    pub guarantee_basis: GuaranteeBasis,
}

impl EffectContract {
    /// True when the operation declares that a repeat is safe under the same
    /// key. Used by recovery to decide between retrying and reconciling.
    pub fn is_idempotent(&self) -> bool {
        matches!(
            self.idempotency,
            IdempotencyGuarantee::Strict { .. } | IdempotencyGuarantee::BestEffort
        )
    }

    /// True when the operation declares an external effect that a local
    /// snapshot cannot undo.
    pub fn has_external_effect(&self) -> bool {
        !self.external_effects.is_empty()
    }

    /// True when the declared basis is only a claim or unknown — the subject
    /// must not treat the boundary as proven.
    pub fn basis_is_unproven(&self) -> bool {
        matches!(
            self.guarantee_basis,
            GuaranteeBasis::ClaimOnly | GuaranteeBasis::Unknown
        )
    }
}

/// Build one effect contract step by step.
///
/// The five actions, the resource scope, the idempotency and the external
/// effects are all explicit: a contract that leaves one of them out says
/// exactly that, instead of implying a guarantee nobody checked.
#[derive(Debug, Clone)]
pub struct EffectContractBuilder {
    contract: EffectContract,
}

impl EffectContractBuilder {
    /// Start the contract of one operation of one implementation.
    ///
    /// The contract identity is derived from the operation and the exact
    /// implementation it describes: the same operation of the same artifact
    /// is always the same contract, and a different implementation is a
    /// different contract.
    pub fn new(
        plugin_id: EntityId,
        implementation: VersionedRef,
        operation_name: impl Into<String>,
        operation_version: u32,
    ) -> Self {
        let operation_name = operation_name.into();
        let mut identity = Vec::new();
        identity.extend_from_slice(plugin_id.to_string().as_bytes());
        identity.push(0);
        identity.extend_from_slice(operation_name.as_bytes());
        identity.push(0);
        identity.extend_from_slice(&operation_version.to_le_bytes());
        identity.push(0);
        identity.extend_from_slice(implementation.artifact.digest.to_string().as_bytes());
        Self {
            contract: EffectContract {
                contract_id: EntityId::derived("effect-contract", &identity),
                plugin_id,
                implementation,
                operation_name,
                operation_version,
                resource_scope: Vec::new(),
                supported_actions: Vec::new(),
                checkpoint: CheckpointContract {
                    capturable: Vec::new(),
                    excluded: Vec::new(),
                    coordinated: false,
                    consistency: "none".to_owned(),
                },
                idempotency: IdempotencyGuarantee::None,
                external_effects: Vec::new(),
                cancel_contract: None,
                compensation: None,
                lifecycle: LifecycleContract {
                    on_upgrade: "the in-flight attempt stays queryable".to_owned(),
                    on_uninstall: "the definition is withdrawn; the attempt stays queryable"
                        .to_owned(),
                    on_crash: "the attempt stays unknown until it is reconciled".to_owned(),
                    receipt_readable_after_removal: true,
                },
                guarantee_basis: GuaranteeBasis::Unknown,
            },
        }
    }

    /// Declare one resource this operation may touch.
    pub fn resource(
        mut self,
        kind: impl Into<String>,
        selector: impl Into<String>,
        access: ResourceAccess,
    ) -> Self {
        self.contract.resource_scope.push(ResourceScope {
            kind: kind.into(),
            selector: selector.into(),
            access,
        });
        self
    }

    /// Declare one supported action of §36.2.
    pub fn action(mut self, action: EffectActionKind) -> Self {
        self.contract.supported_actions.push(action);
        self
    }

    /// Declare the idempotency guarantee of a repeated call.
    pub fn idempotency(mut self, idempotency: IdempotencyGuarantee) -> Self {
        self.contract.idempotency = idempotency;
        self
    }

    /// Declare one possible externally visible effect.
    pub fn external_effect(mut self, effect: ExternalEffect) -> Self {
        self.contract.external_effects.push(effect);
        self
    }

    /// Declare the cancellation semantics.
    pub fn cancel(mut self, contract: CancelContract) -> Self {
        self.contract.cancel_contract = Some(contract);
        self
    }

    /// Declare the compensation semantics.
    pub fn compensation(mut self, contract: CompensationContract) -> Self {
        self.contract.compensation = Some(contract);
        self
    }

    /// Declare what a checkpoint can capture.
    pub fn checkpoint(mut self, contract: CheckpointContract) -> Self {
        self.contract.checkpoint = contract;
        self
    }

    /// Declare the upgrade / uninstall / crash semantics.
    pub fn lifecycle(mut self, contract: LifecycleContract) -> Self {
        self.contract.lifecycle = contract;
        self
    }

    /// Declare on what basis the guarantee is claimed.
    pub fn basis(mut self, basis: GuaranteeBasis) -> Self {
        self.contract.guarantee_basis = basis;
        self
    }

    /// The finished contract.
    pub fn build(self) -> EffectContract {
        self.contract
    }
}
