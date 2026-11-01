use serde::{Deserialize, Serialize};
use shenko_types::ids::{ControllerEpoch, EntityId};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::oneshot;

use crate::anthropic_rules::ProviderCitation;
use crate::profile::ProviderProfile;

pub(crate) const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Non-secret effective transport/model contract, fixed by broker construction.
/// It is sealed into S0; a recovered request must not route through a new binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderBinding {
    /// Explicitly unavailable offline binding. It never fabricates a model result.
    Stub { codec: u32 },
    Local {
        base_url: String,
        model: String,
        allow_invalid_certs: bool,
        serializer_revision: u32,
        default_max_tokens: u64,
    },
    Anthropic {
        base_url: String,
        model: String,
        api_version: String,
        /// Non-secret blake3 hex of the exact credential that was configured
        /// when the binding froze. Two unrelated accounts on the same
        /// endpoint/model produce different fingerprints, so a recovered
        /// pending request cannot silently resume under a different account.
        /// The credential itself is never stored or logged.
        #[serde(default)]
        credential_fingerprint: Option<String>,
        serializer_revision: u32,
        default_max_tokens: u64,
    },
    /// An open profile installed by a provider plugin.
    ///
    /// The whole profile is frozen into the seal: transport, endpoint, model,
    /// API version, SDK closure, sampling capabilities, auth method, usage
    /// mapping and the credential fingerprint. A recovered request therefore
    /// keeps the exact identity it was sent with, and a provider that changed
    /// after the seal cannot silently reinterpret it. The historical
    /// `local`, `anthropic` and `stub` bindings stay readable as they were.
    Profile { profile: ProviderProfile },
}

impl ProviderBinding {
    /// Select another model on the same configured transport. This creates a
    /// new binding; it never edits an existing request seal or its endpoint.
    pub fn with_model(&self, selected: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !selected.trim().is_empty() && selected.len() <= 512
                && !selected.chars().any(char::is_control),
            "invalid selected model"
        );
        let mut binding = self.clone();
        match &mut binding {
            Self::Stub { .. } => anyhow::bail!("unavailable provider has no selectable model"),
            Self::Local { model, .. } | Self::Anthropic { model, .. } => {
                selected.clone_into(model);
            }
            Self::Profile { profile } => {
                selected.clone_into(&mut profile.model_id);
            }
        }
        Ok(binding)
    }

    pub fn model(&self) -> &str {
        match self {
            Self::Stub { .. } => "stub",
            Self::Local { model, .. } | Self::Anthropic { model, .. } => model,
            Self::Profile { profile } => &profile.model_id,
        }
    }

    pub fn profile_id(&self) -> &str {
        match self {
            Self::Stub { .. } => "provider.unavailable",
            Self::Local { .. } => "local.openai.chat_completions",
            Self::Anthropic { .. } => "anthropic.messages",
            Self::Profile { profile } => &profile.profile_id,
        }
    }

    pub fn profile_revision(&self) -> u32 {
        match self {
            Self::Stub { codec }
            | Self::Local {
                serializer_revision: codec,
                ..
            }
            | Self::Anthropic {
                serializer_revision: codec,
                ..
            } => *codec,
            Self::Profile { profile } => profile.revision,
        }
    }

    pub fn api_version(&self) -> Option<&str> {
        match self {
            Self::Anthropic { api_version, .. } => Some(api_version),
            Self::Profile { profile } => profile.api_version.as_deref(),
            Self::Stub { .. } | Self::Local { .. } => None,
        }
    }

    pub fn default_max_tokens(&self) -> u64 {
        match self {
            Self::Stub { .. } => DEFAULT_MAX_TOKENS,
            Self::Local {
                default_max_tokens, ..
            }
            | Self::Anthropic {
                default_max_tokens, ..
            } => *default_max_tokens,
            Self::Profile { profile } => profile.default_max_tokens,
        }
    }

    /// The open profile of this binding, when it has one.
    pub fn profile(&self) -> Option<&ProviderProfile> {
        match self {
            Self::Profile { profile } => Some(profile),
            _ => None,
        }
    }

    /// The non-secret fingerprint of the credential this binding uses.
    ///
    /// Two accounts on one endpoint are two different bindings, so the
    /// fingerprint is the account identity the runtime facts are bound to.
    pub fn credential_fingerprint(&self) -> Option<&str> {
        match self {
            Self::Anthropic { credential_fingerprint, .. } => credential_fingerprint.as_deref(),
            Self::Profile { profile } => profile.auth.credential_fingerprint.as_deref(),
            Self::Stub { .. } | Self::Local { .. } => None,
        }
    }

    /// Whether two bindings reach the same transport with the same identity,
    /// ignoring the model. One runtime serves the models of its own
    /// transport; the sealed request still names the exact model.
    pub fn same_transport(&self, other: &Self) -> bool {
        match self {
            Self::Stub { .. } => matches!(other, Self::Stub { .. }),
            Self::Local {
                base_url,
                allow_invalid_certs,
                serializer_revision,
                default_max_tokens,
                ..
            } => match other {
                Self::Local {
                    base_url: other_url,
                    allow_invalid_certs: other_certs,
                    serializer_revision: other_revision,
                    default_max_tokens: other_tokens,
                    ..
                } => {
                    base_url == other_url
                        && allow_invalid_certs == other_certs
                        && serializer_revision == other_revision
                        && default_max_tokens == other_tokens
                }
                _ => false,
            },
            Self::Anthropic {
                base_url,
                api_version,
                credential_fingerprint,
                serializer_revision,
                default_max_tokens,
                ..
            } => match other {
                Self::Anthropic {
                    base_url: other_url,
                    api_version: other_version,
                    credential_fingerprint: other_fingerprint,
                    serializer_revision: other_revision,
                    default_max_tokens: other_tokens,
                    ..
                } => {
                    base_url == other_url
                        && api_version == other_version
                        && credential_fingerprint == other_fingerprint
                        && serializer_revision == other_revision
                        && default_max_tokens == other_tokens
                }
                _ => false,
            },
            Self::Profile { profile } => match other {
                Self::Profile { profile: other_profile } => {
                    profile.matches_except_model(other_profile)
                }
                _ => false,
            },
        }
    }

    pub(crate) fn validate_endpoint(&self) -> anyhow::Result<()> {
        let endpoint = match self {
            Self::Stub { .. } => return Ok(()),
            Self::Local { base_url, .. } | Self::Anthropic { base_url, .. } => base_url,
            Self::Profile { profile } => {
                profile.validate()?;
                return Ok(());
            }
        };
        let url = reqwest::Url::parse(endpoint)
            .map_err(|_| anyhow::anyhow!("invalid provider endpoint"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider endpoint must be HTTP(S) without credentials/query/fragment; use separate credentials"
        );
        Ok(())
    }
}

/// Advertised capabilities of the exact bound model, not a physical capacity
/// measurement. Missing metadata stays unknown rather than inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub binding: ProviderBinding,
    pub max_context_tokens: Option<u32>,
    pub vision: Option<ProviderVisionCaps>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderVisionCaps {
    pub max_image_context: u32,
    pub patch_px: u32,
}

pub struct InferenceRequest {
    pub request_id: EntityId,
    pub agent_id: EntityId,
    pub conversation_id: EntityId,
    pub provider_binding: ProviderBinding,
    pub priority: InferencePriority,
    pub epoch: ControllerEpoch,
    pub payload: InferencePayload,
    pub result_tx: oneshot::Sender<InferenceResult>,
}

/// Why one controlled call is made. Every controlled consumer — the subject,
/// a child, memory work, compaction and diagnostics — enters the same
/// broker, and the purpose keeps their accounting apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestPurpose {
    Subject,
    Child,
    Memory,
    Compact,
    Diagnostics,
    Discovery,
    /// An external interruption handled by the subject.
    External,
}

impl RequestPurpose {
    /// The purpose a request of this priority belongs to.
    pub fn from_priority(priority: InferencePriority) -> Self {
        match priority {
            InferencePriority::Subject => Self::Subject,
            InferencePriority::Urgent => Self::External,
            InferencePriority::Normal | InferencePriority::Low => Self::Child,
 }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Subject => "subject",
            Self::Child => "child",
            Self::Memory => "memory",
            Self::Compact => "compact",
            Self::Diagnostics => "diagnostics",
            Self::Discovery => "discovery",
            Self::External => "external",
        }
    }
}

/// A controlled call with its declared purpose. The request itself is
/// unchanged, so existing callers keep constructing it exactly as before.
pub struct BrokeredCall {
    pub purpose: RequestPurpose,
    pub request: InferenceRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePriority {
    Low = 0,
    Normal = 1,
    Subject = 2,
    Urgent = 3,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InferencePayload {
    pub system_prompt: String,
    pub messages: Vec<InferenceMessage>,
    pub max_tokens: Option<u64>,
    pub tools: Vec<serde_json::Value>,
    /// Provider-native semantic options fixed at the request seal. Reserved
    /// envelope fields are rejected rather than overriding the frozen binding.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native_options: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferenceMessage {
    pub role: String,
    /// Ordered native parts. This is the only content shape: a text-only
    /// message and a message carrying an image use the same list, so the
    /// order of the parts is the order of the wire blocks.
    pub parts: Vec<InferencePart>,
}

impl InferenceMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            parts: vec![InferencePart::text(text)],
        }
    }
}

/// One ordered part of a message body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InferencePart {
    Text {
        text: String,
    },
    /// Image bytes are shared immutably. Cloning a payload or a request never
    /// copies them; the bytes are encoded only at the provider wire boundary.
    Image {
        media_type: String,
        bytes: Arc<[u8]>,
    },
    /// A document block. Documents carry their source and may enable
    /// citations on the blocks that reference them.
    Document {
        source: String,
        #[serde(default)]
        media_type: Option<String>,
        #[serde(default)]
        title: Option<String>,
        /// Whether the backend may attach citations to this document.
        #[serde(default)]
        citations_enabled: bool,
    },
    /// A citation attached to a text block.
    Citation {
        text: String,
        #[serde(default)]
        cited_text: String,
        #[serde(default)]
        document_index: Option<u32>,
        #[serde(default)]
        document_title: Option<String>,
    },
    /// A search result block with its source text.
    SearchResult {
        source: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        content: String,
        #[serde(default)]
        citations_enabled: bool,
    },
}

impl InferencePart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image(media_type: impl Into<String>, bytes: Arc<[u8]>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            bytes,
        }
    }

    pub fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }
}

/// Media types both real transports carry as an image part.
pub(crate) const SUPPORTED_IMAGE_MEDIA_TYPES: &[&str] =
    &["image/gif", "image/jpeg", "image/png", "image/webp"];

/// Minimal image validation: a supported media type and actual bytes. It says
/// nothing about what a model can perceive.
pub(crate) fn validate_image_part(media_type: &str, bytes: &[u8]) -> Result<(), ProviderWireError> {
    if !SUPPORTED_IMAGE_MEDIA_TYPES.contains(&media_type) {
        return Err(ProviderWireError::InvalidImagePart(format!(
            "unsupported image media type {media_type:?}"
        )));
    }
    if bytes.is_empty() {
        return Err(ProviderWireError::InvalidImagePart(
            "image part carries no bytes".to_string(),
        ));
    }
    Ok(())
}

/// Standard base64 of the shared image bytes, as both transports require.
pub(crate) fn base64_image_bytes(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The exact non-secret provider/profile interpretation used for one result.
/// Credentials and private key material are deliberately absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveInference {
    pub provider_binding: ProviderBinding,
    pub profile_id: String,
    pub profile_revision: u32,
    pub api_version: Option<String>,
    pub max_tokens: u64,
    pub native_options: BTreeMap<String, serde_json::Value>,
}

impl EffectiveInference {
    pub fn matches_seal(&self, binding: &ProviderBinding, payload: &InferencePayload) -> bool {
        self.provider_binding == *binding
            && self.profile_id == binding.profile_id()
            && self.profile_revision == binding.profile_revision()
            && self.api_version.as_deref() == binding.api_version()
            && self.max_tokens
                == payload
                    .max_tokens
                    .unwrap_or_else(|| binding.default_max_tokens())
            && self.native_options == payload.native_options
    }

    pub(crate) fn for_request(req: &InferenceRequest) -> Self {
        Self {
            profile_id: req.provider_binding.profile_id().into(),
            profile_revision: req.provider_binding.profile_revision(),
            api_version: req.provider_binding.api_version().map(str::to_owned),
            max_tokens: req
                .payload
                .max_tokens
                .unwrap_or_else(|| req.provider_binding.default_max_tokens()),
            provider_binding: req.provider_binding.clone(),
            native_options: req.payload.native_options.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResult {
    pub request_id: EntityId,
    pub status: InferenceStatus,
    pub response: Option<InferenceResponse>,
    pub effective: EffectiveInference,
    pub termination: TerminationEvidence,
    /// Why this controlled call was made.
    pub purpose: RequestPurpose,
    /// The stream evidence of a streamed call. `None` for a call that was not
    /// streamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamEvidence>,
    /// Provider rate-limit headers observed alongside this result, including
    /// on rejections. They update quota observations from ordinary traffic;
    /// no paid probe is generated for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota_headers: Vec<(String, String)>,
}

/// What a streamed call actually produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamEvidence {
    /// Provider events observed on the wire.
    pub events: u64,
    /// Batches handed to the durable journal.
    pub batches: u64,
    /// Public text chunks handed to the reader surface.
    pub public_chunks: u64,
    /// Whether a durable journal was installed for this call.
    pub journaled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceStatus {
    Success,
    /// Outcome/remote execution is unknown. Never retry based on this alone.
    Failed,
    Cancelled,
    /// Broker did not dispatch this request. Retained for explicit capacity
    /// rejection during shutdown/recovery; a running broker keeps queued work.
    QueuedCapacityWait,
    /// No provider is configured; this is not a synthetic successful response.
    Unavailable,
    /// Explicit provider rejection, not inferred from error text.
    Rejected {
        http_status: u16,
    },
    /// Local validation failed before dispatch. A changed model/profile/native
    /// option requires a new sealed request revision, not a silent wire rewrite.
    InvalidRequest {
        code: WireErrorCode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    ProviderBindingMismatch,
    ReservedNativeOption,
    ForbiddenSampling,
    InvalidImagePart,
    /// The message or option violates the rules of the bound API version.
    MessageRuleViolation,
}

impl WireErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderBindingMismatch => "MODEL_PROVIDER_BINDING_MISMATCH",
            Self::ReservedNativeOption => "PROVIDER_NATIVE_OPTION_RESERVED",
            Self::ForbiddenSampling => "PROVIDER_SAMPLING_FORBIDDEN",
            Self::InvalidImagePart => "PROVIDER_IMAGE_PART_INVALID",
            Self::MessageRuleViolation => "PROVIDER_MESSAGE_RULE_VIOLATION",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderWireError {
    #[error("MODEL_PROVIDER_BINDING_MISMATCH: frozen request differs from executing provider")]
    BindingMismatch,
    #[error("PROVIDER_NATIVE_OPTION_RESERVED: {0} must use the sealed envelope field")]
    ReservedOption(String),
    #[error("PROVIDER_SAMPLING_FORBIDDEN: {0} is forbidden by this Anthropic API binding")]
    ForbiddenSampling(String),
    #[error("PROVIDER_IMAGE_PART_INVALID: {0}")]
    InvalidImagePart(String),
    #[error("PROVIDER_MESSAGE_RULE_VIOLATION: {0}")]
    MessageRule(String),
}

impl ProviderWireError {
    pub fn code(&self) -> WireErrorCode {
        match self {
            Self::BindingMismatch => WireErrorCode::ProviderBindingMismatch,
            Self::ReservedOption(_) => WireErrorCode::ReservedNativeOption,
            Self::ForbiddenSampling(_) => WireErrorCode::ForbiddenSampling,
            Self::InvalidImagePart(_) => WireErrorCode::InvalidImagePart,
            Self::MessageRule(_) => WireErrorCode::MessageRuleViolation,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("provider rejected request with HTTP {http_status}")]
pub struct ProviderRejection {
    pub http_status: u16,
    /// Rate-limit headers that arrived on the rejection response itself.
    /// A 429 carries real bucket evidence; it is never discarded just
    /// because the request failed.
    pub ratelimit_headers: Vec<(String, String)>,
}

impl ProviderRejection {
    /// Only statuses that are an explicit request rejection become terminal
    /// evidence. Timeouts, conflicts and server/transport failures stay unknown.
    pub(crate) fn from_status(
        status: reqwest::StatusCode,
        ratelimit_headers: Vec<(String, String)>,
    ) -> Option<Self> {
        use reqwest::StatusCode;
        matches!(
            status,
            StatusCode::BAD_REQUEST
                | StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::METHOD_NOT_ALLOWED
                | StatusCode::NOT_ACCEPTABLE
                | StatusCode::PAYLOAD_TOO_LARGE
                | StatusCode::UNSUPPORTED_MEDIA_TYPE
                | StatusCode::UNPROCESSABLE_ENTITY
                | StatusCode::TOO_MANY_REQUESTS
        )
        .then_some(Self {
            http_status: status.as_u16(),
            ratelimit_headers,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationKind {
    ProviderTerminal,
    TransportRejected,
    NeverDispatched,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminationEvidence {
    pub kind: TerminationKind,
    pub stop_reason: Option<String>,
    pub max_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// True for both Anthropic `max_tokens` and OpenAI-compatible `length`.
    pub length: bool,
}

impl TerminationEvidence {
    pub(crate) fn never_dispatched() -> Self {
        Self {
            kind: TerminationKind::NeverDispatched,
            stop_reason: None,
            max_tokens: None,
            output_tokens: None,
            length: false,
        }
    }

    pub(crate) fn unknown() -> Self {
        Self {
            kind: TerminationKind::Unknown,
            stop_reason: None,
            max_tokens: None,
            output_tokens: None,
            length: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResponse {
    pub content: String,
    pub stop_reason: String,
    pub raw_response: String,
    pub usage: Option<TokenUsage>,
    /// Rate-limit headers the provider attached to this response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ratelimit_headers: Vec<(String, String)>,
    /// Citations the provider returned, kept verbatim. A binding that does
    /// not report citations leaves this empty; it is not a claim that the
    /// answer had none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub citations: Vec<ProviderCitation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}


