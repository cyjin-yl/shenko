//! Open provider profiles and wire contracts — SPEC §13.7, §41.10, §47.
//!
//! A profile is the non-secret, versioned description of one provider
//! binding: backend family, transport, endpoint origin, model, API version,
//! SDK closure, sampling capabilities, auth method, usage field mapping and
//! the physical resource domain it belongs to. The profile is sealed with the
//! request, so a recovered request keeps the exact interpretation of the
//! payload that was sent.
//!
//! Nothing here decides which model may be used. A profile describes one
//! actually configured binding; an unavailable capability stays `unknown` or
//! `unsupported` with its reason, and a missing catalog entry is not evidence
//! that a model does not exist.

use serde::{Deserialize, Serialize};

use crate::request::{ProviderBinding, TokenUsage};

/// Native extension namespaces kept distinct from the OpenAI-compatible
/// minimum common denominator (SPEC §13.7).
pub const NATIVE_NAMESPACE_NINFER: &str = "native.ninfer";
pub const NATIVE_NAMESPACE_STRATA: &str = "native.strata";
pub const NATIVE_NAMESPACE_VLLM: &str = "native.vllm";
pub const NATIVE_NAMESPACE_ONE_CAT: &str = "native.1cat";

/// The backend family a binding actually speaks. `Other` keeps the set open
/// for an installed provider plugin that is not one of the four documented
/// native targets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendFamily {
    OpenAiCompatible,
    Anthropic,
    NInfer,
    Strata,
    Vllm,
    OneCat,
    Other(String),
}

impl BackendFamily {
    pub fn as_str(&self) -> &str {
        match self {
            Self::OpenAiCompatible => "openai_compatible",
            Self::Anthropic => "anthropic",
            Self::NInfer => "ninfer",
            Self::Strata => "strata",
            Self::Vllm => "vllm",
            Self::OneCat => "one_cat",
            Self::Other(name) => name,
        }
    }
}

/// How the bytes actually reach the backend. A profile never claims a
/// transport the deployment did not configure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    HttpJson,
    HttpSse,
    Grpc,
    Stdio,
    Sdk,
}

/// The actual credential method of one binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    /// API key in a dedicated header (`x-api-key`).
    ApiKeyHeader,
    /// `Authorization: Bearer <key>`; the default of newer SDK majors.
    Bearer,
    /// The endpoint needs no credential.
    None,
}

impl AuthMethod {
    pub fn header_name(&self) -> Option<&'static str> {
        match self {
            Self::ApiKeyHeader => Some("x-api-key"),
            Self::Bearer => Some("authorization"),
            Self::None => None,
        }
    }
}

/// Non-secret auth binding: the method, the header actually used and a digest
/// of the credential. The credential itself never enters a profile, a prompt,
/// a receipt or a log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthBinding {
    pub method: AuthMethod,
    /// Header name actually sent. It is empty when no credential is used.
    pub header: String,
    /// blake3 hex of the exact credential bound to this profile. Two accounts
    /// on one endpoint are therefore two different bindings.
    #[serde(default)]
    pub credential_fingerprint: Option<String>,
}

impl AuthBinding {
    pub fn none() -> Self {
        Self { method: AuthMethod::None, header: String::new(), credential_fingerprint: None }
    }

    pub fn api_key_header(fingerprint: Option<String>) -> Self {
        Self {
            method: AuthMethod::ApiKeyHeader,
            header: "x-api-key".into(),
            credential_fingerprint: fingerprint,
        }
    }

    pub fn bearer(fingerprint: Option<String>) -> Self {
        Self {
            method: AuthMethod::Bearer,
            header: "authorization".into(),
            credential_fingerprint: fingerprint,
        }
    }
}

/// The SDK closure a binding actually uses. `direct_http` is a real choice:
/// the serializer records its own revision instead of omitting the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SdkBinding {
    pub name: String,
    pub version: String,
    /// The auth method this SDK generation sends by default. The effective
    /// method is the profile's explicit auth binding; this records what the
    /// SDK would do on its own, so an upgrade that flips the default is
    /// visible instead of silently changing the wire.
    pub default_auth: AuthMethod,
}

/// Which sampling keys a binding accepts and which it forbids. A forbidden
/// key is omitted from the final wire entirely — `null`, `0` and an explicit
/// default are all values, not omissions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplingCapabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbidden: Vec<String>,
}

impl SamplingCapabilities {
    pub fn forbids(&self, key: &str) -> bool {
        self.forbidden.iter().any(|forbidden| forbidden == key)
    }

    pub fn allows(&self, key: &str) -> bool {
        self.allowed.iter().any(|allowed| allowed == key)
    }

    /// A binding that forbids a key it also allows is contradictory and is
    /// rejected at seal time rather than resolved at send time.
    pub fn is_consistent(&self) -> bool {
        !self.allowed.iter().any(|allowed| self.forbids(allowed))
    }
}

/// Where the backend reports its token counters. The paths are dotted JSON
/// paths into the raw response; a missing path stays unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageFieldMapping {
    #[serde(default)]
    pub input_tokens_path: Option<String>,
    #[serde(default)]
    pub output_tokens_path: Option<String>,
    #[serde(default)]
    pub cache_read_tokens_path: Option<String>,
    #[serde(default)]
    pub cache_write_tokens_path: Option<String>,
}

/// The complete non-secret wire contract of one provider binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Stable identity of this profile revision, e.g. `native.vllm.chat_completions`.
    pub profile_id: String,
    /// Serializer/codec revision of this profile.
    pub revision: u32,
    pub backend_family: BackendFamily,
    pub transport: TransportKind,
    /// Scheme + host + port of the endpoint. Never carries credentials.
    pub endpoint_origin: String,
    pub model_id: String,
    /// Provider API version when the binding has one (`2026-10-21` for the
    /// current Claude Messages API). `None` means the binding does not carry
    /// an API version, not "unknown version".
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub sdk: Option<SdkBinding>,
    pub request_schema_revision: u32,
    pub response_schema_revision: u32,
    #[serde(default)]
    pub gateway_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub beta_features: Vec<String>,
    #[serde(default)]
    pub sampling: SamplingCapabilities,
    #[serde(default = "AuthBinding::none")]
    pub auth: AuthBinding,
    #[serde(default)]
    pub usage: UsageFieldMapping,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub native_namespaces: Vec<String>,
    /// Identity of the physical engine/resource domain this binding shares.
    /// Two URLs that reach one resident engine must name one domain.
    pub resource_domain_id: String,
    #[serde(default)]
    pub engine_incarnation: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub default_max_tokens: u64,
    #[serde(default)]
    pub serializer_revision: u32,
}

fn default_max_tokens() -> u64 {
    crate::request::DEFAULT_MAX_TOKENS
}

impl ProviderProfile {
    /// The sealed binding this profile produces.
    pub fn binding(&self) -> ProviderBinding {
        ProviderBinding::Profile { profile: self.clone() }
    }

    /// A new profile for another model on the same transport. It never edits
    /// an existing request seal or its endpoint.
    pub fn with_model(&self, model_id: &str) -> ProviderProfile {
        let mut profile = self.clone();
        profile.model_id = model_id.to_string();
        profile
    }

    /// Identity comparison of two profiles. The credential fingerprint is part
    /// of it: two accounts on one endpoint are different bindings.
    pub fn matches(&self, other: &ProviderProfile) -> bool {
        self.profile_id == other.profile_id
            && self.revision == other.revision
            && self.backend_family == other.backend_family
            && self.transport == other.transport
            && self.endpoint_origin == other.endpoint_origin
            && self.model_id == other.model_id
            && self.api_version == other.api_version
            && self.sdk == other.sdk
            && self.request_schema_revision == other.request_schema_revision
            && self.response_schema_revision == other.response_schema_revision
            && self.gateway_profile == other.gateway_profile
            && self.beta_features == other.beta_features
            && self.sampling == other.sampling
            && self.auth == other.auth
            && self.usage == other.usage
            && self.native_namespaces == other.native_namespaces
            && self.resource_domain_id == other.resource_domain_id
            && self.engine_incarnation == other.engine_incarnation
            && self.default_max_tokens == other.default_max_tokens
            && self.serializer_revision == other.serializer_revision
    }

    /// Identity comparison that ignores the model. One runtime serves the
    /// models of its own transport; the sealed request still names the exact
    /// model that goes on the wire.
    pub fn matches_except_model(&self, other: &ProviderProfile) -> bool {
        let mut profile = other.clone();
        profile.model_id = self.model_id.clone();
        self.matches(&profile)
    }

    /// Structural validation performed before a profile may be sealed.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.profile_id.trim().is_empty(), "provider profile id is empty");
        anyhow::ensure!(
            !self.model_id.trim().is_empty() && self.model_id.len() <= 512
                && !self.model_id.chars().any(char::is_control),
            "provider profile model id is invalid"
        );
        anyhow::ensure!(!self.resource_domain_id.trim().is_empty(), "provider profile has no resource domain");
        let url = reqwest::Url::parse(&self.endpoint_origin)
            .map_err(|_| anyhow::anyhow!("invalid provider endpoint origin"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "provider endpoint origin must be HTTP(S) without credentials/query/fragment"
        );
        anyhow::ensure!(self.sampling.is_consistent(), "provider sampling capabilities contradict themselves");
        if self.auth.method != AuthMethod::None {
            anyhow::ensure!(
                !self.auth.header.trim().is_empty()
                    && self.auth.header.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                    }),
                "provider auth header name is invalid"
            );
        }
        Ok(())
    }

    /// The credential method actually used on the wire.
    pub fn effective_auth(&self) -> AuthMethod {
        match self.auth.method {
            AuthMethod::None => self.sdk.as_ref().map(|sdk| sdk.default_auth).unwrap_or(AuthMethod::None),
            method => method,
        }
    }

    /// Read the token counters this backend actually reported. A path that is
    /// absent, non-integer or negative stays unknown; an explicit zero is 0.
    pub fn read_usage(&self, raw: &serde_json::Value) -> TokenUsage {
        TokenUsage {
            input_tokens: read_counter(raw, self.usage.input_tokens_path.as_deref()),
            output_tokens: read_counter(raw, self.usage.output_tokens_path.as_deref()),
            cache_read_tokens: read_counter(raw, self.usage.cache_read_tokens_path.as_deref()),
            cache_write_tokens: read_counter(raw, self.usage.cache_write_tokens_path.as_deref()),
        }
    }

    /// The dotted JSON path of the input token counter of this binding.
    pub fn input_token_path(&self) -> Option<&str> {
        self.usage.input_tokens_path.as_deref()
    }

    /// Whether this binding may use a native extension namespace at all.
    pub fn declares_native(&self, namespace: &str) -> bool {
        self.native_namespaces.iter().any(|declared| declared == namespace)
    }
}

/// Read one dotted JSON path as a non-negative token counter.
fn read_counter(raw: &serde_json::Value, path: Option<&str>) -> Option<u64> {
    let path = path?;
    let mut current = raw;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    current.as_u64()
}

/// The four documented native backend targets, each with the transport and
/// native namespace it actually uses. The endpoint, model and credential are
/// supplied by the deployment; the profile never invents them.
pub mod native_backends {
    use super::{
        AuthBinding, BackendFamily, ProviderProfile, SamplingCapabilities, TransportKind,
        UsageFieldMapping, NATIVE_NAMESPACE_NINFER, NATIVE_NAMESPACE_ONE_CAT,
        NATIVE_NAMESPACE_STRATA, NATIVE_NAMESPACE_VLLM,
    };

    fn base(
        profile_id: &str,
        family: BackendFamily,
        namespace: &str,
        endpoint_origin: &str,
        model_id: &str,
        resource_domain_id: &str,
        transport: TransportKind,
    ) -> ProviderProfile {
        ProviderProfile {
            profile_id: profile_id.to_string(),
            revision: 1,
            backend_family: family,
            transport,
            endpoint_origin: endpoint_origin.to_string(),
            model_id: model_id.to_string(),
            api_version: None,
            sdk: None,
            request_schema_revision: 1,
            response_schema_revision: 1,
            gateway_profile: None,
            beta_features: Vec::new(),
            sampling: SamplingCapabilities::default(),
            auth: AuthBinding::none(),
            usage: UsageFieldMapping::default(),
            native_namespaces: vec![namespace.to_string()],
            resource_domain_id: resource_domain_id.to_string(),
            engine_incarnation: None,
            default_max_tokens: super::default_max_tokens(),
            serializer_revision: 1,
        }
    }

    /// NInfer: prefix reuse and engine-internal state retention. `prompt_cache_key`
    /// is a cache hint, not an engine session or slot handle.
    pub fn ninfer(
        endpoint_origin: &str,
        model_id: &str,
        resource_domain_id: &str,
    ) -> ProviderProfile {
        let mut profile = base(
            "native.ninfer.chat_completions",
            BackendFamily::NInfer,
            NATIVE_NAMESPACE_NINFER,
            endpoint_origin,
            model_id,
            resource_domain_id,
            TransportKind::HttpJson,
        );
        profile.usage = UsageFieldMapping {
            input_tokens_path: Some("usage.prompt_tokens".into()),
            output_tokens_path: Some("usage.completion_tokens".into()),
            cache_read_tokens_path: None,
            cache_write_tokens_path: None,
        };
        profile
    }

    /// Strata: host conversation parking and native engine events. Host parking
    /// is host RAM, not GPU residency.
    pub fn strata(
        endpoint_origin: &str,
        model_id: &str,
        resource_domain_id: &str,
    ) -> ProviderProfile {
        let mut profile = base(
            "native.strata.chat_completions",
            BackendFamily::Strata,
            NATIVE_NAMESPACE_STRATA,
            endpoint_origin,
            model_id,
            resource_domain_id,
            TransportKind::HttpJson,
        );
        profile.usage = UsageFieldMapping {
            input_tokens_path: Some("usage.prompt_tokens".into()),
            output_tokens_path: Some("usage.completion_tokens".into()),
            cache_read_tokens_path: None,
            cache_write_tokens_path: None,
        };
        profile
    }

    /// vLLM: prefix caching, cache salt and request/session identity. The
    /// Rust HTTP generate route of the pinned upstream commit does not carry
    /// `kv_hints`, so that operation is reported unsupported for this
    /// transport instead of being claimed.
    pub fn vllm(
        endpoint_origin: &str,
        model_id: &str,
        resource_domain_id: &str,
    ) -> ProviderProfile {
        let mut profile = base(
            "native.vllm.chat_completions",
            BackendFamily::Vllm,
            NATIVE_NAMESPACE_VLLM,
            endpoint_origin,
            model_id,
            resource_domain_id,
            TransportKind::HttpJson,
        );
        profile.usage = UsageFieldMapping {
            input_tokens_path: Some("usage.prompt_tokens".into()),
            output_tokens_path: Some("usage.completion_tokens".into()),
            cache_read_tokens_path: Some("usage.prompt_tokens_details.cached_tokens".into()),
            cache_write_tokens_path: None,
        };
        profile
    }

    /// 1Cat-vLLM: an independent fork. Unknown fields may be accepted and
    /// ignored, so an accepted request is never evidence that a native
    /// option took effect.
    pub fn one_cat(
        endpoint_origin: &str,
        model_id: &str,
        resource_domain_id: &str,
    ) -> ProviderProfile {
        let mut profile = base(
            "native.1cat.chat_completions",
            BackendFamily::OneCat,
            NATIVE_NAMESPACE_ONE_CAT,
            endpoint_origin,
            model_id,
            resource_domain_id,
            TransportKind::HttpJson,
        );
        profile.usage = UsageFieldMapping {
            input_tokens_path: Some("usage.prompt_tokens".into()),
            output_tokens_path: Some("usage.completion_tokens".into()),
            cache_read_tokens_path: None,
            cache_write_tokens_path: None,
        };
        profile
    }
}
