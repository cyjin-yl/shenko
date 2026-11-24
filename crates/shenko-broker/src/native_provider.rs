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

use crate::capability::{
    BackendCapabilitySnapshot, CacheSurfaces, CapacityEntry, CapacityUnit, DeclaredCapability,
    ProviderCapabilityDeclaration, QuotaSurfaces, classify_operations,
};
use crate::profile::{AuthMethod, BackendFamily, ProviderProfile};
use crate::provider::merge_native_options;
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
        let ProviderBinding::Profile { profile, prepared } = &request.provider_binding else {
            return Err(anyhow!("MODEL_PROVIDER_BINDING_MISMATCH: native runtime received a foreign binding"));
        };
        // This runtime executes a compiled-in transport profile. A binding that
        // names a prepared implementation generation belongs to that prepared
        // adapter; serving it here would run different bytes under identical
        // transport metadata.
        if prepared.is_some() {
            return Err(anyhow!(
                "MODEL_PROVIDER_BINDING_MISMATCH: the sealed binding names a prepared implementation generation"
            ));
        }
        if !self.profile.matches_except_model(profile) {
            return Err(anyhow!(
                "MODEL_PROVIDER_BINDING_MISMATCH: the sealed profile differs from the installed runtime"
            ));
        }
        Ok(())
    }

    /// The body of one chat request. Native options are preserved verbatim:
    /// an unknown native field is passed on, and whether the backend honoured
    /// it is a fact the capability snapshot records separately. The envelope
    /// fields are not native options, so a sealed option cannot rewrite the
    /// model, the messages, the output reservation or the transport: it is an
    /// explicit error instead of a silent re-interpretation of the seal.
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
        merge_native_options(&mut body, &payload.native_options, false)?;
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

    fn declaration(&self) -> ProviderCapabilityDeclaration {
        // Facts of this implementation: it sends the OpenAI-compatible
        // streaming request and parses the provider's own SSE frames, and it
        // reads whatever usage counters the sealed profile maps. The profile's
        // native namespace is not a cache declaration, and none of the four
        // families owns a quota observation contract on this surface.
        ProviderCapabilityDeclaration {
            streaming: DeclaredCapability::implemented(),
            cache: CacheSurfaces {
                cache_read_accounting: if self.profile.usage.cache_read_tokens_path.is_some() {
                    DeclaredCapability::implemented()
                } else {
                    DeclaredCapability::UNDECLARED
                },
                ..CacheSurfaces::UNDECLARED
            },
            quota: QuotaSurfaces::UNDECLARED,
        }
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
            reasoning: None,
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
                reasoning: None,
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
                    "probe_endpoint": "/v1/models", "probe_http_status": response.status().as_u16(),
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
        // The engine's own metrics surface is where a real KV pool figure lives.
        // It is one idempotent read, never a generation and never retried; a
        // backend that does not expose it keeps the pool unknown. Strata speaks
        // its own engine protocol, so no metrics endpoint is assumed for it.
        if !matches!(
            self.profile.backend_family,
            BackendFamily::Strata | BackendFamily::Anthropic
        ) {
            evidence.push(
                crate::capability::metrics_evidence(&client, &self.base_url(), |builder| {
                    self.apply_auth(builder)
                })
                .await,
            );
        }
        let operations = classify_operations(&self.profile.backend_family, &evidence);
        let kv = crate::capability::discover_kv_capacity(&self.profile.backend_family, &evidence);
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
                    value: kv.as_ref().map(|observed| observed.tokens),
                    provenance: kv
                        .as_ref()
                        .map(|observed| observed.provenance)
                        .unwrap_or(crate::admission::CapacityProvenance::Unknown),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{InferenceMessage, ProviderWireError};
    use std::collections::BTreeMap;

    fn provider() -> NativeBackendProvider {
        NativeBackendProvider::new(
            crate::profile::native_backends::vllm(
                "http://127.0.0.1:8000",
                "fixture-model",
                "fixture-domain",
            ),
            None,
        )
        .expect("a native vLLM profile is constructible")
    }

    fn payload(native_options: BTreeMap<String, serde_json::Value>) -> InferencePayload {
        InferencePayload {
            system_prompt: "system".into(),
            messages: vec![InferenceMessage::text("user", "hello")],
            max_tokens: Some(64),
            tools: Vec::new(),
            native_options,
        }
    }

    fn one(key: &str, value: serde_json::Value) -> BTreeMap<String, serde_json::Value> {
        BTreeMap::from([(key.to_string(), value)])
    }

    /// The sealed envelope is not a native-option namespace. A payload that
    /// named `model` would otherwise rewrite the wire model while the seal and
    /// the effective inference still reported the frozen one.
    #[test]
    fn a_sealed_native_option_cannot_rewrite_the_envelope() {
        let provider = provider();
        for reserved in ["model", "messages", "max_tokens", "stream"] {
            let payload = payload(one(reserved, serde_json::json!("rewritten")));
            let error = provider
                .body(&payload, "fixture-model", false)
                .expect_err("an envelope field is not a native option");
            let wire = error
                .downcast_ref::<ProviderWireError>()
                .expect("the rejection is a typed wire error");
            assert!(
                matches!(wire, ProviderWireError::ReservedOption(key) if key.as_str() == reserved),
                "{reserved} must be rejected as an envelope field"
            );
        }
    }

    /// A residency option the sealer named travels verbatim on both the
    /// blocking and the streaming wire; the runtime never rewrites it, so the
    /// affinity that was sealed is the affinity the backend receives.
    #[test]
    fn a_sealed_residency_option_reaches_the_wire_verbatim() {
        let provider = provider();
        let payload = payload(BTreeMap::from([
            ("cache_salt".to_string(), serde_json::json!("context-7")),
            (
                "kv_transfer_params".to_string(),
                serde_json::json!({ "do_remote_decode": false }),
            ),
        ]));
        let blocking = provider
            .body(&payload, "fixture-model", false)
            .expect("a residency option is passed through");
        assert_eq!(blocking["cache_salt"], serde_json::json!("context-7"));
        assert_eq!(
            blocking["kv_transfer_params"],
            serde_json::json!({ "do_remote_decode": false })
        );
        assert_eq!(blocking["model"], serde_json::json!("fixture-model"));
        assert_eq!(blocking["max_tokens"], serde_json::json!(64));
        assert!(blocking.get("stream").is_none());

        let streaming = provider
            .body(&payload, "fixture-model", true)
            .expect("a residency option is passed through on the stream surface");
        assert_eq!(streaming["cache_salt"], serde_json::json!("context-7"));
        assert_eq!(streaming["stream"], serde_json::json!(true));
        assert_eq!(
            streaming["stream_options"],
            serde_json::json!({ "include_usage": true })
        );
    }

    /// Rich text must survive the native selector with or without an image.
    /// The same expected projections bind the shared local renderer and both
    /// native request modes without contacting any backend.
    #[test]
    fn rich_parts_survive_with_and_without_images() {
        use crate::request::InferencePart;
        use serde_json::json;
        use std::sync::Arc;

        let provider = provider();
        let mut payload = payload(BTreeMap::new());
        let parts = vec![
            InferencePart::text("before\n"),
            InferencePart::Document {
                source: "document-source".into(),
                media_type: Some("text/plain".into()),
                title: Some("Document title".into()),
                citations_enabled: true,
            },
            InferencePart::text("\nbetween\n"),
            InferencePart::Citation {
                text: "citation text".into(),
                cited_text: "cited passage".into(),
                document_index: Some(0),
                document_title: Some("Document title".into()),
            },
            InferencePart::SearchResult {
                source: "search-source".into(),
                title: Some("Search title".into()),
                content: "search body".into(),
                citations_enabled: true,
            },
            InferencePart::text("\nafter"),
        ];
        let expected_text = concat!(
            "before\n[document text/plain] Document title (document-source)",
            "\nbetween\ncitation text[search-source] Search title\nsearch body\nafter",
        );
        let expected_blocks = json!([
            {"type": "text", "text": "before\n"},
            {"type": "text", "text": "[document text/plain] Document title (document-source)"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AQID"}},
            {"type": "text", "text": "\nbetween\n"},
            {"type": "text", "text": "citation text"},
            {"type": "text", "text": "[search-source] Search title\nsearch body"},
            {"type": "text", "text": "\nafter"},
        ]);
        for with_image in [false, true] {
            payload.messages[0].parts = parts.clone();
            if with_image {
                payload.messages[0].parts.insert(
                    2,
                    InferencePart::image("image/png", Arc::from([1u8, 2, 3])),
                );
            }
            let expected = if with_image { expected_blocks.clone() } else { json!(expected_text) };
            assert_eq!(
                crate::local_provider::openai_content(&payload.messages[0].parts).unwrap(),
                expected,
            );
            for stream in [false, true] {
                let body = provider.body(&payload, "fixture-model", stream).unwrap();
                assert_eq!(body["messages"][1]["content"], expected);
                assert_eq!(body["messages"][0], json!({"role": "system", "content": "system"}));
                assert_eq!(body["messages"][1]["role"], "user");
            }
        }
        assert_eq!(crate::local_provider::openai_content(&[]).unwrap(), json!(""));
        assert_eq!(
            crate::local_provider::openai_content(&[
                InferencePart::text("plain "), InferencePart::text("text\n"),
            ]).unwrap(),
            json!("plain text\n"),
        );
        for (media_type, bytes) in [("image/png", vec![]), ("text/plain", vec![1])] {
            payload.messages[0].parts = vec![
                InferencePart::text("before"),
                InferencePart::image(media_type, Arc::from(bytes)),
            ];
            assert!(crate::local_provider::openai_content(&payload.messages[0].parts).is_err());
            for stream in [false, true] {
                assert!(provider.body(&payload, "fixture-model", stream).is_err());
            }
        }
    }
}
