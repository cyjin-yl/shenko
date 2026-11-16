use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Request;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::{
    discovery::DiscoveryReport,
    session::{OperatorSigner, SessionIdentity},
};

use super::{
    DpopKey, dpop::StoredDpopKey, form_request, https_url, now, random_secret, response_json,
    scopes, secret, sensitive_header, string,
};

const CLIENT_ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// An issuer and endpoints resolved through Poppy's reciprocal discovery.
/// These are public server declarations, not authority to access an account.
#[derive(Clone, Debug)]
pub struct OAuthServer {
    pub(super) issuer: String,
    pub(super) company_origin: String,
    pub(super) company_name: String,
    pub(super) token_endpoint: Url,
    pub(super) revocation_endpoint: Url,
    pub(super) authorization_endpoint: Option<Url>,
    pub(super) device_endpoint: Option<Url>,
    pub(super) par_endpoint: Option<Url>,
    pub(super) require_par: bool,
    pub(super) direct_supported: bool,
    pub(super) device_supported: bool,
    pub(super) sign_in_disabled: bool,
    pub(super) use_dpop: bool,
    dpop_advertised: bool,
    pub(super) browser_join_supported: bool,
}

impl OAuthServer {
    pub fn from_discovery(report: &DiscoveryReport) -> Result<Self> {
        let issuer = report
            .issuer
            .as_deref()
            .context("company has no OAuth issuer")?;
        ensure!(
            issuer == report.company_identifier,
            "OAuth company binding differs from issuer"
        );
        let document = &report
            .oauth
            .authorization_server
            .as_ref()
            .context("verified authorization metadata is absent")?
            .metadata;
        ensure!(
            document.get("issuer").and_then(Value::as_str) == Some(issuer),
            "OAuth metadata issuer does not match company"
        );
        let company = https_url(&report.company_origin)?;
        ensure!(
            document
                .get("protected_resources")
                .and_then(Value::as_array)
                .is_some_and(|resources| resources.iter().any(|r| r
                    .as_str()
                    .is_some_and(|s| https_url(s).is_ok_and(|u| u == company)))),
            "OAuth issuer does not attest this company origin"
        );
        let grants = document
            .get("grant_types_supported")
            .and_then(Value::as_array)
            .context("OAuth metadata has no grant types")?;
        let has_grant = |name: &str| grants.iter().any(|v| v.as_str() == Some(name));
        ensure!(
            has_grant("refresh_token"),
            "Poppy issuer must support refresh_token"
        );
        let endpoint = |name: &str| -> Result<Option<Url>> {
            document
                .get(name)
                .map(|v| {
                    https_url(
                        v.as_str()
                            .with_context(|| format!("invalid OAuth {name}"))?,
                    )
                })
                .transpose()
        };
        let token_endpoint =
            endpoint("token_endpoint")?.context("OAuth token endpoint is absent")?;
        let revocation_endpoint =
            endpoint("revocation_endpoint")?.context("OAuth revocation endpoint is absent")?;
        let authorization_endpoint = endpoint("authorization_endpoint")?;
        let device_endpoint = endpoint("device_authorization_endpoint")?;
        let par_endpoint = endpoint("pushed_authorization_request_endpoint")?;
        let require_par = match document.get("require_pushed_authorization_requests") {
            None => false,
            Some(value) => value
                .as_bool()
                .context("invalid require_pushed_authorization_requests")?,
        };
        let direct_supported = has_grant("authorization_code");
        let device_supported = has_grant("urn:ietf:params:oauth:grant-type:device_code");
        ensure!(
            !direct_supported || authorization_endpoint.is_some(),
            "direct sign-in endpoint is absent"
        );
        ensure!(
            !device_supported || device_endpoint.is_some(),
            "device sign-in endpoint is absent"
        );
        ensure!(
            !require_par || par_endpoint.is_some(),
            "required PAR endpoint is absent"
        );
        let use_dpop = match document.get("dpop_signing_alg_values_supported") {
            None => false,
            Some(value) => {
                let algorithms = value
                    .as_array()
                    .context("invalid DPoP algorithm metadata")?;
                ensure!(
                    algorithms.iter().all(Value::is_string),
                    "invalid DPoP algorithm metadata"
                );
                algorithms.iter().any(|v| v.as_str() == Some("ES256"))
            }
        };
        Ok(Self {
            issuer: issuer.into(),
            company_origin: report.company_origin.clone(),
            company_name: report
                .metadata
                .pointer("/organization/name")
                .and_then(Value::as_str)
                .context("company name is absent")?
                .into(),
            token_endpoint,
            revocation_endpoint,
            authorization_endpoint,
            device_endpoint,
            par_endpoint,
            require_par,
            direct_supported,
            device_supported,
            use_dpop,
            dpop_advertised: document.get("dpop_signing_alg_values_supported").is_some(),
            sign_in_disabled: report
                .metadata
                .pointer("/auth/disabled")
                .and_then(Value::as_bool)
                == Some(true),
            browser_join_supported: report
                .capabilities
                .web
                .as_ref()
                .and_then(|v| v.get("browser_session_endpoint"))
                .is_some(),
        })
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }
    pub fn token_endpoint(&self) -> &Url {
        &self.token_endpoint
    }
    pub fn uses_dpop(&self) -> bool {
        self.use_dpop
    }
    pub fn has_par(&self) -> bool {
        self.par_endpoint.is_some()
    }
    pub fn par_endpoint(&self) -> Option<&Url> {
        self.par_endpoint.as_ref()
    }

    pub(super) fn check_sign_in(&self, requested_scopes: &[String]) -> Result<()> {
        ensure!(
            !self.sign_in_disabled,
            "company currently disables new sign-in"
        );
        ensure!(
            !self.dpop_advertised || self.use_dpop,
            "issuer advertises no implemented DPoP algorithm (ES256)"
        );
        scopes(requested_scopes)?;
        ensure!(
            self.browser_join_supported
                || !requested_scopes
                    .iter()
                    .any(|s| s == "poppy:browser-sign-in"),
            "browser sign-in scope requires a declared browser join endpoint"
        );
        Ok(())
    }

    pub(super) fn request(
        &self,
        endpoint: &Url,
        fields: &mut Vec<(String, String)>,
        operator: &OperatorSigner,
        dpop: Option<&DpopKey>,
        nonce: Option<&str>,
    ) -> Result<OAuthRequest> {
        fields.push(("client_id".into(), operator.client_id().into()));
        fields.push(("client_assertion_type".into(), CLIENT_ASSERTION_TYPE.into()));
        // The issuer, never the endpoint, is the assertion audience.
        fields.push((
            "client_assertion".into(),
            operator.client_assertion(&self.issuer)?,
        ));
        let mut request = form_request(endpoint, fields)?;
        if let Some(key) = dpop {
            let proof = key.proof("POST", endpoint, None, nonce)?;
            sensitive_header(&mut request, "dpop", &proof)?;
        }
        Ok(OAuthRequest { request })
    }
}

/// Secret-bearing request. Never Debug/Serialize or freeze its body in a
/// shared artifact; persist an opaque node-local credential reference instead.
pub struct OAuthRequest {
    request: Request,
}

impl OAuthRequest {
    pub fn endpoint(&self) -> &Url {
        self.request.url()
    }

    /// The host must use an HTTPS sender with DNS pinning, no redirect, no
    /// automatic replay, bounded response reading and its durable send fence.
    pub fn into_request(self) -> Request {
        self.request
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DirectSignIn {
    pub redirect_uri: String,
    pub registered_redirect_uris: Vec<String>,
    pub scopes: Vec<String>,
    /// True only when the callback is received on the initiating device.
    pub redirect_received_on_initiating_device: bool,
}

/// Node-local PKCE/state context. A pending or failed sign-in never alters an
/// existing account grant; swap credentials only after a successful response.
pub struct AuthorizationAttempt {
    pub(super) server: OAuthServer,
    pub(super) client_id: String,
    pub(super) user_id: String,
    redirect_uri: String,
    requested_scopes: Vec<String>,
    state: String,
    verifier: String,
    prompt_consent: bool,
    pub(super) dpop: Option<DpopKey>,
    par: Option<(String, u64)>,
}

#[derive(Serialize, Deserialize)]
struct StoredAuthorization {
    version: u8,
    issuer: String,
    client_id: String,
    user_id: String,
    redirect_uri: String,
    requested_scopes: Vec<String>,
    state: String,
    verifier: String,
    prompt_consent: bool,
    dpop: Option<StoredDpopKey>,
    par: Option<(String, u64)>,
}

impl AuthorizationAttempt {
    pub fn new(
        server: OAuthServer,
        operator: &OperatorSigner,
        identity: &SessionIdentity,
        config: DirectSignIn,
    ) -> Result<Self> {
        server.check_sign_in(&config.scopes)?;
        ensure!(
            server.direct_supported,
            "company does not offer direct sign-in"
        );
        ensure!(
            identity.company_identifier() == server.issuer,
            "Session belongs to another company"
        );
        validate_redirect(
            operator.client_id(),
            &config.redirect_uri,
            &config.registered_redirect_uris,
        )?;
        let dpop = if server.use_dpop {
            Some(DpopKey::generate(&server.issuer, identity.user_id())?)
        } else {
            None
        };
        Ok(Self {
            server,
            client_id: operator.client_id().into(),
            user_id: identity.user_id().into(),
            redirect_uri: config.redirect_uri,
            requested_scopes: config.scopes,
            state: random_secret()?,
            verifier: random_secret()?,
            prompt_consent: !config.redirect_received_on_initiating_device,
            dpop,
            par: None,
        })
    }

    fn authorization_parameters(&self) -> Result<Vec<(String, String)>> {
        let mut fields = vec![
            ("response_type".into(), "code".into()),
            ("client_id".into(), self.client_id.clone()),
            ("redirect_uri".into(), self.redirect_uri.clone()),
            ("scope".into(), scopes(&self.requested_scopes)?),
            ("state".into(), self.state.clone()),
            (
                "code_challenge".into(),
                URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes())),
            ),
            ("code_challenge_method".into(), "S256".into()),
        ];
        if self.prompt_consent {
            fields.push(("prompt".into(), "consent".into()));
        }
        if let Some(key) = &self.dpop {
            fields.push(("dpop_jkt".into(), key.thumbprint().into()));
        }
        // Resource is intentionally absent: the grant spans the Company's
        // services, while each later token request names at most one service.
        Ok(fields)
    }

    /// Use PAR whenever advertised, including mandatory PAR. This method
    /// returns no usable browser URL until the pushed request was accepted.
    pub fn authorization_url(&self) -> Result<Url> {
        let endpoint = self
            .server
            .authorization_endpoint
            .as_ref()
            .context("no authorization endpoint")?;
        let fields = if self.server.has_par() || self.server.require_par {
            let (request_uri, expires_at) = self
                .par
                .as_ref()
                .context("push authorization request before opening browser")?;
            ensure!(now()? < *expires_at, "pushed authorization request expired");
            vec![
                ("client_id".into(), self.client_id.clone()),
                ("request_uri".into(), request_uri.clone()),
            ]
        } else {
            self.authorization_parameters()?
        };
        with_query(endpoint, &fields)
    }

    pub fn par_request(
        &self,
        operator: &OperatorSigner,
        nonce: Option<&str>,
    ) -> Result<OAuthRequest> {
        self.check_operator(operator)?;
        let endpoint = self
            .server
            .par_endpoint
            .as_ref()
            .context("company has no PAR endpoint")?;
        let mut fields = self.authorization_parameters()?;
        fields.retain(|(name, _)| name != "client_id");
        self.server
            .request(endpoint, &mut fields, operator, self.dpop.as_ref(), nonce)
    }

    pub fn accept_par_response(&mut self, status: u16, body: &[u8]) -> Result<()> {
        ensure!(status == 201, "PAR response was not HTTP 201");
        let value = response_json(body)?;
        let request_uri = string(&value, "request_uri")?;
        Url::parse(&request_uri).map_err(|_| anyhow!("invalid PAR request_uri"))?;
        let lifetime = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .context("PAR lifetime is absent")?;
        ensure!(lifetime > 0, "PAR request is already expired");
        self.par = Some((
            request_uri,
            now()?
                .checked_add(lifetime)
                .context("PAR expiry overflow")?,
        ));
        Ok(())
    }

    /// Validate state, exact issuer and callback route before exchanging the
    /// code. The host records the send boundary, so a lost response does not
    /// silently replay a possibly consumed code. A verified `use_dpop_nonce`
    /// rejection can be retried explicitly with a fresh assertion and proof.
    pub fn exchange_request(
        &self,
        operator: &OperatorSigner,
        callback_url: &str,
        resource: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<OAuthRequest> {
        self.check_operator(operator)?;
        let code = self.callback_code(callback_url)?;
        let mut fields = vec![
            ("grant_type".into(), "authorization_code".into()),
            ("code".into(), code),
            ("redirect_uri".into(), self.redirect_uri.clone()),
            ("code_verifier".into(), self.verifier.clone()),
        ];
        add_resource(&mut fields, resource)?;
        self.server.request(
            &self.server.token_endpoint,
            &mut fields,
            operator,
            self.dpop.as_ref(),
            nonce,
        )
    }

    fn callback_code(&self, callback_url: &str) -> Result<String> {
        let callback =
            Url::parse(callback_url).map_err(|_| anyhow!("invalid OAuth callback URL"))?;
        let redirect = Url::parse(&self.redirect_uri).context("invalid stored callback route")?;
        ensure!(
            callback.scheme() == redirect.scheme()
                && callback.host() == redirect.host()
                && callback.port_or_known_default() == redirect.port_or_known_default()
                && callback.path() == redirect.path()
                && callback.username().is_empty()
                && callback.password().is_none()
                && callback.fragment().is_none(),
            "OAuth callback route does not match the initiating request"
        );
        let mut parameters = BTreeMap::new();
        for (name, value) in callback.query_pairs() {
            ensure!(
                parameters
                    .insert(name.into_owned(), value.into_owned())
                    .is_none(),
                "duplicate OAuth callback parameter"
            );
        }
        for (name, value) in redirect.query_pairs() {
            ensure!(
                parameters.get(name.as_ref()).map(String::as_str) == Some(value.as_ref()),
                "callback changed registered redirect query"
            );
        }
        ensure!(
            parameters.get("iss") == Some(&self.server.issuer),
            "OAuth callback issuer mismatch"
        );
        let state = parameters
            .get("state")
            .context("OAuth callback state is absent")?;
        ensure!(
            constant_time_equal(state.as_bytes(), self.state.as_bytes()),
            "OAuth callback state mismatch"
        );
        if let Some(error) = parameters.get("error") {
            validate_error(error)?;
            // Even a syntactically valid extension error is remote input and
            // can contain a credential. Never project it out of this private
            // callback parser into a durable task error.
            bail!("OAuth authorization was declined");
        }
        let code = parameters
            .remove("code")
            .context("OAuth callback code is absent")?;
        secret(&code, "authorization code")?;
        Ok(code)
    }

    pub fn accept_token_response(
        &self,
        status: u16,
        body: &[u8],
        resource: Option<&str>,
    ) -> Result<AccountTokens> {
        ensure!(status == 200, "token exchange was not HTTP 200");
        AccountTokens::new_grant(
            &self.server,
            &self.client_id,
            &self.user_id,
            &self.requested_scopes,
            self.dpop.as_ref(),
            body,
            resource,
        )
    }

    fn check_operator(&self, operator: &OperatorSigner) -> Result<()> {
        ensure!(
            operator.client_id() == self.client_id,
            "OAuth attempt belongs to another operator"
        );
        Ok(())
    }

    pub fn export_node_local_secret(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&StoredAuthorization {
            version: 1,
            issuer: self.server.issuer.clone(),
            client_id: self.client_id.clone(),
            user_id: self.user_id.clone(),
            redirect_uri: self.redirect_uri.clone(),
            requested_scopes: self.requested_scopes.clone(),
            state: self.state.clone(),
            verifier: self.verifier.clone(),
            prompt_consent: self.prompt_consent,
            dpop: self.dpop.as_ref().map(DpopKey::stored),
            par: self.par.clone(),
        })?)
    }

    pub fn restore_node_local_secret(
        bytes: &[u8],
        server: OAuthServer,
        operator: &OperatorSigner,
        identity: &SessionIdentity,
        registered_redirect_uris: &[String],
    ) -> Result<Self> {
        let stored: StoredAuthorization = serde_json::from_value(response_json(bytes)?)
            .map_err(|_| anyhow!("invalid stored OAuth attempt"))?;
        ensure!(
            stored.version == 1
                && stored.issuer == server.issuer
                && stored.client_id == operator.client_id()
                && stored.user_id == identity.user_id()
                && identity.company_identifier() == server.issuer,
            "stored OAuth attempt binding mismatch"
        );
        validate_redirect(
            operator.client_id(),
            &stored.redirect_uri,
            registered_redirect_uris,
        )?;
        scopes(&stored.requested_scopes)?;
        secret(&stored.state, "OAuth state")?;
        ensure!(
            (43..=128).contains(&stored.verifier.len())
                && stored
                    .verifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
            "invalid stored PKCE verifier"
        );
        let dpop = stored.dpop.map(DpopKey::restore).transpose()?;
        if let Some(key) = &dpop {
            key.check_binding(&server.issuer, identity.user_id())?;
        }
        Ok(Self {
            server,
            client_id: stored.client_id,
            user_id: stored.user_id,
            redirect_uri: stored.redirect_uri,
            requested_scopes: stored.requested_scopes,
            state: stored.state,
            verifier: stored.verifier,
            prompt_consent: stored.prompt_consent,
            dpop,
            par: stored.par,
        })
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum TokenType {
    Bearer,
    DPoP,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AccountMetadata {
    pub issuer: String,
    pub client_id: String,
    pub resource: Option<String>,
    pub token_type: TokenType,
    pub access_scopes: Vec<String>,
    pub grant_scopes: Vec<String>,
    pub expires_at_unix_secs: Option<u64>,
    pub dpop_bound_refresh: bool,
}

pub struct AccountTokens {
    issuer: String,
    client_id: String,
    user_id: String,
    access_token: String,
    refresh_token: String,
    token_type: TokenType,
    resource: Option<String>,
    grant_scopes: Vec<String>,
    access_scopes: Vec<String>,
    expires_at: Option<u64>,
    dpop: Option<DpopKey>,
}

#[derive(Serialize, Deserialize)]
struct StoredAccount {
    version: u8,
    issuer: String,
    client_id: String,
    user_id: String,
    access_token: String,
    refresh_token: String,
    token_type: TokenType,
    resource: Option<String>,
    grant_scopes: Vec<String>,
    access_scopes: Vec<String>,
    expires_at: Option<u64>,
    dpop: Option<StoredDpopKey>,
}

struct ParsedToken {
    access_token: String,
    refresh_token: Option<String>,
    token_type: TokenType,
    access_scopes: Vec<String>,
    expires_at: Option<u64>,
}

impl AccountTokens {
    pub(super) fn new_grant(
        server: &OAuthServer,
        client_id: &str,
        user_id: &str,
        requested_scopes: &[String],
        dpop: Option<&DpopKey>,
        body: &[u8],
        resource: Option<&str>,
    ) -> Result<Self> {
        let parsed = parse_token(body, requested_scopes)?;
        ensure!(
            parsed.token_type != TokenType::DPoP || dpop.is_some(),
            "DPoP token returned without a bound key"
        );
        if let Some(key) = dpop {
            key.check_binding(&server.issuer, user_id)?;
        }
        validate_resource(resource)?;
        Ok(Self {
            issuer: server.issuer.clone(),
            client_id: client_id.into(),
            user_id: user_id.into(),
            access_token: parsed.access_token,
            refresh_token: parsed
                .refresh_token
                .context("Poppy sign-in returned no refresh token")?,
            token_type: parsed.token_type,
            resource: resource.map(str::to_owned),
            grant_scopes: parsed.access_scopes.clone(),
            access_scopes: parsed.access_scopes,
            expires_at: parsed.expires_at,
            dpop: dpop.map(DpopKey::copy_for_same_grant).transpose()?,
        })
    }

    pub fn metadata(&self) -> AccountMetadata {
        AccountMetadata {
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            resource: self.resource.clone(),
            token_type: self.token_type,
            access_scopes: self.access_scopes.clone(),
            grant_scopes: self.grant_scopes.clone(),
            expires_at_unix_secs: self.expires_at,
            dpop_bound_refresh: self.dpop.is_some(),
        }
    }

    pub fn check_binding(
        &self,
        client_id: &str,
        company_identifier: &str,
        user_id: &str,
    ) -> Result<()> {
        ensure!(
            self.client_id == client_id
                && self.issuer == company_identifier
                && self.user_id == user_id,
            "account credentials belong to another operator, company or pairwise user"
        );
        Ok(())
    }

    /// Read a token only after validating the actual service against company
    /// discovery. `resource` is the verified service metadata's identifier;
    /// None means the company's default token, not an arbitrary destination.
    pub fn access_token_for(&self, resource: Option<&str>) -> Result<&str> {
        ensure!(
            self.resource.as_deref() == resource,
            "access token belongs to another resource"
        );
        let time = now()?;
        ensure!(
            self.expires_at.is_none_or(|expiry| time < expiry),
            "access token expired"
        );
        Ok(&self.access_token)
    }

    /// Call only after destination validation, and before adding the Poppy
    /// Session and HTTP Message Signature. DPoP proofs are never cached.
    pub fn authorize(
        &self,
        request: &mut Request,
        resource: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<()> {
        https_url(request.url().as_str())?;
        let token = self.access_token_for(resource)?;
        ensure!(
            !request.headers().contains_key("authorization")
                && !request.headers().contains_key("dpop"),
            "request already contains authorization credentials"
        );
        let scheme = match self.token_type {
            TokenType::Bearer => "Bearer",
            TokenType::DPoP => "DPoP",
        };
        sensitive_header(request, "authorization", &format!("{scheme} {token}"))?;
        if self.token_type == TokenType::DPoP {
            let key = self.dpop.as_ref().context("DPoP token lost its key")?;
            let proof = key.proof(request.method().as_str(), request.url(), Some(token), nonce)?;
            sensitive_header(request, "dpop", &proof)?;
        }
        Ok(())
    }

    pub fn refresh_request(
        &self,
        server: &OAuthServer,
        operator: &OperatorSigner,
        requested_scopes: Option<&[String]>,
        resource: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<OAuthRequest> {
        self.check_server(server, operator)?;
        let mut fields = vec![
            ("grant_type".into(), "refresh_token".into()),
            ("refresh_token".into(), self.refresh_token.clone()),
        ];
        if let Some(requested) = requested_scopes {
            ensure!(
                requested.iter().all(|s| self.grant_scopes.contains(s)),
                "refresh requests scopes outside the original grant"
            );
            fields.push(("scope".into(), scopes(requested)?));
        }
        add_resource(&mut fields, resource)?;
        // auth.disabled applies to new sign-in, not previously issued grants.
        // Metadata changes never silently discard the existing refresh key.
        server.request(
            &server.token_endpoint,
            &mut fields,
            operator,
            self.dpop.as_ref(),
            nonce,
        )
    }

    /// Parse completely before replacing any live credentials. Save the result
    /// atomically with the original DPoP key before subsequent use. If delivery
    /// of a rotated refresh token is lost, the original grant remains unknown;
    /// this helper does not retry it or substitute a new key.
    pub fn accept_refresh_response(
        &mut self,
        status: u16,
        body: &[u8],
        requested_scopes: Option<&[String]>,
        resource: Option<&str>,
    ) -> Result<()> {
        ensure!(status == 200, "refresh response was not HTTP 200");
        let requested = requested_scopes.unwrap_or(&self.grant_scopes);
        ensure!(
            requested.iter().all(|s| self.grant_scopes.contains(s)),
            "refresh scope binding mismatch"
        );
        validate_resource(resource)?;
        let parsed = parse_token(body, requested)?;
        ensure!(
            parsed.token_type != TokenType::DPoP || self.dpop.is_some(),
            "DPoP refresh response has no original key"
        );
        self.access_token = parsed.access_token;
        if let Some(refresh) = parsed.refresh_token {
            self.refresh_token = refresh;
        }
        self.token_type = parsed.token_type;
        self.access_scopes = parsed.access_scopes;
        self.expires_at = parsed.expires_at;
        self.resource = resource.map(str::to_owned);
        Ok(())
    }

    pub fn revocation_request(
        &self,
        server: &OAuthServer,
        operator: &OperatorSigner,
    ) -> Result<OAuthRequest> {
        self.check_server(server, operator)?;
        let mut fields = vec![
            ("token".into(), self.refresh_token.clone()),
            ("token_type_hint".into(), "refresh_token".into()),
        ];
        server.request(
            &server.revocation_endpoint,
            &mut fields,
            operator,
            None,
            None,
        )
    }

    fn check_server(&self, server: &OAuthServer, operator: &OperatorSigner) -> Result<()> {
        ensure!(
            self.issuer == server.issuer && self.client_id == operator.client_id(),
            "account/server/operator binding mismatch"
        );
        if let Some(key) = &self.dpop {
            key.check_binding(&self.issuer, &self.user_id)?;
        }
        Ok(())
    }

    pub fn export_node_local_secret(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&StoredAccount {
            version: 1,
            issuer: self.issuer.clone(),
            client_id: self.client_id.clone(),
            user_id: self.user_id.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            token_type: self.token_type,
            resource: self.resource.clone(),
            grant_scopes: self.grant_scopes.clone(),
            access_scopes: self.access_scopes.clone(),
            expires_at: self.expires_at,
            dpop: self.dpop.as_ref().map(DpopKey::stored),
        })?)
    }

    pub fn restore_node_local_secret(
        bytes: &[u8],
        client_id: &str,
        identity: &SessionIdentity,
    ) -> Result<Self> {
        let stored: StoredAccount = serde_json::from_value(response_json(bytes)?)
            .map_err(|_| anyhow!("invalid stored account credential"))?;
        ensure!(
            stored.version == 1,
            "unsupported account credential version"
        );
        let account = Self {
            issuer: stored.issuer,
            client_id: stored.client_id,
            user_id: stored.user_id,
            access_token: stored.access_token,
            refresh_token: stored.refresh_token,
            token_type: stored.token_type,
            resource: stored.resource,
            grant_scopes: stored.grant_scopes,
            access_scopes: stored.access_scopes,
            expires_at: stored.expires_at,
            dpop: stored.dpop.map(DpopKey::restore).transpose()?,
        };
        account.check_binding(client_id, identity.company_identifier(), identity.user_id())?;
        secret(&account.access_token, "stored access token")?;
        secret(&account.refresh_token, "stored refresh token")?;
        scopes(&account.grant_scopes)?;
        scopes(&account.access_scopes)?;
        ensure!(
            account
                .access_scopes
                .iter()
                .all(|s| account.grant_scopes.contains(s)),
            "stored scopes exceed grant"
        );
        validate_resource(account.resource.as_deref())?;
        if let Some(key) = &account.dpop {
            key.check_binding(&account.issuer, &account.user_id)?;
        }
        ensure!(
            account.token_type != TokenType::DPoP || account.dpop.is_some(),
            "stored DPoP token has no private key"
        );
        Ok(account)
    }
}

fn parse_token(body: &[u8], requested_scopes: &[String]) -> Result<ParsedToken> {
    scopes(requested_scopes)?;
    let value = response_json(body)?;
    ensure!(
        value.get("error").is_none(),
        "OAuth response contains an error"
    );
    let access_token = string(&value, "access_token")?;
    ensure!(
        !access_token.chars().any(char::is_whitespace),
        "access token contains whitespace"
    );
    let token_type = match string(&value, "token_type")?.to_ascii_lowercase().as_str() {
        "bearer" => TokenType::Bearer,
        "dpop" => TokenType::DPoP,
        _ => bail!("unsupported OAuth token_type"),
    };
    // Header validation happens before credential mutation, not at first use.
    reqwest::header::HeaderValue::from_str(&access_token)
        .map_err(|_| anyhow!("access token cannot be represented in an HTTP header"))?;
    let refresh_token = value
        .get("refresh_token")
        .map(|_| string(&value, "refresh_token"))
        .transpose()?;
    let access_scopes = match value.get("scope") {
        Some(scope) => scope
            .as_str()
            .context("invalid response scope")?
            .split(' ')
            .map(str::to_owned)
            .collect(),
        None => requested_scopes.to_vec(),
    };
    scopes(&access_scopes)?;
    ensure!(
        access_scopes.iter().all(|s| requested_scopes.contains(s)),
        "server granted unrequested OAuth scope"
    );
    let expires_at = value
        .get("expires_in")
        .map(|value| -> Result<u64> {
            let seconds = value.as_u64().context("invalid access token lifetime")?;
            now()?
                .checked_add(seconds)
                .context("access token expiry overflow")
        })
        .transpose()?;
    Ok(ParsedToken {
        access_token,
        refresh_token,
        token_type,
        access_scopes,
        expires_at,
    })
}

/// A sanitized protocol error. Descriptions and raw bodies may contain remote
/// secrets or instruction text and are deliberately not copied into it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthFailure {
    pub status: u16,
    pub error: String,
}

pub fn oauth_failure(status: u16, body: &[u8]) -> Result<Option<OAuthFailure>> {
    let value = response_json(body)?;
    let Some(error) = value.get("error") else {
        return Ok(None);
    };
    let error = error.as_str().context("invalid OAuth error code")?;
    validate_error(error)?;
    Ok(Some(OAuthFailure {
        status,
        error: error.into(),
    }))
}

fn validate_error(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-./:".contains(&b)),
        "invalid OAuth error code"
    );
    Ok(())
}

pub(super) fn validate_resource(resource: Option<&str>) -> Result<()> {
    if let Some(resource) = resource {
        https_url(resource)?;
    }
    Ok(())
}

pub(super) fn add_resource(
    fields: &mut Vec<(String, String)>,
    resource: Option<&str>,
) -> Result<()> {
    validate_resource(resource)?;
    if let Some(resource) = resource {
        fields.push(("resource".into(), resource.into()));
    }
    Ok(())
}

fn with_query(endpoint: &Url, fields: &[(String, String)]) -> Result<Url> {
    ensure!(
        !endpoint
            .query_pairs()
            .any(|(name, _)| fields.iter().any(|(key, _)| key == name.as_ref())),
        "authorization endpoint already contains an OAuth request parameter"
    );
    let mut url = endpoint.clone();
    url.query_pairs_mut()
        .extend_pairs(fields.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    Ok(url)
}

fn redirect_url(client_id: &str, uri: &str) -> Result<Url> {
    let client = https_url(client_id)?;
    ensure!(
        !uri.bytes().any(|b| b.is_ascii_control() || b == b'\\'),
        "invalid redirect URI character"
    );
    let url = Url::parse(uri).map_err(|_| anyhow!("invalid redirect URI"))?;
    let loopback = url.host().is_some_and(|h| match h {
        url::Host::Ipv4(ip) => ip.is_loopback(),
        url::Host::Ipv6(ip) => ip.is_loopback(),
        _ => false,
    });
    ensure!(
        (url.scheme() == "https" && url.host_str() == client.host_str())
            || (url.scheme() == "http" && loopback),
        "redirect URI must be same-domain HTTPS or an HTTP loopback IP"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "redirect URI contains credentials or fragment"
    );
    ensure!(
        !url.query_pairs().any(|(k, _)| [
            "code",
            "state",
            "iss",
            "error",
            "error_description",
            "error_uri"
        ]
        .contains(&k.as_ref())),
        "redirect URI query collides with OAuth response parameters"
    );
    Ok(url)
}

fn validate_redirect(client_id: &str, uri: &str, registered: &[String]) -> Result<()> {
    let target = redirect_url(client_id, uri)?;
    let matches = registered.iter().any(|candidate| {
        if candidate == uri {
            return redirect_url(client_id, candidate).is_ok();
        }
        if target.scheme() != "http" {
            return false;
        }
        let Ok(mut registered) = redirect_url(client_id, candidate) else {
            return false;
        };
        if registered.set_port(target.port()).is_err() {
            return false;
        }
        registered == target
    });
    ensure!(matches, "redirect URI is not registered in client metadata");
    Ok(())
}

pub fn client_metadata_with_redirects(
    operator: &OperatorSigner,
    client_name: &str,
    jwks_uri: &str,
    redirect_uris: &[String],
) -> Result<Value> {
    for uri in redirect_uris {
        redirect_url(operator.client_id(), uri)?;
    }
    let mut metadata = operator.client_metadata(client_name, jwks_uri)?;
    metadata["redirect_uris"] = serde_json::to_value(redirect_uris)?;
    Ok(metadata)
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}
