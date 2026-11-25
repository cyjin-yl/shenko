//! The public card a subject offers at an encounter.

use crate::key::{PublicIdentity, SubjectKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// One UDP datagram must carry the whole signed card without fragmentation.
pub const MAX_DATAGRAM_BYTES: usize = 1200;
const MAX_NAME_CHARS: usize = 64;
const MAX_CAPABILITIES: usize = 16;
const MAX_CAPABILITY_CHARS: usize = 64;
const MAX_ENDPOINT_BYTES: usize = 256;
/// Cards older than this, or dated further than [`MAX_SKEW_SECS`] ahead, are
/// refused so a captured beacon cannot be replayed as a fresh encounter.
pub const MAX_AGE_SECS: i64 = 600;
pub const MAX_SKEW_SECS: i64 = 120;

/// What a subject chooses to publish. It has no field for credentials,
/// memory or the owner's private data, so none can be sent by mistake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub v: u8,
    pub identity: PublicIdentity,
    /// A display label only; identity is the key.
    pub display_name: String,
    /// Summary of offered capabilities, e.g. `"files.read"`.
    pub capabilities: Vec<String>,
    /// Where the subject accepts encounter messages, if anywhere.
    pub endpoint: Option<String>,
    pub issued_at: i64,
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCard {
    /// The exact signed bytes, as JSON text.
    pub card: String,
    /// Hex Ed25519 signature over `card`.
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardError {
    Malformed,
    UnsupportedVersion(u8),
    OutOfBounds(&'static str),
    BadSignature,
    Stale,
    FromFuture,
}

impl fmt::Display for CardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed card"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported card version {v}"),
            Self::OutOfBounds(field) => write!(f, "card field out of bounds: {field}"),
            Self::BadSignature => f.write_str("card signature does not verify"),
            Self::Stale => f.write_str("card is too old"),
            Self::FromFuture => f.write_str("card is dated in the future"),
        }
    }
}

impl std::error::Error for CardError {}

impl Card {
    fn check_bounds(&self) -> Result<(), CardError> {
        if self.v != 1 {
            return Err(CardError::UnsupportedVersion(self.v));
        }
        if self.display_name.chars().count() > MAX_NAME_CHARS {
            return Err(CardError::OutOfBounds("display_name"));
        }
        if self.capabilities.len() > MAX_CAPABILITIES
            || self
                .capabilities
                .iter()
                .any(|c| c.chars().count() > MAX_CAPABILITY_CHARS)
        {
            return Err(CardError::OutOfBounds("capabilities"));
        }
        if self
            .endpoint
            .as_ref()
            .is_some_and(|e| e.len() > MAX_ENDPOINT_BYTES)
        {
            return Err(CardError::OutOfBounds("endpoint"));
        }
        if self.nonce.len() > 64 {
            return Err(CardError::OutOfBounds("nonce"));
        }
        Ok(())
    }

    /// Sign this card with `key`. The card must name `key`'s own identity.
    pub fn sign(&self, key: &SubjectKey) -> Result<SignedCard, CardError> {
        if self.identity != *key.identity() {
            return Err(CardError::BadSignature);
        }
        self.check_bounds()?;
        let card = serde_json::to_string(self).map_err(|_| CardError::Malformed)?;
        let signed = SignedCard {
            signature: hex::encode(key.sign(card.as_bytes())),
            card,
        };
        if signed.to_bytes().len() > MAX_DATAGRAM_BYTES {
            return Err(CardError::OutOfBounds("datagram"));
        }
        Ok(signed)
    }
}

impl SignedCard {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a signed card always serializes")
    }

    /// Parse and verify a received card at time `now` (Unix seconds).
    /// Only a card whose signature verifies under the key it names is accepted.
    pub fn verify(bytes: &[u8], now: i64) -> Result<(Card, String), CardError> {
        if bytes.len() > MAX_DATAGRAM_BYTES {
            return Err(CardError::OutOfBounds("datagram"));
        }
        let signed: SignedCard = serde_json::from_slice(bytes).map_err(|_| CardError::Malformed)?;
        let card: Card = serde_json::from_str(&signed.card).map_err(|_| CardError::Malformed)?;
        card.check_bounds()?;
        let signature = hex::decode(&signed.signature).map_err(|_| CardError::Malformed)?;
        if !card.identity.verify(signed.card.as_bytes(), &signature) {
            return Err(CardError::BadSignature);
        }
        if card.issued_at < now - MAX_AGE_SECS {
            return Err(CardError::Stale);
        }
        if card.issued_at > now + MAX_SKEW_SECS {
            return Err(CardError::FromFuture);
        }
        let digest = hex::encode(Sha256::digest(signed.card.as_bytes()));
        Ok((card, digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> SubjectKey {
        let dir = std::env::temp_dir().join(format!("shenko-card-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SubjectKey::load_or_create(&dir.join("k.pk8")).unwrap()
    }

    fn card(key: &SubjectKey, now: i64) -> Card {
        Card {
            v: 1,
            identity: key.identity().clone(),
            display_name: "Shenko".into(),
            capabilities: vec!["files.read".into()],
            endpoint: None,
            issued_at: now,
            nonce: "n1".into(),
        }
    }

    #[test]
    fn only_untampered_fresh_cards_under_their_own_key_verify() {
        let me = key("me");
        let other = key("other");
        let now = 1_800_000_000;
        let signed = card(&me, now).sign(&me).unwrap();
        let (verified, digest) = SignedCard::verify(&signed.to_bytes(), now + 5).unwrap();
        assert_eq!(verified, card(&me, now));
        assert_eq!(digest.len(), 64);

        // A card naming someone else's identity cannot be signed with my key.
        assert_eq!(card(&other, now).sign(&me), Err(CardError::BadSignature));
        // Changing any signed byte, e.g. the display name, breaks the signature.
        let mut forged = signed.clone();
        forged.card = forged.card.replace("Shenko", "Shenkx");
        assert_eq!(
            SignedCard::verify(&forged.to_bytes(), now),
            Err(CardError::BadSignature)
        );
        // Freshness bounds against replay.
        assert_eq!(
            SignedCard::verify(&signed.to_bytes(), now + MAX_AGE_SECS + 1),
            Err(CardError::Stale)
        );
        assert_eq!(
            SignedCard::verify(&signed.to_bytes(), now - MAX_SKEW_SECS - 1),
            Err(CardError::FromFuture)
        );
        // Bounds hold on both sides.
        let mut long = card(&me, now);
        long.capabilities = vec!["x".into(); MAX_CAPABILITIES + 1];
        assert_eq!(long.sign(&me), Err(CardError::OutOfBounds("capabilities")));
        assert_eq!(
            SignedCard::verify(&[b' '; MAX_DATAGRAM_BYTES + 1], now),
            Err(CardError::OutOfBounds("datagram"))
        );
        assert_eq!(SignedCard::verify(b"{}", now), Err(CardError::Malformed));
    }
}
