//! Named WASM tool and environment commands over the Node installation authority.
use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::{ToolCommand, ToolEnvironmentCommand};
use crate::{node_sync, tool_install};
use crate::runtime_bootstrap::open_installation_store;

/// The capability declaration the `--requires-*` install flags author.
///
/// This is authoring, not admission: nothing here grants anything. The
/// declaration it produces is stored in the package description, frozen into
/// every environment generation that pins the version, and admitted by the
/// executor against the resources a run actually receives.
fn authored_capabilities(
    filesystem: &[String],
    environment: &[String],
    host: &[String],
) -> Result<shenko_node::PackageDeclaration> {
    use shenko_node::{
        FileRequirement, HostRequirement, NetworkTarget, PackageCapabilities, PackageDeclaration,
    };
    if filesystem.is_empty() && environment.is_empty() && host.is_empty() {
        // No flags means the author recorded no declaration. That is
        // `unrecorded`, not "needs nothing".
        return Ok(PackageDeclaration::unrecorded());
    }
    let mut capabilities = PackageCapabilities::default();
    for spec in filesystem {
        let (guest_path, write) = match spec.rsplit_once(':') {
            Some((path, "rw")) => (path, true),
            Some((path, "ro")) => (path, false),
            Some((_, other)) => anyhow::bail!(
                "--requires-fs access must be `ro` or `rw`, not {other:?}: {spec:?}"
            ),
            None => (spec.as_str(), false),
        };
        capabilities.filesystem.push(FileRequirement {
            guest_path: guest_path.to_string(),
            write,
        });
    }
    capabilities.environment.extend(environment.iter().cloned());
    // Repeated `network:` flags are one requirement with one target list, so
    // the declaration keeps a single canonical form.
    let mut network_targets: Vec<NetworkTarget> = Vec::new();
    for spec in host {
        match parse_host_requirement(spec)? {
            HostRequirement::Network { targets } => network_targets.extend(targets),
            requirement => capabilities.host.push(requirement),
        }
    }
    if !network_targets.is_empty() {
        capabilities.host.push(HostRequirement::Network {
            targets: network_targets,
        });
    }
    let capabilities = capabilities.normalized();
    capabilities.validate()?;
    Ok(PackageDeclaration::declared(capabilities))
}

/// Parse one `--requires-host` value: `clock`, `random`,
/// `host_import:<module>` or `network:<host>:<port>[:connect,listen,accept]`.
fn parse_host_requirement(spec: &str) -> Result<shenko_node::HostRequirement> {
    use shenko_node::HostRequirement;
    match spec {
        "clock" => Ok(HostRequirement::Clock {}),
        "random" => Ok(HostRequirement::Random {}),
        other => {
            if let Some(module) = other.strip_prefix("host_import:") {
                return Ok(HostRequirement::HostImport {
                    module: module.to_string(),
                });
            }
            if let Some(target) = other.strip_prefix("network:") {
                return Ok(HostRequirement::Network {
                    targets: vec![parse_network_target(target)?],
                });
            }
            anyhow::bail!(
                "--requires-host expects clock, random, host_import:<module> or \
                 network:<host>:<port>[:connect,listen,accept]: {spec:?}"
            )
        }
    }
}

/// Parse `<host>:<port>[:operations]`; a bracketed IPv6 literal is
/// `[<address>]:<port>[:operations]`. Resolving a name is part of what the
/// requirement means, so it is derived from the host being a name rather than
/// an address literal, not from a policy default.
fn parse_network_target(spec: &str) -> Result<shenko_node::NetworkTarget> {
    use shenko_node::{NetworkOperation, NetworkTarget};
    let (host, rest) = if let Some(bracketed) = spec.strip_prefix('[') {
        let (address, rest) = bracketed.split_once("]:").with_context(|| {
            format!("a bracketed network target must be [address]:port[:operations]: {spec:?}")
        })?;
        (address.to_string(), rest)
    } else {
        let (host, rest) = spec
            .split_once(':')
            .with_context(|| format!("a network target must be host:port[:operations]: {spec:?}"))?;
        (host.to_string(), rest)
    };
    let (port, operations) = match rest.split_once(':') {
        Some((port, operations)) => (port, operations),
        None => (rest, "connect"),
    };
    let port: u16 = port
        .parse()
        .with_context(|| format!("network target port is not a u16: {spec:?}"))?;
    let mut parsed = Vec::new();
    for operation in operations.split(',') {
        parsed.push(match operation {
            "connect" => NetworkOperation::Connect {},
            "listen" => NetworkOperation::Listen {},
            "accept" => NetworkOperation::Accept {},
            other => anyhow::bail!(
                "a network operation must be connect, listen or accept, not {other:?}"
            ),
        });
    }
    let resolve = host.parse::<std::net::IpAddr>().is_err();
    Ok(NetworkTarget {
        host,
        port,
        resolve,
        operations: parsed,
    })
}

/// `shenko tool …` — inspect and manage the seed's named WASM tool store.
/// Install and exec go through the node's installation-unit inventory — the
/// same authority node_sync replicates — while list is a pure read of that
/// inventory. `<data>/tools` is only where the unit authority materializes
/// bytes; no separate tool registry exists to diverge or adopt foreign data.
pub(crate) fn tool_command(data_dir: &PathBuf, command: ToolCommand) -> Result<()> {
    use shenko_node::Node;
    use shenko_wasm::{
        InputFile, OutputFile, ToolAbi, ToolSource, WasiRunOutcome, installed_tools,
        named_target_report, resolve_tool,
    };
    use std::io::Write as _;

    let tools_root = data_dir.join(shenko_wasm::TOOL_DIR_NAME);
    match command {
        ToolCommand::List => {
            // Pure read: never creates or migrates storage, never writes a
            // registry file. An installation that has no node DB simply has
            // no installed tools.
            let node = Node::open_inspect(&data_dir.join("node"))?;
            let (records, environment) = match &node {
                Some(node) => {
                    let selector = node.environment_selector()?;
                    (
                        installed_tools(node)?,
                        serde_json::json!({
                            "pinned": selector.is_some(),
                            "selector": selector,
                            "generations": node.environment_generations(16)?,
                        }),
                    )
                }
                None => (
                    Vec::new(),
                    serde_json::json!({
                        "pinned": false,
                        "selector": null,
                        "generations": [],
                    }),
                ),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "tools": records,
                    "named_targets": named_target_report(&records),
                    "environment": environment,
                }))?
            );
        }
        ToolCommand::Environment { command } => {
            use shenko_node::UnitRequest;
            match command {
                ToolEnvironmentCommand::Status => {
                    // Reads never create or migrate storage.
                    let node = Node::open_inspect(&data_dir.join("node"))?;
                    let status = match &node {
                        Some(node) => serde_json::json!({
                            "environment": node.environment_report(64)?,
                            "tools": installed_tools(node)?,
                        }),
                        None => serde_json::json!({
                            "pinned": false,
                            "selector": null,
                            "generations": [],
                            "activations": [],
                            "tools": [],
                            "note": "no node installation at this data directory",
                        }),
                    };
                    println!("{}", serde_json::to_string_pretty(&status)?);
                }
                ToolEnvironmentCommand::Prepare { root } => {
                    let store = open_installation_store(data_dir)?;
                    let mut node = node_sync::open_node(data_dir, &store)?;
                    let roots = tool_install::environment_roots(&node, &root)?;
                    let prepared =
                        node.handle_unit_request(UnitRequest::PrepareEnvironment { roots })?;
                    println!("{}", serde_json::to_string_pretty(&prepared)?);
                }
                ToolEnvironmentCommand::Activate {
                    generation,
                    expected_revision,
                } => {
                    let store = open_installation_store(data_dir)?;
                    let mut node = node_sync::open_node(data_dir, &store)?;
                    let activated = node.handle_unit_request(UnitRequest::ActivateEnvironment {
                        generation,
                        expected_revision,
                    })?;
                    println!("{}", serde_json::to_string_pretty(&activated)?);
                }
                ToolEnvironmentCommand::Rollback { expected_revision } => {
                    let store = open_installation_store(data_dir)?;
                    let mut node = node_sync::open_node(data_dir, &store)?;
                    let rolled = node.handle_unit_request(UnitRequest::RollbackEnvironment {
                        expected_revision,
                    })?;
                    println!("{}", serde_json::to_string_pretty(&rolled)?);
                }
            }
        }
        ToolCommand::Metapackage { unit_id } => {
            use shenko_node::UnitRequest;
            let store = open_installation_store(data_dir)?;
            let mut node = node_sync::open_node(data_dir, &store)?;
            let result = node.handle_unit_request(UnitRequest::QueryMetapackage { unit_id })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        ToolCommand::Distribution { unit_id } => {
            use shenko_node::UnitRequest;
            let store = open_installation_store(data_dir)?;
            let mut node = node_sync::open_node(data_dir, &store)?;
            let result = node.handle_unit_request(UnitRequest::QueryDistribution { unit_id })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        ToolCommand::Slot { macro_id, contract } => {
            use shenko_node::UnitRequest;
            let store = open_installation_store(data_dir)?;
            let mut node = node_sync::open_node(data_dir, &store)?;
            let result = node.handle_unit_request(UnitRequest::QuerySlot { macro_id, contract })?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        ToolCommand::Install {
            file,
            name,
            version,
            abi,
            entrypoint,
            source_uri,
            source_release,
            provenance,
            expected_digest,
            source_sha256,
            license,
            requires_fs,
            requires_env,
            requires_host,
        } => {
            let abi = ToolAbi::parse(&abi)
                .ok_or_else(|| anyhow::anyhow!("unknown --abi {abi:?}"))?;
            shenko_wasm::validate_tool_name(&name)?;
            let capabilities =
                authored_capabilities(&requires_fs, &requires_env, &requires_host)?;
            // The artifact's identity is computed by streaming the file, so
            // a large module is never accumulated here just to be hashed.
            // The real ABI and the provenance digest are checked by the
            // installer, which reads the module's actual imports.
            let actual = {
                let mut handle = std::fs::File::open(&file)
                    .with_context(|| format!("open artifact {}", file.display()))?;
                let mut reader = std::io::BufReader::with_capacity(
                    shenko_wasm::artifact::ARTIFACT_STREAM_BUFFER,
                    &mut handle,
                );
                shenko_wasm::artifact::stream_identity(&mut reader)?.0
            };
            if let Some(expected) = &expected_digest {
                let expected = shenko_types::object::Digest(expected.clone());
                anyhow::ensure!(
                    expected.algorithm() == "blake3",
                    "TOOL_DIGEST_UNSUPPORTED:{name}: only blake3 digests are admitted"
                );
                anyhow::ensure!(
                    expected == actual,
                    "TOOL_DIGEST_MISMATCH:{name}: expected {expected}, artifact is {actual}"
                );
            }

            // Install requires an existing managed installation: the record
            // is this installation's own unit inventory entry.
            let live_slot = shenko_update::slots::resolve_live_slot(data_dir)?
                .unwrap_or_else(|| data_dir.clone());
            anyhow::ensure!(
                live_slot.join(shenko_update::SLOT_DB_FILENAME).is_file(),
                "no managed installation at {}; run `shenko start` once",
                data_dir.display()
            );
            // Admit any existing tools tree BEFORE mutating state: the same
            // foreign-content check Bootstrap::validate_target enforces. An
            // unowned nonempty tools/ directory must fail here rather than be
            // overwritten by write_package after the import is recorded.
            let tools_dir = data_dir.join(shenko_wasm::TOOL_DIR_NAME);
            if tools_dir.exists() && std::fs::read_dir(&tools_dir)?.next().is_some() {
                let node_check = Node::open_inspect(&data_dir.join("node"))?
                    .ok_or_else(|| anyhow::anyhow!(
                        "DATA_PATH_CONFLICT: {} exists but no node installation owns it",
                        tools_dir.display()
                    ))?;
                let roots = node_check.materialization_roots()?;
                let canonical = tools_dir.canonicalize()?;
                anyhow::ensure!(
                    roots.iter().any(|r| std::path::Path::new(r).canonicalize().ok() == Some(canonical.clone())),
                    "DATA_PATH_CONFLICT: {} is not this installation's tool materialization root",
                    tools_dir.display()
                );
            }
            let store = open_installation_store(data_dir)?;
            let mut node = node_sync::open_node(data_dir, &store)?;
            let request = tool_install::InstallRequest {
                name: name.clone(),
                version: version.clone(),
                abi,
                entrypoint: entrypoint.unwrap_or_else(|| format!("/tools/{name}")),
                source: ToolSource {
                    uri: source_uri.unwrap_or_default(),
                    release: source_release.unwrap_or_default(),
                    provenance: provenance.unwrap_or_else(|| "manual".into()),
                },
                license,
                sha256: source_sha256,
                path: file.clone(),
                capabilities,
            };
            let installed = tool_install::install_one(&mut node, &store, &tools_root, &request)?;
            // Storing verified bytes and selecting them are two facts. Locking
            // the closure and publishing it is the second one; a closure that
            // cannot be resolved is reported after the real install instead of
            // being hidden behind a "pip-installed" success.
            let environment = tool_install::sync_environment(
                &mut node,
                &[(installed.unit_id.clone(), installed.version.clone())],
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "installed": installed,
                    "environment": match &environment {
                        Ok(value) => value.clone(),
                        Err(error) => serde_json::json!({
                            "activated": false,
                            "error": format!("{error:#}"),
                        }),
                    },
                }))?
            );
            if let Err(error) = &environment {
                anyhow::bail!(
                    "TOOL_ENVIRONMENT_NOT_ACTIVATED: the unit is stored but the environment \
                     was not updated: {error:#}"
                );
            }
        }
        ToolCommand::Inspect { file } => {
            // No installation is touched: the ABI and the imports are read
            // from the module's actual bytes.
            println!(
                "{}",
                serde_json::to_string_pretty(&tool_install::inspect_artifact(&file)?)?
                        );
        }
        ToolCommand::Manifest {
            receipt,
            artifacts,
            set,
            version,
            name_prefix,
            license,
            license_file,
            source_uri,
            provenance,
            abi,
            out,
        } => {
            let abi = ToolAbi::parse(&abi)
                .ok_or_else(|| anyhow::anyhow!("unknown --abi {abi:?}"))?;
            let meta = shenko_wasm::ToolSetMeta {
                set,
                version,
                name_prefix,
                license_spdx: license,
                license_file,
                license_notice: None,
                source_uri,
                provenance,
                abi,
                recipe: None,
                receipt_path: receipt.display().to_string(),
            };
            let manifest = tool_install::build_manifest(&receipt, &artifacts, &meta)?;
            let encoded = serde_json::to_vec_pretty(&manifest)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &encoded)
                        .with_context(|| format!("write manifest {}", path.display()))?;
                    println!("{}", path.display());
                }
                None => println!("{}", String::from_utf8_lossy(&encoded)),
            }
        }
        ToolCommand::InstallSet {
            manifest,
            artifacts,
            requires_fs,
            requires_env,
            requires_host,
        } => {
            let encoded = std::fs::read(&manifest)
                .with_context(|| format!("read manifest {}", manifest.display()))?;
            let set = shenko_wasm::ToolSetManifest::parse(&encoded)?;
            let capabilities =
                authored_capabilities(&requires_fs, &requires_env, &requires_host)?;
            anyhow::ensure!(
                data_dir.join("shenko.db").is_file(),
                "no managed installation at {}; run `shenko start` once",
                data_dir.display()
            );
            // The same foreign-content admission the single install performs:
            // an unowned nonempty tools/ directory is refused before any
            // unit is written.
            let tools_dir = data_dir.join(shenko_wasm::TOOL_DIR_NAME);
            if tools_dir.exists() && std::fs::read_dir(&tools_dir)?.next().is_some() {
                let node_check = Node::open_inspect(&data_dir.join("node"))?
                    .ok_or_else(|| anyhow::anyhow!(
                        "DATA_PATH_CONFLICT: {} exists but no node installation owns it",
                        tools_dir.display()
                    ))?;
                let roots = node_check.materialization_roots()?;
                let canonical = tools_dir.canonicalize()?;
                anyhow::ensure!(
                    roots.iter().any(|root| std::path::Path::new(root).canonicalize().ok() == Some(canonical.clone())),
                    "DATA_PATH_CONFLICT: {} is not this installation's tool materialization root",
                    tools_dir.display()
                );
            }
            let store = shenko_store::store::Store::open(
                data_dir.join("shenko.db"),
                data_dir.join("cas"),
            )?;
            let mut node = node_sync::open_node(data_dir, &store)?;
            let installed = tool_install::install_set(
                &mut node,
                &store,
                &tools_root,
                &set,
                &artifacts,
                &capabilities,
            )?;
            // One generation for the whole set: a tool set is one closure, so
            // the profile is not advanced 91 times.
            let members: Vec<(String, String)> = installed
                .installed
                .iter()
                .map(|tool| (tool.unit_id.clone(), tool.version.clone()))
                .collect();
            let environment = tool_install::sync_environment(&mut node, &members);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "install": installed,
                    "environment": match &environment {
                        Ok(value) => value.clone(),
                        Err(error) => serde_json::json!({
                            "activated": false,
                            "error": format!("{error:#}"),
                        }),
                    },
                }))?
            );
            if let Err(error) = &environment {
                anyhow::bail!(
                    "TOOL_ENVIRONMENT_NOT_ACTIVATED: the set is stored but the environment \
                     was not updated: {error:#}"
                );
            }
        }
        ToolCommand::Exec {
            tool,
            read_only,
            args,
            env,
            stdin_file,
            stdout_file,
            stderr_file,
        } => {
            let manifest_path = data_dir.join(shenko_seed::HOME_MANIFEST_FILE);
            anyhow::ensure!(
                manifest_path.is_file(),
                "WASM_PROFILE_UNAVAILABLE: no managed home at {}; run `shenko start` once",
                manifest_path.display()
            );
            // Same managed-home admission as normal startup: installation
            // binding, mount-source confinement to this data dir, runtime
            // generation and readiness — a copied home manifest can never
            // alias a sibling installation's mounts.
            let manifest = shenko_seed::bootstrap::validate_home_for_exec(data_dir)?;
            // Installation binding part 1: the home manifest's declared
            // installation must be this store's ledger identity — a copied
            // home/ manifest cannot alias a sibling installation's ledger.
            let binding = shenko_store::Store::inspect_installation_binding(
                &data_dir.join("shenko.db"),
            )?;
            anyhow::ensure!(
                manifest.installation_id == binding.ledger_id,
                "INSTALLATION_IDENTITY_MISMATCH: home manifest installation_id \
                 does not match this store's ledger; reconcile before exec"
            );
            let node = Node::open_inspect(&data_dir.join("node"))?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no node installation at {}/node; run `shenko start` once",
                        data_dir.display()
                    )
                })?;
            // Installation binding part 2: the node must belong to this
            // subject — the same identity check node_sync::open_node enforces
            // on the writable path. A transplanted node's tool records cannot
            // execute against this home.
            let expected = binding.subject.map(|subject| shenko_node::Identity {
                agent_id: subject.agent_id,
                branch_id: subject.branch_id,
                origin_proof: subject.origin_proof,
            });
            anyhow::ensure!(
                node.local_identity()? == expected,
                "INSTALLATION_IDENTITY_MISMATCH: node identity does not match \
                 this installation's subject binding; reconcile before exec"
            );
            let facts = node.tool_unit_facts()?;
            let (record, _) = resolve_tool(&facts, &tool)?;
            // The package's declared requirements are admitted against the
            // grant this run actually gives the guest: the resolved mount
            // profile and the explicit environment. A declared requirement the
            // run does not grant is a refusal — never a silently weaker guest.
            let mut guest_env: Vec<(String, String)> = Vec::with_capacity(env.len());
            for entry in env {
                let (key, value) = entry
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--env requires KEY=VALUE: {entry:?}"))?;
                guest_env.push((key.into(), value.into()));
            }
            let profile = if read_only {
                shenko_wasm::ToolMountProfile::ReadOnly
            } else {
                shenko_wasm::ToolMountProfile::ManagedHome
            };
            let mounts = profile.resolve(&manifest.mounts);
            let granted_environment: std::collections::BTreeMap<String, String> =
                guest_env.iter().cloned().collect();
            let granted = shenko_wasm::GrantedCapabilities::of_invocation(
                &mounts,
                &granted_environment,
            );
            let unmet = shenko_wasm::capability_problems(&record.capabilities, &granted);
            anyhow::ensure!(
                unmet.is_empty(),
                "{}",
                unmet.join("; ")
            );
            // Every execution re-verifies the sealed bytes and the real ABI;
            // the materialized copy is never trusted. A tool whose bytes live
            // in the installation's content store is reassembled from its
            // chunk objects there; only then is it compiled.
            let (module, _inspection) = match record.artifact.kind {
                shenko_wasm::ToolArtifactKind::NodeInline => {
                    shenko_wasm::tool_module(&node, &record)?
                }
                shenko_wasm::ToolArtifactKind::ContentStore => {
                    let store = shenko_store::store::Store::open(
                        data_dir.join("shenko.db"),
                        data_dir.join("cas"),
                    )?;
                    let content = shenko_wasm::CasToolContentStore::new(&store.cas);
                    shenko_wasm::tool_module_with_store(&node, &record, &content)?
                }
            };
            let stdin_bytes = match &stdin_file {
                Some(path) => std::fs::read(path)
                    .with_context(|| format!("read stdin file {}", path.display()))?,
                None => Vec::new(),
            };
            // Streams always ride real files: an explicit destination file, a
            // temp file copied to the inherited terminal stream afterwards.
            let io_dir = std::env::temp_dir().join(format!(
                "shenko-tool-exec-{}",
                shenko_types::ids::EntityId::new()
            ));
            std::fs::create_dir_all(&io_dir)?;
            let stdin_path = io_dir.join("stdin.bin");
            let stdout_path = stdout_file.clone().unwrap_or_else(|| io_dir.join("stdout.bin"));
            let stderr_path = stderr_file.clone().unwrap_or_else(|| io_dir.join("stderr.bin"));
            std::fs::write(&stdin_path, &stdin_bytes)?;
            let mut command = shenko_wasm::WasiCommand::new(
                InputFile::new(std::fs::File::open(&stdin_path)?),
                OutputFile::new(std::fs::File::create(&stdout_path)?),
                OutputFile::new(std::fs::File::create(&stderr_path)?),
            );
            let mut arguments = Vec::with_capacity(args.len() + 1);
            arguments.push(record.name.clone());
            arguments.extend(args);
            command.arguments = arguments;
            command.environment = guest_env;
            let mut runtime = shenko_wasm::WasmRuntime::with_mounts(mounts)?;
            let code = runtime.run_wasi_module(&module, command);
            if stdout_file.is_none() {
                std::io::stdout().write_all(&std::fs::read(&stdout_path).unwrap_or_default())?;
            }
            if stderr_file.is_none() {
                std::io::stderr().write_all(&std::fs::read(&stderr_path).unwrap_or_default())?;
            }
            let _ = std::fs::remove_dir_all(&io_dir);
            match code {
                Ok(WasiRunOutcome::Exited(code)) => std::process::exit(code),
                Ok(outcome) => {
                    eprintln!("wasi exec stopped: {outcome:?}");
                    std::process::exit(124);
                }
                Err(error) => {
                    eprintln!("wasi exec transport error: {error:#}");
                    std::process::exit(2);
                }
            }
        }
    }
    Ok(())
}
