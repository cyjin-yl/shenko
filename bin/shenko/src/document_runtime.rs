//! Local configuration assertions for the Node-owned document generation.
//! Environment configuration cannot install, enable or attest a Node unit.
use std::{sync::Arc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use shenko_document::{
    CredentialHandle, OxrClient, OxrClientConfig, OxrEngineBinding, OxrTransport,
    DEFAULT_POLL_INTERVAL, DEFAULT_POLL_TIMEOUT, DEFAULT_REQUEST_TIMEOUT,
};
use shenko_exec_host::{DocumentParseHandler, DOCUMENT_OXR_UNIT, document_task::OxrInstallation};
use shenko_node::Node;
use shenko_store::Store;
use shenko_types::object::Digest;

/// Acquire only an authenticated, enabled Node generation. The legacy variables
/// remain deployment assertions; they never supply installation authority.
pub(crate) fn configured_handler(
    store: Arc<Store>, node: Arc<Mutex<Node>>,
) -> Result<Option<DocumentParseHandler>> {
    let Some(endpoint) = optional_env("SHENKO_OXR_ENDPOINT") else {
        return DocumentParseHandler::installed(store, node);
    };
    let (identity, profile) = {
        let node = node.lock();
        let Some(fact) = node.installed_unit_fact(DOCUMENT_OXR_UNIT)? else { return Ok(None) };
        if !fact.enabled { return Ok(None); }
        ensure!(fact.kind == shenko_node::UnitKind::Code, "OXR installation must be a code unit");
        let content = node.unit_content(DOCUMENT_OXR_UNIT, &fact.revision, Some(&fact.artifact_digest))?;
        ensure!(content.bytes.len() <= 64 * 1024 && Digest::blake3(&content.bytes) == fact.artifact_digest,
            "OXR installation artifact mismatch or oversized profile");
        // Do not render malformed artifact bytes into runtime diagnostics.
        let profile: OxrInstallation = serde_json::from_slice(&content.bytes)
            .map_err(|_| anyhow::anyhow!("invalid OXR installation profile"))?;
        (DocumentParseHandler::installed_identity(&node)?, profile)
    };
    validate_environment(&profile, endpoint)?;
    let handler = DocumentParseHandler::installed(store, node)?;
    ensure!(handler.as_ref().map(DocumentParseHandler::installation_identity).transpose()? == identity,
        "OXR installation changed during configuration acquisition");
    Ok(handler)
}

fn validate_environment(profile: &OxrInstallation, endpoint: String) -> Result<()> {
    let required = |name| optional_env(name)
        .with_context(|| format!("{name} is required when SHENKO_OXR_ENDPOINT is set"));
    let model = required("SHENKO_OXR_MODEL")?;
    let account = required("SHENKO_OXR_ACCOUNT")?;
    let credential = CredentialHandle::new(required("SHENKO_OXR_API_KEY")?);
    // Client construction normalizes URLs locally; it performs no HTTP request.
    // Keep diagnostics independent of the supplied endpoint and secret bytes.
    let normalize = |endpoint: String| -> Result<String> {
        OxrClient::new(OxrClientConfig::new(endpoint, credential.clone()))
            .map(|client| client.endpoint().to_owned())
            .map_err(|_| anyhow::anyhow!("invalid OXR endpoint configuration"))
    };
    ensure!(normalize(endpoint)? == normalize(profile.endpoint.clone())?,
        "SHENKO_OXR_ENDPOINT differs from the installed Node profile");
    ensure!(model == profile.model_identity,
        "SHENKO_OXR_MODEL differs from the installed Node profile");
    ensure!(profile.credential_env == "SHENKO_OXR_API_KEY",
        "environment-configured OXR requires the installed SHENKO_OXR_API_KEY credential handle");
    ensure!(optional_env("SHENKO_OXR_SCOPE").as_deref() == Some(profile.scope.as_str()),
        "SHENKO_OXR_SCOPE must match the installed nonempty Node deployment scope");
    // Account identity authorizes no new effect. Only explicit original-main
    // admission binds it to historical recovery; do not manufacture admission.
    if let Some(legacy) = &profile.legacy_main {
        ensure!(account == legacy.account_identity,
            "SHENKO_OXR_ACCOUNT differs from the installed original-main admission");
    }
    let transport = match optional_env("SHENKO_OXR_TRANSPORT").as_deref() {
        None | Some("async") => OxrTransport::Async,
        Some("sync") => OxrTransport::Sync,
        Some(_) => anyhow::bail!("SHENKO_OXR_TRANSPORT must be async or sync"),
    };
    ensure!(transport == profile.transport,
        "SHENKO_OXR_TRANSPORT differs from the installed Node profile");
    let engine = match optional_env("SHENKO_OXR_ENGINE").as_deref() {
        None | Some("remote") => OxrEngineBinding::Remote {},
        Some("local") => OxrEngineBinding::Local {
            attestation: optional_env("SHENKO_OXR_ENGINE_ATTESTATION"),
        },
        Some(_) => anyhow::bail!("SHENKO_OXR_ENGINE must be remote or local"),
    };
    ensure!(engine == profile.engine,
        "SHENKO_OXR_ENGINE and its attestation must match the installed Node profile");
    ensure!(duration_env("SHENKO_OXR_REQUEST_TIMEOUT_MS")?.unwrap_or(DEFAULT_REQUEST_TIMEOUT)
        == Duration::from_secs(profile.request_timeout_secs),
        "SHENKO_OXR_REQUEST_TIMEOUT_MS differs from the installed Node budget");
    ensure!(duration_env("SHENKO_OXR_POLL_TIMEOUT_MS")?.unwrap_or(DEFAULT_POLL_TIMEOUT)
        == Duration::from_secs(profile.poll_timeout_secs),
        "SHENKO_OXR_POLL_TIMEOUT_MS differs from the installed Node budget");
    ensure!(duration_env("SHENKO_OXR_POLL_INTERVAL_MS")?.unwrap_or(DEFAULT_POLL_INTERVAL)
        == Duration::from_millis(profile.poll_interval_millis),
        "SHENKO_OXR_POLL_INTERVAL_MS differs from the installed Node budget");
    Ok(())
}

fn optional_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn duration_env(name: &str) -> Result<Option<Duration>> {
    optional_env(name).map(|value| {
        let milliseconds: u64 = value.parse()
            .map_err(|_| anyhow::anyhow!("{name} must be an integer in milliseconds"))?;
        ensure!((1..=86_400_000).contains(&milliseconds),
            "{name} must be between 1 millisecond and 24 hours");
        Ok(Duration::from_millis(milliseconds))
    }).transpose()
}
