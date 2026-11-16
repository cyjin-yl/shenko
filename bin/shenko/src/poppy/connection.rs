//! Public connection facts use the existing Node installation mechanism.
//! Pairwise/session identifiers are continuity data, not bearer credentials.
use std::{collections::BTreeMap, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_compat::installation::{UnitRef, content_revision, plugin_dependency, plugin_package};
use shenko_compat_poppy::{
    UNIT_ID,
    discovery::DiscoveryReport,
    session::{OperatorSigner, SessionIdentity},
    ucp::PlatformProfile,
};
use shenko_node::{Node, UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitKind, UnitPackage};
use shenko_types::object::Digest;

use super::vault::Vault;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperatorConfig {
    pub schema_version: u32,
    pub client_name: String,
    pub client_id: String,
    pub jwks_uri: String,
    pub profile_uri: String,
    pub redirect_uris: Vec<String>,
    /// Explicitly provisioned opaque-instrument providers, never copied from a
    /// merchant advertisement. Empty means browser continuation only.
    #[serde(default)]
    pub payment_handlers: BTreeMap<String, Vec<Value>>,
}

impl OperatorConfig {
    fn profile(&self, signer: &OperatorSigner) -> Result<PlatformProfile> {
        ensure!(
            self.payment_handlers.len() <= 16,
            "too many installed payment handlers"
        );
        let mut profile = PlatformProfile::new(&self.profile_uri, &signer.public_jwks())?;
        for (name, declarations) in &self.payment_handlers {
            ensure!(
                !declarations.is_empty() && declarations.len() <= 8,
                "payment handler declarations must contain 1..=8 versions"
            );
            for declaration in declarations {
                profile = profile.with_payment_handler(name, declaration.clone())?;
            }
        }
        Ok(profile)
    }
}

pub(super) struct Operator {
    pub config: OperatorConfig,
    pub signer: Arc<OperatorSigner>,
    pub profile: PlatformProfile,
}

impl Operator {
    pub fn load(vault: &Vault) -> Result<Self> {
        let bytes = vault
            .read("operator-config")?
            .context("Poppy identity has not been initialized")?;
        let config: OperatorConfig = serde_json::from_slice(&bytes)?;
        ensure!(
            config.schema_version == 1,
            "unsupported Poppy operator configuration"
        );
        let key = vault
            .read("operator-key")?
            .context("Poppy operator private key is missing")?;
        let signer = Arc::new(OperatorSigner::from_pkcs8_thumbprint(
            &config.client_id,
            &key,
        )?);
        shenko_compat_poppy::auth::client_metadata_with_redirects(
            &signer,
            &config.client_name,
            &config.jwks_uri,
            &config.redirect_uris,
        )?;
        let profile = config.profile(&signer)?;
        Ok(Self {
            config,
            signer,
            profile,
        })
    }

    pub fn initialize(data_dir: &Path, config: OperatorConfig) -> Result<Self> {
        // Validate the complete public identity before creating its private key.
        ensure!(
            config.schema_version == 1 && config.redirect_uris.len() <= 16,
            "invalid Poppy operator configuration"
        );
        let vault = Vault::open(data_dir, true)?;
        if vault.read("operator-config")?.is_some() {
            let existing = Self::load(&vault)?;
            ensure!(
                serde_json::to_value(&existing.config)? == serde_json::to_value(&config)?,
                "operator identity already exists with different public URLs; keep its keys and migrate explicitly"
            );
            return Ok(existing);
        }
        let key = match vault.read("operator-key")? {
            Some(key) => key,
            None => {
                let key = EcdsaKeyPair::generate_pkcs8(
                    &ECDSA_P256_SHA256_FIXED_SIGNING,
                    &SystemRandom::new(),
                )
                .map_err(|_| anyhow::anyhow!("Poppy operator key generation failed"))?;
                let candidate =
                    OperatorSigner::from_pkcs8_thumbprint(&config.client_id, key.as_ref())?;
                shenko_compat_poppy::auth::client_metadata_with_redirects(
                    &candidate,
                    &config.client_name,
                    &config.jwks_uri,
                    &config.redirect_uris,
                )?;
                config.profile(&candidate)?;
                vault.write("operator-key", key.as_ref())?;
                zeroize::Zeroizing::new(key.as_ref().to_vec())
            }
        };
        let signer = OperatorSigner::from_pkcs8_thumbprint(&config.client_id, &key)?;
        shenko_compat_poppy::auth::client_metadata_with_redirects(
            &signer,
            &config.client_name,
            &config.jwks_uri,
            &config.redirect_uris,
        )?;
        config.profile(&signer)?;
        vault.write("operator-config", &serde_json::to_vec(&config)?)?;
        Self::load(&vault)
    }

    pub fn configure_payments(
        vault: &Vault,
        handlers: BTreeMap<String, Vec<Value>>,
    ) -> Result<Self> {
        let mut operator = Self::load(vault)?;
        operator.config.payment_handlers = handlers;
        operator.profile = operator.config.profile(&operator.signer)?;
        vault.write("operator-config", &serde_json::to_vec(&operator.config)?)?;
        Ok(operator)
    }

    pub fn public_documents(&self) -> Result<Value> {
        let mut client = shenko_compat_poppy::auth::client_metadata_with_redirects(
            &self.signer,
            &self.config.client_name,
            &self.config.jwks_uri,
            &self.config.redirect_uris,
        )?;
        client["redirect_uris"] = json!(self.config.redirect_uris);
        client["grant_types"] = json!([
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ]);
        client["response_types"] = json!(["code"]);
        Ok(json!({
            "client_metadata": {"url": self.config.client_id, "document": client},
            "jwks": {"url": self.config.jwks_uri, "document": self.signer.public_jwks()},
            "ucp_profile": {"url": self.config.profile_uri, "document": self.profile.document()},
            "public_urls_verified": false,
            "private_key_exported": false
        }))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Connection {
    pub schema_version: u32,
    pub report: DiscoveryReport,
    pub user_id: String,
    pub session_id: String,
    pub agent_id: Option<String>,
}

impl Connection {
    pub fn new(report: DiscoveryReport, node: &Node) -> Result<Self> {
        // One opaque user identifier per company and shared subject, reused
        // across independently installed origins belonging to that company.
        let mut user_id = None;
        for fact in node.installed_unit_facts()? {
            if fact
                .descriptor
                .config
                .pointer("/compat/format")
                .and_then(Value::as_str)
                != Some("poppy-connection")
            {
                continue;
            }
            let bytes =
                node.unit_content(&fact.unit_id, &fact.revision, Some(&fact.artifact_digest))?;
            let connection: Self = serde_json::from_slice(&bytes.bytes)?;
            if connection.report.company_identifier == report.company_identifier {
                if let Some(existing) = &user_id {
                    ensure!(
                        existing == &connection.user_id,
                        "installed company identities conflict; reconcile their original identities"
                    );
                }
                user_id = Some(connection.user_id);
            }
        }
        let user_id = user_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let identity = SessionIdentity::new(&report.company_identifier, &user_id)?;
        Ok(Self {
            schema_version: 1,
            session_id: identity.session_id().into(),
            user_id,
            agent_id: None,
            report,
        })
    }

    pub fn identity(&self) -> Result<SessionIdentity> {
        ensure!(self.schema_version == 1, "unsupported Poppy connection");
        SessionIdentity::resume(
            &self.report.company_identifier,
            &self.user_id,
            &self.session_id,
            self.agent_id.as_deref(),
        )
    }

    pub fn read(node: &Node, reference: &UnitRef) -> Result<Self> {
        let (fact, bytes) = reference.installed(node, UNIT_ID)?;
        ensure!(
            fact.descriptor
                .config
                .pointer("/compat/format")
                .and_then(Value::as_str)
                == Some("poppy-connection"),
            "unit is not a Poppy company connection"
        );
        let connection: Self = serde_json::from_slice(&bytes)?;
        connection.identity()?;
        Ok(connection)
    }

    pub fn credential_ref(&self) -> String {
        let binding = format!("{}\0{}", self.report.company_identifier, self.user_id);
        format!(
            "account-{}",
            Digest::blake3(binding.as_bytes())
                .0
                .trim_start_matches("blake3:")
        )
    }

    pub fn export(&self, unit_id: &str, enabled: bool) -> Result<UnitExport> {
        self.identity()?;
        let bytes = serde_json::to_vec(self)?;
        let digest = Digest::blake3(&bytes);
        let revision = content_revision(&digest)?;
        Ok(UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: unit_id.into(),
            root_revision: Some(revision.clone()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled,
            packages: vec![
                UnitPackage {
                    unit: UnitDescriptor {
                        unit_id: unit_id.into(),
                        kind: UnitKind::Code,
                        revision,
                        artifact_digest: digest,
                        provides: vec![format!("poppy:{unit_id}")],
                        dependencies: vec![plugin_dependency(UNIT_ID)?],
                        applicability: None,
                        config: json!({"compat":{"format":"poppy-connection","schema_version":1},
                        "cognition":{"title":unit_id,"capability_card":"Company discovery and stable Poppy identity; account credentials depend on this node", "provenance_refs":[self.report.source.url]}}),
                    },
                    artifact_base64: STANDARD.encode(bytes),
                    evidence: Vec::new(),
                    resolved_dependencies: None,
                },
                plugin_package(UNIT_ID)?,
            ],
        })
    }
}
