//! Anthropic Claude API provider — SPEC §47 PV01/PV02
//! Wire contract: Anthropic Messages API 2026-10-21 (no temperature/top_p/top_k).
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;
use tracing::{info, instrument};

use crate::request::{
    InferencePayload, InferenceRequest, InferenceResponse, ProviderBinding, ProviderRejection,
    ProviderWireError, TokenUsage,
};

pub(crate) const ANTHROPIC_API_VERSION: &str = "2026-10-21";
const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub(crate) const SERIALIZER_REVISION: u32 = 2;

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            base_url: "https://api.anthropic.com".to_string(),
            model: DEFAULT_MODEL.to_string(),
        }
    }

    pub fn from_env() -> Option<Self> {
        std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|key| !key.is_empty())
            .map(Self::new)
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    fn validate_binding<'a>(&self, req: &'a InferenceRequest) -> Result<&'a ProviderBinding> {
        let ProviderBinding::Anthropic {
            base_url,
            model,
            api_version,
            serializer_revision,
            default_max_tokens,
        } = &req.provider_binding
        else {
            return Err(ProviderWireError::BindingMismatch.into());
        };
        if base_url != &self.base_url
            || model != &self.model
            || api_version != ANTHROPIC_API_VERSION
            || *serializer_revision != SERIALIZER_REVISION
            || *default_max_tokens != crate::request::DEFAULT_MAX_TOKENS
        {
            return Err(ProviderWireError::BindingMismatch.into());
        }
        Ok(&req.provider_binding)
    }

    #[instrument(skip(self, req))]
    pub async fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse> {
        // The request's frozen binding is authoritative; ambient environment or
        // a provider-wide override cannot rewrite an in-flight request.
        self.validate_binding(req)?;
        let ProviderBinding::Anthropic {
            base_url,
            model,
            api_version,
            ..
        } = &req.provider_binding
        else {
            unreachable!("binding checked above")
        };
        let payload = &req.payload;
        let body = anthropic_body(payload, model)?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()?;

        let resp = client
            .post(format!("{}/v1/messages", base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", api_version)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            if let Some(rejection) = ProviderRejection::from_status(status) {
                return Err(rejection.into());
            }
            return Err(anyhow!(
                "Anthropic API HTTP {status}; terminal outcome unknown"
            ));
        }

        let raw_response = resp.text().await?;
        let raw: serde_json::Value = serde_json::from_str(&raw_response)?;

        let content = raw["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<String>()
            })
            .unwrap_or_default();

        let stop_reason = raw["stop_reason"].as_str().unwrap_or_default().to_string();

        let usage = raw["usage"].as_object().map(|u| TokenUsage {
            input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            cache_read_tokens: u.get("cache_read_input_tokens").and_then(|v| v.as_u64()),
            cache_write_tokens: u
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64()),
        });

        info!(model=%model, stop_reason=%stop_reason, in_tokens=?usage.as_ref().map(|u|u.input_tokens), out_tokens=?usage.as_ref().map(|u|u.output_tokens), "inference complete");
        Ok(InferenceResponse {
            content,
            stop_reason,
            raw_response,
            usage,
        })
    }
}

fn anthropic_body(payload: &InferencePayload, model: &str) -> Result<serde_json::Value> {
    let messages: Vec<serde_json::Value> = payload
        .messages
        .iter()
        .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
        .collect();

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
