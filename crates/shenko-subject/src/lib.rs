//! Continuous subject loop — SPEC §5.1
//! "没有外部消息也继续下一步" — subject never stops for empty queue.
pub mod inbox;
pub mod outbox;
pub mod loop_runner;
pub mod step;
pub mod dispatch;

pub use inbox::*;
pub use outbox::*;
pub use loop_runner::*;
pub use step::*;
pub use dispatch::*;
