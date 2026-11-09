//! `document.parse` definition v1 — the replaceable boundary (issue #89).
//!
//! These types describe one on-demand document-parse operation and the
//! durable, per-page evidence it produces. They are provider-agnostic by
//! contract: OXR is the first named provider, but the boundary never embeds
//! engine names into the result identity — a result records *which provider
//! produced it* as evidence, and a missing provider is an honest
//! `ProviderUnavailable`, never a substituted engine.

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

/// The definition version this build encodes.
pub const DOCUMENT_PARSE_DEFINITION_VERSION: u32 = 1;

/// One page of one source document, addressed by its original index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PageRef {
    /// Zero-based index into the source document's own page order.
    pub page_index: u32,
}

/// One parse request: which source and which pages, under which closure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParseRequest {
    /// Schema marker; always `DOCUMENT_PARSE_DEFINITION_VERSION`.
    pub schema_version: u32,
    /// CAS/object digest of the source document bytes — the result is bound
    /// to this exact input, never to a path that could be swapped.
    pub source_digest: String,
    /// Optional original filename for provider-side logging only.
    #[serde(default)]
    pub source_name: Option<String>,
    /// Pages to parse; empty = the whole document.
    #[serde(default)]
    pub pages: Vec<PageRef>,
    /// The provider closure identity (provider id + model + config version).
    /// A cache result is only valid under the exact same closure.
    pub closure_id: String,
    /// Permission/scope binding: results from a different scope never mix.
    #[serde(default)]
    pub scope: Option<String>,
}

/// One parsed text/layout block on a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageBlock {
    /// Reading-order index within the page.
    pub order: u32,
    /// The block's text content.
    pub text: String,
    /// Bounding box in the ORIGINAL page coordinate space, if the provider
    /// could restore it. Absent = unknown; never fabricated.
    #[serde(default)]
    pub bbox: Option<[f32; 4]>,
    /// Optional reference into a provider table/figure structure.
    #[serde(default)]
    pub structure_ref: Option<String>,
}

/// The provider/model/config version evidence for one page's parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageProvenance {
    /// Provider identity (e.g. "oxr"); evidence, not a cache key.
    pub provider_id: String,
    /// Provider code version/digest.
    pub code_version: String,
    /// Model weight version/digest, when the provider is model-backed.
    #[serde(default)]
    pub model_version: Option<String>,
    /// Provider parameter/config digest.
    #[serde(default)]
    pub params_digest: Option<String>,
    /// Reference to the provider's raw output record (CAS), when retained.
    #[serde(default)]
    pub raw_output_ref: Option<String>,
}

/// One page's parse result, with its provenance and restore status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageResult {
    pub page: PageRef,
    /// Digest of the source this page came from (echo of request binding).
    pub source_digest: String,
    /// Parsed blocks in reading order.
    pub blocks: Vec<PageBlock>,
    /// Provider/version evidence for this page.
    pub provenance: PageProvenance,
    /// Whether block bboxes were restored to original-page coordinates.
    /// `false` = coordinates unknown/absent; never a guess.
    pub coordinates_restored: bool,
}

/// A durable per-page checkpoint: a page that is done (or definitively
/// failed) under one closure, resumable across cancel/restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageCheckpoint {
    /// The cache key binding (source+page+closure+scope+schema).
    pub cache_key: String,
    /// The completed page result, when it succeeded.
    #[serde(default)]
    pub result: Option<PageResult>,
    /// The page's terminal failure reason, when it failed.
    #[serde(default)]
    pub failure: Option<String>,
    /// When this checkpoint was committed.
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

/// A parse request's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseState {
    /// Admitted; pages not yet all checkpointed.
    Running,
    /// Every requested page has a checkpoint (success or terminal failure).
    Complete,
    /// Cancelled by the subject; checkpoints retained for resume.
    Cancelled,
    /// The provider could not run at all (no engine/model/runtime).
    ProviderUnavailable,
}

/// Derive the cache key that binds a page result to its exact inputs.
/// Same `(source_digest, page_index, closure_id, scope, schema)` → same key;
/// any version/scope change yields a different key, never a mixed result.
pub fn page_cache_key(req: &ParseRequest, page: PageRef) -> String {
    let scope = req.scope.as_deref().unwrap_or("");
    let identity = format!(
        "document.parse/page/v{}:{}:{}:{}:{}",
        DOCUMENT_PARSE_DEFINITION_VERSION,
        req.source_digest,
        page.page_index,
        req.closure_id,
        scope,
    );
    let hash = blake3::hash(identity.as_bytes());
    format!("doc-parse:{}", hash.to_hex())
}

/// The operation's durable request id, deterministic on the full request.
pub fn parse_request_id(req: &ParseRequest) -> EntityId {
    let canonical = serde_json::to_vec(req).expect("request serializes");
    EntityId::derived("document.parse/request/v1", &canonical)
}
