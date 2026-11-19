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

/// Request encoding: deployment permission and subject cache partition are
/// separate facts. Definition v1 remains the native capability version.
pub const DOCUMENT_PARSE_REQUEST_SCHEMA_VERSION: u32 = 2;

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
    /// Schema marker; always `DOCUMENT_PARSE_REQUEST_SCHEMA_VERSION`.
    pub schema_version: u32,
    /// CAS/object digest of the source document bytes — the result is bound
    /// to this exact input, never to a path that could be swapped.
    pub source_digest: String,
    /// Optional original filename. A provider that uploads the document
    /// (OXR) also uses it as the upload name, because the upstream service
    /// infers the document type from it.
    #[serde(default)]
    pub source_name: Option<String>,
    /// Pages to parse; empty = the whole document.
    #[serde(default)]
    pub pages: Vec<PageRef>,
    /// The provider closure identity (provider id + model + config version).
    /// A cache result is only valid under the exact same closure.
    pub closure_id: String,
    /// Subject/account cache partition. This does not grant deployment
    /// permission and is never parsed to derive that permission.
    pub cache_scope: Option<String>,
    /// Exact deployment permission scope. `None` matches only an explicitly
    /// unscoped provider, never an arbitrary bound scope or an empty string.
    pub deployment_scope: Option<String>,
}

/// One parsed text/layout block on a page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageBlock {
    /// Reading-order index within the page.
    pub order: u32,
    /// The block's text content.
    pub text: String,
    /// Bounding box in the provider's reported page space. The page's
    /// `coordinates_restored` separately states whether a source transform
    /// was established. Absent = unknown; never fabricated.
    #[serde(default)]
    pub bbox: Option<[f32; 4]>,
    /// Optional reference into a provider table/figure structure.
    #[serde(default)]
    pub structure_ref: Option<String>,
    /// The provider's own element-type token for this block. OXR upstream
    /// has a closed vocabulary of layout elements (title, text, table,
    /// figure, formula, code block, chemical structure, ...), and the exact
    /// spelling differs between the OXR-native output ("Page-header") and
    /// the SoMark-compatible API ("header"). The token is therefore carried
    /// verbatim: absent = the provider reported none, and a token this build
    /// does not recognize is kept as-is instead of being dropped or renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_type: Option<String>,
    /// The content format of this block's payload. Upstream values include
    /// `text`, `latex`, `mathml`, `ascii`, `html`, `markdown` and `image`.
    /// Carried verbatim; absent = unknown, never inferred from the element
    /// type (a formula rendered as `latex` and a table rendered as `html`
    /// are different facts the provider must state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_format: Option<String>,
    /// Provider-reported image resource, with its actual retention status.
    /// Absence includes historical checkpoints which did not record image
    /// resources; it never proves that image bytes were retained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_resource: Option<PageImageResource>,
}

/// An image reference is evidence of a provider resource, not of saved bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "retention", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageImageResource {
    /// The provider's URL is preserved verbatim. No resource bytes were
    /// fetched or retained in CAS; the URL may expire or be unreachable.
    TransientUnretained { provider_url: String },
}

/// Dimensions of the provider's output page rectangle, in the same reported
/// space as its bboxes. The unit and transform to source PDF points/image
/// pixels are not established by dimensions alone.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSize {
    /// Page width in the provider's coordinate unit.
    pub width: f32,
    /// Page height in the provider's coordinate unit.
    pub height: f32,
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
    /// `false` includes retained provider-space coordinates whose transform
    /// to the original source has not been established.
    pub coordinates_restored: bool,
    /// Dimensions of the provider's output page rectangle, when reported.
    /// These do not prove a transform to the original source or that every
    /// block has a bbox. Absent = no reported dimensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_size: Option<PageSize>,
}

/// A durable per-page checkpoint: a page that is done (or definitively
/// failed) under one closure, resumable across cancel/restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageCheckpoint {
    /// The cache key binding (source+page+closure+both scopes+schema).
    pub cache_key: String,
    /// The completed page result, when it succeeded.
    #[serde(default)]
    pub result: Option<PageResult>,
    /// The page's terminal failure reason, when it failed.
    #[serde(default)]
    pub failure: Option<String>,
    /// The page's outcome is UNKNOWN: the request may or may not have been
    /// processed upstream. This is deliberately NOT a terminal failure — the
    /// page may be retried under the same cache key, but the caller has to
    /// reconcile the unknown before re-sending, because an unknown page and
    /// a failed page are different facts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_outcome: Option<String>,
    /// When this checkpoint was committed.
    pub committed_at: chrono::DateTime<chrono::Utc>,
}

/// Request-level observation, including discovery before any original page
/// count is known. Durable consumers seal this on the original attempt; no
/// synthetic page number or empty successful document represents an Unknown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestCheckpoint {
    pub request_id: EntityId,
    pub state: ParseState,
    pub reason: String,
    pub observed_at: chrono::DateTime<chrono::Utc>,
}

/// A parse request's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseState {
    /// Admitted; pages not yet all checkpointed.
    Running,
    /// Every requested page has a checkpoint (success or terminal failure).
    Complete,
    /// The request itself was definitively rejected, before page discovery.
    Failed,
    /// The provider could not determine the document's page count, so no
    /// page was claimed. The boundary never guesses a page count.
    PageCountUnknown,
    /// Cancelled by the subject; checkpoints retained for resume.
    Cancelled,
    /// The provider could not run at all (no engine/model/runtime).
    ProviderUnavailable,
}

/// Derive the cache key that binds a page result to its exact inputs.
/// Same source bytes/name/page/closure/deployment scope/cache partition/schema
/// → same key. Upload name affects interpretation; neither scope is inferred
/// from the other. This v2 encoding is separate from recorded v1 page keys.
pub fn page_cache_key(req: &ParseRequest, page: PageRef) -> String {
    let identity = serde_json::to_vec(&(
        "document.parse/page/v2", req.schema_version, &req.source_digest, &req.source_name,
        page.page_index, &req.closure_id, &req.cache_scope, &req.deployment_scope,
    )).expect("page identity serializes");
    let hash = blake3::hash(&identity);
    format!("doc-parse:{}", hash.to_hex())
}

/// The operation's durable request id, deterministic on the full request.
pub fn parse_request_id(req: &ParseRequest) -> EntityId {
    let canonical = serde_json::to_vec(req).expect("request serializes");
    EntityId::derived("document.parse/request/v2", &canonical)
}
