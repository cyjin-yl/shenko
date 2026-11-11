//! The runtime plugin protocol for provider quota observers.
//!
//! docs/specs/quota-resources.md#spec-42-1: `QuotaObserver`, `QuotaForecaster`, `QuotaPolicy` and the
//! provider adapters are replaceable plugins. This module is the versioned
//! seam they plug into: a named provider registers observers that turn one
//! real source payload into a [`QuotaSnapshot`], and the rest of the system
//! consumes snapshots, never provider-specific payloads.
//!
//! Two rules shape the protocol. First, every observer names the exact
//! source contract it parses (`source_contract_ref`) next to the source
//! kind, so a policy can admit one contract without admitting every payload
//! of that kind. Second, an observer never performs I/O of its own: the
//! runtime hands it a payload that was actually obtained, which keeps a
//! failed or rate-limited read an explicit fact instead of a silent gap.

use serde::{Deserialize, Serialize};

use crate::forecast::LocalAttempt;
use crate::model::{AbsentReason, ObservationError, QuotaSnapshot, SourceKind};
use crate::scope::QuotaScope;

/// The version of this observer protocol. An observer built against another
/// version is refused at registration instead of being interpreted with the
/// wrong field meanings.
pub const QUOTA_OBSERVER_PROTOCOL_VERSION: u32 = 1;

/// What one registered observer is and which source it reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverDescriptor {
    pub protocol_version: u32,
    pub provider_id: String,
    pub source_kind: SourceKind,
    /// The exact source contract this observer parses, for example
    /// `anthropic:management-rate-limits:v1`. Two observers of the same
    /// source kind may carry different contracts; they are different facts.
    pub source_contract_ref: String,
    /// The observed source's own version (CLI build, API version, schema
    /// revision) when the source reports one.
    pub source_version: Option<String>,
    /// The real account scope this observer reads.
    pub scope: QuotaScope,
}

/// One payload an observer can interpret.
#[derive(Debug, Clone)]
pub enum ObservationInput {
    /// A Claude Code statusline document.
    StatuslineJson {
        json: String,
        /// Whether the account has already produced an API response. Without
        /// it, a missing `rate_limits` block means "not yet observed".
        first_response_seen: bool,
    },
    /// Rate-limit response headers of ordinary traffic.
    ApiHeaders(Vec<(String, String)>),
    /// A management API document describing *configured* limits.
    ManagementLimits { json: String },
    /// A management API document describing *reported usage and cost*.
    ManagementUsage { json: String },
    /// Durable local attempts, for a locally measured meter.
    LocalAttempts { attempts: Vec<LocalAttempt> },
    /// The read of the source failed before a payload existed. The observer
    /// turns this into an *unavailable* observation: a failed read is a
    /// typed fact about the observation, never a statement about the
    /// remaining quota and never a balance.
    Unavailable { error: ObservationError, reason: AbsentReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveError {
    /// The payload does not belong to this observer's source.
    SourceMismatch,
    /// The observation itself failed. The quota stays unknown.
    Observation(ObservationError),
}

impl From<ObservationError> for ObserveError {
    fn from(error: ObservationError) -> Self {
        ObserveError::Observation(error)
    }
}

impl std::fmt::Display for ObserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ObserveError::SourceMismatch => write!(f, "quota payload does not match the observer source"),
            ObserveError::Observation(error) => write!(f, "quota observation failed: {error:?}"),
        }
    }
}

impl std::error::Error for ObserveError {}

/// One provider's quota observer.
pub trait QuotaObserver: Send + Sync {
    fn descriptor(&self) -> &ObserverDescriptor;

    /// Turn one real payload into a snapshot. A failed read produces a
    /// snapshot whose status says so; it never produces a balance.
    fn observe(&self, input: &ObservationInput) -> Result<QuotaSnapshot, ObserveError>;
}

/// One named provider's quota plugin.
///
/// A plugin contributes the observers of the source contracts that provider
/// can really be read through, for the account scopes this node is bound
/// to. Registration is explicit: a provider that is not configured has no
/// plugin, and nothing about it is assumed to exist.
pub trait QuotaProviderPlugin: Send + Sync {
    /// The provider's stable name.
    fn provider_id(&self) -> &str;

    /// The observer protocol this plugin speaks. A plugin built for another
    /// protocol version is refused instead of being read with the wrong
    /// field meanings.
    fn protocol_version(&self) -> u32 {
        QUOTA_OBSERVER_PROTOCOL_VERSION
    }

    /// The observers this plugin contributes.
    fn observers(&self) -> Vec<Box<dyn QuotaObserver>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginError {
    /// The same provider and source contract is already registered.
    DuplicateSource,
    /// The observer speaks another protocol version.
    ProtocolMismatch,
    /// The descriptor names no provider, contract or scope.
    InvalidDescriptor,
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::DuplicateSource => write!(f, "quota observer for this source is already registered"),
            PluginError::ProtocolMismatch => write!(f, "quota observer protocol version is not supported"),
            PluginError::InvalidDescriptor => write!(f, "quota observer descriptor is incomplete"),
        }
    }
}

impl std::error::Error for PluginError {}

/// Whether two observers read the same source: the same named provider,
/// the same source contract and the same account scope. Two readers of one
/// source would make the resulting snapshot depend on registration order.
fn same_source(left: &dyn QuotaObserver, right: &dyn QuotaObserver) -> bool {
    let left = left.descriptor();
    let right = right.descriptor();
    left.provider_id == right.provider_id
        && left.source_contract_ref == right.source_contract_ref
        && left.scope.scope_ref == right.scope.scope_ref
}

/// The registry of installed quota observers, keyed by named provider and
/// source contract. Registration is the runtime's job: a provider that is
/// not configured has no observer, and nothing is assumed to exist.
#[derive(Default)]
pub struct QuotaPluginRegistry {
    observers: Vec<Box<dyn QuotaObserver>>,
}

impl QuotaPluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install one observer. A second observer for the same provider and
    /// source contract is refused: two readers of one contract would make
    /// the resulting snapshot depend on registration order.
    pub fn register(&mut self, observer: Box<dyn QuotaObserver>) -> Result<(), PluginError> {
        let descriptor = observer.descriptor();
        if descriptor.protocol_version != QUOTA_OBSERVER_PROTOCOL_VERSION {
            return Err(PluginError::ProtocolMismatch);
        }
        if descriptor.provider_id.trim().is_empty()
            || descriptor.source_contract_ref.trim().is_empty()
            || descriptor.scope.scope_ref.trim().is_empty()
            || descriptor.scope.provider_id.trim().is_empty()
        {
            return Err(PluginError::InvalidDescriptor);
        }
        if self.observers.iter().any(|other| {
            let other = other.descriptor();
            other.provider_id == descriptor.provider_id
                && other.source_contract_ref == descriptor.source_contract_ref
                && other.scope.scope_ref == descriptor.scope.scope_ref
        }) {
            return Err(PluginError::DuplicateSource);
        }
        self.observers.push(observer);
        Ok(())
    }

    /// Install one named provider's plugin. Every observer it contributes
    /// is registered under the same rules as a single observer, and a
    /// plugin that speaks another protocol version installs nothing. The
    /// plugin is installed completely or not at all: a refused observer
    /// never leaves the registry half populated.
    pub fn register_plugin(
        &mut self,
        plugin: Box<dyn QuotaProviderPlugin>,
    ) -> Result<(), PluginError> {
        if plugin.protocol_version() != QUOTA_OBSERVER_PROTOCOL_VERSION {
            return Err(PluginError::ProtocolMismatch);
        }
        if plugin.provider_id().trim().is_empty() {
            return Err(PluginError::InvalidDescriptor);
        }
        let observers = plugin.observers();
        for (index, observer) in observers.iter().enumerate() {
            let descriptor = observer.descriptor();
            if descriptor.protocol_version != QUOTA_OBSERVER_PROTOCOL_VERSION {
                return Err(PluginError::ProtocolMismatch);
            }
            if descriptor.provider_id.trim().is_empty()
                || descriptor.source_contract_ref.trim().is_empty()
                || descriptor.scope.scope_ref.trim().is_empty()
                || descriptor.scope.provider_id.trim().is_empty()
            {
                return Err(PluginError::InvalidDescriptor);
            }
            let duplicated_inside = observers
                .iter()
                .enumerate()
                .any(|(other, candidate)| other != index && same_source(candidate.as_ref(), observer.as_ref()));
            let duplicated_outside = self
                .observers
                .iter()
                .any(|candidate| same_source(candidate.as_ref(), observer.as_ref()));
            if duplicated_inside || duplicated_outside {
                return Err(PluginError::DuplicateSource);
            }
        }
        self.observers.extend(observers);
        Ok(())
    }

    /// Every observer of one named provider.
    pub fn named(&self, provider_id: &str) -> impl Iterator<Item = &dyn QuotaObserver> {
        self.observers
            .iter()
            .filter(move |observer| observer.descriptor().provider_id == provider_id)
            .map(|observer| observer.as_ref())
    }

    /// The observer of one provider and source kind, if that source is
    /// installed for any scope.
    pub fn for_source(
        &self,
        provider_id: &str,
        source_kind: SourceKind,
    ) -> Option<&dyn QuotaObserver> {
        self.observers
            .iter()
            .find(|observer| {
                let descriptor = observer.descriptor();
                descriptor.provider_id == provider_id && descriptor.source_kind == source_kind
            })
            .map(|observer| observer.as_ref())
    }

    /// The observer of one provider, source contract and account scope.
    pub fn for_scope(
        &self,
        provider_id: &str,
        source_contract_ref: &str,
        scope_ref: &str,
    ) -> Option<&dyn QuotaObserver> {
        self.observers
            .iter()
            .find(|observer| {
                let descriptor = observer.descriptor();
                descriptor.provider_id == provider_id
                    && descriptor.source_contract_ref == source_contract_ref
                    && descriptor.scope.scope_ref == scope_ref
            })
            .map(|observer| observer.as_ref())
    }

    /// Every observer bound to one account scope, whatever its contract.
    pub fn observers_for_scope(
        &self,
        provider_id: &str,
        scope_ref: &str,
    ) -> impl Iterator<Item = &dyn QuotaObserver> {
        self.observers
            .iter()
            .filter(move |observer| {
                let descriptor = observer.descriptor();
                descriptor.provider_id == provider_id
                    && descriptor.scope.scope_ref == scope_ref
            })
            .map(|observer| observer.as_ref())
    }

    pub fn observers(&self) -> impl Iterator<Item = &dyn QuotaObserver> {
        self.observers.iter().map(|observer| observer.as_ref())
    }

    /// The provider names that currently have an observer installed.
    pub fn providers(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .observers
            .iter()
            .map(|observer| observer.descriptor().provider_id.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }
}
