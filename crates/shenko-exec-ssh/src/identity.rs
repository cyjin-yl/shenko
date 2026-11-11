//! Agent-owned SSH identity generation — docs/specs/remote-seats.md#spec-11/R016.
//!
//! The agent creates and maintains its own ed25519 identity per node. This
//! module writes a private key (mode 0600) and its OpenSSH `.pub`, and is
//! idempotent: an already-installed, self-consistent identity is returned
//! unchanged rather than regenerated. Private key bytes are never serialized
//! into configuration — only the key location and the public identity
//! (OpenSSH line + SHA-256 fingerprint) are exposed, matching the existing
//! `NativeSshFileTargetConfig` invariant.

use anyhow::{Context, Result, bail, ensure};
use russh::keys::{HashAlg, PublicKey, key::safe_rng, ssh_key::{
    Algorithm, LineEnding, PrivateKey,
}};
use serde::Serialize;
use shenko_types::ids::EntityId;
use std::path::{Path, PathBuf};

/// Public-facing identity for R017 display/copy. Excludes the private-key
/// bytes; `private_key_file` is a location, not material.
#[derive(Debug, Clone, Serialize)]
pub struct NodeSshIdentity {
    pub schema: &'static str,
    /// Path of the written (or already-present) private key.
    pub private_key_file: PathBuf,
    /// Canonical OpenSSH public-key line (comment stripped).
    pub public_key_openssh: String,
    /// `SHA256:…` fingerprint suitable for display/copy.
    pub fingerprint_sha256: String,
    /// `ssh-ed25519`.
    pub algorithm: String,
    /// True when this call generated a new keypair; false when it reopened an
    /// existing self-consistent one.
    pub generated: bool,
}

const IDENTITY_FILE_NAME: &str = "id_ed25519";

/// One `shenko.ssh.file-installation.v1` target entry authored from a
/// provisioned node identity. Field names must match
/// `NativeSshFileTargetConfig` exactly (it is `Deserialize`-only). This holds
/// the key LOCATION and the expected PUBLIC identity — never private bytes —
/// so it can be written into a runtime-supplied installation file.
#[derive(Debug, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvisionedSshTarget {
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

/// Provision the node's identity in `dir`, then return a file-installation
/// target entry that points at it. The private key stays on disk at 0600; only
/// its path and public identity are embedded in the config.
///
/// Idempotent alongside `ensure_node_identity`: re-running against an intact
/// identity yields a config for the same public key.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn provision_node_identity(
    dir: &Path,
    comment: &str,
    node_id: EntityId,
    target_id: &str,
    host: &str,
    port: u16,
    account: &str,
    known_hosts_file: PathBuf,
    receipt_root: &str,
    revision: &str,
) -> Result<ProvisionedSshTarget> {
    let identity = ensure_node_identity(dir, comment)?;
    Ok(ProvisionedSshTarget {
        node_id,
        target_id: target_id.to_string(),
        revision: revision.to_string(),
        host: host.to_string(),
        port,
        account: account.to_string(),
        private_key_file: identity.private_key_file,
        public_key_openssh: identity.public_key_openssh,
        known_hosts_file,
        receipt_root: receipt_root.to_string(),
    })
}

/// Return the node's SSH identity, generating a fresh ed25519 keypair only
/// when `dir` does not already hold a self-consistent identity.
///
/// Idempotent: repeated calls with an intact identity return the same public
/// identity with `generated: false`. A present-but-corrupt or mismatched
/// private key is a hard error (never silently overwritten), so a caller does
/// not lose an identity it may still depend on.
///
/// `dir` is created with mode 0700 if missing. The key comment is the node
/// label; it is stripped from the public identity.
#[cfg(unix)]
pub fn ensure_node_identity(dir: &Path, comment: &str) -> Result<NodeSshIdentity> {
    use std::os::unix::fs::DirBuilderExt as _;
    ensure!(!dir.as_os_str().is_empty(), "identity directory must be explicit");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
        .with_context(|| format!("identity dir unavailable: {}", dir.display()))?;

    let priv_path = dir.join(IDENTITY_FILE_NAME);
    let pub_path = priv_path.with_extension("pub");

    // Reopen an intact identity without regenerating.
    if priv_path.exists() && pub_path.exists() {
        if let Some(identity) = read_consistent_identity(&priv_path, &pub_path)? {
            return Ok(identity);
        }
        bail!("existing SSH identity at {} is inconsistent; refusing to overwrite", priv_path.display());
    }
    if priv_path.exists() != pub_path.exists() {
        bail!("partial SSH identity at {}; remove the orphan file to re-key", priv_path.display());
    }

    // Generate fresh ed25519 keypair with the crate's CSPRNG.
    let mut rng = safe_rng();
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|e| anyhow::anyhow!("ssh keygen failed: {e}"))?;
    key.set_comment(comment.to_string());
    let private_openssh = key.to_openssh(LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("ssh key encode failed: {e}"))?;
    let mut public = key.public_key().clone();
    public.set_comment("");
    let public_openssh = public.to_openssh()
        .map_err(|e| anyhow::anyhow!("ssh pubkey encode failed: {e}"))?;

    // Write private key mode 0600, create-new so a concurrent re-key or a
    // planted file is detected rather than overwritten.
    write_new_file(&priv_path, private_openssh.as_bytes(), 0o600)
        .with_context(|| format!("private key write failed: {}", priv_path.display()))?;
    write_new_file(&pub_path, format!("{public_openssh}\n").as_bytes(), 0o644)
        .with_context(|| format!("public key write failed: {}", pub_path.display()))?;

    // Re-read to report the canonical stored identity, not the in-memory key.
    read_consistent_identity(&priv_path, &pub_path)?
        .map(|mut id| { id.generated = true; id })
        .ok_or_else(|| anyhow::anyhow!("freshly written identity did not verify"))
}

#[cfg(unix)]
fn write_new_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true).create_new(true).mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path).with_context(|| format!("create-new failed: {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Parse both files and confirm they encode the *same* key. Returns `Ok(None)`
/// when either file is unreadable/mismatched so the caller can decide whether
/// that means "inconsistent" (error) rather than "absent" (regenerate).
#[cfg(unix)]
fn read_consistent_identity(priv_path: &Path, pub_path: &Path) -> Result<Option<NodeSshIdentity>> {
    let priv_bytes = match std::fs::read(priv_path) { Ok(b) => b, Err(_) => return Ok(None) };
    let pub_bytes = match std::fs::read(pub_path) { Ok(b) => b, Err(_) => return Ok(None) };
    let private = PrivateKey::from_openssh(&priv_bytes).ok();
    let mut public = PublicKey::from_openssh(
        std::str::from_utf8(&pub_bytes).unwrap_or("").trim_end()).ok();
    let (private, public) = match (private, public.as_mut()) {
        (Some(p), Some(q)) => (p, q),
        _ => return Ok(None),
    };
    if private.public_key().key_data() != public.key_data() {
        return Ok(None);
    }
    public.set_comment("");
    let public_openssh = public.to_openssh().ok();
    Ok(public_openssh.map(|public_key_openssh| NodeSshIdentity {
        schema: "shenko.ssh.node-identity.v1",
        private_key_file: priv_path.to_path_buf(),
        public_key_openssh,
        fingerprint_sha256: public.fingerprint(HashAlg::Sha256).to_string(),
        algorithm: public.algorithm().to_string(),
        generated: false,
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("shenko-identity-{tag}-{}", shenko_types::ids::EntityId::new()));
        dir
    }

    #[test]
    fn generates_valid_ed25519_identity_and_reopens_idempotently() {
        let dir = temp_dir("gen");
        let first = ensure_node_identity(&dir, "node-a").unwrap();
        assert!(first.generated);
        assert_eq!(first.algorithm, "ssh-ed25519");
        assert!(first.public_key_openssh.starts_with("ssh-ed25519 "));
        assert!(first.fingerprint_sha256.starts_with("SHA256:"));

        let priv_path = dir.join("id_ed25519");
        let meta = std::fs::metadata(&priv_path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // Written private key parses back to the same public key.
        let key = PrivateKey::from_openssh(std::fs::read(&priv_path).unwrap()).unwrap();
        assert_eq!(key.public_key().to_openssh().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" "),
            first.public_key_openssh);

        // Idempotent: same identity, not regenerated.
        let second = ensure_node_identity(&dir, "node-a").unwrap();
        assert!(!second.generated);
        assert_eq!(second.public_key_openssh, first.public_key_openssh);
        assert_eq!(second.fingerprint_sha256, first.fingerprint_sha256);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_mismatched_pubkey_without_overwriting() {
        let dir = temp_dir("mismatch");
        ensure_node_identity(&dir, "node-a").unwrap();
        // Plant a different pubkey next to the private key.
        let other = PrivateKey::random(&mut safe_rng(), Algorithm::Ed25519).unwrap();
        std::fs::write(dir.join("id_ed25519.pub"),
            format!("{}\n", other.public_key().to_openssh().unwrap())).unwrap();
        assert!(ensure_node_identity(&dir, "node-a").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_orphaned_partial_identity() {
        let dir = temp_dir("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("id_ed25519"), b"not-a-key").unwrap();
        assert!(ensure_node_identity(&dir, "node-a").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn provisioned_entry_roundtrips_through_file_installation_install() {
        // The authored config must deserialize via the real consumer path and
        // resolve to an installed target whose expected public identity is the
        // generated key.
        let id_dir = temp_dir("prov-id");
        let kh_dir = temp_dir("prov-kh");
        std::fs::create_dir_all(&kh_dir).unwrap();
        let kh = kh_dir.join("known_hosts");
        std::fs::write(&kh, b"fixture.invalid ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n").unwrap();

        let node_id: EntityId = "4d275d62-f194-4d28-9338-ceb58b90a2b0".parse().unwrap();
        let entry = provision_node_identity(
            &id_dir, "node-a", node_id, "t1", "127.0.0.1", 22, "u",
            kh.clone(), "/receipts", "rev-1",
        ).unwrap();

        // Serialize into the exact envelope the reader expects.
        let cfg_dir = temp_dir("prov-cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg = cfg_dir.join("inst.json");
        let envelope = serde_json::json!({
            "schema": "shenko.ssh.file-installation.v1",
            "targets": [entry],
        });
        std::fs::write(&cfg, serde_json::to_vec(&envelope).unwrap()).unwrap();

        let targets = crate::NativeSshFileTargetConfig::read_installation_file(&cfg).unwrap();
        assert_eq!(targets.len(), 1);
        let t = &targets[0];
        assert_eq!(t.target_id, "t1");
        assert_eq!(t.node_id, node_id);
        assert!(t.public_key_openssh.starts_with("ssh-ed25519 "));
        assert_eq!(t.private_key_file, id_dir.join("id_ed25519"));

        // The private key file the config points at is a real 0600 OpenSSH key
        // matching the configured public identity.
        let meta = std::fs::metadata(&t.private_key_file).unwrap();
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777, 0o600);
        let key = PrivateKey::from_openssh(std::fs::read(&t.private_key_file).unwrap()).unwrap();
        assert_eq!(key.public_key().to_openssh().unwrap().split(' ').take(2).collect::<Vec<_>>().join(" "),
            t.public_key_openssh);

        let _ = std::fs::remove_dir_all(&id_dir);
        let _ = std::fs::remove_dir_all(&kh_dir);
        let _ = std::fs::remove_dir_all(&cfg_dir);
    }
}
