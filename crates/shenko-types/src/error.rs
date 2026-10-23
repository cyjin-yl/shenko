//! 协议错误 — SPEC §41.3
//! ProtocolError 携带 code、scope、retry 策略和 details。

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::commit::CommitRef;
use crate::ids::EntityId;

/// 错误 scope
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorScope {
    Call,
    Task,
    Operation,
    Request,
    Activation,
    Writer,
    Seat,
    Inbox,
}

/// 重试策略略
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryStrategy {
    SameKey,
    AfterReconcile,
    NewRevision,
    NeverAutomatically,
}

/// 规范协议错误
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolError {
    pub code: String,
    pub scope: ErrorScope,
    pub retry: RetryStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<EntityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_ref: Option<CommitRef>,
    pub details: serde_json::Value,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.scope_as_str(), self.code)
    }
}

impl ProtocolError {
    pub fn scope_as_str(&self) -> &'static str {
        match self.scope {
            ErrorScope::Call => "call",
            ErrorScope::Task => "task",
            ErrorScope::Operation => "operation",
            ErrorScope::Request => "request",
            ErrorScope::Activation => "activation",
            ErrorScope::Writer => "writer",
            ErrorScope::Seat => "seat",
            ErrorScope::Inbox => "inbox",
        }
    }

    pub fn new(code: impl Into<String>, scope: ErrorScope, retry: RetryStrategy) -> Self {
        Self {
            code: code.into(),
            scope,
            retry,
            operation_id: None,
            commit_ref: None,
            details: serde_json::Value::Null,
        }
    }

    pub fn with_operation(mut self, id: EntityId) -> Self {
        self.operation_id = Some(id);
        self
    }

    pub fn with_commit(mut self, ref_: CommitRef) -> Self {
        self.commit_ref = Some(ref_);
        self
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

impl std::error::Error for ProtocolError {}

/// 顶层错误类型
#[derive(Debug, thiserror::Error)]
pub enum ShenkoError {
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("object closure incomplete: missing {missing_count} object(s)")]
    ObjectClosureIncomplete { missing_count: usize },

    #[error("resnapshot required: {reason}")]
    ResnapshotRequired { reason: String },

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl ShenkoError {
    pub fn storage(msg: impl Into<String>) -> Self {
        Self::Storage(msg.into())
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }

    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
