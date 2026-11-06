//! Installation-owned update control. Live RPC only prepares shutdown; the
//! finite helper owns final catchup/selection after the old RuntimeOwner dies.
//! The same owner descriptor crosses successor exec without an unlocked gap.
use std::{path::{Path, PathBuf}, sync::Arc};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use shenko_update::{cli::UpdateCommand, handoff::{HandoffParticipantRegistry, UpdateService}};
use shenko_types::{ids::EntityId, object::Digest};
const ENDPOINT: &str = "update-control.json";
const LIMIT: u64 = 1024 * 1024;
pub struct LiveControl(pub PathBuf);
#[async_trait::async_trait]
impl shenko_update::native::ProcessHandoffControl for LiveControl {
    async fn request(&self, command: UpdateCommand) -> Result<serde_json::Value> {
        request_live(command, &self.0).await
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint { address: std::net::SocketAddr, capability: String, runtime_id: EntityId }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request { capability: String, command: UpdateCommand }
#[derive(Serialize, Deserialize)]
struct Response { result: Option<serde_json::Value>, error: Option<String> }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffRequest {
    schema_version: u32,
    request_id: EntityId,
    agent_id: EntityId,
    installation_id: EntityId,
    command: UpdateCommand,
    source_selector: shenko_types::activation::ActivationSelector,
    source_slot: PathBuf,
    helper_digest: Digest,
}

fn service(data_dir: &Path, store: Arc<shenko_store::Store>, participants: Arc<HandoffParticipantRegistry>) -> Result<(UpdateService, EntityId)> {
    let agent_id = store.get_existing_agent_id()?.context("subject missing")?;
    let slot = shenko_update::slots::resolve_live_slot(data_dir)?.unwrap_or_else(|| data_dir.to_owned());
    let authority = Arc::new(crate::update_runtime::NodeAuthority { data_dir: data_dir.to_owned(), store: Arc::clone(&store), agent_id });
    Ok((UpdateService::new(data_dir.to_owned(), slot, store, participants,
        Arc::new(shenko_update::build::CargoBuildProvider::default())).with_claim_source(authority), agent_id))
}

fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600); }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::File::open(path.parent().context("private control file has no parent")?)?.sync_all()?;
    Ok(())
}
fn private_read(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC); }
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file() && metadata.len() <= LIMIT, "invalid private control file");
    #[cfg(unix)] {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.mode() & 0o077 == 0 && metadata.uid() == unsafe { libc::geteuid() }, "control capability is not private to this installation owner");
    }
    let mut bytes = Vec::new();
    file.by_ref().take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!((bytes.len() as u64) <= LIMIT, "control file exceeds bound");
    Ok(bytes)
}

fn command_activation(command: &UpdateCommand) -> Option<(EntityId, u64)> {
    match command {
        UpdateCommand::Activate { activation, expected_selector_revision }
        | UpdateCommand::Rollback { activation, expected_selector_revision } => Some((*activation, *expected_selector_revision)),
        _ => None,
    }
}

async fn prepare_process_handoff(command: UpdateCommand, data_dir: &Path, store: &Arc<shenko_store::Store>, service: &UpdateService, agent: EntityId) -> Result<serde_json::Value> {
    let (activation, expected) = command_activation(&command).context("not a handoff command")?;
    let source_selector = store.activation_selector(agent)?.context("source selector missing")?;
    ensure!(source_selector.selector_revision == expected, "SELECTOR_REVISION_CONFLICT");
    let request_id = EntityId::new();
    let source_slot = shenko_update::slots::resolve_live_slot(data_dir)?.unwrap_or_else(|| data_dir.to_owned());
    let helper = std::env::current_exe()?;
    let request = HandoffRequest { schema_version: 1, request_id, agent_id: agent,
        installation_id: store.installation_id_for_subject(agent)?, command,
        source_selector, source_slot, helper_digest: Digest::blake3(&std::fs::read(&helper)?), };
    // Freeze the finite continuation BEFORE stopping its producer. This CAS
    // object and private no-replace file survive a helper/old-process crash.
    let bytes = serde_json::to_vec(&request)?;
    let frozen = store.cas.seal_object(&bytes, "application/json", "activation", "process-handoff-request", 1)?.object;
    let path = data_dir.join(format!("update-handoff-{request_id}.json"));
    private_write(&path, &bytes)?;
    let mut record = store.activation_record(activation)?.context("activation missing")?;
    let mut closure: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&record.recovery_closure_ref.digest)?)?;
    closure["process_handoff_request_ref"] = serde_json::to_value(&frozen)?;
    record.recovery_closure_ref = store.cas.seal_object(&serde_json::to_vec(&closure)?, "application/json", "activation", "recovery-closure", 1)?.object;
    store.update_activation_record(&record, shenko_types::ids::RecordRevision(record.record_revision))?;
    service.quiesce_for_process_handoff(activation, request_id, expected).await?;
    // RuntimeOwner is O_CLOEXEC: this finite helper MUST NOT inherit the old
    // owner's file description, otherwise waiting for retirement deadlocks.
    if let Err(error) = std::process::Command::new(helper).arg("--data-dir").arg(data_dir)
        .arg("complete-update").arg(&path).arg(frozen.digest.to_string())
        .stdin(std::process::Stdio::null()).spawn() {
        let resume = service.resume_process_handoff(agent, request_id).await;
        anyhow::bail!("handoff helper spawn failed: {error}; old-runtime resume: {resume:?}");
    }
    Ok(serde_json::json!({"request_id": request_id, "activation_id": activation,
        "request_ref": frozen, "state": "awaiting_old_process_retirement", "activation_succeeded": false}))
}

pub async fn spawn_control(data_dir: PathBuf, store: Arc<shenko_store::Store>, participants: Arc<HandoffParticipantRegistry>, cancel: tokio_util::sync::CancellationToken) -> Result<tokio::task::JoinHandle<Result<()>>> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let endpoint = Endpoint { address: listener.local_addr()?, capability: format!("{}{}", EntityId::new(), EntityId::new()), runtime_id: EntityId::new() };
    let staging = data_dir.join(format!(".update-control-{}", endpoint.runtime_id));
    private_write(&staging, &serde_json::to_vec(&endpoint)?)?;
    std::fs::rename(&staging, data_dir.join(ENDPOINT))?;
    std::fs::File::open(&data_dir)?.sync_all()?;
    let (service, agent) = service(&data_dir, Arc::clone(&store), participants)?;
    Ok(tokio::spawn(async move {
        loop {
            let (socket, peer) = tokio::select! { _ = cancel.cancelled() => return Ok(()), accepted = listener.accept() => accepted? };
            ensure!(peer.ip().is_loopback(), "nonlocal control connection");
            let (read, mut write) = socket.into_split();
            let mut reader = BufReader::new(read.take(LIMIT + 1));
            let mut line = String::new();
            let read = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut line)).await;
            let mut retire = false;
            let response = match read {
                Ok(Ok(count)) if count > 0 && (count as u64) <= LIMIT && line.ends_with('\n') => match serde_json::from_str::<Request>(&line) {
                    Ok(request) if request.capability == endpoint.capability => {
                        let result = if command_activation(&request.command).is_some() {
                            prepare_process_handoff(request.command, &data_dir, &store, &service, agent).await
                        } else { shenko_update::cli::execute(request.command, &service, agent).await };
                        match result {
                            Ok(result) => { retire = result.get("state").and_then(|value| value.as_str()) == Some("awaiting_old_process_retirement"); Response { result: Some(result), error: None } },
                            Err(error) => Response { result: None, error: Some(format!("{error:#}")) },
                        }
                    }
                    _ => Response { result: None, error: Some("control authentication or request rejected".into()) },
                },
                _ => Response { result: None, error: Some("control request incomplete or oversized".into()) },
            };
            let mut bytes = serde_json::to_vec(&response)?; bytes.push(b'\n');
            if let Err(error) = write.write_all(&bytes).await { tracing::warn!(%error, "update response lost; immutable request remains queryable"); }
            if retire { cancel.cancel(); return Ok(()); }
        }
    }))
}

/// Shared by deterministic CLI and the detached native control driver.
pub async fn request_live(command: UpdateCommand, data_dir: &Path) -> Result<serde_json::Value> {
    let endpoint: Endpoint = serde_json::from_slice(&private_read(&data_dir.join(ENDPOINT))?)?;
    ensure!(endpoint.address.ip().is_loopback(), "control endpoint is not local");
    let socket = tokio::net::TcpStream::connect(endpoint.address).await.context("live runtime control unreachable; connection failure does not prove retirement")?;
    let (read, mut write) = socket.into_split();
    let mut bytes = serde_json::to_vec(&Request { capability: endpoint.capability, command })?;
    ensure!((bytes.len() as u64) < LIMIT, "control request exceeds limit"); bytes.push(b'\n');
    write.write_all(&bytes).await?;
    let mut response = String::new();
    BufReader::new(read.take(LIMIT + 1)).read_line(&mut response).await?;
    ensure!((response.len() as u64) <= LIMIT && response.ends_with('\n'), "control response lost/incomplete; inspect immutable activation request, do not blindly replay");
    let response: Response = serde_json::from_str(&response)?;
    if let Some(error) = response.error { anyhow::bail!("{error}"); }
    response.result.context("control result missing")
}

fn wait_owner(data_dir: &Path) -> Result<crate::runtime_owner::RuntimeOwner> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        match crate::runtime_owner::RuntimeOwner::try_acquire(data_dir) {
            Ok(owner) => return Ok(owner),
            Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) && std::time::Instant::now() < deadline =>
                std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(error) => return Err(error).context("old runtime has not proved retirement; selector not changed"),
        }
    }
}

pub async fn complete_handoff(path: PathBuf, digest: String, data_dir: PathBuf) -> Result<()> {
    let bytes = private_read(&path)?;
    ensure!(Digest::blake3(&bytes).to_string() == digest, "immutable handoff request digest mismatch");
    let request: HandoffRequest = serde_json::from_slice(&bytes)?;
    ensure!(request.schema_version == 1, "unsupported process handoff request");
    ensure!(Digest::blake3(&std::fs::read(std::env::current_exe()?)?) == request.helper_digest,
        "helper executable differs from frozen source artifact");
    let owner = wait_owner(&data_dir)?;
    owner.verify_path(&data_dir)?;
    let store = Arc::new(crate::open_installation_store(&data_dir)?);
    ensure!(store.installation_id_for_subject(request.agent_id)? == request.installation_id, "handoff request belongs to another installation");
    let (service, agent) = service(&data_dir, Arc::clone(&store), Arc::new(HandoffParticipantRegistry::for_exclusively_owned_stopped_installation()))?;
    service.finish_committed_handoff(agent)?;
    let selected_store = Arc::new(crate::open_installation_store(&data_dir)?);
    let selected = selected_store.activation_selector(agent)?.context("selector missing after retirement")?;
    let (_, expected) = command_activation(&request.command).context("invalid handoff continuation command")?;
    let result = if selected.selector_revision == expected {
        ensure!(selected == request.source_selector, "source generation/epoch/route changed after request freeze");
        ensure!(shenko_update::slots::resolve_live_slot(&data_dir)?.unwrap_or_else(|| data_dir.clone()) == request.source_slot,
            "source slot changed before handoff");
        match shenko_update::cli::execute(request.command.clone(), &service, agent).await {
            Ok(result) => result,
            Err(error) => {
                // A reported error is not proof the candidate CAS failed.
                // Complete any committed publication before choosing whom
                // to restart, and never restore an old selector/epoch.
                service.finish_committed_handoff(agent)?;
                let current_store = crate::open_installation_store(&data_dir)?;
                let current = current_store.activation_selector(agent)?.context("recovery selector missing")?;
                private_write(&data_dir.join(format!("update-handoff-{}-error-{}.json", request.request_id, EntityId::new())),
                    &serde_json::to_vec(&serde_json::json!({"request_digest":digest,"error":format!("{error:#}"),"selector":current}))?)?;
                if current == request.source_selector {
                    current_store.resume_subject_quiesce(agent, request.request_id)?;
                }
                return exec_selected(&data_dir, &current, owner);
            }
        }
    } else {
        ensure!(selected.selector_revision == expected + 1, "handoff continuation superseded; no replay");
        let selected_activation = match &selected.origin {
            shenko_types::activation::ActivationOrigin::Activation { activation_id } => *activation_id,
            _ => anyhow::bail!("handoff continuation found an unrelated genesis selector"),
        };
        let committed = selected_store.activation_record(selected_activation)?.context("selected activation record missing")?;
        let (original, _) = command_activation(&request.command).context("original activation missing")?;
        match &request.command {
            UpdateCommand::Activate { .. } => ensure!(selected_activation == original,
                "another activation won the expected revision; this request did not succeed"),
            UpdateCommand::Rollback { .. } => {
                let closure: serde_json::Value = serde_json::from_slice(&selected_store.cas.read_object(&committed.recovery_closure_ref.digest)?)?;
                ensure!(closure.get("rollback_of").and_then(|value| value.as_str()) == Some(original.to_string().as_str())
                    && committed.from_generation == request.source_selector.generation,
                    "selected activation is not this request's rollback");
            }
            _ => anyhow::bail!("invalid handoff restart command"),
        }
        serde_json::json!({"selector": selected, "recovered_existing_selection": true})
    };
    let selected_store = crate::open_installation_store(&data_dir)?;
    let selected = selected_store.activation_selector(agent)?.context("selected successor missing")?;
    let result_path = data_dir.join(format!("update-handoff-{}-selected-{}.json", request.request_id, EntityId::new()));
    private_write(&result_path, &serde_json::to_vec(&serde_json::json!({"request_digest":digest,
        "result":result,"state":"selector_committed_successor_exec_pending","launch_succeeded":false}))?)?;
    exec_selected(&data_dir, &selected, owner)
}

fn exec_selected(data_dir: &Path, selector: &shenko_types::activation::ActivationSelector, owner: crate::runtime_owner::RuntimeOwner) -> Result<()> {
    let store = crate::open_installation_store(data_dir)?;
    let route: shenko_types::activation::RouteAuthority = serde_json::from_slice(&store.cas.read_object(&selector.route_authority_ref.digest)?)?;
    ensure!(route.generation == selector.generation && route.selector_revision == selector.selector_revision && route.active_epoch == selector.active_epoch, "selected route mismatch");
    ensure!(Digest::blake3(&std::fs::read(&route.artifact_path)?) == route.artifact_digest, "selected executable digest changed");
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))] {
        use std::os::unix::process::CommandExt;
        let fd = owner.prepare_exec_transfer()?;
        let error = std::process::Command::new(&route.artifact_path).arg("--data-dir").arg(data_dir).arg("start")
            .env("SHENKO_INHERITED_RUNTIME_OWNER", fd.to_string())
            .env("SHENKO_SELECTED_LAUNCH_REVISION", selector.selector_revision.to_string()).exec();
        return Err(error).context("selected successor exec failed; committed selector remains recoverable");
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))] {
        let _ = owner;
        anyhow::bail!("this runtime-owner provider cannot transfer ownership across exec");
    }
}

pub fn acquire_launch_owner(data_dir: &Path) -> Result<crate::runtime_owner::RuntimeOwner> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    let owner = match std::env::var("SHENKO_INHERITED_RUNTIME_OWNER").ok() {
        Some(fd) => crate::runtime_owner::RuntimeOwner::adopt_inherited(data_dir, fd.parse()?)?,
        None => crate::runtime_owner::RuntimeOwner::try_acquire(data_dir)?,
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    let owner = crate::runtime_owner::RuntimeOwner::try_acquire(data_dir)?;
    let slot = shenko_update::slots::resolve_live_slot(data_dir)?.unwrap_or_else(|| data_dir.to_owned());
    if slot.join(shenko_update::SLOT_DB_FILENAME).is_file() {
        let store = Arc::new(crate::open_installation_store(data_dir)?);
        if let Some(agent) = store.get_existing_agent_id()? {
            let (recovery, _) = service(data_dir, store, Arc::new(HandoffParticipantRegistry::for_exclusively_owned_stopped_installation()))?;
            recovery.finish_committed_handoff(agent)?;
            // We hold the RuntimeOwner here, so no other local writer and no
            // still-waiting helper exists. A hand-over that died before its
            // selector CAS left a paused subject nobody on the old side could
            // resume; the same controller generation takes it back now, and a
            // committed selection is fenced instead of revived.
            match recovery.reconcile_interrupted_handoff(agent)? {
                reconciliation if reconciliation.resumed => tracing::warn!(
                    activation_id = ?reconciliation.activation_id,
                    generation = ?reconciliation.generation,
                    "interrupted process hand-over resumed by the same controller generation"
                ),
                reconciliation => tracing::debug!(
                    state = ?reconciliation.state,
                    reason = %reconciliation.reason,
                    "no interrupted hand-over boundary to reconcile"
                ),
            }
        }
    }
    if let Ok(expected) = std::env::var("SHENKO_SELECTED_LAUNCH_REVISION") {
        let store = crate::open_installation_store(data_dir)?;
        let agent = store.get_existing_agent_id()?.context("selected subject missing")?;
        let selected = store.activation_selector(agent)?.context("selected authority missing")?;
        ensure!(selected.selector_revision == expected.parse::<u64>()?, "selected launch superseded");
        let route: shenko_types::activation::RouteAuthority = serde_json::from_slice(&store.cas.read_object(&selected.route_authority_ref.digest)?)?;
        ensure!(Digest::blake3(&std::fs::read(std::env::current_exe()?)?) == route.artifact_digest, "running executable differs from selected artifact");
    }
    Ok(owner)
}

pub async fn run_cli(command: UpdateCommand, data_dir: PathBuf) -> Result<()> {
    if matches!(command, UpdateCommand::Smoke { .. }) { return shenko_update::cli::run(command, data_dir).await; }
    match crate::runtime_owner::RuntimeOwner::try_acquire(&data_dir) {
        Ok(owner) => {
            let store = Arc::new(crate::open_installation_store(&data_dir)?);
            let (service, agent) = service(&data_dir, store, Arc::new(HandoffParticipantRegistry::for_exclusively_owned_stopped_installation()))?;
            let handoff = command_activation(&command).is_some();
            let result = shenko_update::cli::execute(command, &service, agent).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            if handoff {
                let store = crate::open_installation_store(&data_dir)?;
                let selected = store.activation_selector(agent)?.context("offline selection missing")?;
                return exec_selected(&data_dir, &selected, owner);
            }
            Ok(())
        }
        Err(error) if error.downcast_ref::<std::io::Error>().is_some_and(|error| error.kind() == std::io::ErrorKind::WouldBlock) => {
            let result = request_live(command, &data_dir).await?;
            println!("{}", serde_json::to_string_pretty(&result)?);
            Ok(())
        }
        Err(error) => Err(error).context("cannot establish installation control ownership"),
    }
}
