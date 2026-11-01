//! Shenko persistent store — SQLite WAL, SPEC §41.2
//! One mutation line, one current writer incarnation per local ledger.
//! Configures: journal_mode=WAL, synchronous=FULL, foreign_keys=ON
pub mod migrations;
pub mod store;
pub mod cas;
pub mod ledger;
pub mod memory;
pub mod identity;
pub mod session_events;
pub mod context_windows;
pub mod raw_session;
pub mod cognition;
pub mod cognition_archive;
pub mod recovery_sources;
pub mod semantic_wait;
pub mod seat;
pub mod runtime_boundary;
pub mod web_input;

pub use store::*;
pub use cas::*;
pub use ledger::*;
pub use memory::*;
pub use session_events::*;
pub use context_windows::*;
pub use raw_session::*;
pub use cognition::*;
pub use cognition_archive::*;
pub use semantic_wait::*;
pub use seat::*;

pub mod external;
pub use external::{InboxAdmission, TaskInvocation};

pub mod child;
pub use child::{NativeTaskClaim, TaskCancellation};

pub mod intents;

/// Channel publication facts, the shared channel operation ledger and the
/// explicit publication-target migration. SPEC §15.3, §35.6, §41.6.
pub mod channel;
pub use channel::{ArmedPublication, ChannelFact, NewChannelFact};

pub mod media;
pub mod quota;
