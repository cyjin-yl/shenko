//! Local model provider — OpenAI-compatible /v1/chat/completions. SPEC §22.1
//! Native local sampling options remain intact; Claude-only API restrictions do
//! not leak into this binding. Endpoint and model are read once at construction.
use anyhow::{Result, anyhow};
use base64::Engine as _;
use tracing::{info, instrument};

use crate::provider::merge_native_options;
use crate::request::{
    InferencePart, InferencePayload, InferenceRequest, InferenceResponse, ProviderBinding,
    ProviderRejection, ProviderWireError, TokenUsage, validate_image_part,
};

const DEFAULT_MODEL: &str = "qwen3.8-27b";
pub(crate) const SERIALIZER_REVISION: u32 = 3;

#[derive(Debug, Clone)]
pub struct LocalModelProvider {
    pub base_url: String,
    pub model: String,
    pub allow_invalid_certs: bool,
}

impl LocalModelProvider {
    pub fn new(base_url: String, model: String) -> Self {
        Self {
            base_url,
            model,
            allow_invalid_certs: false,
        }
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
        Ok(Self {
            base_url,
            model,
            allow_invalid_certs: std::env::var("SHENKO_LOCAL_INSECURE").as_deref() == Ok("1"),
        })
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
            || model != &self.model
            || allow_invalid_certs != &self.allow_invalid_certs
            || *serializer_revision != SERIALIZER_REVISION
            || *default_max_tokens != crate::request::DEFAULT_MAX_TOKENS
        {
            return Err(ProviderWireError::BindingMismatch.into());
        }
        Ok(&req.provider_binding)
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
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(180));

        if self.allow_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }

        let body = local_body(payload, model)?;
        let client = builder.build()?;
        let resp = client
            .post(format!(
                "{}/v1/chat/completions",
                base_url.trim_end_matches('/')
            ))
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
        })
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

fn openai_content(parts: &[InferencePart]) -> Result<serde_json::Value> {
    if !parts.iter().any(InferencePart::is_image) {
        let text = parts
            .iter()
            .filter_map(|part| match part {
                InferencePart::Text { text } => Some(text.as_str()),
                InferencePart::Image { .. } => None,
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
        }
    }
    Ok(serde_json::Value::Array(blocks))
}

