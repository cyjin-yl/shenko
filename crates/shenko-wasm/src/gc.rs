//! Root-aware reclamation of this installation's content-addressed store.
//!
//! A CAS is shared: home-checkpoint closures, COW commit byte closures, and
//! every other Store fact that seals an object all address the same
//! `ContentStore`. Deleting an object is therefore only safe when the caller
//! can prove that *no* producer still names it. This module implements that
//! proof honestly, including the parts it cannot prove:
//!
//! * The set of producers is an administrative declaration
//!   ([`RootRegistry`]: `cas-roots/registry.json`). Reclamation refuses to run
//!   when a declared producer did not enumerate, when a producer reports
//!   itself incomplete, or when a producer reports a blocker (a live branch
//!   writer, an unfinished publication).
//! * Every root is enumerated from durable records, never from "what the code
//!   usually writes": published checkpoint head + journal + pending restore
//!   intent + restore receipts, and every COW branch's head, journal history,
//!   prepared-but-unpublished attempts, staged publication objects and
//!   writer claim.
//! * A reachable digest that is missing from the CAS is a refusal, not a
//!   shrug: a store that already lost a referenced object cannot be reasoned
//!   about safely.
//! * Staging entries (`.object-*`, `.stream-*`) mean a publication is in
//!   flight in this process or another one; reclamation refuses.
//! * Objects created before the registry was adopted have an unknown owner.
//!   They are reported as ungoverned and are never collected. Adoption must
//!   therefore happen before the store starts receiving objects; adopting
//!   later is allowed but leaves the earlier objects permanently retained.
//! * An object created *during* reclamation is retained as well: a
//!   content-addressed name can be re-created after it was read, and refusing
//!   to touch recent entries is cheaper than losing a producer's fresh object.
//!
//! What this does *not* prove is stated in the same breath: a producer that
//! seals into this store but is not declared in the registry is invisible.
//! Reclamation reports every collection it performed with the exact producer
//! set and adoption time it used, so that boundary is auditable rather than
//! assumed.

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::object::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::checkpoint::{HomeCheckpointStore, STORE_NAMESPACE as CHECKPOINT_STORE_NAMESPACE};
use crate::cow::{BranchBusy, BranchRoots, BranchStore, BranchWriter, COW_STORE_NAMESPACE};

/// Installation-local directory holding the producer declaration and each
/// producer's sealed root record.
pub const CAS_ROOTS_DIR: &str = "cas-roots";
pub const REGISTRY_FILE: &str = "registry.json";
pub const REGISTRY_FORMAT: &str = "shenko-cas-roots/1";
pub const PRODUCER_RECORD_FORMAT: &str = "shenko-cas-roots/producer/1";
/// The home/COW producer's id. One subsystem, one producer entry.
pub const HOME_COW_PRODUCER: &str = "home-cow";
/// CAS prefixes used by the CAS itself while a writer is publishing. They are
/// never objects and never collectable; their presence blocks reclamation.
const STAGING_PREFIXES: [&str; 2] = [".object-", ".stream-"];

/// Administrative declaration of every producer whose objects live in one
/// installation's CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRegistry {
    pub format: String,
    /// When this declaration was adopted. Objects older than this have no
    /// declared owner and are never collected.
    pub adopted_at_unix_ms: u64,
    /// Producer ids, sorted and unique.
    pub producers: Vec<String>,
    /// Every CAS namespace this installation may seal into, sorted and
    /// unique. Reclamation verifies that the participating producers cover
    /// exactly this set: a namespace whose objects nobody enumerated makes
    /// "unreachable" meaningless, so it is a refusal rather than an
    /// assumption.
    pub namespaces: Vec<String>,
}

/// Roots one producer contributes, plus anything that makes reclamation
/// unsafe right now.
#[derive(Debug)]
pub struct ProducerRoots {
    pub producer: String,
    pub namespaces: Vec<String>,
    pub roots: BTreeSet<Digest>,
    /// Closures/manifests whose objects are included in `roots`; reported for
    /// auditability, not used to decide anything.
    pub closures: usize,
    /// False when the producer could not enumerate everything it owns. A
    /// false value is a refusal: "I could not list my roots" is never "I have
    /// no roots".
    pub complete: bool,
    pub observed_at_unix_ms: u64,
    /// Reasons reclamation must not run (live writer, unfinished publication,
    /// interrupted restore, ...). Non-empty is a refusal.
    pub blockers: Vec<String>,
    /// Exclusion guards the producer keeps for as long as these roots are in
    /// use (for the home/COW producer: the branch writer claims). Reclamation
    /// holds them until it has finished deleting.
    pub held_exclusions: Vec<BranchWriter>,
}

impl ProducerRoots {
    pub fn new(producer: &str, namespaces: &[&'static str]) -> Self {
        Self {
            producer: producer.to_string(),
            namespaces: namespaces.iter().map(|name| name.to_string()).collect(),
            roots: BTreeSet::new(),
            closures: 0,
            complete: true,
            observed_at_unix_ms: now_unix_ms(),
            blockers: Vec::new(),
            held_exclusions: Vec::new(),
        }
    }
}

/// A durable root record. Producers that can enumerate live (home/COW) do so;
/// a producer that cannot read its own live state can publish this record
/// instead, and reclamation then verifies that the record is present and
/// complete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootRecord {
    pub format: String,
    pub producer: String,
    pub namespaces: Vec<String>,
    pub roots: BTreeSet<Digest>,
    pub closures: usize,
    pub complete: bool,
    pub observed_at_unix_ms: u64,
}

/// One producer whose objects share this CAS.
pub trait CasRootProducer {
    fn producer_id(&self) -> &'static str;
    /// CAS namespaces this producer seals under. Used to detect two producers
    /// claiming the same namespace (ambiguous ownership).
    fn namespace_claims(&self) -> &'static [&'static str];
    fn enumerate(&self) -> Result<ProducerRoots>;
}

/// Why reclamation declined to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcRefusalReason {
    /// No registry was adopted, so no producer set is declared.
    RegistryMissing,
    RegistryInvalid,
    /// A declared producer did not participate in this run.
    ProducerUnregistered { producer: String },
    /// A producer could not enumerate all of its roots.
    ProducerIncomplete { producer: String },
    /// A producer reported a live writer or unfinished publication.
    ProducerBlocked { producer: String },
    /// CAS staging entries exist: a publication is in flight.
    StagingInProgress,
    /// A root named by a producer is missing from the CAS.
    ReachableMissing { digest: Digest },
    /// Two producers claim the same CAS namespace.
    NamespaceOverlap { namespace: String },
    /// The registry declares a namespace that no participating producer
    /// enumerates (its objects could be collected by mistake).
    NamespaceUncovered { namespace: String },
}

impl GcRefusalReason {
    fn label(&self) -> &'static str {
        match self {
            Self::RegistryMissing => "CAS_ROOT_REGISTRY_MISSING",
            Self::RegistryInvalid => "CAS_ROOT_REGISTRY_INVALID",
            Self::ProducerUnregistered { .. } => "CAS_ROOT_PRODUCER_UNREGISTERED",
            Self::ProducerIncomplete { .. } => "CAS_ROOT_PRODUCER_INCOMPLETE",
            Self::ProducerBlocked { .. } => "CAS_ROOT_PRODUCER_BLOCKED",
            Self::StagingInProgress => "CAS_STAGING_IN_PROGRESS",
            Self::ReachableMissing { .. } => "CAS_ROOT_OBJECT_MISSING",
            Self::NamespaceOverlap { .. } => "CAS_ROOT_NAMESPACE_OVERLAP",
            Self::NamespaceUncovered { .. } => "CAS_ROOT_NAMESPACE_UNCOVERED",
        }
    }
}

/// A refusal with its cause and the exact detail an operator needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcRefusal {
    pub reason: GcRefusalReason,
    pub detail: String,
}

impl std::fmt::Display for GcRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason.label(), self.detail)
    }
}

impl std::error::Error for GcRefusal {}

/// One object that was removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedObject {
    pub digest: Digest,
    pub bytes: u64,
}

/// What reclamation decided and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcReceipt {
    pub producers: Vec<String>,
    pub adopted_at_unix_ms: u64,
    pub objects_before: usize,
    pub reachable: usize,
    pub collected: Vec<CollectedObject>,
    pub collected_bytes: u64,
    /// Objects that predate registry adoption: unknown owner, retained.
    pub retained_ungoverned: Vec<Digest>,
    /// Objects that appeared while reclamation was running: retained.
    pub retained_concurrent: Vec<Digest>,
    /// Objects that were unreachable and governed but could not be removed.
    pub failed: Vec<(Digest, String)>,
}

/// Outcome of one reclamation attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GcOutcome {
    Reclaimed(GcReceipt),
    Refused(GcRefusal),
}

/// A planned reclamation: the roots that must stay, and the objects that may
/// go. Held exclusions from the producers live as long as this value.
pub struct GcPlan {
    pub producers: Vec<String>,
    pub adopted_at_unix_ms: u64,
    pub reachable: BTreeSet<Digest>,
    pub objects: BTreeSet<Digest>,
    pub candidates: Vec<Digest>,
    pub retained_ungoverned: Vec<Digest>,
    planned_at_unix_ms: u64,
    held_exclusions: Vec<BranchWriter>,
}

impl GcPlan {
    pub fn held_exclusions(&self) -> usize {
        self.held_exclusions.len()
    }
}

/// Reclamation bound to one installation's store and root registry.
pub struct CasGc<'a> {
    cas: &'a ContentStore,
    data_dir: &'a Path,
}

impl<'a> CasGc<'a> {
    pub fn new(cas: &'a ContentStore, data_dir: &'a Path) -> Self {
        Self { cas, data_dir }
    }

    fn registry_path(&self) -> PathBuf {
        self.data_dir.join(CAS_ROOTS_DIR).join(REGISTRY_FILE)
    }

    /// The adopted producer declaration, if any.
    pub fn registry(&self) -> Result<Option<RootRegistry>> {
        let path = self.registry_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(anyhow!("read root registry {}: {error}", path.display()));
            }
        };
        let registry: RootRegistry = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("decode root registry {}: {error}", path.display()))?;
        ensure!(
            registry.format == REGISTRY_FORMAT,
            "CAS_ROOT_REGISTRY_INVALID: {} declares format {:?}, not {REGISTRY_FORMAT:?}",
            path.display(),
            registry.format
        );
        Ok(Some(registry))
    }

    /// Declare the complete producer set *and* the complete namespace set for
    /// this store.
    ///
    /// Adoption is a one-time administrative act: re-adopting a different set
    /// is refused instead of silently widening or narrowing what may be
    /// collected. The adoption timestamp is the ownership epoch — anything
    /// created before it is reported as ungoverned and never reclaimed.
    ///
    /// `namespaces` must be exactly the set of CAS namespaces this
    /// installation may seal into, and each of them must be claimed by
    /// exactly one supplied producer. That is what makes a later "these
    /// objects are unreachable" claim meaningful: a namespace nobody
    /// enumerated would produce false orphans.
    pub fn adopt_registry(
        &self,
        producers: &[&dyn CasRootProducer],
        namespaces: &[&str],
    ) -> Result<RootRegistry> {
        ensure!(
            !producers.is_empty(),
            "CAS_ROOT_REGISTRY_INVALID: a registry must declare at least one producer"
        );
        ensure!(
            !namespaces.is_empty(),
            "CAS_ROOT_REGISTRY_INVALID: a registry must declare the CAS namespaces this installation writes"
        );
        let mut declared: Vec<String> = producers.iter().map(|id| id.producer_id().to_string()).collect();
        declared.sort();
        declared.dedup();
        ensure!(
            declared.len() == producers.len(),
            "CAS_ROOT_REGISTRY_INVALID: duplicate producer id in the declaration"
        );
        let mut declared_namespaces: Vec<String> =
            namespaces.iter().map(|name| (*name).to_string()).collect();
        declared_namespaces.sort();
        declared_namespaces.dedup();
        ensure!(
            declared_namespaces.len() == namespaces.len(),
            "CAS_ROOT_REGISTRY_INVALID: duplicate namespace in the declaration"
        );
        let mut claimed: BTreeMap<&'static str, &'static str> = BTreeMap::new();
        for producer in producers {
            for namespace in producer.namespace_claims() {
                if let Some(existing) = claimed.insert(*namespace, producer.producer_id()) {
                    bail!(
                        "CAS_ROOT_NAMESPACE_OVERLAP: producers {existing} and {} both claim CAS namespace {namespace:?}",
                        producer.producer_id()
                    );
                }
                ensure!(
                    namespaces.contains(namespace),
                    "CAS_ROOT_REGISTRY_INVALID: producer {} claims namespace {namespace:?} that the declaration omits",
                    producer.producer_id()
                );
            }
        }
        for namespace in namespaces {
            ensure!(
                claimed.contains_key(namespace),
                "CAS_ROOT_NAMESPACE_UNCOVERED: no supplied producer enumerates CAS namespace {namespace:?}; reclaiming would treat its objects as unreachable"
            );
        }
        if let Some(existing) = self.registry()? {
            ensure!(
                existing.producers == declared && existing.namespaces == declared_namespaces,
                "CAS_ROOT_REGISTRY_INVALID: this store already declared producers {:?} over namespaces {:?}; declaring {:?} over {:?} would change what may be collected",
                existing.producers,
                existing.namespaces,
                declared,
                declared_namespaces
            );
            return Ok(existing);
        }
        let registry = RootRegistry {
            format: REGISTRY_FORMAT.into(),
            adopted_at_unix_ms: now_unix_ms(),
            producers: declared,
            namespaces: declared_namespaces,
        };
        let dir = self.data_dir.join(CAS_ROOTS_DIR);
        fs::create_dir_all(&dir)
            .with_context(|| format!("create root registry directory {}", dir.display()))?;
        let bytes = serde_json::to_vec_pretty(&registry).expect("registry serialize");
        fs::write(self.registry_path(), bytes)
            .with_context(|| format!("write root registry {}", self.registry_path().display()))?;
        sync_dir(&dir)?;
        Ok(registry)
    }

    /// Enumerate every root and decide what may be collected, or refuse.
    pub fn plan(&self, producers: &[&dyn CasRootProducer]) -> Result<GcPlan, GcRefusal> {
        let registry = match self.registry() {
            Ok(Some(registry)) => registry,
            Ok(None) => {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryMissing,
                    detail: format!(
                        "no producer declaration at {}; adopt the complete producer set before reclaiming, or the owners of existing objects are unknown",
                        self.registry_path().display()
                    ),
                });
            }
            Err(error) => {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryInvalid,
                    detail: format!("{error:#}"),
                });
            }
        };
        let mut supplied: BTreeMap<&'static str, &dyn CasRootProducer> = BTreeMap::new();
        for producer in producers {
            let id = producer.producer_id();
            if supplied.insert(id, *producer).is_some() {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryInvalid,
                    detail: format!("producer {id} was supplied twice"),
                });
            }
        }
        // Namespace ambiguity is refused before anything is read: two
        // producers that both claim a namespace cannot be told apart later.
        let mut claimed: BTreeMap<String, &'static str> = BTreeMap::new();
        for producer in producers {
            for namespace in producer.namespace_claims() {
                if let Some(other) = claimed.insert((*namespace).to_string(), producer.producer_id()) {
                    return Err(GcRefusal {
                        reason: GcRefusalReason::NamespaceOverlap {
                            namespace: (*namespace).to_string(),
                        },
                        detail: format!(
                            "producers {other} and {} both claim CAS namespace {namespace:?}",
                            producer.producer_id()
                        ),
                    });
                }
            }
        }
        for namespace in &registry.namespaces {
            let covered = producers
                .iter()
                .any(|producer| producer.namespace_claims().contains(&namespace.as_str()));
            if !covered {
                return Err(GcRefusal {
                    reason: GcRefusalReason::NamespaceUncovered {
                        namespace: namespace.clone(),
                    },
                    detail: format!(
                        "the registry declares CAS namespace {namespace:?} but no participating producer enumerates it; its objects cannot be proven unreachable"
                    ),
                });
            }
        }
        let mut roots: BTreeSet<Digest> = BTreeSet::new();
        let mut exclusions: Vec<BranchWriter> = Vec::new();
        for id in &registry.producers {
            let Some(producer) = supplied.get(id.as_str()) else {
                return Err(GcRefusal {
                    reason: GcRefusalReason::ProducerUnregistered {
                        producer: id.clone(),
                    },
                    detail: format!(
                        "the registry declares producer {id} but it did not participate; its objects cannot be assumed unreferenced"
                    ),
                });
            };
            let enumerated = match producer.enumerate() {
                Ok(enumerated) => enumerated,
                Err(error) => {
                    return Err(GcRefusal {
                        reason: GcRefusalReason::ProducerIncomplete {
                            producer: id.clone(),
                        },
                        detail: format!("producer {id} could not enumerate its roots: {error:#}"),
                    });
                }
            };
            if enumerated.producer != *id {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryInvalid,
                    detail: format!(
                        "producer {id} reported itself as {:?}",
                        enumerated.producer
                    ),
                });
            }
            if !enumerated.complete {
                return Err(GcRefusal {
                    reason: GcRefusalReason::ProducerIncomplete {
                        producer: id.clone(),
                    },
                    detail: format!("producer {id} reported an incomplete root set"),
                });
            }
            if !enumerated.blockers.is_empty() {
                return Err(GcRefusal {
                    reason: GcRefusalReason::ProducerBlocked {
                        producer: id.clone(),
                    },
                    detail: format!(
                        "producer {id} reports reclamation must not run: {}",
                        enumerated.blockers.join("; ")
                    ),
                });
            }
            roots.extend(enumerated.roots);
            exclusions.extend(enumerated.held_exclusions);
        }
        // Staging entries are publications in flight. Their content is not an
        // object yet, so it cannot be resolved to a digest and pinned.
        let mut staging: Vec<String> = Vec::new();
        match fs::read_dir(self.cas.base()) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Err(GcRefusal {
                                reason: GcRefusalReason::RegistryInvalid,
                                detail: format!("list CAS root: {error}"),
                            });
                        }
                    };
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    if STAGING_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) {
                        staging.push(name.to_string());
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryInvalid,
                    detail: format!("list CAS root: {error}"),
                });
            }
        }
        if !staging.is_empty() {
            staging.sort();
            return Err(GcRefusal {
                reason: GcRefusalReason::StagingInProgress,
                detail: format!(
                    "CAS has {} in-progress staging entr{} ({}); a publication that has not been named yet cannot be pinned",
                    staging.len(),
                    if staging.len() == 1 { "y" } else { "ies" },
                    staging.join(", ")
                ),
            });
        }
        let objects: BTreeSet<Digest> = match self.cas.object_digests() {
            Ok(objects) => objects.into_iter().collect(),
            Err(error) => {
                return Err(GcRefusal {
                    reason: GcRefusalReason::RegistryInvalid,
                    detail: format!("enumerate CAS objects: {error}"),
                });
            }
        };
        for digest in &roots {
            if !objects.contains(digest) {
                return Err(GcRefusal {
                    reason: GcRefusalReason::ReachableMissing {
                        digest: digest.clone(),
                    },
                    detail: format!(
                        "a producer names reachable object {digest} that is not in the CAS; refusing to reclaim a store whose closure is already incomplete"
                    ),
                });
            }
        }
        let planned_at_unix_ms = now_unix_ms();
        let mut candidates = Vec::new();
        let mut retained_ungoverned = Vec::new();
        for digest in &objects {
            if roots.contains(digest) {
                continue;
            }
            let Some(modified) = self.object_modified_ms(digest) else {
                // A metadata read failure is not evidence of unreachability.
                retained_ungoverned.push(digest.clone());
                continue;
            };
            if modified < registry.adopted_at_unix_ms {
                retained_ungoverned.push(digest.clone());
            } else {
                candidates.push(digest.clone());
            }
        }
        Ok(GcPlan {
            producers: registry.producers,
            adopted_at_unix_ms: registry.adopted_at_unix_ms,
            reachable: roots,
            objects,
            candidates,
            retained_ungoverned,
            planned_at_unix_ms,
            held_exclusions: exclusions,
        })
    }

    /// Plan and reclaim in one call, returning either the receipt or the
    /// reason reclamation declined.
    pub fn reclaim(&self, producers: &[&dyn CasRootProducer]) -> Result<GcOutcome> {
        let plan = match self.plan(producers) {
            Ok(plan) => plan,
            Err(refusal) => return Ok(GcOutcome::Refused(refusal)),
        };
        let mut receipt = GcReceipt {
            producers: plan.producers.clone(),
            adopted_at_unix_ms: plan.adopted_at_unix_ms,
            objects_before: plan.objects.len(),
            reachable: plan.reachable.len(),
            collected: Vec::new(),
            collected_bytes: 0,
            retained_ungoverned: plan.retained_ungoverned.clone(),
            retained_concurrent: Vec::new(),
            failed: Vec::new(),
        };
        for digest in &plan.candidates {
            // Re-check the object immediately before removing it: an object
            // re-created by a publisher after the plan was taken is not the one
            // that was judged unreachable.
            match self.object_modified_ms(digest) {
                Some(modified) if modified > plan.planned_at_unix_ms => {
                    receipt.retained_concurrent.push(digest.clone());
                    continue;
                }
                None => {
                    // It disappeared between plan and delete; nothing to do and
                    // nothing to claim.
                    continue;
                }
                Some(_) => {}
            }
            let bytes = self.cas.object_size(digest).unwrap_or(0);
            match self.cas.remove_object(digest) {
                Ok(true) => {
                    receipt.collected.push(CollectedObject {
                        digest: digest.clone(),
                        bytes,
                    });
                    receipt.collected_bytes += bytes;
                }
                Ok(false) => {}
                Err(error) => receipt.failed.push((digest.clone(), format!("{error}"))),
            }
        }
        drop(plan);
        Ok(GcOutcome::Reclaimed(receipt))
    }

    /// Last-modification time of one object file, if it exists.
    fn object_modified_ms(&self, digest: &Digest) -> Option<u64> {
        let hex = digest.canonical_blake3_hex()?;
        let path = self
            .cas
            .base()
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(hex);
        let metadata = fs::metadata(path).ok()?;
        metadata
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_millis() as u64)
    }
}

/// The home + COW producer: checkpoint closures and branch commit closures
/// that share this installation's CAS.
pub struct HomeCowRoots<'a> {
    cas: &'a ContentStore,
    checkpoints: Option<HomeCheckpointStore<'a>>,
    /// Root of the managed `/home` mount, which holds `workspaces/`.
    home_root: PathBuf,
}

impl<'a> HomeCowRoots<'a> {
    pub fn new(
        cas: &'a ContentStore,
        checkpoints: Option<HomeCheckpointStore<'a>>,
        home_root: PathBuf,
    ) -> Self {
        Self {
            cas,
            checkpoints,
            home_root,
        }
    }

    /// Prepared branches under `<home>/workspaces/<workspace>/branches/<branch>`.
    /// A branch directory that cannot be read is reported, never skipped.
    pub fn branches(&self) -> Result<Vec<BranchStore>> {
        let workspaces = self.home_root.join("workspaces");
        let read = match fs::read_dir(&workspaces) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(anyhow!(
                    "list COW workspaces {}: {error}",
                    workspaces.display()
                ));
            }
        };
        let mut branches = Vec::new();
        for workspace in read {
            let workspace = workspace.map_err(|error| anyhow!("list COW workspaces: {error}"))?;
            let workspace_name = match workspace.file_name().into_string() {
                Ok(name) => name,
                Err(name) => {
                    return Err(anyhow!(
                        "COW workspace name {:?} is not UTF-8",
                        name
                    ));
                }
            };
            let branches_dir = workspace.path().join("branches");
            let read = match fs::read_dir(&branches_dir) {
                Ok(read) => read,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(anyhow!(
                        "list COW branches {}: {error}",
                        branches_dir.display()
                    ));
                }
            };
            for branch in read {
                let branch = branch.map_err(|error| anyhow!("list COW branches: {error}"))?;
                if !branch
                    .file_type()
                    .map(|kind| kind.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let branch_name = match branch.file_name().into_string() {
                    Ok(name) => name,
                    Err(name) => {
                        return Err(anyhow!("COW branch name {:?} is not UTF-8", name));
                    }
                };
                branches.push(BranchStore::bind_absolute(&workspace_name, &branch_name));
            }
        }
        Ok(branches)
    }
}

impl CasRootProducer for HomeCowRoots<'_> {
    fn producer_id(&self) -> &'static str {
        HOME_COW_PRODUCER
    }

    fn namespace_claims(&self) -> &'static [&'static str] {
        &[CHECKPOINT_STORE_NAMESPACE, COW_STORE_NAMESPACE]
    }

    fn enumerate(&self) -> Result<ProducerRoots> {
        let mut roots = ProducerRoots::new(
            HOME_COW_PRODUCER,
            &[CHECKPOINT_STORE_NAMESPACE, COW_STORE_NAMESPACE],
        );
        if let Some(checkpoints) = &self.checkpoints {
            let checkpoint_roots = checkpoints.roots()?;
            roots.closures += checkpoint_roots.closures.len();
            roots.roots.extend(checkpoint_roots.closures.iter().cloned());
            roots.roots.extend(checkpoint_roots.objects.iter().cloned());
        }
        for branch in self.branches()? {
            let label = format!("{}/{}", branch.workspace, branch.branch);
            let branch_roots: BranchRoots =
                match branch.roots_abs(&self.home_root, self.cas) {
                    Ok(branch_roots) => branch_roots,
                    Err(error) => match error.downcast_ref::<BranchBusy>() {
                        Some(BranchBusy::Writer { .. }) => {
                            // A live writer may be about to seal objects it has
                            // not named yet: the root set cannot be complete.
                            roots.blockers.push(format!(
                                "COW branch {label} is owned by a live writer"
                            ));
                            continue;
                        }
                        None => return Err(error),
                    },
                };
            if branch_roots.publishing {
                roots.blockers.push(format!(
                    "COW branch {label} has an unfinished publication ({} staged objects)",
                    branch_roots.staged.len()
                ));
            }
            roots.closures += branch_roots.commits.len();
            roots.roots.extend(branch_roots.commits.iter().cloned());
            roots.roots.extend(branch_roots.objects.iter().cloned());
            roots.held_exclusions.extend(branch_roots.into_exclusions());
        }
        roots.observed_at_unix_ms = now_unix_ms();
        Ok(roots)
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        bail!("CAS_ROOT_REGISTRY_INVALID: durable registry publication requires directory synchronization");
    }
}

/// Publish a producer's durable root record (for producers that cannot
/// enumerate live during reclamation).
pub fn write_root_record(data_dir: &Path, roots: &ProducerRoots) -> Result<RootRecord> {
    ensure!(
        roots.complete,
        "CAS_ROOT_RECORD_INVALID: refusing to publish an incomplete root record for {}",
        roots.producer
    );
    let record = RootRecord {
        format: PRODUCER_RECORD_FORMAT.into(),
        producer: roots.producer.clone(),
        namespaces: roots.namespaces.clone(),
        roots: roots.roots.clone(),
        closures: roots.closures,
        complete: roots.complete,
        observed_at_unix_ms: roots.observed_at_unix_ms,
    };
    let dir = data_dir.join(CAS_ROOTS_DIR);
    fs::create_dir_all(&dir)
        .with_context(|| format!("create root record directory {}", dir.display()))?;
    let path = dir.join(format!("{}.json", roots.producer));
    let bytes = serde_json::to_vec_pretty(&record).expect("root record serialize");
    let temporary = dir.join(format!(".{}.tmp", roots.producer));
    fs::write(&temporary, bytes)
        .with_context(|| format!("write root record {}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("publish root record {}", path.display()))?;
    sync_dir(&dir)?;
    Ok(record)
}

/// Read one producer's durable root record.
pub fn read_root_record(data_dir: &Path, producer: &str) -> Result<Option<RootRecord>> {
    let path = data_dir.join(CAS_ROOTS_DIR).join(format!("{producer}.json"));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(anyhow!("read root record {}: {error}", path.display()));
        }
    };
    let record: RootRecord = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!("decode root record {}: {error}", path.display()))?;
    ensure!(
        record.format == PRODUCER_RECORD_FORMAT,
        "CAS_ROOT_RECORD_INVALID: {} declares format {:?}",
        path.display(),
        record.format
    );
    ensure!(
        record.producer == producer,
        "CAS_ROOT_RECORD_INVALID: {} names producer {:?} instead of {producer}",
        path.display(),
        record.producer
    );
    Ok(Some(record))
}

/// A producer that re-reads its durable root record. `complete: false` (or a
/// missing record) makes reclamation refuse.
pub struct RecordedRoots {
    data_dir: PathBuf,
    producer: &'static str,
    namespaces: &'static [&'static str],
}

impl RecordedRoots {
    pub fn new(data_dir: PathBuf, producer: &'static str, namespaces: &'static [&'static str]) -> Self {
        Self {
            data_dir,
            producer,
            namespaces,
        }
    }
}

impl CasRootProducer for RecordedRoots {
    fn producer_id(&self) -> &'static str {
        self.producer
    }

    fn namespace_claims(&self) -> &'static [&'static str] {
        self.namespaces
    }

    fn enumerate(&self) -> Result<ProducerRoots> {
        let record = read_root_record(&self.data_dir, self.producer)?.ok_or_else(|| {
            anyhow!(
                "no durable root record for producer {} in {}",
                self.producer,
                self.data_dir.join(CAS_ROOTS_DIR).display()
            )
        })?;
        Ok(ProducerRoots {
            producer: record.producer,
            namespaces: record.namespaces,
            roots: record.roots,
            closures: record.closures,
            complete: record.complete,
            observed_at_unix_ms: record.observed_at_unix_ms,
            blockers: Vec::new(),
            held_exclusions: Vec::new(),
        })
    }
}
