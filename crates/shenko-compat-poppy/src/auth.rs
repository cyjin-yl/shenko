//! Poppy account authorization and controlled-browser Session joining.
//!
//! These primitives prepare wire requests; they do not send them or choose
//! scopes. The host persists intent before sending, validates/pins each public
//! destination, disables redirects and retries, and keeps credentials out of
//! shared task inputs, receipts and model context. A transport failure leaves
//! an exchange, refresh, revocation or join outcome unknown.
//!
//! Account authorization is independent of Session identity. OAuth endpoints
//! receive client assertions and, where advertised, DPoP, never a Session JWT.
//! Browser navigation receives neither token nor Session JWT: the browser joins
//! through its own one-time form and then uses only the Company's cookies.

mod browser;
mod device;
mod dpop;
mod oauth;

pub use browser::{AuthorizedBrowserJoin, BrowserJoin, BrowserSubmission, PendingBrowserJoin};
pub use device::{DeviceAuthorization, DeviceSignIn, DeviceSignInPresentation};
pub use dpop::DpopKey;
pub use oauth::{
    AccountMetadata, AccountTokens, AuthorizationAttempt, DirectSignIn, OAuthFailure, OAuthRequest,
    OAuthServer, TokenType, client_metadata_with_redirects, oauth_failure,
};

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Method, Request, header};
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::Value;
use url::Url;

const MAX_SECRET_RESPONSE: usize = 512 * 1024;

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock precedes Unix epoch")?
        .as_secs())
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("secure random generation failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn https_url(value: &str) -> Result<Url> {
    ensure!(
        !value.bytes().any(|b| b.is_ascii_control() || b == b'\\'),
        "URL contains a control character or backslash"
    );
    let url = Url::parse(value).context("invalid HTTPS URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "expected HTTPS URL without credentials or fragment"
    );
    Ok(url)
}

fn secret(value: &str, name: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= MAX_SECRET_RESPONSE
            && !value.chars().any(char::is_control),
        "invalid {name}"
    );
    Ok(())
}

fn nqchar(byte: u8) -> bool {
    byte == 0x21 || (0x23..=0x5b).contains(&byte) || (0x5d..=0x7e).contains(&byte)
}

fn scopes(values: &[String]) -> Result<String> {
    ensure!(!values.is_empty(), "at least one OAuth scope is required");
    ensure!(
        values
            .iter()
            .all(|v| !v.is_empty() && v.bytes().all(nqchar)),
        "invalid OAuth scope syntax"
    );
    Ok(values.join(" "))
}

fn response_json(body: &[u8]) -> Result<Value> {
    ensure!(
        body.len() <= MAX_SECRET_RESPONSE,
        "OAuth response exceeds bound"
    );
    // A parser error may include remote text. Do not attach that error or the
    // body to a report that could be stored with public operation observations.
    let value: Value =
        serde_json::from_slice(body).map_err(|_| anyhow!("OAuth response is not valid JSON"))?;
    ensure!(value.is_object(), "OAuth response must be an object");
    Ok(value)
}

fn string(value: &Value, name: &str) -> Result<String> {
    let text = value
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("OAuth response has no string {name}"))?;
    secret(text, name)?;
    Ok(text.into())
}

fn form_request(url: &Url, fields: &[(String, String)]) -> Result<Request> {
    https_url(url.as_str())?;
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.extend_pairs(fields.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut request = Request::new(Method::POST, url.clone());
    request.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    request.headers_mut().insert(
        header::ACCEPT,
        header::HeaderValue::from_static("application/json"),
    );
    *request.body_mut() = Some(serializer.finish().into());
    Ok(request)
}

fn sensitive_header(request: &mut Request, name: &'static str, value: &str) -> Result<()> {
    let mut header = header::HeaderValue::from_str(value)
        .map_err(|_| anyhow!("invalid credential header value"))?;
    header.set_sensitive(true);
    request.headers_mut().insert(name, header);
    Ok(())
}
