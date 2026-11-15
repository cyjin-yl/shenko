//! Read-only Poppy company discovery and optional UCP profile observation.
//!
//! All returned capabilities and metadata remain external declarations. A report
//! is neither an authenticated session nor evidence of protocol negotiation.

mod http;

use std::{collections::BTreeMap, future::Future, time::Duration};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use http::{Fetched, HttpFetcher};

const POPPY_PATH: &str = "/.well-known/poppy.json";

/// Network exceptions require explicit configuration by the caller. They are
/// never inferred from untrusted company metadata.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiscoveryOptions {
    pub allow_non_default_ports: bool,
    pub allow_private_networks: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryState {
    Discovered,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoverySource {
    pub requested_url: String,
    pub url: String,
    pub redirects: Vec<String>,
    pub http_status: u16,
    pub fetched_at_unix_ms: u64,
    pub body_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveredDocument {
    pub source: DiscoverySource,
    /// Untrusted external declarations, retained for inspection.
    pub metadata: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProtocol {
    pub protocol_type: String,
    pub endpoint: String,
    pub description: Option<String>,
    pub access: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApiDeclaration {
    pub api_type: String,
    pub url: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeclaredCapabilities {
    pub agent: Vec<AgentProtocol>,
    pub apis: Vec<ApiDeclaration>,
    /// An empty object is meaningful: ordinary signed-out browsing is offered.
    pub web: Option<Value>,
    pub tasks_endpoint: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthDiscovery {
    pub protected_resource_source: DiscoverySource,
    pub protected_resource_metadata: Option<Value>,
    pub authorization_server: Option<DiscoveredDocument>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub state: DiscoveryState,
    /// The initial well-known observation, including permitted redirects.
    pub discovery_source: DiscoverySource,
    /// The selected document, which may come from `supported_versions`.
    pub source: DiscoverySource,
    pub company_origin: String,
    pub company_identifier: String,
    pub issuer: Option<String>,
    /// Version actually present in the selected Poppy document.
    pub protocol_version: String,
    /// Implemented version to use in a future Poppy request; no request is made.
    pub request_protocol_version: String,
    pub metadata: Value,
    pub capabilities: DeclaredCapabilities,
    pub oauth: OAuthDiscovery,
    /// A fetched profile is discovered only, never negotiated or executed.
    pub ucp: Option<DiscoveredDocument>,
}

/// Validate an origin before a runtime arms an external observation. DNS is
/// checked again, and pinned, immediately before each actual HTTP connection.
pub fn validate_origin(origin: &str) -> Result<Url> {
    origin_with_options(origin, &DiscoveryOptions::default())
}

pub async fn discover(origin: &str) -> Result<DiscoveryReport> {
    discover_with_options(origin, DiscoveryOptions::default()).await
}

/// Performs only anonymous metadata GETs. No cookies, bearer tokens, Poppy
/// sessions, checkout calls, or UCP platform profile are sent.
pub async fn discover_with_options(
    origin: &str,
    options: DiscoveryOptions,
) -> Result<DiscoveryReport> {
    let origin = origin_with_options(origin, &options)?;
    tokio::time::timeout(
        Duration::from_secs(60),
        discover_using(&HttpFetcher { options }, origin, &options),
    )
    .await
    .context("Poppy discovery exceeded its total time limit")?
}

trait MetadataFetcher: Sync {
    fn get(&self, url: Url, follow_redirects: bool)
    -> impl Future<Output = Result<Fetched>> + Send;
}

async fn discover_using(
    fetcher: &impl MetadataFetcher,
    origin: Url,
    options: &DiscoveryOptions,
) -> Result<DiscoveryReport> {
    let initial = fetcher.get(origin.join(POPPY_PATH)?, true).await?;
    http::validate_url(&initial.url, options)?;
    ensure!(
        initial.url.path() == POPPY_PATH && initial.url.query().is_none(),
        "final Poppy discovery URL must be {POPPY_PATH} without a query"
    );
    let initial_metadata = http::parse_json(&initial)?;
    let discovery_source = source(&initial);
    let initial_version = document_version(&initial_metadata)?;
    validate_company_domain(&initial_metadata, &initial.url, initial_version, options)?;
    let candidates = supported_versions(&initial_metadata, &initial.url, options)?;
    let selected_version = select_version(initial_version, &candidates)
        .context("company offers no compatible Poppy version (supported: 1.x and 0.4)")?;
    let (selected, metadata) = if selected_version == initial_version {
        (initial, initial_metadata)
    } else {
        let candidate = candidates
            .get(&selected_version)
            .context("selected Poppy version has no document URL")?;
        let response = fetcher.get(candidate.clone(), false).await?;
        ensure!(
            response.url == *candidate && response.redirects.is_empty(),
            "versioned Poppy documents must not redirect"
        );
        let value = http::parse_json(&response)?;
        ensure!(
            document_version(&value)? == selected_version,
            "versioned Poppy document protocol_version does not match supported_versions key"
        );
        validate_company_domain(&value, &initial.url, selected_version, options)?;
        ensure!(
            declared_issuer(&value)? == declared_issuer(&initial_metadata)?,
            "Poppy company issuer changed between protocol versions"
        );
        (response, value)
    };
    let company_url = origin_with_options(&initial_url_origin(&selected.url, &metadata)?, options)?;
    let company_origin = company_url.origin().ascii_serialization();
    let capabilities = validate_capabilities(&metadata, selected_version, options)?;
    validate_auth(&metadata, &capabilities, options)?;
    let (issuer, oauth) = discover_issuer(fetcher, &company_url, &metadata, options).await?;
    let ucp = if advertises_ucp(&metadata)? {
        let response = fetcher
            .get(company_url.join("/.well-known/ucp")?, false)
            .await?;
        ensure!(
            response.redirects.is_empty()
                && response.url == company_url.join("/.well-known/ucp")?,
            "UCP discovery must not redirect"
        );
        let profile = http::parse_json(&response)?;
        let ucp = object_field(&profile, "ucp")?;
        string_field(ucp, "version")?;
        // Preserve the profile and capability versions verbatim. This does not
        // claim a mutual capability intersection or follow advertised services.
        Some(DiscoveredDocument {
            source: source(&response),
            metadata: profile,
        })
    } else {
        None
    };
    // `selected` may be a version URL. Its company origin was checked against
    // the final main well-known domain before selection.
    let selected_source = source(&selected);
    Ok(DiscoveryReport {
        state: DiscoveryState::Discovered,
        discovery_source,
        source: selected_source,
        company_identifier: issuer.clone().unwrap_or_else(|| company_origin.clone()),
        company_origin,
        issuer,
        protocol_version: selected_version.to_string(),
        request_protocol_version: if selected_version.0 == 1 {
            "1.0"
        } else {
            "0.4"
        }
        .to_owned(),
        metadata,
        capabilities,
        oauth,
        ucp,
    })
}

fn origin_with_options(origin: &str, options: &DiscoveryOptions) -> Result<Url> {
    ensure!(
        origin.trim() == origin,
        "origin must not contain surrounding whitespace"
    );
    let url = Url::parse(origin).context("invalid company origin")?;
    http::validate_url(&url, options)?;
    ensure!(
        url.path() == "/" && url.query().is_none() && url.fragment().is_none(),
        "company origin must not contain a path, query, or fragment"
    );
    Ok(url)
}

fn initial_url_origin(selected_url: &Url, metadata: &Value) -> Result<String> {
    // Version URLs have the same host and port as the final company document.
    let domain = string_field(object_field(metadata, "organization")?, "domain")?;
    let origin = format!("https://{domain}");
    ensure!(
        Url::parse(&origin)?.origin() == selected_url.origin(),
        "company origin changed during discovery"
    );
    Ok(origin)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version(u32, u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.0, self.1)
    }
}

fn parse_version(value: &str) -> Result<Version> {
    let (major, minor) = value
        .split_once('.')
        .context("Poppy version must have major.minor form")?;
    ensure!(
        !major.is_empty()
            && !minor.is_empty()
            && major.bytes().all(|c| c.is_ascii_digit())
            && minor.bytes().all(|c| c.is_ascii_digit()),
        "Poppy version must have major.minor form"
    );
    let version = Version(major.parse()?, minor.parse()?);
    ensure!(
        version.to_string() == value,
        "Poppy version must use canonical major.minor form"
    );
    Ok(version)
}

fn document_version(metadata: &Value) -> Result<Version> {
    parse_version(string_field(metadata, "protocol_version")?)
}

fn compatible(version: Version) -> bool {
    version.0 == 1 || version == Version(0, 4)
}

fn select_version(initial: Version, candidates: &BTreeMap<Version, Url>) -> Option<Version> {
    std::iter::once(initial)
        .chain(candidates.keys().copied())
        .filter(|v| compatible(*v))
        .max()
}

fn validate_company_domain(
    metadata: &Value,
    final_url: &Url,
    version: Version,
    options: &DiscoveryOptions,
) -> Result<()> {
    let organization = object_field(metadata, "organization")?;
    string_field(organization, "name")?;
    let domain = string_field(organization, "domain")?;
    let expected = match final_url.port() {
        Some(port) => format!(
            "{}:{port}",
            final_url.host_str().context("company URL has no host")?
        ),
        None => final_url
            .host_str()
            .context("company URL has no host")?
            .to_owned(),
    };
    ensure!(
        domain.eq_ignore_ascii_case(&expected),
        "organization.domain does not match the final Poppy host"
    );
    if version.0 == 0 {
        ensure!(
            final_url.port_or_known_default() == Some(443),
            "Poppy 0.x discovery requires the default HTTPS port"
        );
    }
    if let Some(logo) = organization.get("logo_uri") {
        checked_url(logo, "organization.logo_uri", options)?;
    }
    Ok(())
}

fn supported_versions(
    metadata: &Value,
    final_url: &Url,
    options: &DiscoveryOptions,
) -> Result<BTreeMap<Version, Url>> {
    let mut result = BTreeMap::new();
    if let Some(versions) = metadata.get("supported_versions") {
        for (key, value) in versions
            .as_object()
            .context("supported_versions must be an object")?
        {
            let version = parse_version(key)?;
            let url = checked_url(value, "supported_versions URL", options)?;
            ensure!(
                url.origin() == final_url.origin(),
                "supported_versions URL must have the final company origin"
            );
            result.insert(version, url);
        }
    }
    Ok(result)
}

fn validate_capabilities(
    metadata: &Value,
    version: Version,
    options: &DiscoveryOptions,
) -> Result<DeclaredCapabilities> {
    ensure!(
        metadata.get("agent").is_some()
            || metadata.get("apis").is_some()
            || metadata.get("web").is_some(),
        "Poppy metadata requires at least one of agent, apis, or web"
    );
    let mut capabilities = DeclaredCapabilities {
        agent: Vec::new(),
        apis: Vec::new(),
        web: None,
        tasks_endpoint: None,
    };
    if let Some(agent) = metadata.get("agent") {
        ensure!(agent.is_object(), "agent must be an object");
        for protocol in agent
            .get("protocols")
            .and_then(Value::as_array)
            .context("agent.protocols must be an array")?
        {
            let description = optional_string(protocol, "description")?.map(str::to_owned);
            ensure!(
                version == Version(0, 4) || description.is_some(),
                "agent protocol description is required in Poppy 1.x"
            );
            let endpoint = checked_url(
                protocol
                    .get("endpoint")
                    .context("agent protocol endpoint is missing")?,
                "agent.endpoint",
                options,
            )?;
            capabilities.agent.push(AgentProtocol {
                protocol_type: string_field(protocol, "type")?.to_owned(),
                endpoint: endpoint.to_string(),
                description,
                access: protocol.get("access").cloned(),
            });
        }
    }
    if let Some(apis) = metadata.get("apis") {
        for api in apis.as_array().context("apis must be an array")? {
            let description = optional_string(api, "description")?.map(str::to_owned);
            ensure!(description.is_some(), "API description is required");
            let url = checked_url(
                api.get("url").context("API URL is missing")?,
                "api.url",
                options,
            )?;
            capabilities.apis.push(ApiDeclaration {
                api_type: string_field(api, "type")?.to_owned(),
                url: url.to_string(),
                description,
            });
        }
    }
    if let Some(web) = metadata.get("web") {
        ensure!(web.is_object(), "web must be an object");
        if let Some(endpoint) = web.get("browser_session_endpoint") {
            checked_url(endpoint, "web.browser_session_endpoint", options)?;
        }
        optional_bool(web, "disabled")?;
        optional_bool(web, "unscoped")?;
        capabilities.web = Some(web.clone());
    }
    if let Some(tasks) = metadata.get("tasks") {
        capabilities.tasks_endpoint = Some(
            checked_url(
                tasks.get("endpoint").context("tasks.endpoint is missing")?,
                "tasks.endpoint",
                options,
            )?
            .to_string(),
        );
    }
    Ok(capabilities)
}

fn validate_auth(
    metadata: &Value,
    capabilities: &DeclaredCapabilities,
    options: &DiscoveryOptions,
) -> Result<()> {
    if let Some(auth) = metadata.get("auth") {
        ensure!(auth.is_object(), "auth must be an object");
        if let Some(issuer) = auth.get("issuer") {
            issuer_url(
                issuer.as_str().context("auth.issuer must be a string")?,
                options,
            )?;
        }
        if optional_bool(auth, "disabled")? == Some(true) {
            ensure!(
                !capabilities
                    .agent
                    .iter()
                    .any(|protocol| protocol.protocol_type == "poppy"
                        && protocol.access.as_ref().is_some_and(has_signed_in_access)),
                "auth.disabled cannot advertise signed-in agent access"
            );
            ensure!(
                metadata
                    .get("extensions")
                    .and_then(|v| v.get("mediated_signin"))
                    .is_none(),
                "auth.disabled cannot advertise mediated sign-in"
            );
        }
    }
    Ok(())
}

fn has_signed_in_access(value: &Value) -> bool {
    // The specification requires unknown hints to be treated as absent.
    value.as_str() == Some("signed_in")
}

fn advertises_ucp(metadata: &Value) -> Result<bool> {
    let Some(extensions) = metadata.get("extensions") else {
        return Ok(false);
    };
    ensure!(extensions.is_object(), "extensions must be an object");
    let Some(ucp) = extensions.get("ucp") else {
        return Ok(false);
    };
    let entries = ucp.as_array().context("extensions.ucp must be an array")?;
    Ok(entries
        .iter()
        .any(|entry| entry.get("version").and_then(Value::as_str) == Some("1")))
}

async fn discover_issuer(
    fetcher: &impl MetadataFetcher,
    company: &Url,
    metadata: &Value,
    options: &DiscoveryOptions,
) -> Result<(Option<String>, OAuthDiscovery)> {
    let resource_url = company.join("/.well-known/oauth-protected-resource")?;
    let response = fetcher.get(resource_url.clone(), false).await?;
    ensure!(
        response.url == resource_url && response.redirects.is_empty(),
        "protected-resource metadata must not redirect"
    );
    let protected_resource_source = source(&response);
    let resource_metadata = if response.status == 404 {
        None
    } else {
        Some(http::parse_json(&response)?)
    };
    let company_origin = company.origin().ascii_serialization();
    let servers = authorization_servers(resource_metadata.as_ref(), &company_origin, options)?;
    let declared = declared_issuer(metadata)?;
    let chosen = match servers.as_slice() {
        [] => {
            ensure!(
                declared.is_none(),
                "auth.issuer is advertised but protected-resource metadata has no authorization server"
            );
            ensure!(
                !offers_sign_in(metadata),
                "company advertises sign-in without a discovered authorization server"
            );
            None
        }
        [only] => {
            ensure!(
                declared.is_none_or(|issuer| issuer == only),
                "auth.issuer does not match the protected-resource authorization server"
            );
            Some(only.clone())
        }
        _ => {
            let issuer = declared.context("multiple authorization servers require auth.issuer")?;
            ensure!(
                servers.iter().any(|server| server == issuer),
                "auth.issuer is not listed by protected-resource metadata"
            );
            Some(issuer.to_owned())
        }
    };
    let authorization_server = if let Some(issuer) = &chosen {
        let url = authorization_metadata_url(issuer, options)?;
        let response = fetcher.get(url.clone(), false).await?;
        ensure!(
            response.url == url && response.redirects.is_empty(),
            "authorization-server metadata must not redirect"
        );
        let value = http::parse_json(&response)?;
        ensure!(
            string_field(&value, "issuer")? == issuer,
            "authorization-server metadata issuer does not exactly match the selected issuer"
        );
        let resources = value
            .get("protected_resources")
            .and_then(Value::as_array)
            .context("authorization-server metadata must list protected_resources")?;
        ensure!(
            resources.iter().all(Value::is_string),
            "protected_resources must contain only strings"
        );
        ensure!(
            resources
                .iter()
                .filter_map(Value::as_str)
                .any(|resource| same_company_origin(resource, &company_origin)),
            "authorization-server metadata does not recognize the company origin"
        );
        Some(DiscoveredDocument {
            source: source(&response),
            metadata: value,
        })
    } else {
        None
    };
    Ok((
        chosen,
        OAuthDiscovery {
            protected_resource_source,
            protected_resource_metadata: resource_metadata,
            authorization_server,
        },
    ))
}

fn authorization_servers(
    metadata: Option<&Value>,
    company_origin: &str,
    options: &DiscoveryOptions,
) -> Result<Vec<String>> {
    let Some(metadata) = metadata else {
        return Ok(Vec::new());
    };
    ensure!(
        same_company_origin(string_field(metadata, "resource")?, company_origin),
        "protected-resource resource does not match the company origin"
    );
    let Some(servers) = metadata.get("authorization_servers") else {
        return Ok(Vec::new());
    };
    servers
        .as_array()
        .context("authorization_servers must be an array")?
        .iter()
        .map(|server| {
            let issuer = server
                .as_str()
                .context("authorization server must be a string")?;
            issuer_url(issuer, options)?;
            Ok(issuer.to_owned())
        })
        .collect()
}

fn same_company_origin(value: &str, expected: &str) -> bool {
    // Host casing is insignificant; a trailing slash, path, query, credentials,
    // or non-canonical default port is not silently erased during comparison.
    value
        .strip_prefix("https://")
        .zip(expected.strip_prefix("https://"))
        .is_some_and(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
}

fn offers_sign_in(metadata: &Value) -> bool {
    if metadata
        .get("auth")
        .and_then(|auth| auth.get("disabled"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return false;
    }
    metadata
        .get("agent")
        .and_then(|agent| agent.get("protocols"))
        .and_then(Value::as_array)
        .is_some_and(|protocols| {
            protocols.iter().any(|protocol| {
                protocol.get("type").and_then(Value::as_str) == Some("poppy")
                    && protocol.get("access").is_some_and(has_signed_in_access)
            })
        })
        || metadata
            .get("extensions")
            .and_then(|extensions| extensions.get("mediated_signin"))
            .is_some()
}

fn declared_issuer(metadata: &Value) -> Result<Option<&str>> {
    match metadata.get("auth") {
        Some(auth) => {
            ensure!(auth.is_object(), "auth must be an object");
            optional_string(auth, "issuer")
        }
        None => Ok(None),
    }
}

fn issuer_url(issuer: &str, options: &DiscoveryOptions) -> Result<Url> {
    let url = Url::parse(issuer).context("invalid OAuth issuer URL")?;
    http::validate_url(&url, options)?;
    ensure!(
        url.query().is_none(),
        "OAuth issuer must not contain a query"
    );
    Ok(url)
}

fn authorization_metadata_url(issuer: &str, options: &DiscoveryOptions) -> Result<Url> {
    let mut url = issuer_url(issuer, options)?;
    // RFC 8414 inserts the well-known component before the issuer path.
    let suffix = url.path().trim_end_matches('/');
    let path = format!("/.well-known/oauth-authorization-server{suffix}");
    url.set_path(&path);
    Ok(url)
}

fn checked_url(value: &Value, field: &str, options: &DiscoveryOptions) -> Result<Url> {
    let url = Url::parse(
        value
            .as_str()
            .with_context(|| format!("{field} must be a URL string"))?,
    )
    .with_context(|| format!("invalid {field}"))?;
    http::validate_url(&url, options).with_context(|| format!("invalid {field}"))?;
    Ok(url)
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    let result = value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))?;
    ensure!(!result.is_empty(), "{key} must not be empty");
    Ok(result)
}

fn optional_string<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>> {
    value.get(key).map(|_| string_field(value, key)).transpose()
}

fn object_field<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    let result = value
        .get(key)
        .with_context(|| format!("{key} is missing"))?;
    ensure!(result.is_object(), "{key} must be an object");
    Ok(result)
}

fn optional_bool(value: &Value, key: &str) -> Result<Option<bool>> {
    value
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("{key} must be a boolean"))
        })
        .transpose()
}

fn source(response: &Fetched) -> DiscoverySource {
    DiscoverySource {
        requested_url: response.requested_url.to_string(),
        url: response.url.to_string(),
        redirects: response.redirects.clone(),
        http_status: response.status,
        fetched_at_unix_ms: response.fetched_at_unix_ms,
        body_sha256: format!("{:x}", Sha256::digest(&response.body)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{collections::VecDeque, sync::Mutex};

    struct FixtureFetcher(Mutex<VecDeque<(bool, Fetched)>>);

    impl MetadataFetcher for FixtureFetcher {
        async fn get(&self, url: Url, follow_redirects: bool) -> Result<Fetched> {
            let (redirects, response) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .context("unexpected metadata request")?;
            ensure!(
                url == response.requested_url,
                "unexpected metadata URL: {url}"
            );
            ensure!(follow_redirects == redirects, "unexpected redirect policy");
            Ok(response)
        }
    }

    fn fixture(url: &str, redirects: bool, status: u16, metadata: Value) -> (bool, Fetched) {
        let url = Url::parse(url).unwrap();
        (
            redirects,
            Fetched {
                requested_url: url.clone(),
                url,
                redirects: Vec::new(),
                status,
                body: serde_json::to_vec(&metadata).unwrap(),
                fetched_at_unix_ms: 1,
            },
        )
    }

    #[tokio::test]
    async fn discovers_a_newer_compatible_document_and_read_only_ucp_profile() {
        let fetcher = FixtureFetcher(Mutex::new(VecDeque::from([
            fixture(
                "https://shop.example/.well-known/poppy.json",
                true,
                200,
                json!({
                    "protocol_version": "0.4",
                    "organization": {"name": "Shop", "domain": "shop.example"},
                    "supported_versions": {"1.1": "https://shop.example/poppy/1.1.json"},
                    "agent": {"protocols": [{"type": "ucp", "endpoint": "https://shop.example/.well-known/ucp"}]},
                    "web": {}
                }),
            ),
            fixture(
                "https://shop.example/poppy/1.1.json",
                false,
                200,
                json!({
                    "protocol_version": "1.1",
                    "organization": {"name": "Shop", "domain": "shop.example"},
                    "agent": {"protocols": [{"type": "ucp", "endpoint": "https://shop.example/.well-known/ucp", "description": "Shopping profile"}]},
                    "web": {}, "extensions": {"ucp": [{"version": "1"}]}
                }),
            ),
            fixture(
                "https://shop.example/.well-known/oauth-protected-resource",
                false,
                200,
                json!({
                    "resource": "https://shop.example", "authorization_servers": ["https://identity.example/tenant/shop"]
                }),
            ),
            fixture(
                "https://identity.example/.well-known/oauth-authorization-server/tenant/shop",
                false,
                200,
                json!({
                    "issuer": "https://identity.example/tenant/shop", "protected_resources": ["https://shop.example"]
                }),
            ),
            fixture(
                "https://shop.example/.well-known/ucp",
                false,
                200,
                json!({
                    "ucp": {"version": "2026-08-25", "capabilities": {"dev.ucp.shopping.checkout": [{"version": "2026-08-25"}]}}
                }),
            ),
        ])));
        let report = discover_using(
            &fetcher,
            validate_origin("https://shop.example").unwrap(),
            &DiscoveryOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(report.state, DiscoveryState::Discovered);
        assert_eq!(report.protocol_version, "1.1");
        assert_eq!(report.request_protocol_version, "1.0");
        assert_eq!(
            report.company_identifier,
            "https://identity.example/tenant/shop"
        );
        assert_eq!(
            report.discovery_source.url,
            "https://shop.example/.well-known/poppy.json"
        );
        assert_eq!(report.source.url, "https://shop.example/poppy/1.1.json");
        assert_eq!(report.capabilities.web, Some(json!({})));
        assert_eq!(
            report.ucp.unwrap().metadata["ucp"]["capabilities"]["dev.ucp.shopping.checkout"][0]["version"],
            "2026-08-25"
        );
        assert!(fetcher.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_issuer_confusion_without_an_issuerless_downgrade() {
        let fetcher = FixtureFetcher(Mutex::new(VecDeque::from([
            fixture(
                "https://shop.example/.well-known/poppy.json",
                true,
                200,
                json!({
                    "protocol_version": "1.0", "organization": {"name": "Shop", "domain": "shop.example"}, "web": {}
                }),
            ),
            fixture(
                "https://shop.example/.well-known/oauth-protected-resource",
                false,
                200,
                json!({
                    "resource": "https://shop.example", "authorization_servers": ["https://identity.example/tenant/shop"]
                }),
            ),
            fixture(
                "https://identity.example/.well-known/oauth-authorization-server/tenant/shop",
                false,
                200,
                json!({
                    "issuer": "https://identity.example/tenant/other", "protected_resources": ["https://shop.example"]
                }),
            ),
        ])));
        let error = discover_using(
            &fetcher,
            validate_origin("https://shop.example").unwrap(),
            &DiscoveryOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("issuer does not exactly match"));
        assert!(fetcher.0.lock().unwrap().is_empty());
    }

    #[test]
    fn refuses_private_redirect_before_any_http_connection() {
        let current = Url::parse("https://shop.example/.well-known/poppy.json").unwrap();
        let error = http::redirect_target(
            &current,
            Some("https://127.0.0.1/.well-known/poppy.json"),
            &DiscoveryOptions::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("non-public address"));
    }
}
