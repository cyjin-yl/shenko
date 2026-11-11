//! Named WASM tool installation and execution — docs/specs/wasm-tools.md#spec-8-2, docs/specs/wasm-home.md#spec-8-3, docs/specs/wasm-tools.md#spec-18-8/R008/R101.
//!
//! A tool is a real upstream artifact (WASI command module bytes) with a
//! declared name, version, ABI, source and content digest. Installation
//! authority is the node's existing installation-unit inventory
//! (`shenko_node::Node::handle_unit_request(UnitRequest::Import)`): the unit
//! record, its sealed artifact bytes and the materialized
//! `<data>/tools/units/<unit>/<revision>/<digest>/package.bin` all live
//! through that one path, so `node_sync` replication carries tools exactly
//! like every other code unit — no second registry exists to diverge.
//!
//! Every execution re-reads the unit inventory of *this* installation and
//! re-verifies the artifact bytes against the recorded digest inside the node
//! object store, so a foreign directory dropped at `<data>/tools` can never
//! substitute for an installed record, and an artifact that does not match
//! its descriptor never executes.
//!
//! Named product targets (toybox, gcc-wasm, git, gh, tea, ssh) are declared in
//! [`NAMED_TARGETS`] with their actual acquisition state. A target with no
//! verified WASI artifact stays unresolved with the honest reason; resolving
//! it fails with `TOOL_UNSUPPORTED`, and it is never advertised as installed.
//! Host-CLI relays, imitation WATs and manifest-only entries are not named
//! targets by construction: only real artifact bytes admitted below produce a
//! runnable record.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_node::{Node, ToolUnitFact, UnitKind};
use shenko_types::object::Digest;
use std::collections::{BTreeMap, BTreeSet};
use wasmtime::{Engine, Extern, ExternType, FuncType, Linker, Module, Store};
use wasmtime_wasi::p1;
use wasmtime_wasi::{WasiCtxBuilder, p1::WasiP1Ctx};

/// Subdirectory of the seed data dir used as the tool materialization root
/// (`UnitRequest::Import{package_root}`). It holds only bytes the unit
/// authority already verified; dropping foreign files there changes nothing
/// because execution always resolves through the node inventory.
pub const TOOL_DIR_NAME: &str = "tools";
/// `unit_id` prefix for installed tools. The tool name after the prefix is
/// the caller-facing resolution name.
pub const TOOL_UNIT_PREFIX: &str = "tool.";
/// Upper bound on a single artifact the host will admit. Larger tools need a
/// deliberate profile decision and a raised bound, never a silent truncation.
pub const TOOL_ARTIFACT_MAX_BYTES: u64 = shenko_node::MAX_ARTIFACT_BYTES as u64;

/// An explicit invocation choice, never inferred from a tool name. ReadOnly
/// preserves the admitted mount sources/guest paths but withdraws every guest
/// write permission; it does not change the persistent home declaration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMountProfile {
    #[default]
    ManagedHome,
    ReadOnly,
}

impl ToolMountProfile {
    pub fn resolve(self, declared: &[crate::MountSpec]) -> Vec<crate::MountSpec> {
        declared.iter().map(|mount| {
            let mut actual = mount.clone();
            if self == Self::ReadOnly {
                // Withdraw the guest's write permission without relabelling
                // the mount: a managed COW mount must keep showing the
                // branch's overlay over its base, so a read-only profile is
                // a read-only *view of the branch*, not a direct mount over
                // the base (docs/specs/wasm-home.md#spec-8-3 / A08).
                actual.withdraw_write();
            }
            actual
        }).collect()
    }
}

/// The ABI an artifact was built for. Only a WASI preview1 *command* module
/// (function imports all supplied by the preview1 linker, exporting a
/// `() -> ()` `_start` and `memory`) can execute through
/// [`crate::WasmRuntime::run_wasi_module`]; anything else is recorded
/// honestly and rejected at execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAbi {
    WasiPreview1,
    /// Preview2/component-model artifact — needs a component runtime adapter
    /// this runtime does not provide.
    WasiComponent,
    /// WASIX extension ABI — needs a WASIX-capable engine (e.g. Wasmer);
    /// wasmtime-45 preview1 cannot supply the imports.
    Wasix,
    /// Dedicated host-import module such as wasi-gh: the WASM holds the logic
    /// but requires extra host services we do not implement.
    HostImports,
    /// Anything else, or ABI not yet determined.
    Other,
}

impl ToolAbi {
    pub fn name(&self) -> &'static str {
        match self {
            Self::WasiPreview1 => "wasi_preview1",
            Self::WasiComponent => "wasi_component",
            Self::Wasix => "wasix",
            Self::HostImports => "host_imports",
            Self::Other => "other",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "wasi_preview1" => Self::WasiPreview1,
            "wasi_component" => Self::WasiComponent,
            "wasix" => Self::Wasix,
            "host_imports" => Self::HostImports,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// Where the artifact's upstream content came from. `uri` is informational
/// provenance; the digest is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSource {
    pub uri: String,
    /// Upstream version/release tag as published (e.g. "0.8.15").
    pub release: String,
    /// How the artifact was obtained: "upstream_release" for a downloaded
    /// published binary, "source_build:<toolchain>" for a local port build.
    pub provenance: String,
}

/// The tool manifest stored inside a unit's declared `config.tool`. These are
/// the unit author's claims; admission verifies the executable ones
/// (`abi`, `imports`) against the actual bytes before the record is written.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDescriptor {
    pub name: String,
    pub version: String,
    pub abi: ToolAbi,
    /// Guest path used as argv[0] convention / entrypoint label, e.g.
    /// "/tools/toybox". Free-form, informational.
    pub entrypoint: String,
    pub source: ToolSource,
    /// Import module/function pairs observed at admission (e.g.
    /// "wasi_snapshot_preview1.fd_write"). Recorded evidence, not a promise.
    #[serde(default)]
    pub imports: Vec<String>,
    /// SPDX license identifier of the upstream project the artifact comes
    /// from. Recorded so an installed tool never loses the license it is
    /// distributed under.
    #[serde(default)]
    pub license: Option<String>,
    /// Where the artifact bytes live and how they are identified. Absent for
    /// units written before out-of-line artifacts existed; those are inline
    /// by definition.
    #[serde(default)]
    pub artifact: Option<crate::artifact::ToolArtifactRef>,
    /// The capability requirements this package declares — docs/specs/self-update.md#spec-9
    /// "权限需求". They are requirements, never grants: they are admitted
    /// against what the executor actually hands the guest (the frozen mount
    /// closure and the explicit invocation environment), and a version
    /// written before declarations existed stays
    /// [`shenko_node::PackageDeclaration::unrecorded`] rather than inheriting
    /// a fabricated empty or blanket declaration.
    #[serde(default)]
    pub capabilities: shenko_node::PackageDeclaration,
}

/// Result of inspecting the actual module bytes: what the artifact really is
/// and whether this runtime can execute it. `runnable` is derived from the
/// real ABI — every function import resolvable with an exact signature match
/// against the preview1 linker — never from names or caller claims.
#[derive(Debug, Clone)]
pub struct ModuleInspection {
    /// ABI detected from the bytes.
    pub abi: ToolAbi,
    /// True iff every import is a `wasi_snapshot_preview1` function with an
    /// exactly matching linker signature, and `_start`/`memory` exports have
    /// the required kinds and signatures.
    pub runnable: bool,
    /// `module.name` pairs actually imported.
    pub imports: Vec<String>,
    /// Why `runnable` is false (missing export, foreign import, type
    /// mismatch); `None` when runnable.
    pub detail: Option<String>,
}

fn func_type_eq(a: &FuncType, b: &FuncType) -> bool {
    // Wasmtime 45 `ValType` does not implement `PartialEq`; `FuncType::matches`
    // is the API's explicit type-equality check.
    a.matches(b)
}

/// Classify real artifact bytes. Binary modules only: WAT text and random
/// files are not installable identities. A component-model binary is
/// classified `WasiComponent` even though it never links under preview1.
pub fn inspect_module(artifact: &[u8]) -> Result<ModuleInspection> {
    ensure!(
        artifact.starts_with(b"\0asm"),
        "artifact is not a WASM binary module"
    );
    let engine = Engine::default();
    let module = match Module::new(&engine, artifact) {
        Ok(module) => module,
        Err(core_error) => {
            // Component binaries are not core modules; recognize them so they
            // can be catalogued honestly instead of failing admission.
            return match wasmtime::component::Component::new(&engine, artifact) {
                Ok(_) => Ok(ModuleInspection {
                    abi: ToolAbi::WasiComponent,
                    runnable: false,
                    imports: Vec::new(),
                    detail: Some(format!(
                        "component-model module; no preview1 command ABI ({core_error})"
                    )),
                }),
                Err(_) => {
                    bail!("not a valid WASM module or component: {core_error}")
                }
            };
        }
    };
    // Build the real preview1 linker this runtime uses: an import is runnable
    // only if the linker actually provides that exact function signature,
    // not merely an import whose module name spells "wasi_snapshot_preview1".
    let mut linker: Linker<WasiP1Ctx> = Linker::new(&engine);
    p1::add_to_linker_sync(&mut linker, |cx| cx)
        .map_err(|e| anyhow::anyhow!("configure wasi preview1 linker: {e}"))?;
    let mut store = Store::new(&engine, WasiCtxBuilder::new().build_p1());

    let mut import_modules = Vec::new();
    let mut imports_sorted = Vec::new();
    let mut detail: Option<String> = None;
    let mut preview1_only = true;
    for import in module.imports() {
        import_modules.push(import.module().to_string());
        imports_sorted.push(format!("{}.{}", import.module(), import.name()));
        let bad = match (import.module(), import.ty()) {
            ("wasi_snapshot_preview1", ExternType::Func(typed)) => {
                match linker.get(&mut store, import.module(), import.name()) {
                    Ok(Extern::Func(func)) if func_type_eq(&func.ty(&store), &typed) => None,
                    Ok(Extern::Func(func)) => Some(format!(
                        "import wasi_snapshot_preview1.{} has type {:?}, linker provides {:?}",
                        import.name(),
                        typed,
                        func.ty(&store)
                    )),
                    Ok(_) => Some(format!(
                        "import wasi_snapshot_preview1.{} is not a function",
                        import.name()
                    )),
                    Err(_) => Some(format!(
                        "import wasi_snapshot_preview1.{} is not provided by this runtime's linker",
                        import.name()
                    )),
                }
            }
            (module, _) => {
                preview1_only = false;
                Some(format!("foreign import {module}.{}", import.name()))
            }
        };
        if detail.is_none() {
            detail = bad;
        }
    }
    imports_sorted.sort();
    let abi = if preview1_only && !import_modules.is_empty() {
        ToolAbi::WasiPreview1
    } else if import_modules.is_empty() {
        ToolAbi::Other
    } else {
        ToolAbi::HostImports
    };
    match module.get_export("_start") {
        Some(ExternType::Func(typed))
            if typed.params().len() == 0 && typed.results().len() == 0 => {}
        Some(other) => {
            if detail.is_none() {
                detail = Some(format!("_start export is {other:?}, expected a () -> () function"));
            }
        }
        None => {
            if detail.is_none() {
                detail = Some("no _start export (WASI command modules export _start)".into());
            }
        }
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(_)) => {}
        Some(other) => {
            if detail.is_none() {
                detail = Some(format!("memory export is {other:?}, expected a memory"));
            }
        }
        None => {
            if detail.is_none() {
                detail = Some("no memory export".into());
            }
        }
    }
    Ok(ModuleInspection {
        runnable: preview1_only && detail.is_none(),
        abi,
        imports: imports_sorted,
        detail,
    })
}

/// A declared named product target and its real state. This is the honest
/// docs/specs/wasm-tools.md#spec-8-2 list in code: status reflects whether a verified runnable artifact is
/// installed *right now*, not whether the name is "supported" on paper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedToolTarget {
    pub name: &'static str,
    /// Which installed tools count as this target.
    pub matches: TargetMatch,
    /// The upstream project a real artifact would come from.
    pub upstream: &'static str,
    /// SPDX license of that upstream project. An installed tool keeps it.
    pub license: &'static str,
    /// The docs/specs/wasm-tools.md#spec-8-2 compatibility class the target really has: W0 standard
    /// component, W1 extended POSIX userland, W2 dedicated host imports, H
    /// host adaptation, R remote environment.
    pub compatibility: &'static str,
    /// Host imports or runtime features the artifact needs beyond WASI
    /// preview1. Empty when preview1 alone is enough.
    pub required_capabilities: &'static [&'static str],
    /// None while no verified upstream artifact exists for our runtime.
    pub unresolved_reason: Option<&'static str>,
}

/// How an installed tool counts towards a named target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetMatch {
    /// The installed tool's name is exactly this name.
    Exact(&'static str),
    /// The target is a ported *set*: the installed tools are named
    /// `<prefix><applet>`, and the target counts as installed when at least
    /// one applet of the set is installed.
    Set(&'static str),
}

impl TargetMatch {
    /// Whether an installed tool's name satisfies this target.
    pub fn accepts(&self, tool_name: &str) -> bool {
        match self {
            Self::Exact(name) => tool_name == *name,
            Self::Set(prefix) => tool_name.starts_with(prefix),
        }
    }
}

/// Declared named targets. Order is the docs/specs/wasm-tools.md#spec-8-2 order.
pub const NAMED_TARGETS: &[NamedToolTarget] = &[
    NamedToolTarget {
        name: "toybox",
        matches: TargetMatch::Set("toybox-"),
        upstream: "landley/toybox 0.8.15",
        license: "BSD-3-Clause",
        compatibility: "W0",
        required_capabilities: &[],
        unresolved_reason: Some(
            "the toybox 0.8.15 port's applets exist and the tool-set manifest \
             ships in crates/shenko-wasm/ports/toybox-wasi/, but no toybox \
             applet is installed in this installation yet; install the set \
             with `shenko tool install-set`",
        ),
    },
    NamedToolTarget {
        name: "gcc-wasm",
        matches: TargetMatch::Exact("gcc-wasm"),
        upstream: "wasi-sdk 34 (clang/lld) and GCC hosted/target builds",
        license: "Apache-2.0 WITH LLVM-exception (clang), GPL-3.0-or-later (gcc)",
        compatibility: "W0",
        required_capabilities: &[],
        unresolved_reason: Some(
            "no verified WASM-hosted compiler artifact exists yet; the \
             retained wasi-sdk toolchain runs on the host and *produces* \
             wasm32-wasip1 modules, which is the target side of docs/specs/wasm-tools.md#spec-8-2, \
             not a compiler that runs inside WASM. This target stays open \
             until a real artifact exists",
        ),
    },
    NamedToolTarget {
        name: "git",
        matches: TargetMatch::Exact("git"),
        upstream: "git/git",
        license: "GPL-2.0-only",
        compatibility: "W2",
        required_capabilities: &["cap.network.tcp/1", "cap.process.deny/1"],
        unresolved_reason: Some(
            "no verified wasm32-wasip1 Git client artifact exists yet. This \
             runtime now supplies the missing transport — admitted, \
             target-bound stream sockets (shenko_wasm::NetworkAdmission; guest \
             shim in crates/shenko-wasm/ports/wasi-net/) — so clone/pull/push \
             over http(s) no longer lack sockets. What is still missing is the \
             port: the wasip1 sysroot has no fork/exec, so the credential \
             helper chain and the dashed builtin dispatch must run in-module, \
             and https needs a wasm-built crypto backend. The acquisition and \
             build recipe, with the upstream pin resolved from the project's \
             own repository at acquisition time, is \
             crates/shenko-wasm/ports/git-wasi/. This target stays open until \
             a real artifact exists",
        ),
    },
    NamedToolTarget {
        name: "gh",
        matches: TargetMatch::Exact("gh"),
        upstream: "cli/cli",
        license: "MIT",
        compatibility: "W2",
        required_capabilities: &["cap.network.tcp/1", "cap.network.dns/1", "cap.process.deny/1"],
        unresolved_reason: Some(
            "no verified artifact exists yet: cli/cli is Go, and a \
             GOOS=wasip1 build links without socket support, so its API calls \
             cannot work as built. The preview1 transport now exists \
             (crates/shenko-wasm/ports/wasi-net/), so gh needs a wasip1 build \
             that actually uses admitted sockets, and it still has no \
             substitute for its browser, keyring, editor and aggregation \
             helpers. Acquisition and the open port work are described in \
             crates/shenko-wasm/ports/gh-wasi/. This target stays open until a \
             real artifact exists",
        ),
    },
    NamedToolTarget {
        name: "tea",
        matches: TargetMatch::Exact("tea"),
        upstream: "go-gitea/gitea tea",
        license: "MIT",
        compatibility: "W2",
        required_capabilities: &["cap.network.tcp/1", "cap.network.dns/1", "cap.process.deny/1"],
        unresolved_reason: Some(
            "no verified Gitea CLI WASM artifact exists yet: go-gitea/gitea's \
             tea is Go, and a GOOS=wasip1 build links without sockets, so its \
             API calls cannot work as built; it needs a wasip1 build over the \
             admitted preview1 transport \
             (crates/shenko-wasm/ports/wasi-net/). Acquisition and the open \
             port work are described in \
             crates/shenko-wasm/ports/tea-wasi/. This target stays open until \
             a real artifact exists",
        ),
    },
    NamedToolTarget {
        name: "ssh",
        matches: TargetMatch::Exact("ssh"),
        upstream: "openssh/openssh (WASIX port) / ssh WASM client",
        license: "BSD-3-Clause (OpenSSH)",
        compatibility: "W1",
        required_capabilities: &["cap.network.tcp/1", "cap.random/1", "cap.clock/1"],
        unresolved_reason: Some(
            "no wasm32-wasip1 OpenSSH artifact exists yet. The WASIX ports \
             need a WASIX-capable engine, and this runtime is wasmtime-45 \
             preview1 (docs/specs/wasm-tools.md#spec-8-2 W1, R019); the host ssh client is not a WASM \
             SSH. A real SSH client must run the protocol and authentication \
             inside the module with the host supplying only admitted sockets, \
             getrandom entropy and clock_time_get — all three exist now — so \
             what remains is the port itself, not host capability. Acquisition \
             and the open port work are described in \
             crates/shenko-wasm/ports/openssh-wasi/. This target stays open \
             until a real artifact exists",
        ),
    },
];

pub fn validate_tool_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')),
        "TOOL_NAME_INVALID: {name:?} must be 1..64 chars of [A-Za-z0-9._+-]"
    );
    Ok(())
}

fn named_target(name: &str) -> Option<&'static NamedToolTarget> {
    NAMED_TARGETS.iter().find(|target| target.name == name)
}

/// One installed tool as resolved from the installation-unit inventory. The
/// unit tables are the registry of record; this is a per-read projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRecord {
    pub name: String,
    pub version: String,
    pub abi: ToolAbi,
    pub entrypoint: String,
    pub source: ToolSource,
    pub digest: Digest,
    pub imports: Vec<String>,
    pub enabled: bool,
    /// The unit inventory's current version for this tool name. It is a fact
    /// about the inventory, not about any environment generation.
    pub current: bool,
    /// True when this version is what a name-only lookup resolves. With an
    /// active environment generation that is the generation's pin; an
    /// installation that never activated one is honestly unpinned and this
    /// mirrors `current`.
    pub selected: bool,
    /// The active environment generation that pins this version, if any.
    pub environment: Option<ToolEnvironment>,
    /// The capability requirements that apply to this version.
    ///
    /// When the version is pinned by an active generation this is the
    /// declaration that generation froze, so a reimport of the same artifact
    /// identity cannot change the permission surface of a selected
    /// environment. Otherwise it is what the retained unit version authored;
    /// a version outside the environment has no immutable authority and is
    /// reported as such by `environment: None`.
    pub capabilities: shenko_node::PackageDeclaration,
    pub unit_id: String,
    /// Where the artifact bytes live. The digest here is the blake3 identity
    /// of the module bytes; the upstream receipt's sha256 is provenance and
    /// never an identity.
    pub artifact: crate::artifact::ToolArtifactRef,
    /// SPDX license identifier of the upstream project, when recorded.
    pub license: Option<String>,
    /// Host-side materialized path (`<tools>/units/.../package.bin`) when the
    /// materialization completed; informational — execution reads the
    /// verified object bytes, never this copy.
    pub materialized_path: Option<String>,
}

/// The active environment generation that pins one tool version.
///
/// It is recorded in every execution receipt, so a later activation or
/// rollback cannot rewrite what an already-running invocation resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEnvironment {
    /// Monotonic generation number of the locking closure.
    pub generation: u64,
    /// blake3 identity of the locked closure document.
    pub digest: Digest,
    /// The selector revision that published this generation.
    pub selector_revision: u64,
    /// `root` or `dependency` inside that closure.
    pub role: String,
}

fn record_from_fact(fact: &ToolUnitFact) -> Result<ToolRecord> {
    ensure!(
        fact.kind == UnitKind::Code,
        "TOOL_RECORD_CORRUPT:{}: unit kind {} is not code",
        fact.unit_id,
        fact.kind.as_str()
    );
    let descriptor: ToolDescriptor = serde_json::from_value(fact.tool.clone())
        .with_context(|| format!("TOOL_RECORD_CORRUPT:{}: tool manifest", fact.unit_id))?;
    validate_tool_name(&descriptor.name)?;
    ensure!(fact.unit_id == format!("{TOOL_UNIT_PREFIX}{}", descriptor.name)
        && fact.revision == descriptor.version,
        "TOOL_RECORD_CORRUPT:{}: tool manifest does not match its unit identity", fact.unit_id);
    // A unit written before out-of-line artifacts existed carries no
    // artifact reference: its bytes are the node's inline object for the
    // unit's own digest. The length is not recorded by the unit fact, so
    // the size check is skipped and only the digest is verified.
    let artifact = descriptor.artifact.clone().unwrap_or_else(|| {
        crate::artifact::ToolArtifactRef::node_inline(fact.artifact_digest.clone(), 0, None)
    });
    // The declaration that applies to this version: the copy the active
    // generation froze when it pins this version, otherwise the declaration
    // the retained version authored. A version outside the environment has no
    // immutable authority, and an absent declaration stays unrecorded.
    let capabilities = match &fact.environment {
        Some(pin) => pin.declarations.clone(),
        None => descriptor.capabilities.normalized(),
    };
    Ok(ToolRecord {
        name: descriptor.name,
        version: descriptor.version,
        abi: descriptor.abi,
        entrypoint: descriptor.entrypoint,
        source: descriptor.source,
        digest: fact.artifact_digest.clone(),
        imports: descriptor.imports,
        enabled: fact.enabled,
        current: fact.current,
        selected: fact.selected,
        environment: fact.environment.as_ref().map(|pin| ToolEnvironment {
            generation: pin.generation,
            digest: pin.generation_digest.clone(),
            selector_revision: pin.selector_revision,
            role: pin.role.as_str().to_string(),
        }),
        capabilities,
        unit_id: fact.unit_id.clone(),
        artifact,
        license: descriptor.license.clone(),
        materialized_path: fact.materialization_path.clone(),
    })
}

/// Facts of every installed tool, newest state first — one call of
/// `Node::tool_unit_facts`, projected to records.
pub fn installed_tools(node: &Node) -> Result<Vec<ToolRecord>> {
    node.tool_unit_facts()?
        .iter()
        .map(record_from_fact)
        .collect()
}

/// The real state of one declared named target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedTargetStatus {
    pub name: String,
    pub upstream: String,
    pub license: String,
    /// The docs/specs/wasm-tools.md#spec-8-2 compatibility class the target really has.
    pub compatibility: String,
    /// Host imports or runtime features needed beyond WASI preview1.
    pub required_capabilities: Vec<String>,
    /// `installed` when a verified artifact is installed right now,
    /// `unresolved` otherwise.
    pub state: String,
    /// The installed artifacts (`name@version digest`), empty when
    /// unresolved.
    pub installed: Vec<String>,
    /// Why the target is unresolved, empty when installed.
    pub reason: String,
}

/// Real availability of every declared named target right now.
pub fn named_target_report(records: &[ToolRecord]) -> BTreeMap<String, String> {
    named_target_status(records)
        .into_iter()
        .map(|status| {
            let detail = if status.state == "installed" {
                format!("installed: {}", status.installed.join(", "))
            } else {
                let joined;
                let needed = if status.required_capabilities.is_empty() {
                    "nothing beyond wasi preview1"
                } else {
                    joined = status.required_capabilities.join(", ");
                    &joined
                };
                format!(
                    "unresolved [{}, needs {}]: {}",
                    status.compatibility,
                    needed,
                    status.reason
                )
            };
            (status.name, detail)
        })
        .collect()
}

/// The machine-readable state of every declared named target.
pub fn named_target_status(records: &[ToolRecord]) -> Vec<NamedTargetStatus> {
    NAMED_TARGETS
        .iter()
        .map(|target| {
            let installed: Vec<String> = records
                .iter()
                // Only a version the installation actually selects counts as
                // present. A retained version outside the active generation is
                // installed storage, not a working target here.
                .filter(|record| record.selected && target.matches.accepts(&record.name))
                .map(|record| {
                    format!(
                        "{}@{} {} ({}, {})",
                        record.name,
                        record.version,
                        record.digest,
                        record.artifact.kind.name(),
                        record.license.as_deref().unwrap_or("license-unrecorded")
                    )
                })
                .collect();
            let (state, reason) = if installed.is_empty() {
                (
                    "unresolved".to_string(),
                    target.unresolved_reason.unwrap_or("unresolved").to_string(),
                )
            } else {
                ("installed".to_string(), String::new())
            };
            NamedTargetStatus {
                name: target.name.to_string(),
                upstream: target.upstream.to_string(),
                license: target.license.to_string(),
                compatibility: target.compatibility.to_string(),
                required_capabilities: target
                    .required_capabilities
                    .iter()
                    .map(|capability| capability.to_string())
                    .collect(),
                state,
                installed,
                reason,
            }
        })
        .collect()
}

/// Resolve a tool name (optionally `name@version`) to its installed record.
/// Errors are explicit and stable:
///
/// - `TOOL_UNSUPPORTED:<name>`  — a declared named target with no
///   installed artifact; detail carries the honest reason.
/// - `TOOL_UNAVAILABLE:<name>`  — unknown or uninstalled name.
/// - `TOOL_NOT_SELECTED:<name>` — versions of this name are installed but the
///   active environment generation selects none of them; the caller can name
///   one explicitly or activate a generation that pins it.
/// - `TOOL_AMBIGUOUS:<name>`    — several versions installed; the caller
///   must pass an explicit version.
/// - `TOOL_ABI_UNSUPPORTED:<name>` — installed but not executable under this
///   runtime (catalogued foreign-ABI records stay honest metadata).
///
/// A name-only lookup follows the environment: with an active generation the
/// selected version is its pin, so an upgrade happens by activating a
/// generation that pins the new version and a rollback happens by activating
/// the older one. A deliberate `name@version` still reaches any retained
/// version and reports `environment: None` in its record, so the receipt says
/// the run was outside the generation.
pub fn resolve_tool<'f>(
    facts: &'f [ToolUnitFact],
    spec: &str,
) -> Result<(ToolRecord, &'f ToolUnitFact)> {
    let (name, version) = match spec.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (spec, None),
    };
    validate_tool_name(name)?;
    let mut matched = Vec::new();
    for fact in facts {
        if !fact.unit_id.starts_with(TOOL_UNIT_PREFIX) {
            continue;
        }
        let record = record_from_fact(fact)?;
        if record.name == name && version.map_or(record.selected, |v| record.version == v) {
            matched.push((record, fact));
        }
    }
    let (record, fact) = match matched.len() {
        0 => {
            if let Some(target) = named_target(name) {
                // A target that is a ported set is installed under the names
                // of its applets; asking for the set's own name must say
                // which applets exist, not claim the target is unsupported.
                let set_members: Vec<String> = facts
                    .iter()
                    .filter_map(|fact| record_from_fact(fact).ok())
                    .filter(|record| target.matches.accepts(&record.name))
                    .map(|record| record.name)
                    .collect();
                ensure!(
                    set_members.is_empty(),
                    "TOOL_UNAVAILABLE:{spec}: {name} is an installed tool set; name one of \
                     its tools, for example {}",
                    set_members.first().map(String::as_str).unwrap_or(name)
                );
                let any_version = facts.iter().filter_map(|f| record_from_fact(f).ok()).any(|r| r.name == name);
                ensure!(
                    !any_version,
                    "TOOL_UNAVAILABLE:{spec}: named target {name} is installed only at other version(s)"
                );
                bail!(
                    "TOOL_UNSUPPORTED:{name}: declared target has no installed \
                     artifact — {}",
                    target.unresolved_reason.unwrap_or("unresolved")
                );
            }
            let installed: Vec<String> = facts
                .iter()
                .filter_map(|fact| record_from_fact(fact).ok())
                .filter(|record| record.name == name)
                .map(|record| format!("{}@{}", record.name, record.version))
                .collect();
            ensure!(
                installed.is_empty(),
                "TOOL_NOT_SELECTED:{name}: {} is installed on this node but the active \
                 environment generation does not select it; name it explicitly as \
                 name@version, or prepare and activate a generation that pins it",
                installed.join(", ")
            );
            bail!("TOOL_UNAVAILABLE:{spec}: no installed WASI tool by that name")
        }
        1 => matched.remove(0),
        _ => bail!(
            "TOOL_AMBIGUOUS:{name}: {} versions installed; pass name@version",
            matched.len()
        ),
    };
    // Installed under a foreign ABI is catalogued, never executed: only a
    // preview1 command module can actually run in this runtime.
    ensure!(record.enabled, "TOOL_DISABLED:{name}: installed unit is disabled");
    ensure!(
        record.abi == ToolAbi::WasiPreview1,
        "TOOL_ABI_UNSUPPORTED:{}: installed ABI {} cannot execute under \
         wasmtime preview1 ({} imports {:?})",
        record.name,
        record.abi.name(),
        record.name,
        record.imports
    );
    Ok((record, fact))
}

/// Fetch and verify the selected unit's executable bytes from the node's
/// inline unit object store. Generic package imports/replication may carry
/// authored metadata, so recheck the real ABI at execution too; imported
/// claims cannot acquire preview1 authority. Returns the verified bytes plus
/// the *actual* inspection — the imports a sealed execution record reports
/// must come from this, never from the authored `config.tool` descriptor
/// which can lie.
pub fn tool_module(node: &Node, record: &ToolRecord) -> Result<(Vec<u8>, ModuleInspection)> {
    ensure!(
        record.artifact.kind == crate::artifact::ToolArtifactKind::NodeInline,
        "TOOL_ARTIFACT_OUT_OF_LINE:{}: this tool's bytes live in the installation's content \
         store; resolve it through a content store, not the inline unit store",
        record.name
    );
    let bytes = node.unit_object_bytes(&record.digest)?;
    ensure!(bytes.len() as u64 <= TOOL_ARTIFACT_MAX_BYTES, "TOOL_ARTIFACT_TOO_LARGE:{}", record.name);
    record.artifact.verify_bytes(&bytes)?;
    let inspection = inspect_module(&bytes)?;
    ensure!(inspection.runnable && record.abi == ToolAbi::WasiPreview1,
        "TOOL_ABI_UNSUPPORTED:{}: {:?}", record.name, inspection.detail);
    ensure!(
        declared_imports_match(&record.imports, &inspection.imports),
        "TOOL_IMPORT_MISMATCH:{}: declared imports {:?} do not match the \
         module's actual imports {:?}",
        record.name,
        record.imports,
        inspection.imports
    );
    Ok((bytes, inspection))
}

/// Fetch and verify the selected unit's executable bytes from wherever the
/// artifact actually lives: the node's inline unit object store, or the
/// installation's content store for an artifact that was staged out of
/// line. The bytes are verified against the artifact's recorded identity
/// before they can reach a guest, and the real ABI is re-inspected, so an
/// authored claim never becomes execution authority.
pub fn tool_module_with_store(
    node: &Node,
    record: &ToolRecord,
    store: &dyn crate::artifact::ToolContentStore,
) -> Result<(Vec<u8>, ModuleInspection)> {
    let bytes = crate::artifact::resolve_artifact_bytes(node, &record.digest, &record.artifact, store)?;
    let inspection = inspect_module(&bytes)?;
    ensure!(inspection.runnable && record.abi == ToolAbi::WasiPreview1,
        "TOOL_ABI_UNSUPPORTED:{}: {:?}", record.name, inspection.detail);
    // The authored descriptor's import list is metadata, not authority; a
    // mismatch means the record does not describe these bytes.
    let mut declared = record.imports.clone();
    let mut actual = inspection.imports.clone();
    declared.sort();
    actual.sort();
    ensure!(
        declared == actual,
        "TOOL_IMPORT_MISMATCH:{}: declared imports {:?} do not match the \
         module's actual imports {:?}",
        record.name,
        declared,
        actual
    );
    Ok((bytes, inspection))
}

// -------------------------------------------------- capability admission ---

/// What this runtime actually hands a guest for one invocation: the frozen
/// mount closure, the explicit environment entries and the fixed facilities
/// the preview1 host provides. It is built from the grant, never from the
/// package's claims.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantedCapabilities {
    /// `(guest_path, writable)` for every preopened mount, exactly as admitted.
    pub mounts: Vec<(String, bool)>,
    /// Environment variable names supplied for this invocation.
    pub environment: BTreeSet<String>,
    /// Wall/monotonic clock functions — part of the preview1 host.
    pub clock: bool,
    /// Entropy — part of the preview1 host.
    pub random: bool,
    /// Network targets this invocation actually admitted. `None` means the
    /// network transport is not registered for this run; a package that
    /// requires network is then unsupported rather than run under an invented
    /// empty admission.
    pub network: Option<Vec<shenko_node::NetworkTarget>>,
    /// Host import modules other than `wasi_snapshot_preview1` this run links.
    pub host_imports: BTreeSet<String>,
}

impl GrantedCapabilities {
    /// The grant implied by one execution: the resolved mount profile and the
    /// explicit environment. Clock and entropy are always provided by the
    /// preview1 host; sockets and foreign host imports are not part of this
    /// linker unless a caller explicitly admits them.
    pub fn of_invocation(
        mounts: &[crate::MountSpec],
        environment: &BTreeMap<String, String>,
    ) -> Self {
        Self {
            mounts: mounts
                .iter()
                .map(|mount| (mount.guest_path.clone(), mount.writable))
                .collect(),
            environment: environment.keys().cloned().collect(),
            clock: true,
            random: true,
            network: None,
            host_imports: BTreeSet::new(),
        }
    }

    /// Same grant with the network targets this invocation admitted.
    pub fn with_network(mut self, targets: Vec<shenko_node::NetworkTarget>) -> Self {
        self.network = Some(targets);
        self
    }
}

/// Admit one package declaration against the grant the guest actually gets.
///
/// Returns one message per unmet requirement, sorted. An `unrecorded`
/// declaration produces none: absence is not a claim, and the run keeps
/// exactly the profile's grant — neither more nor less. Every message is
/// derived from a typed declaration field and a real resource, never from a
/// tool name, a keyword list or a model judgement.
pub fn capability_problems(
    declaration: &shenko_node::PackageDeclaration,
    granted: &GrantedCapabilities,
) -> Vec<String> {
    let Some(capabilities) = declaration.capabilities() else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for requirement in &capabilities.filesystem {
        match writable_cover(granted, &requirement.guest_path) {
            Some(writable) if requirement.write && !writable => problems.push(format!(
                "TOOL_MOUNT_READ_ONLY:{}",
                requirement.guest_path
            )),
            Some(_) => {}
            None => problems.push(format!("TOOL_MOUNT_MISSING:{}", requirement.guest_path)),
        }
    }
    for name in &capabilities.environment {
        if !granted.environment.contains(name) {
            problems.push(format!("TOOL_ENV_MISSING:{name}"));
        }
    }
    for requirement in &capabilities.host {
        match requirement {
            shenko_node::HostRequirement::Clock {} if !granted.clock => {
                problems.push("TOOL_CAPABILITY_UNSUPPORTED:host:clock".into())
            }
            shenko_node::HostRequirement::Random {} if !granted.random => {
                problems.push("TOOL_CAPABILITY_UNSUPPORTED:host:random".into())
            }
            shenko_node::HostRequirement::HostImport { module }
                if !granted.host_imports.contains(module) =>
            {
                problems.push(format!("TOOL_CAPABILITY_UNSUPPORTED:host_import:{module}"))
            }
            shenko_node::HostRequirement::Network { targets } => {
                match &granted.network {
                    None => problems.push(
                        "TOOL_CAPABILITY_UNSUPPORTED:network: this run has no network \
                         transport registered"
                            .into(),
                    ),
                    Some(admitted) => {
                        for target in targets {
                            if !network_target_admitted(admitted, target) {
                                problems.push(format!(
                                    "TOOL_NETWORK_NOT_ADMITTED:{}:{}",
                                    target.host, target.port
                                ));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// Whether a preopened guest path covers the required path, and whether that
/// cover is writable. `/home` covers `/home` and `/home/work`, never
/// `/homework`; a writable cover outranks a read-only one.
fn writable_cover(granted: &GrantedCapabilities, required: &str) -> Option<bool> {
    granted
        .mounts
        .iter()
        .filter(|(guest_path, _)| {
            required == guest_path
                || required
                    .strip_prefix(guest_path.as_str())
                    .is_some_and(|suffix| guest_path == "/" || suffix.starts_with('/'))
        })
        .map(|(_, writable)| *writable)
        .max()
}

/// Whether one admitted target covers a required target: same host, same port,
/// same resolution mode, and every required operation admitted. A broader
/// admission never stands in for a narrower requirement, and nothing is
/// defaulted.
fn network_target_admitted(
    admitted: &[shenko_node::NetworkTarget],
    required: &shenko_node::NetworkTarget,
) -> bool {
    admitted.iter().any(|target| {
        target.host == required.host
            && target.port == required.port
            && target.resolve == required.resolve
            && required
                .operations
                .iter()
                .all(|operation| target.operations.contains(operation))
    })
}

/// The host import modules the artifact's real bytes name beyond
/// `wasi_snapshot_preview1`, read out of the observed `module.function`
/// import list.
pub fn foreign_import_modules(imports: &[String]) -> BTreeSet<String> {
    imports
        .iter()
        .filter_map(|import| import.split_once('.').map(|(module, _)| module.to_string()))
        .filter(|module| module != "wasi_snapshot_preview1")
        .collect()
}

/// Admit a declaration against the artifact's *actual* imports.
///
/// Only provable facts are checked: a declared host import module must be one
/// the bytes really import, so a manifest cannot claim a host facility the
/// artifact never reaches. An `unrecorded` declaration makes no claim and is
/// not checked.
pub fn declaration_import_problems(
    declaration: &shenko_node::PackageDeclaration,
    imports: &[String],
) -> Vec<String> {
    let Some(capabilities) = declaration.capabilities() else {
        return Vec::new();
    };
    let mut problems = Vec::new();
    for requirement in &capabilities.host {
        if let shenko_node::HostRequirement::HostImport { module } = requirement {
            let prefix = format!("{module}.");
            if !imports.iter().any(|import| import.starts_with(&prefix)) {
                problems.push(format!(
                    "TOOL_CAPABILITY_MISMATCH: declared host import module {module} is \
                     not imported by the artifact"
                ));
            }
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The authored import list and the module's actual imports must agree.
fn declared_imports_match(declared: &[String], actual: &[String]) -> bool {
    let mut declared = declared.to_vec();
    let mut actual = actual.to_vec();
    declared.sort();
    actual.sort();
    declared == actual
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MountMode, MountSpec, WasiCommand, WasmRuntime, HOME_GUEST_PATH};
    use shenko_node::{UnitDescriptor, UnitExport, UnitPackage, UnitRequest};
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;
    use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("shenko-tools-{tag}-{}", shenko_types::ids::EntityId::new()))
    }

    /// A real preview1 command compiled to actual binary module bytes. It
    /// writes "tool-out\n" to stdout and creates `wrote-by-tool` under the
    /// first preopen (the home mount) — argv/streams/file effects in one.
    fn echo_writer_wasm() -> Vec<u8> {
        let wat = r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "tool-out\n")
              (data (i32.const 140) "wrote-by-tool")
              (data (i32.const 200) "x")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const 9))
                (if (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 91))))
                (i32.store (i32.const 0) (i32.const 140))
                (i32.store (i32.const 4) (i32.const 13))
                (if (i32.ne
                      (call $path_open
                        (i32.const 3) (i32.const 0) (i32.const 140) (i32.const 13)
                        (i32.const 9) (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 20))
                      (i32.const 0))
                  (then (call $exit (i32.const 92))))
                (i32.store (i32.const 0) (i32.const 200))
                (i32.store (i32.const 4) (i32.const 1))
                (if (call $fd_write (i32.load (i32.const 20)) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 93))))
              )
            )
        "#;
        wat::parse_str(wat).expect("fixture module must assemble")
    }

    /// Byte-level ABI proof: a real preview1 command module is runnable with
    /// its exact imports recorded; a `_start` with the wrong signature or a
    /// module importing a foreign host module are detected from the actual
    /// ABI, never from export names alone.
    #[test]
    fn inspection_proves_real_abi() -> Result<()> {
        let ok = inspect_module(&echo_writer_wasm())?;
        assert!(ok.runnable, "{:?}", ok.detail);
        assert_eq!(ok.abi, ToolAbi::WasiPreview1);
        assert!(ok.imports.contains(&"wasi_snapshot_preview1.fd_write".to_string()));

        let wrong_start = wat::parse_str(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
                (memory (export "memory") 1)
                (func (export "_start") (param i32) (call $exit (i32.const 0))))"#,
        )?;
        let bad = inspect_module(&wrong_start)?;
        assert!(!bad.runnable);
        assert!(bad.detail.unwrap().contains("_start"), "wrong signature must be reported");

        let foreign = wat::parse_str(
            r#"(module
                (import "env" "custom" (func $custom))
                (memory (export "memory") 1)
                (func (export "_start") (call $custom)))"#,
        )?;
        let bad = inspect_module(&foreign)?;
        assert!(!bad.runnable);
        assert_eq!(bad.abi, ToolAbi::HostImports);
        assert!(bad.imports.contains(&"env.custom".to_string()));
        Ok(())
    }

    /// The full authority path: import a tool through the unit authority
    /// (UnitRequest::Import, package_root = <data>/tools), then resolve it
    /// from the read-only inventory, verify the sealed bytes and execute it
    /// through the real runtime — argv, streams and the guest-visible file
    /// write all observable; reopening still resolves the same digest.
    #[test]
    fn installed_tool_unit_executes() -> Result<()> {
        let root = temp_dir("roundtrip");
        let node_dir = root.join("node");
        let home = temp_dir("roundtrip-home");
        std::fs::create_dir_all(&home)?;
        let wasm = echo_writer_wasm();
        let digest = Digest::blake3(&wasm);
        let mut node = Node::open(&node_dir)?;
        node.handle(shenko_node::Request::Initialize {
            node_id: shenko_types::ids::EntityId::new(),
            identity: None,
        })?;
        let export = UnitExport {
            schema: shenko_node::UNIT_SCHEMA,
            root_unit_id: "tool.fixture.echo".into(),
            root_revision: Some("1.0.0".into()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled: true,
            packages: vec![UnitPackage {
                unit: UnitDescriptor {
                    unit_id: "tool.fixture.echo".into(),
                    kind: UnitKind::Code,
                    revision: "1.0.0".into(),
                    artifact_digest: digest.clone(),
                    provides: vec![],
                    dependencies: vec![],
                    config: serde_json::json!({"tool": {
                        "name": "fixture.echo",
                        "version": "1.0.0",
                        "abi": "wasi_preview1",
                        "entrypoint": "/tools/fixture.echo",
                        "source": {"uri": "builtin:test", "release": "1.0.0", "provenance": "test_fixture"},
                        "imports": [
                            "wasi_snapshot_preview1.fd_write",
                            "wasi_snapshot_preview1.path_open",
                            "wasi_snapshot_preview1.proc_exit"
                        ],
                    }}),
                    applicability: None,
                },
                artifact_base64: B64_STD.encode(&wasm),
                evidence: vec![],
                resolved_dependencies: None,
            }],
        };
        let tools_root = root.join(TOOL_DIR_NAME);
        node.handle_unit_request(UnitRequest::Import {
            export,
            package_root: tools_root.to_string_lossy().into_owned(),
            enable: Some(true),
        })?;

        // Resolve from a fresh read-only inspection of the same installation.
        let inspect = Node::open_inspect(&node_dir)?.expect("node exists");
        let facts = inspect.tool_unit_facts()?;
        let (record, _) = resolve_tool(&facts, "fixture.echo")?;
        assert_eq!(record.version, "1.0.0");
        // The artifact of this fixture is carried inline by its unit, so the
        // inline resolution is the one that applies.
        let (bytes, inspection) = tool_module(&inspect, &record)?;
        assert_eq!(Digest::blake3(&bytes), digest);
        assert_eq!(inspection.imports, record.imports);

        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let stdout = MemoryOutputPipe::new(4096);
        let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty());
        command.arguments = vec!["fixture.echo".into()];
        let outcome = rt.run_wasi_module(&bytes, command)?;
        assert_eq!(outcome, crate::WasiRunOutcome::Exited(0));
        assert_eq!(stdout.contents().as_ref(), b"tool-out\n");
        assert!(home.join("wrote-by-tool").exists(), "guest write must persist");

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&home).ok();
        Ok(())
    }

    /// A real preview1 command module whose bytes differ per tag, so two
    /// versions of the same tool are genuinely different artifacts.
    fn tagged_command(tag: &str) -> Vec<u8> {
        let wat = format!(
            r#"
            (module
              (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
              (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
              (memory (export "memory") 1)
              (data (i32.const 100) "{tag}\n")
              (func (export "_start")
                (i32.store (i32.const 0) (i32.const 100))
                (i32.store (i32.const 4) (i32.const {len}))
                (drop (call $fd_write (i32.const 1) (i32.const 0) (i32.const 1) (i32.const 8)))
                (call $exit (i32.const 0))))
            "#,
            tag = tag,
            len = tag.len() + 1
        );
        wat::parse_str(wat).expect("fixture module must assemble")
    }

    fn exact_dependency(
        edge_id: &str,
        unit_id: &str,
        requirement: &str,
    ) -> shenko_node::DependencySpec {
        shenko_node::DependencySpec {
            edge_id: edge_id.to_string(),
            strength: shenko_node::DependencyStrength::Required,
            purpose: None,
            selector: shenko_node::DependencySelector::Exact {
                unit_id: unit_id.to_string(),
                revision_requirement: Some(requirement.to_string()),
                artifact_digest: None,
                contract_requirement: None,
            },
        }
    }

    fn tool_export(
        name: &str,
        version: &str,
        module: &[u8],
        dependencies: Vec<shenko_node::DependencySpec>,
    ) -> UnitExport {
        let digest = Digest::blake3(module);
        let unit_id = format!("tool.{name}");
        let entrypoint = format!("/tools/{name}");
        UnitExport {
            schema: shenko_node::UNIT_SCHEMA,
            root_unit_id: unit_id.clone(),
            root_revision: Some(version.to_string()),
            root_artifact_digest: Some(digest.clone()),
            origin_node: None,
            enabled: true,
            packages: vec![UnitPackage {
                unit: UnitDescriptor {
                    unit_id,
                    kind: UnitKind::Code,
                    revision: version.to_string(),
                    artifact_digest: digest,
                    provides: vec![format!("tool:{name}")],
                    dependencies,
                    config: serde_json::json!({"tool": {
                        "name": name,
                        "version": version,
                        "abi": "wasi_preview1",
                        "entrypoint": entrypoint,
                        "source": {
                            "uri": "builtin:test",
                            "release": version,
                            "provenance": "test_fixture"
                        },
                        "imports": [
                            "wasi_snapshot_preview1.fd_write",
                            "wasi_snapshot_preview1.proc_exit"
                        ],
                    }}),
                    applicability: None,
                },
                artifact_base64: B64_STD.encode(module),
                evidence: vec![],
                resolved_dependencies: None,
            }],
        }
    }

    fn generation_number(prepared: &serde_json::Value) -> Result<u64> {
        prepared
            .get("generation")
            .and_then(|record| record.get("generation"))
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("prepared environment is missing its generation number"))
    }

    fn import_tool(node: &mut Node, tools_root: &str, export: UnitExport) -> Result<()> {
        node.handle_unit_request(UnitRequest::Import {
            export,
            package_root: tools_root.to_string(),
            enable: Some(true),
        })?;
        Ok(())
    }

    /// The real generation lifecycle against the real node store: a closure is
    /// locked with its dependency, published once, a newer version is stored
    /// without silently replacing the selection, an explicit pin upgrades it,
    /// and a rollback selects the earlier generation again — every pinned
    /// version, generation and activation staying durable.
    #[test]
    fn environment_generation_pins_closure_and_rolls_back() -> Result<()> {
        let root = temp_dir("env-generation");
        let tools_root = root.join(TOOL_DIR_NAME).to_string_lossy().into_owned();
        let mut node = Node::open(&root.join("node"))?;
        node.handle(shenko_node::Request::Initialize {
            node_id: shenko_types::ids::EntityId::new(),
            identity: None,
        })?;
        let dep_v1 = tagged_command("dep-one");
        let dep_v2 = tagged_command("dep-two");
        let user = tagged_command("user-one");
        import_tool(&mut node, &tools_root, tool_export("dep", "1.0.0", &dep_v1, vec![]))?;
        import_tool(&mut node, &tools_root, tool_export(
            "user",
            "1.0.0",
            &user,
            vec![exact_dependency("needs-dep", "tool.dep", ">=1.0.0")],
        ))?;

        // Preparation locks the retained, resolved closure. It selects nothing:
        // an installation keeps its current pointers until an activation.
        let prepared = node.handle_unit_request(UnitRequest::PrepareEnvironment {
            roots: vec![shenko_node::EnvironmentRoot {
                unit_id: "tool.user".to_string(),
                revision: None,
                artifact_digest: None,
            }],
        })?;
        let first = generation_number(&prepared)?;
        assert!(
            node.environment_selector()?.is_none(),
            "preparing a generation must not select it"
        );
        let locked = node
            .environment_generation(first)?
            .ok_or_else(|| anyhow::anyhow!("prepared generation was not recorded"))?;
        assert_eq!(locked.roots, vec!["tool.user".to_string()]);
        assert_eq!(
            locked
                .members
                .iter()
                .map(|member| (member.unit_id.as_str(), member.role.as_str()))
                .collect::<Vec<_>>(),
            vec![("tool.dep", "dependency"), ("tool.user", "root")]
        );

        // Activation is the one linearization point.
        let activated =
            node.handle_unit_request(UnitRequest::ActivateEnvironment {
                generation: first,
                expected_revision: 0,
            })?;
        assert_eq!(activated["selector"]["revision"].as_u64(), Some(1));
        let facts = node.tool_unit_facts()?;
        let (user_record, _) = resolve_tool(&facts, "user")?;
        assert_eq!(user_record.environment.as_ref().map(|pin| pin.generation), Some(first));
        assert_eq!(user_record.environment.as_ref().map(|pin| pin.role.as_str()), Some("root"));
        let (dep_record, _) = resolve_tool(&facts, "dep")?;
        assert_eq!(dep_record.version, "1.0.0");
        assert_eq!(
            dep_record.environment.as_ref().map(|pin| pin.role.as_str()),
            Some("dependency")
        );

        // Storing another version changes no selection: the environment is the
        // authority, and an explicit version outside it is reported as such.
        import_tool(&mut node, &tools_root, tool_export("dep", "2.0.0", &dep_v2, vec![]))?;
        let facts = node.tool_unit_facts()?;
        assert_eq!(
            resolve_tool(&facts, "dep")?.0.version,
            "1.0.0",
            "an import must not silently upgrade the active environment"
        );
        assert_eq!(
            resolve_tool(&facts, "dep@2.0.0")?.0.environment,
            None,
            "a version outside the generation must be reported as outside it"
        );

        // Upgrade: pin both roots explicitly. The retained root's recorded
        // selection is re-solved onto the pinned dependency because that
        // version still satisfies the declaration.
        let upgraded = node.handle_unit_request(UnitRequest::PrepareEnvironment {
            roots: vec![
                shenko_node::EnvironmentRoot {
                    unit_id: "tool.dep".to_string(),
                    revision: Some("2.0.0".to_string()),
                    artifact_digest: None,
                },
                shenko_node::EnvironmentRoot {
                    unit_id: "tool.user".to_string(),
                    revision: None,
                    artifact_digest: None,
                },
            ],
        })?;
        let second = generation_number(&upgraded)?;
        assert_ne!(second, first);
        node.handle_unit_request(UnitRequest::ActivateEnvironment {
            generation: second,
            expected_revision: 1,
        })?;
        let facts = node.tool_unit_facts()?;
        assert_eq!(resolve_tool(&facts, "dep")?.0.version, "2.0.0");

        // Rollback re-selects the earlier generation; nothing is deleted.
        let rolled = node.handle_unit_request(UnitRequest::RollbackEnvironment {
            expected_revision: 2,
        })?;
        assert_eq!(rolled["selector"]["revision"].as_u64(), Some(3));
        assert_eq!(rolled["rolled_back_from"].as_u64(), Some(second));
        let facts = node.tool_unit_facts()?;
        assert_eq!(resolve_tool(&facts, "dep")?.0.version, "1.0.0");
        assert_eq!(
            node.environment_generations(8)?.len(),
            2,
            "a rollback must not delete either generation"
        );
        assert_eq!(node.environment_activations(8)?.len(), 3);
        assert_eq!(
            node.retained_unit_facts("tool.dep")?.len(),
            2,
            "both versions stay retained after upgrading and rolling back"
        );

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

    /// The refusals that make the generation contract real: an unresolved
    /// required dependency, bytes this installation does not hold, a stale
    /// selector revision, and a tool version the active environment does not
    /// select.
    #[test]
    fn environment_refuses_incomplete_closure_bytes_and_stale_selector() -> Result<()> {
        let root = temp_dir("env-refusals");
        let tools_root = root.join(TOOL_DIR_NAME).to_string_lossy().into_owned();
        let mut node = Node::open(&root.join("node"))?;
        node.handle(shenko_node::Request::Initialize {
            node_id: shenko_types::ids::EntityId::new(),
            identity: None,
        })?;
        let orphan = tagged_command("orphan");
        node.handle_unit_request(UnitRequest::Import {
            export: tool_export(
                "orphan",
                "1.0.0",
                &orphan,
                vec![exact_dependency("needs-absent", "tool.absent", "=1.0.0")],
            ),
            package_root: tools_root.clone(),
            enable: Some(true),
        })?;
        let refused = node
            .handle_unit_request(UnitRequest::PrepareEnvironment {
                roots: vec![shenko_node::EnvironmentRoot {
                    unit_id: "tool.orphan".to_string(),
                    revision: None,
                    artifact_digest: None,
                }],
            })
            .expect_err("an unresolved required dependency must refuse the closure");
        assert!(
            format!("{refused:#}").contains("TOOL_DEPENDENCY_MISSING"),
            "{refused:#}"
        );
        assert!(
            node.environment_generations(8)?.is_empty(),
            "a refused preparation records no generation"
        );

        // Bytes the installation does not hold can be registered out of line,
        // but a generation that pins them is never published.
        let external = Digest::blake3(b"bytes this installation does not hold");
        node.handle_unit_request(UnitRequest::ImportExternal {
            unit: UnitDescriptor {
                unit_id: "code.external".to_string(),
                kind: UnitKind::Code,
                revision: "1.0.0".to_string(),
                artifact_digest: external.clone(),
                provides: vec![],
                dependencies: vec![],
                config: serde_json::json!({}),
                applicability: None,
            },
            artifact: shenko_node::ExternalArtifact {
                digest: external,
                bytes: 32,
                store: "cas".to_string(),
                locator: "objects/absent".to_string(),
            },
            package_root: tools_root.clone(),
            enable: Some(true),
        })?;
        let prepared = node.handle_unit_request(UnitRequest::PrepareEnvironment {
            roots: vec![shenko_node::EnvironmentRoot {
                unit_id: "code.external".to_string(),
                revision: None,
                artifact_digest: None,
            }],
        })?;
        let external_generation = generation_number(&prepared)?;
        let unpublished = node
            .handle_unit_request(UnitRequest::ActivateEnvironment {
                generation: external_generation,
                expected_revision: 0,
            })
            .expect_err("a generation over absent bytes must not be published");
        assert!(
            format!("{unpublished:#}").contains("TOOL_ARTIFACT_UNAVAILABLE"),
            "{unpublished:#}"
        );
        assert!(
            node.environment_selector()?.is_none(),
            "a refused activation publishes no selector"
        );

        // A complete closure publishes; a stale expected revision does not.
        let dep = tagged_command("dep");
        node.handle_unit_request(UnitRequest::Import {
            export: tool_export("dep", "1.0.0", &dep, vec![]),
            package_root: tools_root.clone(),
            enable: Some(true),
        })?;
        let prepared = node.handle_unit_request(UnitRequest::PrepareEnvironment {
            roots: vec![shenko_node::EnvironmentRoot {
                unit_id: "tool.dep".to_string(),
                revision: None,
                artifact_digest: None,
            }],
        })?;
        let generation = generation_number(&prepared)?;
        node.handle_unit_request(UnitRequest::ActivateEnvironment {
            generation,
            expected_revision: 0,
        })?;
        let stale = node
            .handle_unit_request(UnitRequest::ActivateEnvironment {
                generation: external_generation,
                expected_revision: 7,
            })
            .expect_err("a stale selector revision must be refused");
        assert!(
            format!("{stale:#}").contains("UNIT_ENVIRONMENT_CONFLICT"),
            "{stale:#}"
        );

        // A tool imported after the generation exists is not silently usable
        // by name: the environment selects, the inventory only stores.
        let late = tagged_command("late");
        node.handle_unit_request(UnitRequest::Import {
            export: tool_export("late", "1.0.0", &late, vec![]),
            package_root: tools_root.clone(),
            enable: Some(true),
        })?;
        let facts = node.tool_unit_facts()?;
        let not_selected = resolve_tool(&facts, "late")
            .expect_err("a version outside the active generation must not resolve by name");
        assert!(
            format!("{not_selected:#}").contains("TOOL_NOT_SELECTED"),
            "{not_selected:#}"
        );
        assert_eq!(
            resolve_tool(&facts, "late@1.0.0")?.0.environment,
            None,
            "an explicit version stays reachable and says it is outside the generation"
        );

        std::fs::remove_dir_all(&root).ok();
        Ok(())
    }

}
