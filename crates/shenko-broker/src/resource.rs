//! Physical resource domains — docs/specs/quota-resources.md#spec-13-1, docs/specs/inference.md#spec-13-7-3, docs/specs/inference.md#spec-41-9.
//!
//! A resource domain is one physical engine/device pool. It owns the compute
//! budget C, the context residency pool K and the quarantined debt of requests
//! whose outcome is unknown. A domain is identified by the deployment, never by
//! a URL: two endpoints, model aliases or protocol routes that reach one
//! resident engine must resolve to one domain, so a second alias cannot buy a
//! second full budget.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use tokio::sync::Notify;

use crate::admission::AdmissionGuard;
use crate::admission::{AdmissionController, CapacityPool, CapacityProvenance};
use crate::kv::KvPoolState;
use crate::profile::{BackendFamily, ProviderProfile, TransportKind};

/// The authoritative identity of one physical resource domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceDomainIdentity {
    /// Deployment-chosen identity of the physical engine or device pool.
    pub domain_id: String,
    /// The engine identity behind the domain. Two profiles that reach the same
    /// engine must name the same identity, even under different URLs.
    pub engine_identity: String,
    /// Effective instance of that engine, when it was observed. `None` means
    /// the instance is unknown, not "the current one".
    #[serde(default)]
    pub engine_incarnation: Option<String>,
    /// Monotonic epoch of this domain's control authority.
    pub epoch: u64,
}

/// A request whose remote outcome could not be determined. It keeps its
/// capacity: a deadline, a heartbeat or a dropped connection is not proof that
/// the backend stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownDebt {
    pub request_id: EntityId,
    pub admitted_at_ms: i64,
    pub reason: String,
}

/// One physical engine/device pool with its compute budget, its residency pool
/// and its quarantined debt.
pub struct ResourceDomain {
    identity: RwLock<ResourceDomainIdentity>,
    admission: AdmissionController,
    /// Serializes one admission decision against the whole budget so the
    /// external reservation is checked and the slot claimed in one step.
    admission_gate: Mutex<()>,
    /// Wakes the dispatcher when the external reservation changes and queued
    /// work may fit again.
    budget_changed: Notify,
    kv: RwLock<KvPoolState>,
    external: RwLock<Vec<ExternalClientUsage>>,
    debt: RwLock<Vec<UnknownDebt>>,
}

/// Occupancy by a client this product does not control. It is only a hard
/// guarantee when the reservation is enforceable; otherwise the domain's
/// guarantee is soft and says so.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalClientUsage {
    pub client_ref: String,
    pub units: u64,
    pub enforceable: bool,
    #[serde(default)]
    pub evidence: Option<String>,
}

impl ResourceDomain {
    pub fn new(
        identity: ResourceDomainIdentity,
        compute_capacity: u64,
        compute_provenance: CapacityProvenance,
        kv_capacity: Option<u64>,
        kv_provenance: CapacityProvenance,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: RwLock::new(identity),
            admission: AdmissionController::with_provenance(compute_capacity, compute_provenance),
            admission_gate: Mutex::new(()),
            budget_changed: Notify::new(),
            kv: RwLock::new(KvPoolState::new(kv_capacity, kv_provenance)),
            external: RwLock::new(Vec::new()),
            debt: RwLock::new(Vec::new()),
        })
    }

    pub fn identity(&self) -> ResourceDomainIdentity {
        self.identity.read().clone()
    }

    /// Adopt a newly observed engine incarnation. The registry performs the
    /// ownership check first; this setter only records the observation.
    /// Replacing a known incarnation is allowed — an engine restart produces
    /// a new effective instance — but it happens only through the registry so
    /// the uniqueness check always sees the change.
    pub(crate) fn observe_incarnation(&self, incarnation: &str) {
        self.identity.write().engine_incarnation = Some(incarnation.to_string());
    }

    /// Shared accounting for the broker owner. External callers admit only
    /// through `try_admit`, which includes this domain's reservations.
    pub(crate) fn admission(&self) -> &AdmissionController {
        &self.admission
    }

    pub fn kv(&self) -> parking_lot::RwLockReadGuard<'_, KvPoolState> {
        self.kv.read()
    }

    pub fn kv_mut(&self) -> parking_lot::RwLockWriteGuard<'_, KvPoolState> {
        self.kv.write()
    }

    /// Adopt one discovered residency pool size (docs/specs/quota-resources.md#spec-13-3).
    ///
    /// A probe that reported nothing leaves the pool unknown instead of erasing
    /// a healthy observation, and a weaker provenance never overwrites a
    /// stronger one. A stronger or equal observation replaces the figure even
    /// when it is smaller: an engine restarted with less KV really does have
    /// less. The value stays `None` until something was actually observed.
    pub fn apply_kv_capacity(
        &self,
        capacity_units: Option<u64>,
        provenance: CapacityProvenance,
    ) -> bool {
        let Some(capacity_units) = capacity_units else { return false };
        let mut kv = self.kv.write();
        if kv.capacity_units.is_some()
            && provenance_strength(&kv.provenance) > provenance_strength(&provenance)
        {
            return false;
        }
        kv.capacity_units = Some(capacity_units);
        kv.provenance = provenance;
        true
    }

    /// External, unmanaged clients of this engine.
    pub fn external_clients(&self) -> Vec<ExternalClientUsage> {
        self.external.read().clone()
    }

    pub fn register_external_client(&self, usage: ExternalClientUsage) {
        let _gate = self.admission_gate.lock();
        {
            let mut clients = self.external.write();
            clients.retain(|client| client.client_ref != usage.client_ref);
            clients.push(usage);
        }
        // The reservation changed the real dispatch budget; in-flight work is
        // untouched and the queued path must be woken to re-evaluate it.
        self.budget_changed.notify_one();
    }

    /// Release one external client's reservation entirely. Returns whether a
    /// reservation existed. In-flight local requests keep their charges and
    /// nothing is claimed about the remote request having terminated.
    pub fn release_external_client(&self, client_ref: &str) -> bool {
        let _gate = self.admission_gate.lock();
        let removed = {
            let mut clients = self.external.write();
            let before = clients.len();
            clients.retain(|client| client.client_ref != client_ref);
            clients.len() != before
        };
        if removed {
            self.budget_changed.notify_one();
        }
        removed
    }

    /// Units external clients reserved under an enforceable reservation. These
    /// are deducted from the shared total before local dispatch.
    pub fn external_reserved_units(&self) -> u64 {
        self.external
            .read()
            .iter()
            .filter(|client| client.enforceable)
            .map(|client| client.units)
            .fold(0, u64::saturating_add)
    }

    /// Units external clients occupy that this product cannot enforce. They
    /// are reported, never deducted: an unproven external usage cannot be
    /// turned into a local budget figure, and the hard guarantee degrades to
    /// soft instead.
    pub fn external_advisory_units(&self) -> u64 {
        self.external
            .read()
            .iter()
            .filter(|client| !client.enforceable)
            .map(|client| client.units)
            .fold(0, u64::saturating_add)
    }

    /// The one compute budget of this domain, owned here and read identically
    /// by admission and by diagnostics.
    ///
    /// - `configured` is the declared budget C of the physical engine.
    /// - `active` is the number of charges this product currently holds on the
    ///   engine: dispatched requests plus unknown-outcome charges whose guard
    ///   `finish` retained. The `AdmissionGuard`'s drop semantics keep an
    ///   unreleased charge counted here.
    /// - `unknown_charged` is the quarantined unknown debt. In the dispatch
    ///   path it is a subset of `active` — the retained guard already holds
    ///   the unit — so it is never deducted a second time. A caller that
    ///   quarantines without holding a charge still counts against the
    ///   budget: `occupied` is `max(active, unknown_charged)` so an orphaned
    ///   debt cannot be double-counted nor silently freed.
    /// - `external_enforceable` is the units reserved by external clients under
    ///   an enforceable reservation; they are deducted from C before any local
    ///   dispatch.
    /// - `external_advisory` is external usage this product cannot enforce.
    ///   It is not deducted — an unproven figure cannot become a local bound —
    ///   but it keeps the hard guarantee out of reach.
    /// - `available` is `configured - occupied - external_enforceable`, the
    ///   slots the local dispatch path may still claim right now.
    pub fn compute_budget(&self) -> ComputeBudget {
        let configured = self.admission.capacity().value;
        let active = self.admission.active_count();
        let unknown_charged = self.debt.read().len() as u64;
        let external_enforceable = self.external_reserved_units();
        let external_advisory = self.external_advisory_units();
        let occupied = active.max(unknown_charged);
        let available = configured
            .map(|capacity| capacity.saturating_sub(occupied).saturating_sub(external_enforceable));
        ComputeBudget {
            configured,
            active,
            unknown_charged,
            external_enforceable,
            external_advisory,
            available,
        }
    }

    /// Admit one local request against this domain's budget.
    ///
    /// The check and the slot claim happen inside the domain's admission gate
    /// so an enforceable external reservation cannot be bypassed by a
    /// concurrent local admission: `active + external_enforceable` must stay
    /// below `configured`. Unknown-outcome charges are already counted inside
    /// `active` through the retained guard, so they are not deducted again.
    pub fn try_admit(&self) -> Option<AdmissionGuard> {
        let _gate = self.admission_gate.lock();
        let configured = self.admission.capacity().value;
        let external_enforceable = self.external_reserved_units();
        let active = self.admission.active_count();
        let unknown_charged = self.debt.read().len() as u64;
        let occupied = active.max(unknown_charged);
        if let Some(capacity) = configured {
            if occupied.saturating_add(external_enforceable) >= capacity {
                return None;
            }
        }
        self.admission.try_admit()
    }

    /// The notification a dispatcher waits on when its queue is full against
    /// the reservation: it fires when the external ledger changes.
    pub fn budget_changed(&self) -> &Notify {
        &self.budget_changed
    }

    /// Recover one armed request whose remote outcome is unknown as a retained
    /// charge. It may exceed the configured cap and blocks new work until the
    /// original request is reconciled.
    pub fn recover_unknown(&self) -> AdmissionGuard {
        self.admission.recover_unknown()
    }

    /// Debt that is quarantined because the remote outcome is unknown.
    pub fn quarantined_debt(&self) -> Vec<UnknownDebt> {
        self.debt.read().clone()
    }

    /// Transfer facts, not a release certificate. KV leases, unknown remote
    /// requests and external reservations survive the software incarnation.
    pub fn handoff_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "identity": self.identity(),
            "compute_capacity": self.admission.capacity(),
            "compute_budget": self.compute_budget(),
            "active_or_unknown_charges": self.admission.active_count(),
            "kv": self.kv.read().clone(),
            "unknown_debt": self.quarantined_debt(),
            "external_clients": self.external_clients(),
            "remote_termination_claimed": false,
        })
    }

    /// Quarantine one unknown outcome. Its capacity stays occupied.
    pub fn quarantine(&self, request_id: EntityId, admitted_at_ms: i64, reason: impl Into<String>) {
        let mut debt = self.debt.write();
        if debt.iter().any(|entry| entry.request_id == request_id) {
            return;
        }
        debt.push(UnknownDebt { request_id, admitted_at_ms, reason: reason.into() });
    }

    /// Reconcile one quarantined debt against its original receipt. The
    /// capacity is released by the caller through the admission guard; this
    /// only ends the quarantine.
    pub fn reconcile_debt(&self, request_id: EntityId) -> bool {
        let mut debt = self.debt.write();
        let before = debt.len();
        debt.retain(|entry| entry.request_id != request_id);
        debt.len() != before
    }

    /// The compute capacity that is actually available now: the same
    /// `ComputeBudget.available` the dispatch path enforces, so the reported
    /// figure and the enforced figure cannot diverge.
    pub fn effective_compute_capacity(&self) -> CapacitySnapshot {
        let configured = self.admission.capacity();
        let budget = self.compute_budget();
        CapacitySnapshot {
            pool: CapacityPool::Compute,
            value: budget.available,
            provenance: if budget.external_enforceable > 0 || budget.unknown_charged > 0 {
                CapacityProvenance::Measured
            } else {
                configured.provenance
            },
        }
    }

    /// Whether this domain's hard guarantees hold. A domain with unmanaged
    /// external clients, an unknown engine incarnation or unknown KV capacity
    /// can only offer soft guarantees. The compute guarantee is real here:
    /// enforceable external reservations gate the dispatch path itself, not
    /// only this number.
    pub fn hard_guarantee_available(&self) -> bool {
        self.identity.read().engine_incarnation.is_some()
            && self.external.read().iter().all(|client| client.enforceable)
            && self.kv.read().capacity_units.is_some()
    }

    pub fn backend_family(&self) -> BackendFamily {
        BackendFamily::Other("unclassified".into())
    }
}

/// The one compute budget of a domain. `ResourceDomain::compute_budget` is the
/// single place that computes it; admission decisions and diagnostics read
/// the same figures, so a displayed bound and an enforced bound are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeBudget {
    /// The declared compute budget C of the physical engine, with its
    /// provenance carried in `CapacitySnapshot` accessors instead.
    pub configured: Option<u64>,
    /// Charges this product currently holds on the engine: dispatched
    /// requests plus retained unknown-outcome charges.
    pub active: u64,
    /// Quarantined unknown debt. In the dispatch path it is a subset of
    /// `active`; it is reported, never deducted a second time.
    pub unknown_charged: u64,
    /// Enforceable external reservations, deducted from C before dispatch.
    pub external_enforceable: u64,
    /// Unenforceable external usage: reported, never deducted.
    pub external_advisory: u64,
    /// Slots the local dispatch path may still claim right now:
    /// `configured - occupied - external_enforceable`.
    pub available: Option<u64>,
}

/// The compute capacity of one domain at this moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacitySnapshot {
    pub pool: CapacityPool,
    pub value: Option<u64>,
    pub provenance: CapacityProvenance,
}

/// How much a capacity provenance can be trusted. A stronger observation
/// replaces a weaker one; an unknown figure never replaces a known one.
fn provenance_strength(provenance: &CapacityProvenance) -> u8 {
    match provenance {
        CapacityProvenance::Unknown => 0,
        CapacityProvenance::Estimated => 1,
        CapacityProvenance::Configured => 2,
        CapacityProvenance::Measured => 3,
    }
}

/// The registry that resolves a binding to its physical domain.
///
/// The deployment binds aliases explicitly. A profile that reaches an engine
/// through a different URL, model alias or protocol route must be bound to the
/// same domain; an unbound second domain for one engine is refused instead of
/// silently doubling the budget.
pub struct ResourceDomainRegistry {
    domains: RwLock<HashMap<String, Arc<ResourceDomain>>>,
    aliases: RwLock<HashMap<String, String>>,
}

impl Default for ResourceDomainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceDomainRegistry {
    pub fn new() -> Self {
        Self { domains: RwLock::new(HashMap::new()), aliases: RwLock::new(HashMap::new()) }
    }

    /// Register one domain. Registering the same domain id twice keeps the
    /// first registration; the identity must not change underneath it.
    ///
    /// A second domain id that claims the same observed physical engine —
    /// the same `engine_identity` and `engine_incarnation` — is refused rather
    /// than silently doubling the budget; the deployment binds an alias to
    /// the owning domain instead. Domains with an unknown incarnation claim
    /// no physical instance and do not conflict.
    pub fn register(&self, domain: Arc<ResourceDomain>) -> anyhow::Result<()> {
        let identity = domain.identity();
        let mut domains = self.domains.write();
        if let Some(existing) = domains.get(&identity.domain_id) {
            let existing_identity = existing.identity();
            anyhow::ensure!(
                existing_identity.engine_identity == identity.engine_identity,
                "resource domain {} is already bound to engine {}",
                identity.domain_id,
                existing_identity.engine_identity
            );
            return Ok(());
        }
        if let Some(incarnation) = identity.engine_incarnation.as_deref() {
            let conflict = domains
                .values()
                .find(|registered| {
                    let other = registered.identity();
                    other.engine_identity == identity.engine_identity
                        && other.engine_incarnation.as_deref() == Some(incarnation)
                })
                .map(|registered| registered.identity().domain_id);
            if let Some(existing) = conflict {
                anyhow::bail!(
                    "engine incarnation {incarnation} of engine {} is already bound to resource \
                     domain {existing}; bind this endpoint to that domain instead of creating a \
                     second budget",
                    identity.engine_identity
                );
            }
        }
        domains.insert(identity.domain_id.clone(), domain);
        Ok(())
    }

    /// Adopt a newly observed incarnation for one registered domain — for
    /// example after an engine restart or a late probe. The observation may
    /// not claim an incarnation a different domain already owns inside the
    /// same engine scope, and it cannot erase an already known incarnation:
    /// an unknown figure never replaces a known one.
    pub fn observe_incarnation(&self, domain_id: &str, incarnation: &str) -> anyhow::Result<()> {
        let domains = self.domains.write();
        let domain = domains
            .get(domain_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("resource domain {domain_id} is not registered"))?;
        let identity = domain.identity();
        let conflict = domains
            .values()
            .find(|registered| {
                let other = registered.identity();
                other.domain_id != domain_id
                    && other.engine_identity == identity.engine_identity
                    && other.engine_incarnation.as_deref() == Some(incarnation)
            })
            .map(|registered| registered.identity().domain_id);
        if let Some(existing) = conflict {
            anyhow::bail!(
                "engine incarnation {incarnation} of engine {} is already bound to resource \
                 domain {existing}; that domain owns the budget this engine may use",
                identity.engine_identity
            );
        }
        domain.observe_incarnation(incarnation);
        Ok(())
    }

    /// Bind an alias — an endpoint origin, a model alias or a route name — to
    /// one domain. The alias is authoritative: a profile resolved through it
    /// shares that domain's budget instead of creating a new one.
    pub fn bind_alias(&self, alias: &str, domain_id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!alias.trim().is_empty(), "resource domain alias is empty");
        let domains = self.domains.read();
        anyhow::ensure!(
            domains.contains_key(domain_id),
            "resource domain {domain_id} is not registered"
        );
        self.aliases.write().insert(alias.to_string(), domain_id.to_string());
        Ok(())
    }

    pub fn aliases(&self) -> Vec<(String, String)> {
        self.aliases.read().iter().map(|(alias, domain)| (alias.clone(), domain.clone())).collect()
    }

    pub fn domain(&self, domain_id: &str) -> Option<Arc<ResourceDomain>> {
        self.domains.read().get(domain_id).cloned()
    }

    pub fn domains(&self) -> Vec<Arc<ResourceDomain>> {
        self.domains.read().values().cloned().collect()
    }

    /// Resolve the physical domain of one profile.
    ///
    /// An explicit alias wins over the profile's own domain id, so a second
    /// URL cannot obtain a second budget. When two different domains claim the
    /// same observed engine incarnation, the resolution fails with the
    /// conflict instead of picking one.
    ///
    /// The incarnation check runs on every path — an alias hit or an already
    /// registered domain id does not bypass it. The check is scoped by
    /// `engine_identity`: an incarnation string is vendor-scoped, not a global
    /// identity, so it is only compared inside the candidate domain's engine
    /// scope. A profile that claims an incarnation the resolved domain does
    /// not own — or that would resolve to a domain whose engine is already
    /// bound elsewhere — fails instead of silently doubling the budget.
    pub fn resolve(&self, profile: &ProviderProfile) -> anyhow::Result<Arc<ResourceDomain>> {
        let aliases = self.aliases.read();
        let domain_id = aliases
            .get(&profile.endpoint_origin)
            .or_else(|| aliases.get(&profile.profile_id))
            .cloned();
        drop(aliases);
        let candidate = if let Some(domain_id) = domain_id {
            Some(
                self.domain(&domain_id)
                    .ok_or_else(|| anyhow::anyhow!("resource domain {domain_id} is not registered"))?,
            )
        } else {
            self.domain(&profile.resource_domain_id)
        };
        // A declared incarnation is checked against the registry on every
        // path — alias hit, already-registered id, or the fall-through below.
        if let Some(incarnation) = profile.engine_incarnation.as_deref() {
            let candidate_identity = candidate.as_ref().map(|domain| domain.identity());
            let candidate_domain_id = candidate_identity
                .as_ref()
                .map(|identity| identity.domain_id.clone());
            let candidate_scope = candidate_identity
                .as_ref()
                .map(|identity| identity.engine_identity.clone());
            let conflict = self
                .domains
                .read()
                .values()
                .find(|domain| {
                    let identity = domain.identity();
                    identity.engine_incarnation.as_deref() == Some(incarnation)
                        && identity.domain_id
                            != candidate_domain_id.as_deref().unwrap_or_default()
                        && candidate_scope
                            .as_deref()
                            .map_or(true, |scope| scope == identity.engine_identity)
                })
                .map(|domain| domain.identity().domain_id);
            if let Some(existing) = conflict {
                anyhow::bail!(
                    "engine incarnation {incarnation} is already bound to resource domain {existing}; \
                     bind this endpoint to that domain instead of creating a second budget"
                );
            }
            if let Some(domain) = &candidate {
                let identity = domain.identity();
                if let Some(recorded) = identity.engine_incarnation.as_deref() {
                    anyhow::ensure!(
                        recorded == incarnation,
                        "profile declares engine incarnation {incarnation} but resource domain {} is \
                         bound to {recorded}; bind this endpoint to the domain that owns the engine \
                         or update the domain's observed incarnation",
                        identity.domain_id
                    );
                }
            }
        }
        if let Some(domain) = candidate {
            return Ok(domain);
        }
        anyhow::bail!(
            "no physical resource domain is bound for endpoint {} (declared domain {})",
            profile.endpoint_origin,
            profile.resource_domain_id
        )
    }

    /// Diagnostics: domains that claim the same engine incarnation inside one
    /// engine-identity scope — a conflict that must never reach two budgets.
    pub fn incarnation_conflicts(&self) -> Vec<String> {
        let mut conflicts = Vec::new();
        let domains = self.domains.read();
        let identities: Vec<ResourceDomainIdentity> =
            domains.values().map(|domain| domain.identity()).collect();
        for (index, identity) in identities.iter().enumerate() {
            let Some(incarnation) = identity.engine_incarnation.as_deref() else { continue };
            for other in identities.iter().skip(index + 1) {
                if other.engine_incarnation.as_deref() == Some(incarnation)
                    && other.engine_identity == identity.engine_identity
                {
                    conflicts.push(format!(
                        "engine incarnation {incarnation} of engine {} is claimed by domains {} and {}",
                        identity.engine_identity, identity.domain_id, other.domain_id
                    ));
                }
            }
        }
        conflicts
    }
}

/// The transport/backend facts a domain exposes to its callers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainDescriptor {
    pub identity: ResourceDomainIdentity,
    pub backend_family: BackendFamily,
    pub transport: TransportKind,
    pub compute: CapacitySnapshot,
    pub kv_units: Option<u64>,
    pub kv_provenance: CapacityProvenance,
    pub quarantined_debts: usize,
    pub external_clients: Vec<ExternalClientUsage>,
}

impl ResourceDomain {
    /// A readable description of the domain for diagnostics and the UI.
    pub fn describe(&self, backend_family: BackendFamily, transport: TransportKind) -> DomainDescriptor {
        let kv = self.kv.read();
        DomainDescriptor {
            identity: self.identity(),
            backend_family,
            transport,
            compute: self.effective_compute_capacity(),
            kv_units: kv.capacity_units,
            kv_provenance: kv.provenance,
            quarantined_debts: self.debt.read().len(),
            external_clients: self.external_clients(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::TransportKind;

    fn domain(domain_id: &str, engine_identity: &str, incarnation: Option<&str>, capacity: u64) -> Arc<ResourceDomain> {
        ResourceDomain::new(
            ResourceDomainIdentity {
                domain_id: domain_id.into(),
                engine_identity: engine_identity.into(),
                engine_incarnation: incarnation.map(str::to_string),
                epoch: 1,
            },
            capacity,
            CapacityProvenance::Configured,
            None,
            CapacityProvenance::Unknown,
        )
    }

    fn profile(profile_id: &str, endpoint_origin: &str, domain_id: &str, incarnation: Option<&str>) -> ProviderProfile {
        ProviderProfile {
            profile_id: profile_id.into(),
            revision: 1,
            backend_family: BackendFamily::Other("test-engine".into()),
            transport: TransportKind::HttpJson,
            endpoint_origin: endpoint_origin.into(),
            model_id: "test-model".into(),
            api_version: None,
            sdk: None,
            request_schema_revision: 1,
            response_schema_revision: 1,
            gateway_profile: None,
            beta_features: Vec::new(),
            sampling: Default::default(),
            auth: crate::profile::AuthBinding::none(),
            usage: Default::default(),
            native_namespaces: Vec::new(),
            resource_domain_id: domain_id.into(),
            engine_incarnation: incarnation.map(str::to_string),
            default_max_tokens: 1024,
            serializer_revision: 1,
        }
    }

    #[test]
    fn an_enforceable_external_reservation_gates_admission_and_diagnostics() {
        let domain = domain("domain-a", "engine-a", Some("inc-1"), 2);
        domain.register_external_client(ExternalClientUsage {
            client_ref: "ext-1".into(),
            units: 1,
            enforceable: true,
            evidence: None,
        });
        // C=2, external=1: exactly one local slot is left, for both the real
        // admission path and the diagnostic view.
        assert_eq!(domain.effective_compute_capacity().value, Some(1));
        assert_eq!(domain.compute_budget().available, Some(1));
        let first = domain.try_admit().expect("the remaining slot admits");
        assert!(domain.try_admit().is_none(), "the reservation blocks the second slot");
        assert_eq!(domain.effective_compute_capacity().value, Some(0));
        drop(first);
        // A dropped-but-unreleased guard retains occupancy: no slot frees up.
        assert!(domain.try_admit().is_none());
        domain.release_external_client("ext-1");
        assert_eq!(domain.effective_compute_capacity().value, Some(1));
        let held = domain.try_admit().expect("released reservation frees the slot");
        held.release_terminal();
        assert!(domain.try_admit().is_some(), "terminal release frees the slot");
    }

    #[test]
    fn external_reservations_saturate_instead_of_wrapping_the_budget() {
        let domain = domain("domain-a", "engine-a", Some("inc-1"), 2);
        for name in ["ext-a", "ext-b"] {
            domain.register_external_client(ExternalClientUsage {
                client_ref: name.into(), units: u64::MAX, enforceable: true, evidence: None,
            });
        }
        assert_eq!(domain.external_reserved_units(), u64::MAX);
        assert_eq!(domain.compute_budget().available, Some(0));
        assert!(domain.try_admit().is_none());
        domain.release_external_client("ext-a");
        assert!(domain.try_admit().is_none());
        domain.release_external_client("ext-b");
        domain.try_admit().unwrap().release_never_dispatched();
        assert_eq!(domain.compute_budget().available, Some(2));
    }

    #[test]
    fn unknown_debt_is_reported_once_and_never_deducted_twice() {
        let domain = domain("domain-a", "engine-a", Some("inc-1"), 2);
        let guard = domain.try_admit().expect("slot admits");
        // Quarantine the request while its guard is still held — exactly what
        // the broker does on an unknown outcome.
        domain.quarantine(EntityId::new(), 0, "outcome unknown");
        let budget = domain.compute_budget();
        assert_eq!(budget.active, 1);
        assert_eq!(budget.unknown_charged, 1);
        // The unknown charge is inside `active`; it must not be deducted a
        // second time, so available is 1, not 0.
        assert_eq!(budget.available, Some(1));
        assert_eq!(domain.effective_compute_capacity().value, Some(1));
        assert!(domain.try_admit().is_some());
        drop(guard);
    }

    #[test]
    fn one_observed_engine_cannot_register_two_budgets() {
        let registry = ResourceDomainRegistry::new();
        let domain_a = domain("domain-a", "engine-a", Some("inc-1"), 1);
        let domain_b = domain("domain-b", "engine-a", Some("inc-1"), 1);
        registry.register(Arc::clone(&domain_a)).unwrap();
        // A different domain_id for the same observed engine is refused —
        // never a second budget object.
        assert!(registry.register(domain_b).is_err());
        assert_eq!(registry.domains().len(), 1);
        // Resolving the second domain's id also fails: no budget at all.
        let second = profile("p2", "https://b.example", "domain-b", Some("inc-1"));
        assert!(registry.resolve(&second).is_err());
    }

    #[test]
    fn an_alias_of_one_engine_resolves_to_one_budget() {
        let registry = ResourceDomainRegistry::new();
        registry
            .register(domain("domain-a", "engine-a", Some("inc-1"), 2))
            .unwrap();
        registry
            .bind_alias("https://other.example", "domain-a")
            .unwrap();
        let via_alias = profile("p2", "https://other.example", "domain-b", Some("inc-1"));
        let via_declared = profile("p1", "https://primary.example", "domain-a", Some("inc-1"));
        let alias_domain = registry.resolve(&via_alias).unwrap();
        let declared_domain = registry.resolve(&via_declared).unwrap();
        // Both paths land on one budget object — one shared capacity.
        assert!(Arc::ptr_eq(&alias_domain, &declared_domain));
        let first = alias_domain.try_admit().expect("slot admits");
        let second = alias_domain.try_admit().expect("slot admits");
        assert!(
            declared_domain.try_admit().is_none(),
            "the shared budget is exhausted for both bindings"
        );
        first.release_terminal();
        second.release_terminal();
    }

    #[test]
    fn a_registered_id_claiming_an_incarnation_owned_elsewhere_is_refused() {
        let registry = ResourceDomainRegistry::new();
        registry
            .register(domain("domain-a", "engine-a", Some("inc-1"), 1))
            .unwrap();
        registry
            .register(domain("domain-b", "engine-a", Some("inc-2"), 1))
            .unwrap();
        // The profile claims to observe inc-1 (owned by domain-a) but names
        // domain-b: the resolve must not silently hand out domain-b's budget.
        let mistaken = profile("p1", "https://x.example", "domain-b", Some("inc-1"));
        match registry.resolve(&mistaken) {
            Ok(_) => panic!("a conflicting incarnation must not resolve"),
            Err(error) => assert!(
                format!("{error}").contains("domain-a"),
                "the error names the real owner: {error}"
            ),
        }
    }

    #[test]
    fn an_alias_hit_still_runs_the_incarnation_check() {
        let registry = ResourceDomainRegistry::new();
        registry
            .register(domain("domain-a", "engine-a", Some("inc-1"), 1))
            .unwrap();
        registry
            .register(domain("domain-b", "engine-a", Some("inc-2"), 1))
            .unwrap();
        registry
            .bind_alias("https://x.example", "domain-b")
            .unwrap();
        // The alias would resolve to domain-b, but the profile observes
        // inc-1 — owned by domain-a inside the same engine scope. The
        // early return must not bypass the conflict.
        let alias_profile = profile("p1", "https://x.example", "domain-b", Some("inc-1"));
        assert!(registry.resolve(&alias_profile).is_err());
    }

    #[test]
    fn truly_independent_engines_keep_independent_budgets() {
        let registry = ResourceDomainRegistry::new();
        // Same incarnation string, different engine scope: vendor-scoped
        // strings are not a global identity.
        let domain_a = domain("domain-a", "engine-a", Some("inc-1"), 1);
        let domain_b = domain("domain-b", "engine-b", Some("inc-1"), 1);
        registry.register(Arc::clone(&domain_a)).unwrap();
        registry.register(Arc::clone(&domain_b)).unwrap();
        let resolved_a = registry
            .resolve(&profile("p1", "https://a.example", "domain-a", Some("inc-1")))
            .unwrap();
        let resolved_b = registry
            .resolve(&profile("p2", "https://b.example", "domain-b", Some("inc-1")))
            .unwrap();
        assert!(!Arc::ptr_eq(&resolved_a, &resolved_b));
        let guard_a = resolved_a.try_admit().expect("engine-a budget admits");
        assert!(
            resolved_b.try_admit().is_some(),
            "engine-b keeps its own budget even while engine-a is full"
        );
        drop(guard_a);
    }

    #[test]
    fn observe_incarnation_owns_the_observation_or_refuses_it() {
        let registry = ResourceDomainRegistry::new();
        registry
            .register(domain("domain-a", "engine-a", Some("inc-1"), 1))
            .unwrap();
        registry
            .register(domain("domain-b", "engine-a", None, 1))
            .unwrap();
        // domain-b may not claim an incarnation already owned by domain-a.
        assert!(registry.observe_incarnation("domain-b", "inc-1").is_err());
        // An unowned observation is adopted and makes the engine identifiable.
        registry.observe_incarnation("domain-b", "inc-2").unwrap();
        assert_eq!(
            registry.domain("domain-b").unwrap().identity().engine_incarnation.as_deref(),
            Some("inc-2")
        );
    }

    #[test]
    fn concurrent_registration_of_one_incarnation_yields_one_budget() {
        let registry = Arc::new(ResourceDomainRegistry::new());
        let mut handles = Vec::new();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        for index in 0..8 {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                registry.register(domain(
                    &format!("domain-{index}"),
                    "engine-a",
                    Some("inc-1"),
                    4,
                ))
            }));
        }
        let outcomes: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
        // Whatever order the threads ran in, exactly one registration owns
        // the observed engine; everyone else was refused.
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(registry.domains().len(), 1);
        let winner = registry.domains();
        assert_eq!(winner.len(), 1);
        assert!(winner[0].identity().domain_id.starts_with("domain-"));
    }

    #[test]
    fn an_unknown_incarnation_is_not_claimed_as_identity() {
        let registry = ResourceDomainRegistry::new();
        // An unobserved incarnation is no identity: two unknown-instance
        // domains register freely, and neither earns a hard guarantee.
        registry
            .register(domain("domain-a", "engine-a", None, 1))
            .unwrap();
        registry
            .register(domain("domain-b", "engine-b", None, 1))
            .unwrap();
        assert!(!registry.domain("domain-a").unwrap().hard_guarantee_available());
        let resolved = registry
            .resolve(&profile("p1", "https://a.example", "domain-a", None))
            .unwrap();
        assert_eq!(resolved.identity().domain_id, "domain-a");
    }
}
