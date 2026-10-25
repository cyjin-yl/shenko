//! Local model provider — OpenAI-compatible /v1/chat/completions. SPEC §22.1
//! Native local sampling options remain intact; Claude-only API restrictions do
//! not leak into this binding. Endpoint and model are read once at construction.
use anyhow::{Result, anyhow};
use tracing::{info, instrument};

use crate::provider::merge_native_options;
use crate::request::{
    InferenceRequest, InferenceResponse, ProviderBinding, ProviderRejection, ProviderWireError,
    TokenUsage,
};

const DEFAULT_MODEL: &str = "qwen3.8-27b";
pub(crate) const SERIALIZER_REVISION: u32 = 2;

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

        let mut messages: Vec<serde_json::Value> = Vec::new();
        if !payload.system_prompt.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": payload.system_prompt,
            }));
        }
        for message in &payload.messages {
            messages.push(serde_json::json!({
                "role": message.role,
                "content": message.content,
            }));
        }

        let mut body = serde_json::json!({
            "model": model,
            "max_tokens": payload.max_tokens.unwrap_or(crate::request::DEFAULT_MAX_TOKENS),
            "messages": messages,
        });
        merge_native_options(&mut body, &payload.native_options, false)?;
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

        if !resp.status().is_success() {
            let status = resp.status();
            if let Some(rejection) = ProviderRejection::from_status(status) {
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
            input_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            cache_read_tokens: None,
            cache_write_tokens: None,
        });

        info!(
            model = %model,
            finish_reason = %finish_reason,
            in_tokens = ?usage.as_ref().map(|u| u.input_tokens),
            out_tokens = ?usage.as_ref().map(|u| u.output_tokens),
            "local inference complete"
        );

        Ok(InferenceResponse {
            content,
            stop_reason: finish_reason,
            raw_response,
            usage,
        })
    }
}
