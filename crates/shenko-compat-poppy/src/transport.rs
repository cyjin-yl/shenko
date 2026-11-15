//! Destination-bound Poppy signing for real HTTP requests.
//!
//! A binding is derived from verified discovery and optional API descriptions.
//! It conveys where protocol credentials belong, never permission to perform a
//! business action. Dispatch, idempotency and recovery belong to the host.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{
    Client, Request,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderName, HeaderValue},
};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    discovery::{DiscoveryOptions, DiscoveryReport, http},
    session::{OperatorSigner, SessionIdentity, SessionSigner},
};

const SIGNATURE_LABEL: &str = "poppy";

/// An API's actual request destinations, distinguished from its description.
/// No public constructor accepts an arbitrary URL without discovery evidence.
pub struct EndpointBinding {
    company: String,
    version: String,
    scope: Scope,
    excluded: Vec<Url>,
    options: DiscoveryOptions,
}

enum Scope {
    Exact(Url, &'static [&'static str]),
    Mcp(Url, String),
    Conversation(Url, String),
    PlannedRequest {
        url: Url,
        method: &'static str,
        service: String,
    },
    OpenApi(Vec<ApiRoute>),
}

struct ApiRoute {
    method: String,
    server: Url,
    resource: String,
    path: String,
}

impl EndpointBinding {
    pub fn from_discovery(report: &DiscoveryReport, endpoint: &str) -> Result<Self> {
        Self::from_discovery_with_options(report, endpoint, DiscoveryOptions::default())
    }

    /// Exceptions are local deployment configuration, never taken from metadata.
    pub fn from_discovery_with_options(
        report: &DiscoveryReport,
        endpoint: &str,
        options: DiscoveryOptions,
    ) -> Result<Self> {
        let endpoint = checked_url(endpoint, &options)?;
        let mut scope = None;
        for api in &report.capabilities.apis {
            if api.api_type == "mcp" && endpoint == checked_url(&api.url, &options)? {
                scope = Some(Scope::Mcp(endpoint.clone(), api.url.clone()));
            }
        }
        for protocol in &report.capabilities.agent {
            if protocol.protocol_type == "poppy" {
                let base = checked_url(&protocol.endpoint, &options)?;
                if conversation_route(&base, "GET", &endpoint)
                    || conversation_route(&base, "POST", &endpoint)
                {
                    scope = Some(Scope::Conversation(base, protocol.endpoint.clone()));
                }
            }
        }
        if let Some(tasks) = &report.capabilities.tasks_endpoint
            && endpoint == checked_url(tasks, &options)?
        {
            scope = Some(Scope::Exact(endpoint.clone(), &["GET"]));
        }
        if let Some(browser) = report
            .capabilities
            .web
            .as_ref()
            .and_then(|web| web.get("browser_session_endpoint"))
            .and_then(Value::as_str)
        {
            let mut authorize = checked_url(browser, &options)?;
            ensure!(
                authorize.query().is_none(),
                "browser Session endpoint has a query"
            );
            authorize.set_path(&format!(
                "{}/authorize",
                authorize.path().trim_end_matches('/')
            ));
            if endpoint == authorize {
                scope = Some(Scope::Exact(endpoint.clone(), &["POST"]));
            }
        }
        if let Some(services) = report
            .ucp
            .as_ref()
            .and_then(|ucp| ucp.metadata.pointer("/ucp/services"))
            .and_then(Value::as_object)
        {
            for service in services.values().filter_map(Value::as_array).flatten() {
                let Some(url) = service.get("endpoint").and_then(Value::as_str) else {
                    continue;
                };
                if endpoint != checked_url(url, &options)? {
                    continue;
                }
                if service.get("transport").and_then(Value::as_str) == Some("mcp") {
                    scope = Some(Scope::Mcp(endpoint.clone(), url.into()));
                }
            }
        }
        Self::new(
            report,
            scope.context("destination is not a declared Poppy resource endpoint")?,
            options,
        )
    }

    /// Use request routes from an OpenAPI document listed by this company.
    /// The host must fetch that exact description anonymously, retain its source
    /// and freeze the selected document with the operation. Server variables use
    /// their declared defaults; unresolved external references are not guessed.
    pub fn from_openapi(
        report: &DiscoveryReport,
        description_url: &str,
        description: &Value,
        options: DiscoveryOptions,
    ) -> Result<Self> {
        let source = checked_url(description_url, &options)?;
        ensure!(
            report
                .capabilities
                .apis
                .iter()
                .any(|api| api.api_type == "openapi" && api.url == description_url),
            "OpenAPI description is not listed by the company"
        );
        Self::new(
            report,
            Scope::OpenApi(openapi_routes(&source, description, &options, None)?),
            options,
        )
    }

    /// UCP REST endpoint routing comes from the service's declared OpenAPI
    /// schema. The profile's endpoint replaces that schema's server list, as
    /// UCP endpoint resolution specifies; paths and methods remain constrained.
    pub fn from_ucp_rest(
        report: &DiscoveryReport,
        endpoint: &str,
        schema_url: &str,
        schema: &Value,
        options: DiscoveryOptions,
    ) -> Result<Self> {
        let resource = endpoint.to_owned();
        let endpoint = checked_url(endpoint, &options)?;
        let source = checked_url(schema_url, &options)?;
        let services = report
            .ucp
            .as_ref()
            .and_then(|ucp| ucp.metadata.pointer("/ucp/services"))
            .and_then(Value::as_object)
            .context("company UCP services are missing")?;
        let declared = services
            .values()
            .filter_map(Value::as_array)
            .flatten()
            .any(|service| {
                service.get("transport").and_then(Value::as_str) == Some("rest")
                    && service
                        .get("endpoint")
                        .and_then(Value::as_str)
                        .is_some_and(|value| {
                            Url::parse(value).is_ok_and(|candidate| candidate == endpoint)
                        })
                    && service.get("schema").and_then(Value::as_str) == Some(schema_url)
            });
        ensure!(
            declared,
            "UCP REST endpoint/schema pair is not declared by the company"
        );
        let mut routes = openapi_routes(&source, schema, &options, Some(&endpoint))?;
        for route in &mut routes {
            route.server = endpoint.clone();
            route.resource.clone_from(&resource);
        }
        let mut binding = Self::new(report, Scope::OpenApi(routes), options)?;
        binding.excluded.push(source);
        Ok(binding)
    }

    /// Bind a typed UCP request to the exact URL derived from a service in the
    /// observed company profile. RequestPlan reconstructs its own route; the
    /// signer does not maintain a second table of commerce operations.
    pub fn from_ucp_plan(report: &DiscoveryReport, plan: &crate::ucp::RequestPlan) -> Result<Self> {
        Self::from_ucp_plan_with_options(report, plan, DiscoveryOptions::default())
    }

    pub fn from_ucp_plan_with_options(
        report: &DiscoveryReport,
        plan: &crate::ucp::RequestPlan,
        options: DiscoveryOptions,
    ) -> Result<Self> {
        let endpoint = checked_url(&plan.endpoint, &options)?;
        let services = report
            .ucp
            .as_ref()
            .and_then(|ucp| ucp.metadata.pointer("/ucp/services"))
            .and_then(|services| services.get(crate::ucp::SHOPPING_SERVICE))
            .and_then(Value::as_array)
            .context("company shopping services are missing")?;
        let transport = match plan.transport {
            crate::ucp::Transport::Mcp => "mcp",
            crate::ucp::Transport::Rest => "rest",
        };
        let mut service_base = None;
        for service in services {
            if service.get("transport").and_then(Value::as_str) != Some(transport)
                || service.get("version").and_then(Value::as_str) != Some(crate::ucp::VERSION)
            {
                continue;
            }
            let Some(base) = service.get("endpoint").and_then(Value::as_str) else {
                continue;
            };
            checked_url(base, &options)?;
            if plan.validate_destination(base).is_ok() {
                service_base = Some(base.to_owned());
                break;
            }
        }
        let service = service_base
            .context("UCP request is not derived from the company's declared service")?;
        let scope = match plan.transport {
            crate::ucp::Transport::Mcp => Scope::Mcp(endpoint, service),
            crate::ucp::Transport::Rest => Scope::PlannedRequest {
                url: endpoint,
                method: plan.method,
                service,
            },
        };
        Self::new(report, scope, options)
    }

    /// Fetch one website page without a browser. Browser navigation instead
    /// uses the join protocol and native cookies; it must not use this signer.
    pub fn website_page(
        report: &DiscoveryReport,
        page: &str,
        options: DiscoveryOptions,
    ) -> Result<Self> {
        let web = report
            .capabilities
            .web
            .as_ref()
            .context("company has no website declaration")?;
        ensure!(
            web.get("disabled") != Some(&Value::Bool(true)),
            "company website is disabled in discovery"
        );
        let page = checked_url(page, &options)?;
        let origin = checked_url(&report.company_origin, &options)?;
        ensure!(
            origin.origin() == page.origin(),
            "website page is outside the company origin"
        );
        Self::new(report, Scope::Exact(page, &["GET", "HEAD"]), options)
    }

    fn new(report: &DiscoveryReport, scope: Scope, options: DiscoveryOptions) -> Result<Self> {
        ensure!(
            matches!(report.request_protocol_version.as_str(), "1.0" | "0.4"),
            "unsupported Poppy request version"
        );
        let mut excluded = vec![
            checked_url(&report.source.url, &options)?,
            checked_url(&report.discovery_source.url, &options)?,
            checked_url(&report.oauth.protected_resource_source.url, &options)?,
        ];
        excluded.extend(
            report
                .capabilities
                .apis
                .iter()
                .filter(|api| api.api_type == "openapi")
                .map(|api| checked_url(&api.url, &options))
                .collect::<Result<Vec<_>>>()?,
        );
        if let Some(ucp) = &report.ucp {
            excluded.push(checked_url(&ucp.source.url, &options)?);
            if let Some(services) = ucp
                .metadata
                .pointer("/ucp/services")
                .and_then(Value::as_object)
            {
                for service in services.values().filter_map(Value::as_array).flatten() {
                    if let Some(schema) = service.get("schema").and_then(Value::as_str) {
                        excluded.push(checked_url(schema, &options)?);
                    }
                }
            }
        }
        if let Some(auth) = &report.oauth.authorization_server {
            excluded.push(checked_url(&auth.source.url, &options)?);
            for field in [
                "authorization_endpoint",
                "token_endpoint",
                "revocation_endpoint",
                "device_authorization_endpoint",
                "pushed_authorization_request_endpoint",
                "introspection_endpoint",
                "jwks_uri",
            ] {
                if let Some(endpoint) = auth.metadata.get(field).and_then(Value::as_str) {
                    excluded.push(checked_url(endpoint, &options)?);
                }
            }
        }
        Ok(Self {
            company: report.company_identifier.clone(),
            version: report.request_protocol_version.clone(),
            scope,
            excluded,
            options,
        })
    }

    pub fn company_identifier(&self) -> &str {
        &self.company
    }
    pub fn protocol_version(&self) -> &str {
        &self.version
    }

    /// The service identifier for this actual route, as required by Poppy
    /// section 4.5. Preserve the original absolute identifier: OAuth resource
    /// equality does not normalize an origin-only URL by adding a slash.
    /// Protocol-default endpoints (tasks, browser authorization, website) have
    /// no separate service identifier and may only use the default token.
    pub fn service_url(&self, method: &str, url: &Url) -> Result<Option<&str>> {
        self.validate(method, url)?;
        match &self.scope {
            Scope::Exact(..) => Ok(None),
            Scope::Mcp(_, service)
            | Scope::Conversation(_, service)
            | Scope::PlannedRequest { service, .. } => Ok(Some(service)),
            Scope::OpenApi(routes) => {
                let mut selected = None;
                for route in routes.iter().filter(|route| route.matches(method, url)) {
                    ensure!(
                        selected.is_none_or(|resource| resource == route.resource.as_str()),
                        "OpenAPI request matches different service resource identifiers"
                    );
                    selected = Some(route.resource.as_str());
                }
                Ok(selected)
            }
        }
    }

    /// MCP's authorization profile requires verified protected-resource
    /// metadata; a missing document cannot make it a default-token service.
    pub fn requires_resource_metadata(&self) -> bool {
        matches!(self.scope, Scope::Mcp(..))
    }

    pub fn validate(&self, method: &str, url: &Url) -> Result<()> {
        http::validate_url(url, &self.options)?;
        ensure!(
            !self
                .excluded
                .iter()
                .any(|excluded| same_resource(excluded, url)),
            "discovery, descriptions and OAuth endpoints must not receive a Poppy Session"
        );
        let allowed = match &self.scope {
            Scope::Exact(endpoint, methods) => {
                methods.contains(&method)
                    && same_resource(endpoint, url)
                    && endpoint
                        .query()
                        .is_none_or(|query| Some(query) == url.query())
            }
            Scope::Mcp(endpoint, _) => {
                matches!(method, "GET" | "POST" | "DELETE") && endpoint == url
            }
            Scope::Conversation(endpoint, _) => conversation_route(endpoint, method, url),
            Scope::PlannedRequest {
                url: planned,
                method: planned_method,
                ..
            } => planned == url && *planned_method == method,
            Scope::OpenApi(routes) => routes.iter().any(|route| route.matches(method, url)),
        };
        ensure!(
            allowed,
            "request destination is outside the selected Poppy endpoint binding"
        );
        Ok(())
    }
}

impl ApiRoute {
    fn matches(&self, method: &str, url: &Url) -> bool {
        self.method.eq_ignore_ascii_case(method)
            && path_under_base(&self.server, url)
                .is_some_and(|path| path_template_matches(&self.path, path))
    }
}

fn openapi_routes(
    source: &Url,
    document: &Value,
    options: &DiscoveryOptions,
    override_server: Option<&Url>,
) -> Result<Vec<ApiRoute>> {
    ensure!(
        document
            .get("openapi")
            .and_then(Value::as_str)
            .is_some_and(|version| version.starts_with("3.")),
        "expected an OpenAPI 3 description"
    );
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .context("OpenAPI paths missing")?;
    let mut routes = Vec::new();
    for (path, item) in paths {
        if path.starts_with("x-") {
            continue;
        }
        ensure!(path.starts_with('/'), "OpenAPI path must start with '/'");
        ensure!(
            item.get("$ref").is_none(),
            "OpenAPI path references must be resolved before binding"
        );
        for method in [
            "get", "post", "put", "patch", "delete", "options", "head", "trace",
        ] {
            let Some(operation) = item.get(method).filter(|value| value.is_object()) else {
                continue;
            };
            if let Some(server) = override_server {
                routes.push(ApiRoute {
                    method: method.into(),
                    server: server.clone(),
                    resource: server.to_string(),
                    path: path.clone(),
                });
                continue;
            }
            let servers = operation
                .get("servers")
                .or_else(|| item.get("servers"))
                .or_else(|| document.get("servers"));
            let values = match servers {
                Some(value) => value
                    .as_array()
                    .context("OpenAPI servers must be an array")?
                    .as_slice(),
                None => &[],
            };
            if values.is_empty() {
                routes.push(ApiRoute {
                    method: method.into(),
                    server: source.join("/")?,
                    resource: source.join("/")?.to_string(),
                    path: path.clone(),
                });
            }
            for server in values {
                let mut value = server
                    .get("url")
                    .and_then(Value::as_str)
                    .context("OpenAPI server URL missing")?
                    .to_owned();
                if let Some(variables) = server.get("variables").and_then(Value::as_object) {
                    for (name, variable) in variables {
                        let default = variable
                            .get("default")
                            .and_then(Value::as_str)
                            .context("OpenAPI server variable default missing")?;
                        if let Some(values) = variable.get("enum").and_then(Value::as_array) {
                            ensure!(
                                values.iter().any(|entry| entry.as_str() == Some(default)),
                                "OpenAPI server variable default is outside its enum"
                            );
                        }
                        value = value.replace(&format!("{{{name}}}"), default);
                    }
                }
                ensure!(
                    !value.contains(['{', '}', '\\']) && !value.chars().any(char::is_whitespace),
                    "unresolved or invalid OpenAPI server URL"
                );
                let server = source.join(&value)?;
                http::validate_url(&server, options)?;
                ensure!(server.query().is_none(), "OpenAPI server URL has a query");
                let resource = if Url::parse(&value).is_ok() {
                    value
                } else {
                    server.to_string()
                };
                routes.push(ApiRoute {
                    method: method.into(),
                    server,
                    resource,
                    path: path.clone(),
                });
            }
        }
    }
    ensure!(
        !routes.is_empty(),
        "OpenAPI description has no callable routes"
    );
    Ok(routes)
}

fn checked_url(value: &str, options: &DiscoveryOptions) -> Result<Url> {
    ensure!(
        !value.chars().any(char::is_whitespace) && !value.chars().any(char::is_control),
        "endpoint URL contains whitespace or control characters"
    );
    let url = Url::parse(value).context("invalid Poppy endpoint URL")?;
    http::validate_url(&url, options)?;
    Ok(url)
}

fn same_resource(first: &Url, second: &Url) -> bool {
    first.origin() == second.origin() && first.path() == second.path()
}

fn path_under_base<'a>(base: &Url, target: &'a Url) -> Option<&'a str> {
    if base.origin() != target.origin() {
        return None;
    }
    let base_path = base.path().trim_end_matches('/');
    let suffix = target.path().strip_prefix(base_path)?;
    if suffix.is_empty() {
        return Some("/");
    }
    suffix.starts_with('/').then_some(suffix)
}

fn conversation_route(base: &Url, method: &str, target: &Url) -> bool {
    if same_resource(base, target) {
        return method == "POST";
    }
    let Some(relative) = path_under_base(base, target) else {
        return false;
    };
    let Some((id, action)) = relative
        .strip_prefix('/')
        .and_then(|path| path.split_once('/'))
    else {
        return false;
    };
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && matches!(
            (method, action),
            ("GET", "events") | ("POST", "messages" | "handoff" | "handoff/cancel" | "close")
        )
}

fn path_template_matches(template: &str, actual: &str) -> bool {
    let mut expected = template.split('/');
    let mut actual = actual.split('/');
    loop {
        match (expected.next(), actual.next()) {
            (None, None) => return true,
            (Some(pattern), Some(segment))
                if pattern == segment
                    || (pattern.starts_with('{')
                        && pattern.ends_with('}')
                        && pattern.len() > 2
                        && !segment.is_empty()) => {}
            _ => return false,
        }
    }
}

/// Long-lived local context that renews an expired delegation without changing
/// the persisted company/user/Session mapping. No private key is serializable.
pub struct RequestSigner {
    operator: Arc<OperatorSigner>,
    session: Mutex<SessionSigner>,
    binding: EndpointBinding,
    jwks_uri: String,
}

impl RequestSigner {
    pub fn new(
        operator: Arc<OperatorSigner>,
        session: SessionSigner,
        binding: EndpointBinding,
        jwks_uri: &str,
    ) -> Result<Self> {
        ensure!(
            session.identity().company_identifier() == binding.company_identifier(),
            "Session company differs from endpoint discovery"
        );
        ensure!(
            session.client_id() == operator.client_id(),
            "Session signer belongs to a different operator"
        );
        operator.client_metadata("Shenko", jwks_uri)?;
        Ok(Self {
            operator,
            session: Mutex::new(session),
            binding,
            jwks_uri: jwks_uri.into(),
        })
    }

    /// Check the target before asking a credential provider to attach tokens.
    pub fn validate_request(&self, request: &Request) -> Result<()> {
        self.binding
            .validate(request.method().as_str(), request.url())
    }

    /// Sign the actual, fully assembled wire request. Authorization and DPoP,
    /// UCP metadata, idempotency and content type must be set first. This never
    /// sends anything and cannot turn an unknown effect into a retry.
    pub fn authorize(&self, request: &mut Request, access_token: Option<&str>) -> Result<()> {
        self.validate_request(request)?;
        validate_authorization(request, access_token)?;
        for header in [
            "poppy-session",
            "signature",
            "signature-input",
            "signature-agent",
        ] {
            ensure!(
                !request.headers().contains_key(header),
                "request was already signed"
            );
        }
        let now = unix_seconds()?;
        let mut session = self
            .session
            .lock()
            .map_err(|_| anyhow::anyhow!("Poppy Session signer lock poisoned"))?;
        if now >= session.expires_at() {
            let identity = session.identity();
            let renewed = SessionIdentity::resume(
                identity.company_identifier(),
                identity.user_id(),
                identity.session_id(),
                identity.agent_id(),
            )?;
            *session = self.operator.delegate(renewed, Duration::from_secs(3600))?;
        }
        let jwt = session.sign_request(request.method().as_str(), request.url(), access_token)?;
        drop(session);
        insert_sensitive(request, "poppy-session", &jwt)?;
        insert(
            request,
            "poppy-version",
            self.binding.protocol_version(),
            false,
        )?;
        self.sign_operator(request, now)
    }

    fn sign_operator(&self, request: &mut Request, now: u64) -> Result<()> {
        let thumbprint = self.operator.http_key_thumbprint()?;
        if request.headers().contains_key("ucp-agent") {
            ensure!(
                self.operator.key_id() == thumbprint,
                "UCP/Web Bot Auth interoperability requires the published JWK kid to be its RFC 7638 thumbprint"
            );
        }
        let agent_value = structured_string(&self.jwks_uri)?;
        insert(
            request,
            "signature-agent",
            &format!("{SIGNATURE_LABEL}={agent_value}"),
            false,
        )?;
        let mut components = vec![
            (
                "\"@method\"".to_owned(),
                request.method().as_str().to_owned(),
            ),
            (
                "\"@authority\"".to_owned(),
                request.url()[url::Position::BeforeHost..url::Position::AfterPort].to_owned(),
            ),
            ("\"@path\"".to_owned(), request.url().path().to_owned()),
            (
                "\"@target-uri\"".to_owned(),
                request.url().as_str().to_owned(),
            ),
        ];
        if let Some(query) = request.url().query() {
            components.push(("\"@query\"".into(), format!("?{query}")));
        }
        components.push((
            format!("\"signature-agent\";key=\"{SIGNATURE_LABEL}\""),
            agent_value,
        ));
        if let Some(body) = request.body() {
            let bytes = body
                .as_bytes()
                .context("signed Poppy requests require a fully materialized body")?;
            let digest = format!("sha-256=:{}:", STANDARD.encode(Sha256::digest(bytes)));
            ensure!(
                request.headers().contains_key(CONTENT_TYPE),
                "signed request body requires Content-Type"
            );
            insert(request, "content-digest", &digest, false)?;
        }
        for name in [
            "poppy-session",
            "poppy-version",
            "poppy-extensions",
            "authorization",
            "dpop",
            "ucp-agent",
            "idempotency-key",
            "request-id",
            "content-digest",
            "content-type",
            "shopify-buyer-ip",
        ] {
            if let Some(value) = request.headers().get(name) {
                ensure!(
                    request.headers().get_all(name).iter().count() == 1,
                    "duplicate signed request header"
                );
                let value = value
                    .to_str()
                    .context("signed request header is not ASCII")?
                    .trim();
                ensure!(
                    !value.contains(['\r', '\n']),
                    "signed request header contains a line break"
                );
                components.push((format!("\"{name}\""), value.to_owned()));
            }
        }
        let expires = now
            .checked_add(300)
            .context("HTTP signature expiry overflow")?;
        let mut nonce = [0_u8; 64];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("system random source unavailable"))?;
        let params = format!(
            "({});created={now};expires={expires};keyid=\"{thumbprint}\";alg=\"ecdsa-p256-sha256\";nonce=\"{}\";tag=\"web-bot-auth\"",
            components
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            URL_SAFE_NO_PAD.encode(nonce)
        );
        let base = format!(
            "{}\n\"@signature-params\": {params}",
            components
                .iter()
                .map(|(name, value)| format!("{name}: {value}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let signature = self.operator.sign_http_base(base.as_bytes())?;
        insert_sensitive(
            request,
            "signature-input",
            &format!("{SIGNATURE_LABEL}={params}"),
        )?;
        insert_sensitive(
            request,
            "signature",
            &format!("{SIGNATURE_LABEL}=:{}:", STANDARD.encode(signature)),
        )
    }
}

fn validate_authorization(request: &Request, token: Option<&str>) -> Result<()> {
    let authorization = request.headers().get(AUTHORIZATION);
    match (authorization, token) {
        (None, None) => Ok(()),
        (Some(header), Some(token)) => {
            ensure!(
                request.headers().get_all(AUTHORIZATION).iter().count() == 1,
                "duplicate Authorization header"
            );
            let (scheme, value) = header
                .to_str()
                .context("invalid Authorization header")?
                .split_once(' ')
                .context("invalid Authorization scheme")?;
            ensure!(
                (scheme.eq_ignore_ascii_case("Bearer") || scheme.eq_ignore_ascii_case("DPoP"))
                    && value == token
                    && !token.is_empty(),
                "Session token binding differs from Authorization"
            );
            Ok(())
        }
        _ => bail!("Session access token and Authorization must be supplied together"),
    }
}

fn insert_sensitive(request: &mut Request, name: &'static str, value: &str) -> Result<()> {
    insert(request, name, value, true)
}
fn insert(request: &mut Request, name: &'static str, value: &str, sensitive: bool) -> Result<()> {
    let mut value =
        HeaderValue::from_str(value).context("invalid generated HTTP signature header")?;
    value.set_sensitive(sensitive);
    request
        .headers_mut()
        .insert(HeaderName::from_static(name), value);
    Ok(())
}
fn structured_string(value: &str) -> Result<String> {
    ensure!(
        value.bytes().all(|b| (0x20..=0x7e).contains(&b)),
        "HTTP structured string contains non-ASCII characters"
    );
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock precedes Unix epoch")?
        .as_secs())
}

/// Isolated DNS-pinned HTTPS client shared by signed resources and OAuth. The
/// host freezes and arms its durable intent before constructing/executing it.
/// This client must be used only for the bound origin, never reused after a
/// redirect. Response size limits and cancellation belong to the caller.
pub async fn pinned_client(
    url: &Url,
    options: &DiscoveryOptions,
    timeout: Duration,
) -> Result<Client> {
    ensure!(!timeout.is_zero(), "HTTP timeout must be positive");
    tokio::time::timeout(timeout, http::pinned_client(url, options, timeout))
        .await
        .context("endpoint DNS resolution timed out")?
}
