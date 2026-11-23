//! Retained inputs of one Start, including the in-process Android host.
//!
//! The Android foreground service loads the node as a cdylib and calls
//! `run_embedded_start`. That entry has no argv of its own: the service reads
//! an `Intent` extra and nothing else, and an `Intent` is ambient data a later
//! service delivery, replayed broadcast or recreated activity may change behind
//! the same process. The inputs the runtime actually starts with are therefore
//! a retained fact, not an ambient value:
//!
//! * the exact start configuration is sealed in CAS and named by the private
//!   `start-inputs.json` pointer under the installation data dir;
//! * every configuration file the runtime reads is retained by content and its
//!   current bytes are re-verified before a later start, so a file that changed
//!   on disk cannot silently substitute a different channel or target set;
//! * the transport access token never enters the document. The Start contract
//!   keeps that secret out of URLs, logs, receipts and bindings, so the
//!   retained document records only whether a token was configured. Possession
//!   of the retained document is never possession of the secret.
//!
//! Retention records what a real launch asked for; it is not authority and it
//! never creates an activation selector, a stage or a route. A later start
//! either presents the retained inputs exactly or is refused with a typed
//! `START_CUSTODY_FROZEN_INPUTS` reason. The single deliberate exception is the
//! first start of an installation, which retains its own request once; every
//! later start must match that document byte for byte.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use shenko_store::cas::ContentStore;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

use crate::Command;

/// The private pointer naming this installation's retained Start inputs.
pub(crate) const CURRENT_START_INPUTS: &str = "start-inputs.json";

/// The only Start-input document major this build reads or writes.
pub(crate) const START_INPUTS_SCHEMA_VERSION: u32 = 1;

/// Codec identity of a retained Start document in CAS.
const CODEC_NAME: &str = "start-inputs";
const CODEC_MAJOR: u32 = 1;

/// A retained Start document and the pointer naming it stay small; a Start
/// with a channel/target list larger than this is not this product's request.
const MAX_DOCUMENT_BYTES: u64 = 256 * 1024;

/// One retained configuration file: where the runtime reads it from and the
/// immutable bytes it must still have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputFile {
    path: PathBuf,
    object: ObjectRef,
}

/// The exact non-secret inputs of one Start.
///
/// Field order is the serialization order: two equal requests produce equal
/// bytes, so a later request can be compared to the retained document exactly
/// instead of field by field with ambiguous defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FrozenStart {
    schema_version: u32,
    port: u16,
    pause_at_start: Option<EntityId>,
    name: Option<String>,
    napcat: Option<String>,
    napcat_account: Option<u64>,
    /// Whether the launch was configured with a transport access token. The
    /// token itself is deliberately absent (see the module documentation).
    transport_token_present: bool,
    napcat_admin_qq: Vec<u64>,
    napcat_file_root: Option<PathBuf>,
    napcat_file_host: Option<String>,
    napcat_cli_url: Option<String>,
    channels_config: Option<PathBuf>,
    ssh_targets: Option<PathBuf>,
    /// Retained bytes of `channels_config` and `ssh_targets`, in that order.
    files: Vec<InputFile>,
}

/// The inputs a Start request asks for, before any of them are retained.
#[derive(Debug)]
pub(crate) struct StartRequest {
    port: u16,
    pause_at_start: Option<EntityId>,
    name: Option<String>,
    napcat: Option<String>,
    napcat_account: Option<u64>,
    transport_token_present: bool,
    napcat_admin_qq: Vec<u64>,
    napcat_file_root: Option<PathBuf>,
    napcat_file_host: Option<String>,
    napcat_cli_url: Option<String>,
    channels_config: Option<PathBuf>,
    ssh_targets: Option<PathBuf>,
}

impl StartRequest {
    /// The request of the in-process Android host.
    ///
    /// The foreground service supplies a port and nothing else. There is no
    /// renamed ELF, no argv and no host environment carrying channel or SSH
    /// configuration into the node, so those inputs stay unset rather than
    /// being read from ambient process state.
    pub(crate) fn in_process(port: u16) -> Self {
        Self {
            port,
            pause_at_start: None,
            name: None,
            napcat: None,
            napcat_account: None,
            transport_token_present: false,
            napcat_admin_qq: Vec::new(),
            napcat_file_root: None,
            napcat_file_host: None,
            napcat_cli_url: None,
            channels_config: None,
            ssh_targets: None,
        }
    }

}

fn refusal(field: &str) -> anyhow::Error {
    anyhow!(
        "START_CUSTODY_FROZEN_INPUTS: Start field `{field}` differs from the retained request; \
         supplying a different channel, target or pause configuration to the same installation is refused"
    )
}

/// Read the private pointer. Absence is a real answer; anything other than a
/// literal regular file is not absence.
fn read_pointer(data_dir: &Path) -> Result<Option<ObjectRef>> {
    let path = data_dir.join(CURRENT_START_INPUTS);
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| {
            format!("START_CUSTODY_FROZEN_INPUTS: inspect {}", path.display())
        }),
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(anyhow!(
                "START_CUSTODY_FROZEN_INPUTS: {} is not a literal regular file",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    let bytes = crate::update_control::private_read(&path)?;
    let reference: ObjectRef = serde_json::from_slice(&bytes)
        .with_context(|| format!("START_CUSTODY_FROZEN_INPUTS: decode {}", path.display()))?;
    Ok(Some(reference))
}

fn publish_pointer(data_dir: &Path, reference: &ObjectRef) -> Result<()> {
    let staging = data_dir.join(format!(".start-inputs-{}", EntityId::new()));
    crate::update_control::private_write(&staging, &serde_json::to_vec(reference)?)?;
    std::fs::rename(&staging, data_dir.join(CURRENT_START_INPUTS))?;
    std::fs::File::open(data_dir)?.sync_all()?;
    Ok(())
}

/// Read a retained object and prove it is still exactly the bytes its
/// reference names.
fn verified(store: &ContentStore, reference: &ObjectRef) -> Result<Vec<u8>> {
    let bytes = store
        .read_object(&reference.digest)
        .map_err(|error| anyhow!("START_CUSTODY_FROZEN_INPUTS: retained Start object is unavailable: {error}"))?;
    ensure!(
        bytes.len() as u64 == reference.bytes && Digest::blake3(&bytes) == reference.digest,
        "START_CUSTODY_FROZEN_INPUTS: retained Start object differs from its reference"
    );
    Ok(bytes)
}

fn seal_input_file(store: &ContentStore, path: &Path) -> Result<InputFile> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("START_CUSTODY_FROZEN_INPUTS: read Start input {}", path.display()))?;
    let object = store
        .seal_object(&bytes, "application/json", CODEC_NAME, "configuration-file", 1)?
        .object;
    Ok(InputFile { path: path.to_owned(), object })
}

fn canonical_input_path(path: Option<PathBuf>) -> Result<Option<PathBuf>> {
    path.map(|path| {
        std::fs::canonicalize(&path)
            .with_context(|| format!("START_CUSTODY_FROZEN_INPUTS: resolve Start input {}", path.display()))
    })
    .transpose()
}

/// Load a retained document and re-verify every byte it depends on.
pub(crate) fn load(store: &ContentStore, reference: &ObjectRef) -> Result<FrozenStart> {
    ensure!(
        reference.codec.name == CODEC_NAME && reference.codec.major == CODEC_MAJOR,
        "START_CUSTODY_SCHEMA: unsupported retained Start codec {}:{}",
        reference.codec.name,
        reference.codec.major
    );
    ensure!(
        reference.bytes <= MAX_DOCUMENT_BYTES,
        "START_CUSTODY_FROZEN_INPUTS: retained Start document exceeds its bound"
    );
    let bytes = verified(store, reference)?;
    let frozen: FrozenStart = serde_json::from_slice(&bytes)
        .context("START_CUSTODY_FROZEN_INPUTS: retained Start document is malformed")?;
    ensure!(
        frozen.schema_version == START_INPUTS_SCHEMA_VERSION,
        "START_CUSTODY_SCHEMA: unsupported retained Start document schema {} (this build reads {})",
        frozen.schema_version,
        START_INPUTS_SCHEMA_VERSION
    );
    for file in &frozen.files {
        let retained = verified(store, &file.object)?;
        let current = std::fs::read(&file.path)
            .with_context(|| format!("START_CUSTODY_FROZEN_INPUTS: read retained Start input {}", file.path.display()))?;
        ensure!(
            current == retained,
            "START_CUSTODY_FROZEN_INPUTS: Start input {} changed independently; refusing to substitute a different configuration",
            file.path.display()
        );
    }
    Ok(frozen)
}

/// Admit one Start request and return the retained document it must match.
///
/// The first start of an installation retains its own request durably. Every
/// later start must present exactly those inputs (including the exact bytes of
/// the configuration files they name) or is refused.
pub(crate) fn admit(data_dir: &Path, request: StartRequest) -> Result<FrozenStart> {
    let channels_config = canonical_input_path(request.channels_config.clone())?;
    let ssh_targets = canonical_input_path(request.ssh_targets.clone())?;
    let start_inputs_config = channels_config.clone();
    let start_inputs_targets = ssh_targets.clone();
    let store = ContentStore::new(data_dir.join("cas"))?;
    let mut files = Vec::new();
    for path in [start_inputs_config.as_deref(), start_inputs_targets.as_deref()].into_iter().flatten() {
        files.push(seal_input_file(&store, path)?);
    }
    let requested = FrozenStart {
        schema_version: START_INPUTS_SCHEMA_VERSION,
        port: request.port,
        pause_at_start: request.pause_at_start,
        name: request.name,
        napcat: request.napcat,
        napcat_account: request.napcat_account,
        transport_token_present: request.transport_token_present,
        napcat_admin_qq: request.napcat_admin_qq,
        napcat_file_root: request.napcat_file_root,
        napcat_file_host: request.napcat_file_host,
        napcat_cli_url: request.napcat_cli_url,
        channels_config,
        ssh_targets,
        files,
    };
    let Some(reference) = read_pointer(data_dir)? else {
        let bytes = serde_json::to_vec(&requested)?;
        ensure!(
            bytes.len() as u64 <= MAX_DOCUMENT_BYTES,
            "START_CUSTODY_FROZEN_INPUTS: this Start request exceeds the retained document bound"
        );
        let object = store
            .seal_object(&bytes, "application/json", CODEC_NAME, CODEC_NAME, CODEC_MAJOR)?
            .object;
        publish_pointer(data_dir, &object)?;
        return Ok(requested);
    };
    let retained = load(&store, &reference)?;
    ensure!(
        retained == requested,
        "START_CUSTODY_FROZEN_INPUTS: this launch differs from the retained Start request; refusing to overwrite the retained inputs"
    );
    Ok(retained)
}

/// Return the retained Start inputs of this installation, refusing when there
/// are none. Used by an in-process successor start, which must never invent a
/// start request of its own.
pub(crate) fn require(data_dir: &Path) -> Result<FrozenStart> {
    let reference = read_pointer(data_dir)?.ok_or_else(|| {
        anyhow!(
            "START_CUSTODY_FROZEN_INPUTS: this installation has no retained Start request; \
             an in-process successor start requires the original retained inputs"
        )
    })?;
    let store = ContentStore::open_existing(data_dir.join("cas"))
        .map_err(|error| anyhow!("START_CUSTODY_FROZEN_INPUTS: open the retained content store: {error}"))?;
    load(&store, &reference)
}

impl FrozenStart {
    /// The Start command these retained inputs describe.
    ///
    /// The transport access token is not part of the document, so a start that
    /// needs one must refuse (`matches_command` does) instead of silently
    /// running without it.
    pub(crate) fn into_command(&self) -> Command {
        Command::Start {
            port: self.port,
            pause_at_start: self.pause_at_start,
            name: self.name.clone(),
            napcat: self.napcat.clone(),
            napcat_account: self.napcat_account,
            napcat_access_token: None,
            napcat_admin_qq: self.napcat_admin_qq.clone(),
            napcat_file_root: self.napcat_file_root.clone(),
            napcat_file_host: self.napcat_file_host.clone(),
            napcat_cli_url: self.napcat_cli_url.clone(),
            channels_config: self.channels_config.clone(),
            ssh_targets: self.ssh_targets.clone(),
        }
    }

    /// Compare the composition's Start configuration with the retained inputs.
    ///
    /// This runs before home preparation, genesis or any runtime service, so
    /// an ambient delivery that disagrees with the retained request changes
    /// nothing durable and never reaches the runtime.
    pub(crate) fn matches_start(
        &self,
        config: &crate::runtime_start::StartConfig,
    ) -> Result<()> {
        if config.port != self.port {
            return Err(refusal("port"));
        }
        if config.pause_at_start != self.pause_at_start {
            return Err(refusal("pause_at_start"));
        }
        if config.name.as_deref() != self.name.as_deref() {
            return Err(refusal("name"));
        }
        let napcat = &config.channels.napcat;
        if napcat.napcat != self.napcat {
            return Err(refusal("napcat"));
        }
        if napcat.napcat_account != self.napcat_account {
            return Err(refusal("napcat_account"));
        }
        // A configured transport token is a different transport request even
        // though the value itself is never retained.
        if napcat.napcat_access_token.is_some() != self.transport_token_present {
            return Err(refusal("napcat_access_token"));
        }
        if napcat.napcat_admin_qq != self.napcat_admin_qq {
            return Err(refusal("napcat_admin_qq"));
        }
        if napcat.napcat_file_root.as_deref() != self.napcat_file_root.as_deref() {
            return Err(refusal("napcat_file_root"));
        }
        if napcat.napcat_file_host != self.napcat_file_host {
            return Err(refusal("napcat_file_host"));
        }
        if napcat.napcat_cli_url != self.napcat_cli_url {
            return Err(refusal("napcat_cli_url"));
        }
        if config.channels.channels_config.as_deref() != self.channels_config.as_deref() {
            return Err(refusal("channels_config"));
        }
        if config.ssh_targets.as_deref() != self.ssh_targets.as_deref() {
            return Err(refusal("ssh_targets"));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("shenko-start-inputs-{label}-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn first_start_retains_its_request_and_later_matching_starts_are_idempotent() -> Result<()> {
        let root = temp_root("retain");
        let first = admit(&root, StartRequest::in_process(7860))?;
        let second = admit(&root, StartRequest::in_process(7860))?;
        assert_eq!(first, second);
        assert_eq!(require(&root)?, first);
        // The pointer is a private literal file, not a symlink or directory.
        let metadata = std::fs::symlink_metadata(root.join(CURRENT_START_INPUTS))?;
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_changed_start_request_is_refused_and_keeps_the_original_document() -> Result<()> {
        let root = temp_root("changed");
        let first = admit(&root, StartRequest::in_process(7860))?;
        let pointer_before = std::fs::read(root.join(CURRENT_START_INPUTS))?;
        let error = admit(&root, StartRequest::in_process(7861)).unwrap_err().to_string();
        assert!(error.contains("START_CUSTODY_FROZEN_INPUTS"), "unexpected refusal: {error}");
        assert_eq!(std::fs::read(root.join(CURRENT_START_INPUTS))?, pointer_before);
        assert_eq!(require(&root)?, first);
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_configuration_file_that_changed_after_retention_is_refused() -> Result<()> {
        let root = temp_root("file");
        let config = root.join("channels.json");
        std::fs::write(&config, b"{\"qq_official\":{\"enabled\":false}}\n")?;
        let mut request = StartRequest::in_process(7860);
        request.channels_config = Some(config.clone());
        admit(&root, request)?;
        std::fs::write(&config, b"{\"qq_official\":{\"enabled\":true}}\n")?;
        let error = require(&root).unwrap_err().to_string();
        assert!(error.contains("START_CUSTODY_FROZEN_INPUTS"), "unexpected refusal: {error}");
        assert!(error.contains("changed independently"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }

    #[test]
    fn a_missing_retained_request_is_refused_rather_than_invented() -> Result<()> {
        let root = temp_root("absent");
        let error = require(&root).unwrap_err().to_string();
        assert!(error.contains("no retained Start request"), "unexpected refusal: {error}");
        std::fs::remove_dir_all(&root)?;
        Ok(())
    }
}
