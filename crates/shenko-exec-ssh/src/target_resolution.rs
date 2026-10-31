//! Task-bound resolution of registered target REFERENCES, never grants.
//!
//! Node's `members` rows only carry a target name and an opaque digest. This
//! reader recognizes a versioned, credential-free descriptor at that digest
//! and reports the three still-unresolved provider dependencies. There is no
//! successful admission/dispatch state and no default allow policy. A future
//! installed SSH child must resolve current binding, permission and resource
//! authority through actual trusted providers before calling the SSH runner.
use crate::{SelectedSshUnavailable, SshTaskRunner, selected_task::check_task};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use shenko_node::{Identity, Node, NodeSelectionOutcome};
use shenko_store::TaskInvocation;
use shenko_types::{ids::EntityId, object::Digest, operation::AuthorityRef};
use std::sync::Mutex;

const SCHEMA: &str = "ssh.target-references.v1";
const MAX_DESCRIPTOR_BYTES: usize = 8 * 1024;

/// A logical provider-owned object/version, NOT a CAS path, credential,
/// permission token or resource lease. Existence/content hashes are not grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshProviderObjectRef {
    pub provider: String,
    pub object_id: String,
    pub revision: String,
}

impl SshProviderObjectRef {
    fn valid(&self) -> bool {
        fn logical(value: &str, max: usize) -> bool {
            !value.is_empty() && value.len() <= max && value.bytes().all(|byte|
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
        }
        logical(&self.provider, 64) && logical(&self.object_id, 128) && logical(&self.revision, 64)
    }
}

/// Immutable metadata whose digest may be registered through existing Node
/// administration. No new registry table, credential store or admin RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshTargetReferenceDescriptor {
    pub schema: String,
    pub node_id: EntityId,
    pub target_id: String,
    pub binding_ref: SshProviderObjectRef,
    pub permission_ref: SshProviderObjectRef,
    pub resource_ref: SshProviderObjectRef,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetReferenceUnavailable {
    NativeInvocation(SelectedSshUnavailable),
    OperationAlreadyArmed,
    SubjectMismatch,
    SelectionReferenceUnavailable,
    SelectionMissing,
    SelectionNotRecorded,
    TargetNotRegistered,
    UnsupportedRegistrationDigest,
    DescriptorUnavailable,
    DescriptorTooLarge,
    DescriptorDigestMismatch,
    DescriptorInvalid,
    DescriptorTargetMismatch,
}

/// Every reference remains unresolved until its owning provider has actually
/// checked current binding/privacy/permissions and task-specific resources.
/// This enum intentionally has NO Authorized/Ready/Admitted variant.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SelectedTargetRequirements {
    Unavailable { reason: TargetReferenceUnavailable },
    RequiresProviders {
        task_id: EntityId,
        operation_id: EntityId,
        decision_id: EntityId,
        descriptor_digest: Digest,
        descriptor: SshTargetReferenceDescriptor,
        dispatch: ReferenceDispatch,
        execution_authority: ReferenceAuthority,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceDispatch { NotDispatched }
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceAuthority { NotGranted }

fn unavailable(reason: TargetReferenceUnavailable) -> SelectedTargetRequirements {
    SelectedTargetRequirements::Unavailable { reason }
}

fn canonical_digest(digest: &Digest) -> bool {
    digest.0.strip_prefix("blake3:").is_some_and(|hex|
        hex.len() == 64 && hex.bytes().all(|byte|
            byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
}

impl SshTaskRunner<'_> {
    /// Inspect dependencies for one explicit choice using the supplied native
    /// invocation's persisted task/assignment bindings. The caller supplies installation/subject from authenticated
    /// runtime state, NEVER model JSON. Descriptor bytes may only match the
    /// Node-registered digest; locators/paths are not accepted or followed.
    ///
    /// This is a local read, not T1, permission resolution or resource admission.
    /// It neither reads provider objects/credentials nor invokes an executor.
    /// An already-armed operation must use its original query/reconcile reader;
    /// changing references here must never redirect recovery to a new target.
    ///
    /// The snapshot and descriptor are historical metadata, not leases. A real
    /// child still needs authoritative live provider checks and a frozen grant/
    /// resource binding at fresh T1, including current generation/owner checks.
    /// This metadata read does not authenticate Rust callers or prove that a
    /// historical choice receipt belongs to the current subject delegation.
    pub fn selected_target_requirements(
        &self,
        node: &Mutex<Node>,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        authority: &AuthorityRef,
        decision_id: EntityId,
        observer_node_id: EntityId,
        subject: &Identity,
        descriptor_bytes: Option<&[u8]>,
    ) -> Result<SelectedTargetRequirements> {
        use TargetReferenceUnavailable as U;
        let task = self.store.external_task(ctx.task_id)?;
        if task.memos.contains_key("external:last_operation") {
            return Ok(unavailable(U::OperationAlreadyArmed));
        }
        if let Some(reason) = check_task(&task, ctx, authority, operation_id)? {
            return Ok(unavailable(U::NativeInvocation(reason)));
        }
        if task.agent_id != subject.agent_id {
            return Ok(unavailable(U::SubjectMismatch));
        }
        let reference = node.lock()
            .map_err(|_| anyhow::anyhow!("node reference lock poisoned"))?
            .selection_target_reference_for_subject(decision_id, observer_node_id, subject);
        let reference = match reference {
            Ok(Some(reference)) => reference,
            Ok(None) => return Ok(unavailable(U::SelectionMissing)),
            Err(_) => return Ok(unavailable(U::SelectionReferenceUnavailable)),
        };
        if !matches!(reference.receipt.outcome, NodeSelectionOutcome::Recorded) {
            return Ok(unavailable(U::SelectionNotRecorded));
        }
        let Some(registered) = reference.registered_target else {
            return Ok(unavailable(U::TargetNotRegistered));
        };
        // Legacy/unknown digest schemes are retained, not reinterpreted as this
        // descriptor. No invalid digest ever reaches a filesystem CAS API.
        if !canonical_digest(&registered.binding_digest) {
            return Ok(unavailable(U::UnsupportedRegistrationDigest));
        }
        let Some(bytes) = descriptor_bytes else {
            return Ok(unavailable(U::DescriptorUnavailable));
        };
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Ok(unavailable(U::DescriptorTooLarge));
        }
        if Digest::blake3(bytes) != registered.binding_digest {
            return Ok(unavailable(U::DescriptorDigestMismatch));
        }
        let descriptor: SshTargetReferenceDescriptor = match serde_json::from_slice(bytes) {
            Ok(descriptor) => descriptor,
            Err(_) => return Ok(unavailable(U::DescriptorInvalid)),
        };
        if descriptor.schema != SCHEMA || !descriptor.binding_ref.valid()
            || !descriptor.permission_ref.valid() || !descriptor.resource_ref.valid()
        {
            return Ok(unavailable(U::DescriptorInvalid));
        }
        if descriptor.node_id != registered.node_id || descriptor.target_id != registered.target_id {
            return Ok(unavailable(U::DescriptorTargetMismatch));
        }
        Ok(SelectedTargetRequirements::RequiresProviders {
            task_id: task.task_id,
            operation_id,
            decision_id,
            descriptor_digest: registered.binding_digest,
            descriptor,
            dispatch: ReferenceDispatch::NotDispatched,
            execution_authority: ReferenceAuthority::NotGranted,
        })
    }
}
