//! Stateful REPL contract plus the WASI preview1 kernel backend — SPEC §19/§19.1.
//!
//! The contract distinguishes the named kernel runtimes (CPython-WASI,
//! Pyodide, Node-WASM, Bun-WASM, QuickJS-style JS-in-WASM) by their actual
//! runtime/ABI; a runtime that is not bound to a real module reports
//! unavailable instead of borrowing another runtime's execution. The shipped
//! backend executes each cell as one real WASI preview1 command through
//! `shenko-wasm` with explicit argv/environment/stdin/stdout/stderr, journals
//! every cell for R0 recovery, and reports — never performs — interrupt,
//! dependency installation and in-VM state persistence it cannot implement.
//!
//! [`entry`] is the durable *binding* half of §19.1's replaceable kernel
//! plugins: a named entry pins one real interpreter artifact (and its
//! standard library, where the route has one) by digest, so a caller — or a
//! restarted node — can list what this installation actually has and create a
//! session from the name instead of re-supplying the interpreter path.

pub mod adapters;
pub mod contract;
pub mod entry;
pub mod install;
pub mod journal;
pub mod kernel;
pub mod live;
pub mod session;
pub mod task;
mod state;

pub use contract::*;
pub use journal::*;
pub use kernel::*;
pub use session::*;
