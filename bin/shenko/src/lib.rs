//! Shared Shenko runtime and owned host entrypoints.
//! CLI and embedded hosts execute this same composed implementation.
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

pub mod node_sync;
pub mod obelisk;
pub mod cli;
mod cli_browser;
mod cli_dispatch;
mod cli_root;
mod cli_tools;
mod compat;
#[cfg(feature = "compat-poppy")]
mod poppy;
mod context_policy;
mod policy_config;
mod document_runtime;
mod inference_runtime;
mod runtime_adapters;
mod runtime_authority;
mod runtime_bootstrap;
mod runtime_branches;
mod runtime_channels;
mod runtime_cognition;
mod runtime_consumers;
mod runtime_inference;
mod runtime_lifecycle;
mod runtime_model_health;
mod runtime_napcat;
mod runtime_owner;
mod runtime_plugins;
mod runtime_publications;
mod runtime_sessions;
mod runtime_start;
mod runtime_subject;
mod tool_install;
mod update_control;
mod update_runtime;

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Resume an immutable, installation-owned process handoff request.
    #[command(hide = true)]
    CompleteUpdate { request: PathBuf, digest: String },
    /// Print the configured public SSH identity without opening a signer or connecting.
    SshPublicIdentity {
        #[arg(long)]
        installation: PathBuf,
        #[arg(long)]
        target: String,
    },
    /// Start Shenko agent with UI and continuous thought loop
    Start {
        #[arg(long, default_value = "7860")]
        port: u16,
        /// Pause at a durable clean boundary before starting consumers.
        /// Receive input through the UI, then continue with `runtime resume`
        /// and this stable request UUID. An unresolved step cannot be skipped.
        #[arg(long)]
        pause_at_start: Option<shenko_types::ids::EntityId>,
        /// Instance name given by the user. Omit it to let the agent name
        /// itself. Existing instances are edited with an explicit revision CAS.
        #[arg(long)]
        name: Option<String>,
        /// NapCat WebSocket URL (e.g. ws://127.0.0.1:3001)
        #[arg(long, env = "NAPCAT_WS_URL")]
        napcat: Option<String>,
        /// Account bound to the persistent ingress database (not an admin grant).
        #[arg(long, env = "NAPCAT_BOT_QQ", requires = "napcat")]
        napcat_account: Option<u64>,
        /// Transport token of the NapCat endpoint. Sensitive: it is sent as
        /// an Authorization header and never appears in URLs, logs, receipts
        /// or bindings. It proves nothing about who may talk to the agent.
        #[arg(long, env = "NAPCAT_ACCESS_TOKEN", requires = "napcat")]
        napcat_access_token: Option<String>,
        /// Owner-set administrator QQ accounts (repeat the flag or use a
        /// comma-separated NAPCAT_ADMIN_QQ). Administration is a separate
        /// owner-set fact; it is never derived from a group role, the
        /// transport token or familiarity.
        #[arg(long = "napcat-admin", env = "NAPCAT_ADMIN_QQ", requires = "napcat")]
        napcat_admin_qq: Vec<u64>,
        /// Media root on the NapCat host that holds the attachment files the
        /// platform reports. Locators resolve inside it and nowhere else.
        #[arg(long, env = "NAPCAT_FILE_ROOT", requires = "napcat")]
        napcat_file_root: Option<std::path::PathBuf>,
        /// Name of the host that owns the attachment bytes. Defaults to
        /// `local`; a remote NapCat host must name itself so a locator is
        /// never mistaken for a local path.
        #[arg(long, env = "NAPCAT_FILE_HOST", requires = "napcat")]
        napcat_file_host: Option<String>,
        /// Poll an external napcat-cli daemon's event feed as an additional
        /// source of the same QQ channel: shared ingress spool, shared dedup,
        /// one plugin personality. Requires --napcat-account.
        #[arg(long, env = "NAPCAT_CLI_URL", requires = "napcat_account")]
        napcat_cli_url: Option<String>,
        /// JSON file naming the additional channel connectors this runtime
        /// installs: official QQ (`qq_official`), WeChat iLink (`ilink`) and
        /// Feishu/Lark (`feishu`). A platform absent from the file is not
        /// registered, not advertised in the reply grammar and never
        /// contacted. Credentials come from the file's own fields and never
        /// appear in logs, facts or receipts.
        #[arg(long, env = "SHENKO_CHANNELS_CONFIG")]
        channels_config: Option<PathBuf>,
        /// Explicit native SSH target installation; no ambient identity discovery.
        #[arg(long)]
        ssh_targets: Option<PathBuf>,
    },
    /// Show or edit the instance's name, soul and personality.
    /// Every edit is a new revision of the same agent; history is kept.
    Identity {
        #[arg(long)]
        set_name: Option<String>,
        #[arg(long)]
        set_soul: Option<String>,
        #[arg(long)]
        set_personality: Option<String>,
        /// Revision read by the editor; required for any mutation.
        #[arg(long)]
        expected_revision: Option<u64>,
        /// Print every revision, oldest first
        #[arg(long)]
        history: bool,
    },
    /// Handle a unified Node role/installation request from stdin.
    Node,
    /// Model-independent cognition and installation-package replication.
    Sync {
        #[command(subcommand)]
        command: node_sync::SyncCommand,
    },
    /// Inspect or request a subject-only boundary; this does not activate a candidate.
    Runtime {
        #[command(subcommand)]
        command: cli::RuntimeCommand,
    },
    /// Manage named WASM tools in the seed's managed tool store.
    /// Artifacts are real upstream WASI binaries; declared targets with no
    /// verified artifact are reported unresolved, never faked.
    Tool {
        #[command(subcommand)]
        command: cli::ToolCommand,
    },
    /// Show what this installation actually offers right now.
    ///
    /// The runtime installs its real native definitions and answers from the
    /// live registry, so the printed providers, capabilities, effect contracts
    /// and hook contracts are the ones this process would use.
    Capabilities,
    /// Optional third-party ecosystem adapters — MCP and Agent Skills are two
    /// separate plugins. Neither ships a vendor service, Skill, dependency or
    /// account grant, and neither is enabled by installing it.
    Compat {
        #[command(subcommand)]
        command: cli::CompatCommand,
    },
    /// Optional external harness history archives and Obelisk research queries.
    History {
        #[command(subcommand)]
        command: obelisk::HistoryCommand,
    },
    /// Inspect and manage the installed context strategies.
    Context {
        #[command(subcommand)]
        command: cli::ContextCommand,
    },
    /// Owner-set admin facts. Admin is a human decision only:
    /// it never follows familiarity, group status, platform roles or
    /// voiceprints, and ordinary users can talk without one.
    Admin {
        #[command(subcommand)]
        command: cli::AdminCommand,
    },
    /// Contact/person directory. Local owner path to
    /// inspect people and import the owner's own Life identity material;
    /// the running subject reaches the same store through the
    /// person.directory native child.
    Contacts {
        #[command(subcommand)]
        command: cli::ContactsCommand,
    },
    /// Validate configuration, or audit an existing Store's retained objects.
    Check {
        /// Read one existing database and its CAS without opening a writer or
        /// preparing home state. Print a bounded retention report, not a GC plan.
        #[arg(long)]
        retained_objects: bool,
    },
    /// Print version info
    Version,
    /// Drive the native browser provider directly (native proof / diagnostics):
    /// read a JSON batch of steps from `--file` (or stdin) and print one JSON
    /// result per step. Session handles returned by `session open` are valid
    /// across steps of one batch.
    Browser {
        /// JSON batch file (default: stdin)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Software-generation activation. Build a candidate
    /// on a separate copy, smoke-launch it, swap through the durable
    /// selector, roll back while retaining new facts. Every step is
    /// deterministic and recoverable without a model.
    Update {
        #[command(subcommand)]
        command: cli::UpdateCli,
    },
    /// Cognitive root version chain. Capture, reinstate,
    /// inspect the owned versioned root.
    Root {
        #[command(subcommand)]
        command: cli::RootCli,
    },
}


/// Parse the process CLI and enter the same owned host used by embeddings.
pub fn run_cli() -> Result<()> {
    let cli = cli::Cli::parse();

    // The SDK logs full protocol messages, including private payment payloads,
    // at INFO/TRACE. Keep these events out even when a more specific --log
    // directive enables them. Our own bounded operation receipts remain the
    // source of protocol observations; filtering after formatting is too late.
    tracing_subscriber::registry()
        .with(EnvFilter::new(&cli.log))
        .with(tracing_subscriber::filter::filter_fn(|metadata| {
            metadata.target() != "rmcp" && !metadata.target().starts_with("rmcp::")
        }))
        .with(fmt::layer().with_writer(std::io::stderr).with_target(true).compact())
        .init();

    let data_dir = cli.data_dir.unwrap_or_else(|| {
        dirs_next::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("shenko")
    });

    run_owned(cli.command, data_dir)
}

/// Run one host while retaining exclusive installation ownership until its
/// Tokio runtime is dropped. Handoff launches adopt the existing locked FD.
pub fn run_owned(command: Command, data_dir: PathBuf) -> Result<()> {
    if let Some(output) = cli_dispatch::configured_ssh_public_identity_output(&command)? {
        println!("{output}");
        return Ok(());
    }
    // This guard outlives the Tokio runtime. In particular, cancellation of
    // an async task is not proof that a blocking/provider operation has ended.
    let (owner, data_dir) = if matches!(&command, Command::Start { .. }) {
        let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: data_dir.clone(), ..Default::default()
        });
        // Reject an unrelated target before creating any owner or store file.
        bootstrap.validate_target()?;
        runtime_bootstrap::create_data_directory(&data_dir)?;
        let data_dir = data_dir.canonicalize()?;
        let owner = update_control::acquire_launch_owner(&data_dir)?;
        // Recheck after obtaining the guard: another starter may have finished
        // bootstrap between the initial read and lock acquisition.
        let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: data_dir.clone(), ..Default::default()
        });
        bootstrap.validate_target()?;
        owner.verify_path(&data_dir)?;
        (Some(owner), data_dir)
    } else {
        (None, data_dir)
    };
    let mut command = command;
    let result = loop {
        let mut restart = runtime_authority::restart_command(&command);
        let mut startup_pause_applied = false;
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(runtime_lifecycle::run(command, data_dir.clone(), &mut startup_pause_applied));
        if startup_pause_applied {
            // The original request is now durable. Reconstruction reads its
            // current state, including a later explicit resume, instead of
            // replaying a one-time startup action after authority recovery.
            if let Some(Command::Start { pause_at_start, .. }) = &mut restart {
                *pause_at_start = None;
            }
        }
        let recovery = result.as_ref().err()
            .is_some_and(runtime_authority::requires_reconstruction);
        if recovery {
            // Original blocking calls may still finish and append E1. Their
            // canceled provider cannot admit another send, and the next
            // source binding fences every control write across incarnations.
            runtime.shutdown_timeout(std::time::Duration::from_secs(2));
            if let Some(next) = restart {
                tracing::warn!("reconstructing runtime from durable authority and branch facts");
                command = next;
                continue;
            }
        } else {
            drop(runtime);
        }
        break result;
    };
    drop(owner);
    result
}


#[cfg(test)]
mod recovery_integration_tests;
#[cfg(test)]
mod reply_directive_tests;
