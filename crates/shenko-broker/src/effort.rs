//! Provider-owned effort semantics, distinct from quota prices and token limits.
//!
//! The installed runtime resolves a named request to this typed wire contract.
//! Unknown names and runtimes without a mapping remain unsupported. No model
//! default, cost estimate or quality ordering is inferred here.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A missing owner and an explicitly unsupported semantic request are
/// different facts. Neither supplies a default effort or dispatch authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EffortResolutionError {
    #[error("no installed runtime matches the exact effort binding")]
    ProviderUnavailable,
    #[error("bound provider has no native mapping for the requested effort")]
    Unsupported,
}

/// Explicit Messages API values for `output_config.effort`.
/// Source: https://platform.claude.com/docs/en/build-with-claude/effort
/// Effort is a behavioral signal; it is not a hard output-token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnthropicEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl AnthropicEffort {
    /// Only the provider's exact native names are accepted. Logical aliases
    /// such as `standard` or `deep` have no implicit translation.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// A provider-native semantic choice, frozen with a request. Another provider
/// must declare its own mapping; matching a model name or backend family grants
/// no support. The current adapter implements the Messages API contract only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderEffort {
    AnthropicMessages { level: AnthropicEffort },
}

impl ProviderEffort {
    /// Add the declared native field without replacing an existing conflicting
    /// choice or losing unrelated output configuration such as a format.
    pub fn apply(self, options: &mut BTreeMap<String, serde_json::Value>) -> anyhow::Result<()> {
        let Self::AnthropicMessages { level } = self;
        let output = options.entry("output_config".into())
            .or_insert_with(|| serde_json::json!({}));
        let object = output.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("effort requires an object output_config"))?;
        if let Some(existing) = object.get("effort") {
            anyhow::ensure!(existing.as_str() == Some(level.as_str()),
                "native output_config.effort conflicts with the selected effort");
        } else {
            object.insert("effort".into(), serde_json::Value::String(level.as_str().into()));
        }
        Ok(())
    }

    /// Recovery validates the frozen projection without consulting the current
    /// provider installation or translating the original request again.
    pub fn matches_native_options(self, options: &BTreeMap<String, serde_json::Value>) -> bool {
        let Self::AnthropicMessages { level } = self;
        options.get("output_config").and_then(|value| value.get("effort"))
            .and_then(serde_json::Value::as_str) == Some(level.as_str())
    }
}
