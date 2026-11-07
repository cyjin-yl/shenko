//! Local model provider — OpenAI-compatible /v1/chat/completions. SPEC §22.1
//!
//! Native local sampling options remain intact; Claude-only API restrictions
//! do not leak into this binding. The endpoint, model and credential are read
//! once at construction, and the sealed binding keeps the exact identity the
//! request was sent with.
use anyhow::{Result, anyhow};
use base64::Engine as _;
use tracing::{info, instrument};

use crate::anthropic_rules::ProviderCitation;
use crate::capability::{BackendCapabilitySnapshot, CapacityEntry, CapacityUnit, classify_operations};
use crate::profile::{
    AuthBinding, AuthMethod, BackendFamily, ProviderProfile, SamplingCapabilities, TransportKind,
    UsageFieldMapping,
};
use crate::provider::merge_native_options;
use crate::registry::{ProviderRuntime, TokenCount};
use crate::request::{
    InferencePart, InferencePayload, InferenceRequest, InferenceResponse, ProviderBinding,
    ProviderRejection, ProviderWireError, TokenUsage, validate_image_part,
};
use crate::stream::{
    BoundedStreamPublisher, SseDecoder, StreamEvent, StreamOutcome, parse_openai_frame,
};

const DEFAULT_MODEL: &str = "qwen3.8-27b";
pub(crate) const SERIALIZER_REVISION: u32 = 3;

/// A configured OpenAI-compatible binding.
#[derive(Debug, Clone)]
pub struct LocalModelProvider {
    pub base_url: String,
    pub model: String,
    pub allow_invalid_certs: bool,
    profile: ProviderProfile,
    api_key: Option<String>,
}

impl LocalModelProvider {
    pub fn new(base_url: String, model: String) -> Self {
        let profile = local_profile(&base_url, &model, None, "unclassified.local");
        Self { base_url, model, allow_invalid_certs: false, profile, api_key: None }
    }

    /// Build from environment without embedding a private deployment endpoint.
    /// Later environment changes do not alter an existing or recovered binding.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("SHENKO_LOCAL_MODEL_URL").map_err(|_| {
            anyhow!("SHENKO_LOCAL_MODEL_URL is required when SHENKO_USE_LOCAL_MODEL=1")
        })?;
        let model =
            std::env::var("SHENKO_LOCAL_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        anyhow::ensure!(!model.is_empty(), "SHENKO_LOCAL_MODEL must not be empty");
        let api_key = std::env::var("SHENKO_LOCAL_MODEL_API_KEY").ok().filter(|key| !key.is_empty());
        let profile = local_profile(
            &base_url,
            &model,
            api_key.as_ref().map(|key| credential_fingerprint(key)),
            "unclassified.local",
        );
        Ok(Self {
            base_url,
            model,
            allow_invalid_certs: std::env::var("SHENKO_LOCAL_INSECURE").as_deref() == Ok("1"),
            profile,
            api_key,
        })
    }

    /// The physical resource domain this binding belongs to.
    pub fn with_resource_domain(
        mut self,
        domain_id: impl Into<String>,
        engine_identity: Option<String>,
    ) -> Self {
        self.profile.resource_domain_id = domain_id.into();
        self.profile.engine_incarnation = engine_identity;
        self
    }

    /// The non-secret description of this binding.
    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    /// The exact frozen binding this provider executes. Callers (broker or
    /// behavior evaluation) seal requests with it instead of rebuilding the
    /// non-secret envelope by hand.
    pub fn binding(&self) -> ProviderBinding {
        ProviderBinding::Local {
            base_url: self.base_url.clone(),
            model: self.model.clone(),
            allow_invalid_certs: self.allow_invalid_certs,
            serializer_revision: SERIALIZER_REVISION,
            default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
        }
    }

    fn validate_binding<'a>(&self, req: &'a InferenceRequest) -> Result<&'a ProviderBinding> {
        let ProviderBinding::Local {
            base_url,
            model,
            allow_invalid_certs,
            serializer_revision,
            default_max_tokens,
        } = &req.provider_binding
        else {
            return Err(ProviderWireError::BindingMismatch.into());
        };
        if base_url != &self.base_url
            || allow_invalid_certs != &self.allow_invalid_certs
            || *serializer_revision != SERIALIZER_REVISION
            || *default_max_tokens != crate::request::DEFAULT_MAX_TOKENS
        {
            return Err(ProviderWireError::BindingMismatch.into());
        }
        // The model is the caller's choice on this transport; the sealed
        // request names the exact one that goes on the wire.
        let _ = model;
        Ok(&req.provider_binding)
    }

    fn client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180));
        if self.allow_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        // A sealed request is bound to this endpoint; an automatic redirect
        // would replay the frozen payload on another host while the receipt is
        // still attributed here. Any 3xx therefore reports terminal unknown.
        builder = builder.redirect(reqwest::redirect::Policy::none());
        Ok(builder.build()?)
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(key) = &self.api_key else { return builder };
        match self.profile.effective_auth() {
            AuthMethod::Bearer => builder.header("authorization", format!("Bearer {key}")),
            AuthMethod::ApiKeyHeader => builder.header("x-api-key", key),
            AuthMethod::None => builder,
        }
    }

    #[instrument(skip(self, req))]
    pub async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
        self.validate_binding(req)?;
        let ProviderBinding::Local {
            base_url, model, ..
        } = &req.provider_binding
        else {
            unreachable!("binding checked above")
        };
        let payload = &req.payload;
        let client = self.client()?;

        let body = local_body(payload, model)?;
        let resp = self
            .apply_auth(client.post(format!(
                "{}/v1/chat/completions",
                base_url.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        // Retained verbatim for later source contracts; only the observed
        // quota header family is kept, never unrelated headers.
        let ratelimit_headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| {
                name.as_str().starts_with(crate::QUOTA_HEADER_PREFIX)
            })
            .map(|(name, value)| {
                (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string())
            })
            .collect();
        if !resp.status().is_success() {
            let status = resp.status();
            if let Some(rejection) = ProviderRejection::from_status(status, ratelimit_headers) {
                return Err(rejection.into());
            }
            return Err(anyhow!(
                "local model API HTTP {status}; terminal outcome unknown"
            ));
        }

        let raw_response = resp.text().await?;
        let raw: serde_json::Value = serde_json::from_str(&raw_response)?;
        let content = raw["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["message"]["content"].as_str())
            .unwrap_or_default()
            .to_string();
        let finish_reason = raw["choices"]
            .as_array()
            .and_then(|choices| choices.first())
            .and_then(|choice| choice["finish_reason"].as_str())
            .unwrap_or_default()
            .to_string();

        let usage = raw["usage"].as_object().map(|u| TokenUsage {
            input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()),
            output_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64()),
            // Passive prefix-cache evidence: OpenAI-compatible backends that
            // support it report `prompt_tokens_details.cached_tokens` (current
            // shape) or a flat `prompt_cache_hit_tokens` (older vLLM-family
            // shape). Both become cache_read; absent/invalid fields stay
            // unknown rather than 0, and a backend with no reporting keeps
            // `None`. This is a hit observation only — never a residency
            // guarantee (§13.6, §13.7).
            cache_read_tokens: u
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .or_else(|| {
                    u.get("prompt_cache_hit_tokens")
                        .and_then(|v| v.as_u64())
                }),
            cache_write_tokens: None,
        });

        info!(
            model = %model,
            finish_reason = %finish_reason,
            in_tokens = ?usage.as_ref().and_then(|u| u.input_tokens),
            out_tokens = ?usage.as_ref().and_then(|u| u.output_tokens),
            "local inference complete"
        );

        Ok(InferenceResponse {
            content,
            stop_reason: finish_reason,
            raw_response,
            usage,
            ratelimit_headers,
            citations: Vec::new(),
        })
    }

    /// Stream one request from the provider's own event stream.
    pub async fn stream(
        &self,
        req: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome> {
        self.validate_binding(req)?;
        let ProviderBinding::Local {
            base_url, model, ..
        } = &req.provider_binding
        else {
            unreachable!("binding checked above")
        };
        let client = self.client()?;
        let mut body = local_body(&req.payload, model)?;
        body["stream"] = serde_json::Value::Bool(true);
        body["stream_options"] = serde_json::json!({ "include_usage": true });
        let mut resp = self
            .apply_auth(client.post(format!(
                "{}/v1/chat/completions",
                base_url.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let ratelimit_headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| {
                name.as_str().starts_with(crate::QUOTA_HEADER_PREFIX)
            })
            .map(|(name, value)| {
                (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string())
            })
            .collect();
        if !resp.status().is_success() {
            let status = resp.status();
            if let Some(rejection) = ProviderRejection::from_status(status, ratelimit_headers) {
                return Err(rejection.into());
            }
            return Err(anyhow!(
                "local model API HTTP {status}; terminal outcome unknown"
            ));
        }
        let mut decoder = SseDecoder::new();
        let mut raw_frames: Vec<String> = Vec::new();
        let mut content = String::new();
        let mut stop_reason = String::new();
        let mut usage: Option<TokenUsage> = None;
        let mut events = 0u64;
        let mut batches = 0u64;
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await? {
            buffer.extend_from_slice(&chunk);
            while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=position).collect();
                let line = String::from_utf8_lossy(&line).to_string();
                let Some(frame) = decoder.feed_line(&line) else { continue };
                raw_frames.push(frame.data.clone());
                for event in parse_openai_frame(&frame)? {
                    events += 1;
                    match &event {
                        StreamEvent::TextDelta { text } => content.push_str(text),
                        StreamEvent::MessageStop { stop_reason: reason } => {
                            if !reason.is_empty() {
                                stop_reason = reason.clone();
                            }
                        }
                        StreamEvent::Usage { usage: reported } => {
                            usage = Some(TokenUsage {
                                input_tokens: reported.input_tokens.or_else(|| {
                                    usage.as_ref().and_then(|usage| usage.input_tokens)
                                }),
                                output_tokens: reported.output_tokens.or_else(|| {
                                    usage.as_ref().and_then(|usage| usage.output_tokens)
                                }),
                                cache_read_tokens: reported.cache_read_tokens.or_else(|| {
                                    usage.as_ref().and_then(|usage| usage.cache_read_tokens)
                                }),
                                cache_write_tokens: reported.cache_write_tokens.or_else(|| {
                                    usage.as_ref().and_then(|usage| usage.cache_write_tokens)
                                }),
                            });
                        }
                        _ => {}
                    }
                    publisher.push(event, now_ms())?;
                    batches += 1;
                }
            }
        }
        Ok(StreamOutcome {
            response: InferenceResponse {
                content,
                stop_reason,
                raw_response: raw_frames.join("\n"),
                usage,
                ratelimit_headers,
                citations: Vec::new(),
            },
            events,
            batches,
        })
    }
}

#[async_trait::async_trait]
impl ProviderRuntime for LocalModelProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn binding(&self) -> ProviderBinding {
        LocalModelProvider::binding(self)
    }

    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        LocalModelProvider::complete(self, request).await
    }

    async fn stream(
        &self,
        request: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome> {
        LocalModelProvider::stream(self, request, publisher).await
    }

    async fn count_tokens(&self, request: &InferenceRequest) -> Result<TokenCount> {
        self.validate_binding(request)?;
        // The OpenAI-compatible surface has no token counting entry point. The
        // count stays unknown instead of being estimated from characters.
        Err(anyhow!("token counting is not available on this binding"))
    }

    async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
        let client = self.client()?;
        let mut evidence: Vec<serde_json::Value> = Vec::new();
        match client
            .get(format!("{}/v1/models", self.base_url.trim_end_matches('/')))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let raw = response.text().await?;
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    evidence.push(value);
                }
            }
            Ok(response) => {
                evidence.push(serde_json::json!({ "probe_error": format!("HTTP {}", response.status()) }));
            }
            Err(error) => {
                evidence.push(serde_json::json!({ "probe_error": error.to_string() }));
            }
        }
        // The engine's own metrics surface is where a real KV pool figure lives.
        // It is one idempotent read, never a generation and never retried.
        evidence.push(
            crate::capability::metrics_evidence(&client, &self.base_url, |builder| {
                self.apply_auth(builder)
            })
            .await,
        );
        let operations = classify_operations(&self.profile.backend_family, &evidence);
        // A figure the backend itself reported becomes a configured pool; no
        // figure keeps the pool unknown. The model's context window is not a
        // pool and is reported separately, so it is never read from here.
        let kv = crate::capability::discover_kv_capacity(&self.profile.backend_family, &evidence);
        Ok(BackendCapabilitySnapshot {
            capability_id: format!("{}/{}", self.profile.profile_id, self.profile.revision),
            provider_implementation_digest: self.profile.profile_id.clone(),
            backend_family: self.profile.backend_family.clone(),
            backend_build: None,
            transport: self.profile.transport,
            configuration_digest: None,
            engine_incarnation: self.profile.engine_incarnation.clone(),
            model_fingerprint: None,
            tokenizer_fingerprint: None,
            template_fingerprint: None,
            toolset_digest: None,
            operations,
            capacities: vec![
                CapacityEntry {
                    pool: crate::admission::CapacityPool::Compute,
                    unit: CapacityUnit::Requests,
                    value: None,
                    provenance: crate::admission::CapacityProvenance::Unknown,
                },
                kv.map(|observed| observed.entry()).unwrap_or(CapacityEntry {
                    pool: crate::admission::CapacityPool::ContextKv,
                    unit: CapacityUnit::Tokens,
                    value: None,
                    provenance: crate::admission::CapacityProvenance::Unknown,
                }),
            ],
            raw_evidence: evidence,
        })
    }
}

fn credential_fingerprint(api_key: &str) -> String {
    shenko_types::object::Digest::blake3(api_key.as_bytes()).0
}

/// The OpenAI-compatible profile of one configured binding.
fn local_profile(
    base_url: &str,
    model: &str,
    fingerprint: Option<String>,
    resource_domain_id: &str,
) -> ProviderProfile {
    ProviderProfile {
        profile_id: "local.openai.chat_completions".into(),
        revision: SERIALIZER_REVISION,
        backend_family: BackendFamily::OpenAiCompatible,
        transport: TransportKind::HttpJson,
        endpoint_origin: base_url.to_string(),
        model_id: model.to_string(),
        api_version: None,
        sdk: None,
        request_schema_revision: SERIALIZER_REVISION,
        response_schema_revision: SERIALIZER_REVISION,
        gateway_profile: None,
        beta_features: Vec::new(),
        sampling: SamplingCapabilities::default(),
        auth: AuthBinding {
            method: if fingerprint.is_some() { AuthMethod::ApiKeyHeader } else { AuthMethod::None },
            header: "x-api-key".into(),
            credential_fingerprint: fingerprint,
        },
        usage: UsageFieldMapping {
            input_tokens_path: Some("usage.prompt_tokens".into()),
            output_tokens_path: Some("usage.completion_tokens".into()),
            cache_read_tokens_path: Some("usage.prompt_tokens_details.cached_tokens".into()),
            cache_write_tokens_path: None,
        },
        native_namespaces: Vec::new(),
        resource_domain_id: resource_domain_id.to_string(),
        engine_incarnation: None,
        default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
        serializer_revision: SERIALIZER_REVISION,
    }
}

/// OpenAI-compatible request body: a text-only message keeps the plain string
/// content, and a message with an image carries ordered content parts.
fn local_body(payload: &InferencePayload, model: &str) -> Result<serde_json::Value> {
    let mut messages = Vec::with_capacity(payload.messages.len() + 1);
    if !payload.system_prompt.is_empty() {
        messages.push(serde_json::json!({
            "role": "system",
            "content": payload.system_prompt,
        }));
    }
    for message in &payload.messages {
        messages.push(serde_json::json!({
            "role": message.role,
            "content": openai_content(&message.parts)?,
        }));
    }
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": payload.max_tokens.unwrap_or(crate::request::DEFAULT_MAX_TOKENS),
        "messages": messages,
    });
    merge_native_options(&mut body, &payload.native_options, false)?;
    Ok(body)
}

/// Render message parts for an OpenAI-compatible surface. A text-only message
/// keeps the plain string form; a message with an image carries ordered parts.
pub(crate) fn openai_content(parts: &[InferencePart]) -> Result<serde_json::Value> {
    if !parts.iter().any(InferencePart::is_image) {
        let text = parts
            .iter()
            .filter_map(|part| match part {
                InferencePart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        return Ok(serde_json::Value::String(text));
    }
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            InferencePart::Text { text } => blocks.push(serde_json::json!({
                "type": "text",
                "text": text,
            })),
            InferencePart::Image { media_type, bytes } => {
                validate_image_part(media_type, bytes)?;
                let mut url = String::with_capacity(
                    13 + media_type.len() + bytes.len().div_ceil(3) * 4,
                );
                url.push_str("data:");
                url.push_str(media_type);
                url.push_str(";base64,");
                base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut url);
                let mut block = serde_json::json!({
                    "type": "image_url",
                    "image_url": {},
                });
                block["image_url"]["url"] = serde_json::Value::String(url);
                blocks.push(block);
            }
            InferencePart::Document { source, media_type, title, .. } => {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": document_text(source, media_type.as_deref(), title.as_deref()),
                }));
            }
            InferencePart::Citation { text, .. } => {
                blocks.push(serde_json::json!({ "type": "text", "text": text }));
            }
            InferencePart::SearchResult { source, title, content, .. } => {
                blocks.push(serde_json::json!({
                    "type": "text",
                    "text": format!("[{source}] {}\n{content}", title.as_deref().unwrap_or_default()),
                }));
            }
        }
    }
    Ok(serde_json::Value::Array(blocks))
}

/// The plain-text projection of a document block for transports without a
/// document block. The source stays visible; the transport cannot carry the
/// structured block.
pub(crate) fn document_text(source: &str, media_type: Option<&str>, title: Option<&str>) -> String {
    match (media_type, title) {
        (Some(media_type), Some(title)) => format!("[document {media_type}] {title} ({source})"),
        (Some(media_type), None) => format!("[document {media_type}] {source}"),
        (None, Some(title)) => format!("[document] {title} ({source})"),
        (None, None) => format!("[document] {source}"),
    }
}

/// The current wall clock in milliseconds.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The citations a provider returned, kept verbatim.
pub type ResponseCitations = Vec<ProviderCitation>;
