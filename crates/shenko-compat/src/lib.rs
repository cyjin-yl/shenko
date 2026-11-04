//! Shared ledger and installation helpers, not an ecosystem plugin.
//! MCP and Skill are separate crates with independent runtime lifetimes.

pub mod installation;
pub mod artifact;
pub mod recorded;
pub mod runtime;
pub mod usage;

