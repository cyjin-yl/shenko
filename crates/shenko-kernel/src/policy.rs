//! Root-side egress policy: per-plugin declarations, deny-by-default admission.
//!
//! The runtime root owns exactly one policy registry, published on the same
//! Cordis root context as the hook graph and the live plugin runtime. A plugin
//! that is installed registers the egress its own implementation declared; an
//! uninstall withdraws those declarations with the generation's epoch. Every
//! admitted request is judged by [`shenko_types::egress::evaluate_egress`], the
//! single evaluator, against exactly the declarations in force at that moment.
//!
//! # Deny by default
//!
//! There is no privileged plugin, no built-in exemption and no "internal" flag:
//! [`PluginPolicyRegistry::admit`] has one code path for every caller. A plugin
//! that declared nothing reaches nothing, a capability that was never declared
//! is refused, and a destination outside the declared scope is refused. Both
//! decisions are recorded as receipts; only an allowed decision yields an
//! [`EgressAdmission`], and that type has no public constructor, so a consumer
//! cannot manufacture one.
//!
//! # Binding the attempt to the declaration
//!
//! [`EgressAuthority`] is minted only by [`PluginPolicyRegistry::declare`]. An
//! admission requires one, and the authority names the plugin, capability and
//! registration epoch it was minted for. A caller that never declared has no
//! authority; a caller whose generation was withdrawn, or whose declaration was
//! replaced by a newer generation, holds a stale one and is refused. This is
//! what stops one handler from spending another plugin's declared reach:
//! passing a different `plugin_id` in the request is refused as
//! `authority_mismatch`.
//!
//! # What this is not
//!
//! Native plugins are trusted in-process code. This registry is the root
//! admission contract for egress that goes through the shared execution path;
//! it does not sandbox a handler that calls the OS directly. The guarantee it
//! provides is: no admission token without a live declaration, no declaration
//! without a purpose and a scope, no admission outside that scope, and a receipt
//! for every attempt.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use cordis_core::Service;
use parking_lot::RwLock;

use shenko_types::egress::{
    evaluate_egress, EgressDeclaration, EgressDeclarationError, EgressDecision, EgressDenialCode,
    EgressMatch, EgressRequest, PolicyReceipt,
};
use shenko_types::ids::EntityId;

/// The service name of the root egress policy registry.
pub const PLUGIN_POLICY_SERVICE: &str = "shenko.runtime.policy/v1";

/// How many recent receipts the registry keeps for observation.
///
/// Older receipts are dropped and counted; the registry never claims to have
/// every receipt, and the authoritative copy is the one the caller received.
const RECENT_RECEIPTS: usize = 256;

/// The ownership token of one registered egress declaration.
///
/// The token names the registration epoch it observed, so a late cleanup of a
/// replaced generation cannot withdraw its successor's declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDeclarationHandle {
    plugin_id: EntityId,
    capability_id: String,
    epoch: u64,
}

impl PolicyDeclarationHandle {
    pub fn plugin_id(&self) -> EntityId {
        self.plugin_id
    }

    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    /// The authority to make requests under this declaration.
    ///
    /// The authority is a separate value with no withdrawal power: it admits,
    /// it does not register or retire.
    pub fn authority(&self) -> EgressAuthority {
        EgressAuthority {
            plugin_id: self.plugin_id,
            capability_id: self.capability_id.clone(),
            epoch: self.epoch,
        }
    }
}

/// The right to ask the registry to admit one capability's egress.
///
/// Minted only by [`PluginPolicyRegistry::declare`]; there is no public
/// constructor. Holding one does not admit anything by itself: the live
/// declaration must still cover the specific protocol, host and port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressAuthority {
    plugin_id: EntityId,
    capability_id: String,
    epoch: u64,
}

impl EgressAuthority {
    pub fn plugin_id(&self) -> EntityId {
        self.plugin_id
    }

    pub fn capability_id(&self) -> &str {
        &self.capability_id
    }

    /// The registration epoch this authority was minted for.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// A refused declaration. The live policy is unchanged.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum PolicyRegistrationError {
    #[error(transparent)]
    InvalidDeclaration(#[from] EgressDeclarationError),
}

/// One live declaration and the epoch that registered it.
#[derive(Debug, Clone)]
struct DeclarationEntry {
    declaration: EgressDeclaration,
    epoch: u64,
}

#[derive(Debug, Default)]
struct PolicyState {
    revision: u64,
    declarations: BTreeMap<(EntityId, String), DeclarationEntry>,
    recent: VecDeque<PolicyReceipt>,
    dropped_receipts: u64,
}

impl PolicyState {
    /// Retain a bounded tail of receipts and count what fell out.
    fn record(&mut self, receipt: PolicyReceipt) {
        while self.recent.len() >= RECENT_RECEIPTS {
            if self.recent.pop_front().is_some() {
                self.dropped_receipts += 1;
            }
        }
        self.recent.push_back(receipt);
    }
}

/// The live root egress policy.
#[derive(Debug, Default)]
pub struct PluginPolicyRegistry {
    state: RwLock<PolicyState>,
}

impl Service for PluginPolicyRegistry {
    const NAME: &'static str = PLUGIN_POLICY_SERVICE;
}

/// One admitted egress attempt.
///
/// Only [`PluginPolicyRegistry::admit`] can produce this value: the field is
/// private and there is no public constructor, so requiring an admission is
/// the same as requiring a live allowed decision.
#[derive(Debug, Clone)]
pub struct EgressAdmission {
    receipt: PolicyReceipt,
}

impl EgressAdmission {
    pub fn receipt(&self) -> &PolicyReceipt {
        &self.receipt
    }

    /// The declared scope that admitted this attempt.
    pub fn matched(&self) -> Option<&EgressMatch> {
        match &self.receipt.decision {
            EgressDecision::Allowed { matched } => Some(matched),
            EgressDecision::Denied { .. } => None,
        }
    }
}

/// One refused egress attempt, carrying the receipt the refusal produced.
#[derive(Debug, Clone)]
pub struct PolicyDenial {
    receipt: Box<PolicyReceipt>,
}

impl PolicyDenial {
    pub fn code(&self) -> EgressDenialCode {
        self.receipt
            .denial_code()
            .unwrap_or(EgressDenialCode::InvalidRequest)
    }

    pub fn receipt(&self) -> &PolicyReceipt {
        self.receipt.as_ref()
    }
}

impl std::fmt::Display for PolicyDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "egress refused ({}): {} -> {}://{}:{} at policy revision {}, receipt {}",
            self.code(),
            self.receipt.request.capability_id,
            self.receipt.request.protocol.as_str(),
            self.receipt.request.host,
            self.receipt.request.port,
            self.receipt.policy_revision,
            self.receipt.receipt_id,
        )
    }
}

impl std::error::Error for PolicyDenial {}

impl PluginPolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The revision of the declaration set.
    ///
    /// It advances on declaration and withdrawal only. An admission does not
    /// move it: a receipt names the policy state it was judged against, not the
    /// number of requests that have happened.
    pub fn policy_revision(&self) -> u64 {
        self.state.read().revision
    }

    /// Register or replace one plugin capability's egress declaration.
    ///
    /// Replacing is how an upgrade publishes a narrower or wider scope: the
    /// declaration digest and the registration epoch both change, so a receipt
    /// from the old generation is visibly judged against different bytes. The
    /// previous holder's authority becomes stale, which is the point.
    pub fn declare(
        &self,
        declaration: EgressDeclaration,
    ) -> Result<PolicyDeclarationHandle, PolicyRegistrationError> {
        declaration.validate()?;
        let mut state = self.state.write();
        state.revision += 1;
        let epoch = state.revision;
        let key = (declaration.plugin_id, declaration.capability_id.clone());
        let handle = PolicyDeclarationHandle {
            plugin_id: declaration.plugin_id,
            capability_id: declaration.capability_id.clone(),
            epoch,
        };
        state
            .declarations
            .insert(key, DeclarationEntry { declaration, epoch });
        Ok(handle)
    }

    /// Withdraw exactly the declaration this token registered.
    pub fn withdraw_declaration(&self, handle: &PolicyDeclarationHandle) -> bool {
        let mut state = self.state.write();
        let key = (handle.plugin_id, handle.capability_id.clone());
        let Some(entry) = state.declarations.get(&key) else {
            return false;
        };
        if entry.epoch != handle.epoch {
            return false;
        }
        state.declarations.remove(&key);
        state.revision += 1;
        true
    }

    /// One live declaration.
    pub fn declaration(
        &self,
        plugin_id: EntityId,
        capability_id: &str,
    ) -> Option<EgressDeclaration> {
        self.state
            .read()
            .declarations
            .get(&(plugin_id, capability_id.to_owned()))
            .map(|entry| entry.declaration.clone())
    }

    /// Every live declaration of one plugin, in capability order.
    pub fn declarations_of(&self, plugin_id: EntityId) -> Vec<EgressDeclaration> {
        self.state
            .read()
            .declarations
            .values()
            .filter(|entry| entry.declaration.plugin_id == plugin_id)
            .map(|entry| entry.declaration.clone())
            .collect()
    }

    /// Every live declaration, in (plugin, capability) order.
    pub fn declarations(&self) -> Vec<EgressDeclaration> {
        self.state
            .read()
            .declarations
            .values()
            .map(|entry| entry.declaration.clone())
            .collect()
    }

    /// The most recent admission attempts, oldest first.
    pub fn recent_receipts(&self) -> Vec<PolicyReceipt> {
        self.state.read().recent.iter().cloned().collect()
    }

    /// How many receipts fell out of the bounded observation buffer.
    pub fn dropped_receipts(&self) -> u64 {
        self.state.read().dropped_receipts
    }

    /// Ask the one evaluator to admit, or refuse, one concrete attempt.
    ///
    /// A receipt is produced and retained for both outcomes. Only an allowed
    /// decision yields an [`EgressAdmission`]; a refusal returns a
    /// [`PolicyDenial`] that carries its own receipt, so a caller cannot lose
    /// the reason it was stopped.
    pub fn admit(
        &self,
        authority: &EgressAuthority,
        request: &EgressRequest,
    ) -> Result<EgressAdmission, PolicyDenial> {
        let mut state = self.state.write();
        let revision = state.revision;
        let canonical = request.canonical();
        let live_epoch = state
            .declarations
            .get(&(authority.plugin_id, authority.capability_id.clone()))
            .map(|entry| entry.epoch);
        // The binding check decides whether this caller may spend this
        // capability at all; only then does the destination evaluator run.
        let binding_denial = match (&canonical, live_epoch) {
            (None, _) => Some(EgressDenialCode::InvalidRequest),
            (Some(_), Some(epoch)) if epoch == authority.epoch => None,
            (Some(_), _) => Some(EgressDenialCode::AuthorityStale),
        };
        let binding_denial = binding_denial.or_else(|| {
            let canonical = canonical.as_ref()?;
            (canonical.plugin_id != authority.plugin_id
                || canonical.capability_id != authority.capability_id)
                .then_some(EgressDenialCode::AuthorityMismatch)
        });
        let decision = match binding_denial {
            Some(code) => EgressDecision::denied(code),
            None => {
                let declarations: Vec<EgressDeclaration> = state
                    .declarations
                    .values()
                    .filter(|entry| entry.declaration.plugin_id == authority.plugin_id)
                    .map(|entry| entry.declaration.clone())
                    .collect();
                match canonical.as_ref() {
                    Some(canonical) => evaluate_egress(&declarations, canonical),
                    None => EgressDecision::denied(EgressDenialCode::InvalidRequest),
                }
            }
        };
        let receipt = PolicyReceipt {
            schema_version: PolicyReceipt::SCHEMA_VERSION,
            receipt_id: EntityId::new(),
            request: canonical.unwrap_or_else(|| request.clone()),
            decision,
            policy_revision: revision,
            recorded_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        state.record(receipt.clone());
        drop(state);
        if receipt.is_allowed() {
            Ok(EgressAdmission { receipt })
        } else {
            Err(PolicyDenial { receipt: Box::new(receipt) })
        }
    }
}
