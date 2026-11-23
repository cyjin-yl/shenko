//! Custody of the selected installation before an in-process Start.
//!
//! A desktop successor is an `exec`: the helper verifies the selected
//! executable, transfers the same locked directory description and the new
//! process re-proves the selection before it opens Store. Android has no
//! selected CLI executable — the node is a cdylib inside the foreground service
//! — so the successor generation starts in this same process, and no new
//! process boundary re-proves anything. The launcher pointer is a mutable
//! convenience (see `shenko_update::slots::resolve_live_slot`), not authority.
//!
//! Before the runtime starts, this module proves the process still holds the
//! four witnesses the start depends on:
//!
//! * **Store** — the opened handle is this installation's writer (its CAS root
//!   is the installation's shared content store), and the subject and
//!   installation identities are the ones the start path will use;
//! * **Stage** — the live slot, the slot descriptor, the activation selector and
//!   its sealed route name exactly one generation, and the route names this
//!   installation's slot;
//! * **schema** — every retained document (route authority, slot descriptor,
//!   Start document) is read by its declared major, and an unknown major is a
//!   refusal instead of a guess;
//! * **FrozenStart** — the retained Start inputs are still the exact document
//!   and the exact configuration bytes admitted for this installation.
//!
//! A missing, unreadable or contradictory witness refuses the start with the
//! typed code naming it. Nothing is repaired, substituted or re-derived from
//! the mutable pointer: refusing leaves the committed selector readable for an
//! operator or a later ordinary start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use shenko_seed::SubjectLaunchReceipt;
use shenko_store::Store;
use shenko_types::activation::{ActivationOrigin, ActivationSelector, RouteAuthority};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::start_inputs::FrozenStart;

/// One embedded (in-process) start.
///
/// `expected_selector_revision` is `Some` only for a successor start after a
/// committed selection: that start must serve exactly the revision the
/// hand-over committed, never a different selection that appeared meanwhile.
#[derive(Clone, Copy)]
pub(crate) struct EmbeddedStart<'a> {
    pub(crate) frozen: &'a FrozenStart,
    pub(crate) expected_selector_revision: Option<u64>,
}

/// The schema majors this start actually admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchemaWitness {
    pub(crate) route: u32,
    pub(crate) slot_manifest: Option<u32>,
    pub(crate) start_inputs: u32,
}

/// The installation-side witnesses, verified before home preparation, genesis,
/// identity adoption or any runtime service.
pub(crate) struct InstallationCustody {
    pub(crate) store: Arc<Store>,
    pub(crate) data_dir: PathBuf,
    pub(crate) live_slot: PathBuf,
    pub(crate) installation_id: EntityId,
    pub(crate) agent_id: EntityId,
    pub(crate) selector: ActivationSelector,
    pub(crate) route: RouteAuthority,
    pub(crate) schema: SchemaWitness,
    pub(crate) frozen: FrozenStart,
}

/// The complete custody retained until the runtime is running.
pub(crate) struct StartCustody {
    pub(crate) installation: InstallationCustody,
    pub(crate) launch: SubjectLaunchReceipt,
}

fn stage_refusal(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("START_CUSTODY_STAGE: {detail}")
}

fn store_refusal(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("START_CUSTODY_STORE: {detail}")
}

fn schema_refusal(detail: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("START_CUSTODY_SCHEMA: {detail}")
}

fn canonical(path: &Path, code: &str) -> Result<PathBuf> {
    path.canonicalize().map_err(|error| {
        if code == "STAGE" {
            stage_refusal(format!("resolve {}: {error}", path.display()))
        } else {
            store_refusal(format!("resolve {}: {error}", path.display()))
        }
    })
}

/// Verify the installation witnesses of one in-process start.
///
/// Returns the same `Store` handle it verified, so the caller cannot reopen a
/// different writer between the check and the runtime. `Ok(None)` is the one
/// real pre-selection state: a brand new installation before genesis, which
/// creates the selector inside its own transaction. A successor start
/// (`expected_selector_revision: Some`) never accepts that state.
pub(crate) fn verify_installation(
    store: Arc<Store>,
    data_dir: &Path,
    live_slot: &Path,
    embedded: EmbeddedStart<'_>,
) -> Result<(Arc<Store>, Option<InstallationCustody>)> {
    let frozen = embedded.frozen;

    // Frozen inputs first: without them there is no start request to verify the
    // rest against, and the retained document must still be the admitted one.
    let retained = crate::start_inputs::require(data_dir)?;
    ensure!(
        retained == *frozen,
        "START_CUSTODY_FROZEN_INPUTS: the retained Start request changed between admission and start"
    );

    // Store witness: the handle the runtime will use must be this
    // installation's writer, not a store copied in from elsewhere.
    let data_root = canonical(data_dir, "STORE")?;
    let cas_root = canonical(store.cas.base(), "STORE")?;
    ensure!(
        cas_root == data_root.join("cas"),
        "START_CUSTODY_STORE: the opened Store does not write this installation's content store"
    );
    let Some(agent_id) = store.get_existing_agent_id().map_err(|error| {
        store_refusal(format!("read the installation subject: {error}"))
    })? else {
        ensure!(
            embedded.expected_selector_revision.is_none(),
            "START_CUSTODY_STORE: an in-process successor start requires an initialized subject"
        );
        ensure!(
            shenko_update::slots::resolve_live_slot(data_dir)?.is_none(),
            "START_CUSTODY_STORE: the installation publishes a live slot but has no subject"
        );
        ensure!(
            canonical(live_slot, "STAGE")? == data_root,
            "START_CUSTODY_STAGE: this start resolved a slot the installation has not published"
        );
        return Ok((store, None));
    };
    let installation_id = store.installation_id_for_subject(agent_id).map_err(|error| {
        store_refusal(format!("read the subject's installation identity: {error}"))
    })?;

    // Stage witness: the selector is the authority; the slot descriptor and the
    // sealed route must agree with it and with the live slot.
    let selector = store.activation_selector(agent_id).map_err(|error| {
        stage_refusal(format!("read the activation selector: {error}"))
    })?.ok_or_else(|| {
        stage_refusal(format!(
            "subject {agent_id} has no activation selector; only genesis may create the first one"
        ))
    })?;
    if let Some(expected) = embedded.expected_selector_revision {
        ensure!(
            selector.selector_revision == expected,
            "START_CUSTODY_STAGE: the in-process successor expected selector revision {expected}, \
             the durable selector is {}",
            selector.selector_revision
        );
    }
    ensure!(
        selector.agent_id == agent_id,
        "START_CUSTODY_STAGE: the selector belongs to another subject"
    );
    let live_slot = canonical(live_slot, "STAGE")?;
    // Re-resolve the pointer instead of trusting the caller's value: a start
    // that raced a pointer publication must not serve the retired slot.
    let published_slot = shenko_update::slots::resolve_live_slot(data_dir)?;
    match &published_slot {
        Some(published) => ensure!(
            canonical(published, "STAGE")? == live_slot,
            "START_CUSTODY_STAGE: the live slot resolved by this start is not the published slot"
        ),
        None => ensure!(
            live_slot == data_root,
            "START_CUSTODY_STAGE: this start resolved a slot the installation has not published"
        ),
    }

    // Route authority: read the sealed object again, bound by content identity.
    let route_bytes = store.cas.read_object(&selector.route_authority_ref.digest).map_err(|error| {
        schema_refusal(format!("read the selected route authority: {error}"))
    })?;
    ensure!(
        route_bytes.len() as u64 == selector.route_authority_ref.bytes
            && Digest::blake3(&route_bytes) == selector.route_authority_ref.digest,
        "START_CUSTODY_SCHEMA: the selected route authority differs from its retained reference"
    );
    let route: RouteAuthority = serde_json::from_slice(&route_bytes)
        .map_err(|error| schema_refusal(format!("decode the selected route authority: {error}")))?;
    ensure!(
        route.schema_version == RouteAuthority::SCHEMA_VERSION,
        "START_CUSTODY_SCHEMA: unsupported route-authority schema {} (this build reads {})",
        route.schema_version,
        RouteAuthority::SCHEMA_VERSION
    );
    ensure!(
        route.agent_id == agent_id
            && route.generation == selector.generation
            && route.selector_revision == selector.selector_revision
            && route.active_epoch == selector.active_epoch,
        "START_CUSTODY_STAGE: the selected route does not describe this selector"
    );
    ensure!(
        canonical(Path::new(&route.data_dir), "STAGE")? == data_root,
        "START_CUSTODY_STAGE: the selected route belongs to another installation"
    );
    ensure!(
        canonical(Path::new(&route.slot_dir), "STAGE")? == live_slot,
        "START_CUSTODY_STAGE: the selected route names a different slot than the live slot"
    );
    verify_route_artifact(&route)?;

    // Slot descriptor: a post-activation slot publishes one. A pre-slot data
    // dir is its own slot and has none.
    let manifest_path = live_slot.join(shenko_update::slots::SLOT_MANIFEST_FILENAME);
    let slot_manifest = match std::fs::symlink_metadata(&manifest_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(stage_refusal(format!(
            "inspect the slot descriptor {}: {error}",
            manifest_path.display()
        ))),
        Ok(metadata) => {
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "START_CUSTODY_STAGE: the slot descriptor is not a literal regular file"
            );
            let manifest = shenko_update::slots::read_slot_manifest(&live_slot)
                .map_err(|error| stage_refusal(format!("read the slot descriptor: {error}")))?;
            ensure!(
                manifest.schema_version == shenko_update::slots::SlotManifest::SCHEMA_VERSION,
                "START_CUSTODY_SCHEMA: unsupported slot-descriptor schema {} (this build reads {})",
                manifest.schema_version,
                shenko_update::slots::SlotManifest::SCHEMA_VERSION
            );
            ensure!(
                manifest.generation == selector.generation.0,
                "START_CUSTODY_STAGE: the slot descriptor names generation {} but the selector names {}",
                manifest.generation,
                selector.generation.0
            );
            ensure!(
                canonical(Path::new(&manifest.cas_dir), "STAGE")? == cas_root,
                "START_CUSTODY_STAGE: the slot descriptor names another content store"
            );
            Some(manifest.schema_version)
        }
    };

    // The selector's origin must still be the real genesis or activation it
    // names; an unknown origin document is refused, never guessed.
    match &selector.origin {
        ActivationOrigin::Genesis { genesis_ref } => {
            let genesis = store.get_subject_genesis(agent_id).map_err(|error| {
                stage_refusal(format!("read the subject genesis: {error}"))
            })?.ok_or_else(|| stage_refusal(
                "the selector names a genesis this installation does not hold"
            ))?;
            ensure!(
                genesis.genesis_id == genesis_ref.genesis_id
                    && genesis.created_commit.ledger_id == genesis_ref.ledger_id,
                "START_CUSTODY_STAGE: the selector's genesis reference does not name this subject's genesis"
            );
        }
        ActivationOrigin::Activation { activation_id } => {
            let record = store.activation_record(*activation_id).map_err(|error| {
                stage_refusal(format!("read the selected activation record: {error}"))
            })?.ok_or_else(|| stage_refusal(
                "the selector names an activation this installation does not hold"
            ))?;
            ensure!(
                record.agent_id == agent_id && record.to_generation == selector.generation,
                "START_CUSTODY_STAGE: the selected activation record does not describe this generation"
            );
        }
    }

    let schema = SchemaWitness {
        route: route.schema_version,
        slot_manifest,
        start_inputs: crate::start_inputs::START_INPUTS_SCHEMA_VERSION,
    };
    Ok((store, Some(InstallationCustody {
        store,
        data_dir: data_root,
        live_slot,
        installation_id,
        agent_id,
        selector,
        route,
        schema,
        frozen: retained,
    })))
}

/// Whether the staged artifact named by the route is still a real file.
///
/// On Android the node's code is loaded in-process from the installed package,
/// so the route's artifact is the platform process the genesis launch observed
/// (`app_process`), not the app's APK. Reading that file's digest would compare
/// the platform binary, not the installed package, so the installed package
/// identity stays with the installed-package witness instead of being claimed
/// here. A desktop route names the executable this build was started from and
/// keeps the content comparison.
fn verify_route_artifact(route: &RouteAuthority) -> Result<()> {
    ensure!(
        !route.artifact_path.is_empty(),
        "START_CUSTODY_STAGE: the selected route names no artifact"
    );
    let path = Path::new(&route.artifact_path);
    ensure!(
        path.is_file(),
        "START_CUSTODY_STAGE: the selected route artifact {} is missing",
        path.display()
    );
    #[cfg(not(target_os = "android"))]
    {
        let bytes = std::fs::read(path).map_err(|error| {
            stage_refusal(format!("read the selected route artifact {}: {error}", path.display()))
        })?;
        ensure!(
            Digest::blake3(&bytes) == route.artifact_digest,
            "START_CUSTODY_STAGE: the selected route artifact changed after selection"
        );
    }
    Ok(())
}

/// Bind the durable launch to the retained custody.
///
/// `LaunchService::resolve` has already recorded the launch row; this proves the
/// row, the selector, the route, the live slot and the retained inputs still
/// describe the same generation after the setup writes that precede a launch.
pub(crate) fn verify_launch(
    store: &Arc<Store>,
    data_dir: &Path,
    custody: InstallationCustody,
    launch: &SubjectLaunchReceipt,
) -> Result<StartCustody> {
    ensure!(
        Arc::ptr_eq(&custody.store, store),
        "START_CUSTODY_STORE: the runtime Store handle differs from the witnessed handle"
    );
    ensure!(
        !launch.read_only,
        "START_CUSTODY_STAGE: an in-process start cannot be a read-only launch"
    );
    ensure!(
        launch.slot_dir == custody.live_slot
            && launch.generation == custody.selector.generation
            && launch.selector_revision == custody.selector.selector_revision
            && launch.selector_epoch == custody.selector.active_epoch
            && launch.origin == custody.selector.origin,
        "START_CUSTODY_STAGE: the durable launch does not bind the retained selector and slot"
    );
    let recorded = store.activation_launch(launch.launch_id).map_err(|error| {
        stage_refusal(format!("read the durable launch receipt: {error}"))
    })?;
    ensure!(
        recorded == Some(serde_json::to_value(launch)?),
        "START_CUSTODY_STAGE: the durable launch receipt differs from this launch"
    );
    // The whole witness is re-read now, not carried forward as a claim.
    let current = store.activation_selector(custody.agent_id).map_err(|error| {
        stage_refusal(format!("re-read the activation selector: {error}"))
    })?.ok_or_else(|| stage_refusal("the activation selector disappeared during start"))?;
    ensure!(
        current == custody.selector,
        "START_CUSTODY_STAGE: the activation selector changed during start"
    );
    ensure!(
        store.installation_id_for_subject(custody.agent_id).map_err(|error| {
            store_refusal(format!("re-read the subject's installation identity: {error}"))
        })? == custody.installation_id,
        "START_CUSTODY_STORE: the subject's installation identity changed during start"
    );
    let published = shenko_update::slots::resolve_live_slot(data_dir)?;
    let live_slot_unchanged = match &published {
        Some(slot) => canonical(slot, "STAGE").ok().as_deref() == Some(custody.live_slot.as_path()),
        None => custody.live_slot == custody.data_dir,
    };
    ensure!(
        live_slot_unchanged,
        "START_CUSTODY_STAGE: the live slot changed during start"
    );
    let route_bytes = store.cas.read_object(&custody.selector.route_authority_ref.digest).map_err(|error| {
        schema_refusal(format!("re-read the selected route authority: {error}"))
    })?;
    ensure!(
        Digest::blake3(&route_bytes) == custody.selector.route_authority_ref.digest
            && serde_json::from_slice::<RouteAuthority>(&route_bytes).ok().as_ref() == Some(&custody.route),
        "START_CUSTODY_SCHEMA: the selected route authority changed during start"
    );
    let retained = crate::start_inputs::require(data_dir)?;
    ensure!(
        retained == custody.frozen,
        "START_CUSTODY_FROZEN_INPUTS: the retained Start request changed during start"
    );
    tracing::debug!(
        route_schema = custody.schema.route,
        slot_manifest_schema = ?custody.schema.slot_manifest,
        start_inputs_schema = custody.schema.start_inputs,
        launch_id = %launch.launch_id,
        selector_revision = launch.selector_revision,
        "in-process start custody retained"
    );
    Ok(StartCustody { installation: custody, launch: launch.clone() })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A raw installation with no genesis yet is the only accepted
    /// pre-selection state. A successor start never accepts it.
    #[test]
    fn a_successor_start_without_a_selected_subject_is_refused() -> Result<()> {
        let root = std::env::temp_dir()
            .join(format!("shenko-inprocess-custody-{}", EntityId::new()));
        crate::runtime_bootstrap::create_data_directory(&root)?;
        let frozen = crate::start_inputs::admit(
            &root,
            crate::start_inputs::StartRequest::in_process(7860),
        )?;
        let store = Arc::new(shenko_store::Store::open(
            root.join(shenko_update::SLOT_DB_FILENAME),
            root.join("cas"),
        )?);
        let embedded = EmbeddedStart { frozen: &frozen, expected_selector_revision: Some(0) };
        let error = match verify_installation(Arc::clone(&store), &root, &root, embedded) {
            Ok(_) => panic!("a successor start without a selected subject must be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("START_CUSTODY_STORE"), "unexpected refusal: {error}");
        drop(store);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    /// The same installation before genesis is a real state for an ordinary
    /// in-process start; genesis creates the first selector inside its own
    /// transaction.
    #[test]
    fn an_ordinary_start_before_genesis_has_no_stage_witness_yet() -> Result<()> {
        let root = std::env::temp_dir()
            .join(format!("shenko-inprocess-genesis-{}", EntityId::new()));
        crate::runtime_bootstrap::create_data_directory(&root)?;
        let frozen = crate::start_inputs::admit(
            &root,
            crate::start_inputs::StartRequest::in_process(7860),
        )?;
        let store = Arc::new(shenko_store::Store::open(
            root.join(shenko_update::SLOT_DB_FILENAME),
            root.join("cas"),
        )?);
        let embedded = EmbeddedStart { frozen: &frozen, expected_selector_revision: None };
        let (returned, custody) = verify_installation(Arc::clone(&store), &root, &root, embedded)?;
        assert!(custody.is_none());
        assert_eq!(Arc::as_ptr(&returned), Arc::as_ptr(&store));
        drop(returned);
        drop(store);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}
