//! Which real account a quota reading belongs to, and how a local secret is
//! reached without ever storing it.
//!
//! docs/specs/quota-resources.md#spec-42-3, docs/specs/inference.md#spec-47-5: an account scope is a *reference* to a secret-managed
//! identity, never a credential. A provider distinguishes scopes that are not
//! interchangeable — an organization, one of its workspaces, the "Default
//! Workspace" that exists when no workspace id is configured, a single API
//! key's own scope, and a consumer subscription shared by several clients.
//! A reading that cannot name its scope stays [`ScopeIdentity::Unknown`]; it
//! is never rounded up to "the default account", and a workspace without its
//! own limit inherits the organization limit rather than becoming unlimited.

use serde::{Deserialize, Serialize};

/// The real scope a provider reading belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ScopeIdentity {
    /// The provider's own default scope. For Anthropic this is the
    /// "Default Workspace" that exists when no workspace id is configured.
    DefaultWorkspace,
    /// One organization's own limits.
    Organization { organization_id: String },
    /// One workspace, optionally inside a known organization.
    Workspace {
        workspace_id: String,
        #[serde(default)]
        organization_id: Option<String>,
    },
    /// One API key's own scope: rate limits are granted per key.
    ApiKey { key_scope: String },
    /// A consumer subscription shared by several clients (Claude Code,
    /// Desktop and claude.ai draw on the same windows).
    Subscription,
    /// Shenko's own accounting of its own usage.
    LocalMeter,
    /// The scope could not be established and stays unknown.
    Unknown,
}

impl Default for ScopeIdentity {
    fn default() -> Self {
        Self::Unknown
    }
}

impl ScopeIdentity {
    /// Whether this identity names one specific scope rather than a default
    /// or an unknown one. An unknown identity is never treated as specific.
    pub fn is_specific(&self) -> bool {
        !matches!(self, ScopeIdentity::Unknown | ScopeIdentity::DefaultWorkspace)
    }

    /// A short, stable label for diagnostics and the subject's quota view.
    pub fn label(&self) -> String {
        match self {
            ScopeIdentity::DefaultWorkspace => "default_workspace".into(),
            ScopeIdentity::Organization { organization_id } => format!("organization:{organization_id}"),
            ScopeIdentity::Workspace { workspace_id, organization_id } => match organization_id {
                Some(organization) => format!("workspace:{workspace_id}@{organization}"),
                None => format!("workspace:{workspace_id}"),
            },
            ScopeIdentity::ApiKey { key_scope } => format!("api_key:{key_scope}"),
            ScopeIdentity::Subscription => "subscription".into(),
            ScopeIdentity::LocalMeter => "local_meter".into(),
            ScopeIdentity::Unknown => "unknown".into(),
        }
    }
}

/// A local reference to one account scope of one provider. The reference is
/// what the ledger records; the secret behind it stays on this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaScope {
    /// Stable local reference, for example `anthropic:workspace:1234`.
    pub scope_ref: String,
    pub provider_id: String,
    pub identity: ScopeIdentity,
}

impl QuotaScope {
    pub fn new(
        scope_ref: impl Into<String>,
        provider_id: impl Into<String>,
        identity: ScopeIdentity,
    ) -> Self {
        Self {
            scope_ref: scope_ref.into(),
            provider_id: provider_id.into(),
            identity,
        }
    }
}

/// Where a local secret is read from. Only a *reference* is ever recorded;
/// the secret value itself is read at the moment it is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// An environment variable of the running process.
    Environment,
    /// A file on this node, read as UTF-8 and trimmed.
    File,
}

/// A local secret binding. It names where a credential lives; it never
/// contains the credential, so it is safe to keep in a local configuration
/// file and to describe in diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretBinding {
    pub source: SecretSource,
    /// Environment variable name, or file path.
    pub reference: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretError {
    Missing,
    Unreadable,
    Empty,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::Missing => write!(f, "secret reference is not configured"),
            SecretError::Unreadable => write!(f, "secret reference could not be read"),
            SecretError::Empty => write!(f, "secret reference is empty"),
        }
    }
}

impl std::error::Error for SecretError {}

impl SecretBinding {
    /// Read the secret from its local source. The value never leaves this
    /// call: callers pass it straight to a transport and never log it.
    pub fn resolve(&self) -> Result<String, SecretError> {
        let raw = match self.source {
            SecretSource::Environment => std::env::var(&self.reference).map_err(|_| SecretError::Missing)?,
            SecretSource::File => {
                std::fs::read_to_string(&self.reference).map_err(|_| SecretError::Unreadable)?
            }
        };
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() { return Err(SecretError::Empty); }
        Ok(trimmed)
    }
}

/// One account this node can observe: its provider, its real scope identity
/// and the local secret binding used to read account-scoped management data.
/// This record is local configuration; it is never written into the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBinding {
    pub provider_id: String,
    pub scope: QuotaScope,
    /// The credential used for account-scoped management reads. Absent when
    /// the account is only observed through ordinary traffic.
    #[serde(default)]
    pub secret: Option<SecretBinding>,
    /// Management API origin. Absent means the provider's documented
    /// default origin.
    #[serde(default)]
    pub management_base_url: Option<String>,
    /// Where each source contract's payload is read from. The path of a
    /// management endpoint belongs to a provider API version, so the node is
    /// told the real path instead of guessing one.
    #[serde(default)]
    pub endpoints: Vec<SourceEndpoint>,
    /// The provider API version this account's management reads use, sent
    /// as the provider's own version header.
    #[serde(default)]
    pub api_version: Option<String>,
    /// How often this account may be polled. Absent uses the default
    /// interval; a management report is never polled faster than the
    /// provider supports.
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
}

impl AccountBinding {
    pub fn new(provider_id: impl Into<String>, scope: QuotaScope) -> Self {
        Self {
            provider_id: provider_id.into(),
            scope,
            secret: None,
            management_base_url: None,
            endpoints: Vec::new(),
            api_version: None,
            refresh_interval_secs: None,
        }
    }

    /// The endpoint of one source contract, if this account reads it.
    pub fn endpoint(&self, source_contract_ref: &str) -> Option<&SourceEndpoint> {
        self.endpoints
            .iter()
            .find(|endpoint| endpoint.source_contract_ref == source_contract_ref)
    }

    /// The polling interval of this account.
    pub fn refresh_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.refresh_interval_secs.unwrap_or(DEFAULT_REFRESH_SECS))
    }
}

/// The default polling interval of one account. It is deliberately slower
/// than the subject's step cadence: quota observation is background
/// evidence, not a per-step probe.
pub const DEFAULT_REFRESH_SECS: u64 = 300;

/// Where one source contract's payload is read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EndpointKind {
    /// A JSON document that a real CLI session wrote on this node. The
    /// document is read as it is; Shenko never starts a paid generation to
    /// produce one.
    StatuslineFile {
        path: String,
    },
    /// A management API document at `{management_base_url}{path}`.
    ManagementApi {
        path: String,
    },
}

/// One source contract and the local place its payload comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceEndpoint {
    pub source_contract_ref: String,
    pub endpoint: EndpointKind,
}

/// The local account configuration of one node.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountConfig {
    #[serde(default)]
    pub accounts: Vec<AccountBinding>,
}

impl AccountConfig {
    /// Parse a local account configuration. A malformed document is an
    /// error: a half-read account list would silently observe the wrong
    /// scope.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    pub fn find(&self, provider_id: &str, scope_ref: &str) -> Option<&AccountBinding> {
        self.accounts
            .iter()
            .find(|account| account.provider_id == provider_id && account.scope.scope_ref == scope_ref)
    }

    pub fn for_provider(&self, provider_id: &str) -> impl Iterator<Item = &AccountBinding> {
        self.accounts
            .iter()
            .filter(move |account| account.provider_id == provider_id)
    }
}
