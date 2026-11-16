//! Poppy discovery and request identity, independent of the subject and ledger.
//!
//! Discovering a service does not authorize account access. Runtime owners are
//! responsible for installation, durable observations and credential access.

pub mod discovery;
pub mod session;

pub const UNIT_ID: &str = "compat.poppy";

pub mod transport;

pub mod ucp;
pub mod auth;
