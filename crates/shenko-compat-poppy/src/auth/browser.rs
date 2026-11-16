use anyhow::{Context, Result, ensure};
use reqwest::{Method, Request, header};
use serde_json::{Value, json};
use url::Url;

use crate::discovery::DiscoveryReport;

use super::{AccountTokens, https_url, secret};

/// Poppy 1.0/0.4 browser join uses an owned browser's cookie-bound form.
/// No token or Session header is ever part of the browser submission.
pub struct BrowserJoin {
    endpoint: Url,
    authorize_endpoint: Url,
    company_identifier: String,
    unscoped_website: bool,
}

impl BrowserJoin {
    pub fn from_discovery(report: &DiscoveryReport) -> Result<Self> {
        let web = report
            .capabilities
            .web
            .as_ref()
            .context("company has no browser declaration")?;
        let endpoint = https_url(
            web.get("browser_session_endpoint")
                .and_then(Value::as_str)
                .context("company offers no browser Session join")?,
        )?;
        let mut authorize_endpoint = endpoint.clone();
        authorize_endpoint.set_path(&format!(
            "{}/authorize",
            endpoint.path().trim_end_matches('/')
        ));
        Ok(Self {
            endpoint,
            authorize_endpoint,
            company_identifier: report.company_identifier.clone(),
            unscoped_website: web.get("unscoped").and_then(Value::as_bool) == Some(true),
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }
    pub fn authorize_endpoint(&self) -> &Url {
        &self.authorize_endpoint
    }
    pub fn website_has_full_account_access(&self) -> bool {
        self.unscoped_website
    }

    /// This checks an actual account grant, not a scope hint in company prose.
    /// An absent grant leaves signed-out join available; the caller decides
    /// whether a new sign-in is appropriate and retains that decision.
    pub fn check_account(&self, tokens: &AccountTokens) -> Result<()> {
        let metadata = tokens.metadata();
        ensure!(
            metadata.issuer == self.company_identifier,
            "browser account belongs to another company"
        );
        ensure!(
            metadata.resource.is_none(),
            "browser join requires the company's default token"
        );
        ensure!(
            metadata
                .access_scopes
                .iter()
                .any(|s| s == "poppy:browser-sign-in"),
            "account token has no poppy:browser-sign-in scope"
        );
        Ok(())
    }

    /// Trusted browser-adapter boundary. These values must come directly from
    /// the same controlled browser's top-level GET of `endpoint`, with the
    /// final URL exactly matching it. Never call this with a code from model input, messages or an
    /// arbitrary page. Keep the resulting object out of observations and CAS.
    pub fn from_controlled_browser(
        &self,
        browser_identity: &str,
        loaded_url: &Url,
        form_action: &Url,
        hidden_code: &str,
    ) -> Result<PendingBrowserJoin> {
        ensure!(
            !browser_identity.is_empty(),
            "owned browser identity is absent"
        );
        ensure!(
            loaded_url == &self.endpoint && form_action == &self.endpoint,
            "join form was not loaded from and addressed to the exact endpoint"
        );
        secret(hidden_code, "browser join code")?;
        Ok(PendingBrowserJoin {
            browser_identity: browser_identity.into(),
            endpoint: self.endpoint.clone(),
            authorize_endpoint: self.authorize_endpoint.clone(),
            code: hidden_code.into(),
        })
    }
}

pub struct PendingBrowserJoin {
    browser_identity: String,
    endpoint: Url,
    authorize_endpoint: Url,
    code: String,
}

impl PendingBrowserJoin {
    /// Apply account authorization (if any), a freshly signed Session and the
    /// negotiated Poppy-Version through the normal validated Poppy transport.
    /// The request is sent by the agent HTTP client, never the browser.
    pub fn authorization_request(&self) -> Result<Request> {
        let mut request = Request::new(Method::POST, self.authorize_endpoint.clone());
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        *request.body_mut() = Some(serde_json::to_vec(&json!({"code":self.code}))?.into());
        Ok(request)
    }

    /// Only an observed HTTP 204 authorizes form submission. A timeout keeps
    /// the join outcome unknown and does not produce an authorized challenge.
    pub fn accept_authorized(self, status: u16) -> Result<AuthorizedBrowserJoin> {
        ensure!(status == 204, "browser join authorization was not HTTP 204");
        Ok(AuthorizedBrowserJoin {
            browser_identity: self.browser_identity,
            endpoint: self.endpoint,
            code: self.code,
        })
    }
}

pub struct AuthorizedBrowserJoin {
    browser_identity: String,
    endpoint: Url,
    code: String,
}

impl AuthorizedBrowserJoin {
    /// The browser adapter must revalidate its current document, original form
    /// and control lease, then submit the actual form with its own cookie jar.
    /// Passing this object to another browser or restarting loses continuity.
    pub fn submission(
        self,
        browser_identity: &str,
        current_url: &Url,
    ) -> Result<BrowserSubmission> {
        ensure!(
            self.browser_identity == browser_identity && &self.endpoint == current_url,
            "browser identity or endpoint changed during join"
        );
        Ok(BrowserSubmission {
            endpoint: self.endpoint,
            code: self.code,
        })
    }
}

/// A private comparison value for the native browser adapter. Do not issue an
/// unrelated HTTP POST with this body: only the original browser has the form's
/// binding cookie. Submission alone does not prove the merchant set a Session.
pub struct BrowserSubmission {
    endpoint: Url,
    code: String,
}

impl BrowserSubmission {
    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }
    pub fn hidden_code_for_native_adapter(&self) -> &str {
        &self.code
    }
}
