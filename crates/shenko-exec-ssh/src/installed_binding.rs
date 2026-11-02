//! Resolve an explicitly installed native binding for policy inspection.
//!
//! The installer supplies an existing SshBinding (normally from register).
//! This does not discover targets, read keys/agent sockets, grant permission,
//! or install a production policy. No second registry or persistent secret.
use crate::{SshBinding, SshProviderObjectRef, SshTargetReferenceDescriptor};
use anyhow::{Result, ensure};
use serde::Serialize;
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::Digest};

/// Destination facts supplied to trusted in-process policy code. No key path,
/// agent socket, known_hosts contents, CAS locator or receipt directory.
/// Credential availability and remote identity have NOT been probed.
#[derive(Clone, Serialize, PartialEq, Eq)]
pub struct SshResolvedTargetScope {
    pub installation_id: EntityId,
    pub node_id: EntityId,
    pub target_id: String,
    pub host: String,
    pub port: u16,
    pub account: String,
    pub public_trust_digest: Digest,
    pub binding_digest: Digest,
    /// Installer-owned, non-secret local selection name, never a token/path.
    pub authentication_selection: String,
    pub explicit_key_selected: bool,
    pub agent_selected: bool,
    pub credential_availability: CredentialAvailability,
}

#[derive(Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialAvailability { NotChecked }

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstalledBindingUnavailable {
    InstallationMismatch,
    ReferenceMismatch,
    TargetMismatch,
    PublicTrustUnavailable,
}

/// An immutable installation input, not an authorization capability. Its
/// fields are private and it cannot be deserialized from model JSON. A trusted
/// installer chooses the node/reference and binding; Node registration alone
/// never creates one. Replacement requires a newly installed value.
pub struct InstalledSshBinding {
    installation_id: EntityId,
    node_id: EntityId,
    reference: SshProviderObjectRef,
    binding: SshBinding,
    scope: SshResolvedTargetScope,
}

impl InstalledSshBinding {
    pub fn new(
        store: &Store,
        node_id: EntityId,
        reference: SshProviderObjectRef,
        authentication_selection: String,
        binding: SshBinding,
    ) -> Result<Self> {
        let target = &binding.target;
        ensure!(reference.valid(), "invalid installed binding reference");
        ensure!(!authentication_selection.is_empty() && authentication_selection.len() <= 64
            && authentication_selection.bytes().all(|b|
                b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':')),
            "authentication selection must be a non-secret logical name");
        ensure!(target.password.is_none() && (target.key_path.is_some() || target.use_agent),
            "explicit key/agent selection required; no password");
        ensure!(target.key_path.as_ref().is_none_or(|p| p.is_absolute()),
            "installed key handle must be absolute");
        // Registration has already sealed trust. Never accept a mutable file
        // override or a default OpenSSH trust/credential selection here.
        ensure!(target.known_hosts_path.is_none(), "installed trust must be sealed");
        ensure!(!target.host.is_empty() && !target.host.starts_with('-')
            && target.host.bytes().all(|c| c.is_ascii_alphanumeric() || b".-_:[]%".contains(&c))
            && !target.user.is_empty() && !target.user.starts_with('-')
            && target.user.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
            && target.port != 0, "invalid installed endpoint");
        ensure!(!binding.target_id.is_empty() && binding.receipt_root.starts_with('/')
            && !binding.receipt_root.contains('\0'), "invalid installed target binding");
        ensure!(binding.known_hosts.bytes > 0 && binding.known_hosts.bytes <= 1024 * 1024
            && binding.known_hosts.store_namespace == "ssh-public-trust"
            && binding.known_hosts.codec.name == "openssh-known-hosts"
            && binding.known_hosts.codec.major == 1
            && binding.known_hosts.digest.0.strip_prefix("blake3:").is_some_and(|hex|
                hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
            "invalid installed public-trust reference");
        ensure!(store.cas.verified_object_path(&binding.known_hosts).is_ok(),
            "installed public trust unavailable");
        let installation_id = store.ledger.ledger_id();
        // Correlate the complete local binding without projecting its private
        // handles/locators. A digest is NOT proof of permission or possession.
        let binding_digest = Digest::blake3(&serde_json::to_vec(&(
            "ssh.installed-binding.v1", installation_id, node_id, &reference,
            &authentication_selection, &binding,
        ))?);
        let scope = SshResolvedTargetScope {
            installation_id, node_id, target_id: binding.target_id.clone(),
            host: target.host.clone(), port: target.port, account: target.user.clone(),
            public_trust_digest: binding.known_hosts.digest.clone(), binding_digest,
            authentication_selection, explicit_key_selected: target.key_path.is_some(),
            agent_selected: target.use_agent, credential_availability: CredentialAvailability::NotChecked,
        };
        Ok(Self { installation_id, node_id, reference, binding, scope })
    }

    pub(super) fn resolve(
        &self, store: &Store, descriptor: &SshTargetReferenceDescriptor,
    ) -> std::result::Result<SshResolvedTargetScope, InstalledBindingUnavailable> {
        use InstalledBindingUnavailable as U;
        if store.ledger.ledger_id() != self.installation_id { return Err(U::InstallationMismatch); }
        if self.reference.provider != descriptor.binding_ref.provider
            || self.reference.object_id != descriptor.binding_ref.object_id
            || self.reference.revision != descriptor.binding_ref.revision
        { return Err(U::ReferenceMismatch); }
        if self.node_id != descriptor.node_id || self.binding.target_id != descriptor.target_id {
            return Err(U::TargetMismatch);
        }
        // Read/verify only the sealed public-trust object, not credential
        // bytes or environment. Repeat after policy await; no raw errors leak.
        store.cas.verified_object_path(&self.binding.known_hosts)
            .map_err(|_| U::PublicTrustUnavailable)?;
        Ok(self.scope.clone())
    }
}
