//! Device-attested accessibility node-tree fingerprint facts.
//!
//! The bundled bridge produces the digest on-device from the nodes it actually
//! traversed. This node never recomputes it from nothing, never widens the
//! declared scope, and never receives the nodes' text: only the digest and
//! structural facts cross the bridge, so screen content is not retained here.
//!
//! Everything a caller needs in order to judge the digest travels beside it:
//! which display and region it covers, which traversal bounds the device
//! applied, how much of the hierarchy was actually covered, whether the
//! traversal stopped at a declared bound, and when the sample was taken. A
//! bounded prefix is never presented as the whole hierarchy, and an absent or
//! failed hierarchy is a recorded absence instead of a digest.

use serde::{Deserialize, Serialize};

use crate::bridge::{BridgeError, BridgeErrorKind};

/// Complete traversal within the declared bounds: the digest covers every
/// node of every window the accessibility service reported for the display
/// that passed the declared region filter.
pub const KIND_NODE_TREE: &str = "accessibility_node_tree";
/// The traversal stopped at a declared bound (node, depth or window budget, or
/// a reported window whose root could not be read). The digest is a prefix of
/// the hierarchy, exactly as wide as `FingerprintScope` records — a positive
/// change inside that prefix is still a change, but a change outside it is
/// invisible, which is why the observation says `prefix` instead of `tree`.
pub const KIND_NODE_TREE_PREFIX: &str = "accessibility_node_tree_prefix";
/// No hierarchy evidence was available at capture; the frame fingerprint is
/// the pixel hash and `MobileObservation::fingerprint_absent` says why.
pub const KIND_PIXELS_ONLY: &str = "pixels_only";

/// Canonicalization the bundled bridge implements. A digest is comparable only
/// with one produced by this same algorithm over the same declared scope.
pub const FINGERPRINT_ALGORITHM: &str = "shenko-node-tree-sha256/1";
/// Digest prefix; the digest is `<prefix><64 lowercase hex>`.
pub const FINGERPRINT_PREFIX: &str = "sha256:";

/// Bounds every request declares, so the device cannot silently change how
/// much it looked at: a re-observation with different bounds is a different
/// scope, not an unchanged hierarchy.
pub const MAX_FINGERPRINT_NODES: u32 = 4000;
pub const MAX_FINGERPRINT_DEPTH: u32 = 64;
pub const MAX_FINGERPRINT_WINDOWS: u32 = 8;

/// The scope this node asks the device to fingerprint. Both capture and
/// re-verification derive it from the same observation, so both compare the
/// same traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FingerprintRequest {
    pub display_id: i32,
    /// Display-absolute rectangle the bound observation covers.
    pub region: [i32; 4],
    pub max_nodes: u32,
    pub max_depth: u32,
    pub max_windows: u32,
}

/// What a returned digest actually covers, and when it was sampled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintScope {
    /// Device-side canonicalization id; a digest from another algorithm is not
    /// comparable and is never bound.
    pub algorithm: String,
    pub display_id: i32,
    /// Display-absolute rectangle the region filter used.
    pub region: [i32; 4],
    /// Stable id of the inclusion rule (e.g. nodes whose screen bounds
    /// intersect the region, plus zero-area structural nodes). Part of the
    /// scope because a different rule selects a different node set.
    pub node_filter: String,
    pub max_nodes: u32,
    pub max_depth: u32,
    pub max_windows: u32,
    /// Windows the accessibility service reported for this display and the
    /// traversal actually entered.
    pub windows_considered: u32,
    /// Packages of those windows. Structural identity, never node content.
    pub window_packages: Vec<String>,
    /// Nodes popped from the traversal stack (the real cost bound).
    pub visited_nodes: u32,
    /// Nodes that passed the region filter and were hashed.
    pub node_count: u32,
    /// The traversal stopped at a declared bound: the digest is a prefix.
    pub truncated: bool,
    /// Which bound stopped it; `None` exactly when the traversal was complete.
    pub truncation_reason: Option<String>,
    /// Device wall-clock start of the traversal.
    pub sampled_at_ms: i64,
    pub elapsed_ms: u64,
    /// Device-side qualifications (e.g. window kinds the service does not
    /// report). Empty when the device had nothing to add.
    pub notes: Vec<String>,
}

impl FingerprintScope {
    /// Can a re-observation's digest be compared with this one as the *same*
    /// traversal scope?
    ///
    /// Coverage counts, timestamps and elapsed time describe one sample, not
    /// the scope, so they are excluded: a changed count shows up as a changed
    /// digest, which is a hierarchy change, not a scope difference.
    pub fn comparable_with(&self, other: &Self) -> bool {
        self.algorithm == other.algorithm
            && self.display_id == other.display_id
            && self.region == other.region
            && self.node_filter == other.node_filter
            && self.max_nodes == other.max_nodes
            && self.max_depth == other.max_depth
            && self.max_windows == other.max_windows
    }

    /// Kind vocabulary for an observation whose fingerprint is this digest.
    pub fn kind(&self) -> &'static str {
        if self.truncated {
            KIND_NODE_TREE_PREFIX
        } else {
            KIND_NODE_TREE
        }
    }
}

/// Why no hierarchy fingerprint is bound. Absence is a fact, never a digest:
/// nothing here can be compared as "unchanged".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FingerprintAbsence {
    /// Stable host vocabulary (`hierarchy_unavailable`, `hierarchy_denied`,
    /// `hierarchy_timeout`, `hierarchy_unsupported`, `hierarchy_failed`,
    /// `bridge_unreachable`, `protocol_violation`).
    pub reason: String,
    /// The device's own diagnostic, or this node's rejection detail.
    pub detail: String,
}

impl FingerprintAbsence {
    /// Classify a bridge failure without inventing a digest for it. The
    /// device's message is retained verbatim; the vocabulary is this node's.
    pub fn from_bridge_error(error: &BridgeError) -> Self {
        let reason = match error.kind {
            BridgeErrorKind::TypedUnavailable => "hierarchy_unavailable",
            BridgeErrorKind::Denied => "hierarchy_denied",
            BridgeErrorKind::Timeout => "hierarchy_timeout",
            BridgeErrorKind::Unsupported => "hierarchy_unsupported",
            BridgeErrorKind::Io => "bridge_unreachable",
            BridgeErrorKind::Internal => "hierarchy_failed",
        };
        Self {
            reason: reason.to_string(),
            detail: error.message.clone(),
        }
    }

    pub fn protocol_violation(detail: impl Into<String>) -> Self {
        Self {
            reason: "protocol_violation".to_string(),
            detail: detail.into(),
        }
    }
}

/// A digest the device attested, together with the scope it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticFingerprint {
    /// `<FINGERPRINT_PREFIX><64 lowercase hex>` over the canonical traversal.
    pub digest: String,
    pub scope: FingerprintScope,
}

impl SemanticFingerprint {
    pub fn kind(&self) -> &'static str {
        self.scope.kind()
    }
}

/// What the device actually produced for one fingerprint request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintEvidence {
    Available(SemanticFingerprint),
    Absent(FingerprintAbsence),
}

impl FingerprintEvidence {
    pub fn available(&self) -> Option<&SemanticFingerprint> {
        match self {
            Self::Available(available) => Some(available),
            Self::Absent(_) => None,
        }
    }

    pub fn absence(&self) -> Option<&FingerprintAbsence> {
        match self {
            Self::Available(_) => None,
            Self::Absent(absence) => Some(absence),
        }
    }
}

/// Parse one `node_tree_fingerprint` response.
///
/// Every field is checked against the request that produced it: a device that
/// answers a different display, region or traversal bound, or that hands back
/// a digest this node cannot read, yields a recorded absence rather than a
/// digest bound to an observation it does not actually cover.
pub fn parse_response(
    result: &serde_json::Value,
    request: &FingerprintRequest,
) -> FingerprintEvidence {
    match read_response(result, request) {
        Ok(available) => FingerprintEvidence::Available(available),
        Err(detail) => FingerprintEvidence::Absent(FingerprintAbsence::protocol_violation(detail)),
    }
}

fn read_response(
    result: &serde_json::Value,
    request: &FingerprintRequest,
) -> Result<SemanticFingerprint, String> {
    let algorithm = str_field(result, "algorithm")?;
    if algorithm != FINGERPRINT_ALGORITHM {
        return Err(format!(
            "device reported fingerprint algorithm {algorithm:?}; this node binds only \
             {FINGERPRINT_ALGORITHM}, so the digest is not comparable and is not bound"
        ));
    }
    let digest = str_field(result, "fingerprint")?;
    validate_digest(&digest)?;

    let display_id = i32_field(result, "display_id")?;
    if display_id != request.display_id {
        return Err(format!(
            "device answered display_id {display_id} but display_id {} was requested",
            request.display_id
        ));
    }
    let region = region_field(result, "region")?;
    if region != request.region {
        return Err(format!(
            "device answered region {region:?} but region {:?} was requested",
            request.region
        ));
    }
    let max_nodes = u32_field(result, "max_nodes")?;
    let max_depth = u32_field(result, "max_depth")?;
    let max_windows = u32_field(result, "max_windows")?;
    if (max_nodes, max_depth, max_windows)
        != (request.max_nodes, request.max_depth, request.max_windows)
    {
        return Err(format!(
            "device applied max_nodes={max_nodes} max_depth={max_depth} \
             max_windows={max_windows}, but max_nodes={} max_depth={} max_windows={} were \
             requested; a different traversal bound is a different scope",
            request.max_nodes, request.max_depth, request.max_windows
        ));
    }
    let node_filter = str_field(result, "node_filter")?;
    if node_filter.is_empty() {
        return Err("device reported an empty node_filter; the included node set would be unknown".into());
    }

    let windows_considered = u32_field(result, "windows_considered")?;
    if windows_considered > max_windows {
        return Err(format!(
            "device traversed {windows_considered} windows past the declared bound of {max_windows}"
        ));
    }
    let window_packages = str_list(result, "window_packages")?;
    if window_packages.len() > windows_considered as usize {
        return Err(format!(
            "device reported {} package names for {windows_considered} traversed windows",
            window_packages.len()
        ));
    }
    let visited_nodes = u32_field(result, "visited_nodes")?;
    if visited_nodes > max_nodes {
        return Err(format!(
            "device visited {visited_nodes} nodes past the declared bound of {max_nodes}"
        ));
    }
    let node_count = u32_field(result, "node_count")?;
    if node_count > visited_nodes {
        return Err(format!(
            "device hashed {node_count} nodes but visited only {visited_nodes}"
        ));
    }
    let truncated = bool_field(result, "truncated")?;
    let truncation_reason = match result.get("truncation_reason") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(reason)) if !reason.is_empty() => Some(reason.clone()),
        Some(_) => {
            return Err("device reported a non-string or empty truncation_reason".into());
        }
    };
    match (truncated, &truncation_reason) {
        // A prefix must say which bound stopped it, and a complete traversal
        // must not carry a truncation claim.
        (true, None) => {
            return Err(
                "device reported a truncated traversal without a truncation_reason; a bounded \
                 prefix cannot be bound as an unexplained digest"
                    .into(),
            )
        }
        (false, Some(reason)) => {
            return Err(format!(
                "device reported a complete traversal together with truncation_reason \
                 {reason:?}; the coverage claim is contradictory"
            ))
        }
        _ => {}
    }

    let sampled_at_ms = i64_field(result, "sampled_at_ms")?;
    let elapsed_ms = u64_field(result, "elapsed_ms")?;

    Ok(SemanticFingerprint {
        digest,
        scope: FingerprintScope {
            algorithm,
            display_id,
            region,
            node_filter,
            max_nodes,
            max_depth,
            max_windows,
            windows_considered,
            window_packages,
            visited_nodes,
            node_count,
            truncated,
            truncation_reason,
            sampled_at_ms,
            elapsed_ms,
            notes: str_list(result, "notes")?,
        },
    })
}

/// `<prefix><64 lowercase hex>`; anything else is not a digest this node can
/// compare, so it is never stored as a frame fingerprint.
fn validate_digest(digest: &str) -> Result<(), String> {
    let Some(hex) = digest.strip_prefix(FINGERPRINT_PREFIX) else {
        return Err(format!(
            "fingerprint {digest:?} is not `{FINGERPRINT_PREFIX}<hex>`"
        ));
    };
    if hex.len() != 64 {
        return Err(format!(
            "fingerprint hex is {} characters, not 64",
            hex.len()
        ));
    }
    if !hex
        .bytes()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("fingerprint hex must be lowercase hex".into());
    }
    Ok(())
}

fn str_field(value: &serde_json::Value, key: &str) -> Result<String, String> {
    value
        .get(key)
        .and_then(|field| field.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field `{key}`"))
}

fn bool_field(value: &serde_json::Value, key: &str) -> Result<bool, String> {
    value
        .get(key)
        .and_then(|field| field.as_bool())
        .ok_or_else(|| format!("missing boolean field `{key}`"))
}

fn i32_field(value: &serde_json::Value, key: &str) -> Result<i32, String> {
    value
        .get(key)
        .and_then(|field| field.as_i64())
        .and_then(|number| i32::try_from(number).ok())
        .ok_or_else(|| format!("missing or out-of-range integer field `{key}`"))
}

fn u32_field(value: &serde_json::Value, key: &str) -> Result<u32, String> {
    value
        .get(key)
        .and_then(|field| field.as_u64())
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| format!("missing or out-of-range integer field `{key}`"))
}

fn i64_field(value: &serde_json::Value, key: &str) -> Result<i64, String> {
    value
        .get(key)
        .and_then(|field| field.as_i64())
        .ok_or_else(|| format!("missing integer field `{key}`"))
}

fn u64_field(value: &serde_json::Value, key: &str) -> Result<u64, String> {
    value
        .get(key)
        .and_then(|field| field.as_u64())
        .ok_or_else(|| format!("missing integer field `{key}`"))
}

/// A bounded list of short strings. Window/note lists come from the device, so
/// they are capped here as well instead of being trusted to stay small.
fn str_list(value: &serde_json::Value, key: &str) -> Result<Vec<String>, String> {
    const MAX_ITEMS: usize = 64;
    const MAX_ITEM_CHARS: usize = 256;
    let Some(items) = value.get(key) else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .ok_or_else(|| format!("field `{key}` is not an array"))?;
    if items.len() > MAX_ITEMS {
        return Err(format!(
            "field `{key}` has {} entries, above the {MAX_ITEMS} bound",
            items.len()
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Some(text) = item.as_str() else {
            return Err(format!("field `{key}` contains a non-string entry"));
        };
        let mut text = text.to_string();
        if text.chars().count() > MAX_ITEM_CHARS {
            text = text.chars().take(MAX_ITEM_CHARS).collect();
        }
        out.push(text);
    }
    Ok(out)
}

fn region_field(value: &serde_json::Value, key: &str) -> Result<[i32; 4], String> {
    let Some(items) = value.get(key).and_then(|field| field.as_array()) else {
        return Err(format!("missing region array `{key}`"));
    };
    if items.len() != 4 {
        return Err(format!(
            "region field `{key}` has {} entries, not 4",
            items.len()
        ));
    }
    let mut region = [0i32; 4];
    for (slot, item) in region.iter_mut().zip(items) {
        let Some(number) = item.as_i64().and_then(|n| i32::try_from(n).ok()) else {
            return Err(format!("region field `{key}` has a non-integer entry"));
        };
        *slot = number;
    }
    Ok(region)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn request() -> FingerprintRequest {
        FingerprintRequest {
            display_id: 0,
            region: [0, 0, 64, 48],
            max_nodes: MAX_FINGERPRINT_NODES,
            max_depth: MAX_FINGERPRINT_DEPTH,
            max_windows: MAX_FINGERPRINT_WINDOWS,
        }
    }

    fn response(digest: &str) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "fingerprint": digest,
            "algorithm": FINGERPRINT_ALGORITHM,
            "display_id": 0,
            "region": [0, 0, 64, 48],
            "node_filter": "bounds_intersect_region",
            "max_nodes": MAX_FINGERPRINT_NODES,
            "max_depth": MAX_FINGERPRINT_DEPTH,
            "max_windows": MAX_FINGERPRINT_WINDOWS,
            "windows_considered": 1,
            "window_packages": ["com.example.app"],
            "visited_nodes": 12,
            "node_count": 11,
            "truncated": false,
            "sampled_at_ms": 1700000000000i64,
            "elapsed_ms": 4,
            "notes": [],
        })
    }

    fn available(result: &serde_json::Value) -> SemanticFingerprint {
        match parse_response(result, &request()) {
            FingerprintEvidence::Available(available) => available,
            FingerprintEvidence::Absent(absence) => panic!("expected a digest: {absence:?}"),
        }
    }

    #[test]
    fn complete_response_binds_digest_with_its_scope() {
        let parsed = available(&response(DIGEST_A));
        assert_eq!(parsed.digest, DIGEST_A);
        assert_eq!(parsed.kind(), KIND_NODE_TREE);
        assert!(!parsed.scope.truncated);
        assert_eq!(parsed.scope.node_count, 11);
        assert_eq!(parsed.scope.window_packages, vec!["com.example.app".to_string()]);
    }

    /// A device that answers for another display, region or traversal bound is
    /// describing a different part of the screen than the one requested.
    #[test]
    fn response_outside_the_requested_scope_is_not_bound() {
        for (field, value) in [
            ("display_id", serde_json::json!(1)),
            ("region", serde_json::json!([0, 0, 32, 24])),
            ("max_nodes", serde_json::json!(64)),
            ("max_depth", serde_json::json!(2)),
            ("max_windows", serde_json::json!(1)),
        ] {
            let mut result = response(DIGEST_A);
            result[field] = value;
            let absence = parse_response(&result, &request())
                .absence()
                .cloned()
                .unwrap_or_else(|| panic!("{field} mismatch must not bind a digest"));
            assert_eq!(absence.reason, "protocol_violation");
            assert!(
                absence.detail.contains(field),
                "the detail names the mismatched field: {absence:?}"
            );
        }
    }

    /// A digest this node cannot read (wrong prefix, wrong length, uppercase,
    /// or a foreign algorithm) must never become an observation fingerprint.
    #[test]
    fn unreadable_digests_and_foreign_algorithms_are_absences() {
        for digest in [
            "not-a-digest",
            "sha256:abc",
            "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            let absence = parse_response(&response(digest), &request())
                .absence()
                .cloned()
                .unwrap_or_else(|| panic!("{digest} must be rejected"));
            assert_eq!(absence.reason, "protocol_violation");
        }
        let mut result = response(DIGEST_A);
        result["algorithm"] = serde_json::json!("sha256/2");
        let absence = parse_response(&result, &request())
            .absence()
            .cloned()
            .expect("foreign algorithm cannot be bound");
        assert!(absence.detail.contains("algorithm"));
    }

    /// A bounded prefix must name the bound that stopped it, and a complete
    /// traversal must not claim truncation: either shape otherwise hides how
    /// much of the hierarchy the digest covers.
    #[test]
    fn truncation_must_be_declared_and_consistent() {
        let mut truncated = response(DIGEST_A);
        truncated["truncated"] = serde_json::json!(true);
        assert_eq!(
            parse_response(&truncated, &request())
                .absence()
                .expect("unexplained truncation")
                .reason,
            "protocol_violation"
        );

        truncated["truncation_reason"] = serde_json::json!("node_budget");
        let parsed = available(&truncated);
        assert_eq!(parsed.kind(), KIND_NODE_TREE_PREFIX);
        assert_eq!(parsed.scope.truncation_reason.as_deref(), Some("node_budget"));

        let mut contradictory = response(DIGEST_A);
        contradictory["truncation_reason"] = serde_json::json!("depth_budget");
        assert_eq!(
            parse_response(&contradictory, &request())
                .absence()
                .expect("complete traversal with a truncation claim")
                .reason,
            "protocol_violation"
        );

        let mut over_bound = response(DIGEST_A);
        over_bound["visited_nodes"] = serde_json::json!(u64::from(MAX_FINGERPRINT_NODES) + 1);
        assert_eq!(
            parse_response(&over_bound, &request())
                .absence()
                .expect("traversal past the declared bound")
                .reason,
            "protocol_violation"
        );

        let mut impossible_counts = response(DIGEST_A);
        impossible_counts["node_count"] = serde_json::json!(13u32);
        assert_eq!(
            parse_response(&impossible_counts, &request())
                .absence()
                .expect("more hashed nodes than visited")
                .reason,
            "protocol_violation"
        );
    }

    /// Comparability is about the scope, not about one sample: timestamps and
    /// coverage move freely, a changed bound or filter does not.
    #[test]
    fn scope_comparability_ignores_sample_facts_but_not_bounds() {
        let base = available(&response(DIGEST_A)).scope;

        let mut later = base.clone();
        later.sampled_at_ms += 5_000;
        later.elapsed_ms += 90;
        later.node_count += 3;
        later.visited_nodes += 3;
        later.window_packages.push("com.example.second".to_string());
        later.notes.push("window list changed".to_string());
        assert!(base.comparable_with(&later));

        let mut narrower = base.clone();
        narrower.max_nodes /= 2;
        assert!(!base.comparable_with(&narrower));

        let mut other_region = base.clone();
        other_region.region = [1, 0, 64, 48];
        assert!(!base.comparable_with(&other_region));

        let mut other_rule = base.clone();
        other_rule.node_filter = "all_nodes".to_string();
        assert!(!base.comparable_with(&other_rule));
    }

    #[test]
    fn bridge_failures_keep_their_own_absence_vocabulary() {
        let cases = [
            (BridgeErrorKind::TypedUnavailable, "hierarchy_unavailable"),
            (BridgeErrorKind::Denied, "hierarchy_denied"),
            (BridgeErrorKind::Timeout, "hierarchy_timeout"),
            (BridgeErrorKind::Unsupported, "hierarchy_unsupported"),
            (BridgeErrorKind::Io, "bridge_unreachable"),
            (BridgeErrorKind::Internal, "hierarchy_failed"),
        ];
        for (kind, reason) in cases {
            let error = BridgeError {
                kind,
                message: "device said so".to_string(),
                retryable: false,
            };
            let absence = FingerprintAbsence::from_bridge_error(&error);
            assert_eq!(absence.reason, reason);
            assert_eq!(absence.detail, "device said so");
        }
    }
}
