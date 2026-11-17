//! The workspace tool package's installation record and startup composition.
//!
//! Node owns its configuration, artifact identity and enable state. A process
//! takes one startup snapshot: a saved editor change cannot replace the
//! dialect underneath a child that already captured its tool catalog.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};
use serde_json::{Value, json};
use shenko_exec_host::workspace_tools::{
    EditorMode, WorkspaceTools, WorkspaceToolsConfig, implementation_digest,
};
use shenko_kernel::Supervisor;
use shenko_node::{
    InstalledUnitFact, Node, UNIT_SCHEMA, UnitDescriptor, UnitExport, UnitKind, UnitPackage,
    UnitRequest,
};
use shenko_store::Store;
use shenko_subject::NativeTaskHandlerPlugin;
use shenko_types::object::Digest;
use shenko_wasm::{HOME_GUEST_PATH, MountMode, MountSpec};

use crate::cli::ToolProfileCommand;

const UNIT_ID: &str = "tools.workspace";
const CONTRACT: &str = "shenko.workspace-tools/1";
const APPLY_NOTE: &str = "saved installation configuration is adopted at the next process start; an existing process keeps its startup snapshot";

fn definitions(editor: EditorMode) -> [&'static str; 4] {
    [
        "workspace.read",
        "workspace.search",
        "workspace.create",
        match editor {
            EditorMode::Replace => "workspace.edit.replace",
            EditorMode::Hashline => "workspace.edit.hashline",
        },
    ]
}

/// The manifest identifies the compiled package; configuration selects one of
/// its editor implementations without making a second installation registry.
fn manifest() -> Value {
    json!({
        "id": UNIT_ID,
        "contract": CONTRACT,
        "implementation_digest": implementation_digest(),
        "application": "restart_process",
        "default_editor": "replace",
        "profiles": {
            "replace": definitions(EditorMode::Replace),
            "hashline": definitions(EditorMode::Hashline),
        },
    })
}

fn import_record(node: &mut Node, config: Value, enabled: bool, package_root: &Path) -> Result<()> {
    let artifact = serde_json::to_vec(&manifest())?;
    let digest = Digest::blake3(&artifact);
    // A full hash fits Node's 64-byte revision bound. The source digest is
    // also recorded inside the manifest so the two identities are inspectable.
    let revision = digest.hex().to_owned();
    node.handle_unit_request(UnitRequest::Import {
        export: UnitExport {
            schema: UNIT_SCHEMA,
            root_unit_id: UNIT_ID.into(),
            root_revision: Some(revision.clone()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled,
            packages: vec![UnitPackage {
                unit: UnitDescriptor {
                    unit_id: UNIT_ID.into(),
                    kind: UnitKind::Code,
                    revision,
                    artifact_digest: digest,
                    // Package-level ability. The live native registry lists
                    // only the four definitions selected by this process.
                    provides: vec!["cap.workspace.files/1".into()],
                    dependencies: Vec::new(),
                    config,
                    applicability: None,
                },
                artifact_base64: B64_STD.encode(&artifact),
                evidence: Vec::new(),
                resolved_dependencies: None,
            }],
        },
        package_root: package_root.to_string_lossy().into_owned(),
        enable: Some(enabled),
    })
    .context("install workspace tool package")?;
    Ok(())
}

fn installed(node: &Node) -> Result<Option<InstalledUnitFact>> {
    let Some(record) = node.installed_unit_fact(UNIT_ID)? else {
        return Ok(None);
    };
    let bytes = node.unit_object_bytes(&record.artifact_digest)?;
    ensure!(
        record.kind == UnitKind::Code && Digest::blake3(&bytes) == record.artifact_digest,
        "WORKSPACE_PROFILE_INVALID: installed package kind or artifact differs"
    );
    let document: Value = serde_json::from_slice(&bytes)
        .context("WORKSPACE_PROFILE_INVALID: installed manifest is not JSON")?;
    ensure!(
        document["id"] == UNIT_ID && document["contract"] == CONTRACT,
        "WORKSPACE_PROFILE_INVALID: installed unit is not a compatible workspace tool package"
    );
    Ok(Some(record))
}

fn config_of(record: &InstalledUnitFact) -> Result<WorkspaceToolsConfig> {
    let config: WorkspaceToolsConfig = serde_json::from_value(record.descriptor.config.clone())
        .context("WORKSPACE_PROFILE_INVALID: invalid workspace tool configuration")?;
    ensure!(
        config.root.is_absolute(),
        "WORKSPACE_PROFILE_INVALID: workspace root must be an absolute host path"
    );
    Ok(config)
}

fn directory(path: &Path) -> Result<PathBuf> {
    let root = path
        .canonicalize()
        .with_context(|| format!("resolve workspace root {}", path.display()))?;
    ensure!(
        root.is_dir(),
        "workspace root is not a directory: {}",
        root.display()
    );
    Ok(root)
}

fn default_config(mounts: &[MountSpec]) -> Result<WorkspaceToolsConfig> {
    let mut homes = mounts
        .iter()
        .filter(|mount| mount.guest_path == HOME_GUEST_PATH);
    let home = homes.next().context(
        "WORKSPACE_PROFILE_UNAVAILABLE: no WASM /home mount; configure an explicit workspace root",
    )?;
    ensure!(
        homes.next().is_none() && home.mode == MountMode::ReadWrite && home.writable,
        "WORKSPACE_PROFILE_UNAVAILABLE: the default workspace needs one writable direct /home mount; configure an explicit host root for a COW home"
    );
    Ok(WorkspaceToolsConfig {
        root: directory(&home.source)?,
        editor: EditorMode::Replace,
    })
}

/// First use installs one default combination. A compiled package update
/// preserves the existing configuration and disabled state, including on an
/// installation created before this package existed.
fn reconcile(
    node: &mut Node,
    mounts: &[MountSpec],
    package_root: &Path,
) -> Result<InstalledUnitFact> {
    let current = installed(node)?;
    let expected = Digest::blake3(&serde_json::to_vec(&manifest())?);
    if current
        .as_ref()
        .is_none_or(|record| record.artifact_digest != expected)
    {
        let (config, enabled) = match current {
            Some(record) => (record.descriptor.config, record.enabled),
            None => (serde_json::to_value(default_config(mounts)?)?, true),
        };
        import_record(node, config, enabled, package_root)?;
    }
    installed(node)?.context("workspace tool installation record is absent after import")
}

/// The live startup and capability-discovery paths use this same composition.
/// NativeTaskHandlerPlugin owns each handler's registration and teardown.
pub(crate) async fn install(
    supervisor: &Supervisor,
    store: &Arc<Store>,
    data_dir: &Path,
    mounts: &[MountSpec],
) -> Result<()> {
    let record = {
        let mut node = crate::node_sync::open_node(data_dir, store)?;
        reconcile(&mut node, mounts, &data_dir.join("workspace-tool-units"))?
    };
    if record.enabled {
        let tools = WorkspaceTools::new(Arc::clone(store), config_of(&record)?)?;
        for handler in tools.handlers() {
            supervisor.spawn(NativeTaskHandlerPlugin, handler).await?;
        }
    }
    Ok(())
}

fn report(node: Option<&Node>) -> Result<Value> {
    let record = node.map(installed).transpose()?.flatten();
    let compiled = manifest();
    let digest = Digest::blake3(&serde_json::to_vec(&compiled)?);
    let (configured, config_error) = match record.as_ref().map(config_of).transpose() {
        Ok(Some(config)) => (json!(definitions(config.editor)), None),
        Ok(None) => (Value::Null, None),
        Err(error) => (Value::Null, Some(format!("{error:#}"))),
    };
    Ok(json!({
        "unit_id": UNIT_ID,
        "installed": record.is_some(),
        "enabled": record.as_ref().is_some_and(|record| record.enabled),
        "compiled_manifest": compiled,
        "compiled_artifact_digest": digest,
        "compiled_identity_matches": record.as_ref().is_some_and(|record| record.artifact_digest == digest),
        "installation": record,
        "configured_definitions": configured,
        "configuration_error": config_error,
        "runtime_observed": false,
        "application": "restart_process",
        "note": APPLY_NOTE,
    }))
}

fn initial_config(data_dir: &Path, store: &Store) -> Result<WorkspaceToolsConfig> {
    // Reuse the real read-only home admission, including mount/source checks;
    // a copied manifest must not silently select another installation's files.
    let home = shenko_seed::bootstrap::validate_home_for_exec(data_dir)?;
    ensure!(
        home.installation_id == store.installation_binding()?.ledger_id,
        "INSTALLATION_IDENTITY_MISMATCH: home belongs to another installation"
    );
    default_config(&home.mounts)
}

pub(crate) fn command(data_dir: &Path, command: ToolProfileCommand) -> Result<()> {
    if matches!(command, ToolProfileCommand::Inspect) {
        // This query does not initialize a Node, prepare a home or open a
        // writable Store. Absence stays absence.
        let node = Node::open_inspect(&data_dir.join("node"))?;
        println!("{}", serde_json::to_string_pretty(&report(node.as_ref())?)?);
        return Ok(());
    }
    let live_slot = shenko_update::slots::resolve_live_slot(data_dir)?
        .unwrap_or_else(|| data_dir.to_path_buf());
    ensure!(
        live_slot.join(shenko_update::SLOT_DB_FILENAME).is_file(),
        "no managed installation at {}; run `shenko start` once",
        data_dir.display()
    );
    let store = crate::runtime_bootstrap::open_installation_store(data_dir)?;
    let mut node = crate::node_sync::open_node(data_dir, &store)?;
    let current = installed(&node)?;
    let package_root = data_dir.join("workspace-tool-units");
    match command {
        ToolProfileCommand::Inspect => unreachable!(),
        ToolProfileCommand::Configure { editor, root } => {
            let root = match root {
                Some(path) => directory(&path)?,
                None => match &current {
                    Some(record) => directory(&config_of(record)?.root)?,
                    None => initial_config(data_dir, &store)?.root,
                },
            };
            let config = WorkspaceToolsConfig {
                root,
                editor: serde_json::from_value(json!(editor))?,
            };
            import_record(
                &mut node,
                serde_json::to_value(config)?,
                current.as_ref().is_none_or(|record| record.enabled),
                &package_root,
            )?;
        }
        ToolProfileCommand::Enable | ToolProfileCommand::Disable => {
            let enabled = matches!(command, ToolProfileCommand::Enable);
            let config = match current {
                Some(record) => {
                    if enabled {
                        // Refuse an unusable root before enabling; startup
                        // still revalidates the actual directory later.
                        directory(&config_of(&record)?.root)?;
                    }
                    record.descriptor.config
                }
                None => serde_json::to_value(initial_config(data_dir, &store)?)?,
            };
            import_record(&mut node, config, enabled, &package_root)?;
        }
    }
    println!("{}", serde_json::to_string_pretty(&report(Some(&node))?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_subject::{NativeChildPlugin, NativeChildService};
    use shenko_types::ids::{EntityId, RuntimeGeneration};

    async fn runtime(store: &Arc<Store>) -> Result<Supervisor> {
        let supervisor = Supervisor::new(RuntimeGeneration::default());
        supervisor.publish_services().await?;
        supervisor
            .spawn(
                NativeChildPlugin,
                (Arc::clone(store), RuntimeGeneration::default()),
            )
            .await?;
        Ok(supervisor)
    }

    fn kinds(supervisor: &Supervisor) -> Result<Vec<String>> {
        Ok(supervisor
            .context()
            .try_service::<NativeChildService>()?
            .installed_definitions()
            .into_iter()
            .map(|(kind, _)| kind)
            .collect())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_changes_are_persisted_but_only_adopted_by_the_next_runtime() -> Result<()> {
        let dir =
            std::env::temp_dir().join(format!("shenko-workspace-profile-{}", EntityId::new()));
        command(&dir, ToolProfileCommand::Inspect)?;
        assert!(!dir.exists(), "inspection must not create an installation");
        std::fs::create_dir_all(dir.join("home"))?;
        let store = Arc::new(Store::open(dir.join("shenko.db"), dir.join("cas"))?);
        let mounts = vec![MountSpec::new(
            dir.join("home"),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )];
        let first = runtime(&store).await?;
        install(&first, &store, &dir, &mounts).await?;
        let original = kinds(&first)?;
        assert_eq!(original.len(), 4);
        assert!(original.contains(&"workspace.edit.replace".into()));

        command(
            &dir,
            ToolProfileCommand::Configure {
                editor: "hashline".into(),
                root: None,
            },
        )?;
        assert_eq!(
            kinds(&first)?,
            original,
            "a saved profile must not change a running catalog"
        );
        first.stop().await?;
        let second = runtime(&store).await?;
        install(&second, &store, &dir, &mounts).await?;
        let selected = kinds(&second)?;
        assert_eq!(selected.len(), 4);
        assert!(selected.contains(&"workspace.edit.hashline".into()));
        assert!(!selected.contains(&"workspace.edit.replace".into()));

        command(&dir, ToolProfileCommand::Disable)?;
        assert_eq!(kinds(&second)?, selected);
        second.stop().await?;
        let third = runtime(&store).await?;
        install(&third, &store, &dir, &mounts).await?;
        assert!(
            kinds(&third)?.is_empty(),
            "startup must preserve an explicitly disabled unit"
        );
        third.stop().await?;
        drop(store);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }
}
