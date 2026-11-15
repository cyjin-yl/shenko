// Derived from ucp-schema 1.4.1 (Apache-2.0). See README.md and LICENSE.
// Modified for Shenko: private modules and only the pure resolution API.

//! Core types for UCP schema resolution.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Schema transition: from/to are visibility values (omit, optional, required).
/// During the transition period the field is always the `from` visibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaTransitionInfo {
    pub from: String,
    pub to: String,
    pub description: String,
}

/// UCP annotation keys.
pub const UCP_ANNOTATIONS: &[&str] = &["ucp_request", "ucp_response"];

/// Returns the JSON type name for error messages.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Direction of the schema transformation.
///
/// Determines whether to use `ucp_request` or `ucp_response` annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Request,
    Response,
}

impl Direction {
    /// Returns the annotation key for this direction.
    pub fn annotation_key(&self) -> &'static str {
        match self {
            Direction::Request => "ucp_request",
            Direction::Response => "ucp_response",
        }
    }
}

/// Visibility of a field after resolution.
///
/// Determines how a field is transformed in the output schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Visibility {
    /// No transformation - keep field as-is with original required status.
    #[default]
    Include,
    /// Remove field from properties and required array.
    Omit,
    /// Keep field and ensure it's in the required array.
    Required,
    /// Keep field but remove from required array.
    Optional,
}

impl Visibility {
    /// Parse a visibility value from a string.
    ///
    /// Returns `None` for unknown values (caller should error).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "omit" => Some(Visibility::Omit),
            "required" => Some(Visibility::Required),
            "optional" => Some(Visibility::Optional),
            _ => None,
        }
    }
}

/// Returns true if (from, to) is a valid schema transition: both are visibility
/// values (omit, optional, required) and from != to.
pub fn is_valid_schema_transition(from: &str, to: &str) -> bool {
    from != to && Visibility::parse(from).is_some() && Visibility::parse(to).is_some()
}

// ---------------------------------------------------------------------------
// Version constraints (`requires` on extension schemas)
// ---------------------------------------------------------------------------

/// Check if a string is a valid UCP version (YYYY-MM-DD with valid month/day).
pub fn is_valid_version(s: &str) -> bool {
    if s.len() != 10 || s.as_bytes()[4] != b'-' || s.as_bytes()[7] != b'-' {
        return false;
    }
    if !s.bytes().enumerate().all(|(i, b)| {
        if i == 4 || i == 7 {
            b == b'-'
        } else {
            b.is_ascii_digit()
        }
    }) {
        return false;
    }
    let month: u8 = s[5..7].parse().unwrap_or(0);
    let day: u8 = s[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Version range: minimum (required) and optional maximum, both inclusive.
///
/// Date-based versions (YYYY-MM-DD) are lexicographically orderable,
/// so constraint checking is simple string comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionConstraint {
    pub min: String,
    pub max: Option<String>,
}

impl VersionConstraint {
    /// Check if a version satisfies this constraint.
    pub fn satisfied_by(&self, version: &str) -> bool {
        if version < self.min.as_str() {
            return false;
        }
        if let Some(ref max) = self.max {
            if version > max.as_str() {
                return false;
            }
        }
        true
    }

    /// Parse from a JSON value: `{ "min": "...", "max": "..." }`.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let obj = value.as_object().ok_or("expected object")?;

        let min = obj
            .get("min")
            .and_then(|v| v.as_str())
            .ok_or("missing required field \"min\"")?;

        if !is_valid_version(min) {
            return Err(format!(
                "invalid version format for \"min\": \"{}\" (expected YYYY-MM-DD)",
                min
            ));
        }

        let max = match obj.get("max") {
            Some(v) => {
                let s = v.as_str().ok_or("\"max\" must be a string")?;
                if !is_valid_version(s) {
                    return Err(format!(
                        "invalid version format for \"max\": \"{}\" (expected YYYY-MM-DD)",
                        s
                    ));
                }
                Some(s.to_string())
            }
            None => None,
        };

        Ok(Self {
            min: min.to_string(),
            max,
        })
    }
}

/// Extension schema version requirements (`requires` field).
///
/// Declares minimum (and optionally maximum) protocol and capability
/// versions needed for correct operation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Requires {
    pub protocol: Option<VersionConstraint>,
    pub capabilities: Vec<(String, VersionConstraint)>,
}

impl Requires {
    /// Parse from a JSON value: the top-level `requires` object.
    pub fn parse(value: &Value) -> Result<Self, Vec<String>> {
        let obj = value
            .as_object()
            .ok_or_else(|| vec!["\"requires\" must be an object".to_string()])?;
        let mut errors = Vec::new();

        let protocol = match obj.get("protocol") {
            Some(v) => match VersionConstraint::parse(v) {
                Ok(vc) => Some(vc),
                Err(e) => {
                    errors.push(format!("requires.protocol: {}", e));
                    None
                }
            },
            None => None,
        };

        let mut capabilities = Vec::new();
        if let Some(caps_val) = obj.get("capabilities") {
            match caps_val.as_object() {
                Some(caps) => {
                    for (key, val) in caps {
                        match VersionConstraint::parse(val) {
                            Ok(vc) => capabilities.push((key.clone(), vc)),
                            Err(e) => errors.push(format!("requires.capabilities.{}: {}", key, e)),
                        }
                    }
                }
                None => errors.push("requires.capabilities must be an object".to_string()),
            }
        }

        if errors.is_empty() {
            Ok(Self {
                protocol,
                capabilities,
            })
        } else {
            Err(errors)
        }
    }
}

/// Options for schema resolution.
#[derive(Debug, Clone)]
pub struct ResolveOptions {
    /// Whether resolving for request or response.
    pub direction: Direction,
    /// The operation to resolve for (e.g., "create", "update").
    /// Will be normalized to lowercase.
    pub operation: String,
    /// When true, sets `additionalProperties: false` on all object schemas
    /// to reject unknown fields. Defaults to false to respect schema extensibility.
    pub strict: bool,
    /// When true, includes fields with `omit` visibility that have a transition
    /// targeting a non-omit value (i.e., planned additions). These fields appear
    /// in the resolved output with `x-ucp-schema-transition` metadata but are NOT
    /// added to `required`. Completes the lifecycle symmetry: deprecations (to=omit)
    /// are always surfaced; this flag surfaces planned additions (from=omit) too.
    pub include_future: bool,
}

impl ResolveOptions {
    /// Create new resolve options with strict mode disabled (default).
    ///
    /// Operation is normalized to lowercase for case-insensitive matching.
    /// Strict mode is off by default to respect UCP's extensibility model:
    /// schemas validate known fields but allow additional properties.
    pub fn new(direction: Direction, operation: impl Into<String>) -> Self {
        Self {
            direction,
            operation: operation.into().to_lowercase(),
            strict: false,
            include_future: false,
        }
    }
}
