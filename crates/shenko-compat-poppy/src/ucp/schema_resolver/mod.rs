//! The upstream pure UCP annotation semantics; see README.md for provenance.

mod error;
mod resolver;
mod types;

pub(super) use resolver::resolve;
pub(super) use types::{Direction, Requires, ResolveOptions};
