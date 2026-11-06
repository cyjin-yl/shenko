//! Forgetting plugin — SPEC §51.7 six ordered projection stages.
//!
//! Forgetting is a judgement the subject renders about how fully a piece of
//! content should be surfaced. The six stages —
//!
//!  `Degraded → Fuzzy → NearForgotten → Archived → LongTerm → Forgotten`
//!
//! — are written as [`AttitudeRole::Projection(ProjectionStage)`] attitudes
//! via the canonical atoms API. This crate supplies the machinery, never the
//! formula:
//!
//! * **Stages and stage order** are fixed (the enum in `shenko-types`).
//! * **Weights and principles** are revisable content: they are written via
//!   `record_content_attitude` with `AttitudeRole::Definition("forgetting")`
//!   and can be revised at any time.
//! * **Usage and reference signals** from the subject loop and other domains
//!   are delivered as atomic notices via [`ForgettingSignal`].
//! * **Actual stage decisions** remain subject judgement: the plugin
//!   gathers the signals and active-runtime measurement, prepares a
//!   [`ForgettingContext`], and the subject decides whether to advance a
//!   stage. The plugin records the result as an attitude.
//! * A `Forgotten` stage is a default-recall exclusion, never a disable:
//!   the cold index and archive are always available for explicit version
//!   reads that preserve bytes (SPEC §51.7).
//! * Raw text, audio and frames (R111) default to retention: storage
//!   pressure alone may archive or move to long-term, but never command raw
//!   delete. The pool-no-file-delete rule remains (SPEC §39.4).
//!
//! The forgetting macro is installable as a `cap.forgetting` contract,
//! replaceable and recommended by `humanlike-mind`; taste controls display,
//! not forget-timing (SPEC §51.8).

pub mod signals;
pub mod stages;

pub use signals::{ForgettingSignal, ForgettingSignalKind, SignalBatch};
pub use stages::{
    ForgettingContext, ForgettingMacro, ForgettingPlugin, ProjectionRecord,
    StageAdvanceOutcome,
};
