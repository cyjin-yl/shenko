//! Shared Shenko runtime and owned host entrypoints — SPEC §24.5, §44.
//! CLI and embedded hosts execute this same composed implementation.
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use shenko_identity::IdentityService;
use shenko_types::channel::ChannelPlugin as _;
use shenko_types::identity::{IdentityChange, IdentityProfile, IdentitySource};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt};

pub mod node_sync;
mod compat;
mod runtime_owner;
mod context_policy;
mod inference_runtime;
mod tool_install;
mod update_runtime;
mod update_control;
#[derive(Parser, Debug)]
#[command(
    name = "shenko",
    about = "Shenko（深刻）— personal agent framework",
    version = "0.1.0"
)]
struct Cli {
    #[arg(long, env = "SHENKO_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value = "info", env = "SHENKO_LOG")]
    log: String,
    #[command(subcommand)]
    command: Command,
}

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
        /// Explicit native SSH target installation; no ambient identity discovery.
        #[arg(long)]
        ssh_targets: Option<PathBuf>,
    },
    /// Show or edit the instance's name, soul and personality — SPEC §6.
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
        command: RuntimeCommand,
    },
    /// Manage named WASM tools in the seed's managed tool store — SPEC §8.2.
    /// Artifacts are real upstream WASI binaries; declared targets with no
    /// verified artifact are reported unresolved, never faked.
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    /// Show what this installation actually offers right now — SPEC §18.
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
        command: CompatCommand,
    },
    /// Inspect and manage the installed context strategies — SPEC §12.3.1.
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    /// Owner-set admin facts — SPEC §16. Admin is a human decision only:
    /// it never follows familiarity, group status, platform roles or
    /// voiceprints, and ordinary users can talk without one.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Contact/person directory — SPEC §15.2, §15.2.1. Local owner path to
    /// inspect people and import the owner's own Life identity material;
    /// the running subject reaches the same store through the
    /// person.directory native child.
    Contacts {
        #[command(subcommand)]
        command: ContactsCommand,
    },
    /// Validate configuration and exit
    Check,
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
    /// Software-generation activation — SPEC §9/§10/§44. Build a candidate
    /// on a separate copy, smoke-launch it, swap through the durable
    /// selector, roll back while retaining new facts. Every step is
    /// deterministic and recoverable without a model.
    Update {
        #[command(subcommand)]
        command: UpdateCli,
    },
}

#[derive(Subcommand, Debug)]
pub enum CompatCommand {
    /// Installed plugins, their enable state and the external units a previous
    /// import recorded. Reads only; never connects or imports anything.
    Status,
    /// Offer or withdraw one adapter unit. The two plugins are independent.
    Enable { unit_id: String },
    Disable { unit_id: String },
    /// External MCP connections, described by an operator-supplied file.
    #[cfg(feature = "compat-mcp")]
    Mcp {
        #[command(subcommand)]
        command: CompatMcpCommand,
    },
    /// Agent Skills source material, preserved exactly as selected.
    #[cfg(feature = "compat-skill")]
    Skill {
        #[command(subcommand)]
        command: CompatSkillCommand,
    },
}

#[cfg(feature = "compat-mcp")]
#[derive(Subcommand, Debug)]
pub enum CompatMcpCommand {
    /// Record one endpoint as an ordinary unit. Does not start the server,
    /// read a credential or make a request.
    Import {
        /// JSON file: description, provenance_refs and a stdio or
        /// streamable_http connection. Credentials are environment-variable
        /// names, never values.
        file: PathBuf,
        /// Unit identity to install under, e.g. `mcp.example-search`.
        #[arg(long)]
        unit_id: String,
        /// Offer the connection to the subject after importing it.
        #[arg(long)]
        enabled: bool,
    },
}

#[cfg(feature = "compat-skill")]
#[derive(Subcommand, Debug)]
pub enum CompatSkillCommand {
    /// Copy one directory of Skill material into the installation. Symbolic
    /// links are refused; the original files and executable bits are kept.
    Import {
        root: PathBuf,
        #[arg(long)]
        unit_id: String,
        #[arg(long)]
        enabled: bool,
    },
    /// List the original members, or print one member's exact bytes.
    Read {
        unit_id: String,
        #[arg(long)]
        revision: String,
        /// Exact retained artifact digest. Required if a revision is ambiguous.
        #[arg(long)]
        artifact_digest: Option<String>,
        #[arg(long)]
        path: Option<String>,
        /// Byte offset with --path; member ordinal for a listing.
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// Return one bounded page (1..131072 bytes). Without this option,
        /// --path streams the complete remainder of the original file.
        #[arg(long)]
        limit: Option<u32>,
    },
}

#[derive(Subcommand, Debug)]
pub enum UpdateCli {
    /// Show the selector, the activation records and the slot inventory.
    Status,
    /// Build, copy, migrate and smoke a candidate generation.
    Prepare {
        /// The generation name the candidate becomes.
        #[arg(long)]
        generation: String,
        /// Build the candidate from this source directory.
        #[arg(long)]
        source_dir: Option<PathBuf>,
        /// Or accept an artifact that was built elsewhere.
        #[arg(long)]
        artifact: Option<PathBuf>,
        #[arg(long, default_value = "release")]
        profile: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        extra_args: Option<String>,
    },
    /// Swap the selector to a prepared activation. The one linearization
    /// point of a hand-over; the old generation's writers are fenced.
    Activate {
        #[arg(long)]
        activation: shenko_types::ids::EntityId,
        #[arg(long)]
        expected_selector_revision: u64,
    },
    /// Roll the control authority back; new facts of the rolled-back
    /// generation are imported into the old copy, never discarded.
    Rollback {
        #[arg(long)]
        activation: shenko_types::ids::EntityId,
        #[arg(long)]
        expected_selector_revision: u64,
    },
    /// Deterministic recovery read. No model is required.
    Recover,
}

impl From<UpdateCli> for shenko_update::cli::UpdateCommand {
    fn from(command: UpdateCli) -> Self {
        use shenko_update::cli::UpdateCommand as UpdateCommand;
        match command {
            UpdateCli::Status => UpdateCommand::Status,
            UpdateCli::Prepare {
                generation,
                source_dir,
                artifact,
                profile,
                target,
                extra_args,
            } => UpdateCommand::Prepare {
                generation,
                source_dir,
                artifact,
                profile,
                target,
                extra_args,
            },
            UpdateCli::Activate {
                activation,
                expected_selector_revision,
            } => UpdateCommand::Activate {
                activation,
                expected_selector_revision,
            },
            UpdateCli::Rollback {
                activation,
                expected_selector_revision,
            } => UpdateCommand::Rollback {
                activation,
                expected_selector_revision,
            },
            UpdateCli::Recover => UpdateCommand::Recover,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ContextCommand {
    /// The installed context strategies and their installation record.
    Policy {
        #[command(subcommand)]
        command: ContextPolicyCommand,
    },
    /// The committed window chain of the subject scope.
    Windows {
        #[arg(long, default_value_t = 20)]
        limit: u64,
 },
}

#[derive(Subcommand, Debug)]
pub enum ContextPolicyCommand {
    /// Every strategy this product can install, its installation record and
    /// the order the runtime tries them in.
    List,
    /// Install a strategy if it is missing and enable it.
    Enable { id: String },
    /// Disable a strategy: the record stays, the strategy stops running.
    Disable { id: String },
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Grant admin to a person or a platform account. The decision is a
    /// durable fact with the deciding human recorded on it.
    Grant {
        #[arg(long, conflicts_with = "platform_account")]
        person: Option<shenko_types::ids::EntityId>,
        /// provider:account:tenant:platform_user_id (empty tenant: a::b)
        #[arg(long, conflicts_with = "person")]
        platform_account: Option<String>,
        /// The human making this decision; recorded on the fact.
        #[arg(long)]
        granted_by: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Revoke a previously granted admin fact (owner only).
    Revoke {
        #[arg(long, conflicts_with = "platform_account")]
        person: Option<shenko_types::ids::EntityId>,
        #[arg(long, conflicts_with = "person")]
        platform_account: Option<String>,
        #[arg(long)]
        granted_by: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// List every admin fact of this instance.
    List,
}

#[derive(Subcommand, Debug)]
pub enum ContactsCommand {
    /// Import the owner's own Life identity material: the people catalog
    /// (catalog/people) and/or a voiceprint rank export. Only identifiers
    /// and statuses are read; encrypted batches, embeddings and audio are
    /// never touched, and unknown fields fail the import.
    LifeImport {
        /// Path of the local `life` checkout.
        #[arg(long)]
        life_dir: PathBuf,
        /// Optional JSON export of voiceprint rank records.
        #[arg(long)]
        ranks: Option<PathBuf>,
        /// Parse and report only; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Read-only lookup of the local person directory.
    Lookup {
        #[arg(long, conflicts_with_all = ["platform_account", "alias"])]
        person: Option<shenko_types::ids::EntityId>,
        /// provider:account:tenant:platform_user_id (empty tenant: a::b)
        #[arg(long, conflicts_with_all = ["person", "alias"])]
        platform_account: Option<String>,
        #[arg(long, conflicts_with_all = ["person", "platform_account"])]
        alias: Option<String>,
    },
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

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    /// List installed tool records and the honest status of every declared
    /// named target (toybox, gcc-wasm, git, gh, tea, ssh).
    List,
    /// Install a real WASI artifact under a name. `--abi wasi_preview1`
    /// requires the module to actually import only wasi_snapshot_preview1 and
    /// export `_start`+`memory`; any other declared ABI is catalogued but not
    /// executable in this runtime.
    Install {
        /// Host path of the artifact (.wasm binary module).
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        name: String,
        #[arg(long)]
        version: String,
        /// wasi_preview1 | wasi_component | wasix | host_imports | other
        #[arg(long)]
        abi: String,
        /// Free-form entrypoint label (e.g. /tools/toybox); defaults to /tools/<name>.
        #[arg(long)]
        entrypoint: Option<String>,
        /// Upstream origin URI (informational provenance).
        #[arg(long)]
        source_uri: Option<String>,
        /// Upstream release tag (e.g. v76.0.0).
        #[arg(long)]
        source_release: Option<String>,
        /// e.g. "upstream_release" or "source_build:wasi-sdk-34.0".
        #[arg(long)]
        provenance: Option<String>,
        /// Expected blake3:<hex> digest; install aborts on mismatch.
        #[arg(long)]
        expected_digest: Option<String>,
        /// Upstream receipt digest (sha256) of the artifact. Provenance only:
        /// it never becomes the unit's identity.
        #[arg(long)]
        source_sha256: Option<String>,
        /// SPDX license identifier of the upstream project.
        #[arg(long)]
        license: Option<String>,
    },
    /// Inspect a real artifact without installing it: the ABI and the imports
    /// are read from the module's actual bytes, never from its name.
    Inspect {
        /// Host path of the artifact (.wasm binary module).
        #[arg(long)]
        file: PathBuf,
    },
    /// Build a tool-set manifest from a port build receipt. The receipt's
    /// per-artifact sha256 digests become the manifest's upstream
    /// provenance; the manifest also pins each artifact's blake3 identity,
    /// which is the digest the installation binds.
    Manifest {
        /// The build receipt written by the port recipe.
        #[arg(long)]
        receipt: PathBuf,
        /// Directory holding the produced artifacts.
        #[arg(long)]
        artifacts: PathBuf,
        /// Set name, for example toybox-wasi.
        #[arg(long)]
        set: String,
        /// Upstream release, for example 0.8.15.
        #[arg(long)]
        version: String,
        /// Prefix of the installation names, for example toybox.
        #[arg(long)]
        name_prefix: String,
        /// SPDX license identifier of the upstream project.
        #[arg(long)]
        license: String,
        /// Path of the license text shipped with the port.
        #[arg(long)]
        license_file: String,
        /// Upstream project URL.
        #[arg(long)]
        source_uri: String,
        /// How the artifacts were obtained, for example
        /// source_build:wasi-sdk-34.0.
        #[arg(long)]
        provenance: String,
        /// Compatibility class of the artifacts (wasi_preview1, ...).
        #[arg(long)]
        abi: String,
        /// Write the manifest to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Install every artifact of a tool-set manifest. Each artifact's
    /// receipt digest is re-verified against the file before it is admitted.
    InstallSet {
        /// The tool-set manifest.
        #[arg(long)]
        manifest: PathBuf,
        /// Directory holding the produced artifacts.
        #[arg(long)]
        artifacts: PathBuf,
    },
    /// Execute an installed tool with real argv/env/stdio under the managed
    /// mounts. Exit code is reported exactly; tool names resolve against the
    /// installed registry only — host binaries are never substituted.
    Exec {
        /// name or name@version
        #[arg(long)]
        tool: String,
        /// Grant only read permissions on the admitted home/memory mounts.
        /// This is an actual guest capability restriction, not an effect label.
        #[arg(long)]
        read_only: bool,
        /// Guest argv after argv[0].
        #[arg(last = true)]
        args: Vec<String>,
        /// KEY=VALUE guest environment entries (repeatable).
        #[arg(long = "env")]
        env: Vec<String>,
        /// Host file whose bytes become guest stdin (default: empty).
        #[arg(long)]
        stdin_file: Option<PathBuf>,
        /// Host file receiving guest stdout (default: this process's stdout).
        #[arg(long)]
        stdout_file: Option<PathBuf>,
        /// Host file receiving guest stderr (default: this process's stderr).
        #[arg(long)]
        stderr_file: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum RuntimeCommand {
    /// Ask the current subject to pause after a finite step.
    Pause {
        /// Stable ID for recovery after an uncertain write; generated if omitted.
        #[arg(long)]
        request_id: Option<shenko_types::ids::EntityId>,
    },
    /// Read the durable outcome of a previously issued request.
    Status { request_id: shenko_types::ids::EntityId },
    /// Cancel an unaccepted request or resume the same controller.
    Resume { request_id: shenko_types::ids::EntityId },
}

/// The native definitions this runtime installs, and the self-knowledge
/// provider they feed.
struct InstalledDefinitions {
    native_child: Arc<shenko_subject::NativeChildService>,
    self_knowledge: Arc<shenko_awareness_core::SelfKnowledgeProvider>,
    handoff_participants: Arc<shenko_update::handoff::HandoffParticipantRegistry>,
    artifact_services: Arc<shenko_ui::playground_backend::ArtifactServiceController>,
}

/// Install the native task definitions of this runtime.
///
/// Every definition is a real Cordis plugin with a real generation lifetime:
/// it registers itself in the live plugin registry while it is installed and
/// withdraws that registration in the same teardown. The function is the one
/// installation path — the running agent and the discovery command install
/// exactly the same definitions, so the advertised capabilities are the ones
/// that actually exist.
async fn install_native_definitions(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::store::Store>,
    data_dir: &std::path::Path,
    wasi_mounts: Vec<shenko_wasm::MountSpec>,
) -> Result<InstalledDefinitions> {
    let handoff_participants = Arc::new(
        shenko_update::handoff::HandoffParticipantRegistry::new(),
    );
    handoff_participants.bind_subject_driver(false);
    // Native finite children use the same Cordis root and persistent
    // ledger. Fixed native definitions retain their original handlers;
    // the finite model handler is installed with the shared broker below.
    let _native_child_plugin = supervisor
        .spawn(
            shenko_subject::NativeChildPlugin,
            (
                Arc::clone(store),
                shenko_types::ids::RuntimeGeneration::default(),
            ),
        )
        .await?;
    let printf_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(Arc::clone(store)));
    let _native_printf_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, printf_handler)
        .await?;
    let history_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HistoryHandler::new(Arc::clone(store)));
    let _native_history_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, history_handler)
        .await?;
    // Reachability is observed, not assumed: one bounded probe of a declared
    // endpoint with per-stage measured timings and no persistent state.
    let net_probe_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NetProbeHandler::new(Arc::clone(store)));
    let _native_net_probe_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, net_probe_handler)
        .await?;
    let memory_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::MemoryHandler::new(Arc::clone(store)));
    let _native_memory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, memory_handler)
        .await?;
    for handler in shenko_exec_host::research_handlers(Arc::clone(store)) {
        supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    }
    // Runtime-error awareness — SPEC §45.14: errors.read exposes the
    // subject's own committed runtime_error facts; issue.submit writes
    // subject-authored drafts into the local issues outbox only.
    let errors_read_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::ErrorsReadHandler::new(Arc::clone(store)));
    let _native_errors_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, errors_read_handler)
        .await?;
    let issue_submit_handler: Arc<dyn shenko_subject::NativeTaskHandler> = Arc::new(
        shenko_exec_host::IssueSubmitHandler::new(
            Arc::clone(store),
            data_dir.join("issues"),
        ),
    );
    let _native_issue_submit_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, issue_submit_handler)
        .await?;
    // Person-directory operations share the live native registry; owner-set
    // admin grants remain outside the subject's task surface.
    let person_directory_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::PersonDirectoryHandler::new(Arc::clone(store)));
    let _native_person_directory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, person_directory_handler)
        .await?;
    let media_read_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_media::MediaReadHandler::new(Arc::clone(store)));
    let _media_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, media_read_handler)
        .await?;
    // Built-in, replaceable self-explanation — SPEC §38, R124. The
    // provider is seeded from compiled-in material now; the real
    // `self://runtime/capabilities` snapshot is installed below once
    // the native and awareness registries are populated.
    let self_knowledge = Arc::new(shenko_awareness_core::SelfKnowledgeProvider::new(
        option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound"),
        "0.6.2",
    ));
    let self_knowledge_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::SelfKnowledgeHandler::new(
            Arc::clone(store),
            Arc::clone(&self_knowledge),
        ));
    let _native_self_knowledge_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            self_knowledge_handler,
        )
        .await?;
    let artifact_services = Arc::new(
        shenko_ui::playground_backend::ArtifactServiceController::new(
            Arc::clone(store),
            data_dir.join("playground"),
        ),
    );
    handoff_participants.register(Arc::new(update_runtime::PlaygroundServices(
        Arc::clone(&artifact_services),
    )));
    let playground_handler: Arc<dyn shenko_subject::NativeTaskHandler> = Arc::new(
        shenko_ui::PlaygroundBackendHandler::new(Arc::clone(store), data_dir.join("playground"))
            .with_services(Arc::clone(&artifact_services)),
    );
    let _native_playground_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, playground_handler)
        .await?;
    // Desktop and VM handlers share one provider registry, lease authority
    // and session state across tasks.
    let computer_use_backend =
        Arc::new(shenko_exec_host::ComputerUseBackend::new(Arc::clone(store)));
    let computer_use_handlers: [Arc<dyn shenko_subject::NativeTaskHandler>; 6] = [
        Arc::new(shenko_exec_host::ComputerUseSessionHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseObserveHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseLeaseHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseInputHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseCapabilitiesHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
        Arc::new(shenko_exec_host::ComputerUseVmHandler::new(Arc::clone(
            &computer_use_backend,
        ))),
    ];
    for handler in computer_use_handlers {
        let _computer_use_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    let browser_provider = Arc::new(shenko_exec_host::BrowserProvider::new(
        Arc::clone(store),
        data_dir.join("browser"),
    ));
    computer_use_backend.register_human_input_observer(browser_provider.clone());
    let browser_handlers: [Arc<dyn shenko_subject::NativeTaskHandler>; 5] = [
        Arc::new(shenko_exec_host::browser::handlers::BrowserSessionHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserPageHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserActionHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserControlHandler::new(
            Arc::clone(&browser_provider),
        )),
        Arc::new(shenko_exec_host::browser::handlers::BrowserIdentityHandler::new(
            Arc::clone(&browser_provider),
        )),
    ];
    for handler in browser_handlers {
        let _browser_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    let mobile_bridge: Arc<dyn shenko_mobile::DeviceBridge> = {
        #[cfg(target_os = "android")]
        {
            Arc::new(shenko_mobile::SocketBridge::new(data_dir))
        }
        #[cfg(not(target_os = "android"))]
        {
            Arc::new(shenko_mobile::UnavailableBridge::new())
        }
    };
    let mobile_backend = Arc::new(shenko_mobile::AndroidBackend::new(
        Arc::clone(store),
        mobile_bridge,
        data_dir.to_path_buf(),
    ));
    let mobile_handlers: [Arc<dyn shenko_subject::NativeTaskHandler>; 6] = [
        Arc::new(shenko_mobile::AndroidDeviceProbeHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidScreenObserveHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidControlLeaseHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidControlInputHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidShellExecHandler::new(Arc::clone(&mobile_backend))),
        Arc::new(shenko_mobile::AndroidAppDbSnapshotHandler::new(Arc::clone(&mobile_backend))),
    ];
    for handler in mobile_handlers {
        let _mobile_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    // Real named-WASM-tool execution: wasi.exec resolves this
    // installation's own unit inventory and runs their verified
    // artifact bytes in the real WasmRuntime under the home mounts.
    let wasi_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::WasiExecHandler::new(
            Arc::clone(store),
            wasi_mounts,
            data_dir.to_path_buf(),
        ));
    let _native_wasi_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, wasi_handler)
        .await?;
    // Stateful sessions and journaled cells share one session owner and the
    // same native registry used by runtime discovery.
    let repl_session_handler = shenko_repl::task::ReplSessionHandler::new(
        Arc::clone(store),
        data_dir.join("repl"),
    )?;
    let repl_sessions = repl_session_handler.sessions();
    handoff_participants.register(Arc::new(update_runtime::ReplSessions(
        Arc::clone(&repl_sessions),
    )));
    let repl_cell_handler = shenko_repl::task::ReplCellHandler::new(
        Arc::clone(store),
        repl_sessions,
    );
    let repl_session_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(repl_session_handler);
    let repl_cell_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(repl_cell_handler);
    let _repl_session_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, repl_session_handler)
        .await?;
    let _repl_cell_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, repl_cell_handler)
        .await?;
    install_node_capability_reader(
        supervisor,
        Arc::clone(store),
        node_sync::open_node(data_dir, store)?,
    )
    .await?;
    install_node_selection_recorder(
        supervisor,
        Arc::clone(store),
        node_sync::open_node(data_dir, store)?,
    )
    .await?;
    // SSH PTY and seat question/answer operations share one live seat owner.
    let seat_runtime = Arc::new(shenko_exec_ssh::seat::runtime::SeatRuntime::new(Arc::clone(store)));
    let seat_ssh_handlers: [Arc<dyn shenko_subject::NativeTaskHandler>; 4] = [
        Arc::new(shenko_exec_ssh::SshExecHandler::new(Arc::clone(store))),
        Arc::new(shenko_exec_ssh::SshPtyHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::SeatObserveHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::SeatRespondHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
    ];
    for handler in seat_ssh_handlers {
        let _seat_ssh_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    let native_child = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()?;
    handoff_participants.register_admission_barrier(Arc::new(update_runtime::NativeWorkers(
        Arc::clone(&native_child),
    )));
    let agent_id = store.get_existing_agent_id()?.context("no initialized subject")?;
    let update_handler: Arc<dyn shenko_subject::NativeTaskHandler> = Arc::new(
        shenko_update::native::UpdateControlHandler::new(
            Arc::clone(store),
            data_dir.to_path_buf(),
            Arc::clone(&handoff_participants),
            Some(Arc::new(update_runtime::NodeAuthority {
                data_dir: data_dir.to_path_buf(),
                store: Arc::clone(store),
                agent_id,
            })),
            Arc::new(update_control::LiveControl(data_dir.to_path_buf())),
        ),
    );
    let _native_update_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, update_handler)
        .await?;
    Ok(InstalledDefinitions {
        native_child,
        self_knowledge,
        handoff_participants,
        artifact_services,
    })
}

/// Dispatch the runtime's own changes into the live hook graph.
///
/// The registry publishes a revision whenever a provider, a capability or a
/// help document appears or disappears. The graph dispatches those events at
/// this boundary, so a plugin that subscribed to `runtime.capability_changed`
/// observes the real installation instead of a startup snapshot. The task is
/// event driven: it waits for a revision, it does not poll.
fn spawn_runtime_hook_pump(
    supervisor: &shenko_kernel::Supervisor,
    cancel: &tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut revisions = supervisor.runtime.changed();
    let hooks = supervisor.hooks.clone();
    let cancel = cancel.clone();
    tokio::spawn(async move {
        loop {
            for dispatch in hooks.drain_pending().await {
                tracing::debug!(
                    point = %dispatch.point,
                    results = dispatch.results.len(),
                    "runtime change dispatched into the hook graph"
                );
            }
            // Wait for the next registry revision or the shutdown signal;
            // the pump never blocks teardown.
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = revisions.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
    })
}
/// Parse the process CLI and enter the same owned host used by embeddings.
pub fn run_cli() -> Result<()> {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_writer(std::io::stderr)
        .with_target(true)
        .compact()
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
    if let Some(output) = configured_ssh_public_identity_output(&command)? {
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
        create_data_directory(&data_dir)?;
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
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(run(command, data_dir));
    drop(runtime);
    drop(owner);
    result
}

fn configured_ssh_public_identity_output(command: &Command) -> Result<Option<String>> {
    let Command::SshPublicIdentity { installation, target } = command else { return Ok(None); };
    let observed = shenko_exec_ssh::NativeSshFileTargetConfig::inspect_public_identity(installation, target)?;
    Ok(Some(serde_json::to_string_pretty(&observed)?))
}

async fn run(command: Command, data_dir: PathBuf) -> Result<()> {
    // Reuse the actual published C03 host: one native root, one install.
    let supervisor =
        shenko_kernel::supervisor::Supervisor::new(shenko_types::ids::RuntimeGeneration::default());
    // The runtime's own services first: the live plugin registry and the
    // hook graph. Every plugin below resolves them through the Cordis service
    // lookup, so registration and withdrawal happen on the real lifecycle
    // instead of on a startup snapshot.
    let _runtime_services = supervisor.publish_services().await?;
    let policy: Arc<dyn shenko_identity::IdentityPolicy> =
        Arc::new(shenko_identity::DefaultIdentityPolicy);
    let _identity_plugin = supervisor
        .spawn(shenko_identity::IdentityPlugin, policy)
        .await?;
    let identity_context = supervisor.context().clone();
    let identity_service = identity_context.try_service::<IdentityService>()?;
    let result: Result<()> = async {
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
            let wasi_mounts = prepared_home.manifest.mounts.clone();
            drop(prepared_home);
            let installed = install_native_definitions(
                &supervisor,
                &store,
                &data_dir,
                wasi_mounts,
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
                &supervisor,
                Arc::clone(&store),
                discovery_node,
            )
            .await?;
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
        Command::Version => {
            let model = shenko_seed::ModelProfile::from_env();
            println!("Shenko（深刻）0.1.0  SPEC v0.6.2");
            println!("Model: {}", model.model.as_deref().unwrap_or("not_connected"));
            println!("Inference capacity: {:?}", model.capacity);
        }

        Command::Check => {
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
            let (installation_id, subject) = match store.get_existing_agent_id()? {
                Some(agent_id) => (
                    store.installation_id_for_subject(agent_id)?,
                    shenko_seed::BootstrapSubjectState::ExistingGenesis,
                ),
                None => (
                    store.ledger.ledger_id(),
                    shenko_seed::BootstrapSubjectState::PendingGenesis,
                ),
            };
            let prepared = bootstrap.prepare(installation_id)?;
            println!("Home manifest: {}", prepared.manifest_path.display());
            println!("Home: {}", serde_json::to_string_pretty(&prepared.manifest)?);
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
            port,
            name,
            napcat,
            napcat_account,
            napcat_access_token,
            napcat_admin_qq,
            napcat_file_root,
            napcat_file_host,
            napcat_cli_url,
            ssh_targets,
        } => {
            if napcat.is_some() {
                anyhow::ensure!(
                    napcat_account.is_some_and(|account| account > 0),
                    "NapCat persistent ingress requires --napcat-account or NAPCAT_BOT_QQ"
                );
            }
            info!(
                data_dir=%data_dir.display(), port,
                name=name.as_deref().unwrap_or("(unnamed)"),
                "Shenko starting"
            );

            let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
                data_dir: data_dir.clone(),
                agent_name: name.clone(),
                napcat_ws_url: napcat.clone(),
            });
            bootstrap.validate_target()?;
            let model_profile = shenko_seed::ModelProfile::from_env();

            // 1. Open persistent store and prepare the actual WASI home.
            //    The store lives in the live slot the durable pointer names
            //    (`versions/<gen>/`); a data dir without any slot published
            //    yet is itself the live slot (first run).
            create_data_directory_async(&data_dir).await?;
            let live_slot = shenko_update::slots::resolve_live_slot(&data_dir)?
                .unwrap_or_else(|| data_dir.clone());
            let db_path = live_slot.join(shenko_update::SLOT_DB_FILENAME);
            // The content store is shared by all generations.
            let cas_dir = data_dir.join("cas");
            let store = Arc::new(shenko_store::store::Store::open(db_path, cas_dir)?);
            info!(
                sqlite=%store.capabilities.sqlite_version,
                wal=store.capabilities.wal_mode,
                fts5=store.capabilities.fts5_available,
                "store ready"
            );

            let installation_id = match store.get_existing_agent_id()? {
                Some(agent_id) => store.installation_id_for_subject(agent_id)?,
                None => store.ledger.ledger_id(),
            };
            // Prepare the protected installation control channel before home
            // setup, connectors, or subject work can publish runtime effects.
            // Claiming is not a prerequisite for ordinary conversation.
            if let Some(path) = shenko_ui::owner::prepare_owner_claim(
                &store,
                &data_dir,
                installation_id,
            )? {
                info!(control_file=%path.display(), "owner claim available through the private installation control file");
            }
            let owner_state = shenko_ui::owner::OwnerState {
                store: Arc::clone(&store),
                installation_id,
                data_dir: data_dir.clone(),
            };
            let prepared_home = bootstrap.prepare(installation_id)?;
            info!(manifest=%prepared_home.manifest_path.display(), "WASM home ready");

            // 2. Bootstrap genesis if first run — SPEC §44.6. The genesis
            // closure references the actual home manifest and guest artifact.
            let agent_id = bootstrap_genesis(
                &store,
                name.as_deref(),
                &prepared_home.manifest,
                &model_profile,
            )?;
            // §45.14: kernel cordis-lifecycle and hook-graph failures journal
            // into the SAME subject store journal. Genesis is the earliest
            // truthful attach point — before it no branch exists, and the
            // kernel keeps its stderr+counter record instead of manufacturing
            // one.
            supervisor.attach_error_sink(Arc::new(StoreErrorSink {
                store: Arc::clone(&store),
                agent_id,
            }));
            // The wasi.exec handler keeps the mount closure and the data dir;
            // per-execution it reopens this installation's unit inventory so
            // installs made after startup are visible.
            let wasi_mounts = prepared_home.manifest.mounts.clone();
            drop(prepared_home);
            let identity = adopt_identity(&store, agent_id, name.as_deref(), &identity_service)?;
            info!(
                revision = identity.revision,
                name = identity.display_name.as_deref().unwrap_or("(unnamed)"),
                naming = ?identity.naming,
                "identity ready"
            );

            let runtime_instance = shenko_types::ids::EntityId::new();
            // The launch binds this process to the durable control
            // authority (the activation selector) before any subject step.
            let launch_receipt =
                shenko_seed::LaunchService::new(Arc::clone(&store), agent_id).resolve(
                    &shenko_seed::LaunchRequest {
                        launch_id: runtime_instance,
                        installation_id,
                        agent_id,
                        slot_dir: live_slot.clone(),
                        reason: "start".to_string(),
                        read_only: false,
                    },
                )?;
            info!(
                generation = %launch_receipt.generation.0,
                selector_revision = launch_receipt.selector_revision,
                selector_epoch = launch_receipt.selector_epoch,
                "launch bound to control authority"
            );
            // Deterministic recovery services remain available without an LLM.
            let cancel = tokio_util::sync::CancellationToken::new();
            let subject_inbox = Arc::new(shenko_subject::inbox::Inbox::new(1024));
            // Establish recovery consumers BEFORE ResultRecorded can take fast S1.
            store.start_subject_dispatcher()?;
            store.settle_subject_memory()?;
            store.reconcile_subject_publications()?;
            // Native finite children use the same Cordis root and persistent
            // ledger. Fixed native definitions retain their original handlers;
            // the finite model handler is installed with the shared broker below.
            let installed = install_native_definitions(
                &supervisor,
                &store,
                &data_dir,
                wasi_mounts.clone(),
            )
            .await?;
            if let Some(path) = ssh_targets {
                let ssh_node = node_sync::open_node(&data_dir, &store)?;
                install_native_ssh(&supervisor, Arc::clone(&store), ssh_node, &path).await?;
                info!("native SSH explicit installation ready; credentials are loaded only after task admission");
            }
            // The runtime's own changes become hook events from now on: the
            // installation above and every later registration are observed
            // through the live graph, not through a startup snapshot.
            let _runtime_hook_pump = spawn_runtime_hook_pump(&supervisor, &cancel);
            let native_child = installed.native_child.clone();
            let self_knowledge = installed.self_knowledge.clone();
            let handoff_participants = Arc::clone(&installed.handoff_participants);
            // Recovery belongs to the live runtime, never to capability discovery.
            native_child.recover()?;
            installed.artifact_services.recover_on_boot()?;
            let _update_control = update_control::spawn_control(
                data_dir.clone(),
                Arc::clone(&store),
                Arc::clone(&handoff_participants),
                cancel.clone(),
            )
            .await?;
            // Typed continuous-media ingress: one durable revision admission +
            // the bound subject inbox event, published on the same Cordis root
            // so real providers inject it (and the /api/media routes share it).
            // SPEC §15.2, §20. Transcript text still comes only from providers.
            let _media_ingress_plugin = supervisor
                .spawn(
                    shenko_media::MediaIngressPlugin,
                    (Arc::clone(&store), agent_id, subject_inbox.sender()),
                )
                .await?;
            // Resident memory worker — SPEC §12.2: accepted ≠ remembered.
            // It drains the durable memory intent queue and keeps the Git
            // text knowledge repository (data_dir/memory) in sync. The
            // repository is initialized here when the seed bootstrap has
            // not run yet (a copied data dir recovers the same way).
            let memory_knowledge = Arc::new(
                shenko_memory::GitKnowledgeStore::open(data_dir.join("memory"))?,
            );
            let _memory_worker_plugin = supervisor
                .spawn(
                    shenko_memory::MemoryWorkerPlugin,
                    (
                        agent_id,
                        Arc::clone(&store),
                        Some(memory_knowledge),
                    ),
                )
                .await?;
            let media_ingress = supervisor
                .context()
                .try_service::<shenko_media::MediaIngress>()?;
            // Media runtime behind the reserved slots: realtime sessions
            // against configured engine bindings (data_dir/media/
            // providers.json), recording capture with CAS closure, barge-in
            // and speech playback. SPEC §20. No engine configured is honest:
            // the ingress path still admits external provider facts.
            let media_providers =
                shenko_media::runtime::MediaProviders::load(&data_dir, Arc::clone(&store.cas))?;
            let _media_runtime_plugin = supervisor
                .spawn(
                    shenko_media::MediaRuntimePlugin,
                    (
                        Arc::clone(&media_ingress),
                        Arc::clone(&store),
                        media_providers,
                    ),
                )
                .await?;
            // Cognition units are installed capability, not kernel business
            // logic: perception, awareness, Skill and code share one
            // installation authority — the Node's unit store (SPEC §45). The
            // catalog is a read model over it; the shipped seeds plus any
            // authored packages under data_dir/cognition install through it.
            // Nothing here dispatches on keywords; every installed unit's
            // short card travels as bounded advisory text and the model reads
            // the body it actually wants at an exact version.
            let cognition_node = Arc::new(parking_lot::Mutex::new(node_sync::open_node(
                &data_dir,
                &store,
            )?));
            let cognition = Arc::new(shenko_awareness_core::CognitionCatalog::new(
                Arc::clone(&cognition_node),
            ));
            let cognition_root = shenko_awareness_core::cognition_package_root(&data_dir);
            // A shipped seed is bootstrap material, not authority over this
            // installation. A unit that is already installed keeps its own
            // version, its enable state and its observations.
            let installed_units: std::collections::HashSet<String> = cognition
                .units()?
                .into_iter()
                .map(|unit| unit.unit_id)
                .collect();
            for package in shenko_awareness_core::seed_packages()? {
                if installed_units.contains(&package.unit_id) {
                    info!(unit = %package.unit_id, revision = %package.revision, "cognition seed already installed; keeping the installation's own version");
                    continue;
                }
                cognition.install(&package, &cognition_root)?;
                info!(unit = %package.unit_id, revision = %package.revision, "cognition seed installed");
            }
            for unit in shenko_awareness_core::install_dir(&cognition, &cognition_root)? {
                info!(unit = %unit, "cognition unit installed");
            }
            // Legacy location kept for one release: packages authored under
            // data_dir/awareness install into the same authority.
            let legacy_awareness = data_dir.join("awareness");
            if legacy_awareness.is_dir() {
                for unit in shenko_awareness_core::install_dir(&cognition, &legacy_awareness)? {
                    info!(unit = %unit, "legacy awareness unit installed");
                }
            }
            // Optional ecosystem adapters. MCP and Agent Skills are two
            // independent units over the same installation authority: each is
            // resident only while its own unit is enabled, and neither brings
            // a third-party service, Skill, dependency or account with it.
            let _compat_plugins = compat::spawn_adapters(
                &supervisor,
                Arc::clone(&store),
                Arc::clone(&cognition_node),
            )
            .await?;
            // Exact-version unit reads and authored unit writes are native
            // children on the same Cordis root: the subject can inspect what
            // it has and write down what it actually learned, with the
            // judgement and evidence recorded as its own durable facts.
            let cognition_read_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::CognitionReadHandler::new(
                    Arc::clone(&store),
                    Arc::clone(&cognition),
                ));
            let _native_cognition_read_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    cognition_read_handler,
                )
                .await?;
            let cognition_write_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::CognitionWriteHandler::new(
                    Arc::clone(&store),
                    Arc::clone(&cognition),
                    cognition_root.clone(),
                    Some(Arc::clone(&self_knowledge)),
                ));
            let _native_cognition_write_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    cognition_write_handler,
                )
                .await?;
            // The discovery index and the registered evidence of one unit are
            // reads over the same installation authority: the model pages
            // through the cards and cites what this installation actually
            // recorded.
            let cognition_list_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::CognitionQueryHandler::list(
                    Arc::clone(&store),
                    Arc::clone(&cognition),
                ));
            let _native_cognition_list_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    cognition_list_handler,
                )
                .await?;
            let cognition_evidence_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::CognitionQueryHandler::evidence(
                    Arc::clone(&store),
                    Arc::clone(&cognition),
                ));
            let _native_cognition_evidence_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    cognition_evidence_handler,
                )
                .await?;
            // Record what actually registered so self-knowledge answers from
            // the real registries instead of a stale product feature list.
            self_knowledge.install_runtime_capabilities(
                &native_child.installed_definitions(),
                &cognition.units()?,
            );
            // The live agent-visible capability catalog names what this
            // process currently offers; the snapshot is a registry view, not
            // a product list. Providers that register later (e.g. the broker)
            // refresh the view with a second, idempotent call below.
            self_knowledge.install_agent_capabilities(&supervisor.runtime.agent_capabilities());
            // SPEC §38.6: the build manifest and the source index are runtime
            // facts of this artifact, not of the repository's main branch.
            self_knowledge.install_manifest_resource();
            if let Ok(source_root) = std::env::var("SHENKO_SOURCE_ROOT") {
                let revision = option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound");
                match self_knowledge.install_source_index(Path::new(&source_root), revision) {
                    Ok(indexed) => info!(indexed, source_root, "self-knowledge source index installed"),
                    Err(error) => tracing::warn!(
                        error = %error,
                        source_root,
                        "source index unavailable; self-knowledge reports the manifest only"
                    ),
                }
            }
            let consumer_cancel = tokio_util::sync::CancellationToken::new();
            let dispatch_store = Arc::clone(&store);
            let dispatch_native = Arc::clone(&native_child);
            let dispatch_results = model_profile.configured.then(|| subject_inbox.sender());
            let dispatch_stop = consumer_cancel.clone();
            let dispatch_subject_stopping = cancel.clone();
            let failure_cancel = cancel.clone();
            let dispatcher_task = tokio::spawn(async move {
                let result = shenko_subject::dispatch::run_committed_dispatcher(
                    dispatch_store,
                    dispatch_native,
                    agent_id,
                    dispatch_results,
                    dispatch_subject_stopping,
                    dispatch_stop,
                ).await;
                if result.is_err() { failure_cancel.cancel(); }
                result
            });
            let _consumer_stop_on_exit = consumer_cancel.clone().drop_guard();
            let replication_task = node_sync::spawn_runtime(
                Arc::clone(&store), data_dir.clone(), consumer_cancel.clone(),
            );
            if !model_profile.configured {
                node_sync::observe(&data_dir, &store, runtime_instance, None)?;
                if napcat.is_some() {
                    tracing::warn!("NapCat not started without a subject model; durable bootstrap remains available");
                }
                let owner_grant =
                    store.get_bootstrap_owner_grant(installation_id)?;
                let view = bootstrap.view(
                    installation_id,
                    shenko_seed::BootstrapSubjectState::ExistingGenesis,
                    model_profile.clone(),
                    owner_grant,
                );
                println!("Bootstrap: {}", serde_json::to_string_pretty(&view)?);
                let runtime_result =
                    run_deterministic_ui(port, store, data_dir, model_profile, cancel, native_child, owner_state).await;
                consumer_cancel.cancel();
                match tokio::time::timeout(std::time::Duration::from_secs(2), dispatcher_task).await {
                    Ok(result) => result??,
                    Err(_) => tracing::warn!("dispatcher stop pending; durable work retained"),
                }
                replication_task.await?;
                return runtime_result;
            }

            // Start inference only after the model-independent recovery services.
            let (context_head, _) = store.load_subject(agent_id)?;
            let context_registry = Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
                shenko_context_policy::WindowLedger::new(Arc::new(
                    shenko_context_policy::StoreWindowLog::new(
                        Arc::clone(&store), shenko_types::ids::EntityId(context_head.branch_id.0),
                    ),
                )),
            ));
            let _context_host = supervisor
                .spawn(shenko_context_policy::ContextPolicyHost, Arc::clone(&context_registry))
                .await?;
            // Prepared transient context contributions: producers (the PMA
            // intervention ledger, awareness perspectives, a plugin's own
            // notice) register here, and the request path collects what they
            // prepared. The registry holds no business content — it merges
            // by the kernel's one rule and records what was sent.
            let contribution_registry =
                Arc::new(shenko_context_policy::ContributionRegistry::new());
            let _contribution_host = supervisor
                .spawn(
                    shenko_context_policy::ContributionHost,
                    Arc::clone(&contribution_registry),
                )
                .await?;
            supervisor.spawn(
                shenko_memory::intervention_runtime::MemoryInterventionPlugin,
                shenko_memory::intervention_runtime::MemoryInterventionConfig {
                    store: Arc::clone(&store),
                    agent_id,
                    policy: Arc::new(shenko_memory::MemoryInterventionRegistry::new(Arc::new(
                        shenko_memory::ModelMemoryInterventionPolicy,
                    ))),
                    failure_cancel: cancel.clone(),
                },
            ).await?;
            // The Node installation records are the installation authority for
            // the context strategies: a strategy runs only while its unit is
            // enabled, with the configuration that unit carries. The
            // preference order follows the installation order of the enabled
            // units until the configuration service carries a value.
            let mut policy_node = node_sync::open_node(&data_dir, &store)?;
            let installed_policies = context_policy::reconcile_units(
                &mut policy_node,
                &data_dir.join("policy-units"),
            )?;
            drop(policy_node);
            let adopted = context_registry.apply_preference(
                context_policy::preference_order(&installed_policies),
            );
            info!(
                adopted = ?adopted.adopted,
                unknown = ?adopted.unknown,
                "context strategies installed from the unit records"
            );
            for policy in context_policy::enabled_policies(&installed_policies) {
                let runtime = context_policy::runtime_config(policy)
                    .map_err(anyhow::Error::msg)?;
                match runtime {
                    context_policy::PolicyRuntime::Budget(config) => {
                        let _budget_policy = supervisor
                            .spawn(shenko_compact_budget::BudgetCompactPlugin, config)
                            .await?;
                    }
                    context_policy::PolicyRuntime::Snap(config) => {
                        let _snap_policy = supervisor
                            .spawn(shenko_compact_snap::SnapCompactPlugin, config)
                            .await?;
                    }
                }
            }
            let (broker, broker_handle) = shenko_broker::broker::Broker::new()?;
            inference_runtime::install_runtime_error_sink(&broker_handle, Arc::clone(&store));
            handoff_participants.register(Arc::new(update_runtime::InferenceDomain(broker_handle.clone())));
            // Optional quota policy install (SPEC §42): the file holds one
            // serialized QuotaPolicy; a recorded policy makes every inference
            // request pass durable admission before dispatch. Unreadable or
            // invalid content fails startup loudly rather than silently
            // running unpriced.
            if let Ok(path) = std::env::var("SHENKO_QUOTA_POLICY") {
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| anyhow::anyhow!("SHENKO_QUOTA_POLICY unreadable: {error}"))?;
                let policy: shenko_quota::QuotaPolicy = serde_json::from_str(&text)
                    .map_err(|error| anyhow::anyhow!("SHENKO_QUOTA_POLICY invalid: {error}"))?;
                let receipt = store.record_quota_policy(&policy, chrono::Utc::now())?;
                info!(policy_id=%policy.policy_id, revision=policy.revision,
                    commit=%receipt.commit_ref, "quota policy recorded");
            }
            // Provider quota observers (SPEC §42.2/§42.6): the local
            // account configuration names the providers this node observes,
            // the real scope of each account and where its sources are read.
            // Each named provider installs its own observers; the runtime
            // reads the real payloads, records the observations and turns
            // significant changes into a bounded view the subject can read.
            // Without a configuration there is no observer and nothing is
            // collected — observation from ordinary traffic stays available
            // either way.
            let quota_runtime = shenko_subject::quota_runtime::QuotaRuntime::from_env(
                Arc::clone(&store),
                agent_id,
                Some(subject_inbox.sender()),
            )?;
            if let Some(quota_runtime) = quota_runtime {
                info!(providers = ?quota_runtime.providers(), "quota observers registered");
                let quota_stop = consumer_cancel.clone();
                tokio::spawn(async move {
                    Arc::new(quota_runtime).run(quota_stop).await;
                });
            }
            let capabilities = match broker.discover_capabilities().await {
                Ok(capabilities) => Some(capabilities),
                Err(error) => {
                    tracing::warn!(error=%error, "model metadata unavailable; automatic context compaction disabled");
                    None
                }
            };
            // §13.4/§41.9: the dispatch loop enforces the JOINT effective
            // window — the model ceiling, the backend's own observed pool and
            // the owner's configured limit — never one of them alone. Only
            // observed components are installed; a component that is not
            // observed makes the value an explicit upper bound, and with none
            // observed admission stays advisory.
            let effective_window = shenko_context_policy::WindowLimits {
                model_ceiling: capabilities
                    .as_ref()
                    .and_then(|capabilities| capabilities.max_context_tokens.map(u64::from)),
                // The KV residency slice reports the backend's observed pool.
                // A plain OpenAI-compatible endpoint reports none, so this
                // stays unknown instead of being inferred from the model.
                backend_pool: None,
                policy_limit: None,
            }
            .resolve();
            broker_handle.install_effective_window(effective_window.installable());
            info!(window=%effective_window.status(), "effective context window resolved");
            let model_child_handler: Arc<dyn shenko_subject::NativeTaskHandler> = Arc::new(
                shenko_subject::ModelChildHandler::new(Arc::clone(&store), broker_handle.clone(), supervisor.context().clone())?,
            );
            let _model_child_plugin = supervisor
                .spawn(shenko_subject::NativeTaskHandlerPlugin, model_child_handler)
                .await?;
            supervisor.spawn(
                shenko_subject::intervention::PmaEvaluatePlugin,
                (Arc::clone(&store), broker_handle.clone()),
            ).await?;
            // The measurement path of a cognition unit: a finite comparison of
            // the candidate against the same installation with the unit
            // switched off. It runs in throwaway sandboxes and its inference
            // goes through this broker, so the trial is metered like every
            // other inference.
            let cognition_trial_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                Arc::new(shenko_exec_host::CognitionTrialHandler::new(
                    Arc::clone(&store),
                    Arc::clone(&cognition),
                    broker_handle.clone(),
                    data_dir.join("cognition-trials"),
                ));
            let _native_cognition_trial_plugin = supervisor
                .spawn(
                    shenko_subject::NativeTaskHandlerPlugin,
                    cognition_trial_handler,
                )
                .await?;
            // ModelChildHandler reconstructed all outstanding original charges
            // before any subject/child inference can enter this broker.
            tokio::spawn(broker.run());
            // The broker and model-child registrations above joined the live
            // catalog; the self-knowledge snapshot is a registry view and the
            // re-render is idempotent, so refresh it once more here.
            self_knowledge.install_agent_capabilities(&supervisor.runtime.agent_capabilities());
            // The hook graph is the runtime's own service: the subject and
            // every plugin resolve the same live registry, so a handler a
            // plugin registered is visible to the subject's next step.
            let hook_registry = Arc::clone(&supervisor.hooks);
            // Outbound path: subject model output → UI timeline. SPEC §5.1, §21.12
            let (outbox_tx, _outbox_rx) =
                tokio::sync::broadcast::channel::<shenko_subject::outbox::OutboxMessage>(64);
            // The contacts query handler is only real while its connector is
            // configured; the plugin handle must outlive the napcat block.
            let mut _native_contacts_plugin = None;

            // The IM reply boundary ships as channel plugins: the live prompt
            // and parser advertise exactly the channels whose transport is
            // configured. With no NapCat endpoint no reply channel exists, so
            // the contract cannot promise an undeliverable reply. Plugins
            // register into this registry once their connector exists.
            let mut channel_registry = shenko_types::channel::ChannelRegistry::new();

            // 5. Start NapCat if configured — SPEC §16, §35.6
            if napcat.is_some() || napcat_cli_url.is_some() {
                let napcat_config = shenko_channel_napcat::config::NapCatConfig {
                    ws_url: napcat.clone().unwrap_or_default(),
                    access_token: napcat_access_token,
                    bot_qq: napcat_account.expect("NapCat account validated before startup"),
                    admin_qq: napcat_admin_qq.clone(),
                    file_root: napcat_file_root.clone(),
                    file_host: napcat_file_host.clone(),
                    ..Default::default()
                };
                // 独立接收域先落盘；打开失败停止启动，绝不降级到内存。
                let (napcat_client, mut napcat_event_rx, napcat_action_rx) =
                    shenko_channel_napcat::client::NapCatClient::open_persistent(
                        napcat_config.clone(),
                        data_dir.join("napcat-ingress.sqlite"),
                    )?;
                let client = Arc::new(napcat_client);
                let spool = client.spool.clone();
                let event_tx = client.event_sender();
                let pending_actions = client.pending_actions.clone();
                let cancel_napcat = consumer_cancel.clone();

                // Read-only contact/identity queries ride the same echo
                // protocol and durable task/evidence path as every native
                // child. Installing only while the connector exists keeps the
                // advertised capability honest: no NapCat, no definition.
                let contacts_handler: Arc<dyn shenko_subject::NativeTaskHandler> =
                    Arc::new(shenko_exec_host::NapCatContactsHandler::new(
                        Arc::clone(&store),
                        Arc::clone(&client),
                    ));
                _native_contacts_plugin = Some(
                    supervisor
                        .spawn(shenko_subject::NativeTaskHandlerPlugin, contacts_handler)
                        .await?,
                );

                // The QQ plugin owns classification, reply resolution,
                // description and dispatch; the Store owns the durable intent,
                // attempt and receipt. Binding the Store also installs the
                // host file capability: inbound attachment locators resolve
                // inside the configured NapCat-host media root and are sealed
                // by content digest.
                let qq_plugin: Arc<shenko_channel_napcat::NapCatChannel> = Arc::new(
                    shenko_channel_napcat::NapCatChannel::new(
                        napcat_config.clone(),
                        Arc::clone(&client),
                    )
                    .with_store(Arc::clone(&store)),
                );
                channel_registry.register(qq_plugin.clone()).context("register qq channel plugin")?;
                // Administration is an owner-set fact, recorded at startup and
                // never derived from a group role or the transport token.
                for admin in &napcat_config.admin_qq {
                    store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                        channel: shenko_channel_napcat::QQ_CHANNEL_ID.to_string(),
                        account: napcat_config.account_scope(),
                        kind: "admin_grant".to_string(),
                        state: "active".to_string(),
                        subject_ref: Some(format!("qq-admin:{admin}")),
                        detail: serde_json::json!({"admin_qq": admin, "source": "owner_config"}),
                    })?;
                }
                store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                    channel: shenko_channel_napcat::QQ_CHANNEL_ID.to_string(),
                    account: napcat_config.account_scope(),
                    kind: "health".to_string(),
                    state: "starting".to_string(),
                    subject_ref: None,
                    detail: serde_json::json!({
                        "ws_configured": napcat.is_some(),
                        "cli_source_configured": napcat_cli_url.is_some(),
                    }),
                })?;

                // A storage failure closes the connector and stops this runtime;
                // silently leaving an apparently healthy UI would hide lost reception.
                let connection_spool = Arc::clone(&spool);
                let connection_config = napcat_config.clone();
                let connection_cancel = cancel_napcat.clone();
                let connection_store = Arc::clone(&store);
                let connection_agent = agent_id;
                if napcat.is_some() {
                    tokio::spawn(async move {
                        if let Err(error) =
                            shenko_channel_napcat::client::NapCatClient::run_connection_loop(
                                connection_config,
                                event_tx,
                                napcat_action_rx,
                                pending_actions,
                                connection_spool,
                                connection_cancel.clone(),
                            )
                            .await
                        {
                            // The failure is a durable fact of the subject's
                            // history, not just a stderr line: journal it with
                            // the actual agent identity before stopping.
                            journal_channel_error(
                                &connection_store,
                                connection_agent,
                                "napcat_ingress_failed",
                                "shenko_channel_napcat::client",
                                error.to_string(),
                                serde_json::json!({}),
                            );
                            tracing::error!(err=%error, "NapCat ingress failed — stopping runtime for recovery");
                            connection_cancel.cancel();
                        }
                    });
                }

                // napcat-cli is a second SOURCE of the same QQ plugin: its
                // events enter the same spool, dedup by the same source
                // identity, and are classified by the same plugin.
                if let Some(cli_url) = napcat_cli_url.clone() {
                    let cli_spool = Arc::clone(&spool);
                    let cli_events = client.event_sender();
                    let cli_cancel = cancel_napcat.clone();
                    let cli_store = Arc::clone(&store);
                    let cli_agent = agent_id;
                    tokio::spawn(async move {
                        if let Err(error) = shenko_channel_napcat::NapCatCliSource::new(
                            cli_url,
                            napcat_config.bot_qq,
                        )
                        .run(cli_spool, cli_events, cli_cancel.clone())
                        .await
                        {
                            journal_channel_error(
                                &cli_store,
                                cli_agent,
                                "napcat_cli_source_failed",
                                "shenko_channel_napcat::napcat_cli",
                                error.to_string(),
                                serde_json::json!({}),
                            );
                            tracing::error!(err=%error, "napcat-cli source failed — stopping runtime for recovery");
                            cli_cancel.cancel();
                        }
                    });
                }

                // Keep the ingress identity across replay. L's committed S1 receipt
                // wins over a stale I acknowledgement mirror after a crash.
                let inbox_tx = subject_inbox.sender();
                let event_store = Arc::clone(&store);
                let event_spool = Arc::clone(&spool);
                let admission_plugin = Arc::clone(&qq_plugin);
                let admission_facts = Arc::clone(&store);
                let cancel_events = cancel_napcat.clone();
                tokio::spawn(async move {
                    let result: Result<()> = async {
                        loop {
                            let ev = tokio::select! {
                                ev = napcat_event_rx.recv() => match ev {
                                    Some(ev) => ev,
                                    None => break,
                                },
                                _ = cancel_events.cancelled() => break,
                            };
                            let event_id = ev.ingress_id.ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?;
                            if let Some(commit) = event_store.event_disposition_commit(agent_id, event_id)? {
                                event_spool.ack(event_id, &commit.to_string())?;
                                continue;
                            }
                            let mut inbox_ev = napcat_inbox_event(ev)?;
                            // Plugin-owned classification: the envelope, not
                            // the raw wire payload, is what the subject sees.
                            // A payload the plugin does not classify (notices,
                            // meta events) keeps the raw payload; the staged
                            // projection falls back to a plain header.
                            if let Some(mut envelope) = admission_plugin.classify(
                                shenko_channel_napcat::QQ_SOURCE,
                                &inbox_ev.payload,
                            ) {
                                // Resolve attachment locators on the NapCat
                                // host through the file capability: bytes are
                                // sealed into the store and reachable by
                                // digest. A locator that cannot be resolved
                                // stays a locator and is recorded as a
                                // subject-readable gap; the message itself
                                // is never dropped.
                                for index in 0..envelope.attachments.len() {
                                    let attachment = envelope.attachments[index].clone();
                                    match admission_plugin.resolve_attachment(&attachment) {
                                        Ok(resolved) => {
                                            if let Some(object) = resolved.object {
                                                envelope.attachments[index].object =
                                                    Some(object);
                                            }
                                        }
                                        Err(detail) => {
                                            admission_facts.record_channel_fact(
                                                &shenko_store::channel::NewChannelFact {
                                                    channel: shenko_channel_napcat::QQ_CHANNEL_ID
                                                        .to_string(),
                                                    account: envelope.account.clone(),
                                                    kind: "attachment_locator".to_string(),
                                                    state: "pending".to_string(),
                                                    subject_ref: Some(event_id.to_string()),
                                                    detail: serde_json::json!({
                                                        "attachment_id": attachment.attachment_id,
                                                        "detail": detail,
                                                    }),
                                                },
                                            )?;
                                        }
                                    }
                                }
                                inbox_ev.payload = serde_json::to_value(&envelope)?;
                            }
                            tokio::select! { biased;
                                _ = cancel_events.cancelled() => break,
                                sent = inbox_tx.send(inbox_ev) => {
                                    if let Err(error) = sent {
                                        // The spool retains the event, but a
                                        // lost live receiver is still a fault.
                                        if cancel_events.is_cancelled() { break; }
                                        return Err(anyhow::Error::new(error)
                                            .context("subject channel inbox closed before shutdown"));
                                    }
                                }
                            }
                        }
                        Ok(())
                    }.await;
                    if let Err(error) = result {
                        journal_channel_error(
                            &event_store,
                            agent_id,
                            "napcat_admission_failed",
                            "shenko::channel_admission",
                            error.to_string(),
                            serde_json::json!({}),
                        );
                        tracing::error!(err=%error, "NapCat admission failed — ingress remains pending");
                        cancel_events.cancel();
                    }
                });

                // Poll bounded pages: cross-node watchers and one-shot callbacks
                // are not recovery truth. No S1 receipt means no ACK, even if delivered.
                let ack_store = Arc::clone(&store);
                let cancel_acks = cancel_napcat.clone();
                let ack_agent = agent_id;
                tokio::spawn(async move {
                    let result: Result<()> = async {
                        let mut after = 0;
                        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
                        loop {
                            tokio::select! {
                                _ = tick.tick() => {},
                                _ = cancel_acks.cancelled() => break,
                            }
                            let page = spool.pending_after(after, 256)?;
                            if page.is_empty() {
                                after = 0;
                            }
                            for event in page {
                                after = event.seq;
                                if let Some(commit) =
                                    ack_store.event_disposition_commit(agent_id, event.spool_id)?
                                {
                                    spool.ack(event.spool_id, &commit.to_string())?;
                                }
                            }
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = result {
                        journal_channel_error(
                            &ack_store,
                            ack_agent,
                            "napcat_ack_mirror_failed",
                            "shenko::channel_ack_mirror",
                            error.to_string(),
                            serde_json::json!({}),
                        );
                        tracing::error!(err=%error, "NapCat acknowledgement mirror failed — reopen required");
                        cancel_acks.cancel();
                    }
                });

                // Outbound publications across every registered channel: the
                // model emits an explicit target so we never guess who to
                // answer. T1 (arm_publication) claims the intent and arms the
                // durable operation before the network is touched; the
                // provider's answer is E1 evidence. A target no installed
                // plugin serves is refused as never dispatched — durable,
                // effect none, and it never cancels the runtime.
                // SPEC §35.6, §41.4 X03, §41.6.
                let reply_registry = channel_registry.clone();
                let reply_store = Arc::clone(&store);
                let reply_agent = agent_id;
                let cancel_replies = consumer_cancel.clone();
                let reply_failure = cancel.clone();
                tokio::spawn(async move {
                    let result: Result<()> = async {
                    let mut tick=tokio::time::interval(std::time::Duration::from_millis(250));
                    loop {
                        tokio::select! { biased;
                            _ = cancel_replies.cancelled() => break,
                            _ = tick.tick() => {}
                        }
                        // The ready index owns the work.
                        for publication in reply_store.ready_publications(32)? {
                            let intent_id = publication.intent_id;
                            let Some(plugin) = reply_registry.channel(&publication.target.channel) else {
                                // Never dispatched: durable refusal, runtime
                                // keeps running.
                                if let Err(error) = reply_store.refuse_publication(
                                    intent_id,
                                    "no installed channel serves target.channel",
                                ) {
                                    tracing::warn!(err=%error, %intent_id, "publication refusal not committed");
                                }
                                continue;
                            };
                            let dispatch = match plugin.describe(&publication.target) {
                                Ok(dispatch) => dispatch,
                                Err(error) => {
                                    if let Err(refusal_error) =
                                        reply_store.refuse_publication(intent_id, &error.to_string())
                                    {
                                        tracing::warn!(err=%refusal_error, %intent_id, "publication refusal not committed");
                                    }
                                    continue;
                                }
                            };
                            // T1: claim + arm before the network. None means
                            // another dispatcher owns it — never a second send.
                            let Some(armed) = reply_store.arm_publication(intent_id, &dispatch)? else {
                                continue;
                            };
                            let mut dispatch = dispatch;
                            dispatch.echo = Some(armed.operation_id.to_string());
                            let outcome = match plugin.dispatch(&dispatch, &publication.content).await {
                                Ok(receipt) => {
                                    reply_store.record_publication_receipt(&armed, &receipt)?;
                                    if receipt.accepted { "accepted" } else { "rejected" }
                                }
                                Err(
                                    error @ (shenko_types::channel::ChannelError::Transport { .. }
                                    | shenko_types::channel::ChannelError::Unknown { .. }),
                                ) => {
                                    // The request may have been executed. The
                                    // effect is unknown; reconciliation
                                    // settles it and never blind-resends.
                                    let detail = error.to_string();
                                    reply_store.record_publication_unknown(&armed, &detail)?;
                                    "unknown"
                                }
                                Err(error) => {
                                    // Deterministic refusal after arming
                                    // (unsupported/unresolved/rejected): the
                                    // effect is none and the operation is
                                    // resolved failed.
                                    let receipt = shenko_types::channel::ChannelReceipt {
                                        status: "rejected".to_string(),
                                        retcode: -1,
                                        accepted: false,
                                        provider_message_id: None,
                                        raw: serde_json::json!({"error": error.to_string()}),
                                        observed_at: chrono::Utc::now(),
                                    };
                                    reply_store.record_publication_receipt(&armed, &receipt)?;
                                    "refused"
                                }
                            };
                            reply_store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                                channel: publication.target.channel.clone(),
                                account: armed.account.clone(),
                                kind: "publication_outcome".to_string(),
                                state: outcome.to_string(),
                                subject_ref: Some(intent_id.to_string()),
                                detail: serde_json::json!({
                                    "operation_id": armed.operation_id.to_string(),
                                    "kind": publication.target.kind,
                                }),
                            })?;
                        }
                    }
                    Ok(())
                    }.await;
                    if let Err(error) = result {
                        journal_channel_error(
                            &reply_store,
                            reply_agent,
                            "channel_publication_consumer_failed",
                            "shenko::channel_publication_consumer",
                            error.to_string(),
                            serde_json::json!({}),
                        );
                        tracing::error!(err=%error, "publication consumer stopped; reopen writer and reconcile");
                        reply_failure.cancel();
                    }
                });
                info!("NapCat connector started");
            }

            // Built after any connector-scoped handler install so the sealed
            // tool contract advertises exactly the installed definitions.
            let loop_state = shenko_subject::loop_runner::SubjectLoopState::new(
                agent_id,
                Arc::clone(&subject_inbox),
                Arc::clone(&store),
                identity_context.clone(),
                channel_registry,
            )?
            .with_broker(broker_handle)
            .with_model_capabilities(capabilities.clone())
            .with_native_capabilities(native_child.installed_prompt_sections())
            .with_awareness(cognition)
            .with_hooks(Arc::clone(&hook_registry))
            .with_outbox(outbox_tx.clone());

            // Connector/ACK storage failures also stop admission in the subject.
            let connector_failure = cancel.clone();
            let connector_stop = consumer_cancel.clone();
            tokio::spawn(async move { connector_stop.cancelled().await; connector_failure.cancel(); });
            let cancel_subject = cancel.clone();
            // Verified consensus before the subject loop claims any domain
            // authority: only a committed election claim fences the Primary
            // observation. Without a reachable quorum the node opens an
            // availability branch and starts without a primary claim.
            let primary_fence = tokio::task::spawn_blocking({
                let data_dir = data_dir.clone();
                let store = Arc::clone(&store);
                let cancel = cancel.clone();
                move || node_sync::establish_primary(&data_dir, &store, &cancel)
            })
            .await??;
            let subject_driver = update_runtime::SubjectDriver::new(Arc::clone(&handoff_participants));
            let mut subject_task = tokio::spawn(async move {
                let _subject_driver = subject_driver;
                shenko_subject::loop_runner::run_subject_loop(loop_state, cancel_subject).await
            });
            node_sync::observe(&data_dir, &store, runtime_instance, primary_fence.as_ref())?;
            info!("subject loop started");

            // The configuration service: discover/read/preview/apply over the
            // durable config tables. Namespaces register their handlers on
            // this service; without a handler a namespace is not advertised.
            let config_service = Arc::new(shenko_ui::config_service::ConfigService::new(Arc::clone(&store)));

            // 7. Start WebUI
            let ui = shenko_ui::server::UiServer::new(port)
                .with_native(Arc::clone(&native_child))
                .with_inbox(subject_inbox.sender())
                .with_outbox(outbox_tx)
                .with_store(Arc::clone(&store))
                .with_media_ingress(Arc::clone(&media_ingress))
                .with_media_runtime(
                    supervisor
                        .context()
                        .try_service::<shenko_media::runtime::MediaRuntime>()?,
                )
                .with_context(Arc::clone(&context_registry), capabilities)
                .with_data_dir(data_dir.clone())
                .with_owner(owner_state)
                .with_config(config_service)
                .with_artifact_services(Arc::clone(&installed.artifact_services))
                .with_inference_status(model_profile.model.clone(), model_profile.configured);
            println!("Shenko UI: http://{}", ui.bind);
            println!("  Subject task: supervised (durable boundary pause may be active)");
            println!(
                "  Inference: {} (configured; probe/capability evidence separate)",
                model_profile.model.as_deref().unwrap_or("not_connected")
            );

            // Supervise the actual subject task, including panics. Stop claiming
            // availability before waiting for the current durable boundary.
            let live = ui.ws_state().live;
            let mut subject_finished = false;
            let runtime_result: Result<()> = tokio::select! {
                result = &mut subject_task => {
                    subject_finished = true;
                    match result {
                        Ok(Err(error)) => Err(error.into()),
                        Err(error) => Err(error.into()),
                        Ok(Ok(())) => Err(anyhow::anyhow!("subject stopped unexpectedly")),
                    }
                }
                result = ui.run() => result,
                result = tokio::signal::ctrl_c() => {
                    result?;
                    info!("shutdown signal; settling subject at a recoverable boundary");
                    Ok(())
                }
                _ = cancel.cancelled() => Err(anyhow::anyhow!("runtime stopped after connector failure; reopen for recovery")),
            };
            live.set_wired(false);
            // Signal expected delivery shutdown before closing the receiver.
            // Durable consumers retain their separate settlement lifetime.
            cancel.cancel();
            subject_inbox.close().await;
            if !subject_finished {
                // Do not drop an admitted request merely because Ctrl-C arrived.
                // A timeout is an observation limit, never proof the provider stopped.
                match tokio::time::timeout(std::time::Duration::from_secs(15), &mut subject_task)
                    .await
                {
                    Ok(Ok(result)) => result?,
                    Ok(Err(error)) => return Err(error.into()),
                    Err(_) => {
                        consumer_cancel.cancel();
                        let (_, step) = store.load_subject(agent_id)?;
                        anyhow::bail!(
                            "shutdown settlement timed out: durable step {} is {:?}, request {:?}; remote outcome is NOT proven stopped, reopen/reconcile before dispatch",
                            step.step_id,
                            step.state,
                            step.active_request_id
                        );
                    }
                }
            }
            // S1 has settled or left a durable unresolved step. Do not drop the
            // consumers first. Their unfinished work remains queryable on reopen.
            consumer_cancel.cancel();
            match tokio::time::timeout(std::time::Duration::from_secs(2), dispatcher_task).await {
                Ok(result) => result??,
                Err(_) => tracing::warn!("dispatcher stop pending; durable work retained"),
            }
            replication_task.await?;
            runtime_result?;
            info!("Shenko stopped");
        }
    }

    Ok(())
    }.await;
    supervisor.stop().await?;
    result
}

/// `shenko browser` — drive the native browser provider directly.
///
/// Reads a JSON array of steps (`--file` or stdin) and prints one JSON result
/// per step. Steps share one provider, so `session open` handles stay valid
/// for later steps of the same batch. This is a native proof/diagnostics
/// surface; the agent-facing surface is the `browser.*` native task kinds.
async fn browser_batch(data_dir: &std::path::Path, file: Option<&std::path::Path>) -> Result<()> {
    use shenko_exec_host::browser::{
        ActionRequest, BrowserProvider, ControlRequest, IdentityRequest, ObserveOptions,
        PageAction, SessionRequest,
    };
    async fn action_request(
        provider: &BrowserProvider,
        step: &serde_json::Value,
        caller: shenko_types::ids::EntityId,
        actions: Vec<PageAction>,
    ) -> Result<ActionRequest> {
        let session_id = session_id_of(step)?;
        let tab_id = step.get("tab_id").and_then(serde_json::Value::as_str).map(str::to_owned);
        // Both CLI action routes use the same live observation/lease binding.
        let (observation_id, lease_token) = match (
            step.get("observation_id").and_then(serde_json::Value::as_str),
            step.get("lease_token").and_then(serde_json::Value::as_str),
        ) {
            (Some(observation), Some(token)) => (observation.to_owned(), token.parse()?),
            _ => {
                let observation = provider.observe(session_id, tab_id.as_deref(), ObserveOptions {
                    screenshot: false, accessibility: false,
                }).await?;
                let control = provider.control(ControlRequest {
                    session_id, op: "acquire".into(),
                    role: Some(shenko_exec_host::browser::ControllerRole::Agent), lease_token: None,
                }, caller).await?;
                (observation.observation_id, control.lease_token.context("acquire did not return a lease token")?)
            }
        };
        Ok(ActionRequest { session_id, tab_id, observation_id, lease_token, actions })
    }
    let input = if let Some(path) = file {
        tokio::fs::read_to_string(path).await?
    } else {
        use tokio::io::AsyncReadExt;
        let mut buffer = String::new();
        tokio::io::stdin().read_to_string(&mut buffer).await?;
        buffer
    };
    let steps: Vec<serde_json::Value> = serde_json::from_str(&input)?;
    create_data_directory_async(data_dir).await?;
    let store = shenko_store::store::Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
    let provider = Arc::new(BrowserProvider::new(Arc::new(store), data_dir.join("browser")));
    let caller = shenko_types::ids::EntityId::new();
    let node = "browser-cli".to_string();
    for step in &steps {
        let op = step
            .get("op")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("session");
        let result: serde_json::Value = match op {
            "identity" => {
                let request: IdentityRequest = serde_json::from_value(step.clone())?;
                serde_json::to_value(provider.identity_op(request, caller, &node).await?)?
            }
            "session" => {
                let action = step
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("open");
                match action {
                    "open" => {
                        let request: SessionRequest = serde_json::from_value(step.clone())?;
                        serde_json::to_value(provider.open_session(request, caller, &node).await?)?
                    }
                    "close" => {
                        let session_id = session_id_of(step)?;
                        serde_json::to_value(provider.close_session(session_id, caller).await?)?
                    }
                    "list" => serde_json::to_value(provider.list_sessions().await?)?,
                    "facts" => {
                        let session_id = session_id_of(step)?;
                        let facts = provider
                            .list_sessions()
                            .await?
                            .into_iter()
                            .find(|facts| facts.session_id == session_id)
                            .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;
                        serde_json::to_value(&facts)?
                    }
                    other => anyhow::bail!("unknown browser session action {other}"),
                }
            }
            "page" => {
                let action = step
                    .get("action")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("observe");
                let session_id = session_id_of(step)?;
                match action {
                    "navigate" => {
                        let url = step
                            .get("url")
                            .and_then(serde_json::Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("page navigate requires a url"))?;
                        let request = action_request(&provider, step, caller,
                            vec![PageAction::Navigate { url: url.to_string() }]).await?;
                        let report = provider.act(request, caller).await?;
                        serde_json::to_value(&report)?
                    }
                    "observe" => {
                        let options = ObserveOptions {
                            screenshot: step
                                .get("screenshot")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(true),
                            accessibility: step
                                .get("accessibility")
                                .and_then(serde_json::Value::as_bool)
                                .unwrap_or(true),
                        };
                        let tab_id = step
                            .get("tab_id")
                            .and_then(serde_json::Value::as_str);
                        serde_json::to_value(provider.observe(session_id, tab_id, options).await?)?
                    }
                    other => anyhow::bail!("unknown browser page action {other}"),
                }
            }
            "action" => {
                let actions: Vec<PageAction> = serde_json::from_value(
                    step.get("actions")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("action step requires an actions array"))?,
                )?;
                let request = action_request(&provider, step, caller, actions).await?;
                serde_json::to_value(provider.act(request, caller).await?)?
            }
            "control" => {
                let request: ControlRequest = serde_json::from_value(step.clone())?;
                serde_json::to_value(provider.control(request, caller).await?)?
            }
            other => anyhow::bail!("unknown browser op {other}"),
        };
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

fn session_id_of(step: &serde_json::Value) -> Result<shenko_types::ids::EntityId> {
    step.get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("step requires a session_id"))
        .and_then(|text| {
            text.parse::<shenko_types::ids::EntityId>()
                .map_err(|error| anyhow::anyhow!("invalid session_id: {error}"))
        })
}

/// `shenko tool …` — inspect and manage the seed's named WASM tool store.
/// Install and exec go through the node's installation-unit inventory — the
/// same authority node_sync replicates — while list is a pure read of that
/// inventory. `<data>/tools` is only where the unit authority materializes
/// bytes; no separate tool registry exists to diverge or adopt foreign data.
fn tool_command(data_dir: &PathBuf, command: ToolCommand) -> Result<()> {
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
            let records = match Node::open_inspect(&data_dir.join("node"))? {
                Some(node) => installed_tools(&node)?,
                None => Vec::new(),
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "tools": records,
                    "named_targets": named_target_report(&records),
                }))?
            );
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
        } => {
            let abi = ToolAbi::parse(&abi)
                .ok_or_else(|| anyhow::anyhow!("unknown --abi {abi:?}"))?;
            shenko_wasm::validate_tool_name(&name)?;
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
            let live_slot = shenko_update::slots::resolve_live_slot(&data_dir)?
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
            let store = open_installation_store(&data_dir)?;
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
            };
            let installed = tool_install::install_one(&mut node, &store, &tools_root, &request)?;
            println!("{}", serde_json::to_string_pretty(&installed)?);
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
        ToolCommand::InstallSet { manifest, artifacts } => {
            let encoded = std::fs::read(&manifest)
                .with_context(|| format!("read manifest {}", manifest.display()))?;
            let set = shenko_wasm::ToolSetManifest::parse(&encoded)?;
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
            let installed =
                tool_install::install_set(&mut node, &store, &tools_root, &set, &artifacts)?;
            println!("{}", serde_json::to_string_pretty(&installed)?);
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
            for entry in env {
                let (key, value) = entry
                    .split_once('=')
                    .ok_or_else(|| anyhow::anyhow!("--env requires KEY=VALUE: {entry:?}"))?;
                command.environment.push((key.into(), value.into()));
            }
            let profile = if read_only {
                shenko_wasm::ToolMountProfile::ReadOnly
            } else {
                shenko_wasm::ToolMountProfile::ManagedHome
            };
            let mut runtime = shenko_wasm::WasmRuntime::with_mounts(profile.resolve(&manifest.mounts))?;
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

/// Deterministic no-model service: keep the existing UI, configuration and
/// health endpoints available without constructing a broker or a fake thought
/// loop. No inbox/outbox is wired, so browsers cannot mistake it for thinking.
async fn run_deterministic_ui(
    port: u16,
    store: Arc<shenko_store::store::Store>,
    data_dir: PathBuf,
    model_profile: shenko_seed::ModelProfile,
    failure: tokio_util::sync::CancellationToken,
    native: Arc<shenko_subject::NativeChildService>,
    owner: shenko_ui::owner::OwnerState,
) -> Result<()> {
    let ui = shenko_ui::server::UiServer::new(port)
        .with_native(native)
        .with_store(store)
        .with_data_dir(data_dir)
        .with_owner(owner)
        .with_inference_status(model_profile.model, model_profile.configured);
    println!("Shenko UI: http://{}", ui.bind);
    println!("  Subject loop: not started (model_binding needed)");
    println!("  Inference: not_connected; capacity blocked");
    let result = tokio::select! {
        result = ui.run() => result,
        _ = failure.cancelled() => Err(anyhow::anyhow!("deterministic recovery stopped; reopen for recovery")),
        result = tokio::signal::ctrl_c() => {
            result?;
            info!("shutdown signal; deterministic bootstrap stopped without model effects");
            Ok(())
        }
    };
    result?;
    info!("Shenko stopped");
    Ok(())
}

fn napcat_inbox_event(
    event: shenko_channel_napcat::client::OneBotEvent,
) -> Result<shenko_subject::inbox::InboxEvent> {
    Ok(shenko_subject::inbox::InboxEvent {
        event_id: event
            .ingress_id
            .ok_or_else(|| anyhow::anyhow!("NapCat event missing durable ingress identity"))?,
        kind: shenko_subject::inbox::InboxEventKind::Message,
        payload: serde_json::to_value(&event)?,
        created_at: chrono::Utc::now(),
        source: "napcat".to_string(),
    })
}

/// Resolve one classified envelope's attachment locators through the owning
/// plugin's file capability. A locator that cannot be resolved stays a
/// locator and is recorded as a subject-readable `attachment_locator` gap;
/// the message itself is never dropped. Shared by the napcat spool admission
/// and the adapter-source admission.
fn prepare_channel_envelope(
    plugin: &Arc<dyn shenko_types::channel::ChannelPlugin>,
    store: &shenko_store::store::Store,
    event_id: shenko_types::ids::EntityId,
    mut envelope: shenko_types::channel::ChannelEnvelope,
) -> Result<shenko_types::channel::ChannelEnvelope> {
    for index in 0..envelope.attachments.len() {
        let attachment = envelope.attachments[index].clone();
        match plugin.resolve_attachment(&attachment) {
            Ok(resolved) => envelope.attachments[index] = resolved,
            Err(detail) => {
                store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                    channel: envelope.provider.clone(),
                    account: envelope.account.clone(),
                    kind: "attachment_locator".to_string(),
                    state: "pending".to_string(),
                    subject_ref: Some(event_id.to_string()),
                    detail: serde_json::json!({
                        "attachment_id": attachment.attachment_id,
                        "detail": detail,
                    }),
                })?;
            }
        }
    }
    Ok(envelope)
}

/// Admit one channel event from a plugin-owned source (napcat-cli poller or
/// an adapter ingress). The event id is derived from the stable provider
/// keys (source, provider, account, platform message id, revision), so an
/// at-least-once redelivery of the same external event meets the committed
/// disposition and is admitted exactly once.
async fn admit_channel_event(
    registry: &shenko_types::channel::ChannelRegistry,
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    inbox_tx: &tokio::sync::mpsc::Sender<shenko_subject::inbox::InboxEvent>,
    source: &str,
    mut envelope: shenko_types::channel::ChannelEnvelope,
    raw: &serde_json::Value,
) -> Result<()> {
    let key = format!(
        "{}|{}|{}|{}|{}",
        source,
        envelope.provider,
        envelope.account,
        envelope.message_id,
        envelope.source_event_revision.as_deref().unwrap_or(""),
    );
    let digest = shenko_types::object::Digest::blake3(key.as_bytes());
    let hex_text = digest.0.strip_prefix("blake3:").unwrap_or(&digest.0);
    let mut bytes = [0u8; 32];
    for (index, chunk) in hex_text.as_bytes().chunks(2).take(32).enumerate() {
        let hex_pair = std::str::from_utf8(chunk).unwrap_or("00");
        bytes[index] = u8::from_str_radix(hex_pair, 16).unwrap_or(0);
    }
    let event_id = shenko_types::ids::EntityId::from_digest(&bytes);
    if store
        .event_disposition_commit(agent_id, event_id)?
        .is_some()
    {
        return Ok(());
    }
    let Some(plugin) = registry.channel_for_source(source) else {
        anyhow::bail!("no registered channel plugin owns source {source}");
    };
    envelope = prepare_channel_envelope(plugin, store, event_id, envelope)?;
    let inbox_ev = shenko_subject::inbox::InboxEvent {
        event_id,
        kind: shenko_subject::inbox::InboxEventKind::Message,
        payload: serde_json::to_value(&envelope)?,
        created_at: chrono::Utc::now(),
        source: source.to_string(),
    };
    let _ = raw; // the raw frame stays in the plugin's own ingress ledger
    inbox_tx
        .send(inbox_ev)
        .await
        .map_err(|_| anyhow::anyhow!("subject inbox closed; ingress remains pending"))?;
    Ok(())
}

/// Journal one channel-runtime error as a durable fact of the subject's
/// history (SPEC §45.14). The journal entry is NOT a delivery receipt: it
/// never retries the operation and never opens an issue by itself.
/// Independent operation unknown/conflict receipts stay on the operation
/// ledger; this only makes the unable-to-continue boundary readable.
fn journal_channel_error(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    kind: &str,
    source_module: &str,
    message: String,
    extra: serde_json::Value,
) {
    store.journal_runtime_error(
        agent_id,
        shenko_store::raw_session::RuntimeErrorContext {
            kind: kind.to_string(),
            source_module: source_module.to_string(),
            message,
            step_no: None,
            task_id: None,
            extra,
        },
    );
}

/// Make sure the agent has an identity revision, and apply a user-given
/// `--name` only for initialization. Existing edits require an explicit CAS. SPEC §6, R117.
fn adopt_identity(
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

/// §45.14 store-backed kernel error sink: plugin-runtime failures observed
/// at kernel boundaries (Cordis lifecycle incidents, hook-graph admission
/// refusals, publish revision conflicts) journal into the SAME subject store
/// as every other runtime-error fact — no second DB, no autonomous action.
/// `journal_runtime_error` is fail-soft (it counts a dropped journal write
/// instead of propagating), so a journal failure can never recurse into the
/// observation path that produced it.
struct StoreErrorSink {
    store: Arc<shenko_store::store::Store>,
    agent_id: shenko_types::ids::EntityId,
}

impl shenko_kernel::ErrorSink for StoreErrorSink {
    fn observe(
        &self,
        site: shenko_kernel::ErrorSite,
        message: String,
        extra: serde_json::Value,
    ) {
        self.store.journal_runtime_error(
            self.agent_id,
            shenko_store::RuntimeErrorContext {
                kind: site.kind.to_string(),
                source_module: site.module.to_string(),
                message,
                step_no: site.step_no,
                task_id: site.task_id,
                extra,
            },
        );
    }
}

/// Open the store of the live slot the durable pointer names. A data dir
/// that has never published a slot is itself the live slot (first run).
fn open_installation_store(data_dir: &Path) -> Result<shenko_store::store::Store> {
    create_data_directory(data_dir)?;
    let live_slot = shenko_update::slots::resolve_live_slot(data_dir)?
        .unwrap_or_else(|| data_dir.to_path_buf());
    Ok(shenko_store::store::Store::open(
        live_slot.join(shenko_update::SLOT_DB_FILENAME),
        // The content store is shared by every generation of an
        // installation and always lives at <data_dir>/cas.
        data_dir.join("cas"),
    )?)
}

// Specify access when creating each new directory, not by chmod after it is
// exposed. Inherited default ACLs otherwise bypass the process umask. Existing
// directories and ancestors are untouched; owner-control still admits them.
fn create_data_directory(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

async fn create_data_directory_async(path: &Path) -> std::io::Result<()> {
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

/// Ensure genesis exists for this installation. Returns agent_id. SPEC §44.6.
/// New geneses bind the real prepared home. Existing geneses are immutable and
/// are never replaced merely to upgrade their manifest representation.
fn bootstrap_genesis(
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
    let head = SubjectBranchHead {
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

    // The initial control authority — SPEC §41.12 X05. The first selector
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

// Share the actual startup registration path with the lifecycle test. The
// native registry owns discovery and withdraws it with the Cordis generation.
async fn install_node_capability_reader(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
) -> Result<()> {
    let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NodeCapabilitiesHandler::new(store, node));
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}

// Independent installed contract: recording choices must never turn the
// observation-only v1 into a mutating API. Both reuse the same Node journal.
async fn install_node_selection_recorder(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
) -> Result<()> {
    let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NodeSelectionHandler::new(store, node));
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}

// The selected SSH helper handler shares the current native registry and
// generation lifetime; absence of explicit configuration leaves it uninstalled.
async fn install_native_ssh(
    supervisor: &shenko_kernel::Supervisor,
    store: Arc<shenko_store::Store>,
    node: shenko_node::Node,
    path: &std::path::Path,
) -> Result<()> {
    let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NativeSshTaskHandler::from_file(store, node, path)?);
    supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    Ok(())
}

#[cfg(test)]
mod recovery_integration_tests {
    include!("native_ssh_installation_tests.rs");
    include!("ssh_public_identity_tests.rs");
    include!("native_ssh_memory_helper_tests.rs");
    use super::*;
    use shenko_channel_napcat::{NapCatClient, NapCatConfig, spool::IngressSpool};
    use shenko_types::{
        ids::*,
        operation::{EffectStatus, OperationOutcome, OperationState},
        subject::*,
        task::{AbortScope, TaskOutcome, TaskState},
    };

    #[tokio::test]
    async fn node_capability_reader_is_discoverable_only_while_installed() {
        let dir = std::env::temp_dir().join(format!("shenko-node-reader-install-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id()).unwrap();
        let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        drop(home);
        store.start_subject_dispatcher().unwrap();
        let node = node_sync::open_node(&dir, &store).unwrap();
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
        let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
        assert!(service.installed_prompt_sections().is_empty());
        install_node_capability_reader(&supervisor, store.clone(), node).await.unwrap();
        let sections = service.installed_prompt_sections();
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("[delegate:node.capabilities.query:1]"));
        // Real parser/S1/registry/handler/T1/E1/T2, with deterministic output
        // supplied as a fixture. This is not model-behavior validation.
        let task_id = EntityId::new();
        commit_native_fixture_step(&store, "[delegate:node.capabilities.query:1] {\"limit\":1}", &[task_id], vec![]);
        drive_native_until(&service, || Ok(store.native_task_result(task_id)?.is_some())).await.unwrap();
        let result = store.native_task_result(task_id).unwrap().unwrap();
        assert_eq!(result.outcome, TaskOutcome::Succeeded);
        assert!(store.ready_native_results(agent, 8).unwrap().iter().any(|ready| ready.result_id == result.result_id));
        let evidence = store.external_operation_evidence(result.operation_id).unwrap();
        assert_eq!(evidence.len(), 1);
        let observation: serde_json::Value = serde_json::from_slice(&store.cas.read_object(
            &evidence[0].normalized_ref.as_ref().unwrap().digest,
        ).unwrap()).unwrap();
        assert_eq!(observation["snapshot"]["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(observation["dispatch"], "not_dispatched");
        assert_eq!(observation["execution_authority"], "not_granted");
        supervisor.stop().await.unwrap();
        assert!(service.installed_prompt_sections().is_empty());
        drop(service); drop(supervisor); drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn node_selection_recorder_delivers_explicit_choice_only_while_installed() {
        let dir = std::env::temp_dir().join(format!("shenko-node-selection-install-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id()).unwrap();
        let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        drop(home);
        store.start_subject_dispatcher().unwrap();
        let mut observer = node_sync::open_node(&dir, &store).unwrap();
        // Explicit fixture publication; the installed read tool never refreshes.
        observer.capability_profile().unwrap();
        let recorder = node_sync::open_node(&dir, &store).unwrap();
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        supervisor.spawn(shenko_subject::NativeChildPlugin, (store.clone(), RuntimeGeneration::default())).await.unwrap();
        let service = supervisor.context().try_service::<shenko_subject::NativeChildService>().unwrap();
        assert!(service.installed_prompt_sections().is_empty());
        install_node_capability_reader(&supervisor, store.clone(), observer).await.unwrap();
        assert!(!service.installed_prompt_sections().iter().any(|section| section.contains("[delegate:node.selection.record:1]")));
        install_node_selection_recorder(&supervisor, store.clone(), recorder).await.unwrap();
        assert_eq!(service.installed_prompt_sections().len(), 2);
        assert!(service.installed_prompt_sections().iter().any(|section| section.contains("[delegate:node.selection.record:1]")));
        // Real parser/S1/registry/T1/Node command/E1/T2, deterministic fixture
        // output only. This does not sample a model or start SSH.
        let query = EntityId::new();
        commit_native_fixture_step(&store, "[delegate:node.capabilities.query:1] {\"limit\":1}", &[query], vec![]);
        drive_native_until(&service, || Ok(store.native_task_result(query)?.is_some())).await.unwrap();
        let query_result = store.native_task_result(query).unwrap().unwrap();
        let candidate = &query_result.result.as_ref().unwrap()["snapshot"]["candidates"][0];
        let task = EntityId::new();
        let input = serde_json::json!({
            "node_id":candidate["node_id"],
            "observed_profile_digest":candidate["knowledge"]["observation"]["profile_digest"],
            "reason":"I explicitly choose the observed fixture node; no dispatch requested",
        });
        commit_native_fixture_step(&store, &format!("[delegate:node.selection.record:1] {input}"), &[task], vec![]);
        drive_native_until(&service, || Ok(store.native_task_result(task)?.is_some())).await.unwrap();
        let result = store.native_task_result(task).unwrap().unwrap();
        assert_eq!(result.outcome, TaskOutcome::Succeeded);
        let output = result.result.as_ref().unwrap();
        assert_eq!(output["receipt"]["selection"]["decision_id"], serde_json::json!(result.operation_id));
        assert_eq!(output["receipt"]["selection"]["node_id"], input["node_id"]);
        assert_eq!(output["receipt"]["selection"]["reason"], input["reason"]);
        assert_eq!(output["receipt"]["outcome"]["status"], "recorded");
        assert_eq!(output["effect_scope"], "local_selection_journal_only");
        assert_eq!(output["dispatch"], "not_dispatched");
        assert_eq!(output["execution_authority"], "not_granted");
        assert_eq!(store.external_operation_attempts(result.operation_id).unwrap().len(), 1);
        assert_eq!(store.external_operation_evidence(result.operation_id).unwrap().len(), 1);
        assert!(store.ready_native_results(agent, 8).unwrap().iter().any(|ready| ready.result_id == result.result_id));
        supervisor.stop().await.unwrap();
        assert!(service.installed_prompt_sections().is_empty());
        drop(service); drop(supervisor); drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Local I/L/CAS simulation: crash after S1, before I mirrors its ACK.
    // No provider, external account, model inference or production data is used.
    #[tokio::test]
    async fn reopen_reconciles_spool_from_committed_subject_disposition() {
        let dir = std::env::temp_dir().join(format!("shenko-continuity-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let spool_path = dir.join("ingress.sqlite");
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let received = spool
            .ingest(serde_json::json!({
                "post_type":"message", "message_type":"group", "group_id":7,
                "self_id":9, "user_id":8, "message_id":42, "raw_message":"continue"
            }))
            .unwrap()
            .unwrap();
        drop(spool);

        let config = NapCatConfig {
            bot_qq: 9,
            ws_url: "ws://127.0.0.1:0".into(),
            ..Default::default()
        };
        let (client, mut rx, actions) =
            NapCatClient::open_persistent(config.clone(), &spool_path).unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let connector = tokio::spawn(NapCatClient::run_connection_loop(
            config,
            client.event_sender(),
            actions,
            client.pending_actions.clone(),
            client.spool.clone(),
            cancel.clone(),
        ));
        // Delivery now runs independently of connection establishment.
        let replayed = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let inbox_event = napcat_inbox_event(replayed).unwrap();
        cancel.cancel();
        connector.await.unwrap().unwrap();
        assert_eq!(inbox_event.event_id, received.spool_id);

        let store = Arc::new(shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap());
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let model_profile = shenko_seed::ModelProfile::from_env();
        let agent = bootstrap_genesis(
            &store,
            None,
            &prepared_home.manifest,
            &model_profile,
        )
        .unwrap();
        let (head, mut step) = store.load_subject(agent).unwrap();
        assert!(
            store
                .event_disposition_commit(agent, inbox_event.event_id)
                .unwrap()
                .is_none()
        );
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        let object = store
            .cas
            .seal_object(b"continue", "text/plain", "test", "raw", 1)
            .unwrap()
            .object;
        store.stage_subject_inputs(&head, &step, &[shenko_store::SubjectHistoryInput {
            event_id: inbox_event.event_id, event_type: "user".into(),
            data: serde_json::json!({"v":1,"text":inbox_event.payload.to_string(),"inbox_event":&inbox_event}),
        }]).unwrap();
        step.input_plan_ref = Some(prepare_fixture_s0(&store, &head, &step));
        step.state = SubjectStepState::Preparing;
        step.record_revision = RecordRevision(1);
        store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(EntityId::new());
        step.record_revision = RecordRevision(2);
        store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
        step.state = SubjectStepState::ResultRecorded;
        step.model_result_ref = Some(object.clone());
        step.record_revision = RecordRevision(3);
        step.model_outcome_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&shenko_types::subject::SubjectModelOutcome {
                        schema_version: 2,
                        request_id: step.active_request_id.unwrap(),
                        result_ref: step.model_result_ref.clone().unwrap(),
                        stop_reason: "end_turn".into(),
                        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                        usage: None,
                    })
                    .unwrap(),
                    "application/json",
                    "model_outcomes",
                    "SubjectModelOutcome",
                    2,
                )
                .unwrap()
                .object,
        );
        store.upsert_step(&step, &head, RecordRevision(2)).unwrap();
        let next = EntityId::new();
        let identity_context = cordis_core::Context::new();
        let _publication = identity_context
            .provide(Arc::new(IdentityService::new(Arc::new(
                shenko_identity::DefaultIdentityPolicy,
            ))))
            .unwrap();
        let identity_service = identity_context.try_service::<IdentityService>().unwrap();
        let _context = identity_context.provide(Arc::new(shenko_context_policy::PolicyRegistry::with_ledger(
            shenko_context_policy::WindowLedger::new(Arc::new(
                shenko_context_policy::StoreWindowLog::new(store.clone(), EntityId(head.branch_id.0)),
            )),
        ))).unwrap();
        let decision = StepDecision {
            schema_version: 8,
            output: shenko_subject::output::parse_output(step.step_id, object.clone(), "continue", &identity_service, &shenko_channel_napcat::qq_test_registry(), None)
            .unwrap(),
            identity_intent: None,
            step_id: step.step_id,
            input_plan_ref: step.input_plan_ref.clone().unwrap(),
            model_result_ref: Some(object),
            applied_steering_refs: vec![],
            event_dispositions: vec![EventDisposition {
                event_id: inbox_event.event_id,
                disposition: Disposition::Acknowledged,
                reason: None,
                review_after: None,
                priority: None,
            }],
            delegated_task_intents: vec![],
            child_dialogue_intents: vec![],
            memory_intents: vec![],
            policy_intents: vec![],
            publication_intents: vec![],
            pending_obligations: vec![],
            evidence_refs: vec![],
            attention_policy: None,
            successor_step_id: next,
        };
        step.decision_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&decision).unwrap(),
                    "application/json",
                    "test",
                    "StepDecision",
                    8,
                )
                .unwrap()
                .object,
        );
        step.state = SubjectStepState::Committed;
        step.record_revision = RecordRevision(4);
        let receipt = store
            .commit_step_and_advance_head(&step, &head, next)
            .unwrap();
        // Intentionally omit ACK: model-side processing committed, I mirror is stale.
        assert!(!client.spool.event(received.spool_id).unwrap().consumed);
        drop(store);
        drop(client);

        let store = Arc::new(
            shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap(),
        );
        assert_eq!(
            bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
            agent
        );
        let restored = shenko_subject::loop_runner::SubjectLoopState::new(
            agent,
            Arc::new(shenko_subject::inbox::Inbox::new(8)),
            store.clone(),
            identity_context,
            shenko_channel_napcat::qq_test_registry(),
        )
        .unwrap();
        assert_eq!(restored.head.branch_id, head.branch_id);
        assert_eq!(restored.head.committed_at, receipt.commit_ref);
        assert_eq!(restored.step.step_id, next);
        assert_eq!(restored.step.state, SubjectStepState::Planned);
        let spool = IngressSpool::open(&spool_path, 9).unwrap();
        let pending = spool.consume(8).unwrap();
        assert_eq!(pending.len(), 1);
        let commit = store
            .event_disposition_commit(agent, pending[0].spool_id)
            .unwrap()
            .unwrap();
        assert_eq!(commit, receipt.commit_ref);
        spool.ack(pending[0].spool_id, &commit.to_string()).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(spool.event(received.spool_id).unwrap().raw, received.raw);
        assert!(spool.ingest(received.raw).unwrap().is_none());
        // Temporary evidence files are deliberately retained for inspection.
    }

    fn open_native_fixture(dir: &std::path::Path) -> shenko_store::store::Store {
        shenko_store::store::Store::open(dir.join("shenko.db"), dir.join("cas")).unwrap()
    }

    fn prepare_fixture_s0(
        store: &Arc<shenko_store::store::Store>,
        head: &shenko_types::subject::SubjectBranchHead,
        step: &shenko_types::subject::SubjectStep,
    ) -> shenko_types::object::ObjectRef {
        let scope = EntityId(head.branch_id.0);
        let staged = store.stage_subject_inputs(head, step, &[]).unwrap();
        let input_raw_event_ids = staged.iter().map(|row| row.event_id).collect::<Vec<_>>();
        let (events, messages): (Vec<_>, Vec<_>) = staged.iter().map(|row| {
            let data: serde_json::Value = serde_json::from_str(&row.data).unwrap();
            let text = data.get("text").and_then(serde_json::Value::as_str).unwrap_or(&row.data);
            (data.get("inbox_event").cloned().unwrap(),
                shenko_broker::request::InferenceMessage::text("user", text))
        }).unzip();
        let ledger = shenko_context_policy::WindowLedger::new(Arc::new(
            shenko_context_policy::StoreWindowLog::new(store.clone(), scope),
        ));
        let profile = shenko_types::object::Digest::blake3(b"native-recovery-fixture");
        ledger.rebind_model(1, &profile.0).unwrap();
        let window = ledger.current().unwrap();
        let leaf_seq = store.session_leaf_seq(scope).unwrap()
            .map(|seq| u64::try_from(seq).unwrap());
        let input = serde_json::json!({
            "schema_version":12, "step_id":step.step_id, "step_no":step.step_no, "provider_selection":{"schema_version":1,"binding":null,"model":null,"profile_id":null,"profile_revision":null,"credential_fingerprint":null,"resource_domain_id":null,"serializer_revision":null}, "request_id":"00000000-0000-0000-0000-000000000000",
            "events":events, "provider_binding":null,
            "payload":shenko_broker::request::InferencePayload {
                system_prompt:"native recovery fixture".into(), messages,
                max_tokens:Some(4096), tools:vec![], native_options:Default::default(),
            },
            "semantic_wait":store.semantic_wait(head.agent_id).unwrap(), "identity":store.latest_identity(head.agent_id).unwrap(),
            "identity_provider_revision":"shenko.identity/default@1",
            "context":{
                "source":shenko_store::SubjectContextRef {
                    scope_id:scope, source_commit_seq:store.current_commit_seq().unwrap(), leaf_seq,
                    input_raw_event_ids, window_id:window.id,
                    window_generation:window.generation, model_generation:1 },
                "profile_digest":profile,
                "estimated_input_tokens":null, "budget_input_tokens":null,
                "trigger_budget_input_tokens":null,
                "budget_provenance":"native fixture without measured current input",
                "reserved_output_tokens":4096, "unfinished_call_ids":[],
                "capabilities":null, "observed_input_tokens":null,
                "token_counter":"heuristic:cjk-1-other-4@1; not a provider tokenizer",
                "policy_contract":shenko_context_policy::CONTRACT_VERSION,
                "selection":{"outcome":"unavailable","reason":"fixture_without_model_capabilities"},
            },
        });
        store.cas.seal_object(&serde_json::to_vec(&input).unwrap(), "application/json",
            "input_plans", "subject_step_input", 12).unwrap().object
    }

    /// Deterministic model-output fixture admitted through the real parser and S1
    /// transaction. No model provider is invoked by this test helper.
    fn commit_native_fixture_step(
        store: &Arc<shenko_store::store::Store>,
        model_text: &str,
        task_ids: &[EntityId],
        event_dispositions: Vec<EventDisposition>,
    ) -> shenko_types::commit::CommitRef {
        let (head, mut step) = store
            .load_subject(store.get_existing_agent_id().unwrap().unwrap())
            .unwrap();
        let object = store
            .cas
            .seal_object(model_text.as_bytes(), "text/plain", "test", "raw", 1)
            .unwrap()
            .object;
        step.input_plan_ref = Some(prepare_fixture_s0(store, &head, &step));
        step.state = SubjectStepState::Preparing;
        step.record_revision = RecordRevision(1);
        store.upsert_step(&step, &head, RecordRevision(0)).unwrap();
        step.state = SubjectStepState::AwaitingModel;
        step.active_request_id = Some(EntityId::new());
        step.record_revision = RecordRevision(2);
        store.upsert_step(&step, &head, RecordRevision(1)).unwrap();
        step.state = SubjectStepState::ResultRecorded;
        step.model_result_ref = Some(object.clone());
        step.model_outcome_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&SubjectModelOutcome {
                        schema_version: 2,
                        request_id: step.active_request_id.unwrap(),
                        result_ref: object.clone(),
                        stop_reason: "end_turn".into(),
                        raw_response: r#"{"fixture":true,"stop_reason":"end_turn"}"#.into(),
                        usage: None,
                    })
                    .unwrap(),
                    "application/json",
                    "model_outcomes",
                    "SubjectModelOutcome",
                    2,
                )
                .unwrap()
                .object,
        );
        step.record_revision = RecordRevision(3);
        store.upsert_step(&step, &head, RecordRevision(2)).unwrap();

        let identity = IdentityService::new(Arc::new(shenko_identity::DefaultIdentityPolicy));
        let output = shenko_subject::output::parse_output(step.step_id, object.clone(), model_text, &identity, &shenko_channel_napcat::qq_test_registry(), None)
        .unwrap();
        let mut delegated_task_intents = vec![];
        for control in &output.controls {
            if let shenko_types::output::OutputAction::Delegate { text, definition } =
                &control.action
            {
                delegated_task_intents.push(DelegationIntent {
                    intent_id: task_ids[delegated_task_intents.len()],
                    goal: text.clone(),
                    system_prompt: text.clone(),
                    model: None,
                    capabilities: vec![],
                    budget_hint: None,
                    reply_grant: None,
                    definition: definition.clone(),
                });
            }
        }
        assert_eq!(delegated_task_intents.len(), task_ids.len());
        let next = EntityId::new();
        let decision = StepDecision {
            schema_version: 8,
            output,
            identity_intent: None,
            step_id: step.step_id,
            input_plan_ref: step.input_plan_ref.clone().unwrap(),
            model_result_ref: Some(object),
            applied_steering_refs: vec![],
            event_dispositions,
            delegated_task_intents,
            child_dialogue_intents: vec![],
            memory_intents: vec![],
            policy_intents: vec![],
            publication_intents: vec![],
            pending_obligations: vec![],
            evidence_refs: vec![],
            attention_policy: None,
            successor_step_id: next,
        };
        step.decision_ref = Some(
            store
                .cas
                .seal_object(
                    &serde_json::to_vec(&decision).unwrap(),
                    "application/json",
                    "test",
                    "StepDecision",
                    8,
                )
                .unwrap()
                .object,
        );
        step.state = SubjectStepState::Committed;
        step.record_revision = RecordRevision(4);
        store
            .commit_step_and_advance_head(&step, &head, next)
            .unwrap()
            .commit_ref
    }

    async fn native_fixture_helper(dir: &std::path::Path) -> Result<()> {
        let store = Arc::new(open_native_fixture(dir));
        store.start_subject_dispatcher()?;
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (store.clone(), RuntimeGeneration::default()),
            )
            .await?;
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()?;
        native.recover()?;
        let agent = store.get_existing_agent_id()?.expect("fixture identity exists");
        let cancel = tokio_util::sync::CancellationToken::new();
        let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(), native, agent, None, cancel.clone(), cancel.clone(),
        ));
        let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while store.ready_native_results(agent, 1)?.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            anyhow::Ok(())
        }).await;
        cancel.cancel();
        let dispatched = dispatcher.await;
        supervisor.stop().await?;
        observed??;
        dispatched??;
        Ok(())
    }

    /// Drive bounded dispatcher passes until a durable condition is met. The
    /// dispatcher manages executions asynchronously; asserting "two passes
    /// finish a task" would re-serialize them in the test's favour.
    async fn drive_native_until(
        native: &Arc<shenko_subject::NativeChildService>,
        mut met: impl FnMut() -> Result<bool>,
    ) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                native.run_once().await?;
                if met()? {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    // Real local process boundary: one process executes the committed Pending
    // task and exits; its parent reopens the ledger and must not execute it again.
    // The model output is a deterministic fixture, not production-model evidence.
    #[tokio::test]
    async fn native_printf_result_survives_real_process_reopen() {
        const HELPER: &str = "SHENKO_NATIVE_PRINTF_REOPEN_HELPER";
        if let Some(dir) = std::env::var_os(HELPER) {
            native_fixture_helper(std::path::Path::new(&dir))
                .await
                .unwrap();
            return;
        }
        let dir = std::env::temp_dir().join(format!("shenko-native-reopen-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let model_profile = shenko_seed::ModelProfile::from_env();
        let agent = bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap();
        store.start_subject_dispatcher().unwrap();
        let printf_task = EntityId::new();
        let missing_task = EntityId::new();
        let expected = "native child 真实结果\nwith spaces";
        let model_text = format!(
            "[delegate:host.printf:1] {}\n[delegate:example.missing:1] {{\"text\":\"missing\"}}",
            serde_json::json!({"text":expected,"timeout_secs":5})
        );
        commit_native_fixture_step(
            &store,
            &model_text,
            &[printf_task, missing_task],
            vec![],
        );
        let pending = store.external_task(printf_task).unwrap();
        assert!(matches!(pending.state, TaskState::Pending { .. }));
        assert_eq!(pending.kind, "host.printf");
        assert!(store.native_task_result(printf_task).unwrap().is_none());
        drop(store);

        let status = tokio::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("recovery_integration_tests::native_printf_result_survives_real_process_reopen")
            .arg("--nocapture")
            .env(HELPER, &dir)
            .status()
            .await
            .unwrap();
        assert!(status.success(), "native helper process failed: {status}");

        let store = Arc::new(open_native_fixture(&dir));
        assert_eq!(
            bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap(),
            agent
        );
        store.start_subject_dispatcher().unwrap();
        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (store.clone(), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(store.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await
            .unwrap();
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()
            .unwrap();
        native.recover().unwrap();
        drive_native_until(&native, || {
            Ok(store.native_task_result(printf_task)?.is_some())
        })
        .await
        .unwrap();

        let result = store.native_task_result(printf_task).unwrap().unwrap();
        assert!(matches!(&result.outcome, TaskOutcome::Succeeded));
        assert_eq!(result.task_id, printf_task);
        assert_eq!(
            result
                .result
                .as_ref()
                .and_then(|value| value.get("stdout_utf8"))
                .and_then(serde_json::Value::as_str),
            Some(expected)
        );
        let task = store.external_task(printf_task).unwrap();
        assert!(task.is_terminal());
        assert_eq!(task.updated_commit, result.commit_ref);
        let attempts = store
            .external_operation_attempts(result.operation_id)
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].invocation_id, result.invocation_id);
        let evidence = store
            .external_operation_evidence(result.operation_id)
            .unwrap();
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].receipt_id, result.receipt_id);
        assert_eq!(
            shenko_store::child::park_reason(&store.external_task(missing_task).unwrap()),
            Some("handler_unavailable")
        );
        assert!(store.native_task_result(missing_task).unwrap().is_none());
        assert_eq!(store.ready_native_results(agent, 8).unwrap().len(), 1);

        // A receiver can close while its owning Arc still lives. Durable
        // results do not make that unexpected delivery failure a success.
        let closed_inbox = Arc::new(shenko_subject::inbox::Inbox::new(8));
        closed_inbox.close().await;
        let closed_cancel = tokio_util::sync::CancellationToken::new();
        let mut closed_dispatcher = tokio::spawn(
            shenko_subject::dispatch::run_committed_dispatcher(
                store.clone(), native.clone(), agent,
                Some(closed_inbox.sender()),
                tokio_util::sync::CancellationToken::new(), closed_cancel.clone(),
            ),
        );
        let failed = tokio::time::timeout(
            std::time::Duration::from_secs(5), &mut closed_dispatcher,
        ).await;
        closed_cancel.cancel();
        if failed.is_err() {
            closed_dispatcher.await.unwrap().unwrap();
            supervisor.stop().await.unwrap();
            panic!("unexpected inbox closure was silently suppressed by the native dispatcher");
        }
        let delivery_error = failed.unwrap().unwrap().expect_err(
            "unexpected inbox closure must propagate, not return success");
        assert!(delivery_error.is::<tokio::sync::mpsc::error::SendError<shenko_subject::InboxEvent>>(),
            "the propagated cause must be receiver loss, not an unrelated store failure");
        let (head, _) = store.load_subject(agent).unwrap();
        let error_filter = shenko_store::SessionEventFilter {
            from_seq: 0,
            to_seq: i64::MAX,
            limit: 20,
            event_types: vec![shenko_store::RUNTIME_ERROR_EVENT_TYPE.to_owned()],
            roles: vec![],
        };
        let errors = store.list_session_events(
            EntityId(head.branch_id.0), &error_filter,
        ).unwrap();
        assert_eq!(errors.len(), 1);
        let error: serde_json::Value = serde_json::from_str(&errors[0].data).unwrap();
        assert_eq!(error["kind"], "native_dispatcher");
        assert_eq!(error["journal_dropped_total"], 0);
        assert_eq!(store.ready_native_results(agent, 8).unwrap()[0].result_id, result.result_id);
        assert_eq!(
            serde_json::to_value(store.external_operation_evidence(result.operation_id).unwrap()).unwrap(),
            serde_json::to_value(&evidence).unwrap(),
            "delivery failure must not replace the original native E1",
        );

        // Stopping subject delivery must not stop durable consumers before
        // the final S1 work can settle. Execute another real printf while
        // the same inbox is closed, with only the subject token cancelled.
        let subject_stopping = tokio_util::sync::CancellationToken::new();
        subject_stopping.cancel();
        let settling_task = EntityId::new();
        let settling_text = "native work settles after subject delivery stops";
        commit_native_fixture_step(
            &store,
            &format!("[delegate:host.printf:1] {}", serde_json::json!({
                "text": settling_text, "timeout_secs": 5,
            })),
            &[settling_task], vec![],
        );
        let settling_cancel = tokio_util::sync::CancellationToken::new();
        let settling_dispatcher = tokio::spawn(
            shenko_subject::dispatch::run_committed_dispatcher(
                store.clone(), native.clone(), agent, Some(closed_inbox.sender()),
                subject_stopping, settling_cancel.clone(),
            ),
        );
        let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(result) = store.native_task_result(settling_task)? {
                    break anyhow::Ok(result);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await;
        settling_cancel.cancel();
        settling_dispatcher.await.unwrap().unwrap();
        let settled = settled.unwrap().unwrap();
        assert!(matches!(&settled.outcome, TaskOutcome::Succeeded));
        assert_eq!(settled.result.as_ref().unwrap()["stdout_utf8"], settling_text);
        assert_eq!(store.external_operation_attempts(settled.operation_id).unwrap().len(), 1);
        assert_eq!(store.ready_native_results(agent, 8).unwrap().len(), 2);
        assert_eq!(store.list_session_events(EntityId(head.branch_id.0), &error_filter).unwrap().len(), 1,
            "expected shutdown must not add a runtime error");

        let inbox = Arc::new(shenko_subject::inbox::Inbox::new(8));
        let cancel = tokio_util::sync::CancellationToken::new();
        let dispatcher = tokio::spawn(shenko_subject::dispatch::run_committed_dispatcher(
            store.clone(),
            native.clone(),
            agent,
            Some(inbox.sender()),
            cancel.clone(),
            cancel.clone(),
        ));
        let events = inbox.recv_batch(2, 2_000).await;
        cancel.cancel();
        dispatcher.await.unwrap().unwrap();
        assert_eq!(events.len(), 2);
        let original_event = events.iter().find(|event| event.event_id == result.result_id).unwrap();
        let settled_event = events.iter().find(|event| event.event_id == settled.result_id).unwrap();
        assert_eq!(settled_event.payload["result"]["stdout_utf8"], settling_text);
        assert_eq!(
            original_event.kind,
            shenko_subject::inbox::InboxEventKind::TaskResult
        );
        assert_eq!(
            original_event
                .payload
                .get("result")
                .and_then(|value| value.get("stdout_utf8"))
                .and_then(serde_json::Value::as_str),
            Some(expected)
        );
        assert_eq!(
            store
                .external_operation_attempts(result.operation_id)
                .unwrap()
                .len(),
            1
        );
        supervisor.stop().await.unwrap();

        commit_native_fixture_step(
            &store,
            "已接收并核对 child 结果",
            &[],
            events.iter().map(|event| EventDisposition {
                event_id: event.event_id,
                disposition: Disposition::Acknowledged,
                reason: None,
                review_after: None,
                priority: None,
            }).collect(),
        );
        assert!(store.ready_native_results(agent, 8).unwrap().is_empty());
        assert_eq!(
            store
                .external_operation_attempts(result.operation_id)
                .unwrap()
                .len(),
            1
        );
        drop(native);
        drop(_handler);
        drop(_child);
        drop(supervisor);
        drop(store);
        let reopened = open_native_fixture(&dir);
        let reopened_errors = reopened.list_session_events(
            EntityId(head.branch_id.0), &error_filter,
        ).unwrap();
        assert_eq!(reopened_errors.len(), 1);
        assert_eq!(reopened_errors[0].data, errors[0].data);
        assert!(reopened.ready_native_results(agent, 8).unwrap().is_empty());
        assert_eq!(
            serde_json::to_value(reopened.external_operation_evidence(result.operation_id).unwrap()).unwrap(),
            serde_json::to_value(&evidence).unwrap(),
        );
        // The process/ledger/evidence directory is retained for readback.
    }

    /// Arm the existing T1 record without spawning a process. This is explicitly
    /// a fault/interleaving simulation; only the preceding test is real process
    /// execution evidence. It verifies fencing and recovery, not SSH or a model.
    fn arm_simulated_native_operation(
        store: &shenko_store::store::Store,
        claim: &shenko_store::NativeTaskClaim,
        implementation: shenko_types::task::ImplementationRef,
        operation_name: &str,
    ) -> Result<(
        shenko_types::operation::OperationRecord,
        shenko_types::operation::AttemptRecord,
    )> {
        use shenko_types::{commit::CommitRef, object::*, operation::*};
        let seal =
            |value: &serde_json::Value, schema: &str| -> Result<shenko_types::object::ObjectRef> {
                Ok(store
                    .cas
                    .seal_object(
                        &serde_json::to_vec(value)?,
                        "application/json",
                        "native-fault-simulation",
                        schema,
                        1,
                    )?
                    .object)
            };
        let input_ref = seal(&serde_json::json!({"text":"simulated"}), "input")?;
        let bindings = ResourceBindings {
            provider: "simulation.only".into(),
            account_identity: String::new(),
            runtime_node: claim.invocation.node_id.to_string(),
            mount_generation: 0,
            capability_level: "no_process_was_started".into(),
            extra: serde_json::json!({"simulation":true}),
        };
        let resource = seal(&serde_json::to_value(bindings)?, "bindings")?;
        let acceptance = seal(
            &serde_json::json!("original simulated result"),
            "acceptance",
        )?;
        let retry = seal(&serde_json::json!("no automatic replay"), "retry")?;
        let version = VersionRef {
            artifact: ArtifactRef {
                digest: implementation.artifact_digest.clone(),
                locator: "simulation:host-printf-v1".into(),
            },
            symbol: format!("{operation_name}.execute"),
            schema: SchemaRef {
                name: "host-printf-observation-v1".into(),
                major: 1,
                digest: implementation.artifact_digest.clone(),
            },
        };
        let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
        let mut operation = OperationRecord {
            record_schema_version: 1,
            operation_id: claim.operation_id,
            record_revision: RecordRevision(0),
            task_id: claim.task.task_id,
            logical_key: "native-handler".into(),
            operation_name: operation_name.into(),
            input_schema_version: 1,
            output_schema_version: 1,
            final_input_ref: input_ref.clone(),
            input_digest: input_ref.digest.clone(),
            contract_ref: OperationContractRef {
                plugin_id: implementation.plugin_id,
                artifact_digest: implementation.artifact_digest.clone(),
                contract_digest: implementation.artifact_digest.clone(),
                receipt_reader_ref: version.clone(),
            },
            resource_bindings_ref: resource,
            authority_ref: AuthorityRef {
                delegation_id: claim.task.task_id,
                delegation_revision: RecordRevision(claim.invocation.task_revision.0),
                decision_ref: claim
                    .task
                    .created_by_step_id
                    .unwrap_or(claim.invocation.invocation_id),
            },
            acceptance_predicate_ref: acceptance,
            retry_contract_ref: retry,
            checkpoint_scope_ref: None,
            state: OperationState::InFlight,
            outcome: None,
            outcome_conflict: false,
            effect_status: EffectStatus::Unknown,
            retry_of: None,
            compensates: None,
            possible_duplicate_of: vec![],
            prepared_commit: zero,
            evidence_through: None,
        };
        let mut attempt = AttemptRecord {
            record_schema_version: 1,
            attempt_id: EntityId::new(),
            operation_id: claim.operation_id,
            attempt_no: 1,
            invocation_id: claim.invocation.invocation_id,
            worker_id: claim.invocation.worker_id,
            node_id: claim.invocation.node_id,
            epoch: claim.invocation.epoch,
            implementation_ref: version,
            dispatch_stage: AttemptDispatchStage::Armed,
            armed_commit: zero,
            provider_handle: None,
            provider_idempotency: None,
            transport_ref: None,
            started_at: None,
            last_observed_at: None,
            final_receipt_id: None,
        };
        assert!(
            store
                .arm_external_operation(&claim.invocation, &mut operation, &mut attempt)?
                .is_none()
        );
        Ok((operation, attempt))
    }

    #[test]
    fn subject_boundary_keeps_admitted_operation_and_late_evidence() {
        let dir = std::env::temp_dir().join(format!("shenko-native-boundary-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(), ..Default::default()
        }).prepare(store.ledger.ledger_id()).unwrap();
        let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        store.start_subject_dispatcher().unwrap();
        let admitted = EntityId::new();
        let pending = EntityId::new();
        commit_native_fixture_step(
            &store,
            "[delegate:host.printf:1] {\"text\":\"admitted\"}\n[delegate:host.printf:1] {\"text\":\"pending\"}",
            &[admitted, pending], vec![],
        );
        let descriptor = shenko_subject::NativeTaskHandler::descriptor(
            &shenko_exec_host::HostPrintfHandler::new(store.clone()),
        );
        let claim = store.claim_native_task(admitted, descriptor.implementation_ref.clone(), false).unwrap().unwrap();
        // This exercises the real T1/E1 ledger, not a completed handoff or a
        // claim that an external process ran.
        let (operation, attempt) = arm_simulated_native_operation(&store, &claim, descriptor.implementation_ref.clone(), "host.printf.v1").unwrap();
        let request_id = EntityId::new();
        store.request_subject_quiesce(agent, request_id).unwrap();
        let (head, _) = store.load_subject(agent).unwrap();
        assert!(store.pause_subject_at_boundary(&head).unwrap().is_some());
        assert!(store.claim_native_task(pending, descriptor.implementation_ref.clone(), false).unwrap().is_none());
        assert_eq!(store.external_operation(operation.operation_id).unwrap().state, OperationState::InFlight);

        let raw = store.cas.seal_object(b"late evidence", "text/plain", "test", "evidence", 1).unwrap().object;
        let evidence = shenko_types::evidence::EvidenceReceipt {
            record_schema_version: 1, receipt_id: EntityId::new(),
            operation_id: Some(operation.operation_id), attempt_id: Some(attempt.attempt_id),
            source_id: "boundary.fixture".into(), source_instance_id: attempt.node_id.to_string(),
            source_event_key: Some("late".into()), producer_epoch: Some(attempt.epoch.0),
            producer_schema_version: Some("1".into()), raw_ref: raw,
            decoder_ref: Some(operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: shenko_types::evidence::DecodeStatus::Decoded,
            observation_level: Some(shenko_types::evidence::ObservationLevel::PostconditionVerified),
            normalized_ref: None, observed_at: Some(chrono::Utc::now()),
            received_commit: operation.prepared_commit,
        };
        let recorded = store.record_external_evidence(operation.operation_id, attempt.attempt_id, evidence,
            Some(OperationOutcome::Succeeded), EffectStatus::None).unwrap();
        assert_eq!(recorded.state, OperationState::Resolved);
        assert_eq!(store.external_operation_attempts(operation.operation_id).unwrap().len(), 1);
        store.resume_subject_quiesce(agent, request_id).unwrap();
        assert!(store.claim_native_task(pending, descriptor.implementation_ref, false).unwrap().is_some());
    }

    #[tokio::test]
    async fn native_cancel_late_evidence_and_unknown_recovery_are_fenced() {
        let dir = std::env::temp_dir().join(format!("shenko-native-fencing-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let old_store = Arc::new(open_native_fixture(&dir));
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(old_store.ledger.ledger_id())
        .unwrap();
        bootstrap_genesis(
            &old_store,
            None,
            &prepared_home.manifest,
            &shenko_seed::ModelProfile::from_env(),
        ).unwrap();
        old_store.start_subject_dispatcher().unwrap();
        let cancelled_task = EntityId::new();
        let unknown_task = EntityId::new();
        commit_native_fixture_step(
            &old_store,
            "[delegate:host.printf:1] {\"text\":\"cancel\"}\n[delegate:host.printf:1] {\"text\":\"unknown\"}",
            &[cancelled_task, unknown_task],
            vec![],
        );
        let descriptor = shenko_subject::NativeTaskHandler::descriptor(
            &shenko_exec_host::HostPrintfHandler::new(old_store.clone()),
        );
        let cancelled_claim = old_store
            .claim_native_task(cancelled_task, descriptor.implementation_ref.clone(), false)
            .unwrap()
            .unwrap();
        let unknown_claim = old_store
            .claim_native_task(unknown_task, descriptor.implementation_ref.clone(), false)
            .unwrap()
            .unwrap();
        let (cancelled_operation, cancelled_attempt) = arm_simulated_native_operation(
            &old_store,
            &cancelled_claim,
            descriptor.implementation_ref.clone(),
            "host.printf.v1",
        )
        .unwrap();
        let (_unknown_operation, _unknown_attempt) = arm_simulated_native_operation(
            &old_store,
            &unknown_claim,
            descriptor.implementation_ref.clone(),
            "host.printf.v1",
        )
        .unwrap();
        let old_native = shenko_subject::NativeChildService::new(
            old_store.clone(),
            RuntimeGeneration::default(),
        );
        assert_eq!(
            old_native
                .cancel(
                    cancelled_task,
                    "test cancellation",
                    AbortScope::SubtreeOrdinary
                )
                .unwrap()
                .cancelled,
            vec![cancelled_task]
        );

        // A late original result is durable E1, not authority for the cancelled
        // invocation to create T2 or a subject result.
        let raw = old_store
            .cas
            .seal_object(
                br#"{"simulated_late_result":true}"#,
                "application/json",
                "native-fault-simulation",
                "late-result",
                1,
            )
            .unwrap()
            .object;
        let late = shenko_types::evidence::EvidenceReceipt {
            record_schema_version: 1,
            receipt_id: EntityId::new(),
            operation_id: Some(cancelled_operation.operation_id),
            attempt_id: Some(cancelled_attempt.attempt_id),
            source_id: "simulation.original".into(),
            source_instance_id: cancelled_attempt.node_id.to_string(),
            source_event_key: Some("late-original-result".into()),
            producer_epoch: Some(cancelled_attempt.epoch.0),
            producer_schema_version: Some("1".into()),
            raw_ref: raw,
            decoder_ref: Some(cancelled_operation.contract_ref.receipt_reader_ref.clone()),
            decode_status: shenko_types::evidence::DecodeStatus::Decoded,
            observation_level: Some(
                shenko_types::evidence::ObservationLevel::PostconditionVerified,
            ),
            normalized_ref: None,
            observed_at: Some(chrono::Utc::now()),
            received_commit: cancelled_operation.prepared_commit,
        };
        let resolved = old_store
            .record_external_evidence(
                cancelled_operation.operation_id,
                cancelled_attempt.attempt_id,
                late,
                Some(OperationOutcome::Succeeded),
                EffectStatus::None,
            )
            .unwrap();
        assert_eq!(resolved.state, OperationState::Resolved);
        assert!(
            old_store
                .complete_native_task(&cancelled_claim, &resolved)
                .unwrap()
                .is_none()
        );
        assert!(
            old_store
                .native_task_result(cancelled_task)
                .unwrap()
                .is_none()
        );

        let reopened = Arc::new(open_native_fixture(&dir));
        reopened.start_subject_dispatcher().unwrap();
        assert!(
            old_store
                .complete_native_task(&cancelled_claim, &resolved)
                .is_err()
        );
        drop(old_native);
        drop(old_store);

        let supervisor = shenko_kernel::Supervisor::new(RuntimeGeneration::default());
        let _child = supervisor
            .spawn(
                shenko_subject::NativeChildPlugin,
                (reopened.clone(), RuntimeGeneration::default()),
            )
            .await
            .unwrap();
        let handler: Arc<dyn shenko_subject::NativeTaskHandler> =
            Arc::new(shenko_exec_host::HostPrintfHandler::new(reopened.clone()));
        let _handler = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await
            .unwrap();
        let native = supervisor
            .context()
            .try_service::<shenko_subject::NativeChildService>()
            .unwrap();
        native.recover().unwrap();
        drive_native_until(&native, || {
            let task = reopened.external_task(unknown_task)?;
            Ok(matches!(task.state, TaskState::Pending { .. })
                && shenko_store::child::park_reason(&task) == Some("reconciling"))
        })
        .await
        .unwrap();

        let unknown = reopened
            .external_operation(unknown_claim.operation_id)
            .unwrap();
        assert_eq!(unknown.state, OperationState::Reconciling);
        assert_eq!(unknown.effect_status, EffectStatus::Unknown);
        assert_eq!(unknown.outcome, None);
        let unknown_task_record = reopened.external_task(unknown_task).unwrap();
        assert!(matches!(
            unknown_task_record.state,
            TaskState::Pending { .. }
        ));
        assert_eq!(
            shenko_store::child::park_reason(&unknown_task_record),
            Some("reconciling")
        );
        assert_eq!(
            reopened
                .external_operation_attempts(unknown_claim.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_evidence(unknown_claim.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert!(reopened.native_task_result(unknown_task).unwrap().is_none());
        assert!(
            reopened
                .native_task_result(cancelled_task)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            reopened
                .external_operation_attempts(cancelled_operation.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            reopened
                .external_operation_evidence(cancelled_operation.operation_id)
                .unwrap()
                .len(),
            1
        );
        assert!(
            reopened
                .external_task(cancelled_task)
                .unwrap()
                .abort_request
                .is_some()
        );
        supervisor.stop().await.unwrap();
        // Simulated fault artifacts and all original receipts remain on disk.
    }

    // ---------------------------------------------------------------
    // C15 Computer Use: real X11 (x11rb wire) observation/input/lease.
    // Xvfb is spawned only by these tests; DISPLAY is never assumed and
    // no human desktop is touched. x11rb is a dev-dependency of this bin.
    // ---------------------------------------------------------------

    /// Owning teardown for a spawned Xvfb: `Child::drop` does NOT kill, so a
    /// raw Child would leak the server on success or on assert-panic.
    struct XvfbGuard(std::process::Child);

    impl XvfbGuard {
        fn kill_and_wait(mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for XvfbGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The display + the process that owns it. `owned: None` means the
    /// display came from SHENKO_TEST_X11_DISPLAY and must never be killed.
    struct X11Display {
        display: String,
        owned: Option<XvfbGuard>,
    }

    /// A usable X11 display: a live server (owned by us or injected) whose
    /// connect actually succeeds — never a dead child sharing the number.
    fn x11_display_or_skip() -> Option<X11Display> {
        use x11rb::rust_connection::RustConnection;
        if let Ok(display) = std::env::var("SHENKO_TEST_X11_DISPLAY") {
            // The port bound only applies when the resolver constructs a
            // TCP address; Unix-only `unix/:60000` is valid.
            let safe = x11rb::reexports::x11rb_protocol::parse_display::parse_display(
                Some(display.as_str()),
            )
            .is_ok_and(|p| {
                let wants_tcp = ((p.protocol.is_none()
                    || p.protocol.as_deref() != Some("unix"))
                    && !p.host.is_empty()
                    && p.host != "unix")
                    || (p.protocol.is_none() && p.host.is_empty());
                !wants_tcp || p.display <= 59535
            });
            if safe && RustConnection::connect(Some(display.as_str())).is_ok() {
                return Some(X11Display { display, owned: None });
            }
        }
        // Parallel test processes must not reserve the same display: spread
        // candidates seeded by this pid, skip numbers with a live server
        // lock or answering socket BEFORE spawn, then admit only when the
        // answering server is provably THIS child — the X lock file is
        // created atomically by Xvfb and carries its PID. The upper bound
        // is 59535, not 65535: the x11rb resolver computes `6000 + display`
        // in u16 for its TCP fallback, so larger numbers panic under
        // overflow checks even when only the unix socket is usable.
        // Residual hazard: two children may still race into spawn; the
        // loser exits or fails to claim the lock and is killed below, so an
        // external server is never killed nor silently adopted.
        let pid = std::process::id() as usize;
        let range = 59535usize - 90;
        for attempt in 0..48usize {
            let n = 90usize + (pid.wrapping_mul(2654435761).wrapping_add(attempt * 7919)) % range;
            let display = format!(":{n}");
            let lock_path = format!("/tmp/.X{n}-lock");
            // Never spawn on a number already reserved or serving.
            if std::path::Path::new(&lock_path).exists()
                || RustConnection::connect(Some(display.as_str())).is_ok()
            {
                continue;
            }
            let mut child = match std::process::Command::new("Xvfb")
                .args([&display, "-screen", "0", "800x600x24"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
                Err(_) => continue,
            };
            // Admit only if THIS child is alive, the endpoint answers, AND
            // the server's lock names our child's PID — socket-connect alone
            // cannot prove ownership under parallel startup.
            for _ in 0..100 {
                match child.try_wait() {
                    Ok(Some(_)) | Err(_) => break, // child exited: not ours
                    Ok(None) => {
                        if RustConnection::connect(Some(display.as_str())).is_ok() {
                            let owned_lock = std::fs::read_to_string(&lock_path)
                                .ok()
                                .and_then(|s| s.trim().parse::<u32>().ok())
                                == Some(child.id());
                            if owned_lock {
                                return Some(X11Display {
                                    display,
                                    owned: Some(XvfbGuard(child)),
                                });
                            }
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        None
    }

    struct ComputerUseFixture {
        store: Arc<shenko_store::store::Store>,
        backend: Arc<shenko_exec_host::ComputerUseBackend>,
        handlers: ComputerUseHandlers,
        dir: std::path::PathBuf,
        /// The one admitted controller instance all fixture tasks delegate
        /// from (session owner / lease holder identity).
        instance_id: EntityId,
    }

    struct ComputerUseHandlers {
        session: shenko_exec_host::ComputerUseSessionHandler,
        observe: shenko_exec_host::ComputerUseObserveHandler,
        lease: shenko_exec_host::ComputerUseLeaseHandler,
        input: shenko_exec_host::ComputerUseInputHandler,
    }

    /// Real Store + committed Pending native tasks through the actual S1
    /// transaction; claims and invocations mirror NativeChildService::process.
    fn computer_use_fixture(
        task_count: usize,
        model_text: &str,
    ) -> (ComputerUseFixture, Vec<EntityId>) {
        let dir = std::env::temp_dir().join(format!("shenko-cu-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let prepared_home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let model_profile = shenko_seed::ModelProfile::from_env();
        bootstrap_genesis(&store, None, &prepared_home.manifest, &model_profile).unwrap();
        store.start_subject_dispatcher().unwrap();
        let task_ids: Vec<EntityId> = (0..task_count).map(|_| EntityId::new()).collect();
        commit_native_fixture_step(&store, model_text, &task_ids, vec![]);
        let backend = Arc::new(shenko_exec_host::ComputerUseBackend::new(store.clone()));
        (
            ComputerUseFixture {
                store,
                backend: backend.clone(),
                handlers: ComputerUseHandlers {
                    session: shenko_exec_host::ComputerUseSessionHandler::new(backend.clone()),
                    observe: shenko_exec_host::ComputerUseObserveHandler::new(backend.clone()),
                    lease: shenko_exec_host::ComputerUseLeaseHandler::new(backend.clone()),
                    input: shenko_exec_host::ComputerUseInputHandler::new(backend.clone()),
                },
                dir,
                instance_id: EntityId::new(),
            },
            task_ids,
        )
    }

    /// Claim a Pending task and run the real handler boundary. `input` is the
    /// exact frozen op input (the committed delegate JSON is only the
    /// admission marker; real dispatch resolves it from the frozen decision).
    async fn drive_native_as(
        fixture: &ComputerUseFixture,
        caller_instance_id: EntityId,
        handler: &dyn shenko_subject::NativeTaskHandler,
        task_id: EntityId,
        input: serde_json::Value,
    ) -> shenko_types::operation::OperationRecord {
        let claim = fixture
            .store
            .claim_native_task(task_id, handler.descriptor().implementation_ref, false)
            .unwrap()
            .expect("pending task claimable");
        let invocation = shenko_subject::NativeTaskInvocation {
            child_tools: None,
            context: shenko_types::agent::CallContext {
                caller_instance_id,
                role_revision: handler.descriptor().role_revision,
                task_id: Some(task_id),
                expected_task_revision: Some(claim.invocation.task_revision.0),
                invocation_id: Some(claim.invocation.invocation_id),
                controller_epoch: claim.invocation.epoch,
                software_generation: claim.software_generation.clone(),
                authority_refs: handler.descriptor().authority_refs,
                trace_id: EntityId::new(),
            },
            task_invocation: claim.invocation.clone(),
            task: claim.task.clone(),
            operation_id: claim.operation_id,
            authority: shenko_types::operation::AuthorityRef {
                delegation_id: task_id,
                delegation_revision: RecordRevision(claim.invocation.task_revision.0),
                decision_ref: claim
                    .task
                    .created_by_step_id
                    .unwrap_or(claim.invocation.invocation_id),
            },
            input,
        };
        handler.execute(invocation).await.unwrap()
    }

    /// Drive as the fixture's single admitted controller instance.
    async fn drive_native(
        fixture: &ComputerUseFixture,
        handler: &dyn shenko_subject::NativeTaskHandler,
        task_id: EntityId,
        input: serde_json::Value,
    ) -> shenko_types::operation::OperationRecord {
        drive_native_as(fixture, fixture.instance_id, handler, task_id, input).await
    }

    /// The sealed normalized result of an operation's evidence (T2 output).
    fn op_normalized(
        fixture: &ComputerUseFixture,
        operation_id: EntityId,
    ) -> serde_json::Value {
        let evidence = fixture
            .store
            .external_operation_evidence(operation_id)
            .unwrap();
        let last = evidence.last().expect("at least one receipt");
        let normalized = last
            .normalized_ref
            .clone()
            .or_else(|| Some(last.raw_ref.clone()))
            .unwrap();
        let bytes = fixture.store.cas.read_object(&normalized.digest).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn computer_use_delegates(specs: &[(u32, &str)]) -> (String, usize) {
        let mut lines = String::new();
        for (version, kind) in specs {
            lines.push_str(&format!(
                "[delegate:{kind}:{version}] {{\"admission\":true}}\n"
            ));
        }
        (lines, specs.len())
    }

    // Permanent suite: 4 test fns, 9 independent behavior cases.
    // Deferred to the ephemeral native probes (C15X11NativePrep fixture):
    // typed-unavailable backend/display, unreachable display, unknown-session
    // inject, cross-display independent leases, bogus observation id,
    // sibling-session non-revoke, session close fencing, store-reopen
    // persistence, dead-display follow-ups, lost-transport partial
    // accounting, dispatch run_once wiring.
    //
    // Cases 1-3: real Xvfb — framebuffer/CAS observation; pointer-move
    // postcondition; raw XTEST button delivery to a real client window with
    // honest ACK-only Partial accounting.
    #[tokio::test]
    async fn computer_use_xvfb_observe_and_real_input() {
        let Some(x11) = x11_display_or_skip() else {
            eprintln!("SKIP computer_use_xvfb_observe_and_real_input: no usable X11 display");
            return;
        };
        use x11rb::{connection::Connection, protocol::xproto, protocol::xproto::ConnectionExt as _};
        let (delegate, _) = computer_use_delegates(&[
            (1, "computer.use.session"),
            (1, "computer.use.observe"),
            (1, "computer.use.lease"),
            (1, "computer.use.input"),
            (1, "computer.use.input"),
        ]);
        let (fixture, ids) = computer_use_fixture(5, &delegate);

        let opened = drive_native(
            &fixture,
            &fixture.handlers.session,
            ids[0],
            serde_json::json!({"backend":"x11","display":x11.display.as_str()}),
        )
        .await;
        assert_eq!(opened.state, OperationState::Resolved);
        let opened_n = op_normalized(&fixture, opened.operation_id);
        assert_eq!(opened_n["result"], "opened");
        let session_id: EntityId = opened_n["session"]["session_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(opened_n["session"]["xtest_version"].is_string());

        // The delivery target window must exist BEFORE the observation is
        // bound — creating it after observe would stale the fingerprint.
        let (client, screen_num) =
            x11rb::rust_connection::RustConnection::connect(Some(x11.display.as_str())).unwrap();
        let screen = &client.setup().roots[screen_num];
        let window = client.generate_id().unwrap();
        client
            .create_window(
                screen.root_depth,
                window,
                screen.root,
                10,
                10,
                120,
                80,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                screen.root_visual,
                &xproto::CreateWindowAux::new()
                    .event_mask(xproto::EventMask::BUTTON_PRESS | xproto::EventMask::EXPOSURE),
            )
            .unwrap()
            .check()
            .unwrap();
        client.map_window(window).unwrap().check().unwrap();
        client.flush().unwrap();

        let observed = drive_native(
            &fixture,
            &fixture.handlers.observe,
            ids[1],
            serde_json::json!({"session_id":session_id}),
        )
        .await;
        assert_eq!(observed.state, OperationState::Resolved);
        let observed_n = op_normalized(&fixture, observed.operation_id);
        assert_eq!(observed_n["result"], "observed");
        let observation_id = observed_n["observation_id"].as_str().unwrap().to_string();
        // The raw pixels are real CAS evidence: exact ZPixmap size bound.
        let raw_ref = &observed_n["observation"]["raw_pixels_ref"];
        let raw_digest = shenko_types::object::Digest(
            raw_ref["digest"].as_str().unwrap().to_string(),
        );
        let raw = fixture.store.cas.read_object(&raw_digest).unwrap();
        assert_eq!(
            raw.len() as u64,
            raw_ref["bytes"].as_u64().unwrap()
        );
        assert!(raw.len() > 800 * 600);

        let lease = drive_native(
            &fixture,
            &fixture.handlers.lease,
            ids[2],
            serde_json::json!({"session_id":session_id,"op":"acquire"}),
        )
        .await;
        let lease_n = op_normalized(&fixture, lease.operation_id);
        assert_eq!(lease_n["result"], "acquired");
        let token: EntityId = lease_n["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

        let moved = drive_native(
            &fixture,
            &fixture.handlers.input,
            ids[3],
            serde_json::json!({
                "session_id":session_id,"lease_token":token,
                "observation_id":observation_id,
                "actions":[{"kind":"pointer_move","x":40,"y":40}],
            }),
        )
        .await;
        assert_eq!(moved.state, OperationState::Resolved);
        assert_eq!(moved.effect_status, EffectStatus::Complete);
        let moved_n = op_normalized(&fixture, moved.operation_id);
        assert_eq!(moved_n["pointer_post"], serde_json::json!([40, 40]));

        let clicked = drive_native(
            &fixture,
            &fixture.handlers.input,
            ids[4],
            serde_json::json!({
                "session_id":session_id,"lease_token":token,
                "observation_id":observation_id,
                "actions":[{"kind":"button","button":1,"press":"tap"}],
            }),
        )
        .await;
        assert_eq!(clicked.state, OperationState::Resolved);
        // Button ACK alone cannot claim an app effect: Partial, not Complete.
        assert_eq!(clicked.effect_status, EffectStatus::Partial);
        // The real client window actually received the press at 40x40.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let press = loop {
            match client.poll_for_event().unwrap() {
                Some(x11rb::protocol::Event::ButtonPress(e)) => break e,
                _ => {
                    assert!(std::time::Instant::now() < deadline, "no real ButtonPress reached client");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        };
        assert_eq!(press.detail, 1);
        assert_eq!((press.event_x, press.event_y), (30, 30));
    }

    // Cases 4-6: one display domain = one writer across ALL sessions; two
    // sessions on the same Xvfb share the lease (a DIFFERENT controller
    // instance cannot acquire), and explicit takeover bumps the epoch so a
    // pre-takeover observation is fenced; the new holder must re-observe
    // before injecting. The old-token rejection is the same revocation code
    // path and is covered by the stale-observation case plus external prep.
    #[tokio::test]
    async fn computer_use_lease_exclusion_and_takeover() {
        let Some(x11) = x11_display_or_skip() else {
            eprintln!("SKIP computer_use_lease_exclusion_and_takeover: no usable X11 display");
            return;
        };
        let (delegate, _) = computer_use_delegates(&[
            (1, "computer.use.session"),
            (1, "computer.use.session"),
            (1, "computer.use.lease"),
            (1, "computer.use.lease"),
            (1, "computer.use.lease"),
            (1, "computer.use.observe"),
            (1, "computer.use.input"),
            (1, "computer.use.observe"),
            (1, "computer.use.input"),
        ]);
        let (fixture, ids) = computer_use_fixture(9, &delegate);
        let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
            serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
        let opened_n = op_normalized(&fixture, opened.operation_id);
        let session_id: EntityId = opened_n["session"]["session_id"].as_str().unwrap().parse().unwrap();
        let domain_id = opened_n["session"]["domain_id"].as_str().unwrap().to_string();

        // A second session on the SAME display binds the same domain.
        let opened_b = drive_native(&fixture, &fixture.handlers.session, ids[1],
            serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
        let opened_b_n = op_normalized(&fixture, opened_b.operation_id);
        let session_b: EntityId = opened_b_n["session"]["session_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(opened_b_n["session"]["domain_id"].as_str().unwrap(), domain_id.as_str(),
            "sessions on one display share the lease domain");

        let a = drive_native(&fixture, &fixture.handlers.lease, ids[2],
            serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
        let a_n = op_normalized(&fixture, a.operation_id);

        // A DIFFERENT controller instance cannot become a writer on the same
        // domain, even through a separate admitted session.
        let conflict = drive_native_as(&fixture, EntityId::new(), &fixture.handlers.lease, ids[3],
            serde_json::json!({"session_id":session_b,"op":"acquire"})).await;
        let conflict_n = op_normalized(&fixture, conflict.operation_id);
        assert_eq!(conflict_n["result"], "rejected");
        assert_eq!(conflict_n["reason"], "lease_conflict");

        let observed = drive_native(&fixture, &fixture.handlers.observe, ids[5],
            serde_json::json!({"session_id":session_id})).await;
        let observation_id = op_normalized(&fixture, observed.operation_id)
            ["observation_id"].as_str().unwrap().to_string();

        // Human takeover: A's token becomes stale; late input is fenced out.
        let human = drive_native(&fixture, &fixture.handlers.lease, ids[4],
            serde_json::json!({"session_id":session_id,"op":"takeover"})).await;
        let human_n = op_normalized(&fixture, human.operation_id);
        assert_eq!(human_n["result"], "taken_over");
        let token_h: EntityId = human_n["lease"]["lease_token"].as_str().unwrap().parse().unwrap();
        assert!(human_n["lease"]["epoch"].as_u64().unwrap() > a_n["lease"]["epoch"].as_u64().unwrap());

        // An observation bound to the PRE-takeover epoch is stale too: the
        // holder must re-observe under the new epoch before injecting.
        let pre_epoch = drive_native(&fixture, &fixture.handlers.input, ids[6],
            serde_json::json!({"session_id":session_id,"lease_token":token_h,
                "observation_id":observation_id,
                "actions":[{"kind":"pointer_move","x":7,"y":7}]})).await;
        assert_eq!(
            op_normalized(&fixture, pre_epoch.operation_id)["reason"],
            "observation_stale_epoch"
        );
        let observed2 = drive_native(&fixture, &fixture.handlers.observe, ids[7],
            serde_json::json!({"session_id":session_id})).await;
        let observation_id2 = op_normalized(&fixture, observed2.operation_id)
            ["observation_id"].as_str().unwrap().to_string();
        let fresh = drive_native(&fixture, &fixture.handlers.input, ids[8],
            serde_json::json!({"session_id":session_id,"lease_token":token_h,
                "observation_id":observation_id2,
                "actions":[{"kind":"pointer_move","x":7,"y":7}]})).await;
        assert_eq!(fresh.state, OperationState::Resolved);
    }

    // Case 7: stale observation is rejected before any event; a changed
    // window layout invalidates the bound coordinates.
    #[tokio::test]
    async fn computer_use_stale_observation_rejected() {
        let Some(x11) = x11_display_or_skip() else {
            eprintln!("SKIP computer_use_stale_observation_rejected: no usable X11 display");
            return;
        };
        use x11rb::{connection::Connection, protocol::xproto, protocol::xproto::ConnectionExt as _};
        let (delegate, _) = computer_use_delegates(&[
            (1, "computer.use.session"),
            (1, "computer.use.observe"),
            (1, "computer.use.lease"),
            (1, "computer.use.input"),
        ]);
        let (fixture, ids) = computer_use_fixture(4, &delegate);
        let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
            serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
        let session_id: EntityId = op_normalized(&fixture, opened.operation_id)
            ["session"]["session_id"].as_str().unwrap().parse().unwrap();
        let observed = drive_native(&fixture, &fixture.handlers.observe, ids[1],
            serde_json::json!({"session_id":session_id})).await;
        let observation_id = op_normalized(&fixture, observed.operation_id)
            ["observation_id"].as_str().unwrap().to_string();
        let lease = drive_native(&fixture, &fixture.handlers.lease, ids[2],
            serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
        let token: EntityId = op_normalized(&fixture, lease.operation_id)
            ["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

        // Change the real window layout after the observation was bound.
        let (client, screen_num) =
            x11rb::rust_connection::RustConnection::connect(Some(x11.display.as_str())).unwrap();
        let screen = &client.setup().roots[screen_num];
        let window = client.generate_id().unwrap();
        client
            .create_window(
                screen.root_depth, window, screen.root, 0, 0, 100, 100, 0,
                xproto::WindowClass::INPUT_OUTPUT, screen.root_visual,
                &xproto::CreateWindowAux::new(),
            )
            .unwrap().check().unwrap();
        client.map_window(window).unwrap().check().unwrap();
        client.flush().unwrap();

        let stale = drive_native(&fixture, &fixture.handlers.input, ids[3],
            serde_json::json!({"session_id":session_id,"lease_token":token,
                "observation_id":observation_id,
                "actions":[{"kind":"pointer_move","x":5,"y":5}]})).await;
        let stale_n = op_normalized(&fixture, stale.operation_id);
        assert_eq!(stale_n["result"], "rejected");
        assert_eq!(stale_n["reason"], "observation_stale_layout");
        assert_eq!(stale.effect_status, EffectStatus::None);
    }

    // Cases 8-9: (a) an armed input op whose controller produced no
    // conclusive evidence reconciles to honest Unknown with no retry;
    // (b) after REAL display loss (our own spawned server killed and reaped)
    // follow-up input is rejected by re-verification, never sent blind.
    // Skipped when the display is borrowed — we only kill servers we own.
    #[tokio::test]
    async fn computer_use_armed_reconcile_and_dead_display_fencing() {
        let Some(mut x11) = x11_display_or_skip() else {
            eprintln!("SKIP computer_use_armed_reconcile_and_dead_display_fencing: no usable X11");
            return;
        };
        let (delegate, _) = computer_use_delegates(&[
            (1, "computer.use.session"),
            (1, "computer.use.observe"),
            (1, "computer.use.lease"),
            (1, "computer.use.input"),
            (1, "computer.use.input"),
        ]);
        let (fixture, ids) = computer_use_fixture(5, &delegate);
        let opened = drive_native(&fixture, &fixture.handlers.session, ids[0],
            serde_json::json!({"backend":"x11","display":x11.display.as_str()})).await;
        let session_id: EntityId = op_normalized(&fixture, opened.operation_id)
            ["session"]["session_id"].as_str().unwrap().parse().unwrap();
        let observed = drive_native(&fixture, &fixture.handlers.observe, ids[1],
            serde_json::json!({"session_id":session_id})).await;
        let observation_id = op_normalized(&fixture, observed.operation_id)
            ["observation_id"].as_str().unwrap().to_string();
        let lease = drive_native(&fixture, &fixture.handlers.lease, ids[2],
            serde_json::json!({"session_id":session_id,"op":"acquire"})).await;
        let token: EntityId = op_normalized(&fixture, lease.operation_id)
            ["lease"]["lease_token"].as_str().unwrap().parse().unwrap();

        // An armed input attempt whose controller never produced a result:
        // reconcile reports Unknown with honest progress, arms no retry.
        let input_desc = shenko_subject::NativeTaskHandler::descriptor(
            &fixture.handlers.input,
        );
        let claim = fixture
            .store
            .claim_native_task(ids[3], input_desc.implementation_ref.clone(), false)
            .unwrap()
            .unwrap();
        let (armed_op, _armed_attempt) = arm_simulated_native_operation(
            &fixture.store,
            &claim,
            input_desc.implementation_ref.clone(),
            "computer.use.input",
        )
        .unwrap();
        let task = fixture.store.external_task(ids[3]).unwrap();
        let reconciled = shenko_subject::NativeTaskHandler::reconcile(
            &fixture.handlers.input,
            &task,
            armed_op.operation_id,
        )
        .await
        .unwrap();
        assert_eq!(reconciled.effect_status, EffectStatus::Unknown);
        assert_eq!(reconciled.state, OperationState::Reconciling);
        assert_eq!(
            fixture
                .store
                .external_operation_attempts(armed_op.operation_id)
                .unwrap()
                .len(),
            1
        );
        // Display lost mid-work: only a server WE spawned is terminated —
        // a borrowed SHENKO_TEST_X11_DISPLAY is never killed, and the
        // dead-display case is then reported as skipped rather than faked.
        let Some(xvfb) = x11.owned.take() else {
            eprintln!("SKIP dead-display fencing: display not owned by this test");
            return;
        };
        xvfb.kill_and_wait();
        let late = drive_native(&fixture, &fixture.handlers.input, ids[4],
            serde_json::json!({"session_id":session_id,"lease_token":token,
                "observation_id":observation_id,
                "actions":[{"kind":"pointer_move","x":9,"y":9},
                           {"kind":"button","button":1,"press":"tap"}]})).await;
        let late_n = op_normalized(&fixture, late.operation_id);
        assert_eq!(late_n["result"], "rejected");
        assert_eq!(late_n["sent_events"].as_u64().unwrap_or(0), 0);
        assert_eq!(late.effect_status, EffectStatus::None);
    }

    /// SPEC §5.4: adoption retires the node's independent self while its
    /// retained experience survives as a recorded branch — but the node
    /// identity and the store's derived subject binding are one installation
    /// fact. An adoption that cannot move them together must refuse before
    /// the node is touched, because succeeding would leave every later
    /// `open_node` failing on `Node/subject branch differs`. Adopting the
    /// bound identity back is the path that re-converges an already diverged
    /// installation.
    #[tokio::test]
    async fn sync_adopt_keeps_subject_binding_coherent() {
        let dir = std::env::temp_dir().join(format!("shenko-adopt-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        let home = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
            data_dir: dir.clone(),
            ..Default::default()
        })
        .prepare(store.ledger.ledger_id())
        .unwrap();
        let agent = bootstrap_genesis(&store, None, &home.manifest, &shenko_seed::ModelProfile::from_env()).unwrap();
        drop(home);
        let bound = node_sync::open_node(&dir, &store)
            .unwrap()
            .local_identity()
            .unwrap()
            .expect("initialized node carries the bound subject identity");
        assert_eq!(bound.agent_id, agent);

        // An adoption the binding cannot follow is refused BEFORE the node
        // records anything: the command reports no success.
        let foreign = shenko_node::Identity {
            agent_id: EntityId::new(),
            branch_id: EntityId::new(),
            origin_proof: shenko_types::object::Digest::blake3(b"trusted peer genesis"),
        };
        let foreign_file = dir.join("foreign-identity.json");
        std::fs::write(&foreign_file, serde_json::to_vec(&foreign).unwrap()).unwrap();
        let refusal = node_sync::run(
            node_sync::SyncCommand::Adopt {
                identity_file: foreign_file,
                trusted_node: EntityId::new(),
                provenance: "test-fixture: operator-provided file".into(),
                reason: "join the peer's existing self".into(),
            },
            dir.clone(),
        )
        .await
        .expect_err("adoption must refuse when the bound subject cannot transition");
        assert!(
            refusal.to_string().contains("sync adopt refused"),
            "unexpected refusal message: {refusal:#}"
        );
        // The refused adoption left the installation exactly as it was:
        // startup still opens, the bound identity is intact, no adoption
        // was recorded.
        let node = node_sync::open_node(&dir, &store).unwrap();
        assert_eq!(node.local_identity().unwrap(), Some(bound.clone()));
        assert!(node
            .branches()
            .unwrap()
            .iter()
            .all(|branch| branch.branch_id != foreign.branch_id));
        drop(node);

        // The historical defect state — node identity adopted while the
        // binding stayed behind — is reproduced at the node level so the
        // command path can prove recovery. The next open sees the real
        // divergence and refuses, as before.
        {
            let mut node = node_sync::open_node(&dir, &store).unwrap();
            node.adopt_identity(shenko_node::AdoptRequest {
                operation_id: EntityId::new(),
                trusted_identity: foreign.clone(),
                trusted_node_id: EntityId::new(),
                provenance: "test-fixture: operator-provided file".into(),
                reason: "reproduce the diverged binding defect".into(),
            })
            .unwrap();
        }
        let divergence = node_sync::open_node(&dir, &store)
            .err()
            .expect("diverged node/subject pair must still be refused");
        assert!(divergence.to_string().contains("Node/subject branch differs"));
        // The startup observation path is equally blocked before recovery.
        assert!(node_sync::observe(&dir, &store, EntityId::new(), None).is_err());

        // Adopting the bound identity back through the real command
        // re-converges the pair and records the retired self's experience.
        let bound_file = dir.join("bound-identity.json");
        std::fs::write(&bound_file, serde_json::to_vec(&bound).unwrap()).unwrap();
        node_sync::run(
            node_sync::SyncCommand::Adopt {
                identity_file: bound_file,
                trusted_node: EntityId::new(),
                provenance: "test-fixture: installation binding projection".into(),
                reason: "re-converge the diverged node/subject pair".into(),
            },
            dir.clone(),
        )
        .await
        .unwrap();
        let node = node_sync::open_node(&dir, &store).unwrap();
        assert_eq!(node.local_identity().unwrap(), Some(bound.clone()));
        // Both retired selves stay auditable: nothing was deleted.
        let retired: Vec<EntityId> = node
            .branches()
            .unwrap()
            .into_iter()
            .filter_map(|branch| {
                (branch.kind == shenko_node::BranchKind::AdoptedExperience)
                    .then_some(branch.branch_id)
            })
            .collect();
        assert!(retired.contains(&bound.branch_id) && retired.contains(&foreign.branch_id));
        // Startup and role observation work again on the recovered pair.
        drop(node);
        node_sync::observe(&dir, &store, EntityId::new(), None).unwrap();
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// With no local subject bound, node-level adoption is the only
    /// consistent state `open_node` can check: it proceeds and the node
    /// carries the adopted self until the subject's own facts arrive.
    #[tokio::test]
    async fn sync_adopt_without_bound_subject_stays_coherent() {
        let dir = std::env::temp_dir().join(format!("shenko-adopt-unbound-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(open_native_fixture(&dir));
        assert!(store.installation_binding().unwrap().subject.is_none());
        let trusted = shenko_node::Identity {
            agent_id: EntityId::new(),
            branch_id: EntityId::new(),
            origin_proof: shenko_types::object::Digest::blake3(b"trusted peer genesis"),
        };
        let identity_file = dir.join("trusted-identity.json");
        std::fs::write(&identity_file, serde_json::to_vec(&trusted).unwrap()).unwrap();
        node_sync::run(
            node_sync::SyncCommand::Adopt {
                identity_file,
                trusted_node: EntityId::new(),
                provenance: "test-fixture: operator-provided file".into(),
                reason: "carry the peer's self until its facts arrive".into(),
            },
            dir.clone(),
        )
        .await
        .unwrap();
        // Every later open stays consistent: still no bound subject, so the
        // binding gate never compares identities.
        let node = node_sync::open_node(&dir, &store).unwrap();
        assert_eq!(node.local_identity().unwrap(), Some(trusted));
        drop(node);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod reply_directive_tests {
    use shenko_types::{ids::EntityId, output::*};
    #[test]
    fn identity_lines_are_actions_not_reply_text() {
        let out = "先想一下\n[reply:group:7]  你好\n[identity:name] 岚\n继续说\n[identity:soul] 记住关系；不要把 delegate 和委派当指令\n[remember] 一整段记忆\n[delegate] {\"goal\":\"完整有限目标\",\"system_prompt\":\"独立子角色\",\"model\":\"fixture-child\",\"max_tokens\":32}\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n[reply:private:9] 第二条";
        let dir = std::env::temp_dir().join(format!("shenko-controls-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
        let raw = store
            .cas
            .seal_object(out.as_bytes(), "text/plain", "test", "text", 1)
            .unwrap()
            .object;
        let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ));
        let parsed =
            shenko_subject::output::parse_output(EntityId::new(), raw.clone(), out, &identity, &shenko_channel_napcat::qq_test_registry(), None)
                .unwrap();
        assert_eq!(parsed.controls.len(), 6); // two replies, two identity, one memory, one task
        let first = &parsed.controls[0];
        let OutputAction::Reply { target, text } = &first.action else {
            panic!("reply")
        };
        assert_eq!(
            *target,
            PublicationTarget::new(shenko_channel_napcat::QQ_CHANNEL_ID, "group", "7", "qq:10000")
                .with_scope(None, Some("7".to_string()))
        );
        assert_eq!(
            text,
            " 你好\n继续说\n> [delegate] 引用，不执行\n[remember] 懒续行仍是引用\n\n[identity:personality] 这是链接旁的说明\n\n[identity:personality]: https://example.invalid/\n\n[delegate](https://example.invalid/)\n[1] 引用编号不是控制行\n```text\n[identity:name] 代码，不执行\n[remember] 代码例子\n```\n"
        );
        assert!(!parsed.text.contains("一整段记忆") && !parsed.text.contains("完整有限目标"));
        assert_eq!(
            &out[first.spans[0].start..first.spans[0].end],
            "[reply:group:7]  你好\n"
        );
        assert_eq!(
            store.cas.read_object(&parsed.raw_ref.digest).unwrap(),
            out.as_bytes()
        );
        // Unsupported old syntax is a diagnosed protocol error, not a fallback.
        let obsolete = store
            .cas
            .seal_object(b"[memory] obsolete", "text/plain", "test", "text", 1)
            .unwrap()
            .object;
        assert!(
            shenko_subject::output::parse_output(
                EntityId::new(),
                obsolete,
                "[memory] obsolete",
                &identity,
                &shenko_channel_napcat::qq_test_registry(),
                None,
            )
            .unwrap_err()
            .to_string()
            .contains("CONTROL_PROTOCOL_INVALID")
        );
    }

    /// The semantic wait protocol is one typed control per result.
    ///
    /// Malformed or duplicate wait controls reject the whole result, while
    /// a quoted or fenced wait line remains literal prose.
    #[test]
    fn semantic_wait_controls_are_typed_and_atomic() {
        use shenko_types::subject::SemanticWaitChange;

        let dir = std::env::temp_dir().join(format!("shenko-wait-{}", EntityId::new()));
        std::fs::create_dir(&dir).unwrap();
        let store = shenko_store::store::Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
        let identity = shenko_identity::IdentityService::new(std::sync::Arc::new(
            shenko_identity::DefaultIdentityPolicy,
        ));
        let parse = |raw: &str| {
            let raw_ref = store
                .cas
                .seal_object(raw.as_bytes(), "text/plain", "test", "text", 1)
                .unwrap()
                .object;
            shenko_subject::output::parse_output(EntityId::new(), raw_ref, raw, &identity, &shenko_channel_napcat::qq_test_registry(), None)
        };

        // A start names the condition and its cap, next to an ordinary action.
        let started = parse(
            "先想清楚再等\n[wait] {\"condition\":\"实验出现足够判断的结果\",\"max_wait_secs\":120}\n[remember] 记住等待条件",
        )
        .unwrap();
        assert_eq!(started.controls.len(), 2);
        let OutputAction::SemanticWait { change } = &started.controls[0].action else {
            panic!("the wait control must be a typed semantic wait action")
        };
        assert_eq!(
            change,
            &SemanticWaitChange::Start {
                condition: "实验出现足够判断的结果".into(),
                max_wait_secs: 120,
            }
        );
        assert!(!started.text.contains("max_wait_secs"));

        // An out-of-range cap rejects the whole result, not just this action.
        let invalid = parse("[remember] 先记下\n[wait] {\"condition\":\"x\",\"max_wait_secs\":601}");
        assert!(invalid.unwrap_err().to_string().contains("CONTROL_PROTOCOL_INVALID"));

        // One wait control per result: a second one rejects the whole result.
        let duplicate = parse(
            "[wait] {\"condition\":\"x\",\"max_wait_secs\":10}\n[wait] {\"condition\":\"y\",\"max_wait_secs\":10}",
        );
        assert!(
            duplicate
                .unwrap_err()
                .to_string()
                .contains("CONTROL_PROTOCOL_INVALID")
        );

        // A quoted wait line is prose, not a control.
        let quoted = parse("> [wait] {\"condition\":\"引用不执行\",\"max_wait_secs\":10}\n").unwrap();
        assert!(quoted.controls.is_empty());
        assert!(quoted.text.contains("[wait] {\"condition\":\"引用不执行\""));

        // A fenced wait line is prose, not a control.
        let fenced = parse("```text\n[wait:end] 代码不执行\n```\n")
        .unwrap();
        assert!(fenced.controls.is_empty());
        assert!(fenced.text.contains("[wait:end]"));
    }
}
