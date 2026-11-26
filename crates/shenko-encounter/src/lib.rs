//! Encounters between Shenko subjects (decision 0008 §4).
//!
//! An encounter is proven by the other subject's public key, never by its
//! name. This crate owns the three pieces every encounter transport shares:
//!
//! - [`SubjectKey`]: the subject's Ed25519 signing key, persisted owner-only;
//! - [`Card`] / [`SignedCard`]: the bounded public card a subject offers, with
//!   only what it chooses to publish (no credentials, memory or owner data);
//! - [`lan`]: the first transport, a signed LAN multicast beacon.
//!
//! Recording encounters as durable facts and opening an IM conversation are
//! the caller's job; this crate returns verified observations only.

pub mod card;
pub mod key;
pub mod lan;

pub use card::{Card, CardError, MAX_DATAGRAM_BYTES, SignedCard};
pub use key::{KeyError, PublicIdentity, SubjectKey};
pub use lan::{Encounter, LAN_GROUP, LAN_PORT, LAN_TRANSPORT, LanBeacon};
