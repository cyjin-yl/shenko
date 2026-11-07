//! WASM 运行状态引用 — SPEC §51.4、§10.2.1
//!
//! 认知状态回滚要把**执行中的内部状态**一起带回某个历史内容：线性内存、
//! 执行栈与解释器对象图。这些不是「进程内存」这种不可保存的东西，而是可以被
//! 后端捕获成不可变对象、按摘要复用的一组工件。
//!
//! 捕获与验证由 `shenko-wasm` 的 COW/状态后端负责（它产出
//! `shenko-wasm-state/1` 的 manifest 并在 `seed-wasm-state` namespace 里封存）；
//! 这里只定义**可以嵌进自有版本化根**的引用。引用放在 `shenko-types`，因为
//! `shenko-store` 只依赖本 crate，而根内容必须能原样落账。
//!
//! 捕获不到时不允许伪造：调用方用
//! [`crate::cognitive_root::RuntimeStateParticipant::NotCaptured`] 如实记录缺口。

use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::object::Digest;

/// 产出并重放这份状态的后端身份。
///
/// 同一份字节在不同引擎/ABI 下不保证可重放，所以身份要跟着引用一起走，
/// 而不是靠「大概一样的版本」推断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmBackendIdentity {
    /// 引擎名，例如 `wasmi`。
    pub engine: String,
    /// 引擎版本。
    pub engine_version: String,
    /// 被捕获状态的 guest ABI／状态格式标识。
    pub abi: String,
}

impl WasmBackendIdentity {
    pub fn new(engine: impl Into<String>, engine_version: impl Into<String>, abi: impl Into<String>) -> Self {
        Self {
            engine: engine.into(),
            engine_version: engine_version.into(),
            abi: abi.into(),
        }
    }

    /// 两个身份是否指向同一个可重放后端。
    pub fn same_backend(&self, other: &Self) -> bool {
        self == other
    }
}

/// 一次已经捕获的 WASM 状态。
///
/// `manifest` 是后端的 `WasmStateManifest` 对象：它记录这次捕获的模块、
/// 内存段、栈与对象图的完整清单。`memories` 是线性内存对象的摘要，
/// `module` 是被执行模块的摘要，`instance_id` 是这次实例的身份。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WasmStateRef {
    /// 状态 manifest 的内容摘要（不另存第二份清单）。
    pub manifest: Digest,
    /// 线性内存对象，按段顺序。
    pub memories: Vec<Digest>,
    /// 产出并重放这份状态的后端。
    pub backend: WasmBackendIdentity,
    /// 被执行模块的摘要。
    pub module: Digest,
    /// 这次实例的身份。回滚后仍是同一实例的续体，不是新实例。
    pub instance_id: EntityId,
}

impl WasmStateRef {
    /// 摘要必须是规范 blake3，缺口要当场拒绝而不是留到读取时。
    pub fn validate(&self) -> Result<(), String> {
        self.manifest
            .canonical_blake3_hex()
            .ok_or_else(|| format!("wasm state manifest digest {} is not canonical blake3", self.manifest.0))?;
        self.module
            .canonical_blake3_hex()
            .ok_or_else(|| format!("wasm module digest {} is not canonical blake3", self.module.0))?;
        for memory in &self.memories {
            memory
                .canonical_blake3_hex()
                .ok_or_else(|| format!("wasm memory digest {} is not canonical blake3", memory.0))?;
        }
        if self.backend.engine.is_empty() || self.backend.engine_version.is_empty() || self.backend.abi.is_empty() {
            return Err("wasm backend identity is incomplete".to_string());
        }
        Ok(())
    }
}
