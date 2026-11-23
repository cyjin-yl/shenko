//! Declared egress and the one evaluator that admits it.
//!
//! A plugin declares, per capability it offers, the destinations it may reach
//! while installed. Nothing is admitted by default: a plugin that declares
//! nothing, a capability that was never declared, and a destination outside a
//! declared scope are all refused.
//!
//! These are protocol types: plain data plus one pure function. There is no
//! I/O, no clock and no dependency on the kernel. [`evaluate_egress`] is the
//! single destination evaluator; the kernel's root policy registry is its only
//! production caller, and every caller of that registry (built-in or not) is
//! judged by the same declarations. A refusal is a decision value, so a caller
//! cannot mistake "not evaluated" for "allowed".
//!
//! # The request canonicalisation is part of the decision
//!
//! `example.com`, `Example.COM` and `example.com.` name the same DNS target;
//! a scope that admitted one while refusing another would be trivially
//! bypassed by changing case or the root label. [`EgressRequest::canonical`]
//! performs that normalisation once, and both evaluation and the recorded
//! receipt use its result. A request whose host cannot be canonicalised is
//! refused as [`EgressDenialCode::InvalidRequest`] instead of being compared
//! loosely.

use serde::{Deserialize, Serialize};

use crate::ids::EntityId;
use crate::object::Digest;

/// The wire semantics of one egress attempt.
///
/// The protocol is compared exactly: admitting `https` does not admit a raw
/// `tcp` connection to the same endpoint, because the request the plugin asked
/// to make is part of what it declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressProtocol {
    /// An HTTP client request/response exchange.
    Http,
    /// An HTTPS client request/response exchange.
    Https,
    /// A raw TCP connection.
    Tcp,
    /// A raw UDP socket.
    Udp,
}

impl EgressProtocol {
    /// Every protocol this evaluator knows, in stable order.
    pub const ALL: [EgressProtocol; 4] = [
        EgressProtocol::Http,
        EgressProtocol::Https,
        EgressProtocol::Tcp,
        EgressProtocol::Udp,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }

    /// Parse the label a URL scheme or a handler uses.
    ///
    /// An unknown scheme is not widened to a neighbouring protocol: an
    /// observer that cannot name what it would do cannot have declared it.
    pub fn parse(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }
}

/// Which request hosts one declared scope admits.
///
/// Patterns are matched against a canonical host, never against the raw text a
/// caller supplied. `SubdomainsOf` matches on a label boundary, so declaring
/// `example.com` does not admit `notexample.com`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HostPattern {
    /// Exactly this host, after canonicalisation.
    Exact { host: String },
    /// This host and every subdomain of it, on a label boundary.
    SubdomainsOf { domain: String },
    /// Any host. Only a capability whose own contract is to reach an
    /// arbitrary caller-declared endpoint declares this, and its purpose text
    /// has to say so.
    Any,
}

impl HostPattern {
    /// True when this pattern admits the canonical host.
    ///
    /// A pattern that cannot be canonicalised (an empty host, a malformed
    /// domain) matches nothing; it never degrades into `Any`.
    pub fn matches(&self, canonical_host: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact { host } => {
                canonical_host_text(host).is_some_and(|host| host.as_str() == canonical_host)
            }
            Self::SubdomainsOf { domain } => {
                canonical_suffix_domain(domain).is_some_and(|domain| {
                    canonical_host == domain.as_str()
                        || (canonical_host.len() > domain.len()
                            && canonical_host.ends_with(domain.as_str())
                            && canonical_host.as_bytes()[canonical_host.len() - domain.len() - 1] == b'.')
                })
            }
        }
    }
}

/// One inclusive range of destination ports.
///
/// Port 0 is not a destination and a span whose first port is above its last
/// port cannot match anything; both are refused when the declaration is
/// admitted, so a scope that looks restrictive cannot silently match nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressPortSpan {
    pub first: u16,
    pub last: u16,
}

impl EgressPortSpan {
    /// One single port.
    pub fn single(port: u16) -> Self {
        Self {
            first: port,
            last: port,
        }
    }

    /// Every port a destination can have.
    pub fn any() -> Self {
        Self { first: 1, last: u16::MAX }
    }

    pub fn contains(self, port: u16) -> bool {
        self.first <= port && port <= self.last
    }
}

/// One destination a declaration admits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressScope {
    pub protocol: EgressProtocol,
    pub host: HostPattern,
    /// At least one span; spans may not overlap so the same port is never
    /// stated twice.
    pub ports: Vec<EgressPortSpan>,
}

impl EgressScope {
    fn validate(&self, scope: usize) -> Result<(), EgressDeclarationError> {
        if self.ports.is_empty() {
            return Err(EgressDeclarationError::EmptyPorts { scope });
        }
        let mut spans = self.ports.clone();
        spans.sort();
        let mut previous: Option<EgressPortSpan> = None;
        for span in &spans {
            if span.first == 0 {
                return Err(EgressDeclarationError::ZeroPort { scope });
            }
            if span.first > span.last {
                return Err(EgressDeclarationError::InvertedPortSpan { scope });
            }
            if let Some(previous) = previous
                && span.first <= previous.last
            {
                return Err(EgressDeclarationError::OverlappingPorts { scope });
            }
            previous = Some(*span);
        }
        match &self.host {
            HostPattern::Any => Ok(()),
            HostPattern::Exact { host } => {
                if host.trim().is_empty() {
                    return Err(EgressDeclarationError::EmptyHost { scope });
                }
                if canonical_host_text(host).is_none() {
                    return Err(EgressDeclarationError::InvalidHost { scope });
                }
                Ok(())
            }
            HostPattern::SubdomainsOf { domain } => {
                if domain.trim().is_empty() {
                    return Err(EgressDeclarationError::EmptyHost { scope });
                }
                if canonical_suffix_domain(domain).is_none() {
                    return Err(EgressDeclarationError::InvalidHost { scope });
                }
                Ok(())
            }
        }
    }
}

/// One plugin's egress declaration for one capability it offers.
///
/// The declaration is authored by the implementation that will make the
/// request, but the installing generation supplies the `plugin_id`: a
/// definition cannot declare reachability under another plugin's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressDeclaration {
    pub plugin_id: EntityId,
    pub capability_id: String,
    pub capability_version: String,
    /// Why this capability reaches these destinations. A declaration without a
    /// stated purpose is refused: a permission whose reason is unknown cannot
    /// be reviewed or revoked on purpose.
    pub purpose: String,
    pub scopes: Vec<EgressScope>,
}

/// The part of a declaration an implementation authors.
///
/// The installing generation binds it to the definition's own plugin identity,
/// so the handler never supplies the identity it is judged under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressDeclarationDraft {
    pub capability_id: String,
    pub capability_version: String,
    pub purpose: String,
    pub scopes: Vec<EgressScope>,
}

impl EgressDeclarationDraft {
    /// Bind this draft to the application that installs it.
    pub fn bind(self, plugin_id: EntityId) -> EgressDeclaration {
        EgressDeclaration {
            plugin_id,
            capability_id: self.capability_id,
            capability_version: self.capability_version,
            purpose: self.purpose,
            scopes: self.scopes,
        }
    }
}

impl EgressDeclaration {
    /// Refuse a declaration that could never be reviewed or matched.
    pub fn validate(&self) -> Result<(), EgressDeclarationError> {
        if self.capability_id.trim().is_empty() {
            return Err(EgressDeclarationError::EmptyCapabilityId);
        }
        if self.capability_id != self.capability_id.trim() {
            return Err(EgressDeclarationError::SurroundingWhitespace);
        }
        if self.capability_version.trim().is_empty() {
            return Err(EgressDeclarationError::EmptyCapabilityVersion);
        }
        if self.purpose.trim().is_empty() {
            return Err(EgressDeclarationError::EmptyPurpose);
        }
        if self.scopes.is_empty() {
            return Err(EgressDeclarationError::EmptyScope);
        }
        for (index, scope) in self.scopes.iter().enumerate() {
            scope.validate(index)?;
        }
        Ok(())
    }

    /// A stable digest of exactly this declaration's bytes.
    ///
    /// A receipt names the declaration it was judged against by this digest, so
    /// a later widening of the scope is visibly a different declaration rather
    /// than the same one re-read.
    pub fn digest(&self) -> Digest {
        Digest::blake3(&self.canonical_bytes())
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        push_text(&mut out, &self.plugin_id.to_string());
        push_text(&mut out, &self.capability_id);
        push_text(&mut out, &self.capability_version);
        push_text(&mut out, &self.purpose);
        for scope in &self.scopes {
            push_text(&mut out, scope.protocol.as_str());
            match &scope.host {
                HostPattern::Any => push_text(&mut out, "any"),
                HostPattern::Exact { host } => {
                    push_text(&mut out, "exact");
                    push_text(&mut out, host);
                }
                HostPattern::SubdomainsOf { domain } => {
                    push_text(&mut out, "subdomains_of");
                    push_text(&mut out, domain);
                }
            }
            out.extend_from_slice(&(scope.ports.len() as u32).to_le_bytes());
            for span in &scope.ports {
                out.extend_from_slice(&span.first.to_le_bytes());
                out.extend_from_slice(&span.last.to_le_bytes());
            }
        }
        out
    }
}

/// Why an authored declaration or a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EgressDeclarationError {
    #[error("capability id must not be empty")]
    EmptyCapabilityId,
    #[error("capability id must not carry surrounding whitespace")]
    SurroundingWhitespace,
    #[error("capability version must not be empty")]
    EmptyCapabilityVersion,
    #[error("declaration purpose must state why the egress exists")]
    EmptyPurpose,
    #[error("a declaration must name at least one egress scope")]
    EmptyScope,
    #[error("scope {scope} declares no port")]
    EmptyPorts { scope: usize },
    #[error("scope {scope} includes port 0, which is not a destination")]
    ZeroPort { scope: usize },
    #[error("scope {scope} has a port span whose first port is above its last")]
    InvertedPortSpan { scope: usize },
    #[error("scope {scope} states the same port more than once")]
    OverlappingPorts { scope: usize },
    #[error("scope {scope} has an empty host pattern")]
    EmptyHost { scope: usize },
    #[error("scope {scope} has a host pattern that cannot be compared")]
    InvalidHost { scope: usize },
}

/// One concrete attempt a plugin asks the root policy to admit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressRequest {
    /// The plugin that would make the request.
    pub plugin_id: EntityId,
    /// The capability the request is made through.
    pub capability_id: String,
    pub protocol: EgressProtocol,
    /// Host as the caller names it; the canonical form is what is compared and
    /// recorded.
    pub host: String,
    pub port: u16,
    /// What this one attempt is for. It reaches the receipt and is never a
    /// permission.
    pub purpose: String,
}

impl EgressRequest {
    pub fn new(
        plugin_id: EntityId,
        capability_id: impl Into<String>,
        protocol: EgressProtocol,
        host: impl Into<String>,
        port: u16,
        purpose: impl Into<String>,
    ) -> Self {
        Self {
            plugin_id,
            capability_id: capability_id.into(),
            protocol,
            host: host.into(),
            port,
            purpose: purpose.into(),
        }
    }

    /// The canonical request used for both evaluation and the receipt.
    ///
    /// `None` means the attempt cannot be stated precisely enough to judge. It
    /// is refused as `invalid_request`; no lenient fallback comparison happens.
    pub fn canonical(&self) -> Option<EgressRequest> {
        let capability_id = self.capability_id.trim();
        if capability_id.is_empty() {
            return None;
        }
        let purpose = self.purpose.trim();
        if purpose.is_empty() {
            return None;
        }
        if self.port == 0 {
            return None;
        }
        let host = canonical_host_text(&self.host)?;
        Some(EgressRequest {
            plugin_id: self.plugin_id,
            capability_id: capability_id.to_owned(),
            protocol: self.protocol,
            host,
            port: self.port,
            purpose: purpose.to_owned(),
        })
    }
}

/// One declared scope a request was admitted by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressMatch {
    pub plugin_id: EntityId,
    pub capability_id: String,
    pub capability_version: String,
    /// The digest of the exact declaration the scope belonged to.
    pub declaration_digest: Digest,
    /// Index of the matched scope inside that declaration.
    pub scope_index: usize,
}

/// The outcome of evaluating one request against the declarations in force.
///
/// [`EgressDenialCode::AuthorityStale`] and
/// [`EgressDenialCode::AuthorityMismatch`] are produced by the registry's
/// binding checks, not by [`evaluate_egress`]: the evaluator only sees
/// declarations and a canonical request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum EgressDecision {
    Allowed { matched: EgressMatch },
    Denied { code: EgressDenialCode },
}

impl EgressDecision {
    pub fn denied(code: EgressDenialCode) -> Self {
        Self::Denied { code }
    }

    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }

    pub fn denial_code(&self) -> Option<EgressDenialCode> {
        match self {
            Self::Allowed { .. } => None,
            Self::Denied { code } => Some(*code),
        }
    }
}

/// Why an egress attempt was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressDenialCode {
    /// The request itself is not precise enough to judge (unparseable or
    /// non-canonicalisable host, port 0, empty capability or purpose).
    InvalidRequest,
    /// This plugin declared no egress at all: deny by default.
    UndeclaredPlugin,
    /// The plugin declared other capabilities, but not this one.
    UndeclaredCapability,
    /// No declared scope of this capability uses the requested protocol.
    ProtocolNotDeclared,
    /// A scope uses the protocol, but none admits the canonical host.
    HostNotDeclared,
    /// Protocol and host are declared, but no span admits the port.
    PortNotDeclared,
    /// The registry-decided binding: the registration this authority named is
    /// no longer installed, or has been replaced by a newer generation.
    AuthorityStale,
    /// The registry-decided binding: the authority and the request name
    /// different plugins or capabilities.
    AuthorityMismatch,
}

impl EgressDenialCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::UndeclaredPlugin => "undeclared_plugin",
            Self::UndeclaredCapability => "undeclared_capability",
            Self::ProtocolNotDeclared => "protocol_not_declared",
            Self::HostNotDeclared => "host_not_declared",
            Self::PortNotDeclared => "port_not_declared",
            Self::AuthorityStale => "authority_stale",
            Self::AuthorityMismatch => "authority_mismatch",
        }
    }
}

impl std::fmt::Display for EgressDenialCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The recorded outcome of one admission attempt.
///
/// A receipt exists for every attempt, allowed or refused, and names the
/// canonical request, the decision, the declaration digest that admitted it and
/// the policy revision it was judged under. A refused attempt has no admission
/// token, so a receipt is the only thing it can attach to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyReceipt {
    pub schema_version: u32,
    pub receipt_id: EntityId,
    /// The canonical request when one exists; otherwise the request as
    /// received, so the receipt still says what was refused.
    pub request: EgressRequest,
    pub decision: EgressDecision,
    /// The registry revision this decision was made against.
    pub policy_revision: u64,
    pub recorded_at_ms: i64,
}

impl PolicyReceipt {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn is_allowed(&self) -> bool {
        self.decision.is_allowed()
    }

    pub fn denial_code(&self) -> Option<EgressDenialCode> {
        self.decision.denial_code()
    }

    /// The exact bytes a caller can seal into evidence.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.schema_version.to_le_bytes());
        push_text(&mut out, &self.receipt_id.to_string());
        push_text(&mut out, &self.request.plugin_id.to_string());
        push_text(&mut out, &self.request.capability_id);
        push_text(&mut out, self.request.protocol.as_str());
        push_text(&mut out, &self.request.host);
        out.extend_from_slice(&self.request.port.to_le_bytes());
        push_text(&mut out, &self.request.purpose);
        match &self.decision {
            EgressDecision::Allowed { matched } => {
                push_text(&mut out, "allowed");
                push_text(&mut out, &matched.plugin_id.to_string());
                push_text(&mut out, &matched.capability_id);
                push_text(&mut out, &matched.capability_version);
                push_text(&mut out, &matched.declaration_digest.to_string());
                out.extend_from_slice(&(matched.scope_index as u64).to_le_bytes());
            }
            EgressDecision::Denied { code } => {
                push_text(&mut out, "denied");
                push_text(&mut out, code.as_str());
            }
        }
        out.extend_from_slice(&self.policy_revision.to_le_bytes());
        out.extend_from_slice(&self.recorded_at_ms.to_le_bytes());
        out
    }

    pub fn digest(&self) -> Digest {
        Digest::blake3(&self.canonical_bytes())
    }
}

/// The one destination evaluator.
///
/// Every admitted egress attempt, for every plugin, passes through this
/// function. It is deliberately pure: the same declarations and the same
/// request always produce the same decision, and a caller cannot pass a flag
/// that makes its own request special. `declarations` is the full live set
/// (all plugins); declarations for other plugins are ignored.
pub fn evaluate_egress(
    declarations: &[EgressDeclaration],
    request: &EgressRequest,
) -> EgressDecision {
    let Some(request) = request.canonical() else {
        return EgressDecision::denied(EgressDenialCode::InvalidRequest);
    };
    let plugin_declarations: Vec<&EgressDeclaration> = declarations
        .iter()
        .filter(|declaration| declaration.plugin_id == request.plugin_id)
        .collect();
    if plugin_declarations.is_empty() {
        return EgressDecision::denied(EgressDenialCode::UndeclaredPlugin);
    }
    let mut saw_protocol = false;
    let mut saw_host = false;
    for declaration in &plugin_declarations {
        if declaration.capability_id != request.capability_id {
            continue;
        }
        let declaration_digest = declaration.digest();
        for (scope_index, scope) in declaration.scopes.iter().enumerate() {
            if scope.protocol != request.protocol {
                continue;
            }
            saw_protocol = true;
            if !scope.host.matches(&request.host) {
                continue;
            }
            saw_host = true;
            if !scope.ports.iter().any(|span| span.contains(request.port)) {
                continue;
            }
            return EgressDecision::Allowed {
                matched: EgressMatch {
                    plugin_id: declaration.plugin_id,
                    capability_id: declaration.capability_id.clone(),
                    capability_version: declaration.capability_version.clone(),
                    declaration_digest,
                    scope_index,
                },
            };
        }
    }
    let code = if !saw_protocol {
        EgressDenialCode::ProtocolNotDeclared
    } else if !saw_host {
        EgressDenialCode::HostNotDeclared
    } else {
        EgressDenialCode::PortNotDeclared
    };
    EgressDecision::denied(code)
}

fn push_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

/// Canonical host text, or `None` when the host is not a name this evaluator
/// can compare.
///
/// The accepted shapes are: a bracketed or bare IPv6 literal, or a dotted name
/// whose labels are ASCII alphanumerics, `-`, `_` and non-empty dots. Case is
/// folded and one trailing root dot is removed, both of which are what DNS
/// itself ignores. Anything else (credentials, a path, a query, a port smuggled
/// into the host) is refused rather than trimmed into something narrower than
/// what the caller wrote.
pub fn canonical_host_text(raw: &str) -> Option<String> {
    let host = raw.trim();
    if host.is_empty() || host.len() > 253 {
        return None;
    }
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']')?;
        inner.parse::<std::net::Ipv6Addr>().ok()?;
        return Some(format!("[{}]", inner.to_ascii_lowercase()));
    }
    if host.contains(':') {
        host.parse::<std::net::Ipv6Addr>().ok()?;
        return Some(host.to_ascii_lowercase());
    }
    if host.starts_with('.') || host.contains("..") {
        return None;
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Canonical dotted domain for a `subdomains_of` pattern.
fn canonical_suffix_domain(raw: &str) -> Option<String> {
    let domain = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if domain.is_empty() || domain.len() > 253 {
        return None;
    }
    if domain.starts_with('.') || domain.contains("..") {
        return None;
    }
    if !domain
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_')
    {
        return None;
    }
    Some(domain)
}
