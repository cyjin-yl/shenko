//! Anthropic 2026-10-21 message rules, scoped per binding — docs/specs/inference.md#spec-47-3–docs/specs/inference.md#spec-47-5.
//!
//! The rules are version scoped: a binding that still uses an older API
//! version keeps the older rules, and a binding that uses the current version
//! gets the current restrictions. They apply to every path that renders a
//! request — ordinary messages, streaming, token counting and Batch — so the
//! same input is never accepted on one path and rejected on another.
//!
//! A rule violation is a diagnosable outcome, not a silent rewrite: the
//! original session keeps its history and the caller decides what to change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::request::InferenceMessage;

/// The current Anthropic Messages API version this product binds.
pub const ANTHROPIC_API_VERSION: &str = "2026-10-21";

/// The rule set of one API version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnthropicRuleSet {
    /// `temperature`, `top_p` and `top_k` are rejected on the wire.
    pub sampling_forbidden: bool,
    /// A user text block may not carry non-empty citations.
    pub user_citations_forbidden: bool,
    /// An assistant message may not contain a document block directly.
    pub assistant_document_forbidden: bool,
}

impl AnthropicRuleSet {
    /// The rules of one API version. An unknown version keeps the older,
    /// permissive rules and is reported as unknown rather than assumed to be
    /// the current one.
    pub fn for_api_version(api_version: &str) -> Self {
        if api_version >= ANTHROPIC_API_VERSION {
            Self {
                sampling_forbidden: true,
                user_citations_forbidden: true,
                assistant_document_forbidden: true,
            }
        } else {
            Self {
                sampling_forbidden: false,
                user_citations_forbidden: false,
                assistant_document_forbidden: false,
            }
        }
    }
}

/// A rule this binding's version does not allow.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnthropicRuleViolation {
    #[error("an assistant message may not contain a document block directly (line {0})")]
    AssistantDocumentNotAllowed(usize),
    #[error("a user message may not carry non-empty citations (line {0})")]
    UserCitationNotAllowed(usize),
    #[error("sampling option {0} is forbidden by this API version")]
    ForbiddenSampling(String),
    #[error("native option {0} must use the sealed envelope field")]
    ReservedOption(String),
}

/// Validate the messages of one request against one API version.
pub fn validate_messages(
    messages: &[InferenceMessage],
    api_version: &str,
) -> Result<(), AnthropicRuleViolation> {
    let rules = AnthropicRuleSet::for_api_version(api_version);
    for (index, message) in messages.iter().enumerate() {
        let line = index + 1;
        for part in &message.parts {
            match part {
                crate::request::InferencePart::Document { .. } => {
                    if rules.assistant_document_forbidden && message.role == "assistant" {
                        return Err(AnthropicRuleViolation::AssistantDocumentNotAllowed(line));
                    }
                }
                crate::request::InferencePart::Citation { .. } => {
                    if rules.user_citations_forbidden && message.role == "user" {
                        return Err(AnthropicRuleViolation::UserCitationNotAllowed(line));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// The envelope fields the serializer owns. A native option that collides
/// with one of them is rejected instead of overriding the frozen binding.
pub const RESERVED_ENVELOPE_FIELDS: &[&str] = &[
    "model",
    "max_tokens",
    "max_completion_tokens",
    "messages",
    "system",
    "tools",
    "stream",
];

/// The sampling keys the current Anthropic API version rejects.
pub const FORBIDDEN_SAMPLING_KEYS: &[&str] = &["temperature", "top_p", "top_k"];

/// Validate the native options of one request against one API version.
///
/// Unknown options are preserved: the caller's intent is not silently
/// dropped. Only a collision with a sealed envelope field or a sampling key
/// the version forbids is an error.
pub fn validate_native_options(
    options: &BTreeMap<String, serde_json::Value>,
    api_version: &str,
) -> Result<(), AnthropicRuleViolation> {
    let rules = AnthropicRuleSet::for_api_version(api_version);
    for key in options.keys() {
        if RESERVED_ENVELOPE_FIELDS.contains(&key.as_str()) {
            return Err(AnthropicRuleViolation::ReservedOption(key.clone()));
        }
        if rules.sampling_forbidden && FORBIDDEN_SAMPLING_KEYS.contains(&key.as_str()) {
            return Err(AnthropicRuleViolation::ForbiddenSampling(key.clone()));
        }
    }
    Ok(())
}

/// A citation block the provider returned, kept verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderCitation {
    /// The citing text the provider returned.
    pub text: String,
    /// The source text that was cited.
    #[serde(default)]
    pub cited_text: String,
    #[serde(default)]
    pub document_index: Option<u32>,
    #[serde(default)]
    pub document_title: Option<String>,
}
