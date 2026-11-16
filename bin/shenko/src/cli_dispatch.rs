//! CLI command dispatch; live startup enters the same composed runtime as embeddings.
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use shenko_identity::IdentityService;
use shenko_types::identity::{IdentityChange, IdentitySource};
use tracing::info;

use crate::Command;
use crate::cli::{AdminCommand, ContactsCommand, RuntimeCommand};
use crate::{compat, context_policy, node_sync, obelisk, runtime_channels, runtime_lifecycle, runtime_napcat, runtime_start, update_control};
use crate::cli_browser::browser_batch;
use crate::cli_root::root_command;
use crate::cli_tools::tool_command;
use crate::runtime_bootstrap::{create_data_directory_async, open_installation_store};
use crate::runtime_plugins::{install_native_definitions, spawn_media_services};

pub(crate) fn configured_ssh_public_identity_output(command: &Command) -> Result<Option<String>> {
    let Command::SshPublicIdentity { installation, target } = command else { return Ok(None); };
    let observed = shenko_exec_ssh::NativeSshFileTargetConfig::inspect_public_identity(installation, target)?;
    Ok(Some(serde_json::to_string_pretty(&observed)?))
}


pub(crate) async fn dispatch(
    command: Command,
    data_dir: PathBuf,
    supervisor: &shenko_kernel::Supervisor,
    identity_context: &cordis_core::Context,
    identity_service: &IdentityService,
    state: &mut runtime_lifecycle::RuntimeState,
) -> Result<()> {
    match command {
        Command::SshPublicIdentity { .. } => {
            anyhow::bail!("public identity command must complete before runtime setup");
        }
        Command::CompleteUpdate { request, digest } => {
            update_control::complete_handoff(request, digest, data_dir.clone()).await?;
        }
        Command::Node => node_sync::node_command(&data_dir)?,
        Command::Tool { command } => tool_command(&data_dir, command)?,
        Command::Capabilities => {
            // Real discovery, not a static list: the same installation path
            // the running agent uses is executed here, and the answer comes
            // from the live registry the plugins registered into.
            let db_path = data_dir.join("shenko.db");
            anyhow::ensure!(
                db_path.is_file(),
                "no installed runtime in this data directory"
            );
            let store = Arc::new(shenko_store::store::Store::open(
                db_path,
                data_dir.join("cas"),
            )?);
            let agent_id = store.get_existing_agent_id()?
                .context("no initialized subject in this data directory")?;
            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.clone(),
                ..Default::default()
            });
            let prepared_home = bootstrap.prepare(store.installation_id_for_subject(agent_id)?)?;
            prepared_home.record_local_provisioning(&store)?;
            let wasi_mounts = prepared_home.manifest.mounts.clone();
            drop(prepared_home);
            // The same media services the running agent publishes, so a
            // definition reported here is bound to the real runtime instead of
            // a name. No subject loop drains a discovery process, so the wake
            // channel has no receiver: an admission would honestly report
            // `pending` (its durable row already carries the fact) instead of
            // claiming a live queue.
            let (discovery_wake, discovery_wake_receiver) = tokio::sync::mpsc::channel(1024);
            drop(discovery_wake_receiver);
            let media = spawn_media_services(
                supervisor,
                &store,
                &data_dir,
                agent_id,
                discovery_wake,
            )
            .await?;
            let installed = install_native_definitions(
                supervisor,
                &store,
                &data_dir,
                wasi_mounts,
                &media,
            )
            .await?;
            // Discovery runs the same installation path as the agent: an
            // adapter this installation has enabled is registered here too,
            // so the printed providers are the ones the process would use.
            let discovery_node = Arc::new(parking_lot::Mutex::new(node_sync::open_node(
                &data_dir,
                &store,
            )?));
            let _compat_plugins = compat::spawn_adapters(
                supervisor,
                Arc::clone(&store),
                Arc::clone(&discovery_node),
                &data_dir,
            )
            .await?;
            let _obelisk_plugin = obelisk::spawn(supervisor, Arc::clone(&store), discovery_node, &data_dir).await?;
            let view = supervisor.runtime.view();
            let contracts = supervisor.hooks.contracts();
            let binding = supervisor.hooks.binding();
            // The capability changes the installation just produced are real
            // hook events. Dispatch them here and report the receipts, so the
            // discovery view and the graph agree about what happened.
            let dispatches = supervisor.hooks.drain_pending().await;
            let changes = supervisor.runtime.drain_changes();
            let report = serde_json::json!({
                "runtime_revision": view.revision,
                "capability_revision": supervisor.runtime.capability_revision(),
                "hook_graph": {
                    "digest": binding.graph_digest.to_string(),
                    "revision": binding.revision,
                    "points": binding.point_ids,
                    "live_leases": supervisor.hooks.live_leases(binding.revision),
                },
                "hook_points": contracts
                    .iter()
                    .map(|(point, contract)| serde_json::json!({
                        "point": point.as_str(),
                        "major": contract.major,
                        "mode": contract.mode,
                        "timeout_ms": contract.timeout_ms,
                        "failure": contract.failure,
                        "writable_paths": contract.writable_paths,
                        "writable_slots": contract.writable_slots,
                    }))
                    .collect::<Vec<_>>(),
                "providers": view.providers,
                "capabilities": view.capabilities,
                "help": view.help,
                "runtime_changes": changes,
                "hook_dispatches": dispatches
                    .iter()
                    .map(|dispatch| serde_json::json!({
                        "dispatch_id": dispatch.dispatch_id,
                        "point": dispatch.point.as_str(),
                        "generation": dispatch.generation,
                        "graph_digest": dispatch.graph_digest.to_string(),
                        "results": dispatch.results.len(),
                        "stopped_early": dispatch.stopped_early,
                    }))
                    .collect::<Vec<_>>(),
                "native_definitions": installed.native_child.installed_definitions(),
            });
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        Command::Context { command } => context_policy::command(&data_dir, command)?,
        Command::Compat { command } => compat::command(&data_dir, command).await?,
        Command::History { command } => obelisk::command(&data_dir, command).await?,
        Command::Admin { command } => {
            create_data_directory_async(&data_dir).await?;
            let store =
                shenko_store::store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
            let agent_id = store
                .get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no instance yet; run `shenko start` first"))?;
            match command {
                AdminCommand::Grant {
                    person,
                    platform_account,
                    granted_by,
                    reason,
                } => {
                    let input =
                        admin_input(person, platform_account, true, granted_by, reason)?;
                    let fact = store.set_admin_fact(agent_id, &input)?;
                    println!("{}", serde_json::to_string_pretty(&fact)?);
                }
                AdminCommand::Revoke {
                    person,
                    platform_account,
                    granted_by,
                    reason,
                } => {
                    let input =
                        admin_input(person, platform_account, false, granted_by, reason)?;
                    let fact = store.set_admin_fact(agent_id, &input)?;
                    println!("{}", serde_json::to_string_pretty(&fact)?);
                }
                AdminCommand::List => {
                    let facts = store.list_admin_facts(agent_id)?;
                    println!("{}", serde_json::to_string_pretty(&facts)?);
                }
            }
        }
        Command::Contacts { command } => {
            create_data_directory_async(&data_dir).await?;
            let store =
                shenko_store::store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
            let agent_id = store
                .get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no instance yet; run `shenko start` first"))?;
            match command {
                ContactsCommand::LifeImport {
                    life_dir,
                    ranks,
                    dry_run,
                } => {
                    let entries =
                        shenko_exec_host::read_people_catalog(&life_dir.join("catalog/people"))?;
                    let rank_records = ranks
                        .as_deref()
                        .map(shenko_exec_host::read_ranks_export)
                        .transpose()?;
                    if dry_run {
                        let mut report = shenko_exec_host::LifeImportReport {
                            persons_seen: entries.len(),
                            persons_created: 0,
                            persons_updated: 0,
                            claims_admitted: 0,
                            claims_replayed: 0,
                            ranks_seen: rank_records.as_ref().map(Vec::len).unwrap_or(0),
                            ranks_admitted: 0,
                            ranks_replayed: 0,
                            links: 0,
                            skipped: Vec::new(),
                        };
                        for entry in &entries {
                            if entry.life_person_id.trim().is_empty() {
                                report.skipped.push("entry without id".into());
                            }
                        }
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        let adapter = shenko_exec_host::LifeIdentityAdapter::new(Arc::new(store));
                        let mut report = adapter.import_people(agent_id, &entries)?;
                        if let Some(records) = &rank_records {
                            let rank_report = adapter.import_ranks(agent_id, records)?;
                            report.ranks_seen = rank_report.ranks_seen;
                            report.ranks_admitted = rank_report.ranks_admitted;
                            report.ranks_replayed = rank_report.ranks_replayed;
                            report.links = rank_report.links;
                            report.skipped.extend(rank_report.skipped);
                        }
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    }
                }
                ContactsCommand::Lookup {
                    person,
                    platform_account,
                    alias,
                } => {
                    let views = match (person, platform_account, alias) {
                        (Some(person), None, None) => {
                            store
                                .person_view(agent_id, person)?
                                .into_iter()
                                .collect::<Vec<_>>()
                        }
                        (None, Some(account), None) => {
                            store.person_by_platform_account(
                                agent_id,
                                &parse_platform_account(&account)?,
                            )?
                        }
                        (None, None, Some(alias)) => store.person_by_alias(agent_id, &alias)?,
                        _ => anyhow::bail!("give exactly one of --person, --platform-account or --alias"),
                    };
                    println!("{}", serde_json::to_string_pretty(&views)?);
                }
            }
        }
        Command::Sync { command } => node_sync::run(command, data_dir.clone()).await?,
        Command::Browser { file } => browser_batch(&data_dir, file.as_deref()).await?,
        Command::Runtime { command } => {
            let live_slot = shenko_update::slots::resolve_live_slot(&data_dir)?
                .unwrap_or_else(|| data_dir.clone());
            let db_path = live_slot.join(shenko_update::SLOT_DB_FILENAME);
            anyhow::ensure!(db_path.is_file(), "no existing subject store in data directory");
            let store = open_installation_store(&data_dir)?;
            let agent_id = store.get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no initialized subject"))?;
            let record = match command {
                RuntimeCommand::Pause { request_id } => {
                    let id = request_id.unwrap_or_default();
                    // Publish the caller's stable key even when commit outcome is
                    // uncertain; status reads it without replaying a request.
                    eprintln!("subject boundary request: {id}");
                    store.request_subject_quiesce(agent_id, id)?
                }
                RuntimeCommand::Status { request_id } => store.subject_quiesce(request_id)?
                    .ok_or_else(|| anyhow::anyhow!("quiesce request not found"))?,
                RuntimeCommand::Resume { request_id } => store.resume_subject_quiesce(agent_id, request_id)?,
            };
            println!("{}", serde_json::to_string_pretty(&record)?);
        }
        Command::Update { command } => {
            update_control::run_cli(command.into(), data_dir.clone()).await?;
        }
        Command::Root { command } => {
            root_command(&data_dir, command)?;
        }
        Command::Version => {
            let model = shenko_seed::ModelProfile::from_env();
            println!("Shenko（深刻）0.1.0  SPEC v0.6.2");
            println!("Model: {}", model.model.as_deref().unwrap_or("not_connected"));
            println!("Inference capacity: {:?}", model.capacity);
        }

        Command::Check { retained_objects } => {
            if retained_objects {
                // The pointer only chooses the physical database to audit. Its
                // path is reported; this does not assert active authority or
                // coverage of the initial, historical or staged sibling slots.
                let selected = shenko_update::slots::resolve_live_slot(&data_dir)?
                    .unwrap_or_else(|| data_dir.clone());
                let report = shenko_store::Store::inspect_retained_objects(
                    &selected.join(shenko_update::SLOT_DB_FILENAME),
                    &data_dir.join("cas"),
                    shenko_store::RetentionLimits::default(),
                )?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                anyhow::ensure!(report.complete,
                    "Store retention audit is incomplete; the report retains its gaps");
                return Ok(());
            }
            info!(data_dir=%data_dir.display(), "check");
            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.clone(),
                ..Default::default()
            });
            bootstrap.validate_target()?;
            let store = open_installation_store(&data_dir)?;
            println!(
                "Store: OK (SQLite {}, WAL={}, FTS5={})",
                store.capabilities.sqlite_version,
                store.capabilities.wal_mode,
                store.capabilities.fts5_available,
            );
            let subject = match store.get_existing_agent_id()? {
                Some(_) => shenko_seed::BootstrapSubjectState::ExistingGenesis,
                None => shenko_seed::BootstrapSubjectState::PendingGenesis,
            };
            let installation_id = store.installation_id_for_home()?;
            let prepared = bootstrap.prepare_with_recovery(installation_id, &store.cas)?;
            prepared.record_local_provisioning(&store)?;
            println!("Home manifest: {}", prepared.manifest_path.display());
            println!("Home: {}", serde_json::to_string_pretty(&prepared.manifest)?);
            for recovery in &prepared.recovered {
                println!("Recovered: {recovery}");
            }
            let owner_grant = store.get_bootstrap_owner_grant(installation_id)?;
            let view = bootstrap.view(
                installation_id,
                subject,
                shenko_seed::ModelProfile::from_env(),
                owner_grant,
            );
            println!("Bootstrap: {}", serde_json::to_string_pretty(&view)?);
            drop(prepared);
            println!("OK");
        }

        Command::Identity {
            set_name,
            set_soul,
            set_personality,
            expected_revision,
            history,
        } => {
            let store = open_installation_store(&data_dir)?;
            let agent_id = store
                .get_existing_agent_id()?
                .ok_or_else(|| anyhow::anyhow!("no instance yet; run `shenko start` first"))?;
            let mut current = store
                .latest_identity(agent_id)?
                .ok_or_else(|| anyhow::anyhow!("identity not initialized"))?;
            let changes = [
                set_name.map(IdentityChange::Name),
                set_soul.map(IdentityChange::Soul),
                set_personality.map(IdentityChange::Personality),
            ];
            let changes: Vec<_> = changes.into_iter().flatten().collect();
            if !changes.is_empty() {
                let expected = expected_revision.ok_or_else(|| {
                    anyhow::anyhow!(
                        "editing requires --expected-revision from the profile you read"
                    )
                })?;
                anyhow::ensure!(
                    current.revision == expected,
                    "IDENTITY_REVISION_CONFLICT: expected {expected}, actual {}",
                    current.revision
                );
                // An over-long edit is refused with a reason: it is never
                // silently truncated, so the operator sees what did not fit.
                for change in &changes {
                    identity_service
                        .check_change(change)
                        .map_err(|rejection| anyhow::anyhow!("IDENTITY_EDIT_REJECTED: {rejection}"))?;
                }
                if let Some(next) =
                    identity_service.revise(&current, &changes, IdentitySource::Operator)
                {
                    store.append_identity(&next, expected)?;
                    current = next;
                }
            }
            let shown = if history {
                store.identity_history(agent_id)?
            } else {
                vec![current]
            };
            for profile in shown {
                println!("{}", serde_json::to_string_pretty(&profile)?);
            }
        }

        Command::Start {
            port, pause_at_start, name, napcat, napcat_account, napcat_access_token, napcat_admin_qq,
            napcat_file_root, napcat_file_host, napcat_cli_url, channels_config, ssh_targets,
        } => {
            runtime_start::start(
                supervisor, identity_context, identity_service, state, data_dir,
                runtime_start::StartConfig {
                    port, pause_at_start, name, ssh_targets,
                    channels: runtime_channels::ChannelConfig {
                        napcat: runtime_napcat::NapCatOptions { napcat, napcat_account, napcat_access_token, napcat_admin_qq, napcat_file_root, napcat_file_host, napcat_cli_url },
                        channels_config,
                    },
                },
            ).await?;
        }
    }
    Ok(())
}

fn parse_platform_account(arg: &str) -> Result<shenko_types::contacts::PlatformAccount> {
    let parts: Vec<&str> = arg.split(':').collect();
    anyhow::ensure!(
        parts.len() == 4,
        "platform accounts use provider:account:tenant:platform_user_id \
         (empty tenant: provider:account::user)"
    );
    anyhow::ensure!(
        !parts[0].is_empty() && !parts[1].is_empty() && !parts[3].is_empty(),
        "provider, account and platform_user_id must not be empty"
    );
    Ok(shenko_types::contacts::PlatformAccount {
        provider: parts[0].to_owned(),
        account: parts[1].to_owned(),
        tenant: parts[2].to_owned(),
        platform_user_id: parts[3].to_owned(),
    })
}

fn admin_input(
    person: Option<shenko_types::ids::EntityId>,
    platform_account: Option<String>,
    granted: bool,
    granted_by: String,
    reason: Option<String>,
) -> Result<shenko_types::contacts::AdminFactInput> {
    use shenko_types::contacts::AdminSubjectKind;
    let (subject_kind, subject_key) = match (person, platform_account) {
        (Some(person), None) => (AdminSubjectKind::Person, person.to_string()),
        (None, Some(account)) => {
            (AdminSubjectKind::PlatformAccount, parse_platform_account(&account)?.key())
        }
        _ => anyhow::bail!("give exactly one of --person or --platform-account"),
    };
    Ok(shenko_types::contacts::AdminFactInput {
        subject_kind,
        subject_key,
        granted,
        granted_by,
        reason,
    })
}
