//! Process options and subordinate CLI command schemas.
//! The shared host command remains crate::Command for native embeddings.
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use crate::Command;

#[derive(Parser, Debug)]
#[command(
    name = "shenko",
    about = "Shenko（深刻）— personal agent framework",
    version = "0.1.0"
)]
pub(crate) struct Cli {
    #[arg(long, env = "SHENKO_DATA_DIR")]
    pub(crate) data_dir: Option<PathBuf>,
    #[arg(long, default_value = "info", env = "SHENKO_LOG")]
    pub(crate) log: String,
    #[command(subcommand)]
    pub(crate) command: Command,
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
