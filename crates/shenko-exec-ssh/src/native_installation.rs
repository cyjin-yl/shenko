//! Explicit node-local SSH installation, not a policy grant or credential store.
//!
//! Configuration is supplied by the runtime installer, never model arguments.
//! Private key bytes are read only by the selected opener AFTER durable T1.
use crate::{SshBinding, SshTarget};
use crate::native_transport::{NativeSshConnection, NativeSshError, NativeSshSession, NativeSshTarget};
use anyhow::{Result, ensure};
use async_trait::async_trait;
use russh::keys::{PublicKey, PrivateKeyWithHashAlg, HashAlg};
use serde::{Deserialize, Serialize};
use shenko_store::Store;
use shenko_types::{ids::EntityId, object::Digest};
use std::{io::Read as _, path::{Path, PathBuf}, sync::Arc, time::Duration};
use tokio::io::AsyncReadExt;
use zeroize::Zeroizing;

const MAX_KEY_BYTES: u64 = 1024 * 1024;

/// Non-secret exact installation identity frozen with the operation. It says
/// which capability was selected, not which OS operations it may perform.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeInstallationSeal {
    pub schema: String,
    pub ledger_id: EntityId,
    pub registered_node_id: EntityId,
    pub revision: String,
    pub binding_digest: Digest,
    pub public_key_digest: Digest,
}

/// Trusted native plugin seam, actually consumed by the installed handler.
/// It cannot be deserialized or selected by a model-provided implementation.
/// Implementations must use only their explicitly installed endpoint/signer;
/// no ambient SSH configuration, identity enumeration or host-ssh fallback.
#[async_trait]
pub trait NativeSessionOpener: Send + Sync {
    async fn open(&self, store: &Store, binding: &SshBinding, budget: Duration)
        -> std::result::Result<NativeSshSession, NativeSshError>;
}

pub struct NativeSshInstallation {
    binding: SshBinding,
    seal: NativeInstallationSeal,
    public_key: PublicKey,
    opener: Arc<dyn NativeSessionOpener>,
}

impl NativeSshInstallation {
    pub fn new(store: &Store, registered_node_id: EntityId, binding: SshBinding, revision: String,
        public_key_openssh: &str, opener: Arc<dyn NativeSessionOpener>) -> Result<Self>
    {
        ensure!(!revision.is_empty() && revision.len() <= 128 && !revision.contains('\0'),
            "invalid SSH installation revision");
        ensure!(!binding.target_id.is_empty() && binding.target_id.len() <= 128
            && binding.target_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b)),
            "invalid installed SSH target identifier");
        NativeSshTarget::from_binding(store, &binding)?;
        let public_key = PublicKey::from_openssh(public_key_openssh)
            .map_err(|_| anyhow::anyhow!("invalid installed SSH public identity"))?;
        let key_bytes = public_key.to_bytes()
            .map_err(|_| anyhow::anyhow!("unsupported installed SSH public identity"))?;
        let seal = NativeInstallationSeal {
            schema: "ssh.native-installation.v1".into(), ledger_id: store.ledger.ledger_id(), registered_node_id, revision,
            binding_digest: NativeSshTarget::binding_digest(&binding)?,
            public_key_digest: Digest::blake3(&key_bytes),
        };
        Ok(Self { binding, seal, public_key, opener })
    }

    pub fn target_id(&self) -> &str { &self.binding.target_id }
    pub fn binding(&self) -> &SshBinding { &self.binding }
    pub fn seal(&self) -> &NativeInstallationSeal { &self.seal }

    pub(crate) fn matches_original(&self, store: &Store, binding: &SshBinding,
        seal: Option<&NativeInstallationSeal>) -> bool
    {
        self.seal.ledger_id == store.ledger.ledger_id()
            && NativeSshTarget::binding_digest(binding).ok().as_ref() == Some(&self.seal.binding_digest)
            && seal.is_some_and(|original| original == &self.seal)
    }

    pub(crate) async fn open_original(&self, store: &Store, binding: &SshBinding,
        seal: Option<&NativeInstallationSeal>, budget: Duration)
        -> std::result::Result<NativeSshSession, NativeSshError>
    {
        if !self.matches_original(store, binding, seal) {
            return Err(NativeSshError::InstallationMismatch);
        }
        let session = self.opener.open(store, binding, budget).await?;
        if !session.matches_binding(binding)? || !session.matches_public_key(&self.public_key) {
            return Err(NativeSshError::InstallationMismatch);
        }
        Ok(session)
    }
}

/// Explicit compatibility mode from SPEC§11. Contains a key LOCATION and
/// expected PUBLIC identity, never a serialized private key/passphrase.
/// Managed/hardware signers use NativeSessionOpener without this file adapter.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSshFileTargetConfig {
    pub node_id: EntityId,
    pub target_id: String,
    pub revision: String,
    pub host: String,
    pub port: u16,
    pub account: String,
    pub private_key_file: PathBuf,
    pub public_key_openssh: String,
    pub known_hosts_file: PathBuf,
    pub receipt_root: String,
}

/// Public configuration observation only. This does not attest possession of
/// the private signer, authentication, endpoint trust or a managed-key history.
/// Intentionally excludes paths, account/host, free-form comments and secrets.
#[derive(Debug, Serialize)]
pub struct ConfiguredSshPublicIdentity {
    pub schema: &'static str,
    pub status: &'static str,
    pub target_id: String,
    pub algorithm: String,
    pub public_key_openssh: String,
    pub fingerprint_sha256: String,
}

impl NativeSshFileTargetConfig {
    /// Inspect only the public identity in one explicitly selected config.
    /// Unlike `install`, this never opens known_hosts/private_key_file, creates
    /// a Store/CAS object, authenticates or connects. Do not infer that any
    /// unselected entry, remote endpoint or private signer has been verified.
    pub fn inspect_public_identity(path: &Path, target_id: &str) -> Result<ConfiguredSshPublicIdentity> {
        ensure!(!target_id.is_empty() && target_id.len() <= 128
            && target_id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-:".contains(&b)),
            "ssh_public_identity_target_invalid");
        let configs = Self::read_installation_file(path)?;
        let mut matching = configs.into_iter().filter(|config| config.target_id == target_id);
        let selected = matching.next().ok_or_else(|| anyhow::anyhow!("ssh_public_identity_target_not_found"))?;
        ensure!(matching.next().is_none(), "ssh_public_identity_target_ambiguous");
        let mut key = PublicKey::from_openssh(&selected.public_key_openssh)
            .map_err(|_| anyhow::anyhow!("ssh_public_identity_invalid"))?;
        // A public-key comment may contain arbitrary private notes. Only the
        // canonical algorithm/key blob is relevant for copy/fingerprint use.
        key.set_comment("");
        let public_key_openssh = key.to_openssh()
            .map_err(|_| anyhow::anyhow!("ssh_public_identity_unsupported"))?;
        Ok(ConfiguredSshPublicIdentity {
            schema: "ssh.configured-public-identity.v1", status: "configured_unverified",
            target_id: selected.target_id, algorithm: key.algorithm().to_string(),
            public_key_openssh, fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        })
    }

    /// Parse only an explicitly selected, bounded regular configuration file.
    /// This does not read private material or install a network connection.
    pub fn read_installation_file(path: &Path) -> Result<Vec<Self>> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FileInstallation { schema: String, targets: Vec<NativeSshFileTargetConfig> }
        let bytes = read_public_installation_file(path)
            .map_err(|_| anyhow::anyhow!("ssh_installation_file_unavailable"))?;
        let config: FileInstallation = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("ssh_installation_file_invalid"))?;
        ensure!(config.schema == "shenko.ssh.file-installation.v1", "ssh_installation_schema_unsupported");
        ensure!(!config.targets.is_empty() && config.targets.len() <= 128,
            "ssh_installation_target_count_invalid");
        Ok(config.targets)
    }

    /// Startup reads only explicitly configured public trust. Missing or
    /// unusable private material is diagnosed when an armed task actually uses
    /// it. No key generation, rotation, HOME or environment credential lookup.
    pub fn install(self, store: &Store) -> Result<NativeSshInstallation> {
        ensure!(self.private_key_file.is_absolute() && self.known_hosts_file.is_absolute(),
            "SSH installation paths must be explicit and absolute");
        let trust = read_public_installation_file(&self.known_hosts_file)
            .map_err(|_| anyhow::anyhow!("SSH public trust installation unavailable"))?;
        ensure!(!trust.is_empty(), "SSH public trust installation is empty");
        let known_hosts = store.cas.seal_object(&trust, "text/plain", "ssh-public-trust",
            "openssh-known-hosts", 1)?.object;
        let binding = SshBinding { target_id: self.target_id, target: SshTarget {
            host: self.host, port: self.port, user: self.account,
            key_path: Some(self.private_key_file.clone()), known_hosts_path: None,
            use_agent: false, ..SshTarget::default()
        }, receipt_root: self.receipt_root, known_hosts };
        ensure!(binding.receipt_root.starts_with('/') && !binding.receipt_root.contains('\0'),
            "SSH receipt root must be absolute");
        NativeSshInstallation::new(store, self.node_id, binding, self.revision, &self.public_key_openssh,
            Arc::new(ExplicitFileOpener { path: self.private_key_file, public_key: self.public_key_openssh.clone() }))
    }
}

// Config and public trust are not secret files, so no private-key permission
// policy is imposed. They still must not block startup on FIFO/device targets.
fn read_public_installation_file(path: &Path) -> Result<Vec<u8>> {
    ensure!(path.is_absolute(), "installation file must be explicit and absolute");
    let mut options = std::fs::OpenOptions::new(); options.read(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|_| anyhow::anyhow!("installation file unavailable"))?;
    let metadata = file.metadata().map_err(|_| anyhow::anyhow!("installation file unavailable"))?;
    ensure!(metadata.is_file() && metadata.len() <= MAX_KEY_BYTES, "installation file size/type invalid");
    let mut bytes = Vec::new();
    file.take(MAX_KEY_BYTES + 1).read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("installation file unreadable"))?;
    ensure!(bytes.len() as u64 <= MAX_KEY_BYTES, "installation file too large");
    Ok(bytes)
}

struct ExplicitFileOpener { path: PathBuf, public_key: String }

#[async_trait]
impl NativeSessionOpener for ExplicitFileOpener {
    async fn open(&self, store: &Store, binding: &SshBinding, budget: Duration)
        -> std::result::Result<NativeSshSession, NativeSshError>
    {
        if binding.target.key_path.as_ref() != Some(&self.path) || binding.target.use_agent {
            return Err(NativeSshError::InstallationMismatch);
        }
        let end = tokio::time::Instant::now().checked_add(budget)
            .filter(|_| !budget.is_zero()).ok_or(NativeSshError::InvalidLimits)?;
        tokio::time::timeout_at(end, async {
            let key = self.read_selected_key().await?;
            let target = NativeSshTarget::from_binding(store, binding)?;
            let io = tokio::net::TcpStream::connect((binding.target.host.as_str(), binding.target.port)).await
                .map_err(|_| NativeSshError::Connect)?;
            let connection = NativeSshConnection::connect_stream(target, io,
                end.saturating_duration_since(tokio::time::Instant::now())).await?;
            connection.authenticate_private_key(key).await
        }).await.map_err(|_| NativeSshError::Deadline)?
    }
}

impl ExplicitFileOpener {
    async fn read_selected_key(&self) -> std::result::Result<PrivateKeyWithHashAlg, NativeSshError> {
        let mut options = tokio::fs::OpenOptions::new(); options.read(true);
        // A replaced FIFO/device must not strand a blocking open after timeout.
        // Regular local file IO still has OS-level scheduling semantics; this
        // is not a claim that aborting Rust can cancel every filesystem driver.
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let mut file = options.open(&self.path).await.map_err(|_| NativeSshError::CredentialUnavailable)?;
        let metadata = file.metadata().await.map_err(|_| NativeSshError::CredentialUnavailable)?;
        if !metadata.is_file() || metadata.len() > MAX_KEY_BYTES {
            return Err(NativeSshError::CredentialUnavailable);
        }
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(NativeSshError::CredentialUnavailable);
            }
        }
        // Allocate before admitting secret bytes, as the current native exec
        // loader does. Growing a zeroized Vec can leave earlier allocations.
        let mut bytes = Zeroizing::new(vec![0_u8; (MAX_KEY_BYTES + 1) as usize].into_boxed_slice());
        let mut used = 0;
        while used < bytes.len() {
            match file.read(&mut bytes[used..]).await {
                Ok(0) => break,
                Ok(count) => used += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(NativeSshError::CredentialUnavailable),
            }
        }
        if used as u64 > MAX_KEY_BYTES { return Err(NativeSshError::CredentialUnavailable); }
        let text = std::str::from_utf8(&bytes[..used]).map_err(|_| NativeSshError::CredentialUnavailable)?;
        // No passphrase source is configured. Never consult ambient env/agent
        // or select another identity when this exact key cannot be unlocked.
        let key = russh::keys::decode_secret_key(text, None)
            .map_err(|_| NativeSshError::CredentialUnavailable)?;
        let expected = PublicKey::from_openssh(&self.public_key)
            .map_err(|_| NativeSshError::InstallationMismatch)?;
        if key.public_key().key_data() != expected.key_data() {
            return Err(NativeSshError::InstallationMismatch);
        }
        let hash = if key.algorithm().is_rsa() { Some(HashAlg::Sha512) } else { None };
        Ok(PrivateKeyWithHashAlg::new(Arc::new(key), hash))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    #[tokio::test]
    async fn explicit_file_signer_reads_only_owned_protected_fixture_identity() {
        use russh::keys::ssh_key::{LineEnding, private::Ed25519Keypair};
        let root = std::env::temp_dir().join(format!("shenko-own-file-signer-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let key: russh::keys::PrivateKey = Ed25519Keypair::from_seed(&[84; 32]).into();
        let path = root.join("owned-synthetic-key");
        use std::os::unix::fs::OpenOptionsExt as _;
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).unwrap();
        file.write_all(key.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap(); drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let opener = ExplicitFileOpener { path: path.clone(), public_key: key.public_key().to_openssh().unwrap() };
        let actual = opener.read_selected_key().await.unwrap();
        assert_eq!(actual.public_key().key_data(), key.public_key().key_data());
        // Same selected path made group-readable is not silently consumed.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(opener.read_selected_key().await, Err(NativeSshError::CredentialUnavailable)));
        drop(actual); drop(key); std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_public_installation_reader_rejects_fifo_without_waiting_for_writer() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = std::env::temp_dir().join(format!("shenko-own-installation-fifo-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("owned-fifo-not-a-config-or-trust-file");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // Own temporary fixture only; no subprocess/socket or external peer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_public_installation_file(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

}
