//! `shenko compat` — install, inspect and switch the optional ecosystem
//! adapters. MCP and Agent Skills are separate units with separate plugins, so
//! one can be installed, enabled, upgraded or removed without the other.
//!
//! Nothing here bundles a vendor server, ships a vendor Skill, authorizes an
//! account or runs a third-party script. Import copies exactly the material the
//! operator selected; enabling only makes the adapter available to the subject.

use std::path::Path;

use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};
use shenko_compat::installation::{plugin_export, plugin_package};
#[cfg(feature = "compat-mcp")]
use shenko_compat_mcp::{McpConnection, UNIT_ID as MCP_UNIT};
#[cfg(feature = "compat-skill")]
use shenko_compat_skill::{InstalledSource, SkillSource, SourceInput, SourceQuery, MAX_READ_BYTES, UNIT_ID as SKILL_UNIT, metadata};
use shenko_node::{Node, UnitRequest};

use crate::CompatCommand;
#[cfg(feature = "compat-mcp")]
use crate::CompatMcpCommand;
#[cfg(feature = "compat-skill")]
use crate::CompatSkillCommand;

/// One compatibility plugin the product can offer. Both are optional: the
/// product ships as neither, and installing one never installs the other.
fn plugins() -> impl Iterator<Item = (&'static str, &'static str)> {
    const PLUGINS: &[(&str, &str)] = &[
        #[cfg(feature = "compat-mcp")]
        (MCP_UNIT, "external MCP protocol client; no bundled server"),
        #[cfg(feature = "compat-skill")]
        (SKILL_UNIT, "Agent Skills source reader; no bundled Skill, no execution"),
    ];
    PLUGINS.iter().copied()
}

fn store_of(data_dir: &Path) -> Result<std::sync::Arc<shenko_store::store::Store>> {
    let db_path = data_dir.join("shenko.db");
    ensure!(
        db_path.is_file(),
        "no managed installation at {}; run `shenko start` once",
        data_dir.display()
    );
    Ok(std::sync::Arc::new(shenko_store::store::Store::open(
        db_path,
        data_dir.join("cas"),
    )?))
}

/// The Node that owns installation facts.
///
/// Installing an adapter is ordinary provisioning, so it must work before the
/// subject's first start: there is no subject binding to enforce yet, and
/// creating the node record is the same initialization `shenko sync` performs.
/// Once a subject exists, the real binding check applies — an adapter import
/// can never attach this node to a different subject.
fn node_of(data_dir: &Path) -> Result<Node> {
    if data_dir.join("shenko.db").is_file() {
        let store = store_of(data_dir)?;
        return crate::node_sync::open_node(data_dir, &store);
    }
    let mut node = Node::open(&data_dir.join("node"))?;
    if node.local_node_id()?.is_none() {
        node.handle(shenko_node::Request::Initialize {
            node_id: shenko_types::ids::EntityId::new(),
            identity: None,
        })?;
    }
    Ok(node)
}

/// Ensure exactly one plugin record exists, disabled.
///
/// Only the adapter an import actually needs is recorded: importing a Skill
/// never installs the MCP record, and importing a connection never installs
/// the Skill record. Enabling stays a separate, explicit action.
fn ensure_plugin(node: &mut Node, root: &Path, unit_id: &str) -> Result<()> {
    let current = plugin_package(unit_id)?.unit;
    if node
        .installed_unit_fact(unit_id)?
        .is_some_and(|fact| fact.revision == current.revision && fact.artifact_digest == current.artifact_digest)
    {
        return Ok(());
    }
    node.handle_unit_request(UnitRequest::Import {
        export: plugin_export(unit_id, false)?,
        package_root: root.to_string_lossy().into_owned(),
        enable: Some(false),
    })
    .with_context(|| format!("install compatibility plugin record {unit_id}"))?;
    Ok(())
}

fn set_enabled(data_dir: &Path, unit_id: &str, enabled: bool) -> Result<()> {
    let mut node = node_of(data_dir)?;
    let root = data_dir.join("compat-units");
    if plugins().any(|(known, _)| known == unit_id) {
        ensure_plugin(&mut node, &root, unit_id)?;
    }
    let outcome = node.handle_unit_request(UnitRequest::SetEnabled {
        unit_id: unit_id.to_string(),
        enabled,
    })?;
    println!("{}", serde_json::to_string_pretty(&outcome)?);
    Ok(())
}

pub async fn command(data_dir: &Path, command: CompatCommand) -> Result<()> {
    match command {
        CompatCommand::Status => status(data_dir),
        CompatCommand::Enable { unit_id } => set_enabled(data_dir, &unit_id, true),
        CompatCommand::Disable { unit_id } => set_enabled(data_dir, &unit_id, false),
        #[cfg(feature = "compat-mcp")]
        CompatCommand::Mcp { command } => mcp(data_dir, command).await,
        #[cfg(feature = "compat-skill")]
        CompatCommand::Skill { command } => skill(data_dir, command),
    }
}

/// Report what is installed and, separately, what is offered.
fn status(data_dir: &Path) -> Result<()> {
    let description = plugins()
        .map(|(unit_id, purpose)| json!({"unit_id": unit_id, "purpose": purpose}))
        .collect::<Vec<_>>();
    let fact = |node: &Node, unit_id: &str| -> Result<Value> {
        Ok(match node.installed_unit_fact(unit_id)? {
            None => json!({"installed": false, "enabled": false}),
            Some(fact) => json!({
                "installed": true, "enabled": fact.enabled, "revision": fact.revision,
                "artifact_digest": fact.artifact_digest.0, "current": fact.current,
            }),
        })
    };
    match Node::open_inspect(&data_dir.join("node"))? {
        Some(node) => {
            let mut plugins_report = serde_json::Map::new();
            for (unit_id, _) in plugins() {
                plugins_report.insert(unit_id.into(), fact(&node, unit_id)?);
            }
            // External connections and Skill sources are ordinary units; the
            // report names only the ones that declare a compat format.
            let mut externals = Vec::new();
            for fact in node.installed_unit_facts()? {
                if fact.unit_id.starts_with("compat.") {
                    continue;
                }
                let format = fact.descriptor.config["compat"]["format"].as_str();
                if !matches!(format, Some("mcp-connection") | Some("agent-skill")) {
                    continue;
                }
                externals.push(json!({
                    "unit_id": fact.unit_id,
                    "format": format,
                    "title": fact.descriptor.config["cognition"]["title"],
                    "description": fact.descriptor.config["cognition"]["capability_card"],
                    "provenance_refs": fact.descriptor.config["cognition"]["provenance_refs"],
                    "revision": fact.revision,
                    "artifact_digest": fact.artifact_digest.0,
                    "enabled": fact.enabled,
                }));
            }
            let plugins_value = Value::Object(plugins_report);
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "available": true,
                    "plugins": plugins_value,
                    "installed_external": externals,
                    "external_services_bundled": false,
                    "note": "Both adapters are optional and independent. Installing a connection or Skill never installs a third-party service, account or dependency.",
                }))?
            );
            Ok(())
        }
        None => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "available": false,
                    "reason": "no node installation",
                    "installable": description,
                    "external_services_bundled": false,
                }))?
            );
            Ok(())
        }
    }
}

#[cfg(feature = "compat-mcp")]
async fn mcp(data_dir: &Path, command: CompatMcpCommand) -> Result<()> {
    match command {
        CompatMcpCommand::Import {
            file,
            unit_id,
            enabled,
        } => {
            let bytes = std::fs::read(&file)
                .with_context(|| format!("read MCP connection {}", file.display()))?;
            let connection: McpConnection = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse MCP connection {}", file.display()))?;
            let mut node = node_of(data_dir)?;
            let root = data_dir.join("compat-units");
            ensure_plugin(&mut node, &root, MCP_UNIT)?;
            let export = connection.export(&unit_id, false)?;
            let revision = export.root_revision.clone().unwrap_or_default();
            let artifact_digest = export.root_artifact_digest.clone();
            let imported = node.handle_unit_request(UnitRequest::Import {
                export,
                package_root: root.to_string_lossy().into_owned(),
                // Import never enables an adapter on the subject's behalf.
                enable: Some(false),
            })?;
            if enabled {
                node.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: unit_id.clone(),
                    enabled: true,
                })?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "unit_id": unit_id,
                    "revision": revision,
                    "artifact_digest": artifact_digest,
                    "imported": imported,
                    "enabled": node.installed_unit_fact(&unit_id)?.context("imported MCP unit missing")?.enabled,
                    "connection_attempted": false,
                    "note": "Importing describes an endpoint. No process was started and no credential was read.",
                }))?
            );
            Ok(())
        }
    }
}

/// Read one selected directory as exact Skill source material.
///
/// Pin root ancestors and descend from open directory handles. A concurrent
/// rename cannot redirect a later read through an ancestor symlink. This is
/// confinement, not an atomic snapshot of a concurrently edited source tree.
#[cfg(feature = "compat-skill")]
struct CollectedSource {
    root: cap_std::fs::Dir,
    files: std::collections::BTreeMap<String, (u64, bool)>,
    entry: Vec<u8>,
    provenance_refs: Vec<String>,
}

#[cfg(feature = "compat-skill")]
impl CollectedSource {
    fn open(&self, path: &str) -> Result<SourceInput<cap_std::fs::File>> {
        use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
        let mut directory = self.root.try_clone()?;
        let mut components = path.split('/').peekable();
        while let Some(name) = components.next() {
            if components.peek().is_some() {
                directory = directory.open_dir_nofollow(name)?;
                continue;
            }
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No);
            #[cfg(unix)]
            {
                use cap_std::fs::OpenOptionsExt as _;
                options.custom_flags(libc::O_NONBLOCK);
            }
            let reader = directory.open_with(name, &options)?;
            let metadata = reader.metadata()?;
            ensure!(metadata.is_file(), "Skill source member is no longer a regular file: {path}");
            let (bytes, executable) = *self.files.get(path).context("member is not in the frozen inventory")?;
            ensure!(metadata.len() == bytes, "Skill source member changed length: {path}");
            #[cfg(unix)]
            {
                use cap_std::fs::PermissionsExt as _;
                ensure!((metadata.permissions().mode() & 0o111 != 0) == executable, "Skill source member changed executable mode: {path}");
            }
            return Ok(SourceInput { path: path.into(), executable, bytes, reader });
        }
        anyhow::bail!("empty source member path")
    }

    fn capture(&self, cas: &shenko_store::cas::ContentStore) -> Result<SkillSource> {
        let source = SkillSource::capture(cas, self.provenance_refs.clone(), self.files.keys().map(|path| self.open(path)))?;
        ensure!(source.entry == self.entry, "SKILL.md changed during capture");
        Ok(source)
    }
}

#[cfg(feature = "compat-skill")]
fn collect_source(root: &Path) -> Result<CollectedSource> {
    use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
    use cap_std::fs::{Dir, OpenOptions};
    use std::{io::Read as _, path::{Component, PathBuf}};

    // Only the filesystem anchor is ambient. Every caller-supplied component,
    // including the root's ancestors, is opened without following symlinks.
    // Keep the stack so `a/../b` follows the opened parent, not a new lookup.
    let absolute = std::path::absolute(root)?;
    let mut components = absolute.components().peekable();
    let mut anchor = PathBuf::new();
    while matches!(components.peek(), Some(Component::Prefix(_) | Component::RootDir)) {
        anchor.push(components.next().expect("peeked component").as_os_str());
    }
    ensure!(anchor.has_root(), "Skill source has no filesystem root");
    let mut parents = vec![Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?];
    for component in components {
        match component {
            Component::Normal(name) => {
                let directory = parents.last().expect("root handle")
                    .open_dir_nofollow(name)
                    .with_context(|| format!("open Skill source directory {} without symbolic links", name.to_string_lossy()))?;
                parents.push(directory);
            }
            Component::ParentDir if parents.len() > 1 => { parents.pop(); }
            Component::ParentDir | Component::CurDir => {}
            _ => anyhow::bail!("unexpected Skill source root component"),
        }
    }
    let directory = parents.pop().expect("root handle");
    let source_root = directory.try_clone()?;
    let entries = directory.entries()?;
    // An explicit stack avoids recursive call-stack growth on deep sources.
    // Freeze names before any CAS/Node write, including when output lives below
    // the source. No staging file created by capture can enter this inventory.
    let mut directories = vec![(directory, entries, String::new())];
    let mut files = std::collections::BTreeMap::new();
    let mut remaining = shenko_node::MAX_ARTIFACT_BYTES;
    let mut remaining_bytes = shenko_node::MAX_EXTERNAL_ARTIFACT_BYTES;
    while let Some((directory, entries, prefix)) = directories.last_mut() {
        let Some(entry) = entries.next() else {
            directories.pop();
            continue;
        };
        let entry = entry?;
        let name = entry.file_name();
        let name_text = name.to_str().context("Skill source member path is not UTF-8")?;
        let relative = if prefix.is_empty() { name_text.to_owned() } else { format!("{prefix}/{name_text}") };
        shenko_compat_skill::validate_member_path(&relative)?;
        remaining = remaining.checked_sub(relative.len() * 6 + 256)
            .context("Skill source inventory exceeds the Node inline index bound")?;
        let file_type = entry.file_type()?;
        ensure!(!file_type.is_symlink(), "Skill source member {relative} is a symbolic link; copy the real file");
        if file_type.is_dir() {
            let child = directory.open_dir_nofollow(&name)
                .with_context(|| format!("open Skill source directory {relative}"))?;
            let entries = child.entries()?;
            directories.push((child, entries, relative));
            continue;
        }
        ensure!(file_type.is_file(), "Skill source member {relative} is not a regular file");
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt as _;
            // A regular file can become a FIFO between enumeration and open.
            // Open without blocking, then admit the actual opened descriptor.
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = directory.open_with(&name, &options)
            .with_context(|| format!("open Skill source member {relative}"))?;
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "Skill source member {relative} is not a regular file");
        remaining_bytes = remaining_bytes.checked_sub(metadata.len()).context("Skill source exceeds the external artifact bound")?;
        ensure!(relative != "SKILL.md" || metadata.len() <= shenko_node::MAX_ARTIFACT_BYTES as u64, "Skill entry exceeds the Node inline bound");
        #[cfg(unix)]
        let executable = {
            use cap_std::fs::PermissionsExt as _;
            metadata.permissions().mode() & 0o111 != 0
        };
        #[cfg(not(unix))]
        let executable = false;
        files.insert(relative, (metadata.len(), executable));
    }
    let mut source = CollectedSource {
        root: source_root,
        provenance_refs: vec![format!("source-directory:{}", root.display())],
        files,
        entry: Vec::new(),
    };
    source.open("SKILL.md")?.reader.take(shenko_node::MAX_ARTIFACT_BYTES as u64 + 1).read_to_end(&mut source.entry)?;
    ensure!(source.entry.len() <= shenko_node::MAX_ARTIFACT_BYTES, "Skill entry exceeds the Node inline bound");
    metadata(&source.entry)?;
    Ok(source)
}

#[cfg(feature = "compat-skill")]
fn skill(data_dir: &Path, command: CompatSkillCommand) -> Result<()> {
    match command {
        CompatSkillCommand::Import {
            root,
            unit_id,
            enabled,
        } => {
            let selected = collect_source(&root)?;
            let cas = shenko_store::cas::ContentStore::new(data_dir.join("cas"))?;
            let source = selected.capture(&cas)?;
            let metadata = metadata(&source.entry)?;
            let export = source.export(&unit_id, false)?;
            let mut node = node_of(data_dir)?;
            let package_root = data_dir.join("compat-units");
            ensure_plugin(&mut node, &package_root, SKILL_UNIT)?;
            let revision = export.root_revision.clone().unwrap_or_default();
            let artifact_digest = export.root_artifact_digest.clone();
            let imported = node.handle_unit_request(UnitRequest::Import {
                export,
                package_root: package_root.to_string_lossy().into_owned(),
                enable: Some(false),
            })?;
            if enabled {
                node.handle_unit_request(UnitRequest::SetEnabled {
                    unit_id: unit_id.clone(),
                    enabled: true,
                })?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "unit_id": unit_id,
                    "revision": revision,
                    "artifact_digest": artifact_digest,
                    "title": metadata.name,
                    "description": metadata.description,
                    "imported": imported,
                    "enabled": node.installed_unit_fact(&unit_id)?.context("imported Skill unit missing")?.enabled,
                    "source_members": source.index.files.len(),
                    "source_bytes": source.archive.bytes,
                    "source_archive_digest": source.archive.digest,
                    "original_entry_preserved": true,
                    "scripts_executed": false,
                    "dependencies_installed": false,
                    "declared_dependencies": metadata.frontmatter.get("metadata"),
                }))?
            );
            Ok(())
        }
        CompatSkillCommand::Read {
            unit_id,
            revision,
            artifact_digest,
            path,
            offset,
            limit,
        } => {
            let node = Node::open_inspect(&data_dir.join("node"))?.context("no node installation")?;
            let mut matches = node
                .retained_unit_facts(&unit_id)?
                .into_iter()
                .filter(|fact| fact.revision == revision && artifact_digest.as_ref().is_none_or(|digest| fact.artifact_digest.0 == *digest));
            let fact = matches.next()
                .with_context(|| format!("{unit_id} has no retained revision {revision}"))?;
            ensure!(matches.next().is_none(), "{unit_id}@{revision} is ambiguous; provide --artifact-digest");
            let unit = shenko_compat::installation::UnitRef {
                unit_id: unit_id.clone(),
                revision: fact.revision,
                artifact_digest: fact.artifact_digest,
            };
            // Local inspection reads a retained version; `offered` reports
            // whether the subject would actually be given this unit.
            let cas = shenko_store::cas::ContentStore::open_existing(data_dir.join("cas"))?;
            let source = InstalledSource::load(&cas, &node, &unit, false)?;
            match path {
                Some(path) => {
                    use std::io::Write as _;
                    ensure!(limit.is_none_or(|value| value > 0 && value <= MAX_READ_BYTES), "Skill read limit must be between 1 and {MAX_READ_BYTES}");
                    let file = source.index.files.get(&path).context("Skill source member is absent")?;
                    ensure!(offset <= file.bytes, "Skill read offset exceeds member length");
                    let mut position = offset;
                    let mut out = std::io::stdout().lock();
                    loop {
                        // Full-file output aligns reads to archive chunks, so
                        // each resource chunk is authenticated only once.
                        let chunk_bytes = shenko_compat_skill::CHUNK_BYTES as u64;
                        let batch = limit.unwrap_or((chunk_bytes - (file.offset + position) % chunk_bytes) as u32);
                        let bytes = source.index.read(&cas, &path, position, batch)?;
                        out.write_all(&bytes)?;
                        position += bytes.len() as u64;
                        if limit.is_some() || position == file.bytes { break; }
                    }
                    out.flush()?;
                    // Bytes are the payload on stdout; the mode is metadata.
                    eprintln!(
                        "read {path} from {unit_id}@{revision} ({} bytes, executable={}, next_offset={:?})",
                        position - offset, file.executable, (position < file.bytes).then_some(position)
                    );
                }
                None => println!("{}", serde_json::to_string_pretty(&source.view(&cas, &SourceQuery { path: None, offset, limit: limit.unwrap_or(MAX_READ_BYTES) })?)?),
            }
            Ok(())
        }
    }
}

/// Observe each independently installed adapter for this runtime lifetime.
///
/// The returned handles own the resident Fibers: dropping them is not a
/// promise of cleanup, so the caller keeps them for the process lifetime and
/// Cordis's own terminal teardown retires the workers and connections. A
/// disabled or absent unit publishes no definition and starts no external
/// connection. Later Node mutations are observed without restarting the CLI.
pub async fn spawn_adapters(
    supervisor: &shenko_kernel::Supervisor,
    store: std::sync::Arc<shenko_store::store::Store>,
    node: std::sync::Arc<parking_lot::Mutex<shenko_node::Node>>,
) -> Result<Vec<cordis_core::FiberHandle>> {
    let mut handles = Vec::new();
    for (unit_id, purpose) in plugins() {
        match unit_id {
            #[cfg(feature = "compat-mcp")]
            MCP_UNIT => handles.push(
                supervisor
                    .spawn(
                        shenko_compat_mcp::McpPlugin,
                        shenko_compat::runtime::AdapterHost::new(supervisor, store.clone(), node.clone()),
                    )
                    .await?,
            ),
            #[cfg(feature = "compat-skill")]
            SKILL_UNIT => handles.push(
                supervisor
                    .spawn(
                        shenko_compat_skill::SkillPlugin,
                        shenko_compat::runtime::AdapterHost::new(supervisor, store.clone(), node.clone()),
                    )
                    .await?,
            ),
            other => unreachable!("unlisted compatibility plugin {other}"),
        }
        tracing::info!(unit_id, purpose, "optional ecosystem adapter installation observer started");
    }
    Ok(handles)
}

#[cfg(all(test, unix, feature = "compat-skill"))]
mod tests {
    use super::collect_source;
    use std::{fs, os::unix::fs::{PermissionsExt, symlink}, path::PathBuf};

    #[test]
    fn source_import_rejects_link_roots_ancestors_and_oversized_members() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/compat-source-tests")
            .join(uuid::Uuid::new_v4().to_string());
        let source = fixture.join("parent/source");
        fs::create_dir_all(source.join("nested")).unwrap();
        let entry = b"\xef\xbb\xbf---\r\nname: source\r\ndescription: Original\r\n---\r\nBytes.\r\n";
        fs::write(source.join("SKILL.md"), entry).unwrap();
        let resource = source.join("nested/script");
        fs::write(&resource, b"\x00\xfforiginal").unwrap();
        fs::set_permissions(&resource, fs::Permissions::from_mode(0o755)).unwrap();

        let collected = collect_source(&source.join("nested/..")).unwrap();
        let cas = shenko_store::cas::ContentStore::new(fixture.join("cas")).unwrap();
        let captured = collected.capture(&cas).unwrap();
        assert_eq!(captured.entry, entry);
        assert_eq!(captured.index.read(&cas, "nested/script", 0, 128).unwrap(), b"\x00\xfforiginal");
        assert!(captured.index.files["nested/script"].executable);

        symlink(&source, fixture.join("root-link")).unwrap();
        assert!(collect_source(&fixture.join("root-link")).is_err());
        symlink(fixture.join("parent"), fixture.join("ancestor-link")).unwrap();
        assert!(collect_source(&fixture.join("ancestor-link/source")).is_err());
        // A cancelled lexical component must not bypass root admission.
        assert!(collect_source(&fixture.join("ancestor-link/../parent/source")).is_err());

        fs::File::options().write(true).open(&resource).unwrap()
            .set_len(shenko_node::MAX_EXTERNAL_ARTIFACT_BYTES + 1).unwrap();
        assert!(collect_source(&source).is_err());
    }
}


