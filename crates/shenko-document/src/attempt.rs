//! The OXR transport's binding to the host's existing effect ledger.
//!
//! This is not another task store. A host implements the receipt operations using
//! its original OperationRecord/AttemptRecord and append-only CAS receipts.
//! An in-memory implementation cannot provide restart safety.

use crate::{OxrDocument, OxrError, OxrHttpResponse, OxrTaskHandle};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use std::sync::Arc;

/// The original external operation and attempt, assigned by the host before
/// any upload. A deliberate new operation receives new identities even when
/// its source bytes are identical to a previous operation's.
#[derive(Debug, Clone)]
pub struct OxrAttemptContext {
    pub operation_id: EntityId,
    pub attempt_id: EntityId,
    /// Stable, non-secret identity of the account behind the credential.
    pub account_identity: String,
    pub store: Arc<dyn OxrAttemptStore>,
}

/// All inputs that an armed attempt must keep fixed. Page selection is absent:
/// OXR uploads the whole source once, and selected pages share its receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OxrAttemptBinding {
    pub operation_id: EntityId,
    pub attempt_id: EntityId,
    pub account_identity: String,
    pub source_digest: String,
    pub source_bytes_digest: String,
    pub source_name: String,
    pub request_schema_version: u32,
    pub closure_id: String,
    pub scope: Option<String>,
    pub endpoint: String,
    pub model_identity: String,
    pub params_digest: String,
}

impl OxrAttemptBinding {
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let bytes = serde_json::to_vec(self)?;
        Ok(format!("blake3:{}", blake3::hash(&bytes).to_hex()))
    }
}

/// A typed transport observation; the host owns its durable receipt identity
/// and its optional archive of the original bounded HTTP response.
/// A task handle must survive before read-only polling starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OxrAttemptReceipt {
    pub task: Option<OxrTaskHandle>,
    /// `None` is an async acceptance. Errors preserve unknown vs terminal;
    /// an unknown outcome never authorizes a new upload.
    pub outcome: Option<Result<OxrDocument, OxrError>>,
    /// Host reference to a complete, unmodified response body. Historical
    /// typed-only receipts, partial reads and withheld credential echoes have
    /// no reference. The host keeps explicit capture metadata in its receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_output_ref: Option<String>,
}

/// Result of an atomic claim of the original attempt.
#[derive(Debug, Clone)]
pub enum OxrAttemptClaim {
    /// The host just durably armed this attempt. Only this caller may upload.
    Dispatch,
    /// Already armed. No receipt means the external effect is unknown, even
    /// if the original caller was cancelled before it sent anything.
    Replay(Option<OxrAttemptReceipt>),
}

/// A narrow adapter to the host's existing external-operation ledger.
///
/// `claim` must atomically validate the complete binding and durably arm the
/// original operation/attempt BEFORE returning Dispatch. Exactly one caller
/// may receive Dispatch, including across processes and restarts. A different
/// binding for an existing identity is an error, never a new attempt.
///
/// `append` must durably retain evidence before returning the admitted receipt,
/// binding any original response body to that same immutable evidence and
/// returning its reference. It must preserve a known
/// task handle and terminal receipts against later unknown observations. A
/// conflicting terminal receipt remains an explicit conflict, not last-write
/// wins. No method may reset/rearm an existing attempt. Implementations
/// must reject use with a different operation/attempt than their host binding.
/// Synchronous calls keep cancellation out of the small ledger transaction;
/// no network operation belongs inside either transaction.
pub trait OxrAttemptStore: Send + Sync + std::fmt::Debug {
    /// Read an already committed receipt without claiming dispatch. This lets
    /// an offline host materialize a terminal result after interruption.
    fn receipt(&self, binding: &OxrAttemptBinding) -> anyhow::Result<Option<OxrAttemptReceipt>>;

    fn claim(&self, binding: &OxrAttemptBinding) -> anyhow::Result<OxrAttemptClaim>;
    fn append(
        &self,
        binding: &OxrAttemptBinding,
        receipt: &OxrAttemptReceipt,
        response: Option<&OxrHttpResponse>,
    ) -> anyhow::Result<OxrAttemptReceipt>;
}
