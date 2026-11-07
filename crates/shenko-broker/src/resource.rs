//! Physical resource domains — SPEC §13.1, §13.7.3, §41.9.
//!
//! A resource domain is one physical engine/device pool. It owns the compute
//! budget C, the context residency pool K and the quarantined debt of requests
//! whose outcome is unknown. A domain is identified by the deployment, never by
//! a URL: two endpoints, model aliases or protocol routes that reach one
//! resident engine must resolve to one domain, so a second alias cannot buy a
//! second full budget.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

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
            kv: RwLock::new(KvPoolState::new(kv_capacity, kv_provenance)),
            external: RwLock::new(Vec::new()),
            debt: RwLock::new(Vec::new()),
        })
    }

    pub fn identity(&self) -> ResourceDomainIdentity {
        self.identity.read().clone()
    }

    /// The compute budget of this domain.
    pub fn admission(&self) -> &AdmissionController {
        &self.admission
    }

    pub fn kv(&self) -> parking_lot::RwLockReadGuard<'_, KvPoolState> {
        self.kv.read()
    }

    pub fn kv_mut(&self) -> parking_lot::RwLockWriteGuard<'_, KvPoolState> {
        self.kv.write()
    }

    /// Adopt one discovered residency pool size (SPEC §13.3).
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
        let mut clients = self.external.write();
        clients.retain(|client| client.client_ref != usage.client_ref);
        clients.push(usage);
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

    /// The compute capacity that is actually available now: the configured
    /// budget minus the quarantined unknown debt and the enforceable external
    /// reservations this domain knows about.
    pub fn effective_compute_capacity(&self) -> CapacitySnapshot {
        let configured = self.admission.capacity();
        let debt = self.debt.read().len() as u64;
        let external: u64 = self
            .external
            .read()
            .iter()
            .filter(|client| client.enforceable)
            .map(|client| client.units)
            .sum();
        let unavailable = debt.saturating_add(external);
        let value = configured.value.map(|value| value.saturating_sub(unavailable));
        CapacitySnapshot {
            pool: CapacityPool::Compute,
            value,
            provenance: if external > 0 || debt > 0 {
                CapacityProvenance::Measured
            } else {
                configured.provenance
            },
        }
    }

    /// Whether this domain's hard guarantees hold. A domain with unmanaged
    /// external clients, an unknown engine incarnation or unknown KV capacity
    /// can only offer soft guarantees.
    pub fn hard_guarantee_available(&self) -> bool {
        self.identity.read().engine_incarnation.is_some()
            && self.external.read().iter().all(|client| client.enforceable)
            && self.kv.read().capacity_units.is_some()
    }

    pub fn backend_family(&self) -> BackendFamily {
        BackendFamily::Other("unclassified".into())
    }
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
        domains.insert(identity.domain_id.clone(), domain);
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
    pub fn resolve(&self, profile: &ProviderProfile) -> anyhow::Result<Arc<ResourceDomain>> {
        let aliases = self.aliases.read();
        let domain_id = aliases
            .get(&profile.endpoint_origin)
            .or_else(|| aliases.get(&profile.profile_id))
            .cloned();
        drop(aliases);
        if let Some(domain_id) = domain_id {
            let domain = self
                .domain(&domain_id)
                .ok_or_else(|| anyhow::anyhow!("resource domain {domain_id} is not registered"))?;
            return Ok(domain);
        }
        if let Some(domain) = self.domain(&profile.resource_domain_id) {
            return Ok(domain);
        }
        // No explicit binding yet: the profile's own domain id defines the
        // engine identity. A second domain for an already known engine
        // incarnation is a capacity multiplication and is refused.
        if let Some(incarnation) = profile.engine_incarnation.as_deref() {
            let conflict = self
                .domains
                .read()
                .values()
                .find(|domain| {
                    domain.identity().engine_incarnation.as_deref() == Some(incarnation)
                        && domain.identity().domain_id != profile.resource_domain_id
                })
                .map(|domain| domain.identity().domain_id);
            if let Some(existing) = conflict {
                anyhow::bail!(
                    "engine incarnation {incarnation} is already bound to resource domain {existing}; \
                     bind this endpoint to that domain instead of creating a second budget"
                );
            }
        }
        anyhow::bail!(
            "no physical resource domain is bound for endpoint {} (declared domain {})",
            profile.endpoint_origin,
            profile.resource_domain_id
        )
    }

    /// Diagnostics: domains that claim the same engine incarnation.
    pub fn incarnation_conflicts(&self) -> Vec<String> {
        let mut conflicts = Vec::new();
        let domains = self.domains.read();
        let identities: Vec<ResourceDomainIdentity> =
            domains.values().map(|domain| domain.identity()).collect();
        for (index, identity) in identities.iter().enumerate() {
            let Some(incarnation) = identity.engine_incarnation.as_deref() else { continue };
            for other in identities.iter().skip(index + 1) {
                if other.engine_incarnation.as_deref() == Some(incarnation) {
                    conflicts.push(format!(
                        "engine incarnation {incarnation} is claimed by domains {} and {}",
                        identity.domain_id, other.domain_id
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
