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

pub mod contract;
pub mod journal;
pub mod kernel;
pub mod session;
mod state;

pub use contract::*;
pub use journal::*;
pub use kernel::*;
pub use session::*;
