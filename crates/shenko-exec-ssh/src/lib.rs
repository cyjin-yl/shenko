//! SSH and tmux seat execution — SPEC §11, §17, §35.5
mod executor;
pub mod ssh_exec;
pub use ssh_exec::*;
