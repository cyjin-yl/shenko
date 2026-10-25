use serde::{Deserialize, Serialize};
use shenko_types::ids::{ControllerEpoch, EntityId};
use std::collections::BTreeMap;
use tokio::sync::oneshot;

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
        serializer_revision: u32,
        default_max_tokens: u64,
    },
}

impl ProviderBinding {
    pub fn model(&self) -> &str {
        match self {
            Self::Stub { .. } => "stub",
            Self::Local { model, .. } | Self::Anthropic { model, .. } => model,
        }
    }

    pub fn profile_id(&self) -> &'static str {
        match self {
            Self::Stub { .. } => "provider.unavailable",
            Self::Local { .. } => "local.openai.chat_completions",
            Self::Anthropic { .. } => "anthropic.messages",
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
        }
    }

    pub fn api_version(&self) -> Option<&str> {
        match self {
            Self::Anthropic { api_version, .. } => Some(api_version),
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
        }
    }

    pub(crate) fn validate_endpoint(&self) -> anyhow::Result<()> {
        let endpoint = match self {
            Self::Stub { .. } => return Ok(()),
            Self::Local { base_url, .. } | Self::Anthropic { base_url, .. } => base_url,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferencePriority {
    Low = 0,
    Normal = 1,
    Subject = 2,
    Urgent = 3,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceMessage {
    pub role: String,
    pub content: String,
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
}

impl WireErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProviderBindingMismatch => "MODEL_PROVIDER_BINDING_MISMATCH",
            Self::ReservedNativeOption => "PROVIDER_NATIVE_OPTION_RESERVED",
            Self::ForbiddenSampling => "PROVIDER_SAMPLING_FORBIDDEN",
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
}

impl ProviderWireError {
    pub fn code(&self) -> WireErrorCode {
        match self {
            Self::BindingMismatch => WireErrorCode::ProviderBindingMismatch,
            Self::ReservedOption(_) => WireErrorCode::ReservedNativeOption,
            Self::ForbiddenSampling(_) => WireErrorCode::ForbiddenSampling,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("provider rejected request with HTTP {http_status}")]
pub struct ProviderRejection {
    pub http_status: u16,
}

impl ProviderRejection {
    /// Only statuses that are an explicit request rejection become terminal
    /// evidence. Timeouts, conflicts and server/transport failures stay unknown.
    pub(crate) fn from_status(status: reqwest::StatusCode) -> Option<Self> {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}
