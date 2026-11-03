//! Native backend runtimes — NInfer, Strata, vLLM and 1Cat-vLLM.
//!
//! The four native targets speak an OpenAI-compatible chat surface with their
//! own extensions. The runtime keeps their real differences: the native
//! namespaces a profile declares, the transport it actually uses, the usage
//! counters the backend reports and the diagnostics endpoint that family
//! exposes. A native extension is only claimed where the deployment showed
//! it; the profile's namespace name is not itself a guarantee.

use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::capability::{BackendCapabilitySnapshot, CapacityEntry, CapacityUnit, classify_operations};
use crate::profile::{AuthMethod, BackendFamily, ProviderProfile};
use crate::request::{
    InferencePayload, InferenceRequest, InferenceResponse, ProviderBinding, ProviderRejection,
    TokenUsage,
};
use crate::stream::{
    BoundedStreamPublisher, SseDecoder, StreamEvent, StreamOutcome, parse_openai_frame,
};

/// One native backend binding.
#[derive(Debug, Clone)]
pub struct NativeBackendProvider {
    profile: ProviderProfile,
    api_key: Option<String>,
    allow_invalid_certs: bool,
}

impl NativeBackendProvider {
    pub fn new(profile: ProviderProfile, api_key: Option<String>) -> Result<Self> {
        profile.validate()?;
        Ok(Self { profile, api_key, allow_invalid_certs: false })
    }

    pub fn with_insecure_certs(mut self, allow: bool) -> Self {
        self.allow_invalid_certs = allow;
        self
    }

    fn base_url(&self) -> String {
        self.profile.endpoint_origin.trim_end_matches('/').to_string()
    }

    fn client(&self) -> Result<reqwest::Client> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(180));
        if self.allow_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        // A sealed request is bound to this endpoint. A redirect would replay
        // the frozen payload on another host while the receipt still names
        // this one, so a redirect is reported as an unknown outcome.
        builder = builder.redirect(reqwest::redirect::Policy::none());
        Ok(builder.build()?)
    }

    /// Validate the sealed binding against this runtime's profile.
    fn validate_binding(&self, request: &InferenceRequest) -> Result<()> {
        let ProviderBinding::Profile { profile } = &request.provider_binding else {
            return Err(anyhow!("MODEL_PROVIDER_BINDING_MISMATCH: native runtime received a foreign binding"));
        };
        if !self.profile.matches_except_model(profile) {
            return Err(anyhow!(
                "MODEL_PROVIDER_BINDING_MISMATCH: the sealed profile differs from the installed runtime"
            ));
        }
        Ok(())
    }

    /// The body of one chat request. Native options are preserved verbatim:
    /// an unknown native field is passed on, and whether the backend honoured
    /// it is a fact the capability snapshot records separately.
    fn body(
        &self,
        payload: &InferencePayload,
        model: &str,
        stream: bool,
    ) -> Result<serde_json::Value> {
        let mut messages: Vec<serde_json::Value> = Vec::with_capacity(payload.messages.len() + 1);
        if !payload.system_prompt.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": payload.system_prompt,
            }));
        }
        for message in &payload.messages {
            messages.push(serde_json::json!({
                "role": message.role,
                "content": crate::local_provider::openai_content(&message.parts)?,
            }));
        }
        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": payload.max_tokens.unwrap_or(self.profile.default_max_tokens),
            "messages": messages,
        });
        if !payload.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(payload.tools.clone());
        }
        for (key, value) in &payload.native_options {
            body[key] = value.clone();
        }
        if stream {
            body["stream"] = serde_json::Value::Bool(true);
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }
        Ok(body)
    }

    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(key) = &self.api_key else { return builder };
        match self.profile.effective_auth() {
            AuthMethod::Bearer => builder.header("authorization", format!("Bearer {key}")),
            AuthMethod::ApiKeyHeader => builder.header("x-api-key", key),
            AuthMethod::None => builder,
        }
    }

    fn ratelimit_headers(response: &reqwest::Response) -> Vec<(String, String)> {
        response
            .headers()
            .iter()
            .filter(|(name, _)| name.as_str().starts_with(crate::QUOTA_HEADER_PREFIX))
            .map(|(name, value)| {
                (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string())
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl crate::registry::ProviderRuntime for NativeBackendProvider {
    fn profile(&self) -> &ProviderProfile {
        &self.profile
    }

    fn binding(&self) -> ProviderBinding {
        self.profile.binding()
    }

    async fn complete(&self, request: &InferenceRequest) -> Result<InferenceResponse> {
        self.validate_binding(request)?;
        let body = self.body(&request.payload, request.provider_binding.model(), false)?;
        let client = self.client()?;
        let response = self
            .apply_auth(client.post(format!("{}/v1/chat/completions", self.base_url())))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let ratelimit_headers = Self::ratelimit_headers(&response);
        if !response.status().is_success() {
            let status = response.status();
            if let Some(rejection) = ProviderRejection::from_status(status, ratelimit_headers) {
                return Err(rejection.into());
            }
            return Err(anyhow!("native backend HTTP {status}; terminal outcome unknown"));
        }
        let raw_response = response.text().await?;
        let raw: serde_json::Value = serde_json::from_str(&raw_response)?;
        let content = raw
            .pointer("/choices/0/message/content")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let stop_reason = raw
            .pointer("/choices/0/finish_reason")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        let usage = self.profile.read_usage(&raw);
        Ok(InferenceResponse {
            content,
            stop_reason,
            raw_response,
            usage: Some(usage),
            ratelimit_headers,
            citations: Vec::new(),
        })
    }

    async fn stream(
        &self,
        request: &InferenceRequest,
        publisher: &BoundedStreamPublisher,
    ) -> Result<StreamOutcome> {
        self.validate_binding(request)?;
        let body = self.body(&request.payload, request.provider_binding.model(), true)?;
        let client = self.client()?;
        let mut response = self
            .apply_auth(client.post(format!("{}/v1/chat/completions", self.base_url())))
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;
        let ratelimit_headers = Self::ratelimit_headers(&response);
        if !response.status().is_success() {
            let status = response.status();
            if let Some(rejection) = ProviderRejection::from_status(status, ratelimit_headers) {
                return Err(rejection.into());
            }
            return Err(anyhow!("native backend HTTP {status}; terminal outcome unknown"));
        }
        let mut decoder = SseDecoder::new();
        let mut raw_frames: Vec<String> = Vec::new();
        let mut content = String::new();
        let mut stop_reason = String::new();
        let mut usage: Option<TokenUsage> = None;
        let mut events = 0u64;
        let mut batches = 0u64;
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await? {
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
        let raw_response = raw_frames.join("\n");
        let usage = usage.or_else(|| {
            let raw: serde_json::Value = serde_json::from_str(&raw_response).ok()?;
            Some(self.profile.read_usage(&raw))
        });
        Ok(StreamOutcome {
            response: InferenceResponse {
                content,
                stop_reason,
                raw_response,
                usage,
                ratelimit_headers,
                citations: Vec::new(),
            },
            events,
            batches,
        })
    }

    async fn count_tokens(&self, request: &InferenceRequest) -> Result<crate::registry::TokenCount> {
        self.validate_binding(request)?;
        // The OpenAI-compatible surface has no token counting entry point. The
        // count stays unknown instead of being estimated from characters.
        Err(anyhow!("token counting is not available on this binding"))
    }

    async fn probe(&self) -> Result<BackendCapabilitySnapshot> {
        let client = self.client()?;
        let mut evidence: Vec<serde_json::Value> = Vec::new();
        let metadata = self
            .apply_auth(client.get(format!("{}/v1/models", self.base_url())))
            .send()
            .await;
        match metadata {
            Ok(response) if response.status().is_success() => {
                let raw = response.text().await?;
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                    evidence.push(value);
                }
            }
            Ok(response) => {
                evidence.push(serde_json::json!({
                    "probe_error": format!("HTTP {}", response.status()),
                }));
            }
            Err(error) => {
                evidence.push(serde_json::json!({ "probe_error": error.to_string() }));
            }
        }
        // 1Cat exposes its own SM70 acceleration diagnostics; the answer is
        // kept as evidence, whether or not it succeeded.
        if matches!(self.profile.backend_family, BackendFamily::OneCat) {
            let diagnostics = self
                .apply_auth(client.get(format!("{}/v1/sm70/acceleration", self.base_url())))
                .send()
                .await;
            match diagnostics {
                Ok(response) => {
                    let raw = response.text().await.unwrap_or_default();
                    match serde_json::from_str::<serde_json::Value>(&raw) {
                        Ok(value) => evidence.push(value),
                        Err(_) => evidence.push(serde_json::json!({
                            "probe_error": "diagnostics response is not JSON",
                        })),
                    }
                }
                Err(error) => {
                    evidence.push(serde_json::json!({ "probe_error": error.to_string() }));
                }
            }
        }
        let operations = classify_operations(&self.profile.backend_family, &evidence, false);
        let snapshot = BackendCapabilitySnapshot {
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
                CapacityEntry {
                    pool: crate::admission::CapacityPool::ContextKv,
                    unit: CapacityUnit::Tokens,
                    value: None,
                    provenance: crate::admission::CapacityProvenance::Unknown,
                },
            ],
            raw_evidence: evidence,
        };
        Ok(snapshot)
    }
}

/// The current wall clock in milliseconds. The publisher batches on this
/// clock; it is not a deadline and it never releases capacity.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

