//! Shared Shenko runtime and owned host entrypoints — docs/specs/mobile-runtime.md#spec-24-5, docs/specs/plugins.md#spec-44.
//! CLI and embedded hosts execute this same composed implementation.
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt};

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
mod runtime_napcat;
mod runtime_owner;
mod runtime_plugins;
mod runtime_publications;
mod runtime_sessions;
mod runtime_start;
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
    /// Show or edit the instance's name, soul and personality — docs/specs/subject-runtime.md#spec-6.
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
    /// Manage named WASM tools in the seed's managed tool store — docs/specs/wasm-tools.md#spec-8-2.
    /// Artifacts are real upstream WASI binaries; declared targets with no
    /// verified artifact are reported unresolved, never faked.
    Tool {
        #[command(subcommand)]
        command: cli::ToolCommand,
    },
    /// Show what this installation actually offers right now — docs/specs/plugins.md#spec-18.
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
    /// Inspect and manage the installed context strategies — docs/specs/context-policies.md#spec-12-3-1.
    Context {
        #[command(subcommand)]
        command: cli::ContextCommand,
    },
    /// Owner-set admin facts — docs/specs/channel-adapters.md#spec-16. Admin is a human decision only:
    /// it never follows familiarity, group status, platform roles or
    /// voiceprints, and ordinary users can talk without one.
    Admin {
        #[command(subcommand)]
        command: cli::AdminCommand,
    },
    /// Contact/person directory — docs/specs/contacts.md#spec-15-2, docs/specs/contacts.md#spec-15-2-1. Local owner path to
    /// inspect people and import the owner's own Life identity material;
    /// the running subject reaches the same store through the
    /// person.directory native child.
    Contacts {
        #[command(subcommand)]
        command: cli::ContactsCommand,
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
    /// Software-generation activation — docs/specs/self-update.md#spec-9, docs/specs/durable-state.md#spec-10, docs/specs/plugins.md#spec-44. Build a candidate
    /// on a separate copy, smoke-launch it, swap through the durable
    /// selector, roll back while retaining new facts. Every step is
    /// deterministic and recoverable without a model.
    Update {
        #[command(subcommand)]
        command: cli::UpdateCli,
    },
    /// Cognitive root version chain — docs/specs/cognition-units.md#spec-51-4. Capture, reinstate,
    /// inspect the owned versioned root.
    Root {
        #[command(subcommand)]
        command: cli::RootCli,
    },
}

<<<<<<< HEAD

=======
#[derive(Subcommand, Debug)]
pub enum CompatCommand {
    /// Installed plugins, their enable state and the external units a previous
    /// import recorded. Reads only; never connects or imports anything.
    Status,
    /// Offer or withdraw one independently installed adapter unit.
    Enable { unit_id: String },
    Disable { unit_id: String },
    /// External MCP connections, described by an operator-supplied file.
    #[cfg(feature = "compat-mcp")]
    Mcp {
        #[command(subcommand)]
        command: CompatMcpCommand,
    },
    /// Public Poppy company discovery; account access is separate.
    #[cfg(feature = "compat-poppy")]
    Poppy {
        #[command(subcommand)]
        command: CompatPoppyCommand,
    },
    /// Agent Skills source material, preserved exactly as selected.
    #[cfg(feature = "compat-skill")]
    Skill {
        #[command(subcommand)]
        command: CompatSkillCommand,
    },
}

#[cfg(feature = "compat-poppy")]
#[derive(Subcommand, Debug)]
pub enum CompatPoppyCommand {
    /// Read public discovery metadata as a diagnostic, without installing a
    /// service, sending credentials or creating a commerce workflow.
    Discover { origin: String },
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
    /// Deterministic recovery. No model is required: it completes a committed
    /// publication, takes back an interrupted hand-over boundary when this
    /// process owns the installation, and reports what it found.
    Recover,
    /// docs/specs/cognition-units.md#spec-51-4 append-node rollback: restore the subject's state root to a
    /// historical node. This appends a new node, never rewinding or
    /// truncating the chain. Distinct from `rollback` which switches the
    /// software generation.
    RootRollback {
        /// The canonical `blake3:<64 hex>` digest of the historical root
        /// node to restore.
        #[arg(long)]
        target_node: String,
        /// The chain head digest at selection time; omit for an empty chain.
        #[arg(long)]
        expected_head: Option<String>,
        /// Activation this rollback accompanies, if any.
        #[arg(long)]
        activation: Option<shenko_types::ids::EntityId>,
    },
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
            UpdateCli::RootRollback {
                target_node,
                expected_head,
                activation,
            } => UpdateCommand::RootRollback {
                target_node,
                expected_head,
                activation,
            },
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum RootCli {
    /// Show the current cognitive root head.
    Head,
    /// Show the version chain (newest first).
    Chain {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show a specific root node by revision.
    Node {
        #[arg(long)]
        revision: u64,
    },
    /// Capture the current state into a new root node.
    Capture,
    /// Reinstate a historical root node.
    Reinstate {
        #[arg(long)]
        target_revision: u64,
    },
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
    /// Replace a strategy's configuration at the same compiled identity. A
    /// running process installs the new configuration at its next observation.
    Config {
        id: String,
        /// The configuration document, as JSON.
        config: String,
    },
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
        /// Declare a guest filesystem path this package needs, as `PATH` for
        /// read-only use or `PATH:rw` when it writes. Repeatable. A
        /// declaration is a requirement, never a permission grant: the
        /// executor admits it against the mount closure it actually freezes.
        #[arg(long = "requires-fs")]
        requires_fs: Vec<String>,
        /// Declare an environment variable the package needs supplied per
        /// invocation. Repeatable.
        #[arg(long = "requires-env")]
        requires_env: Vec<String>,
        /// Declare a host facility: `clock`, `random`, `host_import:<module>`
        /// or `network:<host>:<port>[:connect,listen,accept]`. Repeatable.
        #[arg(long = "requires-host")]
        requires_host: Vec<String>,
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
        /// Declare a guest filesystem path every artifact of the set needs, as
        /// `PATH` (read-only) or `PATH:rw`. Repeatable.
        #[arg(long = "requires-fs")]
        requires_fs: Vec<String>,
        /// Declare an environment variable every artifact of the set needs
        /// supplied per invocation. Repeatable.
        #[arg(long = "requires-env")]
        requires_env: Vec<String>,
        /// Declare a host facility the set needs: `clock`, `random`,
        /// `host_import:<module>` or
        /// `network:<host>:<port>[:connect,listen,accept]`. Repeatable.
        #[arg(long = "requires-host")]
        requires_host: Vec<String>,
    },
    /// Environment generations: the locked package closure this installation
    /// executes. Preparation resolves and pins a closure; activation is the
    /// single linearization point that makes it current; rollback selects the
    /// previous generation without deleting a version, a generation or any
    /// other fact.
    Environment {
        #[command(subcommand)]
        command: ToolEnvironmentCommand,
    },
    /// Walk a metapackage's declared members, resolve each against installed
    /// units, check adoption via the content/attitude ledger, and return the
    /// membership standing of each. Pure read.
    Metapackage {
        /// The metapackage unit id.
        #[arg(long)]
        unit_id: String,
    },
    /// Walk a distribution's preinstalled macros, target-access expectations,
    /// and capability-provider defaults. Pure read: declarations are facts
    /// about the distribution version, not grants of local readiness.
    Distribution {
        /// The distribution unit id.
        #[arg(long)]
        unit_id: String,
    },
    /// Resolve one named capability-contract slot across the installed
    /// providers on this node. Pure read.
    Slot {
        /// The metapackage unit id.
        #[arg(long)]
        macro_id: String,
        /// The capability contract, e.g. `cap.taste.projection/1`.
        #[arg(long)]
        contract: String,
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

/// Environment-generation subcommands — the Nix-profile surface of the tool
/// package closure.
#[derive(Subcommand, Debug)]
pub enum ToolEnvironmentCommand {
    /// Show the selector, the recorded generations, the append-only activation
    /// log and the closure the current generation pins. Pure read.
    Status,
    /// Resolve the closure of these roots and record it as a locked
    /// generation. Nothing becomes current until it is activated.
    Prepare {
        /// `name`, `name@version`, or a `tool.<name>` unit id. Omitted means
        /// every enabled installed tool — the Nix-profile meaning of "the
        /// environment".
        #[arg(long = "root")]
        root: Vec<String>,
    },
    /// Publish a prepared generation as the current environment. The write is
    /// a compare-and-swap on the selector revision that was read.
    Activate {
        #[arg(long)]
        generation: u64,
        #[arg(long)]
        expected_revision: u64,
    },
    /// Select the previous generation of the current environment. New facts
    /// produced after the switch are never rolled back by this.
    Rollback {
        #[arg(long)]
        expected_revision: u64,
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
    /// The one configuration service: the panel routes and the subject's
    /// `config.manage` tool share it, so a change made one way is the state
    /// the other observes. docs/specs/configuration-playground.md#spec-43-1.
    config_service: Arc<shenko_ui::config_service::ConfigService>,
    /// The one Playground instance: the subject's `playground.manage` child
    /// and the Web UI's artifact routes write the same lifecycle (docs/specs/configuration-playground.md#spec-57-6).
    playground_state: Arc<shenko_ui::playground::PlaygroundState>,
}

/// The media services one process publishes: the typed ingress every provider
/// (and the `/api/media` routes) admits through, and the live runtime the
/// session, recording, speech and speaker-identity engines hang off.
struct MediaServices {
    ingress: Arc<shenko_media::MediaIngress>,
    runtime: Arc<shenko_media::runtime::MediaRuntime>,
}

/// Construct and publish the real media services on the shared Cordis root —
/// docs/specs/contacts.md#spec-15-2, docs/specs/media.md#spec-20.
///
/// This is the single construction path, and it runs before the native
/// definitions that bind to it. The running agent passes its live subject
/// inbox so an admitted media fact wakes the same subject; capability
/// discovery publishes the same pair so a definition it reports as installed
/// really has the service behind it. Nothing here is a stub: the runtime holds
/// the engine bindings loaded from `<data_dir>/media/providers.json`, and an
/// absent file means no engine is configured — which the handlers report as a
/// typed refusal at call time rather than as a fake capability.
async fn spawn_media_services(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::store::Store>,
    data_dir: &std::path::Path,
    agent_id: shenko_types::ids::EntityId,
    inbox: tokio::sync::mpsc::Sender<shenko_types::inbox::InboxEvent>,
) -> Result<MediaServices> {
    let _media_ingress_plugin = supervisor
        .spawn(
            shenko_media::MediaIngressPlugin,
            (Arc::clone(store), agent_id, inbox),
        )
        .await?;
    // Engine bindings are configuration, not model input: a malformed binding
    // is a typed error here instead of a silently engine-less runtime.
    let media_providers =
        shenko_media::runtime::MediaProviders::load(data_dir, Arc::clone(&store.cas))?;
    let _media_runtime_plugin = supervisor
        .spawn(
            shenko_media::MediaRuntimePlugin,
            (
                supervisor
                    .context()
                    .try_service::<shenko_media::MediaIngress>()?,
                Arc::clone(store),
                media_providers,
            ),
        )
        .await?;
    Ok(MediaServices {
        ingress: supervisor
            .context()
            .try_service::<shenko_media::MediaIngress>()?,
        runtime: supervisor
            .context()
            .try_service::<shenko_media::runtime::MediaRuntime>()?,
    })
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
    media: &MediaServices,
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
    let printf_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HostPrintfHandler::new(Arc::clone(store)));
    let _native_printf_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, printf_handler)
        .await?;
    let history_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::HistoryHandler::new(Arc::clone(store)));
    let _native_history_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, history_handler)
        .await?;
    if let Some(handler) = document_runtime::configured_handler(Arc::clone(store))? {
        supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    // Reachability is observed, not assumed: one bounded probe of a declared
    // endpoint with per-stage measured timings and no persistent state.
    let net_probe_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::NetProbeHandler::new(Arc::clone(store)));
    let _native_net_probe_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, net_probe_handler)
        .await?;
    let memory_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::MemoryHandler::new(Arc::clone(store)));
    let _native_memory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, memory_handler)
        .await?;
    for handler in shenko_exec_host::research_handlers(Arc::clone(store)) {
        supervisor.spawn(shenko_subject::NativeTaskHandlerPlugin, handler).await?;
    }
    // Runtime-error awareness — docs/specs/cognition-units.md#spec-45-14: errors.read exposes the
    // subject's own committed runtime_error facts; issue.submit writes
    // subject-authored drafts into the local issues outbox only.
    let errors_read_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::ErrorsReadHandler::new(Arc::clone(store)));
    let _native_errors_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, errors_read_handler)
        .await?;
    let issue_submit_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
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
    let person_directory_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_exec_host::PersonDirectoryHandler::new(Arc::clone(store)));
    let _native_person_directory_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, person_directory_handler)
        .await?;
    let media_read_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_media::MediaReadHandler::new(Arc::clone(store)));
    let _media_read_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, media_read_handler)
        .await?;
    // Speaker identification of one already-sealed segment. The handler binds
    // to the SAME live media runtime the UI routes and provider plugins use,
    // which is why the media services are constructed before this install.
    let media_identify_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_media::MediaIdentifyHandler::new(
            Arc::clone(store),
            Arc::clone(&media.runtime),
        ),
    );
    let _media_identify_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, media_identify_handler)
        .await?;
    // Built-in, replaceable self-explanation — docs/specs/cognition-units.md#spec-38, R124. The
    // provider is seeded from compiled-in material now; the real
    // `self://runtime/capabilities` snapshot is installed below once
    // the native and awareness registries are populated.
    let self_knowledge = Arc::new(shenko_awareness_core::SelfKnowledgeProvider::new(
        option_env!("SHENKO_SOURCE_REVISION").unwrap_or("unbound"),
        "0.6.2",
    ));
    let self_knowledge_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
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
    let playground_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_ui::PlaygroundBackendHandler::new(Arc::clone(store), data_dir.join("playground"))
            .with_services(Arc::clone(&artifact_services)),
    );
    let _native_playground_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, playground_handler)
        .await?;
    // The subject's own hands on its works — docs/specs/configuration-playground.md#spec-57-6. One Playground
    // instance serves the subject and the Web UI, so a version the subject
    // writes is the version a human sees.
    let playground_state = Arc::new(
        shenko_ui::playground::PlaygroundState::open(data_dir.join("playground"), Arc::clone(store))?,
    );
    let playground_manage_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
        shenko_ui::playground_manage::PlaygroundManageHandler::new(
            Arc::clone(store),
            Arc::clone(&playground_state),
        ),
    );
    let _native_playground_manage_plugin = supervisor
        .spawn(
            shenko_subject::NativeTaskHandlerPlugin,
            playground_manage_handler,
        )
        .await?;
    // Configuration through conversation — docs/specs/configuration-playground.md#spec-43-1, R133. The handler runs
    // the same ConfigService the panel routes expose: one durable change
    // ledger, one set of consumer adoptions, no approval gate and no second
    // configuration store. Its built-in namespaces are registered by the
    // service itself, and every plugin may add its own.
    let config_service = Arc::new(shenko_ui::config_service::ConfigService::new(
        Arc::clone(store),
    ));
    let config_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(shenko_ui::ConfigHandler::new(
            Arc::clone(store),
            Arc::clone(&config_service),
        ));
    let _native_config_plugin = supervisor
        .spawn(shenko_subject::NativeTaskHandlerPlugin, config_handler)
        .await?;
    // Desktop and VM handlers share one provider registry, lease authority
    // and session state across tasks.
    let computer_use_backend =
        Arc::new(shenko_exec_host::ComputerUseBackend::new(Arc::clone(store)));
    let computer_use_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 6] = [
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
    let browser_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 5] = [
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
    let mobile_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 6] = [
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
    let wasi_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
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
    let repl_session_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
        Arc::new(repl_session_handler);
    let repl_cell_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> =
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
    // SSH PTY, managed OMP/Claude seats and seat question/answer operations
    // share one live seat owner.
    let seat_runtime = Arc::new(shenko_exec_ssh::seat::runtime::SeatRuntime::new(Arc::clone(store)));
    let seat_ssh_handlers: [Arc<dyn shenko_kernel::task::NativeTaskHandler>; 7] = [
        Arc::new(shenko_exec_ssh::SshExecHandler::new(Arc::clone(store))),
        Arc::new(shenko_exec_ssh::SshPtyHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::OmpSeatHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
        Arc::new(shenko_exec_ssh::ClaudeSeatHandler::new(
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
        Arc::new(shenko_exec_ssh::SeatFleetHandler::new(
            Arc::clone(store),
            Arc::clone(&seat_runtime),
        )),
    ];
    for handler in seat_ssh_handlers {
        let _seat_ssh_plugin = supervisor
            .spawn(shenko_subject::NativeTaskHandlerPlugin, handler)
            .await?;
    }
    // R070: durable seat records outlive this process. Rebind every active
    // record whose transport survives a restart (a local tmux pane) so the
    // subject can observe and answer questions recorded before the restart;
    // managed processes and PTYs stay durable-only. A record the runtime
    // cannot reach is reported, never fabricated or retired here.
    match seat_runtime.reattach_active_seats().await {
        Ok(report) => tracing::info!(
            reattached = report.reattached.len(),
            incarnation_bumped = report.incarnation_bumped.len(),
            skipped = report.skipped.len(),
            "seat runtime reattached durable seat records"
        ),
        Err(error) => tracing::warn!(
            error = format!("{error:#}"),
            "seat runtime reattach scan failed; durable seat records remain the authority"
        ),
    }
    let native_child = supervisor
        .context()
        .try_service::<shenko_subject::NativeChildService>()?;
    playground_state.with_native(Arc::clone(&native_child));
    handoff_participants.register_admission_barrier(Arc::new(update_runtime::NativeWorkers(
        Arc::clone(&native_child),
    )));
    let agent_id = store.get_existing_agent_id()?.context("no initialized subject")?;
    let update_handler: Arc<dyn shenko_kernel::task::NativeTaskHandler> = Arc::new(
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
        config_service,
        playground_state,
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
>>>>>>> 3a58c69f9495251a8a8946dfd8a96f6bda3d5cf8
/// Parse the process CLI and enter the same owned host used by embeddings.
pub fn run_cli() -> Result<()> {
    let cli = cli::Cli::parse();

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
        let restart = runtime_authority::restart_command(&command);
        let runtime = tokio::runtime::Runtime::new()?;
        let result = runtime.block_on(runtime_lifecycle::run(command, data_dir.clone()));
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
