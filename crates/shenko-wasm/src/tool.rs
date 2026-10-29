//! Named WASM tool installation and execution — SPEC §8.2/§8.3/§18.8/R008/R101.
//!
//! A tool is a real upstream artifact (WASI command module bytes) with a
//! declared name, version, ABI, source and content digest. Installed records
//! live in a content-addressed registry under the seed data directory — NOT
//! inside the guest-writable `/home` mount, so a guest cannot rewrite the
//! executable it will be run as. Every run re-verifies the artifact digest;
//! a record without a verified identical artifact never executes.
//!
//! Named product targets (toybox, gcc-wasm, git, gh, tea, ssh) are declared in
//! [`NAMED_TARGETS`] with their actual acquisition state. A target that has no
//! verified WASI artifact stays `Unresolved` with the honest reason; resolving
//! it fails with `TOOL_UNSUPPORTED`, and it is never advertised as installed.
//! Host-CLI relays, imitation WATs and manifest-only entries are not named
//! targets by construction: only real artifact bytes admitted below produce a
//! runnable record.
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use wasmtime::{Engine, Module};

/// Registry filename inside the tool directory.
pub const TOOL_REGISTRY_FILE: &str = "tools.json";
/// Subdirectory of the seed data dir holding the tool closure.
pub const TOOL_DIR_NAME: &str = "tools";
/// Current registry schema. Schema changes need an explicit migration.
pub const TOOL_REGISTRY_SCHEMA: u32 = 1;
/// Upper bound on a single artifact the host will admit. Larger tools need a
/// deliberate profile decision and a raised bound, never a silent truncation.
pub const TOOL_ARTIFACT_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// The ABI an artifact was built for. Only a WASI preview1 *command* module
/// (imports restricted to `wasi_snapshot_preview1`, exporting `_start` and
/// `memory`) can execute through [`crate::WasmRuntime::run_wasi_module`];
/// anything else is recorded honestly and rejected.
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
}

/// Where the artifact's upstream content came from. `uri` is informational
/// provenance; the digest is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSource {
    pub uri: String,
    /// Upstream version/release tag as published (e.g. "v76.0.0").
    pub release: String,
    /// How the artifact was obtained: "upstream_release" for a downloaded
    /// published binary, "source_build:<toolchain>" for a local port build.
    pub provenance: String,
}

/// Caller-supplied install request. `bytes` are the actual artifact.
#[derive(Debug, Clone)]
pub struct ToolInstallSpec {
    pub name: String,
    pub version: String,
    pub abi: ToolAbi,
    /// Guest path used as argv[0] convention / entrypoint label, e.g.
    /// "/tools/toybox". Free-form, informational.
    pub entrypoint: String,
    pub source: ToolSource,
    /// Expected blake3 digest ("blake3:<hex>"). Required for named targets;
    /// optional for generic tools — either way the actual digest is verified
    /// and recorded.
    pub expected_digest: Option<Digest>,
    pub artifact: Vec<u8>,
}

/// One installed tool record. `artifact_file` names `artifacts/<digest>.wasm`
/// inside the tool dir; the digest alone is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub name: String,
    pub version: String,
    pub abi: ToolAbi,
    pub entrypoint: String,
    pub source: ToolSource,
    pub digest: Digest,
    pub bytes: u64,
    pub artifact_file: String,
    /// Import module names observed at admission (e.g.
    /// ["wasi_snapshot_preview1"]). Recorded evidence, not a promise.
    pub imports: Vec<String>,
    pub installed_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolRegistryFile {
    pub schema_version: u32,
    /// Set when a managed installation binds the registry; blocks adopting a
    /// foreign installation's tool store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<EntityId>,
    #[serde(default)]
    pub tools: Vec<ToolRecord>,
}

/// A declared named product target and its real state. This is the honest
/// §8.2 list in code: status reflects whether a verified runnable artifact is
/// installed *right now*, not whether the name is "supported" on paper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedToolTarget {
    pub name: &'static str,
    /// None while no verified upstream artifact exists for our runtime.
    pub unresolved_reason: Option<&'static str>,
}

/// Declared named targets. Order is the SPEC §8.2 order.
pub const NAMED_TARGETS: &[NamedToolTarget] = &[
    NamedToolTarget {
        name: "toybox",
        unresolved_reason: Some(
            "no published WASI artifact found for landley/toybox 0.8.x; a \
             wasm32-wasip1 port must be built from pinned source with \
             wasi-sdk (fork/exec-dependent applets do not apply to preview1)",
        ),
    },
    NamedToolTarget {
        name: "gcc-wasm",
        unresolved_reason: Some(
            "no verified WASM compiler artifact; hosted-vs-target meaning is \
             unresolved per SPEC §8.2",
        ),
    },
    NamedToolTarget {
        name: "git",
        unresolved_reason: Some(
            "no verified wasm32-wasip1 Git client artifact; WASIX ports need a \
             WASIX runtime",
        ),
    },
    NamedToolTarget {
        name: "gh",
        unresolved_reason: Some(
            "wasi-gh exists but requires dedicated host HTTP/exec imports this \
             runtime does not provide (SPEC §8.2 W2)",
        ),
    },
    NamedToolTarget {
        name: "tea",
        unresolved_reason: Some(
            "no verified Gitea CLI WASM artifact found",
        ),
    },
    NamedToolTarget {
        name: "ssh",
        unresolved_reason: Some(
            "OpenSSH WASIX ports need a WASIX runtime and socket imports; \
             wasmtime-45 preview1 provides neither; the host ssh client is not \
             a WASM SSH (SPEC §8.2/R019)",
        ),
    },
];

fn validate_tool_name(name: &str) -> Result<()> {
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

/// Persisted tool store rooted at `<data>/tools`. Artifacts are addressed by
/// digest; the registry file is rewritten atomically (tmp+fsync+rename+dir
/// fsync), so a torn write can never publish a truncated registry.
pub struct ToolStore {
    dir: PathBuf,
    registry: ToolRegistryFile,
}

impl ToolStore {
    /// Open (and for an empty directory, initialize) the tool store.
    ///
    /// - empty dir → fresh empty registry, published durably.
    /// - dir with `tools.json` → load and re-verify every record's artifact
    ///   bytes against its recorded digest (stable reopen; wrong bytes are
    ///   reported, never silently replaced).
    /// - dir with other content but no registry → `DATA_PATH_CONFLICT`;
    ///   unknown files are never adopted into a new registry.
    pub fn open(dir: &Path) -> Result<Self> {
        if dir.exists() && !dir.is_dir() {
            bail!("DATA_PATH_CONFLICT: {} is not a directory", dir.display());
        }
        fs::create_dir_all(dir)
            .with_context(|| format!("create tool directory {}", dir.display()))?;
        let dir = dir.canonicalize()?;
        let registry_path = dir.join(TOOL_REGISTRY_FILE);
        let registry = if registry_path.exists() {
            let bytes = fs::read(&registry_path).map_err(|error| {
                anyhow!(
                    "TOOL_REGISTRY_CORRUPT: read {}: {error}",
                    registry_path.display()
                )
            })?;
            let stored: ToolRegistryFile = serde_json::from_slice(&bytes).map_err(|error| {
                anyhow!(
                    "TOOL_REGISTRY_CORRUPT: decode {}: {error}",
                    registry_path.display()
                )
            })?;
            ensure!(
                stored.schema_version == TOOL_REGISTRY_SCHEMA,
                "TOOL_REGISTRY_CORRUPT: unsupported schema {}; migrate explicitly",
                stored.schema_version
            );
            stored
        } else {
            // Foreign content without a registry is not adoptable.
            for entry in fs::read_dir(&dir)? {
                let entry = entry?;
                bail!(
                    "DATA_PATH_CONFLICT: unrecognized data {} in tool store {}; import explicitly",
                    entry.file_name().to_string_lossy(),
                    dir.display()
                );
            }
            let created = ToolRegistryFile {
                schema_version: TOOL_REGISTRY_SCHEMA,
                installation_id: None,
                tools: Vec::new(),
            };
            persist_registry(&registry_path, &created)?;
            created
        };
        let store = Self { dir, registry };
        // Reopen verification: every recorded artifact must still be the exact
        // recorded bytes. Unrelated extra files are reported only when they
        // collide with a recorded path; other files are simply not tools.
        store.verify_all()?;
        Ok(store)
    }

    /// Bind this registry to a managed installation identity (the ledger id).
    /// Once bound, a different installation refuses to adopt the store.
    pub fn bind_installation(&mut self, installation_id: EntityId) -> Result<()> {
        match self.registry.installation_id {
            Some(bound) if bound != installation_id => bail!(
                "DATA_PATH_CONFLICT: tool store belongs to installation {bound}; \
                 data preserved"
            ),
            Some(_) => Ok(()),
            None => {
                self.registry.installation_id = Some(installation_id);
                persist_registry(&self.registry_path(), &self.registry)
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn registry_path(&self) -> PathBuf {
        self.dir.join(TOOL_REGISTRY_FILE)
    }

    pub fn records(&self) -> &[ToolRecord] {
        &self.registry.tools
    }

    /// Host-side absolute path of a record's artifact (for callers that hand
    /// bytes to the runtime themselves).
    pub fn artifact_path(&self, record: &ToolRecord) -> PathBuf {
        self.dir.join(&record.artifact_file)
    }

    /// Real availability of every declared named target right now.
    pub fn named_target_report(&self) -> BTreeMap<String, String> {
        let mut report = BTreeMap::new();
        for target in NAMED_TARGETS {
            let installed: Vec<String> = self
                .registry
                .tools
                .iter()
                .filter(|record| record.name == target.name)
                .map(|record| format!("{}@{}", record.version, record.digest))
                .collect();
            let status = if installed.is_empty() {
                format!("unresolved: {}", target.unresolved_reason.unwrap_or(""))
            } else {
                format!("installed: {}", installed.join(", "))
            };
            report.insert(target.name.to_string(), status);
        }
        report
    }

    /// Resolve a tool name (optionally `name@version`) to its verified
    /// artifact bytes. Errors are explicit and stable:
    ///
    /// - `TOOL_UNSUPPORTED:<name>`  — a declared named target with no
    ///   installed artifact; detail carries the honest reason.
    /// - `TOOL_UNAVAILABLE:<name>`  — unknown or uninstalled name.
    /// - `TOOL_AMBIGUOUS:<name>`    — several versions installed; the caller
    ///   must pass an explicit version.
    /// - `TOOL_ARTIFACT_MISMATCH:<name>` — recorded artifact bytes changed.
    pub fn resolve(&self, spec: &str) -> Result<(ToolRecord, Vec<u8>)> {
        let (name, version) = match spec.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (spec, None),
        };
        validate_tool_name(name)?;
        let candidates: Vec<&ToolRecord> = self
            .registry
            .tools
            .iter()
            .filter(|record| {
                record.name == name && version.is_none_or(|v| record.version == v)
            })
            .collect();
        let record = match candidates.len() {
            0 => {
                if version.is_none() {
                    if let Some(target) = named_target(name) {
                        bail!(
                            "TOOL_UNSUPPORTED:{name}: declared target has no installed \
                             artifact — {}",
                            target.unresolved_reason.unwrap_or("unresolved")
                        );
                    }
                }
                bail!("TOOL_UNAVAILABLE:{spec}: no installed WASI tool by that name")
            }
            1 => candidates[0],
            _ => bail!(
                "TOOL_AMBIGUOUS:{name}: {} versions installed; pass name@version",
                candidates.len()
            ),
        };
        // Installed under a foreign ABI is catalogued, never executed: only a
        // preview1 command module can actually run in this runtime.
        ensure!(
            record.abi == ToolAbi::WasiPreview1,
            "TOOL_ABI_UNSUPPORTED:{}: installed ABI {} cannot execute under \
             wasmtime preview1 ({} imports {:?})",
            record.name,
            record.abi.name(),
            record.name,
            record.imports
        );
        let bytes = self.read_verified_artifact(record)?;
        Ok((record.clone(), bytes))
    }

    /// Read a record's artifact and prove it is exactly the recorded bytes.
    fn read_verified_artifact(&self, record: &ToolRecord) -> Result<Vec<u8>> {
        let path = self.artifact_path(record);
        let bytes = fs::read(&path).map_err(|error| {
            anyhow!(
                "TOOL_ARTIFACT_MISSING:{}: {}: {error}",
                record.name,
                path.display()
            )
        })?;
        let actual = Digest::blake3(&bytes);
        ensure!(
            actual == record.digest && bytes.len() as u64 == record.bytes,
            "TOOL_ARTIFACT_MISMATCH:{}: recorded {} ({} bytes), found {} ({} bytes); \
             refusing to execute",
            record.name,
            record.digest,
            record.bytes,
            actual,
            bytes.len()
        );
        Ok(bytes)
    }

    /// Re-verify every recorded artifact. Called on open; idempotent repair is
    /// deliberately NOT performed — a wrong artifact is reported, not replaced.
    pub fn verify_all(&self) -> Result<()> {
        for record in &self.registry.tools {
            self.read_verified_artifact(record)?;
        }
        Ok(())
    }

    /// Install real artifact bytes under a name. Idempotent for an identical
    /// record; a conflicting same-name@same-version record fails loudly —
    /// upgrades happen as a distinct version (new content gets a new identity;
    /// already-dispatched invocations keep their recorded digest).
    pub fn install(&mut self, spec: ToolInstallSpec) -> Result<ToolRecord> {
        validate_tool_name(&spec.name)?;
        ensure!(
            spec.artifact.len() as u64 <= TOOL_ARTIFACT_MAX_BYTES,
            "TOOL_ARTIFACT_TOO_LARGE:{}: {} bytes exceeds {}",
            spec.name,
            spec.artifact.len(),
            TOOL_ARTIFACT_MAX_BYTES
        );
        let actual = Digest::blake3(&spec.artifact);
        if let Some(expected) = &spec.expected_digest {
            ensure!(
                expected.algorithm() == "blake3",
                "TOOL_DIGEST_UNSUPPORTED:{}: only blake3 digests are admitted",
                expected
            );
            ensure!(
                *expected == actual,
                "TOOL_DIGEST_MISMATCH:{}: expected {expected}, artifact is {actual}",
                spec.name
            );
        }
        // Real ABI from the real bytes, not the caller's claim. Binary modules
        // only: WAT text is not an installable upstream artifact identity.
        ensure!(
            spec.artifact.starts_with(b"\0asm"),
            "TOOL_ABI_UNSUPPORTED:{}: artifact is not a WASM binary module",
            spec.name
        );
        let engine = Engine::default();
        let module = Module::new(&engine, &spec.artifact).map_err(|error| {
            anyhow!("TOOL_ABI_UNSUPPORTED:{}: not a valid core module: {error}", spec.name)
        })?;
        let imports: Vec<String> = module
            .imports()
            .iter()
            .map(|import| import.module().to_string())
            .collect();
        let unique_imports: std::collections::BTreeSet<&String> = imports.iter().collect();
        let preview1_only =
            unique_imports.iter().all(|module| module.as_str() == "wasi_snapshot_preview1");
        let runnable = preview1_only
            && module.exports().iter().any(|export| export.name() == "_start")
            && module.exports().iter().any(|export| export.name() == "memory");
        // The declared ABI must agree with what the bytes actually are:
        // `wasi_preview1` is admitted only when the module is runnable by this
        // runtime; any other declared ABI is admitted only when it is NOT —
        // cataloguing a preview1-runnable module under a foreign ABI is a lie.
        ensure!(
            runnable == (spec.abi == ToolAbi::WasiPreview1),
            "TOOL_ABI_UNSUPPORTED:{}: declared {} but imports are {:?} \
             (preview1-runnable: {})",
            spec.name,
            spec.abi.name(),
            unique_imports,
            runnable
        );

        let artifact_file = format!("artifacts/{}.wasm", actual.hex());
        let record = ToolRecord {
            name: spec.name.clone(),
            version: spec.version.clone(),
            abi: spec.abi,
            entrypoint: spec.entrypoint.clone(),
            source: spec.source.clone(),
            digest: actual,
            bytes: spec.artifact.len() as u64,
            artifact_file,
            imports: unique_imports.into_iter().cloned().collect(),
            installed_at: chrono::Utc::now(),
        };
        if let Some(existing) = self
            .registry
            .tools
            .iter()
            .find(|existing| existing.name == record.name && existing.version == record.version)
        {
            ensure!(
                *existing == record || (existing.digest == record.digest
                    && existing.abi == record.abi
                    && existing.entrypoint == record.entrypoint
                    && existing.source == record.source),
                "TOOL_INSTALL_CONFLICT:{}@{}: a different artifact/record is \
                 already installed under this name@version",
                record.name,
                record.version
            );
            // Same digest + same identity fields: idempotent re-install; the
            // stored timestamp is authoritative for first install.
            return Ok(existing.clone());
        }

        // Content-addressed artifact write: identical content is never
        // rewritten; a file under the same name with different bytes is
        // corruption we refuse to paper over.
        let artifact_path = self.dir.join(&record.artifact_file);
        fs::create_dir_all(artifact_path.parent().expect("artifacts/ has a parent"))?;
        if artifact_path.exists() {
            let existing = fs::read(&artifact_path)?;
            ensure!(
                Digest::blake3(&existing) == record.digest,
                "TOOL_ARTIFACT_MISMATCH: {} exists with different bytes; investigate before \
                 overwriting",
                artifact_path.display()
            );
        } else {
            let temporary = artifact_path.with_extension("wasm.tmp");
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .with_context(|| format!("create {}", temporary.display()))?;
            file.write_all(&spec.artifact)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &artifact_path).with_context(|| {
                format!(
                    "publish artifact {} -> {}",
                    temporary.display(),
                    artifact_path.display()
                )
            })?;
            fs::File::open(artifact_path.parent().expect("artifacts/ has a parent"))?
                .sync_all()?;
        }

        self.registry.tools.push(record.clone());
        self.registry
            .tools
            .sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
        if let Err(error) = persist_registry(&self.registry_path(), &self.registry) {
            // Roll the in-memory record back so this ToolStore stays consistent
            // with the durable registry.
            self.registry
                .tools
                .retain(|existing| !(existing.name == record.name && existing.version == record.version));
            return Err(error);
        }
        Ok(record)
    }
}

fn persist_registry(path: &Path, registry: &ToolRegistryFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("tool registry has no parent"))?;
    let temporary = parent.join(format!(".tools.{}.tmp", EntityId::new()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, registry)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path).with_context(|| {
        format!(
            "publish tool registry {} -> {}",
            temporary.display(),
            path.display()
        )
    })?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MountMode, MountSpec, WasiCommand, WasmRuntime, HOME_GUEST_PATH};
    use wasmtime_wasi::p2::pipe::MemoryOutputPipe;

    fn temp_tools(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shenko-tools-{tag}-{}", uuid::Uuid::new_v4()));
        dir
    }

    fn spec(name: &str, version: &str, abi: ToolAbi, artifact: Vec<u8>) -> ToolInstallSpec {
        ToolInstallSpec {
            name: name.into(),
            version: version.into(),
            abi,
            entrypoint: format!("/tools/{name}"),
            source: ToolSource {
                uri: format!("builtin:test/{name}"),
                release: version.into(),
                provenance: "test_fixture".into(),
            },
            expected_digest: None,
            artifact,
        }
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
                (data (i32.const 200) "x")
                (i32.store (i32.const 0) (i32.const 200))
                (i32.store (i32.const 4) (i32.const 1))
                (if (call $fd_write (i32.load (i32.const 20)) (i32.const 0) (i32.const 1) (i32.const 8))
                  (then (call $exit (i32.const 93))))
              )
            )
        "#;
        wat::parse_str(wat).expect("fixture module must assemble")
    }

    /// Fresh dir → empty registry; named targets resolve with explicit
    /// TOOL_UNSUPPORTED (not a fake available), unknown names TOOL_UNAVAILABLE.
    #[test]
    fn named_targets_and_unknown_names_fail_explicitly() -> Result<()> {
        let dir = temp_tools("explicit");
        let store = ToolStore::open(&dir)?;
        assert!(store.records().is_empty());
        let err = store.resolve("toybox").unwrap_err().to_string();
        assert!(err.contains("TOOL_UNSUPPORTED:toybox"), "{err}");
        let err = store.resolve("ssh").unwrap_err().to_string();
        assert!(err.contains("TOOL_UNSUPPORTED:ssh"), "{err}");
        let err = store.resolve("nonesuch").unwrap_err().to_string();
        assert!(err.contains("TOOL_UNAVAILABLE:nonesuch"), "{err}");
        let report = store.named_target_report();
        assert!(report["toybox"].starts_with("unresolved:"));
        assert_eq!(report.len(), NAMED_TARGETS.len());
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// Install a real module, run it through the real runtime with argv and a
    /// stdout stream, observe its guest-visible file write, then reopen the
    /// store: the record persists, resolves, and unrelated data stays intact.
    #[test]
    fn installed_tool_executes_and_survives_reopen() -> Result<()> {
        let dir = temp_tools("roundtrip");
        let home = temp_tools("roundtrip-home");
        fs::create_dir_all(&home)?;
        fs::write(home.join("keep.txt"), b"unrelated")?;
        let wasm = echo_writer_wasm();
        let expected = Digest::blake3(&wasm);

        let mut store = ToolStore::open(&dir)?;
        let record = store.install(spec("fixture.echo", "1.0.0", ToolAbi::WasiPreview1, wasm))?;
        assert_eq!(record.digest, expected);
        assert_eq!(record.imports, vec!["wasi_snapshot_preview1".to_string()]);

        let mut rt = WasmRuntime::with_mounts(vec![MountSpec::new(
            home.clone(),
            HOME_GUEST_PATH,
            MountMode::ReadWrite,
            true,
            false,
        )])?;
        let (resolved, bytes) = store.resolve("fixture.echo")?;
        let stdout = MemoryOutputPipe::new(4096);
        let mut command = WasiCommand::new(std::io::empty(), stdout.clone(), std::io::empty());
        command.arguments = vec!["fixture.echo".into()];
        let code = rt.run_wasi_module(&bytes, command)?;
        assert_eq!(code, 0);
        assert_eq!(resolved.name, "fixture.echo");
        assert_eq!(stdout.contents().as_ref(), b"tool-out\n");
        assert!(home.join("wrote-by-tool").exists(), "guest write must persist");

        // Stable reopen: same records, resolution still works, unrelated files
        // and unrelated installations' records are untouched.
        drop(store);
        let reopened = ToolStore::open(&dir)?;
        assert_eq!(reopened.records(), &[record]);
        assert_eq!(fs::read(home.join("keep.txt"))?, b"unrelated");
        let (_, bytes2) = reopened.resolve("fixture.echo@1.0.0")?;
        assert_eq!(Digest::blake3(&bytes2), expected);
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&home).ok();
        Ok(())
    }

    /// A recorded artifact whose bytes changed on disk is reported
    /// TOOL_ARTIFACT_MISMATCH and never executes; the corrupt bytes are not
    /// repaired or overwritten behind the record's back.
    #[test]
    fn tampered_artifact_is_detected_and_refused() -> Result<()> {
        let dir = temp_tools("tampered");
        let mut store = ToolStore::open(&dir)?;
        let wasm = echo_writer_wasm();
        let record = store.install(spec("fixture.echo", "1.0.0", ToolAbi::WasiPreview1, wasm))?;
        fs::write(store.artifact_path(&record), b"tampered")?;

        let err = store.resolve("fixture.echo").unwrap_err().to_string();
        assert!(err.contains("TOOL_ARTIFACT_MISMATCH"), "{err}");
        let err = ToolStore::open(&dir).unwrap_err().to_string();
        assert!(err.contains("TOOL_ARTIFACT_MISMATCH"), "{err}");
        // Corrupt bytes remain for diagnosis; the store does not self-heal
        // silently.
        assert_eq!(fs::read(store.artifact_path(&record))?, b"tampered");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// Declared-vs-observed ABI and identity mismatches fail admission: a
    /// preview2/foreign-import module is not "wasi_preview1", a wrong
    /// expected digest is rejected, and WAT text is not an install artifact.
    #[test]
    fn admission_rejects_wrong_abi_digest_and_nonbinary() -> Result<()> {
        let dir = temp_tools("admission");
        let mut store = ToolStore::open(&dir)?;
        // Real binary module importing a non-preview1 host module.
        let foreign = wat::parse_str(
            r#"(module
                (import "env" "custom" (func $custom))
                (memory (export "memory") 1)
                (func (export "_start") (call $custom)))"#,
        )?;
        let err = store
            .install(spec("fixture.foreign", "1", ToolAbi::WasiPreview1, foreign.clone()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("TOOL_ABI_UNSUPPORTED"), "{err}");
        // Same module honestly declared as host-imports is admitted and
        // recorded as unrunnable-by-this-runtime metadata.
        let record = store.install(spec("fixture.foreign", "1", ToolAbi::HostImports, foreign))?;
        assert_eq!(record.imports, vec!["env".to_string()]);
        // Catalogued but not executable in this runtime: explicit ABI refusal.
        let err = store.resolve("fixture.foreign").unwrap_err().to_string();
        assert!(err.contains("TOOL_ABI_UNSUPPORTED"), "{err}");

        let wasm = echo_writer_wasm();
        let mut wrong = spec("fixture.echo", "1.0.0", ToolAbi::WasiPreview1, wasm.clone());
        wrong.expected_digest = Some(Digest::blake3(b"not the artifact"));
        let err = store.install(wrong).unwrap_err().to_string();
        assert!(err.contains("TOOL_DIGEST_MISMATCH"), "{err}");

        let err = store
            .install(spec(
                "fixture.wat",
                "1",
                ToolAbi::WasiPreview1,
                b"(module)".to_vec(),
            ))
            .unwrap_err()
            .to_string();
        assert!(err.contains("TOOL_ABI_UNSUPPORTED"), "{err}");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// name@version disambiguates; a same-name@same-version record with a
    /// different artifact is a loud conflict, never an in-place overwrite.
    #[test]
    fn versions_disambiguate_and_conflicts_fail_loudly() -> Result<()> {
        let dir = temp_tools("versions");
        let mut store = ToolStore::open(&dir)?;
        let v1 = echo_writer_wasm();
        // A second, genuinely different real module (exits 7).
        let v2 = wat::parse_str(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
                (memory (export "memory") 1)
                (func (export "_start") (call $exit (i32.const 7))))"#,
        )?;
        store.install(spec("fixture.echo", "1.0.0", ToolAbi::WasiPreview1, v1.clone()))?;
        store.install(spec("fixture.echo", "2.0.0", ToolAbi::WasiPreview1, v2))?;
        let err = store.resolve("fixture.echo").unwrap_err().to_string();
        assert!(err.contains("TOOL_AMBIGUOUS"), "{err}");
        let (r1, b1) = store.resolve("fixture.echo@1.0.0")?;
        assert_eq!(r1.version, "1.0.0");
        assert_eq!(Digest::blake3(&b1), Digest::blake3(&v1));

        // Same name@version, different artifact → conflict, original kept.
        let mut clash = spec("fixture.echo", "1.0.0", ToolAbi::WasiPreview1, v1.clone());
        clash.entrypoint = "/tools/other".into();
        let err = store.install(clash).unwrap_err().to_string();
        assert!(err.contains("TOOL_INSTALL_CONFLICT"), "{err}");
        let (kept, _) = store.resolve("fixture.echo@1.0.0")?;
        assert_eq!(kept.entrypoint, "/tools/fixture.echo");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }

    /// A foreign directory (content without a registry) is never adopted; a
    /// registry bound to another installation id refuses to open for us.
    #[test]
    fn foreign_tool_data_is_not_adopted() -> Result<()> {
        let dir = temp_tools("foreign");
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("stray.bin"), b"someone else's data")?;
        let err = ToolStore::open(&dir).unwrap_err().to_string();
        assert!(err.contains("DATA_PATH_CONFLICT"), "{err}");
        assert_eq!(fs::read(dir.join("stray.bin"))?, b"someone else's data");
        fs::remove_dir_all(&dir).ok();

        let dir = temp_tools("bound");
        let mut store = ToolStore::open(&dir)?;
        let id_a = EntityId::new();
        store.bind_installation(id_a)?;
        drop(store);
        // Reopening with the same installation is fine; rebinding to another
        // installation id is refused.
        let mut reopened = ToolStore::open(&dir)?;
        let err = reopened.bind_installation(EntityId::new()).unwrap_err().to_string();
        assert!(err.contains("DATA_PATH_CONFLICT"), "{err}");
        fs::remove_dir_all(&dir).ok();
        Ok(())
    }
}
