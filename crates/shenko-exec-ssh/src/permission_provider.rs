//! Read-only bridge to an installed permission policy provider.
//!
//! This invokes real injected code, but is NOT dispatch admission. Target
//! metadata, a provider's policy observation and an execution permit are three
//! different things. Binding/resource resolution and live dispatch checks are
//! still required; this module never constructs a grant, lease or SSH action.
use crate::{
    ReferenceAuthority, ReferenceDispatch, SelectedTargetRequirements, SshAction,
    SshProviderObjectRef, SshTargetReferenceDescriptor, SshTaskRunner,
    TargetReferenceUnavailable,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use shenko_node::{Identity, Node};
use shenko_store::TaskInvocation;
use shenko_types::{ids::EntityId, object::Digest, operation::AuthorityRef};
use std::{fmt, sync::Mutex};

/// Exact requested scope, not proof that the target is bound or currently
/// authorized. The provider owns policy interpretation, privacy, delegation
/// lineage, current-generation checks and revocation. No keyword policy here.
#[derive(Clone, Serialize)]
pub struct SshPermissionScope {
    pub schema: &'static str,
    pub observation_id: EntityId,
    pub invocation: TaskInvocation,
    pub authority: AuthorityRef,
    pub subject: Identity,
    pub observer_node_id: EntityId,
    pub operation_id: EntityId,
    pub decision_id: EntityId,
    pub logical_key: String,
    pub timeout_secs: u64,
    pub descriptor_digest: Digest,
    pub descriptor: SshTargetReferenceDescriptor,
    pub provider_implementation: Digest,
    pub action_digest: Digest,
}

/// Constructed only by the bridge after reading the persisted references.
/// Full action bytes are borrowed for in-process policy inspection, not copied
/// to a diagnostic/result/CAS. A provider must not forward them to a remote
/// policy service without an independently authorized data-access contract.
pub struct SshPermissionRequest<'a> {
    scope: SshPermissionScope,
    request_digest: Digest,
    action: &'a SshAction,
}

impl SshPermissionRequest<'_> {
    pub fn scope(&self) -> &SshPermissionScope { &self.scope }
    pub fn request_digest(&self) -> &Digest { &self.request_digest }
    pub fn action(&self) -> &SshAction { self.action }
}

impl fmt::Debug for SshPermissionRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshPermissionRequest")
            .field("observation_id", &self.scope.observation_id)
            .field("request_digest", &self.request_digest)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum SshPolicyVerdict {
    /// Policy allows the requested logical scope at inspection time. This
    /// does not verify the SSH account/host binding or reserve any resources.
    Allows,
    Denied,
    Unavailable,
}

pub struct SshPermissionReply {
    /// Must echo THIS inspection, including its fresh observation ID.
    pub request_digest: Digest,
    /// Non-secret logical revision of the policy actually consulted.
    pub policy_revision: String,
    pub verdict: SshPolicyVerdict,
}

/// Installed, trusted policy adapter, not supplied by model JSON or loaded
/// from the descriptor's provider string. There is deliberately no default
/// implementation. Inspection must be read-only: no reservations or effects
/// that would need an operation/attempt record for recovery. Identity and
/// implementation must stay stable for a call; replacement uses a new binding.
#[async_trait]
pub trait SshPermissionProvider: Send + Sync {
    fn provider_id(&self) -> &str;
    /// Version identity verified by the trusted installer. Merely returning
    /// this digest does not authenticate an arbitrary Rust implementation.
    fn implementation_digest(&self) -> Digest;
    async fn inspect(&self, request: &SshPermissionRequest<'_>) -> Result<SshPermissionReply>;
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionInspectionUnavailable {
    TargetReference(TargetReferenceUnavailable),
    InvalidAction,
    InvalidInvocationOptions,
    ProviderNotInstalled,
    ProviderIdentityMismatch,
    ProviderImplementationChanged,
    InvalidProviderImplementation,
    ProviderFailed,
    ProviderTimedOut,
    ProviderUnavailable,
    ProviderReplyMismatch,
    InvalidPolicyRevision,
    PolicyDenied,
    ReferencesChanged,
}

/// Safe diagnostic projection. No command, stdin, upload bytes, paths,
/// credential handles or raw provider errors are included. Not a reusable
/// permit: no execution API consumes this result.
#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SelectedPermissionInspection {
    Unavailable { reason: PermissionInspectionUnavailable },
    PolicyObserved {
        observation_id: EntityId,
        operation_id: EntityId,
        request_digest: Digest,
        permission_ref: SshProviderObjectRef,
        provider_implementation: Digest,
        policy_revision: String,
        execution_authority: ReferenceAuthority,
        dispatch: ReferenceDispatch,
    },
}

fn unavailable(reason: PermissionInspectionUnavailable) -> SelectedPermissionInspection {
    SelectedPermissionInspection::Unavailable { reason }
}

impl SshTaskRunner<'_> {
    /// Read the chosen target's references, ask the exact installed policy
    /// provider, then re-read the task/Node references after the await. Nothing
    /// is armed or sent. Repeated calls ask policy again with fresh identities;
    /// no allow cache, fallback provider or automatic retry is used.
    ///
    /// The metadata helper does NOT prove current software generation or that
    /// the historical choice belongs to the current delegation. The trusted
    /// provider must establish those facts, or return Unavailable. Even Allows
    /// is only a policy observation: binding/resource providers, a fresh live
    /// permission check and Store T1 fencing remain dispatch prerequisites.
    pub async fn inspect_selected_permission(
        &self,
        node: &Mutex<Node>,
        ctx: &TaskInvocation,
        operation_id: EntityId,
        authority: &AuthorityRef,
        decision_id: EntityId,
        observer_node_id: EntityId,
        subject: &Identity,
        descriptor_bytes: Option<&[u8]>,
        logical_key: &str,
        action: &SshAction,
        timeout_secs: u64,
        provider: Option<&dyn SshPermissionProvider>,
    ) -> Result<SelectedPermissionInspection> {
        use PermissionInspectionUnavailable as U;
        let before = self.selected_target_requirements(
            node, ctx, operation_id, authority, decision_id,
            observer_node_id, subject, descriptor_bytes,
        )?;
        let (descriptor_digest, descriptor) = match &before {
            SelectedTargetRequirements::Unavailable { reason } => {
                return Ok(unavailable(U::TargetReference(reason.clone())));
            }
            SelectedTargetRequirements::RequiresProviders {
                descriptor_digest, descriptor, ..
            } => (descriptor_digest.clone(), descriptor.clone()),
        };
        if action.validate().is_err() { return Ok(unavailable(U::InvalidAction)); }
        if logical_key.is_empty() || logical_key.len() > 128 || timeout_secs == 0 {
            return Ok(unavailable(U::InvalidInvocationOptions));
        }
        let Some(provider) = provider else { return Ok(unavailable(U::ProviderNotInstalled)); };
        // Fail before handing action/payload bytes to an unrelated provider.
        if provider.provider_id() != descriptor.permission_ref.provider {
            return Ok(unavailable(U::ProviderIdentityMismatch));
        }
        let provider_implementation = provider.implementation_digest();
        if !provider_implementation.0.strip_prefix("blake3:").is_some_and(|hex|
            hex.len() == 64 && hex.bytes().all(|b|
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        {
            return Ok(unavailable(U::InvalidProviderImplementation));
        }
        let scope = SshPermissionScope {
            schema: "ssh.permission-inspection.v1",
            observation_id: EntityId::new(),
            invocation: ctx.clone(), authority: authority.clone(), subject: subject.clone(),
            observer_node_id, operation_id, decision_id, logical_key: logical_key.into(),
            timeout_secs, descriptor_digest, descriptor, provider_implementation,
            action_digest: Digest::blake3(&serde_json::to_vec(action)?),
        };
        // Schema domain + all scope fields + fresh nonce are covered. A hash
        // correlates the response; it is not authentication of plugin code.
        let request_digest = Digest::blake3(&serde_json::to_vec(&scope)?);
        let request = SshPermissionRequest { scope, request_digest, action };
        // Cooperative async deadline only: trusted provider code must not
        // block the executor thread. Timeout never retries or fabricates deny.
        let reply = match tokio::time::timeout(
            std::time::Duration::from_secs(5), provider.inspect(&request),
        ).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => return Ok(unavailable(U::ProviderFailed)),
            Err(_) => return Ok(unavailable(U::ProviderTimedOut)),
        };
        if reply.request_digest != request.request_digest {
            return Ok(unavailable(U::ProviderReplyMismatch));
        }
        if provider.provider_id() != request.scope.descriptor.permission_ref.provider {
            return Ok(unavailable(U::ProviderIdentityMismatch));
        }
        if provider.implementation_digest() != request.scope.provider_implementation {
            return Ok(unavailable(U::ProviderImplementationChanged));
        }
        let revision = &reply.policy_revision;
        if revision.is_empty() || revision.len() > 64 || !revision.bytes().all(|b|
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
        {
            return Ok(unavailable(U::InvalidPolicyRevision));
        }
        match reply.verdict {
            SshPolicyVerdict::Denied => return Ok(unavailable(U::PolicyDenied)),
            SshPolicyVerdict::Unavailable => return Ok(unavailable(U::ProviderUnavailable)),
            SshPolicyVerdict::Allows => {}
        }
        let after = self.selected_target_requirements(
            node, ctx, operation_id, authority, decision_id,
            observer_node_id, subject, descriptor_bytes,
        )?;
        if serde_json::to_value(&before)? != serde_json::to_value(&after)? {
            return Ok(unavailable(U::ReferencesChanged));
        }
        Ok(SelectedPermissionInspection::PolicyObserved {
            observation_id: request.scope.observation_id, operation_id,
            request_digest: request.request_digest,
            permission_ref: request.scope.descriptor.permission_ref,
            provider_implementation: request.scope.provider_implementation,
            policy_revision: reply.policy_revision,
            execution_authority: ReferenceAuthority::NotGranted,
            dispatch: ReferenceDispatch::NotDispatched,
        })
    }
}
