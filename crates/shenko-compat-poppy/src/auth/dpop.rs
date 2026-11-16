//! RFC 9449 proofs, with a private key fixed to one Poppy company/user pair.

use anyhow::{Result, anyhow, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use url::Url;

use super::{https_url, now, nqchar, random_secret, secret};

/// Keep this key with every refresh token it originally bound. A new Session
/// does not rotate it, and a refresh returning a Bearer access token does not
/// release the refresh token's key binding (Poppy 4.4).
///
/// No Debug or Serialize: exporting private bytes is an explicit node-local
/// credential operation, never ordinary observation serialization.
pub struct DpopKey {
    company_identifier: String,
    user_id: String,
    pkcs8: Vec<u8>,
    key: EcdsaKeyPair,
    public_jwk: Value,
    thumbprint: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct StoredDpopKey {
    company_identifier: String,
    user_id: String,
    pkcs8: String,
}

impl DpopKey {
    pub fn generate(company_identifier: &str, user_id: &str) -> Result<Self> {
        let document =
            EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &SystemRandom::new())
                .map_err(|_| anyhow!("DPoP key generation failed"))?;
        Self::from_pkcs8(company_identifier, user_id, document.as_ref())
    }

    /// Restore the original key, not a replacement, when resuming a grant.
    pub fn from_pkcs8(company_identifier: &str, user_id: &str, bytes: &[u8]) -> Result<Self> {
        let issuer = https_url(company_identifier)?;
        ensure!(issuer.query().is_none(), "issuer contains query");
        secret(user_id, "pairwise user ID")?;
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            bytes,
            &SystemRandom::new(),
        )
        .map_err(|_| anyhow!("invalid DPoP P-256 private key"))?;
        let point = key.public_key().as_ref();
        ensure!(
            point.len() == 65 && point[0] == 4,
            "invalid P-256 public point"
        );
        let x = URL_SAFE_NO_PAD.encode(&point[1..33]);
        let y = URL_SAFE_NO_PAD.encode(&point[33..]);
        // A struct keeps RFC 7638's ordering even with preserve_order enabled
        // in another crate's serde_json feature unification.
        #[derive(Serialize)]
        struct Thumbprint<'a> {
            crv: &'a str,
            kty: &'a str,
            x: &'a str,
            y: &'a str,
        }
        let canonical = serde_json::to_vec(&Thumbprint {
            crv: "P-256",
            kty: "EC",
            x: &x,
            y: &y,
        })?;
        let thumbprint = URL_SAFE_NO_PAD.encode(Sha256::digest(canonical));
        Ok(Self {
            company_identifier: company_identifier.into(),
            user_id: user_id.into(),
            pkcs8: bytes.into(),
            key,
            public_jwk: json!({"kty":"EC","crv":"P-256","x":x,"y":y}),
            thumbprint,
        })
    }

    pub fn thumbprint(&self) -> &str {
        &self.thumbprint
    }

    pub fn check_binding(&self, company_identifier: &str, user_id: &str) -> Result<()> {
        ensure!(
            self.company_identifier == company_identifier && self.user_id == user_id,
            "DPoP key belongs to another company or pairwise user"
        );
        Ok(())
    }

    /// Sign the final method and URL. Supply a nonce only from this particular
    /// authorization/resource server; their nonce namespaces are independent.
    /// Token endpoint proofs omit `access_token`; DPoP resource proofs include
    /// it. `ath` and `htu` follow RFC 9449 §4.2, not HTTP Message Signatures.
    pub fn proof(
        &self,
        method: &str,
        url: &Url,
        access_token: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<String> {
        https_url(url.as_str())?;
        reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| anyhow!("invalid DPoP HTTP method"))?;
        let mut target = url.clone();
        target.set_query(None);
        target.set_fragment(None);
        let mut claims =
            json!({"jti":random_secret()?,"htm":method,"htu":target.as_str(),"iat":now()?});
        if let Some(token) = access_token {
            secret(token, "access token")?;
            claims["ath"] = URL_SAFE_NO_PAD
                .encode(Sha256::digest(token.as_bytes()))
                .into();
        }
        if let Some(nonce) = nonce {
            secret(nonce, "DPoP nonce")?;
            ensure!(
                !nonce.is_empty() && nonce.bytes().all(nqchar),
                "invalid DPoP nonce"
            );
            claims["nonce"] = nonce.into();
        }
        let header = json!({"typ":"dpop+jwt","alg":"ES256","jwk":self.public_jwk});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?)
        );
        let signature = self
            .key
            .sign(&SystemRandom::new(), signing_input.as_bytes())
            .map_err(|_| anyhow!("DPoP signing failed"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_ref())
        ))
    }

    /// Explicit secret export for the node-local credential vault. The caller
    /// owns permissions, atomic persistence, backups and buffer erasure.
    pub fn export_private_pkcs8(&self) -> Vec<u8> {
        self.pkcs8.clone()
    }

    pub(super) fn stored(&self) -> StoredDpopKey {
        StoredDpopKey {
            company_identifier: self.company_identifier.clone(),
            user_id: self.user_id.clone(),
            pkcs8: URL_SAFE_NO_PAD.encode(&self.pkcs8),
        }
    }

    pub(super) fn restore(stored: StoredDpopKey) -> Result<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(stored.pkcs8)
            .map_err(|_| anyhow!("invalid stored DPoP private key encoding"))?;
        Self::from_pkcs8(&stored.company_identifier, &stored.user_id, &bytes)
    }

    pub(super) fn copy_for_same_grant(&self) -> Result<Self> {
        Self::from_pkcs8(&self.company_identifier, &self.user_id, &self.pkcs8)
    }
}
