//! Continuous subject loop — SPEC §5.1
//! "没有外部消息也继续下一步" — subject never stops for empty queue.
pub mod child;
pub mod dispatch;
pub mod inbox;
mod context_projection;
pub mod loop_runner;
pub mod outbox;

pub use child::*;
pub use dispatch::*;
pub use inbox::*;
pub use loop_runner::*;
pub use outbox::*;

pub mod output;
