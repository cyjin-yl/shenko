//! The first owner claim and the authenticated actor — SPEC §44.4, §43.1.
//!
//! The installation ships with an installation-scoped, one-time claim
//! capability. It is provided through the install control channel (the data
//! directory on the machine that runs the installer); the first recognized
//! human who presents it is recorded as the owner. An HTTP visit is never
//! proof, and the principal is resolved from the capability itself — a
//! request body cannot name one.
//!
//! Ordinary chat stays open: claiming installs an owner, it does not close
//! the default conversation. Administrative actions afterwards require the
//! admin bearer issued by the claim.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, ensure};

use shenko_store::config::{ClaimOutcome, OwnerGrant};
use shenko_store::store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

/// The file that carries the claim capability for the installer.
pub const CLAIM_CAPABILITY_FILE: &str = "bootstrap/claim-capability.json";

/// The secret capability issued at install time. It is shown once to the
/// installer and never leaves the install control channel in the clear.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimCapability {
    pub schema_version: u32,
    pub installation_id: EntityId,
    pub capability: String,
}

/// Issue a new claim capability. Called by the installer/bootstrap before the
/// first launch; the returned secret must be given to the installer and is
/// not stored anywhere else by this module.
pub fn issue_claim_capability(installation_id: EntityId) -> (ClaimCapability, String) {
    let capability = format!("sc_{}", hex_encode(&EntityId::new().as_uuid().to_bytes()));
    (
        ClaimCapability {
            schema_version: 1,
            installation_id,
            capability: Digest::blake3(capability.as_bytes()).0,
        },
        capability,
    )
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Store the claim capability so the install control channel can present it.
/// The file is written 0600 and is consumed by the claim.
pub fn persist_claim_capability(data_dir: &std::path::Path, file: &ClaimCapability) -> Result<()> {
    let path = data_dir.join(CLAIM_CAPABILITY_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(file)?;
    write_private(&path, &bytes)?;
    Ok(())
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Load the persisted claim capability, if it exists.
pub fn load_claim_capability(data_dir: &std::path::Path) -> Option<ClaimCapability> {
    let path = data_dir.join(CLAIM_CAPABILITY_FILE);
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The authenticated actor resolved from a request. Extracted by
/// [`OwnerAuthLayer`] from the admin bearer or a valid claim capability.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthenticatedActor {
    /// The recognized owner acting with the admin bearer.
    Owner { principal_ref: shenko_types::object::ObjectRef },
    /// An ordinary session participant. Ordinary conversation is always
    /// allowed; administrative configuration is not.
    Participant,
}

/// Resolve an authenticated actor from the request headers.
///
/// * `x-shenko-admin: <bearer>` — verified against the installation's stored
///   admin token digest; when it matches, the actor is the owner.
/// * Otherwise the actor is an ordinary participant. Nothing else in a
///   request is trusted: no body field may name a principal.
pub fn actor_from_headers(
    store: &Store,
    installation_id: EntityId,
    headers: &axum::http::HeaderMap,
) -> Result<AuthenticatedActor> {
    if let Some(value) = headers.get("x-shenko-admin").and_then(|v| v.to_str().ok()) {
        if store.verify_admin_token(installation_id, value)? {
            let principal_ref = principal_ref_for(store, installation_id)?;
            return Ok(AuthenticatedActor::Owner { principal_ref });
        }
    }
    Ok(AuthenticatedActor::Participant)
}

/// The principal reference of the installation's owner, derived from the
/// durable grant — never from the request.
fn principal_ref_for(store: &Store, installation_id: EntityId) -> Result<shenko_types::object::ObjectRef> {
    let grant = store
        .owner_grant(installation_id)?
        .context("owner principal requested before any claim")?;
    Ok(grant.human_principal_ref)
}

/// Result of a claim attempt — SPEC §44.4 error contract.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimResponse {
    pub outcome: String,
    pub grant_id: Option<EntityId>,
    /// The admin bearer for this owner. Present exactly once, only when the
    /// claim created the grant.
    pub admin_bearer: Option<String>,
    pub error: Option<String>,
}

/// Verify the presented capability against the install control channel's
/// secret and, if valid, claim the installation.
pub fn claim_installation(
    store: &Store,
    data_dir: &std::path::Path,
    presented: &str,
) -> Result<ClaimResponse> {
    let capability = match load_claim_capability(data_dir) {
        Some(capability) => capability,
        None => {
            return Ok(ClaimResponse {
                outcome: "claim_invalid".into(),
                grant_id: None,
                admin_bearer: None,
                error: Some("CLAIM_INVALID: this installation has no pending claim capability".into()),
            });
        }
    };
    if Digest::blake3(presented.as_bytes()).0 != capability.capability {
        return Ok(ClaimResponse {
            outcome: "claim_invalid".into(),
            grant_id: None,
            admin_bearer: None,
            error: Some("CLAIM_INVALID: the presented capability is not this installation's claim".into()),
        });
    }
    let installation_id = capability.installation_id;
    let principal_ref = store.seal_config_object(
        &serde_json::json!({
            "kind": "human_owner",
            "installation_id": installation_id,
            "recognized_by": "claim_capability",
        }),
        "owner-principal",
    )?;
    let authority_ref = store.seal_config_object(
        &serde_json::json!({
            "kind": "installation_admin",
            "scope": ["admin"],
        }),
        "owner-authority",
    )?;
    // The admin bearer is the owner's secret; its digest is what persists.
    let bearer = format!("sat_{}", hex_encode(&EntityId::new().as_uuid().to_bytes()));
    let token_hash = Digest::blake3(bearer.as_bytes());
    let (outcome, grant) = store.claim_owner(
        installation_id,
        EntityId::new(),
        principal_ref,
        authority_ref,
        token_hash,
    )?;
    match outcome {
        ClaimOutcome::Granted => {
            // The one-time capability is consumed by a successful claim.
            let path = data_dir.join(CLAIM_CAPABILITY_FILE);
            let _ = std::fs::remove_file(&path);
            Ok(ClaimResponse {
                outcome: "granted".into(),
                grant_id: Some(grant.expect("granted claim returns a grant").grant_id),
                admin_bearer: Some(bearer),
                error: None,
            })
        }
        ClaimOutcome::Replayed => Ok(ClaimResponse {
            outcome: "replayed".into(),
            grant_id: Some(grant.expect("replayed claim returns the original grant").grant_id),
            admin_bearer: None,
            error: None,
        }),
        ClaimOutcome::AlreadyClaimed => {
            let existing: OwnerGrant = grant.expect("already-claimed returns the existing grant");
            Ok(ClaimResponse {
                outcome: "owner_already_claimed".into(),
                grant_id: Some(existing.grant_id),
                admin_bearer: None,
                error: Some("OWNER_ALREADY_CLAIMED: this installation already has an owner".into()),
            })
        }
    }
}

/// Axum state carrying everything the ownership surfaces need.
#[derive(Clone)]
pub struct OwnerState {
    pub store: Arc<Store>,
    pub installation_id: EntityId,
    pub data_dir: PathBuf,
}

impl OwnerState {
    /// The authenticated actor for this request, or `Participant` when the
    /// request carries no valid credential.
    pub fn actor(&self, headers: &axum::http::HeaderMap) -> Result<AuthenticatedActor> {
        actor_from_headers(&self.store, self.installation_id, headers)
    }

    /// Refuse unless the actor is the recognized owner.
    pub fn require_owner(&self, headers: &axum::http::HeaderMap) -> Result<shenko_types::object::ObjectRef> {
        match self.actor(headers)? {
            AuthenticatedActor::Owner { principal_ref } => Ok(principal_ref),
            AuthenticatedActor::Participant => anyhow::bail!("CONFIG_DENIED: owner authentication required"),
        }
    }
}

/// Ensure a directory is present. Used by the bootstrap path before the
/// capability file is written.
pub fn ensure_data_dir(data_dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    Ok(())
}

/// Check the state for an internal invariant (no silent credential leaks).
pub fn validate_capability_file(data_dir: &std::path::Path) -> Result<()> {
    let capability = load_claim_capability(data_dir)
        .context("no claim capability present")?;
    ensure!(
        capability.capability.starts_with("blake3:"),
        "claim capability must store only a digest"
    );
    Ok(())
}
