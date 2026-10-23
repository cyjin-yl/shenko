//! 对象与内容寻址存储引用 — SPEC §41.2.2, §41.3

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::ids::{EntityId, RecordRevision};

/// 内容摘要，含算法名。
/// 格式: "algo:hexdigest"
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest(pub String);

impl Digest {
    pub fn blake3(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self(format!("blake3:{}", hex::encode(hash.as_bytes())))
    }

    pub fn algorithm(&self) -> &str {
        self.0.split(':').next().unwrap_or("unknown")
    }

    pub fn hex(&self) -> &str {
        self.0.split(':').nth(1).unwrap_or("")
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 对象在存储中的定位。locator 可变化，digest 与内容身份不变。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub digest: Digest,
    pub locator: String,
}

/// schema 引用：名称、主版本、digest。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SchemaRef {
    pub name: String,
    pub major: u32,
    pub digest: Digest,
}

/// 版本化引用：工件 + 符号 + schema。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionedRef {
    pub artifact: ArtifactRef,
    pub symbol: String,
    pub schema: SchemaRef,
}

pub type VersionRef = VersionedRef;

/// 持久对象引用 — 带内容 digest、字节数、media/schema 类型和 store namespace。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub digest: Digest,
    pub bytes: u64,
    pub media_type: String,
    pub store_namespace: String,
    pub locator: String,
    pub codec: SchemaRef,
}

impl ObjectRef {
    pub fn content_id(&self) -> &Digest {
        &self.digest
    }
}

pub type DataRef = ObjectRef;

/// sealed object ref — 对象写入完成并校验后的引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedObjectRef {
    pub object: ObjectRef,
    pub sealed_at: chrono::DateTime<chrono::Utc>,
    pub sealed_commit: crate::commit::CommitRef,
}

/// GC pin 状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GcPin {
    Active,
    Candidate,
    Recovery,
    UserArchive,
    Manual,
}

/// 内容寻址存储 trait。具体实现可以是 SQLite-backed 或文件系统。
pub trait ContentAddressedStore: Send + Sync {
    fn seal_object(
        &self,
        data: &[u8],
        media_type: &str,
        namespace: &str,
    ) -> Result<SealedObjectRef, crate::error::ShenkoError>;

    fn read_object(&self, digest: &Digest) -> Result<Vec<u8>, crate::error::ShenkoError>;

    fn object_exists(&self, digest: &Digest) -> bool;

    fn object_size(&self, digest: &Digest) -> Option<u64>;

    fn delete_unpinned(&self, digest: &Digest) -> Result<(), crate::error::ShenkoError>;
}

/// 闭包验证结果 — SPEC §41.2.2
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosureValidation {
    pub root_revision: RecordRevision,
    pub all_objects_present: bool,
    pub missing_digests: Vec<Digest>,
}

impl ClosureValidation {
    pub fn is_complete(&self) -> bool {
        self.all_objects_present && self.missing_digests.is_empty()
    }
}
