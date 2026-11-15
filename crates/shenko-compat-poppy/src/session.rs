//! Delegated Poppy Session signatures for 1.0 and the compatible 0.4 wire format.
//!
//! This module does no I/O and grants no account access. The caller supplies a
//! company identifier from verified discovery and a confidential, persistent
//! pairwise user ID. Only the transport can decide whether a particular URL is
//! a discovered Session destination; merely sharing a hostname is insufficient.
//! It must also send the negotiated Poppy-Version and any Poppy-Extensions.
//!
//! The returned JWT does not replace an HTTP Message Signature or a DPoP proof,
//! and it does not cover a request body or query. Secret-bearing objects have
//! no Debug or Serialize implementation, and this module never exports a private key.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use url::Url;

const MAX_DELEGATION_SECONDS: u64 = 24 * 60 * 60;

// Declaration order is RFC 7638's lexicographic order. Serializing this type
// rather than a general JSON map keeps thumbprints independent of map features.
#[derive(Serialize)]
struct PublicEcJwk {
    crv: &'static str,
    kty: &'static str,
    x: String,
    y: String,
}

impl PublicEcJwk {
    fn from_key(key: &EcdsaKeyPair) -> Result<Self> {
        let point = key.public_key().as_ref();
        ensure!(
            point.len() == 65 && point[0] == 4,
            "invalid P-256 public key encoding"
        );
        Ok(Self {
            crv: "P-256",
            kty: "EC",
            x: URL_SAFE_NO_PAD.encode(&point[1..33]),
            y: URL_SAFE_NO_PAD.encode(&point[33..65]),
        })
    }

    fn thumbprint(&self) -> Result<String> {
        Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(serde_json::to_vec(self)?)))
    }
}

/// A node-local operator key whose public key is published in `jwks_uri`.
///
/// This key signs delegations only. Loading a key neither publishes its public
/// metadata nor establishes that a company accepts the client.
pub struct OperatorSigner {
    client_id: String,
    key_id: String,
    key: EcdsaKeyPair,
    public_key: PublicEcJwk,
}

impl OperatorSigner {
    /// Load an operator-provisioned P-256 key without retaining its PKCS#8 input.
    /// The caller owns that input and its storage/zeroization policy.
    pub fn from_pkcs8(client_id: &str, key_id: &str, pkcs8: &[u8]) -> Result<Self> {
        https_url(client_id)?;
        opaque_id(key_id, "operator key ID")?;
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow!("invalid ES256 PKCS#8 operator key"))?;
        let public_key = PublicEcJwk::from_key(&key)?;
        Ok(Self {
            client_id: client_id.into(),
            key_id: key_id.into(),
            key,
            public_key,
        })
    }

    /// Public metadata sufficient for signed-out identity. Direct Sign-In also
    /// needs registered redirect URIs and an OAuth implementation outside here.
    pub fn client_metadata(&self, client_name: &str, jwks_uri: &str) -> Result<Value> {
        ensure!(!client_name.trim().is_empty(), "client name is empty");
        let client = https_url(&self.client_id)?;
        let keys = https_url(jwks_uri)?;
        ensure!(
            client.origin() == keys.origin(),
            "JWKS URL must use the client metadata origin"
        );
        Ok(json!({
            "client_id": self.client_id,
            "client_name": client_name,
            "jwks_uri": jwks_uri,
            "token_endpoint_auth_method": "private_key_jwt",
        }))
    }

    /// Public verification material only; the private scalar is never emitted.
    pub fn public_jwks(&self) -> Value {
        json!({"keys": [{
            "kty": self.public_key.kty, "crv": self.public_key.crv,
            "x": self.public_key.x, "y": self.public_key.y,
            "kid": self.key_id, "alg": "ES256", "use": "sig",
        }]})
    }

    /// Create a fresh key delegated to exactly this local Session identity.
    /// Company/user information stays local and is absent from the delegation.
    pub fn delegate(&self, identity: SessionIdentity, lifetime: Duration) -> Result<SessionSigner> {
        self.delegate_at(identity, lifetime, unix_seconds()?)
    }

    fn delegate_at(
        &self,
        identity: SessionIdentity,
        lifetime: Duration,
        now: u64,
    ) -> Result<SessionSigner> {
        let seconds = lifetime.as_secs();
        ensure!(
            (1..=MAX_DELEGATION_SECONDS).contains(&seconds),
            "delegation lifetime must be 1..=86400 seconds"
        );
        let expires_at = now
            .checked_add(seconds)
            .context("delegation expiry overflow")?;
        let random = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &random)
            .map_err(|_| anyhow!("could not generate delegated P-256 key"))?;
        let key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &random)
                .map_err(|_| anyhow!("could not load generated delegated P-256 key"))?;
        let public_key = PublicEcJwk::from_key(&key)?;
        let thumbprint = public_key.thumbprint()?;
        let delegation = compact_jwt(
            &self.key,
            &json!({"typ":"poppy-delegation+jwt", "alg":"ES256", "kid":self.key_id}),
            &json!({
                "iss":self.client_id,
                "cnf":{"jwk":public_key},
                "iat":now, "exp":expires_at, "jti":random_id()?,
            }),
        )?;
        Ok(SessionSigner {
            client_id: self.client_id.clone(),
            identity,
            key,
            thumbprint,
            delegation,
            issued_at: now,
            expires_at,
        })
    }
}

/// A stable binding within a single company. Its fields are private so a
/// delegated signer cannot be retargeted to a second company or user.
///
/// The runtime must persist the pairwise user ID mapping confidentially. IDs
/// must not contain or derive from personal data, even through a keyed hash.
/// This type represents regular, stable user IDs, not private-browsing IDs.
pub struct SessionIdentity {
    company_identifier: String,
    user_id: String,
    session_id: String,
    agent_id: Option<String>,
}

impl SessionIdentity {
    /// Start a new Session using a cryptographically random, independent ID.
    pub fn new(company_identifier: &str, stable_user_id: &str) -> Result<Self> {
        Self::resume(company_identifier, stable_user_id, &random_id()?, None)
    }

    /// Restore an existing Session, for example when rotating a delegated key.
    /// The caller must restore the original company/user/session mapping, not
    /// reuse a Session ID for a different task or user. No key is restored here.
    pub fn resume(
        company_identifier: &str,
        stable_user_id: &str,
        session_id: &str,
        agent_id: Option<&str>,
    ) -> Result<Self> {
        let company = https_url(company_identifier)?;
        ensure!(
            company.query().is_none(),
            "company identifier must not contain a query"
        );
        opaque_id(stable_user_id, "pairwise user ID")?;
        ensure!(
            !session_id.is_empty()
                && session_id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "Session ID must contain only ASCII letters, digits, '-' or '_'"
        );
        if let Some(id) = agent_id {
            opaque_id(id, "pairwise agent ID")?;
        }
        Ok(Self {
            // Preserve the issuer byte-for-byte: a trailing slash can be part
            // of a real issuer and must not be silently normalized away.
            company_identifier: company_identifier.into(),
            user_id: stable_user_id.into(),
            session_id: session_id.into(),
            agent_id: agent_id.map(str::to_owned),
        })
    }

    pub fn company_identifier(&self) -> &str {
        &self.company_identifier
    }
    pub fn user_id(&self) -> &str {
        &self.user_id
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }
}

/// A short-lived signing key fixed to one company/user/session binding.
pub struct SessionSigner {
    client_id: String,
    identity: SessionIdentity,
    key: EcdsaKeyPair,
    thumbprint: String,
    delegation: String,
    issued_at: u64,
    expires_at: u64,
}

impl SessionSigner {
    pub fn identity(&self) -> &SessionIdentity {
        &self.identity
    }
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Sign the final wire method and URL with a fresh nonce and current time.
    ///
    /// The caller must validate this exact destination against discovery before
    /// sending the result. In particular, do not attach it to discovery, OAuth,
    /// description documents, arbitrary same-host paths, or browser navigation.
    /// An access token here is only hashed; it must already be scoped to the
    /// actual service and account by the credential-owning runtime.
    pub fn sign_request(
        &self,
        method: &str,
        request_url: &Url,
        access_token: Option<&str>,
    ) -> Result<String> {
        self.sign_request_at(method, request_url, access_token, unix_seconds()?)
    }

    fn sign_request_at(
        &self,
        method: &str,
        request_url: &Url,
        access_token: Option<&str>,
        now: u64,
    ) -> Result<String> {
        ensure!(
            now >= self.issued_at && now < self.expires_at,
            "delegation is not currently valid"
        );
        ensure!(
            !method.is_empty() && method.bytes().all(method_token),
            "invalid HTTP method"
        );
        let mut target = request_url.clone();
        target.set_query(None);
        target.set_fragment(None);
        https_url(target.as_str())?;
        let token_hash = access_token
            .map(|token| -> Result<String> {
                ensure!(!token.is_empty(), "access token is empty");
                Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes())))
            })
            .transpose()?;
        let claims = SessionClaims {
            iss: &self.client_id,
            aud: &self.identity.company_identifier,
            sid: &self.identity.session_id,
            sub: &self.identity.user_id,
            agent_id: self.identity.agent_id.as_deref(),
            htm: method,
            htu: target.as_str(),
            iat: now,
            jti: random_id()?,
            ath: token_hash,
        };
        compact_jwt(
            &self.key,
            &json!({
                "typ":"poppy-session+jwt", "alg":"ES256",
                "kid":self.thumbprint, "poppy_dlg":self.delegation,
            }),
            &claims,
        )
    }
}

#[derive(Serialize)]
struct SessionClaims<'a> {
    iss: &'a str,
    aud: &'a str,
    sid: &'a str,
    sub: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    htm: &'a str,
    htu: &'a str,
    iat: u64,
    jti: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ath: Option<String>,
}

fn https_url(value: &str) -> Result<Url> {
    ensure!(
        !value.chars().any(char::is_whitespace) && !value.chars().any(char::is_control),
        "HTTPS URL contains whitespace or a control character"
    );
    let url = Url::parse(value).context("invalid HTTPS URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "expected an HTTPS URL without credentials or a fragment"
    );
    Ok(url)
}

fn opaque_id(value: &str, name: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && !value.chars().any(char::is_control),
        "invalid {name}"
    );
    Ok(())
}

fn method_token(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c)
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs())
}

fn random_id() -> Result<String> {
    let mut bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| anyhow!("system random source unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn compact_jwt(
    key: &EcdsaKeyPair,
    header: &impl Serialize,
    claims: &impl Serialize,
) -> Result<String> {
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(header)?),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?)
    );
    let signature = key
        .sign(&SystemRandom::new(), input.as_bytes())
        .map_err(|_| anyhow!("ES256 signing failed"))?;
    // FIXED_SIGNING emits JOSE's 32-byte r followed by 32-byte s, not ASN.1 DER.
    Ok(format!(
        "{input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED, UnparsedPublicKey};

    fn operator() -> Result<OperatorSigner> {
        let pkcs8 =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| anyhow!("test key generation failed"))?;
        OperatorSigner::from_pkcs8(
            "https://agent.example/agent.json",
            "operator-2026",
            pkcs8.as_ref(),
        )
    }

    fn verified(jwt: &str, key: &Value) -> Result<(Value, Value)> {
        let parts: Vec<_> = jwt.split('.').collect();
        ensure!(parts.len() == 3, "JWT parts");
        let mut point = vec![4];
        point.extend(URL_SAFE_NO_PAD.decode(key["x"].as_str().context("JWK x")?)?);
        point.extend(URL_SAFE_NO_PAD.decode(key["y"].as_str().context("JWK y")?)?);
        let signature = URL_SAFE_NO_PAD.decode(parts[2])?;
        assert_eq!(signature.len(), 64, "JOSE ES256 uses raw r || s");
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
            .map_err(|_| anyhow!("JWT verification failed"))?;
        Ok((
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0])?)?,
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1])?)?,
        ))
    }

    #[test]
    fn company_verifies_delegation_and_fresh_request_bindings() -> Result<()> {
        let operator = operator()?;
        let metadata = operator.client_metadata("Shenko", "https://agent.example/jwks.json")?;
        assert_eq!(metadata["client_id"], "https://agent.example/agent.json");
        let keys = operator.public_jwks();
        assert!(keys["keys"][0].get("d").is_none());
        let identity = SessionIdentity::new("https://auth.example/tenant/", "opaque-user-vZzyq8")?;
        let original_sid = identity.session_id().to_owned();
        let signer = operator.delegate_at(identity, Duration::from_secs(600), 1_790_000_000)?;
        let request = Url::parse("https://api.example/Orders/a%2Fb/?page=2#anchor")?;
        let jwt = signer.sign_request_at("POST", &request, Some("access-token"), 1_790_000_001)?;
        let header: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD.decode(jwt.split('.').next().context("JWT header")?)?,
        )?;
        let (delegation_header, delegation) = verified(
            header["poppy_dlg"].as_str().context("delegation")?,
            &keys["keys"][0],
        )?;
        assert_eq!(delegation_header["typ"], "poppy-delegation+jwt");
        assert_eq!(delegation_header["kid"], "operator-2026");
        assert_eq!(
            delegation.as_object().context("delegation object")?.len(),
            5
        );
        assert!(delegation.get("aud").is_none() && delegation.get("sub").is_none());
        let public_key = &delegation["cnf"]["jwk"];
        assert_eq!(public_key.as_object().context("public JWK")?.len(), 4);
        let (session_header, claims) = verified(&jwt, public_key)?;
        let thumbprint_input = format!(
            "{{\"crv\":\"P-256\",\"kty\":\"EC\",\"x\":\"{}\",\"y\":\"{}\"}}",
            public_key["x"].as_str().context("x")?,
            public_key["y"].as_str().context("y")?
        );
        assert_eq!(
            session_header["kid"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(thumbprint_input))
        );
        assert_eq!(session_header["typ"], "poppy-session+jwt");
        assert_eq!(claims["aud"], "https://auth.example/tenant/");
        assert_eq!(claims["sid"], original_sid);
        assert_eq!(claims["sub"], "opaque-user-vZzyq8");
        assert_eq!(claims["htm"], "POST");
        assert_eq!(claims["htu"], "https://api.example/Orders/a%2Fb/");
        assert_eq!(
            claims["ath"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"access-token"))
        );
        assert!(claims.get("agent_id").is_none());
        let signed_out = signer.sign_request_at("GET", &request, None, 1_790_000_002)?;
        let (_, second) = verified(&signed_out, public_key)?;
        assert_ne!(claims["jti"], second["jti"]);
        assert_eq!(claims["sid"], second["sid"]);
        assert_eq!(second["htm"], "GET");
        assert!(second.get("ath").is_none());
        Ok(())
    }

    #[test]
    fn expired_delegation_cannot_sign_another_request() -> Result<()> {
        let identity = SessionIdentity::resume(
            "https://company.example",
            "opaque-user",
            "persisted_random_sid",
            None,
        )?;
        let signer = operator()?.delegate_at(identity, Duration::from_secs(10), 1_790_000_000)?;
        let request = Url::parse("https://company.example/api/orders")?;
        assert!(
            signer
                .sign_request_at("GET", &request, None, 1_790_000_009)
                .is_ok()
        );
        assert!(
            signer
                .sign_request_at("GET", &request, None, signer.expires_at())
                .is_err()
        );
        Ok(())
    }
}
