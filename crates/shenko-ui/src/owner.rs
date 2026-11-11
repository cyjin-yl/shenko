//! The first owner claim and the authenticated actor — docs/specs/plugins.md#spec-44-4, docs/specs/configuration-playground.md#spec-43-1.
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

#[path = "owner_control.rs"]
mod control;
use control::ClaimControl;

/// Local-only verifier, retained after consumption to recognize retries.
pub const CLAIM_CAPABILITY_FILE: &str = "bootstrap/claim-capability.json";
/// Plaintext delivery, accessible only through the installation control channel.
pub const CLAIM_SECRET_FILE: &str = "bootstrap/claim-secret.json";

/// The persisted verifier contains a digest, never the plaintext capability.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimCapability {
    pub schema_version: u32,
    pub installation_id: EntityId,
    pub capability: String,
}

// Deliberately no Debug implementation: this is credential material, not a
// runtime fact, model input, CAS object, or replicated installation record.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimDelivery {
    schema_version: u32,
    installation_id: EntityId,
    capability: String,
}

impl ClaimDelivery {
    fn verifier(&self) -> ClaimCapability {
        ClaimCapability {
            schema_version: 1,
            installation_id: self.installation_id,
            capability: Digest::blake3(self.capability.as_bytes()).0,
        }
    }
}

fn load_verifier(control: &ClaimControl, installation_id: EntityId) -> Result<Option<ClaimCapability>> {
    let Some(bytes) = control.read(c"claim-capability.json")? else { return Ok(None) };
    let record: ClaimCapability = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid owner claim verifier JSON"))?;
    ensure!(record.schema_version == 1 && record.installation_id == installation_id,
        "owner claim verifier schema or installation binding mismatch");
    ensure!(record.capability.strip_prefix("blake3:")
        .is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "owner claim verifier must contain a BLAKE3 digest");
    Ok(Some(record))
}

fn load_delivery(control: &ClaimControl, installation_id: EntityId) -> Result<Option<ClaimDelivery>> {
    let Some(bytes) = control.read(c"claim-secret.json")? else { return Ok(None) };
    let record: ClaimDelivery = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid owner claim delivery JSON"))?;
    ensure!(record.schema_version == 1 && record.installation_id == installation_id,
        "owner claim delivery schema or installation binding mismatch");
    ensure!(record.capability.strip_prefix("sc_")
        .is_some_and(|secret| secret.len() == 32 && secret.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        "invalid owner claim delivery capability");
    Ok(Some(record))
}

/// Prepare local delivery before exposing the runtime. Interrupted publication
/// resumes from the already-published secret; it never rotates a pending claim.
/// An older digest-only installation retains its previously delivered secret.
pub fn prepare_owner_claim(
    store: &Store,
    data_dir: &std::path::Path,
    installation_id: EntityId,
) -> Result<Option<PathBuf>> {
    let claimed = store.owner_grant(installation_id)?.is_some();
    let Some(control) = ClaimControl::open(data_dir, !claimed)? else { return Ok(None) };
    let verifier = load_verifier(&control, installation_id)?;
    let delivery = load_delivery(&control, installation_id)?;
    if let (Some(verifier), Some(delivery)) = (&verifier, &delivery) {
        ensure!(verifier.capability == delivery.verifier().capability,
            "owner claim delivery does not match the retained verifier");
    }
    if claimed {
        control.remove(c"claim-secret.json")?;
        return Ok(None);
    }
    if verifier.is_some() {
        return Ok(delivery.map(|_| data_dir.join(CLAIM_SECRET_FILE)));
    }
    let delivery = match delivery {
        Some(delivery) => delivery,
        None => {
            let delivery = ClaimDelivery {
                schema_version: 1,
                installation_id,
                capability: format!("sc_{}", EntityId::new().as_uuid().simple()),
            };
            control.write_new(c"claim-secret.json", &serde_json::to_vec(&delivery)?)?;
            delivery
        }
    };
    control.write_new(c"claim-capability.json", &serde_json::to_vec(&delivery.verifier())?)?;
    Ok(Some(data_dir.join(CLAIM_SECRET_FILE)))
}

/// Missing is distinct from corrupt, unsafe, or foreign-installation state.
pub fn load_claim_capability(
    data_dir: &std::path::Path,
    installation_id: EntityId,
) -> Result<Option<ClaimCapability>> {
    let Some(control) = ClaimControl::open(data_dir, false)? else { return Ok(None) };
    load_verifier(&control, installation_id)
}

fn consume_delivery(control: &ClaimControl, verifier: &ClaimCapability) -> Result<()> {
    if let Some(delivery) = load_delivery(control, verifier.installation_id)? {
        ensure!(delivery.verifier().capability == verifier.capability,
            "owner claim delivery does not match the retained verifier");
        control.remove(c"claim-secret.json")?;
    }
    Ok(())
}

/// The authenticated actor resolved from a request. Extracted by
/// [`OwnerState::actor`] from the admin bearer.
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

/// Result of a claim attempt — docs/specs/plugins.md#spec-44-4 error contract.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    installation_id: EntityId,
    presented: &str,
) -> Result<ClaimResponse> {
    let control = ClaimControl::open(data_dir, false)?;
    let capability = match &control {
        Some(control) => load_verifier(control, installation_id)?,
        None => None,
    };
    let capability = match capability {
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
    let control = control.context("owner control directory missing after verifier admission")?;
    // Retire plaintext before the durable grant. A cleanup failure cannot
    // commit ownership and then lose the only response carrying its bearer.
    consume_delivery(&control, &capability)?;
    let claim_id = EntityId::derived("shenko.owner.claim.v1", capability.capability.as_bytes());
    if let Some(grant) = store.owner_grant(installation_id)? {
        return Ok(ClaimResponse {
            outcome: if grant.claim_id == claim_id { "replayed" } else { "owner_already_claimed" }.into(),
            grant_id: Some(grant.grant_id),
            admin_bearer: None,
            error: None,
        });
    }
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
    let bearer = format!("sat_{}", EntityId::new().as_uuid().simple());
    let token_hash = Digest::blake3(bearer.as_bytes());
    let (outcome, grant) = store.claim_owner(
        installation_id,
        claim_id,
        principal_ref,
        authority_ref,
        token_hash,
    )?;
    match outcome {
        ClaimOutcome::Granted => {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::{DirBuilderExt, PermissionsExt}, path::Path};

    fn private_fixture(run: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        let root = std::env::temp_dir().join(format!("shenko-owner-test-{}", EntityId::new()));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        let result = run(&root);
        let cleanup = fs::remove_dir_all(&root);
        result?;
        cleanup?;
        Ok(())
    }

    #[test]
    fn consumed_claim_reopens_with_the_original_grant_and_bearer() -> Result<()> {
        private_fixture(|root| {
            let store = Store::open(root.join("state.db"), root.join("cas"))?;
            let installation_id = store.ledger.ledger_id();
            let path = prepare_owner_claim(&store, root, installation_id)?
                .context("new installation did not deliver a claim")?;
            let delivery: ClaimDelivery = serde_json::from_slice(&fs::read(&path)?)?;
            let granted = claim_installation(&store, root, installation_id, &delivery.capability)?;
            ensure!(granted.outcome == "granted" && !path.exists(), "claim did not consume delivery");
            let bearer = granted.admin_bearer.context("grant did not deliver an admin bearer")?;
            drop(store);

            let reopened = Store::open(root.join("state.db"), root.join("cas"))?;
            ensure!(prepare_owner_claim(&reopened, root, installation_id)?.is_none(), "claimed installation reissued a capability");
            let replay = claim_installation(&reopened, root, installation_id, &delivery.capability)?;
            ensure!(replay.outcome == "replayed" && replay.grant_id == granted.grant_id
                && replay.admin_bearer.is_none(), "replay lost the original grant or issued another bearer");
            ensure!(reopened.verify_admin_token(installation_id, &bearer)?, "original admin bearer stopped authenticating");
            Ok(())
        })
    }

    #[test]
    fn malformed_pending_verifier_is_not_replaced_on_startup() -> Result<()> {
        private_fixture(|root| {
            let store = Store::open(root.join("state.db"), root.join("cas"))?;
            let installation_id = store.ledger.ledger_id();
            fs::DirBuilder::new().mode(0o700).create(root.join("bootstrap"))?;
            let path = root.join(CLAIM_CAPABILITY_FILE);
            fs::write(&path, b"{not-json}")?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            ensure!(prepare_owner_claim(&store, root, installation_id).is_err(), "corruption was treated as absence");
            ensure!(fs::read(&path)? == b"{not-json}" && !root.join(CLAIM_SECRET_FILE).exists(),
                "startup replaced a corrupt pending claim");
            Ok(())
        })
    }
}

