//! Installation storage, private data roots, identity and immutable genesis bootstrap.
use std::path::Path;

use anyhow::{Context, Result};
use shenko_identity::IdentityService;
use shenko_types::identity::IdentityProfile;
use tracing::info;

/// Make sure the agent has an identity revision, and apply a user-given
/// `--name` only for initialization. Existing edits require an explicit CAS., R117.
pub(crate) fn adopt_identity(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    name: Option<&str>,
    identity: &IdentityService,
) -> Result<IdentityProfile> {
    let genesis = identity.genesis(agent_id, name);
    let current = store.initialize_identity(&genesis)?;
    anyhow::ensure!(
        name.is_none() || current.display_name == genesis.display_name,
        "IDENTITY_INITIAL_NAME_CONFLICT: start --name only initializes; use identity --expected-revision {} to edit",
        current.revision
    );
    Ok(current)
}


/// Resolve the database through the same installation route for every CLI.
/// Checking the retired root database is not evidence that the selected slot
/// exists, and opening that root after a handoff would write an obsolete copy.
pub(crate) fn installation_db_path(data_dir: &Path) -> Result<std::path::PathBuf> {
    let live_slot = shenko_update::slots::resolve_live_slot(data_dir)?
        .unwrap_or_else(|| data_dir.to_path_buf());
    Ok(live_slot.join(shenko_update::SLOT_DB_FILENAME))
}

/// Open the store of the selected live slot. A data dir that has never
/// published a slot is itself the live slot (first run).
pub(crate) fn open_installation_store(data_dir: &Path) -> Result<shenko_store::store::Store> {
    create_data_directory(data_dir)?;
    Ok(shenko_store::store::Store::open(
        installation_db_path(data_dir)?,
        // The content store is shared by every generation of an
        // installation and always lives at <data_dir>/cas.
        data_dir.join("cas"),
    )?)
}

// Specify access when creating each new directory, not by chmod after it is
// exposed. Inherited default ACLs otherwise bypass the process umask. Existing
// directories and ancestors are untouched; owner-control still admits them.
pub(crate) fn create_data_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

pub(crate) async fn create_data_directory_async(path: &Path) -> std::io::Result<()> {
    let mut builder = tokio::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(path).await
}

#[cfg(all(test, unix))]
mod data_directory_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cli_mutations_follow_the_selected_slot_and_keep_the_shared_content_store() -> Result<()> {
        use shenko_store::Store;
        use shenko_types::store::WriteBatch;
        let root = std::env::temp_dir().join(format!(
            "shenko-cli-live-slot-{}", shenko_types::ids::EntityId::new()
        ));
        create_data_directory(&root)?;
        let retired = open_installation_store(&root)?;
        let retired_id = retired.ledger.ledger_id();
        let selected = root.join("versions").join("selected");
        create_data_directory(&selected)?;
        let candidate = Store::open(selected.join("shenko.db"), root.join("cas"))?;
        let selected_id = candidate.ledger.ledger_id();
        assert_ne!(selected_id, retired_id);
        drop(candidate);
        shenko_update::slots::publish_current_slot(&root, &selected)?;
        let opened = open_installation_store(&root)?;
        let receipt = opened.ledger.commit(WriteBatch::new())?;
        assert_eq!(receipt.commit_ref.ledger_id, selected_id);
        assert_eq!(installation_db_path(&root)?, selected.canonicalize()?.join("shenko.db"));
        drop(opened);
        drop(retired);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn new_data_roots_are_private_without_chmod_of_existing_directories() -> Result<()> {
        let parent = std::env::temp_dir().join(format!(
            "shenko-private-data-{}", shenko_types::ids::EntityId::new()
        ));
        std::fs::create_dir(&parent)?;
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o770))?;
        for asynchronous in [false, true] {
            let path = parent.join(if asynchronous { "async" } else { "sync" }).join("data");
            if asynchronous { create_data_directory_async(&path).await?; }
            else { create_data_directory(&path)?; }
            for new_directory in [path.as_path(), path.parent().unwrap()] {
                assert_eq!(new_directory.metadata()?.permissions().mode() & 0o777, 0o700);
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o770))?;
            if asynchronous { create_data_directory_async(&path).await?; }
            else { create_data_directory(&path)?; }
            assert_eq!(path.metadata()?.permissions().mode() & 0o777, 0o770);
        }
        assert_eq!(parent.metadata()?.permissions().mode() & 0o777, 0o770);
        Ok(())
    }
}

/// Ensure genesis exists for this installation. Returns agent_id.
/// New geneses bind the real prepared home. Existing geneses are immutable and
/// are never replaced merely to upgrade their manifest representation.
pub(crate) fn bootstrap_genesis(
    store: &shenko_store::store::Store,
    name: Option<&str>,
    home: &shenko_seed::HomeManifest,
    model_profile: &shenko_seed::ModelProfile,
) -> Result<shenko_types::ids::EntityId> {
    use shenko_types::commit::CommitRef;
    use shenko_types::ids::*;
    use shenko_types::object::ObjectRef;
    use shenko_types::subject::*;

    if let Some(agent_id) = store.get_existing_agent_id()? {
        let genesis = store
            .get_subject_genesis(agent_id)?
            .ok_or_else(|| anyhow::anyhow!("existing subject has no genesis"))?;
        let read_json = |object: &ObjectRef, label: &str| -> Result<serde_json::Value> {
            let path = store.cas.verified_object_path(object)?;
            let bytes = std::fs::read(&path)?;
            serde_json::from_slice(&bytes).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: decode {label}: {error}")
            })
        };
        let seed = read_json(&genesis.seed_manifest_ref, "seed manifest")?;
        read_json(&genesis.initial_profile_ref, "initial model profile")?;
        read_json(&genesis.bootstrap_event_ref, "bootstrap event")?;
        if let Some(value) = seed.get("home_manifest_ref") {
            let nested: ObjectRef = serde_json::from_value(value.clone()).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: seed home reference: {error}")
            })?;
            anyhow::ensure!(
                nested.digest == genesis.home_manifest_ref.digest,
                "SEED_CLOSURE_INCOMPLETE: seed/genesis home references differ; data preserved"
            );
        }
        if let Some(value) = seed.get("home_probe_ref") {
            let probe: ObjectRef = serde_json::from_value(value.clone()).map_err(|error| {
                anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: seed guest reference: {error}")
            })?;
            store.cas.verified_object_path(&probe)?;
        }

        let home_path = store.cas.verified_object_path(&genesis.home_manifest_ref)?;
        let bytes = std::fs::read(home_path)?;
        match serde_json::from_slice::<shenko_seed::HomeManifest>(&bytes) {
            Ok(stored) => {
                if stored != *home {
                    let origin = store.cognition_subject_origin(agent_id)?;
                    anyhow::ensure!(origin.as_ref().is_some_and(|origin|
                        origin.genesis_id == genesis.genesis_id
                        && origin.source_ledger_id == genesis.created_commit.ledger_id),
                        "DATA_PATH_CONFLICT: genesis and current home manifests differ without verified replication provenance; data preserved");
                    anyhow::ensure!(stored.schema_version == home.schema_version
                        && stored.runtime == home.runtime && stored.wasi == home.wasi
                        && stored.checkpoint == home.checkpoint,
                        "WASM_PROFILE_UNAVAILABLE: replicated seed contract differs from this node's verified local home; no runtime success claimed");
                    info!(agent_id=%agent_id,
                        "replicated immutable genesis retained; using this node's independently prepared local home, not foreign mount paths");
                }
            }
            Err(error) => {
                // Before C05 the genesis contained only a descriptive placeholder
                // and no production runtime used it. Recognize exactly that shape;
                // arbitrary corrupt closures still stop with their data intact.
                let legacy: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| {
                    anyhow::anyhow!("SEED_CLOSURE_INCOMPLETE: decode genesis home: {error}")
                })?;
                let legacy_placeholder = legacy.get("wasm").and_then(|v| v.as_bool()) == Some(true)
                    && legacy.get("home").and_then(|v| v.as_str()).is_some()
                    && legacy
                        .get("mounts")
                        .and_then(|v| v.as_array())
                        .is_some_and(|mounts| mounts.is_empty());
                anyhow::ensure!(
                    legacy_placeholder,
                    "SEED_CLOSURE_INCOMPLETE: unrecognized genesis home manifest; data preserved"
                );
                tracing::warn!(
                    agent_id=%agent_id,
                    "legacy genesis home placeholder retained; current prepared home is not a new subject"
                );
            }
        }
        info!(agent_id=%agent_id, "existing genesis found — resuming");
        return Ok(agent_id);
    }

    let agent_id = EntityId::new();
    let bootstrap_id = EntityId::new();
    let launch_id = EntityId::new();
    let genesis_id = EntityId::new();
    let ledger_id = store.ledger.ledger_id();

    let seal_json = |value: &serde_json::Value| -> Result<ObjectRef> {
        let bytes = serde_json::to_vec(value)?;
        Ok(store
            .cas
            .seal_object(&bytes, "application/json", "seed", "json", 1)?
            .object)
    };

    // The home object and actual WASI probe are sealed before a manifest may
    // reference them. This is a bootstrap-home closure, not a claim that every
    // product plugin or VM live state has been captured.
    let home_manifest = seal_json(&serde_json::to_value(home)?)?;
    let home_probe = store
        .cas
        .seal_object(
            shenko_seed::HOME_PROBE_WAT.as_bytes(),
            "application/wat",
            "seed",
            "home_probe",
            1,
        )?
        .object;
    let seed_manifest = seal_json(&serde_json::json!({
        "seed_version": env!("CARGO_PKG_VERSION"),
        "platform": std::env::consts::OS,
        "created": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "spec_version": "0.6.2",
        "runtime": home.runtime,
        "runtime_generation": home.runtime_generation,
        "home_manifest_ref": home_manifest.clone(),
        "home_probe_ref": home_probe,
        "closure_level": "bootstrap_home_only",
        "complete_product_seed": false,
    }))?;
    let initial_profile = seal_json(&serde_json::to_value(model_profile)?)?;
    let bootstrap_event = seal_json(&serde_json::json!({
        "event": "bootstrap",
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "kind": "genesis",
        "installation_id": home.installation_id,
        "seed_manifest_ref": seed_manifest.clone(),
        "home_manifest_ref": home_manifest.clone(),
    }))?;

    let genesis = SubjectGenesis {
        genesis_id,
        subject_id: agent_id,
        bootstrap_id,
        launch_id,
        seed_manifest_ref: seed_manifest.clone(),
        initial_profile_ref: initial_profile,
        home_manifest_ref: home_manifest,
        bootstrap_event_ref: bootstrap_event,
        created_commit: CommitRef {
            ledger_id,
            seq: LedgerSeq(0),
        },
    };

    let first_step_id = EntityId::new();
    let head = SubjectBranchHead { branch_origin: None,
        agent_id,
        branch_id: BranchId::new(),
        subject_root_task_id: EntityId::new(),
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        genesis_ref: GenesisRef {
            ledger_id,
            genesis_id,
        },
        committed_step_no: 0,
        committed_step_id: None,
        committed_at: CommitRef {
            ledger_id,
            seq: LedgerSeq(0),
        },
        next_step_id: first_step_id,
        mode: SubjectMode::Active,
        mode_reason_ref: None,
        record_revision: RecordRevision(0),
    };

    let first_step = shenko_types::subject::SubjectStep {
        step_id: first_step_id,
        branch_id: head.branch_id.clone(),
        step_no: 1,
        expected_parent_commit: genesis.created_commit,
        controller_epoch: ControllerEpoch(1),
        software_generation: RuntimeGeneration::default(),
        state: shenko_types::subject::SubjectStepState::Planned,
        input_plan_ref: None,
        context_revision: None,
        active_request_id: None,
        active_attempt_id: None,
        model_result_ref: None,
        model_outcome_ref: None,
        decision_ref: None,
        commit_ref: None,
        retry_reason_ref: None,
        record_revision: RecordRevision(0),
        created_by_step_id: None,
    };

    // The initial control authority — X05. The first selector
    // is created with the genesis inside the same transaction; a launch
    // never invents one, and a generation swap is the only other writer.
    use shenko_types::activation::{ActivationOrigin, ActivationSelector, RouteAuthority};
    let genesis_ref = GenesisRef {
        ledger_id,
        genesis_id,
    };
    let state_ref = seal_json(&serde_json::json!({
        "schema_version": 1,
        "kind": "activation_state_ref",
        "slot": "data-dir",
        "db": "shenko.db",
        "controller_epoch": 1,
        "generation": head.software_generation,
    }))?;
    let executable = std::env::current_exe()?;
    let data_dir = home.home_dir().parent().context("seed home has no installation directory")?;
    anyhow::ensure!(home.memory_dir().parent() == Some(data_dir), "seed managed roots disagree on installation directory");
    let route_ref = seal_json(&serde_json::to_value(RouteAuthority {
        schema_version: RouteAuthority::SCHEMA_VERSION,
        agent_id, generation: head.software_generation.clone(), active_epoch: 1, selector_revision: 0,
        data_dir: data_dir.to_string_lossy().into_owned(),
        slot_dir: data_dir.to_string_lossy().into_owned(),
        artifact_path: executable.to_string_lossy().into_owned(),
        artifact_digest: shenko_types::object::Digest::blake3(&std::fs::read(&executable)?),
        launch_command: vec![executable.to_string_lossy().into_owned(), "--data-dir".into(),
            data_dir.to_string_lossy().into_owned(), "start".into()],
        primary_claim: None, activation_id: None,
    })?)?;
    let selector = ActivationSelector {
        agent_id,
        selector_revision: 0,
        active_epoch: 1,
        generation: head.software_generation.clone(),
        state_ref,
        origin: ActivationOrigin::Genesis {
            genesis_ref: genesis_ref.clone(),
        },
        route_authority_ref: route_ref,
    };
    let root_task = shenko_types::task::TaskRecord::subject_root(
        head.subject_root_task_id,
        agent_id,
        seed_manifest,
        genesis.created_commit,
    );
    let receipt = store.initialize_genesis_with(
        &genesis, &head, &first_step, &root_task, Some(&selector),
    )?;
    info!(agent_id=%agent_id, name=name.unwrap_or("(unnamed)"), commit=%receipt.commit_ref, "genesis created");

    Ok(agent_id)
}
