//! Named provider quota observers.
//!
//! Each module here implements the versioned observer protocol of
//! [`crate::plugin`] for one provider. An observer turns a payload that was
//! really obtained into a snapshot; it never performs I/O and never starts a
//! paid generation to look for a balance.
//!
//! [`ProviderQuotaPlugin`] is the runtime piece: it binds a named provider
//! to the account scopes this node is bound to and contributes the observers
//! of the source contracts that provider can really be read through. A
//! provider that is not configured contributes nothing, and a source
//! contract that is not configured for an account is never read.

pub mod anthropic;
pub mod local_meter;

use crate::plugin::{QuotaObserver, QuotaProviderPlugin};
use crate::scope::{AccountBinding, QuotaScope};

use anthropic::AnthropicObserver;
use local_meter::LocalMeterObserver;

/// The source contracts of the Anthropic management surfaces. They are
/// different facts: a configured limit document is not a usage report.
pub use anthropic::{MANAGEMENT_LIMITS_CONTRACT, MANAGEMENT_USAGE_CONTRACT};

/// The provider id of the Anthropic surface.
pub use anthropic::ANTHROPIC_PROVIDER_ID;

/// The provider id of Shenko's own local meter.
pub const LOCAL_PROVIDER_ID: &str = "local";

/// The source contracts one named provider can really be read through.
///
/// A provider that is not named here has no known contract: nothing is
/// assumed to exist, and no observer is installed for it.
pub fn contracts_for_provider(provider_id: &str) -> &'static [&'static str] {
    match provider_id {
        ANTHROPIC_PROVIDER_ID => &[
            crate::observe::STATUSLINE_CONTRACT,
            crate::observe::API_HEADERS_CONTRACT,
            MANAGEMENT_LIMITS_CONTRACT,
            MANAGEMENT_USAGE_CONTRACT,
        ],
        LOCAL_PROVIDER_ID => &[local_meter::LOCAL_METER_CONTRACT],
        _ => &[],
    }
}

/// The source kind one source contract carries.
pub fn source_kind_for_contract(provider_id: &str, contract: &str) -> Option<crate::model::SourceKind> {
    use crate::model::SourceKind;
    match (provider_id, contract) {
        (ANTHROPIC_PROVIDER_ID, crate::observe::STATUSLINE_CONTRACT) => Some(SourceKind::CliStatusline),
        (ANTHROPIC_PROVIDER_ID, crate::observe::API_HEADERS_CONTRACT) => Some(SourceKind::ApiHeaders),
        (ANTHROPIC_PROVIDER_ID, MANAGEMENT_LIMITS_CONTRACT | MANAGEMENT_USAGE_CONTRACT) => {
            Some(SourceKind::AdminApi)
        }
        (LOCAL_PROVIDER_ID, local_meter::LOCAL_METER_CONTRACT) => Some(SourceKind::LocalMeter),
        _ => None,
    }
}

/// Build the observer of one named provider, source contract and account
/// scope. A provider or contract this build does not know yields no
/// observer: an unknown provider is never approximated by a known one.
pub fn observer_for(
    provider_id: &str,
    source_contract_ref: &str,
    scope: QuotaScope,
    source_version: Option<String>,
) -> Option<Box<dyn QuotaObserver>> {
    let source_kind = source_kind_for_contract(provider_id, source_contract_ref)?;
    Some(match provider_id {
        ANTHROPIC_PROVIDER_ID => Box::new(AnthropicObserver::new(
            scope,
            source_kind,
            source_contract_ref,
            source_version,
        )),
        LOCAL_PROVIDER_ID => Box::new(LocalMeterObserver::new(scope)),
        _ => return None,
    })
}

/// One named provider's quota plugin, bound to the account scopes this node
/// can read.
///
/// The plugin contributes one observer per (account scope, source contract)
/// pair the local configuration actually declares. A contract without a
/// configured endpoint is not read at all: the plugin never invents a
/// source.
pub struct ProviderQuotaPlugin {
    provider_id: String,
    accounts: Vec<AccountBinding>,
}

impl ProviderQuotaPlugin {
    pub fn new(provider_id: impl Into<String>, accounts: Vec<AccountBinding>) -> Self {
        Self {
            provider_id: provider_id.into(),
            accounts,
        }
    }
}

impl QuotaProviderPlugin for ProviderQuotaPlugin {
    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn observers(&self) -> Vec<Box<dyn QuotaObserver>> {
        let mut observers = Vec::new();
        for account in &self.accounts {
            if account.provider_id != self.provider_id {
                continue;
            }
            for contract in contracts_for_provider(&self.provider_id) {
                // The local meter is measured from this node's own ledger,
                // not from a remote endpoint: it needs no configured source.
                if *contract == local_meter::LOCAL_METER_CONTRACT {
                    if let Some(observer) = observer_for(
                        &self.provider_id,
                        contract,
                        account.scope.clone(),
                        None,
                    ) {
                        observers.push(observer);
                    }
                    continue;
                }
                if account.endpoint(contract).is_none() {
                    continue;
                }
                if let Some(observer) = observer_for(
                    &self.provider_id,
                    contract,
                    account.scope.clone(),
                    account.api_version.clone(),
                ) {
                    observers.push(observer);
                }
            }
        }
        observers
    }
}
