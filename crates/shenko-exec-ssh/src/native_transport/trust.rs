//! Convert the exact registered public-trust CAS bytes into in-memory pins.
//! This initial contract deliberately rejects unsupported known_hosts syntax;
//! it never learns a host, discovers HOME or reduces a certificate to its key.
use super::NativeSshError;
use crate::SshBinding;
use russh::keys::{PublicKey, PublicKeyOrCertificate};
use shenko_store::Store;
use shenko_types::object::Digest;

pub struct NativeSshTarget {
    pub(super) account: String,
    pub(super) pins: Vec<PublicKey>,
    pub(super) revoked: Vec<PublicKey>,
    public_trust_digest: Digest,
    pub(super) binding_digest: Digest,
}

impl NativeSshTarget {
    /// Trusted native caller only: a binding and host pin are NOT a grant.
    /// No key-path, password, agent socket or mutable known_hosts file is read.
    pub fn from_binding(store: &Store, binding: &SshBinding) -> Result<Self, NativeSshError> {
        let t = &binding.target;
        let trust = &binding.known_hosts;
        if t.password.is_some() || t.known_hosts_path.is_some()
            || (t.key_path.is_none() && !t.use_agent)
            || t.key_path.as_ref().is_some_and(|path| !path.is_absolute())
            || t.host.is_empty() || t.host.starts_with('-') || t.port == 0
            || !t.host.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_:[]%".contains(&b))
            || t.user.is_empty() || t.user.starts_with('-')
            || !t.user.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || trust.bytes == 0 || trust.bytes > 1024 * 1024
            || trust.store_namespace != "ssh-public-trust"
            || trust.codec.name != "openssh-known-hosts" || trust.codec.major != 1
            || !trust.digest.0.strip_prefix("blake3:").is_some_and(|s|
                s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        { return Err(NativeSshError::InvalidBinding); }
        // The existing Store read allocates the CAS file. Its allocation policy
        // is not changed here. Verify the exact bytes used, not a path that a
        // later SSH subprocess reopens; reject wrong length/hash before parsing.
        let bytes = store.cas.read_object(&trust.digest)
            .map_err(|_| NativeSshError::TrustUnavailable)?;
        if bytes.len() as u64 != trust.bytes || Digest::blake3(&bytes) != trust.digest {
            return Err(NativeSshError::TrustUnavailable);
        }
        let binding_digest = Self::binding_digest(binding)?;
        Self::parse(&t.host, t.port, t.user.clone(), trust.digest.clone(), binding_digest, &bytes)
    }

    pub fn public_trust_digest(&self) -> &Digest { &self.public_trust_digest }

    // Exact persisted installation facts, not an arbitrary caller tag or grant.
    // Includes endpoint/account, selected credential handle, receipt root and
    // the full sealed trust reference. No private key bytes are read or emitted.
    pub(super) fn binding_digest(binding: &SshBinding) -> Result<Digest, NativeSshError> {
        serde_json::to_vec(&("ssh.native-session-binding.v1", binding))
            .map(|bytes| Digest::blake3(&bytes))
            .map_err(|_| NativeSshError::InvalidBinding)
    }

    fn parse(host: &str, port: u16, account: String, digest: Digest, binding_digest: Digest, bytes: &[u8])
        -> Result<Self, NativeSshError>
    {
        let text = std::str::from_utf8(bytes).map_err(|_| NativeSshError::TrustUnsupported)?;
        let target = if port == 22 { host.to_owned() } else { format!("[{host}]:{port}") };
        let mut this = Self { account, pins: vec![], revoked: vec![], public_trust_digest: digest, binding_digest };
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let mut fields = line.split_whitespace();
            let mut hosts = fields.next().ok_or(NativeSshError::TrustUnsupported)?;
            let revoked = hosts == "@revoked";
            if revoked { hosts = fields.next().ok_or(NativeSshError::TrustUnsupported)?; }
            // Explicit fail-closed subset, not a claim of full OpenSSH parsing.
            // Includes unrelated unsupported lines: never miss a revocation/CA
            // rule because a loose parser silently skipped its host expression.
            if hosts.starts_with('@') || hosts.contains(['*', '?', '!', '|']) {
                return Err(NativeSshError::TrustUnsupported);
            }
            let algorithm = fields.next().ok_or(NativeSshError::TrustUnsupported)?;
            let encoded = fields.next().ok_or(NativeSshError::TrustUnsupported)?;
            let key = PublicKey::from_openssh(&format!("{algorithm} {encoded}"))
                .map_err(|_| NativeSshError::TrustUnsupported)?;
            if hosts.split(',').any(|h| h == target) {
                if revoked { this.revoked.push(key); } else { this.pins.push(key); }
            }
        }
        if this.pins.is_empty() { return Err(NativeSshError::TrustUnavailable); }
        Ok(this)
    }

    pub(super) fn accepts(&self, peer: &PublicKeyOrCertificate) -> bool {
        match peer {
            PublicKeyOrCertificate::PublicKey { key, .. } =>
                self.pins.iter().any(|pin| pin.key_data() == key.key_data())
                    && !self.revoked.iter().any(|pin| pin.key_data() == key.key_data()),
            PublicKeyOrCertificate::Certificate(_) => false,
        }
    }
}
