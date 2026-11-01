//! Anthropic Claude API provider — SPEC §47 PV01–PV06, §13.7.
//!
//! The wire contract is the Anthropic Messages API. The rules of the bound
//! API version — the forbidden sampling keys, the role and citation limits —
//! are applied to every path that renders a request: ordinary messages,
//! streaming, token counting and Batch. The credential method is a property
//! of the binding, so the same account is used the same way on every path.
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;
use tracing::{info, instrument};

use crate::anthropic_rules::{
    ANTHROPIC_API_VERSION, AnthropicRuleViolation, ProviderCitation, validate_messages,
    validate_native_options,
};
use crate::capability::{BackendCapabilitySnapshot, CapacityEntry, CapacityUnit, classify_operations};
use crate::profile::{
    AuthBinding, AuthMethod, BackendFamily, ProviderProfile, SamplingCapabilities, TransportKind,
    UsageFieldMapping,
};
use crate::registry::{ProviderRuntime, TokenCount};
use crate::request::{
    InferencePart, InferencePayload, InferenceRequest, InferenceResponse, ProviderBinding,
    ProviderRejection, ProviderWireError, TokenUsage, base64_image_bytes, validate_image_part,
};
use crate::stream::{
    BoundedStreamPublisher, SseDecoder, StreamEvent, StreamOutcome, parse_anthropic_frame,
};

pub(crate) const ANTHROPIC_API_VERSION_HEADER: &str = ANTHROPIC_API_VERSION;
const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub(crate) const SERIALIZER_REVISION: u32 = 3;

/// How this provider seals its binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingShape {
    /// The historical Anthropic binding. It stays byte-compatible with the
    /// sealed history that already exists.
    Historical,
    /// The open profile binding, used when the deployment needs a gateway, a
    /// different credential method or another API version.
    Profile,
}

/// A configured Anthropic binding.
#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    profile: ProviderProfile,
    api_key: String,
    shape: BindingShape,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        let profile = anthropic_profile(
            "https://api.anthropic.com",
            DEFAULT_MODEL,
            AuthMethod::ApiKeyHeader,
            Some(credential_fingerprint(&api_key)),
            "unclassified.anthropic",
        );
        Self { profile, api_key, shape: BindingShape::Historical }
    }

    /// Build a provider for an explicitly configured endpoint and model.
    pub fn with_endpoint(
        api_key: String,
        base_url: String,
        model: impl Into<String>,
    ) -> Self {
        let model = model.into();
        let profile = anthropic_profile(
            &base_url,
            &model,
            AuthMethod::ApiKeyHeader,
            Some(credential_fingerprint(&api_key)),
            "unclassified.anthropic",
        );
        Self { profile, api_key, shape: BindingShape::Historical }
    }

    /// Build a provider from an open profile. The profile freezes the
    /// transport, the endpoint, the model, the API version and the credential
    /// method that the sealed request will carry.
    pub fn with_profile(profile: ProviderProfile, api_key: String) -> Result<Self> {
        profile.validate()?;
        Ok(Self { profile, api_key, shape: BindingShape::Profile })
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

    /// Use a different credential method. Newer SDK generations send the API
    /// key as a bearer token; the server still accepts the dedicated header.
    pub fn with_auth_method(mut self, method: AuthMethod) -> Self {
        self.profile.auth.method = method;
        self.profile.auth.header = match method {
            AuthMethod::Bearer => "authorization".into(),
            AuthMethod::ApiKeyHeader => "x-api-key".into(),
            AuthMethod::None => String::new(),
        };
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.profile.model_id = model.into();
        self
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .map(Self::new)
    }

    /// The non-secret description of this binding.
    pub fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    /// Non-secret digest of the configured credential. Only equality is ever
    /// checked; the key material itself leaves this struct only as a request
    /// header on the bound endpoint.
    fn credential_fingerprint(&self) -> String {
        credential_fingerprint(&self.api_key)
    }

    /// The sealed non-secret transport contract for this configured provider.
    /// The credential fingerprint makes two different accounts on the same
    /// endpoint/model distinct bindings.
    pub fn binding(&self) -> ProviderBinding {
        match self.shape {
            BindingShape::Historical => ProviderBinding::Anthropic {
                base_url: self.profile.endpoint_origin.clone(),
                model: self.profile.model_id.clone(),
                api_version: ANTHROPIC_API_VERSION.to_string(),
                credential_fingerprint: Some(self.credential_fingerprint()),
                serializer_revision: SERIALIZER_REVISION,
                default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
            },
            BindingShape::Profile => self.profile.binding(),
        }
    }

    fn validate_binding<'a>(&self, req: &'a InferenceRequest) -> Result<&'a ProviderBinding> {
        match &req.provider_binding {
            ProviderBinding::Anthropic {
                base_url,
                model,
                api_version,
                credential_fingerprint,
                serializer_revision,
                default_max_tokens,
            } => {
                if base_url != &self.profile.endpoint_origin
                    || api_version != ANTHROPIC_API_VERSION
                    || credential_fingerprint.as_deref() != Some(self.credential_fingerprint().as_str())
                    || *serializer_revision != SERIALIZER_REVISION
                    || *default_max_tokens != crate::request::DEFAULT_MAX_TOKENS
                {
                    return Err(ProviderWireError::BindingMismatch.into());
                }
                // The model is the caller's choice on this transport; the
                // sealed request names the exact one that goes on the wire.
                let _ = model;
            }
            ProviderBinding::Profile { profile } => {
                if !self.profile.matches_except_model(profile) {
                    return Err(ProviderWireError::BindingMismatch.into());
                }
            }
            _ => return Err(ProviderWireError::BindingMismatch.into()),
        }
        Ok(&req.provider_binding)
    }

    /// The API version this binding speaks.
    fn api_version(&self) -> &str {
        self.profile.api_version.as_deref().unwrap_or(ANTHROPIC_API_VERSION)
    }

    /// The request headers of this binding. The credential method is bound to
    /// the profile, so every path sends the same identity.
    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let builder = builder.header("anthropic-version", self.api_version());
        match self.profile.effective_auth() {
            AuthMethod::Bearer => builder.header("authorization", format!("Bearer {}", self.api_key)),
            AuthMethod::ApiKeyHeader => builder.header("x-api-key", &self.api_key),
            AuthMethod::None => builder,
        }
    }

    fn client(&self) -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            // Inference POSTs are sealed to this endpoint. An automatic 307/308
            // re-send would replay the frozen prompt (and the credential) on a
            // different host while attributing the result to the original
            // endpoint. A redirect is a terminal unknown instead.
            .redirect(reqwest::redirect::Policy::none())
            .build()?)
    }

    /// The rendered request body of one call.
    fn body(
        &self,
        payload: &InferencePayload,
        model: &str,
        stream: bool,
    ) -> Result<serde_json::Value> {
        let rules = self.api_version();
        validate_messages(&payload.messages, rules)
            .map_err(|violation| ProviderWireError::MessageRule(violation.to_string()))?;
        validate_native_options(&payload.native_options, rules)
            .map_err(|violation| ProviderWireError::MessageRule(violation.to_string()))?;
        let mut body = anthropic_body(payload, model)?;
        if stream {
            body["stream"] = serde_json::Value::Bool(true);
        }
        Ok(body)
    }

    #[instrument(skip(self, req))]
    pub async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
        // The request's frozen binding is authoritative; ambient environment or
        // a provider-wide override cannot rewrite an in-flight request.
        self.validate_binding(req)?;
        let payload = &req.payload;
        let body = self.body(payload, req.provider_binding.model(), false)?;
        let client = self.client()?;

        let resp = self
            .apply_auth(client.post(format!(
                "{}/v1/messages",
                self.profile.endpoint_origin.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        // Retain only the exact rate-limit names the observer contract owns;
        // every other response header is ignored.
        let ratelimit_headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| name.as_str().starts_with(crate::QUOTA_HEADER_PREFIX))
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
                "Anthropic API HTTP {status}; terminal outcome unknown"
            ));
        }

        let raw_response = resp.text().await?;
        let raw: serde_json::Value = serde_json::from_str(&raw_response)?;
        let (content, citations) = anthropic_content_and_citations(&raw);

        let stop_reason = raw["stop_reason"].as_str().unwrap_or_default().to_string();

        let usage = raw["usage"].as_object().map(|u| TokenUsage {
            input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()),
            output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()),
            cache_read_tokens: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            cache_write_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64()),
        });

        info!(model=%self.profile.model_id, stop_reason=%stop_reason, in_tokens=?usage.as_ref().and_then(|u|u.input_tokens), out_tokens=?usage.as_ref().and_then(|u|u.output_tokens), "inference complete");
        Ok(InferenceResponse {
            content,
            stop_reason,
            raw_response,
            usage,
            ratelimit_headers,
            citations,
        })
    }

    /// Count the tokens one request would send. The same version rules apply
    /// as for the ordinary path.
    pub async fn count_tokens(&self, req: &InferenceRequest) -> Result<TokenCount> {
        self.validate_binding(req)?;
        let mut body = self.body(&req.payload, req.provider_binding.model(), false)?;
        body.as_object_mut().expect("provider request body is a JSON object").remove("max_tokens");
        let client = self.client()?;
        let resp = self
            .apply_auth(client.post(format!(
                "{}/v1/messages/count_tokens",
                self.profile.endpoint_origin.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(anyhow!("Anthropic token count HTTP {status}; outcome unknown"));
        }
        let raw: serde_json::Value = resp.json().await?;
        let input_tokens = raw
            .get("input_tokens")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow!("provider did not report an input token count"))?;
        Ok(TokenCount { input_tokens, binding: req.provider_binding.clone() })
    }

    /// Submit one Message Batch. Every entry is validated with the same
    /// version rules as an ordinary request, and the batch keeps the binding
    /// it was created with.
    pub async fn submit_batch(
        &self,
        req: &InferenceRequest,
        entries: &[BatchEntry],
    ) -> Result<BatchOutcome> {
        self.validate_binding(req)?;
        let requests: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| {
                let mut params = self.body(&entry.payload, req.provider_binding.model(), false)?;
                params.as_object_mut().expect("provider request body is a JSON object").remove("stream");
                serde_json::json!({ "custom_id": entry.custom_id, "params": params })
            })
            .collect();
        let body = serde_json::json!({ "requests": requests });
        let client = self.client()?;
        let resp = self
            .apply_auth(client.post(format!(
                "{}/v1/messages/batches",
                self.profile.endpoint_origin.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(anyhow!("Anthropic batch HTTP {status}; outcome unknown"));
        }
        let raw: serde_json::Value = resp.json().await?;
        Ok(BatchOutcome {
            batch_id: raw.get("id").and_then(|value| value.as_str()).unwrap_or_default().to_string(),
            status: raw.get("processing_status").and_then(|value| value.as_str()).unwrap_or_default().to_string(),
        })
    }

    /// Stream one request. The public projection and the durable journal are
    /// fed from the provider's own stream; the terminal receipt is independent
    /// of the caller.
    pub async fn stream(
        &self,
        req: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome> {
        self.validate_binding(req)?;
        let body = self.body(&req.payload, req.provider_binding.model(), true)?;
        let client = self.client()?;
        let resp = self
            .apply_auth(client.post(format!(
                "{}/v1/messages",
                self.profile.endpoint_origin.trim_end_matches('/')
            )))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let ratelimit_headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter(|(name, _)| name.as_str().starts_with(crate::QUOTA_HEADER_PREFIX))
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
                "Anthropic API HTTP {status}; terminal outcome unknown"
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
                for event in parse_anthropic_frame(&frame)? {
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

/// One entry of a Message Batch.
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub custom_id: String,
    pub payload: InferencePayload,
}

/// The receipt of a submitted batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchOutcome {
    pub batch_id: String,
    pub status: String,
}

fn credential_fingerprint(api_key: &str) -> String {
    shenko_types::object::Digest::blake3(api_key.as_bytes()).0
}

/// The Anthropic profile of one configured binding.
fn anthropic_profile(
    base_url: &str,
    model: &str,
    auth_method: AuthMethod,
    fingerprint: Option<String>,
    resource_domain_id: &str,
) -> ProviderProfile {
    ProviderProfile {
        profile_id: "anthropic.messages".into(),
        revision: SERIALIZER_REVISION,
        backend_family: BackendFamily::Anthropic,
        transport: TransportKind::HttpJson,
        endpoint_origin: base_url.to_string(),
        model_id: model.to_string(),
        api_version: Some(ANTHROPIC_API_VERSION.to_string()),
        sdk: None,
        request_schema_revision: SERIALIZER_REVISION,
        response_schema_revision: SERIALIZER_REVISION,
        gateway_profile: None,
        beta_features: Vec::new(),
        sampling: SamplingCapabilities {
            allowed: Vec::new(),
            forbidden: vec!["temperature".into(), "top_p".into(), "top_k".into()],
        },
        auth: AuthBinding {
            method: auth_method,
            header: match auth_method {
                AuthMethod::Bearer => "authorization".into(),
                AuthMethod::ApiKeyHeader => "x-api-key".into(),
                AuthMethod::None => String::new(),
            },
            credential_fingerprint: fingerprint,
        },
        usage: UsageFieldMapping {
            input_tokens_path: Some("usage.input_tokens".into()),
            output_tokens_path: Some("usage.output_tokens".into()),
            cache_read_tokens_path: Some("usage.cache_read_input_tokens".into()),
            cache_write_tokens_path: Some("usage.cache_creation_input_tokens".into()),
        },
        native_namespaces: Vec::new(),
        resource_domain_id: resource_domain_id.to_string(),
        engine_incarnation: None,
        default_max_tokens: crate::request::DEFAULT_MAX_TOKENS,
        serializer_revision: SERIALIZER_REVISION,
    }
}

#[async_trait::async_trait]
impl ProviderRuntime for AnthropicProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn binding(&self) -> ProviderBinding {
        AnthropicProvider::binding(self)
    }

    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        AnthropicProvider::complete(self, request).await
    }

    async fn stream(
        &self,
        request: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome> {
        AnthropicProvider::stream(self, request, publisher).await
    }

    async fn count_tokens(&self, request: &InferenceRequest) -> Result<TokenCount> {
        AnthropicProvider::count_tokens(self, request).await
    }

    async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
        let client = self.client()?;
        let mut evidence: Vec<serde_json::Value> = Vec::new();
        match self
            .apply_auth(client.get(format!(
                "{}/v1/models",
                self.profile.endpoint_origin.trim_end_matches('/')
            )))
            .query(&[("limit", "1000")])
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
        let operations = classify_operations(&self.profile.backend_family, &evidence, false);
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
            capacities: vec![CapacityEntry {
                pool: crate::admission::CapacityPool::Compute,
                unit: CapacityUnit::Requests,
                value: None,
                provenance: crate::admission::CapacityProvenance::Unknown,
            }],
            raw_evidence: evidence,
        })
    }
}

fn anthropic_body(payload: &InferencePayload, model: &str) -> Result<serde_json::Value> {
    let mut messages = Vec::with_capacity(payload.messages.len());
    for message in &payload.messages {
        messages.push(serde_json::json!({
            "role": message.role,
            "content": anthropic_content(&message.parts)?,
        }));
    }

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": payload.max_tokens.unwrap_or(crate::request::DEFAULT_MAX_TOKENS),
        "messages": messages,
    });

    if !payload.system_prompt.is_empty() {
        body["system"] = serde_json::Value::String(payload.system_prompt.clone());
    }
    if !payload.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(payload.tools.clone());
    }
    merge_native_options(&mut body, &payload.native_options, true)?;
    Ok(body)
}

/// The plain text and the citations of one Anthropic response.
fn anthropic_content_and_citations(raw: &serde_json::Value) -> (String, Vec<ProviderCitation>) {
    let mut content = String::new();
    let mut citations = Vec::new();
    let Some(blocks) = raw["content"].as_array() else {
        return (content, citations);
    };
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    content.push_str(text);
                }
                if let Some(items) = block["citations"].as_array() {
                    for citation in items {
                        citations.push(ProviderCitation {
                            text: citation["text"].as_str().unwrap_or_default().to_string(),
                            cited_text: citation["cited_text"].as_str().unwrap_or_default().to_string(),
                            document_index: citation["document_index"].as_u64().map(|value| value as u32),
                            document_title: citation["document_title"].as_str().map(str::to_string),
                        });
                    }
                }
            }
            Some("tool_use") => {
                content.push_str("");
            }
            _ => {}
        }
    }
    (content, citations)
}

/// Anthropic message content: the plain string a text-only message keeps, and
/// ordered blocks once an image or a document is present.
fn anthropic_content(parts: &[InferencePart]) -> Result<serde_json::Value> {
    let structured = parts
        .iter()
        .any(|part| !matches!(part, InferencePart::Text { .. }));
    if !structured {
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
                let mut block = serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                    },
                });
                block["source"]["data"] = serde_json::Value::String(base64_image_bytes(bytes));
                blocks.push(block);
            }
            InferencePart::Document { source, media_type, title, citations_enabled } => {
                let mut block = serde_json::json!({
                    "type": "document",
                    "source": {
                        "type": "text",
                        "data": source,
                    },
                });
                if let Some(media_type) = media_type {
                    block["source"]["media_type"] = serde_json::Value::String(media_type.clone());
                }
                if let Some(title) = title {
                    block["title"] = serde_json::Value::String(title.clone());
                }
                if *citations_enabled {
                    block["citations"] = serde_json::json!({ "enabled": true });
                }
                blocks.push(block);
            }
            InferencePart::Citation { text, cited_text, document_index, document_title } => {
                let mut block = serde_json::json!({
                    "type": "text",
                    "text": text,
                });
                let mut citation = serde_json::json!({
                    "type": "char_location",
                    "cited_text": cited_text,
                });
                if let Some(index) = document_index {
                    citation["document_index"] = serde_json::Value::from(*index);
                }
                if let Some(title) = document_title {
                    citation["document_title"] = serde_json::Value::String(title.clone());
                }
                block["citations"] = serde_json::Value::Array(vec![citation]);
                blocks.push(block);
            }
            InferencePart::SearchResult { source, title, content, citations_enabled } => {
                let mut block = serde_json::json!({
                    "type": "search_result",
                    "source": source,
                    "content": content,
                });
                if let Some(title) = title {
                    block["title"] = serde_json::Value::String(title.clone());
                }
                if *citations_enabled {
                    block["citations"] = serde_json::json!({ "enabled": true });
                }
                blocks.push(block);
            }
        }
    }
    Ok(serde_json::Value::Array(blocks))
}

pub(crate) fn merge_native_options(
    body: &mut serde_json::Value,
    options: &BTreeMap<String, serde_json::Value>,
    anthropic_2026_10_21: bool,
) -> Result<(), ProviderWireError> {
    const RESERVED: &[&str] = &[
        "model",
        "max_tokens",
        "max_completion_tokens",
        "messages",
        "system",
        "tools",
        "stream",
    ];
    for key in options.keys() {
        if RESERVED.contains(&key.as_str()) {
            return Err(ProviderWireError::ReservedOption(key.clone()));
        }
        if anthropic_2026_10_21 && matches!(key.as_str(), "temperature" | "top_p" | "top_k") {
            return Err(ProviderWireError::ForbiddenSampling(key.clone()));
        }
    }
    let object = body
        .as_object_mut()
        .expect("provider request body is a JSON object");
    object.extend(
        options
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    Ok(())
}

/// The current wall clock in milliseconds.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The rule violations of this binding, exposed for diagnostics.
pub fn rule_violations(
    messages: &[crate::request::InferenceMessage],
    api_version: &str,
) -> Vec<AnthropicRuleViolation> {
    match validate_messages(messages, api_version) {
        Ok(()) => Vec::new(),
        Err(violation) => vec![violation],
    }
}
