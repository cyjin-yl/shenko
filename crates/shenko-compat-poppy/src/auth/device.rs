use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::session::{OperatorSigner, SessionIdentity};

use super::{
    AccountTokens, DpopKey, OAuthFailure, OAuthRequest, OAuthServer, dpop::StoredDpopKey,
    https_url, now, oauth::add_resource, response_json, scopes, secret, string,
};

/// Initial device authorization context. All selections come from the caller;
/// the protocol does not decide that an account or scope should be requested.
pub struct DeviceSignIn {
    server: OAuthServer,
    client_id: String,
    user_id: String,
    requested_scopes: Vec<String>,
    dpop: Option<DpopKey>,
}

impl DeviceSignIn {
    pub fn new(
        server: OAuthServer,
        operator: &OperatorSigner,
        identity: &SessionIdentity,
        requested_scopes: Vec<String>,
    ) -> Result<Self> {
        server.check_sign_in(&requested_scopes)?;
        ensure!(
            server.device_supported,
            "company does not offer device sign-in"
        );
        ensure!(
            identity.company_identifier() == server.issuer,
            "Session belongs to another company"
        );
        let dpop = if server.use_dpop {
            Some(DpopKey::generate(&server.issuer, identity.user_id())?)
        } else {
            None
        };
        Ok(Self {
            server,
            client_id: operator.client_id().into(),
            user_id: identity.user_id().into(),
            requested_scopes,
            dpop,
        })
    }

    pub fn request(&self, operator: &OperatorSigner) -> Result<OAuthRequest> {
        ensure!(
            operator.client_id() == self.client_id,
            "device request belongs to another operator"
        );
        let endpoint = self
            .server
            .device_endpoint
            .as_ref()
            .context("device authorization endpoint is absent")?;
        let mut fields = vec![("scope".into(), scopes(&self.requested_scopes)?)];
        self.server
            .request(endpoint, &mut fields, operator, None, None)
    }

    pub fn accept_response(self, status: u16, body: &[u8]) -> Result<DeviceAuthorization> {
        ensure!(
            status == 200,
            "device authorization response was not HTTP 200"
        );
        let value = response_json(body)?;
        let device_code = string(&value, "device_code")?;
        let verification_uri = verified_link(&self.server, &string(&value, "verification_uri")?)?;
        let verification_uri_complete = value
            .get("verification_uri_complete")
            .map(|_| verified_link(&self.server, &string(&value, "verification_uri_complete")?))
            .transpose()?;
        let user_code = string(&value, "user_code")?;
        let lifetime = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .context("device code lifetime is absent")?;
        ensure!(lifetime > 0, "device code has expired");
        let interval = match value.get("interval") {
            None => 5,
            Some(value) => value
                .as_u64()
                .filter(|interval| *interval > 0)
                .context("invalid device polling interval")?,
        };
        let time = now()?;
        Ok(DeviceAuthorization {
            context: self,
            device_code,
            verification_uri,
            verification_uri_complete,
            user_code,
            expires_at: time
                .checked_add(lifetime)
                .context("device expiry overflow")?,
            interval,
            next_poll_at: time
                .checked_add(interval)
                .context("device poll time overflow")?,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceSignInPresentation {
    pub company_name: String,
    pub company_domain: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_at_unix_secs: u64,
    pub next_poll_at_unix_secs: u64,
}

/// Secret device code remains private; only the validated, company-labelled
/// presentation is for the user. A crash does not restart the original expiry.
pub struct DeviceAuthorization {
    context: DeviceSignIn,
    device_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    user_code: String,
    expires_at: u64,
    interval: u64,
    next_poll_at: u64,
}

#[derive(Serialize, Deserialize)]
struct StoredDevice {
    version: u8,
    issuer: String,
    client_id: String,
    user_id: String,
    requested_scopes: Vec<String>,
    dpop: Option<StoredDpopKey>,
    device_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    user_code: String,
    expires_at: u64,
    interval: u64,
    next_poll_at: u64,
}

impl DeviceAuthorization {
    pub fn presentation(&self) -> DeviceSignInPresentation {
        DeviceSignInPresentation {
            company_name: self.context.server.company_name.clone(),
            company_domain: self.context.server.company_origin.clone(),
            verification_uri: self.verification_uri.clone(),
            verification_uri_complete: self.verification_uri_complete.clone(),
            user_code: self.user_code.clone(),
            expires_at_unix_secs: self.expires_at,
            next_poll_at_unix_secs: self.next_poll_at,
        }
    }

    /// No loop or sleeping is hidden here. Persist the updated next-poll time
    /// before dispatch; every actual poll carries fresh client authentication.
    pub fn poll_request(
        &mut self,
        operator: &OperatorSigner,
        resource: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<OAuthRequest> {
        ensure!(
            operator.client_id() == self.context.client_id,
            "device grant belongs to another operator"
        );
        let time = now()?;
        ensure!(time < self.expires_at, "device authorization expired");
        ensure!(
            time >= self.next_poll_at,
            "device token poll would violate issuer interval"
        );
        let mut fields = vec![
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:device_code".into(),
            ),
            ("device_code".into(), self.device_code.clone()),
        ];
        add_resource(&mut fields, resource)?;
        let request = self.context.server.request(
            &self.context.server.token_endpoint,
            &mut fields,
            operator,
            self.context.dpop.as_ref(),
            nonce,
        )?;
        self.next_poll_at = time
            .checked_add(self.interval)
            .context("device poll time overflow")?;
        Ok(request)
    }

    /// True means this particular protocol rejection leaves sign-in pending.
    /// All other errors are exposed to the caller without inventing a retry.
    pub fn observe_failure(&mut self, failure: &OAuthFailure) -> Result<bool> {
        if failure.status != 400 {
            return Ok(false);
        }
        match failure.error.as_str() {
            "authorization_pending" => Ok(true),
            "slow_down" => {
                self.interval = self
                    .interval
                    .checked_add(5)
                    .context("device poll interval overflow")?;
                self.next_poll_at = now()?
                    .checked_add(self.interval)
                    .context("device poll time overflow")?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// RFC 8628 requires slower polling after connection timeouts. This records
    /// timing only; it does not assert that the previous poll had no effect.
    pub fn record_transport_timeout(&mut self) -> Result<()> {
        self.interval = self
            .interval
            .checked_mul(2)
            .context("device poll interval overflow")?;
        self.next_poll_at = now()?
            .checked_add(self.interval)
            .context("device poll time overflow")?;
        Ok(())
    }

    pub fn accept_token_response(
        &self,
        status: u16,
        body: &[u8],
        resource: Option<&str>,
    ) -> Result<AccountTokens> {
        ensure!(status == 200, "device token response was not HTTP 200");
        AccountTokens::new_grant(
            &self.context.server,
            &self.context.client_id,
            &self.context.user_id,
            &self.context.requested_scopes,
            self.context.dpop.as_ref(),
            body,
            resource,
        )
    }

    pub fn export_node_local_secret(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(&StoredDevice {
            version: 1,
            issuer: self.context.server.issuer.clone(),
            client_id: self.context.client_id.clone(),
            user_id: self.context.user_id.clone(),
            requested_scopes: self.context.requested_scopes.clone(),
            dpop: self.context.dpop.as_ref().map(DpopKey::stored),
            device_code: self.device_code.clone(),
            verification_uri: self.verification_uri.clone(),
            verification_uri_complete: self.verification_uri_complete.clone(),
            user_code: self.user_code.clone(),
            expires_at: self.expires_at,
            interval: self.interval,
            next_poll_at: self.next_poll_at,
        })?)
    }

    pub fn restore_node_local_secret(
        bytes: &[u8],
        server: OAuthServer,
        operator: &OperatorSigner,
        identity: &SessionIdentity,
    ) -> Result<Self> {
        let stored: StoredDevice = serde_json::from_value(response_json(bytes)?)
            .map_err(|_| anyhow!("invalid stored device grant"))?;
        ensure!(
            stored.version == 1
                && stored.issuer == server.issuer
                && stored.client_id == operator.client_id()
                && stored.user_id == identity.user_id()
                && identity.company_identifier() == server.issuer,
            "stored device grant binding mismatch"
        );
        secret(&stored.device_code, "device code")?;
        scopes(&stored.requested_scopes)?;
        secret(&stored.user_code, "user code")?;
        ensure!(stored.interval > 0, "invalid stored poll interval");
        let verification_uri = verified_link(&server, &stored.verification_uri)?;
        let verification_uri_complete = stored
            .verification_uri_complete
            .as_deref()
            .map(|u| verified_link(&server, u))
            .transpose()?;
        let dpop = stored.dpop.map(DpopKey::restore).transpose()?;
        if let Some(key) = &dpop {
            key.check_binding(&server.issuer, identity.user_id())?;
        }
        Ok(Self {
            context: DeviceSignIn {
                server,
                client_id: stored.client_id,
                user_id: stored.user_id,
                requested_scopes: stored.requested_scopes,
                dpop,
            },
            device_code: stored.device_code,
            verification_uri,
            verification_uri_complete,
            user_code: stored.user_code,
            expires_at: stored.expires_at,
            interval: stored.interval,
            next_poll_at: stored.next_poll_at,
        })
    }
}

fn verified_link(server: &OAuthServer, text: &str) -> Result<String> {
    let url = https_url(text)?;
    let issuer = Url::parse(&server.issuer)?;
    let company = Url::parse(&server.company_origin)?;
    let host = url
        .host_str()
        .context("device verification link has no host")?;
    ensure!(
        [issuer.host_str(), company.host_str()]
            .into_iter()
            .flatten()
            .any(|allowed| host == allowed
                || host
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))),
        "device verification link does not belong to the company or its issuer"
    );
    Ok(text.into())
}
