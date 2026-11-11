//! SSH and tmux seat execution — docs/specs/remote-seats.md#spec-11, docs/specs/remote-seats.md#spec-17, docs/specs/remote-seats.md#spec-35-5
mod executor;
pub mod ssh_exec;
pub use ssh_exec::*;

pub mod task_runner;
pub use task_runner::*;

mod selected_task;
pub use selected_task::*;

mod target_resolution;
pub use target_resolution::*;

mod permission_provider;
pub use permission_provider::*;

mod installed_binding;
pub use installed_binding::*;

/// Native protocol adapter only; not installed task authority or a sandbox.
pub mod native_transport;

mod native_installation;
pub use native_installation::*;

mod identity;
pub use identity::*;

/// Terminal screen state machine for seat observation (docs/specs/remote-seats.md#spec-17-5).
pub mod terminal;
pub use terminal::*;

/// Seat adapters: tmux, managed OMP JSONL RPC, managed Claude Code.
pub mod seat;
pub use seat::*;

/// Installed native task authority for SSH exec/PTY and seat observe/respond.
pub mod native;
pub use native::*;
