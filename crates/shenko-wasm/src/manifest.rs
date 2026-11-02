//! Tool-set manifests — the durable description of a built tool set.
//!
//! A *tool set* is one upstream project ported to WASI: the toybox applets
//! built by `crates/shenko-wasm/ports/toybox-wasi/build.sh`, a Git client, a
//! Gitea CLI, an SSH client. The manifest is what the product consumes:
//!
//! * it names the upstream project, its release, its license and the exact
//!   source archive digest, so an installed tool never loses its provenance;
//! * it records the compatibility class the artifacts really have (WASI
//!   preview1, a component, WASIX, dedicated host imports) instead of a
//!   marketing label;
//! * it lists every artifact with its **sha256 provenance digest** from the
//!   build receipt. The sha256 says which upstream bytes were received; the
//!   **blake3 digest of the same bytes** is the identity the installation
//!   binds. The two are never compared against each other and never
//!   substituted for each other.
//!
//! The manifest is produced from a build receipt (`shenko tool manifest`) and
//! consumed by the installer (`shenko tool install-set`). Nothing in this
//! module downloads, builds or executes anything.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::artifact::{ToolContentStore, stream_digest};
use crate::tool::ToolAbi;

/// Schema version of the tool-set manifest.
pub const TOOL_SET_MANIFEST_SCHEMA: u32 = 1;

/// The media type a tool-set manifest is stored under.
pub const TOOL_SET_MANIFEST_MEDIA_TYPE: &str = "application/json";

/// The license of the upstream project the artifacts come from. The license
/// text itself ships with the port; the manifest records which file it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetLicense {
    /// SPDX identifier, for example `BSD-3-Clause`.
    pub spdx: String,
    /// Path of the license text shipped with the port, relative to the port
    /// directory.
    pub file: String,
    /// Optional copyright notice.
    #[serde(default)]
    pub notice: Option<String>,
}

/// Where the upstream sources came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetSource {
    /// Upstream project URL.
    pub uri: String,
    /// Upstream release the artifacts were built from.
    pub release: String,
    /// How the artifacts were obtained: `source_build:<toolchain>` for a
    /// local port build, `upstream_release` for a published binary.
    pub provenance: String,
    /// sha256 of the upstream source archive. Provenance, never identity.
    #[serde(default)]
    pub source_sha256: Option<String>,
}

/// How the set was built. This is evidence, not a promise: the recipe that
/// produced the receipt is the port directory named here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetBuild {
    /// The toolchain that produced the artifacts, for example
    /// `wasi-sdk-34.0 (clang 23.1.0)`.
    pub toolchain: String,
    /// The build receipt that recorded the per-artifact sha256 digests.
    pub receipt: String,
    /// The port recipe that produced the artifacts.
    #[serde(default)]
    pub recipe: Option<String>,
}

/// One artifact of the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetEntry {
    /// Installation name, for example `toybox-cat`.
    pub name: String,
    /// The upstream command the artifact implements, for example `cat`.
    pub applet: String,
    /// File name of the artifact inside the artifact directory.
    pub file: String,
    /// Exact byte length of the artifact.
    pub bytes: u64,
    /// sha256 recorded by the build receipt. Provenance only: it says which
    /// upstream bytes were received.
    pub sha256: String,
    /// blake3 identity of the artifact bytes, when the manifest pins it.
    /// This is what the installation binds, and it is what a peer or a seed
    /// verifies the bytes against. It is never compared against the sha256.
    ///
    /// A manifest built by `shenko tool manifest` always pins it. A manifest
    /// that only carries the upstream provenance is still installable: the
    /// installer computes the identity from the bytes it admits and binds
    /// that.
    #[serde(default)]
    pub identity: Option<String>,
    /// Guest argv[0] label used when the tool is executed.
    pub entrypoint: String,
}

/// A built tool set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolSetManifest {
    pub schema: u32,
    /// Set name, for example `toybox-wasi`.
    pub set: String,
    /// Upstream release, for example `0.8.15`.
    pub version: String,
    /// Prefix of the installation names, for example `toybox`.
    pub name_prefix: String,
    pub source: ToolSetSource,
    pub license: ToolSetLicense,
    /// The compatibility class the artifacts really have.
    pub abi: ToolAbi,
    pub build: ToolSetBuild,
    /// Every artifact of the set.
    pub tools: Vec<ToolSetEntry>,
}

impl ToolSetManifest {
    /// Parse and validate a manifest.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let manifest: ToolSetManifest =
            serde_json::from_slice(bytes).context("parse tool-set manifest")?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject a manifest that could not be installed honestly: unknown
    /// schema, empty set, duplicate names or files, a malformed sha256, or a
    /// name the tool layer would not accept.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == TOOL_SET_MANIFEST_SCHEMA,
            "TOOL_SET_MANIFEST_UNSUPPORTED: schema {}",
            self.schema
        );
        ensure!(!self.set.is_empty(), "TOOL_SET_MANIFEST_INVALID: empty set name");
        ensure!(
            !self.version.is_empty(),
            "TOOL_SET_MANIFEST_INVALID: empty set version"
        );
        ensure!(
            !self.name_prefix.is_empty(),
            "TOOL_SET_MANIFEST_INVALID: empty name prefix"
        );
        ensure!(
            !self.tools.is_empty(),
            "TOOL_SET_MANIFEST_INVALID: set {set} has no artifacts",
            set = self.set
        );
        crate::artifact::validate_provenance(self.source.source_sha256.as_deref())?;
        let mut names = BTreeSet::new();
        let mut files = BTreeSet::new();
        for entry in &self.tools {
            validate_manifest_name(&entry.name)?;
            ensure!(
                names.insert(entry.name.clone()),
                "TOOL_SET_MANIFEST_INVALID: duplicate tool name {}",
                entry.name
            );
            ensure!(
                files.insert(entry.file.clone()),
                "TOOL_SET_MANIFEST_INVALID: duplicate artifact file {}",
                entry.file
            );
            ensure!(
                !entry.applet.is_empty(),
                "TOOL_SET_MANIFEST_INVALID: tool {} has no applet",
                entry.name
            );
            ensure!(
                !entry.file.is_empty() && !entry.file.contains('/') && entry.file != "..",
                "TOOL_SET_MANIFEST_INVALID: tool {} has an unusable artifact file name {:?}",
                entry.name,
                entry.file
            );
            crate::artifact::validate_provenance(Some(entry.sha256.as_str()))?;
            if let Some(identity) = entry.identity.as_deref() {
                crate::artifact::validate_identity(identity)?;
            }
            ensure!(
                entry.bytes > 0,
                "TOOL_SET_MANIFEST_INVALID: tool {} has no recorded length",
                entry.name
            );
        }
        Ok(())
    }

    /// Look up one artifact of the set.
    pub fn entry(&self, name: &str) -> Option<&ToolSetEntry> {
        self.tools.iter().find(|entry| entry.name == name)
    }

    /// Verify one artifact against the manifest entry.
    ///
    /// The file is read once, in fixed-size blocks: the sha256 (provenance)
    /// and the blake3 (identity) are computed from the same pass, and the
    /// length is checked against the manifest. A mismatch is reported as
    /// exactly that — a provenance mismatch is not an identity mismatch.
    pub fn verify_artifact(&self, entry: &ToolSetEntry, directory: &Path) -> Result<VerifiedArtifact> {
        let path = directory.join(&entry.file);
        let file = std::fs::File::open(&path)
            .with_context(|| format!("open artifact {}", path.display()))?;
        let mut reader = std::io::BufReader::with_capacity(
            crate::artifact::ARTIFACT_STREAM_BUFFER,
            file,
        );
        let mut sha = Sha256::new();
        let mut identity = blake3::Hasher::new();
        let mut buffer = vec![0u8; crate::artifact::ARTIFACT_STREAM_BUFFER];
        let mut total = 0u64;
        loop {
            let count = match std::io::Read::read(&mut reader, &mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                other => other.with_context(|| format!("read artifact {}", path.display()))?,
            };
            if count == 0 {
                break;
            }
            sha.update(&buffer[..count]);
            identity.update(&buffer[..count]);
            total += count as u64;
        }
        let sha256 = format!("{:x}", sha.finalize());
        let digest = shenko_types::object::Digest(format!(
            "blake3:{}",
            identity.finalize().to_hex()
        ));
        ensure!(
            total == entry.bytes,
            "TOOL_ARTIFACT_SIZE_MISMATCH: {} is {total} bytes, manifest records {}",
            entry.name,
            entry.bytes
        );
        ensure!(
            sha256 == entry.sha256.to_ascii_lowercase(),
            "TOOL_PROVENANCE_MISMATCH: {} hashes to {sha256}, manifest records {}",
            entry.name,
            entry.sha256
        );
        if let Some(pinned) = entry.identity.as_deref() {
            ensure!(
                digest.to_string() == pinned,
                "TOOL_ARTIFACT_DIGEST_MISMATCH: {} is {digest}, the manifest pins {pinned}",
                entry.name
            );
        }
        Ok(VerifiedArtifact {
            bytes: total,
            sha256,
            digest,
        })
    }
}

/// The verified facts about one artifact on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedArtifact {
    pub bytes: u64,
    /// The upstream provenance digest recomputed from the file.
    pub sha256: String,
    /// The blake3 identity of the same bytes.
    pub digest: shenko_types::object::Digest,
}

/// The digest of a stream, as the install path uses it for the artifact
/// identity.
pub fn artifact_identity(reader: &mut impl std::io::Read) -> Result<(shenko_types::object::Digest, u64)> {
    stream_digest(reader)
}

/// Reject a name the tool layer would not accept.
pub fn validate_manifest_name(name: &str) -> Result<()> {
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

/// A parsed build receipt.
///
/// The receipt format is the one the port recipes write: `key=value` header
/// lines followed by one `sha256  path` line per produced artifact.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildReceipt {
    pub fields: BTreeMap<String, String>,
    /// `(sha256, produced path)` in receipt order.
    pub artifacts: Vec<(String, String)>,
}

impl BuildReceipt {
    /// Parse a build receipt.
    pub fn parse(text: &str) -> Result<Self> {
        let mut receipt = BuildReceipt::default();
        for line in text.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            if let Some((sha256, path)) = split_receipt_artifact(line) {
                receipt.artifacts.push((sha256, path));
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                receipt
                    .fields
                    .insert(key.trim().to_string(), value.trim().to_string());
            }
        }
        ensure!(
            !receipt.artifacts.is_empty(),
            "TOOL_RECEIPT_EMPTY: the build receipt lists no artifacts"
        );
        Ok(receipt)
    }

    /// One header field of the receipt.
    pub fn field(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }
}

/// Split a `sha256  path` receipt line. The separator is two spaces, which
/// is what `sha256sum` writes; a single space is accepted as well because a
/// hand-written receipt may use it.
fn split_receipt_artifact(line: &str) -> Option<(String, String)> {
    let (left, right) = line.split_once(char::is_whitespace)?;
    let left = left.trim();
    let right = right.trim();
    if left.len() != 64 || !left.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    if right.is_empty() {
        return None;
    }
    Some((left.to_ascii_lowercase(), right.to_string()))
}

/// The metadata a receipt cannot carry by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSetMeta {
    /// Set name, for example `toybox-wasi`.
    pub set: String,
    /// Upstream release, for example `0.8.15`.
    pub version: String,
    /// Prefix of the installation names, for example `toybox`.
    pub name_prefix: String,
    /// SPDX license identifier of the upstream project.
    pub license_spdx: String,
    /// Path of the license text shipped with the port.
    pub license_file: String,
    /// Optional copyright notice.
    pub license_notice: Option<String>,
    /// Upstream project URL.
    pub source_uri: String,
    /// How the artifacts were obtained.
    pub provenance: String,
    /// The compatibility class the artifacts really have.
    pub abi: ToolAbi,
    /// The port recipe that produced the artifacts.
    pub recipe: Option<String>,
    /// The build receipt path, recorded as evidence.
    pub receipt_path: String,
}

impl ToolSetManifest {
    /// Build a manifest from a build receipt.
    ///
    /// The receipt's per-artifact sha256 values become the manifest's
    /// provenance digests; the artifact length is read from the artifact
    /// itself, so a manifest never claims a length it did not measure.
    pub fn from_receipt(
        receipt: &BuildReceipt,
        meta: &ToolSetMeta,
        artifacts: &Path,
    ) -> Result<Self> {
        let mut tools = Vec::with_capacity(receipt.artifacts.len());
        for (sha256, produced) in &receipt.artifacts {
            let produced_path = PathBuf::from(produced);
            let file = produced_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .context("build receipt artifact without a file name")?;
            let applet = file.clone();
            // The length is measured in the artifact directory the manifest
            // describes, not at the path the receipt happened to record: the
            // same bytes may live in a different directory now.
            let path = artifacts.join(&file);
            let bytes = std::fs::metadata(&path)
                .with_context(|| format!("stat artifact {}", path.display()))?
                .len();
            // The manifest pins the artifact's identity as well as its
            // upstream provenance, so a peer or a seed can verify the bytes
            // without installing them first.
            let identity = {
                let handle = std::fs::File::open(&path)
                    .with_context(|| format!("open artifact {}", path.display()))?;
                let mut reader = std::io::BufReader::with_capacity(
                    crate::artifact::ARTIFACT_STREAM_BUFFER,
                    handle,
                );
                let (digest, _provenance, length) =
                    crate::artifact::stream_identity(&mut reader)?;
                ensure!(
                    length == bytes,
                    "TOOL_ARTIFACT_SIZE_MISMATCH: {} is {length} bytes, its file reports {bytes}",
                    file
                );
                digest.to_string()
            };
            let name = format!("{}-{}", meta.name_prefix, applet);
            let entrypoint = format!("/tools/{name}");
            tools.push(ToolSetEntry {
                name,
                applet,
                file,
                bytes,
                sha256: sha256.to_ascii_lowercase(),
                identity: Some(identity),
                entrypoint,
            });
        }
        let toolchain = receipt
            .field("wasi_sdk")
            .map(|sdk| format!("wasi-sdk ({sdk})"))
            .unwrap_or_else(|| "unknown".into());
        let manifest = ToolSetManifest {
            schema: TOOL_SET_MANIFEST_SCHEMA,
            set: meta.set.clone(),
            version: meta.version.clone(),
            name_prefix: meta.name_prefix.clone(),
            source: ToolSetSource {
                uri: meta.source_uri.clone(),
                release: meta.version.clone(),
                provenance: meta.provenance.clone(),
                source_sha256: receipt.field("source_sha256").map(str::to_string),
            },
            license: ToolSetLicense {
                spdx: meta.license_spdx.clone(),
                file: meta.license_file.clone(),
                notice: meta.license_notice.clone(),
            },
            abi: meta.abi,
            build: ToolSetBuild {
                toolchain,
                receipt: meta.receipt_path.clone(),
                recipe: meta.recipe.clone(),
            },
            tools,
        };
        manifest.validate()?;
        // The artifacts directory must actually hold the artifacts the receipt
        // names; a manifest that points at a missing file is not installable.
        for entry in &manifest.tools {
            let path = artifacts.join(&entry.file);
            ensure!(
                path.is_file(),
                "TOOL_SET_ARTIFACT_MISSING: {} is not in {}",
                entry.file,
                artifacts.display()
            );
        }
        Ok(manifest)
    }
}

/// The install plan for one artifact: what the installer has to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInstallPlan {
    pub entry: ToolSetEntry,
    /// The verified identity and provenance of the artifact on disk.
    pub verified: VerifiedArtifact,
    /// Where the artifact bytes will live after the install.
    pub storage: ToolInstallStorage,
}

/// Where an installed artifact's bytes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolInstallStorage {
    /// Inlined by the installation unit, because the artifact fits the node's
    /// inline object store.
    NodeInline,
    /// Staged as chunk objects in the installation's content store, described
    /// by a chunk manifest that the ledger's recovery closure carries.
    Chunked {
        manifest: shenko_types::object::ObjectRef,
        manifest_digest: shenko_types::object::Digest,
        chunks: usize,
    },
}

/// Plan the install of a whole set: verify every artifact against the
/// manifest, then decide where each artifact's bytes live.
///
/// Artifacts that fit the node's inline unit store are inlined, so node
/// replication keeps carrying them. Larger artifacts are staged as chunk
/// objects and bound into the recovery closure instead.
pub fn plan_install(
    manifest: &ToolSetManifest,
    artifacts: &Path,
    node_inline_max_bytes: u64,
    store: &dyn ToolContentStore,
) -> Result<Vec<ToolInstallPlan>> {
    let mut plans = Vec::with_capacity(manifest.tools.len());
    for entry in &manifest.tools {
        let verified = manifest.verify_artifact(entry, artifacts)?;
        let storage = if verified.bytes <= node_inline_max_bytes {
            ToolInstallStorage::NodeInline
        } else {
            let staged = crate::artifact::stage_artifact_file(
                store,
                &artifacts.join(&entry.file),
                Some(entry.sha256.clone()),
            )?;
            ToolInstallStorage::Chunked {
                manifest: staged.manifest,
                manifest_digest: staged.manifest_digest,
                chunks: staged.chunks,
            }
        };
        plans.push(ToolInstallPlan {
            entry: entry.clone(),
            verified,
            storage,
        });
    }
    Ok(plans)
}

/// Fail when a manifest entry and an artifact disagree about provenance.
pub fn check_provenance(manifest: &ToolSetManifest, plan: &ToolInstallPlan) -> Result<()> {
    crate::artifact::ensure_provenance_matches(
        Some(plan.entry.sha256.as_str()),
        Some(plan.verified.sha256.as_str()),
    )
}

/// The honest reason a set cannot be installed as a whole.
pub fn describe_set_state(manifest: &ToolSetManifest) -> String {
    format!(
        "{} {} — {} artifacts, abi {}, license {}, upstream {}",
        manifest.set,
        manifest.version,
        manifest.tools.len(),
        manifest.abi.name(),
        manifest.license.spdx,
        manifest.source.uri
    )
}

/// Fail when the manifest and the artifact directory describe different sets.
pub fn ensure_manifest_matches_directory(
    manifest: &ToolSetManifest,
    artifacts: &Path,
) -> Result<()> {
    for entry in &manifest.tools {
        let path = artifacts.join(&entry.file);
        if !path.is_file() {
            bail!(
                "TOOL_SET_ARTIFACT_MISSING: {} is not in {}",
                entry.file,
                artifacts.display()
            );
        }
    }
    Ok(())
}
