//! The effective context window of one dispatch loop (SPEC §13.4, §41.9).
//!
//! §13.4 requires that every request be checked, before it is enqueued, as
//!
//! ```text
//! rendered input tokens + output reservation + template/tool/multimodal cost
//!     <= effective window
//! ```
//!
//! and §41.9 defines the effective window as the *joint* limit of the model,
//! the backend and the owner's configuration. No single component is the
//! window: `max_context_tokens` from `/v1/models` is only the model ceiling, a
//! KV pool size is only the backend's share, and a configured cap is only the
//! owner's preference. Taking one of them for the window over-admits whenever
//! another is smaller.
//!
//! This module resolves the components into one value. It does not invent a
//! missing component: when one is unknown, the result is an explicitly
//! *provisional upper bound* over the components that were really observed,
//! and when none is known the window stays unknown, which keeps admission
//! advisory instead of pretending a capacity exists.

use serde_json::json;

/// The three components of the joint effective window.
///
/// `None` means "not observed/configured". A zero value is not a real limit
/// (an empty window cannot admit any generation), so it is treated as unknown
/// and reported as such rather than silently enforcing zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowLimits {
    /// The bound model's advertised context ceiling (models metadata).
    pub model_ceiling: Option<u64>,
    /// The backend's own observed pool/available capacity for this engine.
    ///
    /// A plain OpenAI-compatible endpoint does not report KV residency, so this
    /// stays `None` there and the resolved window is an upper bound.
    pub backend_pool: Option<u64>,
    /// The owner's configured limit.
    pub policy_limit: Option<u64>,
}

const COMPONENTS: [&str; 3] = ["model_ceiling", "backend_pool", "policy_limit"];

/// The resolved effective window of one dispatch loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectiveWindow {
    /// Every component was observed or configured: this is the joint limit
    /// §41.9 requires, not an estimate.
    Joint { tokens: u64 },
    /// At least one component is unknown. `tokens` is the smallest component
    /// that *was* known, so it is a legitimate refusal threshold and an upper
    /// bound — never a claim that the unknown component cannot be smaller.
    ProvisionalUpperBound { tokens: u64, unknown: Vec<&'static str> },
    /// No component is known. Admission stays advisory.
    Unknown,
}

impl WindowLimits {
    /// Resolve the components into the joint limit, keeping every unknown
    /// component explicit.
    pub fn resolve(&self) -> EffectiveWindow {
        let values = [
            self.model_ceiling.filter(|tokens| *tokens > 0),
            self.backend_pool.filter(|tokens| *tokens > 0),
            self.policy_limit.filter(|tokens| *tokens > 0),
        ];
        let unknown = COMPONENTS
            .into_iter()
            .zip(values)
            .filter_map(|(name, value)| value.is_none().then_some(name))
            .collect::<Vec<_>>();
        let min = values.into_iter().flatten().min();
        match (min, unknown.is_empty()) {
            (None, _) => EffectiveWindow::Unknown,
            (Some(tokens), true) => EffectiveWindow::Joint { tokens },
            (Some(tokens), false) => EffectiveWindow::ProvisionalUpperBound { tokens, unknown },
        }
    }
}

impl EffectiveWindow {
    /// The value a dispatch loop may install: the joint limit, or the
    /// observed-components upper bound while a component is unknown.
    ///
    /// `None` (unknown) is deliberately not installed, so admission keeps its
    /// advisory behaviour instead of enforcing a fabricated number.
    pub fn installable(&self) -> Option<u64> {
        match self {
            Self::Joint { tokens } | Self::ProvisionalUpperBound { tokens, .. } => Some(*tokens),
            Self::Unknown => None,
        }
    }

    /// The resolved token count, when one is known.
    pub fn tokens(&self) -> Option<u64> {
        self.installable()
    }

    /// Whether this is the complete joint limit §41.9 asks for.
    pub fn is_joint(&self) -> bool {
        matches!(self, Self::Joint { .. })
    }

    /// The components that were not observed or configured.
    pub fn unknown_components(&self) -> &[&'static str] {
        match self {
            Self::Joint { .. } | Self::Unknown => &[],
            Self::ProvisionalUpperBound { unknown, .. } => unknown,
        }
    }

    pub fn authority(&self) -> &'static str {
        match self {
            Self::Joint { .. } => "joint",
            Self::ProvisionalUpperBound { .. } => "upper_bound",
            Self::Unknown => "unknown",
        }
    }

    /// Non-secret diagnostic shape: what the runtime installed and why.
    pub fn status(&self) -> serde_json::Value {
        json!({
            "v": 1,
            "authority": self.authority(),
            "tokens": self.tokens(),
            "unknown_components": self.unknown_components(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_joint_window_is_the_smallest_real_component() {
        let all = WindowLimits {
            model_ceiling: Some(32768),
            backend_pool: Some(8192),
            policy_limit: Some(65536),
        };
        let resolved = all.resolve();
        assert_eq!(resolved, EffectiveWindow::Joint { tokens: 8192 });
        assert!(resolved.is_joint());
        assert_eq!(resolved.installable(), Some(8192));
        assert_eq!(resolved.unknown_components(), &[] as &[&str]);
        assert_eq!(resolved.authority(), "joint");
    }

    #[test]
    fn an_unobserved_component_stays_explicit_instead_of_being_guessed() {
        // A plain HTTP backend reports no KV pool: the window cannot be called
        // joint, and the model ceiling is not silently taken as the window.
        let resolved = WindowLimits {
            model_ceiling: Some(32768),
            backend_pool: None,
            policy_limit: Some(4096),
        }
        .resolve();
        assert_eq!(
            resolved,
            EffectiveWindow::ProvisionalUpperBound {
                tokens: 4096,
                unknown: vec!["backend_pool"],
            }
        );
        assert!(!resolved.is_joint());
        assert_eq!(resolved.installable(), Some(4096));
        assert_eq!(resolved.unknown_components(), &["backend_pool"]);
        assert_eq!(resolved.status()["authority"], "upper_bound");
        assert_eq!(
            resolved.status()["unknown_components"],
            json!(["backend_pool"])
        );
    }

    #[test]
    fn nothing_known_keeps_admission_advisory() {
        let resolved = WindowLimits::default().resolve();
        assert_eq!(resolved, EffectiveWindow::Unknown);
        assert_eq!(resolved.installable(), None);
        assert_eq!(resolved.tokens(), None);
        assert_eq!(resolved.authority(), "unknown");
        // A zero component is not a real limit; it is unknown, never enforced.
        let zero = WindowLimits {
            model_ceiling: Some(0),
            backend_pool: Some(0),
            policy_limit: Some(1024),
        }
        .resolve();
        assert_eq!(
            zero,
            EffectiveWindow::ProvisionalUpperBound {
                tokens: 1024,
                unknown: vec!["model_ceiling", "backend_pool"],
            }
        );
    }
}
