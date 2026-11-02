//! `shenko tool …` — install, inspect and describe real WASM artifacts.
//!
//! This module owns the install path of the named WASM tool surface:
//!
//! * `tool inspect` reports what an artifact *really* is (its actual ABI and
//!   its actual imports) without installing anything;
//! * `tool manifest` turns a port build receipt into the durable tool-set
//!   manifest the product consumes;
//! * `tool install` / `tool install-set` register real artifacts in the
//!   installation's unit inventory, verifying the upstream receipt digest
//!   (sha256, provenance) and binding the artifact's blake3 identity (the
//!   unit identity) to the installed bytes.
//!
//! Artifacts that fit the node's inline unit store are carried inline, so
//! node replication keeps working. Larger artifacts are staged as chunk
//! objects in the installation's content store and their chunk manifest is
//! bound into the ledger's recovery closure, so the bytes travel with the
//! installation's cognition archive instead of being base64'd into a JSON
//! request.
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64_STD};
use serde::Serialize;
use shenko_node::{
    Node, UnitDescriptor, UnitExport, UnitKind, UnitPackage, UnitRequest, UNIT_SCHEMA,
};
use shenko_store::store::Store;
use shenko_types::{
    commit::CommitRef,
    ids::{EntityId, LedgerSeq, RecordRevision, TaskRevision},
    object::{Digest, ObjectRef},
    operation::{
        AuthorityRef, EffectStatus, OperationContractRef, OperationOutcome, OperationRecord,
        OperationState,
    },
    store::WriteBatch,
    task::{ImplementationRef, TaskOwner, TaskRecord, TaskState},
};
use shenko_wasm::{
    BuildReceipt, CasToolContentStore, ToolAbi, ToolArtifactRef, ToolDescriptor, ToolSetManifest,
    ToolSetMeta, ToolSource, inspect_module, stage_artifact_file,
};
use std::path::{Path, PathBuf};

/// The plugin identity of the install operation. Fixed, like every other
/// built-in native plugin: the operation contract is a product fact, not a
/// per-installation identity.
fn install_plugin_id() -> EntityId {
    "6d0a3f21-1c74-4f2b-9d55-6f0f4f0a7c31"
        .parse()
        .expect("fixed built-in plugin UUID")
}

/// The contract the install operation is bound to.
const INSTALL_CONTRACT: &[u8] = b"shenko.wasi.tool.install/v1: register a digest-verified WASI \
     artifact in this installation's unit inventory, keep the upstream receipt digest as provenance \
     only, and bind an out-of-line artifact into the recovery closure without replaying effects";

/// One artifact to install.
#[derive(Debug, Clone)]
pub struct InstallRequest {
    pub name: String,
    pub version: String,
    pub abi: ToolAbi,
    pub entrypoint: String,
    pub source: ToolSource,
    /// SPDX license identifier of the upstream project.
    pub license: Option<String>,
    /// Upstream receipt digest (sha256). Provenance, never identity.
    pub sha256: Option<String>,
    /// Host path of the artifact.
    pub path: PathBuf,
}

/// What an install actually did.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledTool {
    pub name: String,
    pub version: String,
    pub unit_id: String,
    /// blake3 identity of the artifact bytes.
    pub digest: String,
    pub bytes: u64,
    pub abi: String,
    /// The module's real imports, as observed from its bytes.
    pub imports: Vec<String>,
    /// `node_inline` or `content_store`.
    pub storage: String,
    /// The upstream receipt digest, when the manifest recorded one.
    pub sha256: Option<String>,
    /// The SPDX license, when the manifest recorded one.
    pub license: Option<String>,
    /// Whether this run bound the artifact's chunk manifest into the
    /// installation's recovery closure.
    pub recovery_bound: bool,
    /// True when the artifact was already installed with exactly these bytes
    /// and nothing was written. Installing the same bytes again is
    /// idempotent, not an error.
    pub already_present: bool,
}

/// Inspect a real artifact without installing it.
pub fn inspect_artifact(path: &Path) -> Result<serde_json::Value> {
    let bytes = std::fs::read(path).with_context(|| format!("read artifact {}", path.display()))?;
    let inspection = inspect_module(&bytes)
        .with_context(|| format!("inspect artifact {}", path.display()))?;
    Ok(serde_json::json!({
        "file": path.display().to_string(),
        "bytes": bytes.len(),
        "digest": Digest::blake3(&bytes).to_string(),
        "abi": inspection.abi.name(),
        "runnable": inspection.runnable,
        "imports": inspection.imports,
        "detail": inspection.detail,
    }))
}

/// Build a tool-set manifest from a port build receipt.
pub fn build_manifest(
    receipt_path: &Path,
    artifacts: &Path,
    meta: &ToolSetMeta,
) -> Result<ToolSetManifest> {
    let text = std::fs::read_to_string(receipt_path)
        .with_context(|| format!("read build receipt {}", receipt_path.display()))?;
    let receipt = BuildReceipt::parse(&text)
        .with_context(|| format!("parse build receipt {}", receipt_path.display()))?;
    ToolSetManifest::from_receipt(&receipt, meta, artifacts)
}

/// Install one artifact into the installation's unit inventory.
pub fn install_one(
    node: &mut Node,
    store: &Store,
    tools_root: &Path,
    request: &InstallRequest,
) -> Result<InstalledTool> {
    shenko_wasm::validate_tool_name(&request.name)?;
    shenko_wasm::artifact::validate_provenance(request.sha256.as_deref())?;
    // Identity and provenance come from one bounded streaming pass over the
    // file: the blake3 of the bytes is the artifact's identity, the sha256 is
    // the upstream receipt digest. They are never compared to each other.
    let (identity, sha256, length) = {
        let mut file = std::fs::File::open(&request.path)
            .with_context(|| format!("open artifact {}", request.path.display()))?;
        let mut reader = std::io::BufReader::with_capacity(
            shenko_wasm::artifact::ARTIFACT_STREAM_BUFFER,
            &mut file,
        );
        shenko_wasm::artifact::stream_identity(&mut reader)?
    };
    if let Some(expected) = request.sha256.as_deref() {
        ensure!(
            expected.eq_ignore_ascii_case(&sha256),
            "TOOL_PROVENANCE_MISMATCH:{}: the artifact hashes to {sha256}, the receipt records \
             {expected}",
            request.name
        );
    }
    // The module bytes are resident exactly once: the ABI is decided by the
    // real imports of the real module, and the runtime compiles those same
    // bytes. Everything else on this path streams.
    let bytes = std::fs::read(&request.path)
        .with_context(|| format!("read artifact {}", request.path.display()))?;
    ensure!(
        bytes.len() as u64 == length,
        "TOOL_ARTIFACT_SIZE_MISMATCH:{}: the artifact changed while it was being installed",
        request.name
    );
    ensure!(
        Digest::blake3(&bytes) == identity,
        "TOOL_ARTIFACT_DIGEST_MISMATCH:{}: the artifact changed while it was being installed",
        request.name
    );
    // The real ABI comes from the real bytes; the declared ABI only has to
    // agree with it.
    let inspection = inspect_module(&bytes)
        .with_context(|| format!("inspect artifact {}", request.path.display()))?;
    ensure!(
        inspection.runnable == (request.abi == ToolAbi::WasiPreview1),
        "TOOL_ABI_UNSUPPORTED:{}: declared {} but {:?}",
        request.name,
        request.abi.name(),
        inspection.detail
    );
    let content = CasToolContentStore::new(&store.cas);

    // Storage class: an artifact the node's inline unit store admits is
    // carried inline by its unit, so node replication keeps working exactly
    // as before. A larger artifact is staged as chunk objects and its chunk
    // manifest is bound into the recovery closure.
    let (unit_bytes, artifact, recovery_manifest) = if length <= shenko_node::MAX_ARTIFACT_BYTES as u64
    {
        (
            bytes,
            ToolArtifactRef::node_inline(identity.clone(), length, Some(sha256.clone())),
            None,
        )
    } else {
        let staged = stage_artifact_file(&content, &request.path, Some(sha256.clone()))?;
        ensure!(
            staged.digest == identity && staged.bytes == length,
            "TOOL_ARTIFACT_DIGEST_MISMATCH: staged artifact does not match the inspected bytes"
        );
        let artifact = ToolArtifactRef::content_store(
            identity.clone(),
            staged.bytes,
            Some(sha256.clone()),
            staged.manifest.clone(),
        );
        // The unit's artifact is the pointer document: a small, digest
        // verified record of where the module's bytes live. The module
        // itself never enters a JSON request.
        let pointer = shenko_wasm::artifact::ArtifactPointer::new(artifact.clone());
        (
            pointer.to_bytes()?,
            artifact,
            Some(staged.manifest),
        )
    };

    let descriptor = ToolDescriptor {
        name: request.name.clone(),
        version: request.version.clone(),
        abi: request.abi,
        entrypoint: request.entrypoint.clone(),
        source: request.source.clone(),
        imports: inspection.imports.clone(),
        license: request.license.clone(),
        artifact: Some(artifact.clone()),
    };
    let unit_id = format!("{}{}", shenko_wasm::TOOL_UNIT_PREFIX, request.name);
    // The unit's artifact digest identifies the unit's own bytes: the module
    // for an inline tool, the pointer document for an out-of-line one.
    let unit_digest = Digest::blake3(&unit_bytes);
    // The same name@version is idempotent only when the artifact is
    // identical; different bytes under an existing version are a loud
    // conflict, never an in-place overwrite.
    for fact in node.tool_unit_facts()? {
        if fact.unit_id == unit_id {
            let recorded = fact
                .tool
                .get("version")
                .and_then(|version| version.as_str())
                .unwrap_or_default();
            if recorded == request.version {
                // Compare the artifact's identity, not the unit's carrier
                // digest: an out-of-line tool's unit carries a pointer.
                let existing = fact
                    .tool
                    .get("artifact")
                    .and_then(|artifact| artifact.get("digest"))
                    .and_then(|digest| digest.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| fact.artifact_digest.to_string());
                ensure!(
                    existing == identity.to_string(),
                    "TOOL_INSTALL_CONFLICT:{}@{}: a different artifact is already installed \
                     under this version",
                    request.name,
                    request.version
                );
                // The same bytes under the same version are already
                // installed: nothing is written, and the install stays
                // idempotent.
                return Ok(InstalledTool {
                    name: request.name.clone(),
                    version: request.version.clone(),
                    unit_id: unit_id.clone(),
                    digest: identity.to_string(),
                    bytes: length,
                    abi: request.abi.name().to_string(),
                    imports: inspection.imports.clone(),
                    storage: artifact.kind.name().to_string(),
                    sha256: Some(sha256.clone()),
                    license: request.license.clone(),
                    recovery_bound: false,
                    already_present: true,
                });
            }
        }
    }
    let export = UnitExport {
        schema: UNIT_SCHEMA,
        root_unit_id: unit_id.clone(),
        root_revision: Some(request.version.clone()),
        root_artifact_digest: Some(unit_digest.clone()),
        origin_node: None,
        enabled: true,
        packages: vec![UnitPackage {
            unit: UnitDescriptor {
                unit_id: unit_id.clone(),
                kind: UnitKind::Code,
                revision: request.version.clone(),
                artifact_digest: unit_digest.clone(),
                provides: vec![format!("tool:{}", request.name)],
                dependencies: vec![],
                config: serde_json::json!({ "tool": descriptor }),
                applicability: None,
            },
            artifact_base64: B64_STD.encode(&unit_bytes),
            evidence: vec![],
            resolved_dependencies: None,
        }],
    };
    node.handle_unit_request(UnitRequest::Import {
        export,
        package_root: tools_root.to_string_lossy().into_owned(),
        enable: Some(true),
    })?;

    // An out-of-line artifact only travels with the installation when the
    // ledger's recovery closure names it. Inline artifacts travel with their
    // unit, so they need no binding.
    let recovery_bound = match (&artifact.chunk_manifest, recovery_manifest) {
        (Some(described), Some(manifest)) => {
            ensure!(
                described == &manifest,
                "TOOL_ARTIFACT_MANIFEST_MISMATCH: the recorded chunk manifest is not the staged one"
            );
            bind_artifact_to_recovery(store, &request.name, &request.version, &artifact, &manifest)?;
            true
        }
        (None, None) => false,
        _ => {
            bail!(
                "TOOL_ARTIFACT_MANIFEST_MISSING: the artifact's chunk manifest is not the staged one"
            )
        }
    };
    Ok(InstalledTool {
        name: request.name.clone(),
        version: request.version.clone(),
        unit_id,
        digest: identity.to_string(),
        bytes: length,
        abi: request.abi.name().to_string(),
        imports: inspection.imports,
        storage: artifact.kind.name().to_string(),
        sha256: Some(sha256.clone()),
        license: request.license.clone(),
        recovery_bound,
        already_present: false,
    })
}

/// Bind an installed tool's artifact into the installation's recovery
/// closure.
///
/// The binding object names the artifact's chunk manifest; the operation row
/// references that object, so the cognition archive's object closure follows
/// the reference to the manifest and, through it, to every chunk of the
/// artifact. Nothing is replayed and no external effect is claimed.
fn bind_artifact_to_recovery(
    store: &Store,
    tool: &str,
    version: &str,
    artifact: &ToolArtifactRef,
    manifest: &ObjectRef,
) -> Result<()> {
    let binding = store.installation_binding()?;
    let Some(subject) = binding.subject else {
        bail!(
            "TOOL_BINDING_UNAVAILABLE: this installation has no subject yet, so the artifact \
             cannot be bound into the recovery closure; the tool stays installed and usable \
             locally"
        );
    };
    let payload = serde_json::json!({
        "kind": "wasm.tool.artifact",
        "tool": tool,
        "version": version,
        "artifact": artifact,
        "chunk_manifest": manifest,
    });
    let sealed = store
        .cas
        .seal_object(
            &serde_json::to_vec(&payload)?,
            "application/json",
            "wasm-tool-binding",
            "wasm-tool-binding",
            1,
        )
        .map_err(|error| anyhow::anyhow!("seal tool binding: {error}"))?;
    let binding_object = sealed.object;
    let contract_digest = Digest::blake3(INSTALL_CONTRACT);
    let zero = CommitRef::new(store.ledger.ledger_id(), LedgerSeq::ZERO);
    let task_id = EntityId::new();
    let operation_id = EntityId::new();
    let acceptance = store
        .cas
        .seal_object(
            b"the artifact is registered in this installation's unit inventory and its bytes \
              match the recorded identity",
            "text/plain",
            "wasm-tool-binding",
            "wasm-tool-binding",
            1,
        )
        .map_err(|error| anyhow::anyhow!("seal acceptance predicate: {error}"))?
        .object;
    let retry = store
        .cas
        .seal_object(
            b"an install without its unit record is unknown; the installer never re-registers \
              different bytes under the same version",
            "text/plain",
            "wasm-tool-binding",
            "wasm-tool-binding",
            1,
        )
        .map_err(|error| anyhow::anyhow!("seal retry contract: {error}"))?
        .object;
    let task = TaskRecord {
        record_schema_version: 1,
        task_id,
        task_revision: TaskRevision(0),
        record_revision: RecordRevision(0),
        agent_id: subject.agent_id,
        conversation_id: subject.agent_id,
        owner: TaskOwner::Conversation {
            id: subject.agent_id,
        },
        background: false,
        kind: "wasi.tool.install".into(),
        definition_version: 1,
        implementation_ref: ImplementationRef {
            plugin_id: install_plugin_id(),
            artifact_digest: binding_object.digest.clone(),
            registry_revision: 1,
        },
        input_ref: binding_object.clone(),
        state: TaskState::Terminal {
            outcome_ref: binding_object.clone(),
        },
        abort_request: None,
        memos: serde_json::Map::new(),
        residual_case_ids: vec![],
        created_by_step_id: None,
        dispatch_eligibility: shenko_types::task::DispatchEligibility::Ready,
        no_progress_crashes: 0,
        first_run_at: None,
        terminal_at: Some(chrono::Utc::now()),
        created_commit: zero,
        updated_commit: zero,
    };
    let operation = OperationRecord {
        record_schema_version: 1,
        operation_id,
        record_revision: RecordRevision(0),
        task_id,
        logical_key: format!("tool-install:{tool}@{version}"),
        operation_name: "wasi.tool.install.v1".into(),
        input_schema_version: 1,
        output_schema_version: 1,
        final_input_ref: binding_object.clone(),
        input_digest: binding_object.digest.clone(),
        contract_ref: OperationContractRef {
            plugin_id: install_plugin_id(),
            artifact_digest: contract_digest.clone(),
            contract_digest: contract_digest.clone(),
            receipt_reader_ref: shenko_types::object::VersionRef {
                artifact: shenko_types::object::ArtifactRef {
                    digest: contract_digest.clone(),
                    locator: "builtin:wasi-tool-install-v1".into(),
                },
                symbol: "wasi.tool.install".into(),
                schema: shenko_types::object::SchemaRef {
                    name: "wasm-tool-binding-v1".into(),
                    major: 1,
                    digest: contract_digest,
                },
            },
        },
        resource_bindings_ref: binding_object.clone(),
        authority_ref: AuthorityRef {
            delegation_id: subject.agent_id,
            delegation_revision: RecordRevision(0),
            decision_ref: subject.branch_id,
        },
        acceptance_predicate_ref: acceptance,
        retry_contract_ref: retry,
        checkpoint_scope_ref: None,
        state: OperationState::Resolved,
        outcome: Some(OperationOutcome::Succeeded),
        outcome_conflict: false,
        // The install writes this installation's own files and records; it
        // performs no effect outside the installation.
        effect_status: EffectStatus::None,
        retry_of: None,
        compensates: None,
        possible_duplicate_of: vec![],
        prepared_commit: zero,
        evidence_through: None,
    };
    store.commit(WriteBatch {
        task_upserts: vec![task],
        operation_upserts: vec![operation],
        ..WriteBatch::new()
    })?;
    Ok(())
}

/// What a tool-set install did.
#[derive(Debug, Clone, Serialize)]
pub struct InstalledSet {
    /// The artifacts this run registered.
    pub installed: Vec<InstalledTool>,
    /// The artifacts that were already present with exactly these bytes.
    /// They are not reinstalled and not reported as failures.
    pub already_present: Vec<String>,
    /// The set the install belongs to.
    pub set: String,
    pub version: String,
    /// The upstream license of the set.
    pub license: String,
}

/// Install every artifact of a tool set.
///
/// The install is idempotent: an artifact that is already present with
/// exactly the same bytes is left alone, and an artifact that is present
/// under the same name and version with *different* bytes is a loud
/// conflict, never an in-place overwrite.
pub fn install_set(
    node: &mut Node,
    store: &Store,
    tools_root: &Path,
    manifest: &ToolSetManifest,
    artifacts: &Path,
) -> Result<InstalledSet> {
    shenko_wasm::ensure_manifest_matches_directory(manifest, artifacts)?;
    let mut installed = Vec::with_capacity(manifest.tools.len());
    let mut already_present = Vec::new();
    let facts = node.tool_unit_facts()?;
    for entry in &manifest.tools {
        let path = artifacts.join(&entry.file);
        // The receipt digest is re-verified against the file before the
        // artifact is admitted: the sha256 is provenance, the blake3 digest
        // computed here is the identity that gets bound.
        let verified = manifest.verify_artifact(entry, artifacts)?;
        let unit_id = format!("{}{}", shenko_wasm::TOOL_UNIT_PREFIX, entry.name);
        if let Some(fact) = facts.iter().find(|fact| fact.unit_id == unit_id) {
            let recorded = fact
                .tool
                .get("version")
                .and_then(|version| version.as_str())
                .unwrap_or_default();
            if recorded == manifest.version {
                let existing = fact
                    .tool
                    .get("artifact")
                    .and_then(|artifact| artifact.get("digest"))
                    .and_then(|digest| digest.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| fact.artifact_digest.to_string());
                ensure!(
                    existing == verified.digest.to_string(),
                    "TOOL_INSTALL_CONFLICT:{}@{}: a different artifact is already installed \
                     under this version",
                    entry.name,
                    manifest.version
                );
                already_present.push(format!("{}@{}", entry.name, manifest.version));
                continue;
            }
        }
        let request = InstallRequest {
            name: entry.name.clone(),
            version: manifest.version.clone(),
            abi: manifest.abi,
            entrypoint: entry.entrypoint.clone(),
            source: ToolSource {
                uri: manifest.source.uri.clone(),
                release: manifest.source.release.clone(),
                provenance: manifest.source.provenance.clone(),
            },
            license: Some(manifest.license.spdx.clone()),
            sha256: Some(verified.sha256.clone()),
            path,
        };
        installed.push(install_one(node, store, tools_root, &request)?);
    }
    Ok(InstalledSet {
        installed,
        already_present,
        set: manifest.set.clone(),
        version: manifest.version.clone(),
        license: manifest.license.spdx.clone(),
    })
}
