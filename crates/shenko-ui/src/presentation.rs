//! Rich presentation contract — SPEC §21.5–§21.9, R091.
//!
//! The subject's committed public text may carry a fenced ```shenko-ui block
//! holding a versioned SemanticDocument (schema v1). This module validates
//! that document against the declared component versions, degrades it to the
//! client's real capabilities, and binds actions to durable inbox inputs.
//!
//! Honesty boundaries:
//! - An invalid or unsupported document degrades to its declared text
//!   fallback (or the raw block region); it never becomes an executable
//!   surface. Rendering failure is contained to the document region.
//! - Action execution authority never comes from the document. Clicking an
//!   action admits a durable web input (dedup key = document:action:nonce)
//!   into the subject inbox; the subject dispositions it like any input.
//! - Capability facts are negotiated per connection with a revision and a
//!   validity window; a reconnect or client change renegotiates. Unknown
//!   capabilities degrade to the confirmed base form.
//! - Documents are re-derived from committed decision text on replay; no
//!   separate document store exists, so replay cannot diverge from truth.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

/// Current semantic document schema. Bump with an explicit migration note;
/// readers reject unknown versions rather than guessing.
pub const PRESENTATION_SCHEMA_VERSION: u32 = 1;

/// Document node kinds this renderer generation understands. Anything else
/// fails validation → text fallback, never partial execution.
pub const KNOWN_NODE_KINDS: &[&str] = &[
    "section",
    "text",
    "markdown",
    "code",
    "table",
    "compare",
    "select",
    "form",
    "chart",
    "media",
    "actions",
];

/// One semantic document authored by the subject in committed public text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDocument {
    pub schema_version: u32,
    pub document_id: String,
    /// Business revision of the document's meaning. Changing recipients,
    /// targets or semantics requires a new revision; pure re-layout does not.
    pub revision: String,
    pub purpose: String,
    #[serde(default)]
    pub source_refs: Vec<String>,
    #[serde(default)]
    pub audience_task: Option<String>,
    pub fallback: FallbackSpec,
    #[serde(default)]
    pub component_versions: BTreeMap<String, u32>,
    pub nodes: Vec<DocumentNode>,
}

/// What the document reduces to when the surface cannot render it richly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FallbackSpec {
    /// One-line summary preserving the intent (§21.7: degradation keeps
    /// facts, intent and completable operations).
    pub summary: String,
    /// Full plain-text rendering; defaults to the summary when absent.
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentNode {
    pub node_id: String,
    pub kind: String,
    #[serde(default)]
    pub props: serde_json::Value,
    #[serde(default)]
    pub children: Vec<DocumentNode>,
    #[serde(default)]
    pub action: Option<ActionBinding>,
}

/// §21.8 action binding. `payload_hash` is blake3 over the canonical (sorted
/// key) JSON of `payload`; the server recomputes it at submit time and never
/// trusts a client-supplied approval.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionBinding {
    pub action_id: String,
    pub revision: String,
    pub nonce: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub actor_scope: Option<String>,
    pub payload: serde_json::Value,
    pub payload_hash: String,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
}

/// Why a document or action was rejected. Serialized verbatim to the client
/// so failure stays readable; no internal state leaks.
#[derive(Debug, Clone, Serialize)]
pub struct PresentationError {
    pub code: &'static str,
    pub reason: String,
}

impl PresentationError {
    fn new(code: &'static str, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// Canonical JSON: serde_json's default Map is a BTreeMap, so `to_string`
/// yields a stable sorted-key representation for hashing and dedup.
fn canonical(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_owned())
}

pub fn payload_digest(payload: &serde_json::Value) -> String {
    blake3::hash(canonical(payload).as_bytes()).to_hex().to_string()
}

fn object(value: &serde_json::Value, what: &str) -> Result<(), PresentationError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(PresentationError::new("INVALID_NODE_PROPS", format!(
            "{what} must be a JSON object"
        )))
    }
}

fn validate_node(
    node: &DocumentNode,
    seen: &mut std::collections::HashSet<String>,
    path: &str,
) -> Result<(), PresentationError> {
    if node.node_id.is_empty() {
        return Err(PresentationError::new("EMPTY_NODE_ID", format!("{path}: node_id is empty")));
    }
    if !seen.insert(node.node_id.clone()) {
        return Err(PresentationError::new(
            "DUPLICATE_NODE_ID",
            format!("{path}: duplicate node_id {}", node.node_id),
        ));
    }
    if !KNOWN_NODE_KINDS.contains(&node.kind.as_str()) {
        return Err(PresentationError::new(
            "UNKNOWN_NODE_KIND",
            format!("{path}: node kind {} is not supported by this renderer", node.kind),
        ));
    }
    object(&node.props, &format!("{path} props"))?;
    for (index, child) in node.children.iter().enumerate() {
        validate_node(child, seen, &format!("{path}/{index}"))?;
    }
    if let Some(action) = &node.action {
        validate_action_binding(action)
            .map_err(|error| PresentationError::new(error.code, format!("{path}: {}", error.reason)))?;
    }
    Ok(())
}

fn validate_action_binding(action: &ActionBinding) -> Result<(), PresentationError> {
    if action.action_id.is_empty() {
        return Err(PresentationError::new("EMPTY_ACTION_ID", "action_id is empty"));
    }
    if action.revision.is_empty() {
        return Err(PresentationError::new("EMPTY_ACTION_REVISION", "action revision is empty"));
    }
    if action.nonce.is_empty() {
        return Err(PresentationError::new("EMPTY_ACTION_NONCE", "action nonce is empty"));
    }
    let digest = payload_digest(&action.payload);
    if digest != action.payload_hash {
        return Err(PresentationError::new(
            "ACTION_PAYLOAD_HASH_MISMATCH",
            "action payload hash does not match the canonical payload",
        ));
    }
    Ok(())
}

/// Validate a parsed document. Returns the stable document digest used for
/// revision comparisons, or the first rejection reason.
pub fn validate_document(
    document: &SemanticDocument,
) -> Result<String, PresentationError> {
    if document.schema_version != PRESENTATION_SCHEMA_VERSION {
        return Err(PresentationError::new(
            "UNKNOWN_DOCUMENT_SCHEMA",
            format!(
                "document schema_version {} is not supported (this renderer understands v{PRESENTATION_SCHEMA_VERSION})",
                document.schema_version
            ),
        ));
    }
    if document.document_id.is_empty() || document.revision.is_empty() {
        return Err(PresentationError::new(
            "EMPTY_DOCUMENT_IDENTITY",
            "document_id and revision are required",
        ));
    }
    if document.fallback.summary.trim().is_empty() {
        return Err(PresentationError::new(
            "EMPTY_FALLBACK",
            "fallback.summary is required so degradation keeps the intent",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (index, node) in document.nodes.iter().enumerate() {
        validate_node(node, &mut seen, &format!("/nodes/{index}"))?;
    }
    Ok(payload_digest(&serde_json::to_value(document).map_err(|error| {
        PresentationError::new("UNSERIALIZABLE_DOCUMENT", error.to_string())
    })?))
}

/// A document extracted from one committed output, bound to its source facts.
#[derive(Debug, Clone)]
pub struct ExtractedDocument {
    pub document: SemanticDocument,
    pub digest: String,
    pub source_step_id: EntityId,
    pub source_commit: shenko_types::commit::CommitRef,
}

/// Why one ```shenko-ui block could not become a surface. The raw text stays
/// visible; only the rich layer is dropped.
#[derive(Debug, Clone, Serialize)]
pub struct DocumentExtractionError {
    pub reason: String,
    pub source_step_id: EntityId,
    pub source_commit: shenko_types::commit::CommitRef,
}

/// Extract and validate every ```shenko-ui fenced block in committed public
/// text. Malformed blocks yield extraction errors; they never abort the text.
pub fn extract_documents(
    text: &str,
    source_step_id: EntityId,
    source_commit: shenko_types::commit::CommitRef,
) -> (Vec<ExtractedDocument>, Vec<DocumentExtractionError>) {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    let parser = Parser::new_ext(text, Options::empty());
    let mut documents = Vec::new();
    let mut errors = Vec::new();
    let mut in_block = false;
    let mut body = String::new();
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info)))
                if info.trim() == "shenko-ui" =>
            {
                in_block = true;
                body.clear();
            }
            Event::Text(chunk) if in_block => body.push_str(&chunk),
            Event::End(TagEnd::CodeBlock) if in_block => {
                in_block = false;
                match serde_json::from_str::<SemanticDocument>(&body) {
                    Ok(document) => match validate_document(&document) {
                        Ok(digest) => documents.push(ExtractedDocument {
                            document,
                            digest,
                            source_step_id,
                            source_commit,
                        }),
                        Err(error) => errors.push(DocumentExtractionError {
                            reason: format!("{}: {}", error.code, error.reason),
                            source_step_id,
                            source_commit,
                        }),
                    },
                    Err(error) => errors.push(DocumentExtractionError {
                        reason: format!("文档块不是有效的 JSON：{error}"),
                        source_step_id,
                        source_commit,
                    }),
                }
            }
            _ => {}
        }
    }
    (documents, errors)
}

/// Per-surface capability facts with revision and validity (§21.5). Client-
/// reported fields are display facts attested by that connection; server-
/// enforced fields (streaming) come from real wiring, never from the client.
#[derive(Debug, Clone, Serialize)]
pub struct SurfaceCapabilities {
    pub revision: u64,
    pub valid_until: chrono::DateTime<chrono::Utc>,
    pub streaming: bool,
    pub native_cards: bool,
    pub buttons_forms: bool,
    pub executable_components: bool,
    pub tables: bool,
    pub charts: bool,
    pub media: bool,
    pub markdown: MarkdownCaps,
    pub input_methods: Vec<String>,
    /// Identity used for action callbacks on this surface.
    pub callback_identity: String,
    pub message_patch: bool,
    pub size_limit_bytes: u64,
    pub state_persistence: Vec<String>,
    pub cross_device: bool,
    pub viewport: Option<(u32, u32)>,
    pub language: String,
    pub theme: String,
    pub font_scale: f32,
    pub reduced_motion: bool,
    pub component_runtime: String,
    pub resource_scope: String,
    pub low_bandwidth: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct MarkdownCaps {
    pub lists: bool,
    pub tables: bool,
    pub code: bool,
    pub math: bool,
}

/// Client-reported display facts (client_hello). These are attestation-scoped:
/// they describe what THIS connection can display, not permissions.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClientSurfaceFacts {
    pub viewport: Option<(u32, u32)>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub font_scale: Option<f32>,
    #[serde(default)]
    pub reduced_motion: Option<bool>,
    #[serde(default)]
    pub low_bandwidth: Option<bool>,
}

/// Computes the effective capabilities for a surface. `streaming_wired` is a
/// server fact (durable stream sink + provider streaming); everything else
/// merges the client's attested display facts with the Web surface baseline.
pub struct SurfaceCapabilityProvider {
    streaming_wired: bool,
    component_runtime: String,
}

impl SurfaceCapabilityProvider {
    pub fn new(streaming_wired: bool, component_runtime: String) -> Self {
        Self {
            streaming_wired,
            component_runtime,
        }
    }

    pub fn capabilities_for(&self, facts: &ClientSurfaceFacts, revision: u64) -> SurfaceCapabilities {
        let narrow = facts
            .viewport
            .is_some_and(|(width, _)| width < 480);
        let low_bandwidth = facts.low_bandwidth == Some(true);
        SurfaceCapabilities {
            revision,
            valid_until: chrono::Utc::now() + chrono::Duration::seconds(3600),
            streaming: self.streaming_wired && !low_bandwidth,
            // The Web surface genuinely supports these; degradation only
            // kicks in for explicit client constraints.
            native_cards: !narrow,
            buttons_forms: !low_bandwidth,
            executable_components: !low_bandwidth,
            tables: !narrow,
            charts: !low_bandwidth && !narrow,
            media: !low_bandwidth,
            markdown: MarkdownCaps {
                lists: true,
                tables: !narrow,
                code: true,
                math: !narrow,
            },
            input_methods: vec!["text".to_owned()],
            callback_identity: "web_owner_session".to_owned(),
            message_patch: true,
            size_limit_bytes: if low_bandwidth { 16 * 1024 } else { 256 * 1024 },
            state_persistence: vec!["local_draft".to_owned(), "reading_state".to_owned()],
            cross_device: false,
            viewport: facts.viewport,
            language: facts.language.clone().unwrap_or_else(|| "zh".to_owned()),
            theme: facts.theme.clone().unwrap_or_else(|| "dark".to_owned()),
            font_scale: facts.font_scale.unwrap_or(1.0),
            reduced_motion: facts.reduced_motion.unwrap_or(false),
            component_runtime: self.component_runtime.clone(),
            resource_scope: "local".to_owned(),
            low_bandwidth,
        }
    }
}

/// How far a document had to degrade for this surface (§21.7 ladder).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradedMode {
    /// Full node tree with interactive actions.
    Rich,
    /// Structured text + numbered options; actions remain completable.
    Card,
    /// Plain text + numbered options bound to the same action identities.
    Text,
}

#[derive(Debug, Clone, Serialize)]
pub struct DegradedDocument {
    pub mode: DegradedMode,
    pub document_id: String,
    pub revision: String,
    /// Numbered options preserve action identity across degradation: option N
    /// is always the same action_id + nonce, so "选 2" and a button click
    /// address one decision (§21.8).
    pub options: Vec<ActionOption>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Vec<DocumentNode>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionOption {
    pub index: u32,
    pub action_id: String,
    pub nonce: String,
    pub revision: String,
    pub label: String,
    pub required_capabilities: Vec<String>,
    /// The canonical payload bound in the committed document. The client
    /// submits it verbatim; the server re-hashes at admission and never
    /// trusts a client-supplied digest (§21.8).
    pub payload: serde_json::Value,
}

fn collect_options(nodes: &[DocumentNode], options: &mut Vec<ActionOption>) {
    for node in nodes {
        if let Some(action) = &node.action {
            let label = action
                .payload
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| action.payload.get("text").and_then(serde_json::Value::as_str).map(ToOwned::to_owned))
                .unwrap_or_else(|| action.action_id.clone());
            options.push(ActionOption {
                index: options.len() as u32,
                action_id: action.action_id.clone(),
                nonce: action.nonce.clone(),
                revision: action.revision.clone(),
                label,
                required_capabilities: action.required_capabilities.clone(),
            payload: action.payload.clone(),
            });
        }
        collect_options(&node.children, options);
    }
}

/// Degrade a validated document to the surface's real capabilities. Facts,
/// intent and completable operations survive every step of the ladder.
pub fn degrade(document: &SemanticDocument, caps: &SurfaceCapabilities) -> DegradedDocument {
    let mut options = Vec::new();
    collect_options(&document.nodes, &mut options);
    let summary = document.fallback.summary.clone();
    let unsupported_action = options
        .iter()
        .any(|option| option.required_capabilities.iter().any(|required| !capability_present(caps, required)));
    if caps.native_cards && caps.buttons_forms && !unsupported_action {
        DegradedDocument {
            mode: DegradedMode::Rich,
            document_id: document.document_id.clone(),
            revision: document.revision.clone(),
            options,
            summary,
            nodes: Some(document.nodes.clone()),
        }
    } else if caps.native_cards {
        DegradedDocument {
            mode: DegradedMode::Card,
            document_id: document.document_id.clone(),
            revision: document.revision.clone(),
            options,
            summary,
            nodes: None,
        }
    } else {
        DegradedDocument {
            mode: DegradedMode::Text,
            document_id: document.document_id.clone(),
            revision: document.revision.clone(),
            options,
            summary,
            nodes: None,
        }
    }
}

pub fn capability_present(caps: &SurfaceCapabilities, required: &str) -> bool {
    match required {
        "streaming" => caps.streaming,
        "native_cards" => caps.native_cards,
        "buttons_forms" => caps.buttons_forms,
        "executable_components" => caps.executable_components,
        "tables" => caps.tables,
        "charts" => caps.charts,
        "media" => caps.media,
        "markdown" => true,
        _ => false,
    }
}

/// Why an action submission was rejected before durable admission. The client
/// may retry only after correcting the named fact; never blind resend.
#[derive(Debug, Clone)]
pub enum ActionRejection {
    UnknownDocument,
    StaleRevision { current: String },
    UnknownAction,
    NonceMismatch,
    Expired,
    PayloadHashMismatch,
    MissingCapability(String),
    ActorScopeViolation,
}

/// Validate one action submission against the committed document binding.
/// `now_unix` is injected for expiry checks. No durable write happens here;
/// the caller admits the input only when this returns Ok.
pub fn validate_action(
    document: &SemanticDocument,
    action_id: &str,
    revision: &str,
    nonce: &str,
    payload: &serde_json::Value,
    now_unix: i64,
) -> Result<ActionBinding, ActionRejection> {
    if revision != document.revision {
        return Err(ActionRejection::StaleRevision {
            current: document.revision.clone(),
        });
    }
    let mut found: Option<&ActionBinding> = None;
    fn find_in<'a>(nodes: &'a [DocumentNode], action_id: &str, found: &mut Option<&'a ActionBinding>) {
        for node in nodes {
            if let Some(action) = &node.action {
                if action.action_id == action_id {
                    *found = Some(action);
                    return;
                }
            }
            find_in(&node.children, action_id, found);
        }
    }
    find_in(&document.nodes, action_id, &mut found);
    let binding = found.ok_or(ActionRejection::UnknownAction)?;
    if binding.nonce != nonce {
        return Err(ActionRejection::NonceMismatch);
    }
    if let Some(expires_at) = binding.expires_at {
        if now_unix > expires_at {
            return Err(ActionRejection::Expired);
        }
    }
    if payload_digest(payload) != binding.payload_hash {
        return Err(ActionRejection::PayloadHashMismatch);
    }
    Ok(binding.clone())
}

/// Bounded cache of validated documents, keyed by identity + revision.
/// Documents are re-derivable from committed text; this cache only saves
/// re-parsing within a session. Eviction is oldest-first.
pub struct PresentationService {
    cache: parking_lot::Mutex<std::collections::VecDeque<CacheEntry>>,
    capacity: usize,
    capability_provider: SurfaceCapabilityProvider,
}

struct CacheEntry {
    key: String,
    document: SemanticDocument,
    digest: String,
}

impl PresentationService {
    pub fn new(
        capacity: usize,
        capability_provider: SurfaceCapabilityProvider,
    ) -> Arc<Self> {
        Arc::new(Self {
            cache: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            capacity: capacity.max(1),
            capability_provider,
        })
    }

    pub fn capability_provider(&self) -> &SurfaceCapabilityProvider {
        &self.capability_provider
    }

    /// Extract + validate + cache the documents of one committed output.
    pub fn project(
        &self,
        text: &str,
        source_step_id: EntityId,
        source_commit: shenko_types::commit::CommitRef,
    ) -> (Vec<ExtractedDocument>, Vec<DocumentExtractionError>) {
        let (documents, errors) = extract_documents(text, source_step_id, source_commit);
        if !documents.is_empty() {
            let mut cache = self.cache.lock();
            for extracted in &documents {
                let key = format!("{}:{}", extracted.document.document_id, extracted.document.revision);
                if cache.iter().any(|entry| entry.key == key) {
                    continue;
                }
                if cache.len() >= self.capacity {
                    cache.pop_front();
                }
                cache.push_back(CacheEntry {
                    key,
                    document: extracted.document.clone(),
                    digest: extracted.digest.clone(),
                });
            }
        }
        (documents, errors)
    }

    /// Look up a cached validated document by identity + revision.
    pub fn document(&self, document_id: &str, revision: &str) -> Option<SemanticDocument> {
        let key = format!("{document_id}:{revision}");
        self.cache
            .lock()
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.document.clone())
    }
}
