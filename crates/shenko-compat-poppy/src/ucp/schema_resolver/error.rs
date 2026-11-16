// Derived from ucp-schema 1.4.1 (Apache-2.0). See README.md and LICENSE.
// Modified for Shenko: only pure errors; explicit Display instead of thiserror.

use std::fmt;

#[derive(Debug)]
pub enum ResolveError {
    InvalidAnnotationType {
        path: String,
        actual: String,
    },
    UnknownVisibility {
        path: String,
        value: String,
    },
    InvalidSchemaTransition {
        path: String,
        message: String,
    },
    MonotonicityViolation {
        path: String,
        field: String,
        base_status: String,
        attempted: String,
    },
    TypeConflict {
        path: String,
        base_type: String,
        ext_type: String,
    },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAnnotationType { path, actual } => write!(
                f,
                "invalid annotation at {path}: expected string or object, got {actual}"
            ),
            Self::UnknownVisibility { path, value } => write!(
                f,
                "unknown visibility \"{value}\" at {path}: expected omit, required, or optional"
            ),
            Self::InvalidSchemaTransition { path, message } => {
                write!(f, "invalid schema transition at {path}: {message}")
            }
            Self::MonotonicityViolation {
                path,
                field,
                base_status,
                attempted,
            } => write!(
                f,
                "monotonicity violation at {path}: field \"{field}\" is {base_status} in base schema but extension sets it to \"{attempted}\""
            ),
            Self::TypeConflict {
                path,
                base_type,
                ext_type,
            } => write!(
                f,
                "type conflict at {path}: base declares \"{base_type}\" but extension declares \"{ext_type}\""
            ),
        }
    }
}

impl std::error::Error for ResolveError {}
