//! Shenko 核心协议类型 — 对应 SPEC §41.3
//! 所有跨模块协议类型定义于此，无业务逻辑。

pub mod ids;
pub mod commit;
pub mod task;
pub mod operation;
pub mod evidence;
pub mod store;
pub mod object;
pub mod agent;
pub mod subject;
pub mod error;

pub use ids::*;
pub use commit::*;
pub use task::*;
pub use operation::*;
pub use evidence::*;
pub use store::*;
pub use object::*;
pub use agent::*;
pub use subject::*;
pub use error::*;
