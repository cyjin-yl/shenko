//! Application-owned credential and durable-authority bridge to HTTP/MCP.
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use reqwest::{
    Method, Request,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use shenko_compat_mcp::protocol::{ConnectionSpec, HttpRequestAuthorizer, Session};
use shenko_compat_poppy::{
    auth::{AccountTokens, TokenType},
    discovery::DiscoveryOptions,
    session::SessionIdentity,
    transport::{EndpointBinding, RequestSigner, pinned_client},
    ucp::{CompletionBinding, PlatformProfile, RequestPlan},
};
use tokio_util::sync::CancellationToken;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use super::{
    account::load_account,
    connection::{Connection, Operator},
    vault::Vault,
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Authentication {
    #[default]
    SignedOut,
    Account {
        resource: Option<String>,
    },
}

/// Raw responses can carry credentials; never derive Debug/Serialize here.
pub(super) struct HttpObservation {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Drop for HttpObservation {
    fn drop(&mut self) {
        self.body.zeroize();
    }
}

type Guard = Arc<dyn Fn() -> Result<()> + Send + Sync>;

pub(super) struct CompanyClient {
    vault: Arc<Vault>,
    operator: Operator,
    connection: Connection,
    tokens: Option<Arc<AccountTokens>>,
    account_binding: Option<String>,
    session_id: String,
    resource: Option<String>,
    timeout: Duration,
    http: Arc<HttpState>,
    resources: Mutex<BTreeMap<(String, bool), Option<String>>>,
    guard: Option<Guard>,
    cancellation: Option<CancellationToken>,
}

impl CompanyClient {
    /// Entirely local preparation: this must complete before T1 freezes the
    /// actual operator, grant and company bindings used by the attempt.
    pub fn new(
        vault: Vault,
        connection: Connection,
        authentication: &Authentication,
        timeout: Duration,
    ) -> Result<Self> {
        let operator = Operator::load(&vault)?;
        let (tokens, account_binding, resource) = match authentication {
            Authentication::SignedOut => (None, None, None),
            Authentication::Account { resource } => {
                let grant = load_account(&vault, &operator, &connection)?
                    .context("account credentials are absent or an original credential exchange is unresolved on this node")?;
                grant.tokens.access_token_for(resource.as_deref())?;
                (
                    Some(Arc::new(grant.tokens)),
                    Some(grant.binding),
                    resource.clone(),
                )
            }
        };
        let vault = Arc::new(vault);
        let http = Arc::new(HttpState::load(vault.clone(), &operator, &connection)?);
        if let Some(tokens) = &tokens {
            http.secrets
                .lock()
                .remember(tokens.access_token_for(resource.as_deref())?)?;
        }
        // OAuth tokens are opaque, so a replacement grant is not proof of the
        // same merchant account. Give every grant its own stable Session;
        // refresh retains its binding. Anonymous cart/cookie state is not
        // implicitly transferred into the first signed-in Session.
        let session_id = if let Some(binding) = &account_binding {
            let bytes = serde_json::to_vec(&[
                "shenko.poppy.grant-session/1",
                &connection.session_id,
                binding,
            ])?;
            URL_SAFE_NO_PAD.encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref())
        } else {
            connection.session_id.clone()
        };
        Ok(Self {
            vault,
            operator,
            connection,
            tokens,
            account_binding,
            session_id,
            resource,
            timeout,
            http,
            resources: Mutex::new(BTreeMap::new()),
            guard: None,
            cancellation: None,
        })
    }

    pub fn with_guard(mut self, guard: Guard) -> Self {
        self.guard = Some(guard);
        self
    }
    pub fn with_cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
    pub fn operation_guard(&self) -> Result<Arc<dyn Fn() -> Result<()> + Send + Sync>> {
        self.guard
            .clone()
            .context("Poppy operation authority guard is missing")
    }
    pub fn vault(&self) -> &Vault {
        &self.vault
    }
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn profile(&self) -> &PlatformProfile {
        &self.operator.profile
    }
    pub fn client_id(&self) -> &str {
        &self.operator.config.client_id
    }
    pub fn tokens(&self) -> Option<&AccountTokens> {
        self.tokens.as_deref()
    }
    pub fn account_binding(&self) -> Option<&str> {
        self.account_binding.as_deref()
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn session_extensions(&self) -> Option<&'static str> {
        shenko_compat_poppy::transport::session_extensions(&self.connection.report)
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn nonce_for(&self, url: &str) -> Option<String> {
        self.http.nonce(NonceContext::AuthorizationServer, url)
    }

    /// Check before writing an OAuth credential-flight record so an already
    /// known rate limit cannot quarantine a request that has not been sent.
    /// The sender repeats both checks immediately before the real dispatch.
    pub fn check_endpoint_ready(&self, url: &Url) -> Result<()> {
        check_guard(&self.guard)?;
        self.http.check_backoff(url)
    }

    pub fn credential_for_checkout(
        &self,
        name: &str,
        binding: &CompletionBinding,
    ) -> Result<Value> {
        let suffix = name
            .strip_prefix("instrument-")
            .context("invalid payment credential reference")?;
        let id = uuid::Uuid::parse_str(suffix).context("invalid payment credential reference")?;
        ensure!(
            name == format!("instrument-{id}"),
            "payment credential reference is not canonical"
        );
        ensure!(
            binding.credential_ref == name,
            "payment credential reference differs from checkout binding"
        );
        binding.identity.validate_owner(
            &self.connection.report.company_identifier,
            &self.connection.user_id,
        )?;
        let bytes = self
            .vault
            .read(name)?
            .context("payment credential is absent on this node")?;
        let mut record: InstrumentRecord =
            serde_json::from_slice(&bytes).context("invalid local instrument credential")?;
        ensure!(
            record.schema_version == 1
                && record.company == self.connection.report.company_identifier
                && record.user_id == self.connection.user_id
                && record.client_id == self.client_id()
                && record.account_binding.as_deref() == self.account_binding()
                && record.checkout_id == binding.identity.resource_id
                && record.terms_sha256 == binding.terms_sha256,
            "payment credential belongs to different checkout, terms, or account credentials"
        );
        if let Some(credential) = record.instrument.get("credential") {
            self.http.secrets.lock().remember_value(credential)?;
        }
        Ok(std::mem::take(&mut record.instrument))
    }

    /// Public observations must not echo a credential used by this operation.
    /// Withhold the whole observation on a match; never alter purchase terms
    /// or IDs and then present the altered value as merchant evidence.
    pub fn redact_observation(&self, observation: &mut Value) -> Result<()> {
        if self.http.secrets.lock().contains_value(observation) {
            clear_json(observation);
            bail!("merchant observation reflected a local credential; response withheld");
        }
        Ok(())
    }

    fn signer(&self, binding: EndpointBinding) -> Result<RequestSigner> {
        let identity = SessionIdentity::resume(
            &self.connection.report.company_identifier,
            &self.connection.user_id,
            &self.session_id,
            self.connection.agent_id.as_deref(),
        )?;
        let session = self
            .operator
            .signer
            .delegate(identity, Duration::from_secs(3600))?;
        RequestSigner::new(
            self.operator.signer.clone(),
            session,
            binding,
            &self.operator.config.jwks_uri,
        )
    }

    fn authorizer(
        &self,
        binding: EndpointBinding,
        headers: BTreeMap<String, String>,
        resource: Option<String>,
    ) -> Result<Arc<CompanyAuthorizer>> {
        for key in headers.keys() {
            validate_public_header(key)?;
        }
        Ok(Arc::new(CompanyAuthorizer {
            signer: self.signer(binding)?,
            tokens: self.tokens.clone(),
            resource,
            headers,
            profile_url: self.profile().url().into(),
            http: self.http.clone(),
            timeout: self.timeout,
            guard: self.guard.clone(),
            cancellation: self.cancellation.clone(),
        }))
    }

    pub async fn connect_mcp(
        &self,
        endpoint: &str,
        headers: BTreeMap<String, String>,
    ) -> Result<Session> {
        let binding = EndpointBinding::from_discovery(&self.connection.report, endpoint)?;
        ensure!(
            binding.requires_resource_metadata(),
            "selected endpoint is not an MCP service"
        );
        let resource = self
            .verify_request_resource(&binding, "POST", &Url::parse(endpoint)?)
            .await?;
        let authorizer = self.authorizer(binding, headers, resource)?;
        Session::connect_authorized(
            &ConnectionSpec::StreamableHttp {
                url: endpoint.into(),
                header_env_refs: BTreeMap::new(),
            },
            self.timeout,
            authorizer,
        )
        .await
    }

    pub async fn send_signed(&self, request: Request) -> Result<HttpObservation> {
        let binding =
            EndpointBinding::from_discovery(&self.connection.report, request.url().as_str())?;
        self.send_with_binding(request, binding).await
    }

    pub async fn send_signed_plan(&self, plan: &RequestPlan) -> Result<HttpObservation> {
        ensure!(
            self.tokens
                .as_ref()
                .is_none_or(|tokens| tokens.metadata().token_type == TokenType::Bearer),
            "UCP requires a Bearer access token issued by the selected company issuer"
        );
        let binding = EndpointBinding::from_ucp_plan(&self.connection.report, plan)?;
        let mut request = Request::new(
            Method::from_bytes(plan.method.as_bytes())?,
            Url::parse(&plan.endpoint)?,
        );
        for (name, value) in &plan.headers {
            validate_public_header(name)?;
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
        if let Some(body) = plan.rest_body() {
            *request.body_mut() = Some(serde_json::to_vec(body)?.into());
        }
        self.send_with_binding(request, binding).await
    }

    /// Generic API callers select a route from an actually fetched OpenAPI
    /// document. The description itself is always fetched anonymously.
    pub async fn send_api(
        &self,
        request: Request,
        description_url: &str,
        description: &Value,
    ) -> Result<HttpObservation> {
        let binding = EndpointBinding::from_openapi(
            &self.connection.report,
            description_url,
            description,
            DiscoveryOptions::default(),
        )?;
        self.send_with_binding(request, binding).await
    }

    pub async fn read_website(&self, page: &str) -> Result<HttpObservation> {
        ensure!(
            self.tokens.is_none(),
            "a non-browser website read must be signed out"
        );
        let binding = EndpointBinding::website_page(
            &self.connection.report,
            page,
            DiscoveryOptions::default(),
        )?;
        self.send_with_binding(Request::new(Method::GET, Url::parse(page)?), binding)
            .await
    }

    async fn send_with_binding(
        &self,
        mut request: Request,
        binding: EndpointBinding,
    ) -> Result<HttpObservation> {
        let resource = self
            .verify_request_resource(&binding, request.method().as_str(), request.url())
            .await?;
        let authorizer = self.authorizer(binding, BTreeMap::new(), resource)?;
        let url = request.url().clone();
        let client = authorizer
            .prepare_client(&url)
            .await?
            .context("Poppy pinned HTTP client missing")?;
        authorizer.authorize(&mut request).await?;
        let response = client
            .execute(request)
            .await
            .map_err(|error| error.without_url())?;
        authorizer.observe_response(&url, response.status().as_u16(), response.headers())?;
        let observation = read_response(response, 1024 * 1024).await?;
        self.check_http_observation(&observation)?;
        Ok(observation)
    }

    /// OAuth exchanges carry private_key_jwt/DPoP only. Endpoint selection and
    /// assertion audience come from the validated OAuthServer, not a task URL.
    pub async fn send_oauth(&self, request: Request) -> Result<HttpObservation> {
        ensure!(
            !request.headers().contains_key("poppy-session")
                && !request.headers().contains_key("poppy-version")
                && !request.headers().contains_key("poppy-extensions"),
            "OAuth request contains Session protocol headers"
        );
        check_guard(&self.guard)?;
        let url = request.url().clone();
        self.http.check_backoff(&url)?;
        let client = pinned_client(&url, &DiscoveryOptions::default(), self.timeout).await?;
        check_guard(&self.guard)?;
        self.http.check_backoff(&url)?;
        let response = client
            .execute(request)
            .await
            .map_err(|error| error.without_url())?;
        self.http.observe(
            &url,
            response.status().as_u16(),
            response.headers(),
            Some(NonceContext::AuthorizationServer),
        )?;
        read_response(response, 64 * 1024).await
    }

    pub async fn fetch_schema(&self, url: &str) -> Result<Value> {
        let response = self.anonymous_get(Url::parse(url)?, 1024 * 1024).await?;
        ensure!(
            response.status == 200,
            "schema HTTP request did not return 200"
        );
        Ok(serde_json::from_slice(&response.body)?)
    }

    async fn anonymous_get(&self, url: Url, limit: usize) -> Result<HttpObservation> {
        check_guard(&self.guard)?;
        self.http.check_backoff(&url)?;
        let client = pinned_client(&url, &DiscoveryOptions::default(), self.timeout).await?;
        check_guard(&self.guard)?;
        self.http.check_backoff(&url)?;
        let response = client
            .execute(Request::new(Method::GET, url.clone()))
            .await
            .map_err(|error| error.without_url())?;
        self.http
            .observe(&url, response.status().as_u16(), response.headers(), None)?;
        read_response(response, limit).await
    }

    async fn verify_request_resource(
        &self,
        binding: &EndpointBinding,
        method: &str,
        url: &Url,
    ) -> Result<Option<String>> {
        let service = binding.service_url(method, url)?;
        let Some(tokens) = &self.tokens else {
            return Ok(None);
        };
        let resource = if let Some(service) = service {
            let key = (service.to_owned(), binding.requires_resource_metadata());
            let cached = self.resources.lock().get(&key).cloned();
            if let Some(resource) = cached {
                resource
            } else {
                let resource = self
                    .verify_service_resource(service, binding.requires_resource_metadata())
                    .await?;
                let mut resources = self.resources.lock();
                ensure!(
                    resources.len() < 128,
                    "too many protected resource identities in one operation"
                );
                resources.insert(key, resource.clone());
                resource
            }
        } else {
            None
        };
        ensure!(
            self.resource == resource,
            "selected access token is not for the actual service; refresh the existing grant for that service first"
        );
        tokens.access_token_for(resource.as_deref())?;
        Ok(resource)
    }

    async fn verify_service_resource(
        &self,
        service: &str,
        required: bool,
    ) -> Result<Option<String>> {
        let resource = Url::parse(service)?;
        let mut metadata_url = resource.clone();
        // RFC9728 inserts its well-known path before both path and query. Only
        // the origin's empty/root path is omitted; resource queries survive.
        let path = if resource.path() == "/" {
            ""
        } else {
            resource.path()
        };
        metadata_url.set_path(&format!("/.well-known/oauth-protected-resource{path}"));
        let response = self.anonymous_get(metadata_url, 64 * 1024).await?;
        if response.status == 404 {
            ensure!(!required, "MCP protected-resource metadata is missing");
            return Ok(None);
        }
        ensure!(
            response.status == 200,
            "protected-resource metadata could not be verified"
        );
        ensure!(
            response
                .headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json")),
            "protected-resource metadata is not application/json"
        );
        let metadata: Value = serde_json::from_slice(&response.body)?;
        // Poppy permits treating mismatched metadata as absent. This host is
        // deliberately stricter: only an actual 404 establishes absence, so
        // an unexpected identity cannot cause a default token to be sent.
        ensure!(
            metadata["resource"].as_str() == Some(service),
            "protected resource identity differs from the actual service"
        );
        ensure!(
            metadata["authorization_servers"]
                .as_array()
                .is_some_and(|servers| servers
                    .iter()
                    .any(|issuer| issuer.as_str()
                        == Some(&self.connection.report.company_identifier))),
            "protected resource does not accept the selected company's issuer"
        );
        if metadata["dpop_bound_access_tokens_required"] == true {
            ensure!(
                self.tokens
                    .as_ref()
                    .is_some_and(|tokens| tokens.metadata().token_type == TokenType::DPoP),
                "protected resource requires a DPoP-bound access token"
            );
        }
        Ok(
            (required || service != self.connection.report.company_origin)
                .then(|| service.to_owned()),
        )
    }

    fn check_http_observation(&self, observation: &HttpObservation) -> Result<()> {
        let secrets = self.http.secrets.lock();
        ensure!(
            !secrets.contains_bytes(&observation.body)
                && !observation
                    .headers
                    .iter()
                    .any(|(_, value)| secrets.contains_bytes(value.as_bytes())),
            "merchant observation reflected a local credential; response withheld"
        );
        drop(secrets);
        // JSON escaping can hide a credential from a raw byte comparison.
        if let Ok(mut value) = serde_json::from_slice::<Value>(&observation.body) {
            let result = self.redact_observation(&mut value);
            clear_json(&mut value);
            result?;
        }
        Ok(())
    }
}

struct CompanyAuthorizer {
    signer: RequestSigner,
    tokens: Option<Arc<AccountTokens>>,
    resource: Option<String>,
    headers: BTreeMap<String, String>,
    profile_url: String,
    http: Arc<HttpState>,
    timeout: Duration,
    guard: Option<Guard>,
    cancellation: Option<CancellationToken>,
}

impl HttpRequestAuthorizer for CompanyAuthorizer {
    fn cancellation_token(&self) -> Option<CancellationToken> {
        self.cancellation.clone()
    }
    fn prepare_client<'a>(
        &'a self,
        url: &'a Url,
    ) -> Pin<Box<dyn Future<Output = Result<Option<reqwest::Client>>> + Send + 'a>> {
        Box::pin(async move {
            check_guard(&self.guard)?;
            self.http.check_backoff(url)?;
            Ok(Some(
                pinned_client(url, &DiscoveryOptions::default(), self.timeout).await?,
            ))
        })
    }

    fn authorize<'a>(
        &'a self,
        request: &'a mut Request,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            check_guard(&self.guard)?;
            self.http.check_backoff(request.url())?;
            self.signer.validate_request(request)?;
            for (name, value) in &self.headers {
                request.headers_mut().insert(
                    HeaderName::from_bytes(name.as_bytes())?,
                    HeaderValue::from_str(value)?,
                );
            }
            if let Some(bytes) = request.body().and_then(reqwest::Body::as_bytes)
                && let Ok(rpc) = serde_json::from_slice::<Value>(bytes)
                && let Some(meta) = rpc.pointer("/params/arguments/meta")
            {
                if let Some(profile) = meta.pointer("/ucp-agent/profile").and_then(Value::as_str) {
                    ensure!(
                        profile == self.profile_url,
                        "MCP and HTTP UCP platform profiles differ"
                    );
                    let expected = format!("profile=\"{}\"", self.profile_url);
                    ensure!(
                        request
                            .headers()
                            .get("ucp-agent")
                            .and_then(|value| value.to_str().ok())
                            == Some(expected.as_str()),
                        "UCP MCP request lacks its exact platform header"
                    );
                    for name in ["idempotency-key", "request-id"] {
                        if let Some(key) = meta.get(name).and_then(Value::as_str) {
                            request
                                .headers_mut()
                                .insert(HeaderName::from_static(name), HeaderValue::from_str(key)?);
                        }
                    }
                }
            }
            let nonce = self
                .http
                .nonce(NonceContext::ResourceServer, request.url().as_str());
            if let Some(tokens) = &self.tokens {
                if request.headers().contains_key("ucp-agent") {
                    ensure!(
                        tokens.metadata().token_type == TokenType::Bearer,
                        "UCP requires a Bearer token"
                    );
                }
                tokens.authorize(request, self.resource.as_deref(), nonce.as_deref())?;
            }
            let token = self
                .tokens
                .as_ref()
                .map(|tokens| tokens.access_token_for(self.resource.as_deref()))
                .transpose()?;
            self.signer.authorize(request, token)?;
            let mut secrets = self.http.secrets.lock();
            for name in ["poppy-session", "dpop", "signature", "signature-input"] {
                if let Some(value) = request.headers().get(name) {
                    secrets.remember(
                        value
                            .to_str()
                            .context("invalid generated credential header")?,
                    )?;
                }
            }
            Ok(())
        })
    }

    fn observe_response(&self, url: &Url, status: u16, headers: &HeaderMap) -> Result<()> {
        self.http
            .observe(url, status, headers, Some(NonceContext::ResourceServer))
    }
    fn max_json_bytes(&self) -> usize {
        512 * 1024
    }
}

pub(super) fn validate_public_header(name: &str) -> Result<()> {
    let name = name.to_ascii_lowercase();
    ensure!(
        !matches!(
            name.as_str(),
            "authorization"
                | "dpop"
                | "poppy-session"
                | "poppy-version"
                | "poppy-extensions"
                | "signature"
                | "signature-input"
                | "signature-agent"
                | "cookie"
                | "host"
                | "content-length"
                | "content-digest"
        ),
        "credentials, identity and transport headers are generated from the actual node-local binding"
    );
    HeaderName::from_bytes(name.as_bytes())?;
    Ok(())
}

fn check_guard(guard: &Option<Guard>) -> Result<()> {
    guard
        .as_ref()
        .context("Poppy network request has no durable dispatch owner")?()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstrumentRecord {
    schema_version: u32,
    company: String,
    user_id: String,
    client_id: String,
    account_binding: Option<String>,
    checkout_id: String,
    terms_sha256: String,
    instrument: Value,
}

impl Drop for InstrumentRecord {
    fn drop(&mut self) {
        clear_json(&mut self.instrument);
    }
}

/// Shared by every request instance in this operation; the held Vault lock
/// excludes other processes until all MCP authorizers have been dropped.
struct HttpState {
    vault: Arc<Vault>,
    reference: String,
    nonces: Mutex<BTreeMap<String, String>>,
    backoff: Mutex<BackoffDocument>,
    secrets: Mutex<SecretRegistry>,
}

#[derive(Clone, Copy)]
enum NonceContext {
    AuthorizationServer,
    ResourceServer,
}

impl NonceContext {
    fn key(self, url: &str) -> String {
        let prefix = match self {
            Self::AuthorizationServer => "as",
            Self::ResourceServer => "rs",
        };
        format!("{prefix}:{url}")
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BackoffDocument {
    schema_version: u32,
    client_id: String,
    company: String,
    user_id: String,
    endpoints: BTreeMap<String, RetryDeadline>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryDeadline {
    observed_at: u64,
    not_before: u64,
    source: RetrySource,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RetrySource {
    ServerDelay,
    ServerHttpDate,
    LocalDefault,
}

impl HttpState {
    fn load(vault: Arc<Vault>, operator: &Operator, connection: &Connection) -> Result<Self> {
        let reference = connection.credential_ref();
        let mut nonces: BTreeMap<String, String> =
            match vault.read(&format!("{reference}-nonces"))? {
                Some(bytes) => serde_json::from_slice(&bytes)?,
                None => BTreeMap::new(),
            };
        ensure!(
            nonces.len() <= 128
                && nonces
                    .values()
                    .all(|value| !value.is_empty() && value.len() <= 4096),
            "invalid persisted DPoP nonce observations"
        );
        // An earlier development draft did not distinguish AS and RS nonce
        // contexts. Such observations cannot safely be reused; a fresh server
        // challenge supplies the correct nonce without replacing any grant.
        nonces.retain(|key, _| key.starts_with("as:") || key.starts_with("rs:"));
        let mut backoff: BackoffDocument = match vault.read(&format!("{reference}-backoff"))? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => BackoffDocument {
                schema_version: 1,
                client_id: operator.config.client_id.clone(),
                company: connection.report.company_identifier.clone(),
                user_id: connection.user_id.clone(),
                endpoints: BTreeMap::new(),
            },
        };
        ensure!(
            backoff.schema_version == 1
                && backoff.client_id == operator.config.client_id
                && backoff.company == connection.report.company_identifier
                && backoff.user_id == connection.user_id,
            "rate-limit observations belong to a different local account binding"
        );
        ensure!(
            backoff.endpoints.len() <= 128,
            "too many persisted rate-limit destinations"
        );
        let now = unix_seconds()?;
        backoff
            .endpoints
            .retain(|_, deadline| deadline.not_before > now);
        Ok(Self {
            vault,
            reference,
            nonces: Mutex::new(nonces),
            backoff: Mutex::new(backoff),
            secrets: Mutex::new(SecretRegistry::default()),
        })
    }

    fn check_backoff(&self, url: &Url) -> Result<()> {
        if let Some(deadline) = self.backoff.lock().endpoints.get(url.as_str()) {
            let now = unix_seconds()?;
            ensure!(
                now >= deadline.not_before,
                "endpoint is rate limited for another {} seconds; no request was sent",
                deadline.not_before.saturating_sub(now)
            );
        }
        Ok(())
    }

    fn nonce(&self, context: NonceContext, url: &str) -> Option<String> {
        self.nonces.lock().get(&context.key(url)).cloned()
    }

    fn observe(
        &self,
        url: &Url,
        status: u16,
        headers: &HeaderMap,
        nonce_context: Option<NonceContext>,
    ) -> Result<()> {
        if let Some(context) = nonce_context
            && let Some(value) = headers.get("dpop-nonce")
        {
            let nonce = value.to_str().context("invalid DPoP nonce header")?;
            ensure!(
                !nonce.is_empty() && nonce.len() <= 4096,
                "invalid DPoP nonce size"
            );
            let mut values = self.nonces.lock();
            let key = context.key(url.as_str());
            ensure!(
                values.len() < 128 || values.contains_key(&key),
                "too many DPoP nonce destinations"
            );
            values.insert(key, nonce.into());
            self.vault.write(
                &format!("{}-nonces", self.reference),
                &serde_json::to_vec(&*values)?,
            )?;
        }
        if status == 429 {
            let now = unix_seconds()?;
            let mut deadline = retry_deadline(headers, now);
            let mut state = self.backoff.lock();
            state.endpoints.retain(|_, value| value.not_before > now);
            if let Some(previous) = state.endpoints.get(url.as_str())
                && previous.not_before > deadline.not_before
            {
                deadline = previous.clone();
            }
            ensure!(
                state.endpoints.len() < 128 || state.endpoints.contains_key(url.as_str()),
                "too many rate-limited destinations"
            );
            state.endpoints.insert(url.to_string(), deadline);
            self.vault.write(
                &format!("{}-backoff", self.reference),
                &serde_json::to_vec(&*state)?,
            )?;
        }
        Ok(())
    }
}

fn retry_deadline(headers: &HeaderMap, now: u64) -> RetryDeadline {
    if let Some(value) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
    {
        if !value.is_empty()
            && value.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(delay) = value.parse::<u64>()
        {
            return RetryDeadline {
                observed_at: now,
                not_before: now.saturating_add(delay),
                source: RetrySource::ServerDelay,
            };
        }
        if let Some(time) = http_date(value, now) {
            return RetryDeadline {
                observed_at: now,
                not_before: time.max(now),
                source: RetrySource::ServerHttpDate,
            };
        }
    }
    // A malformed/absent Retry-After is an observed 429, not permission to
    // retry immediately. Record this finite client choice as a local default,
    // never as a server-supplied date; a later operation remains explicit.
    RetryDeadline {
        observed_at: now,
        not_before: now.saturating_add(60),
        source: RetrySource::LocalDefault,
    }
}

fn http_date(value: &str, now: u64) -> Option<u64> {
    use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
    if let Ok(date) = DateTime::parse_from_rfc2822(value) {
        return u64::try_from(date.timestamp().max(0)).ok();
    }
    // RFC9110 recipients also accept the two obsolete HTTP-date formats.
    if let Ok(mut date) = NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT") {
        let current = DateTime::<Utc>::from_timestamp(i64::try_from(now).ok()?, 0)?;
        if date.year() > current.year() + 50 {
            date = date.with_year(date.year() - 100)?;
        }
        return u64::try_from(date.and_utc().timestamp().max(0)).ok();
    }
    NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
        .ok()
        .and_then(|date| u64::try_from(date.and_utc().timestamp().max(0)).ok())
}

fn unix_seconds() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("clock precedes Unix epoch")?
        .as_secs())
}

#[derive(Default)]
struct SecretRegistry {
    values: Vec<Zeroizing<String>>,
    bytes: usize,
}

impl SecretRegistry {
    fn remember(&mut self, value: &str) -> Result<()> {
        if value.is_empty() || self.values.iter().any(|known| known.as_str() == value) {
            return Ok(());
        }
        ensure!(
            self.values.len() < 4096 && self.bytes.saturating_add(value.len()) <= 4 * 1024 * 1024,
            "credential echo protection reached its per-operation memory bound"
        );
        self.bytes += value.len();
        self.values.push(Zeroizing::new(value.to_owned()));
        Ok(())
    }

    fn remember_value(&mut self, value: &Value) -> Result<()> {
        match value {
            Value::String(value) => self.remember(value)?,
            Value::Number(value) => self.remember(&value.to_string())?,
            Value::Array(values) => {
                for value in values {
                    self.remember_value(value)?;
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    // UCP's credential type is a public discriminator. The rest
                    // of the provider-defined credential payload stays private.
                    if key != "type" {
                        self.remember_value(value)?;
                    }
                }
            }
            Value::Null | Value::Bool(_) => {}
        }
        Ok(())
    }

    fn contains_bytes(&self, value: &[u8]) -> bool {
        self.values.iter().any(|secret| {
            value
                .windows(secret.len())
                .any(|part| part == secret.as_bytes())
        })
    }

    fn contains_value(&self, value: &Value) -> bool {
        match value {
            Value::String(value) => {
                self.contains_bytes(value.as_bytes())
                    || serde_json::from_str::<Value>(value)
                        .ok()
                        .is_some_and(|nested| {
                            // MCP text results can contain another JSON document.
                            let mut nested = nested;
                            let contains = self.contains_value(&nested);
                            clear_json(&mut nested);
                            contains
                        })
            }
            Value::Number(value) => self.contains_bytes(value.to_string().as_bytes()),
            Value::Array(values) => values.iter().any(|value| self.contains_value(value)),
            Value::Object(values) => values.iter().any(|(key, value)| {
                self.contains_bytes(key.as_bytes()) || self.contains_value(value)
            }),
            Value::Null | Value::Bool(_) => false,
        }
    }
}

pub(super) fn clear_json(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(clear_json),
        Value::Object(values) => {
            for (mut key, mut value) in std::mem::take(values) {
                key.zeroize();
                clear_json(&mut value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    *value = Value::Null;
}

async fn read_response(mut response: reqwest::Response, limit: usize) -> Result<HttpObservation> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= limit as u64),
        "HTTP response exceeds bound"
    );
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| error.without_url())?
    {
        ensure!(
            body.len().saturating_add(chunk.len()) <= limit,
            "HTTP response exceeds bound"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(HttpObservation {
        status,
        headers,
        body: std::mem::take(&mut *body),
    })
}
