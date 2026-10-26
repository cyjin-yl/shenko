//! Model-independent cognition/package replication over the node's real SSH path.
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_node::{Node, NodeRole, SyncConfirmation, SyncPeer, UnitExport, UnitRequest};
use shenko_store::{
    cognition::{
        CognitionAck, CognitionExport, CognitionImportOptions, CognitionReceipt,
        CognitionSnapshotOptions,
    },
    store::Store,
};
use shenko_types::{ids::EntityId, object::Digest};
use std::{
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const MAGIC: &[u8] = b"shenko-node-sync/1\n";
const MAX_HEADER: u64 = 64 * 1024 * 1024;

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    /// Local durable facts, unacknowledged changes and verified peer watermarks.
    Status,
    /// Query this installation's real Node identity, without starting a model.
    Identity,
    /// Discover a credential-accessible peer and persist its actual Node ID.
    Enroll {
        host: String,
        #[arg(long)]
        remote_data_dir: PathBuf,
        #[arg(long, default_value = "shenko")]
        remote_binary: String,
        #[arg(long)]
        workspace: Vec<PathBuf>,
        #[arg(long)]
        remote_workspace_root: Option<PathBuf>,
    },
    /// Stream a verified recovery closure and installation packages to stdout.
    Export {
        #[arg(long)]
        peer_id: Option<EntityId>,
        #[arg(long)]
        workspace: Vec<PathBuf>,
    },
    /// Verify/import stdin and emit a receipt; does not dispatch imported work.
    Receive {
        #[arg(long)]
        workspace_root: Option<PathBuf>,
    },
    /// Record the receiver's exact verified receipt from stdin.
    Ack,
    /// Reconcile configured peers once; an interrupted transfer remains debt.
    Push,
    /// List captured raw recovery sources; no provider is activated.
    Sources,
    /// Extract a retained data database at a new path without replaying it.
    RecoverSource {
        source_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: u32,
    source_node_id: EntityId,
    units: Vec<UnitExport>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    receiver_node_id: EntityId,
    units_fingerprint: Digest,
    cognition_ack: CognitionAck,
    unit_statuses: Vec<Value>,
}

pub fn open_node(data_dir: &Path, store: &Store) -> Result<Node> {
    let mut node = Node::open(&data_dir.join("node"))?;
    let identity = if let Some(agent) = store.get_existing_agent_id()? {
        let genesis = store
            .get_subject_genesis(agent)?
            .context("subject identity has no actual genesis")?;
        let (head, _) = store.load_subject(agent)?;
        Some(shenko_node::Identity {
            agent_id: agent,
            branch_id: EntityId(head.branch_id.0),
            origin_proof: Digest::blake3(&serde_json::to_vec(&genesis)?),
        })
    } else {
        None
    };
    if node.local_node_id()?.is_none() {
        node.handle(shenko_node::Request::Initialize {
            node_id: EntityId::new(),
            identity: identity.clone(),
        })?;
    } else if let Some(identity) = identity {
        match node.local_identity()? {
            Some(old) => ensure!(
                old == identity,
                "Node/subject branch differs; reconcile retained partition facts before adoption"
            ),
            None => {
                let id = node.local_node_id()?.context("node not initialized")?;
                node.handle(shenko_node::Request::Join {
                    operation_id: EntityId::new(),
                    expected_node: id,
                    identity,
                    manifest: shenko_node::running_manifest()?,
                })?;
            }
        }
    }
    Ok(node)
}

fn options(data_dir: &Path, workspace_roots: Vec<PathBuf>) -> CognitionSnapshotOptions {
    CognitionSnapshotOptions {
        memory_root: Some(data_dir.join("memory")),
        workspace_roots,
        include_git_history: true,
    }
}

fn stage_declared_recovery_sources(data_dir: &Path, store: &Store, node: &Node) -> Result<()> {
    // This component-owned database contains raw accepted events, observations,
    // pending input and gaps. Its account number is data, not a provider grant;
    // no client URL, environment value or credential store is discovered here.
    let ingress = data_dir.join("napcat-ingress.sqlite");
    if ingress.try_exists()? {
        let node_id = node.local_node_id()?.context("node not initialized")?;
        store.stage_sqlite_recovery_source(
            &format!("napcat-ingress:{node_id}"),
            "napcat_ingress",
            &ingress,
            0x534e4947,
            &data_dir.join("cognition-staging"),
        )?;
    }
    Ok(())
}

fn header(node: &Node) -> Result<(Vec<u8>, Digest)> {
    let bytes = serde_json::to_vec(&Header {
        schema: 1,
        source_node_id: node.local_node_id()?.context("node not initialized")?,
        units: node.export_installation_units()?,
    })?;
    ensure!(
        bytes.len() as u64 <= MAX_HEADER,
        "installation metadata exceeds 64MiB frame"
    );
    let fingerprint = Digest::blake3(&bytes);
    Ok((bytes, fingerprint))
}

fn write_transfer<W: Write>(
    mut writer: W,
    header: &[u8],
    export: &CognitionExport,
) -> Result<CognitionReceipt> {
    writer.write_all(MAGIC)?;
    writer.write_all(header)?;
    writer.write_all(b"\n")?;
    let receipt = export.write_archive(&mut writer)?;
    writer.flush()?;
    Ok(receipt)
}

fn receive<R: Read>(
    reader: R,
    data_dir: &Path,
    store: &Store,
    workspace_root: Option<PathBuf>,
) -> Result<Receipt> {
    let mut reader = BufReader::new(reader);
    let mut magic = vec![0; MAGIC.len()];
    reader.read_exact(&mut magic)?;
    ensure!(magic == MAGIC, "unknown node replication framing");
    let mut bytes = Vec::new();
    (&mut reader)
        .take(MAX_HEADER + 1)
        .read_until(b'\n', &mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_HEADER + 1 && bytes.last() == Some(&b'\n'),
        "incomplete or oversized node metadata"
    );
    bytes.pop();
    let fingerprint = Digest::blake3(&bytes);
    let metadata: Header = serde_json::from_slice(&bytes)?;
    ensure!(metadata.schema == 1, "unsupported node replication schema");
    // Prepare this installation's own native/WASI home before any imported
    // memory appears; never pretend the source's mount paths are local ones.
    let bootstrap = shenko_seed::Bootstrap::new(shenko_seed::SeedConfig {
        data_dir: data_dir.to_path_buf(),
        ..Default::default()
    });
    drop(bootstrap.prepare(store.ledger.ledger_id())?);
    // Cognition validates its entire declared closure before publication. A
    // package failure afterwards leaves local cognition durable but no ACK.
    let outcome = store.import_cognition_archive(
        reader,
        &CognitionImportOptions {
            memory_root: Some(data_dir.join("memory")),
            workspace_root,
            peer_id: Some(metadata.source_node_id),
        },
    )?;
    let mut node = open_node(data_dir, store)?;
    let mut unit_statuses = Vec::with_capacity(metadata.units.len());
    for export in metadata.units {
        let enabled = export.enabled;
        unit_statuses.push(
            node.handle_unit_request(UnitRequest::Import {
                export,
                package_root: data_dir
                    .join("installations")
                    .to_str()
                    .context("installation package root is not UTF-8")?
                    .to_owned(),
                enable: Some(enabled),
            })?,
        );
    }
    let receiver = node.local_node_id()?.context("node not initialized")?;
    Ok(Receipt {
        receiver_node_id: receiver,
        units_fingerprint: fingerprint,
        cognition_ack: outcome.ack(receiver),
        unit_statuses,
    })
}

// Quote remote argv as literal POSIX-shell arguments, not executable fragments.
// SSH host configuration/credentials remain node-local; no copied grants.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn remote_command(peer: &SyncPeer, command: &str) -> String {
    let mut argv = vec![
        peer.remote_binary.clone(),
        "--data-dir".into(),
        peer.remote_data_dir.to_string_lossy().into_owned(),
        "sync".into(),
        command.into(),
    ];
    if command == "receive" {
        if let Some(root) = &peer.remote_workspace_root {
            argv.extend([
                "--workspace-root".into(),
                root.to_string_lossy().into_owned(),
            ]);
        }
    }
    argv.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
}

fn drain<R: Read>(mut reader: R, limit: usize) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let keep = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok((bytes, truncated))
}

fn stop_transport(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // SAFETY: this process group was created for this owned SSH invocation;
        // no existing ControlMaster or unrelated process joins it.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Drain both outputs while streaming stdin. Cancellation requests termination
/// of the owned local transport, never proof of a remote application outcome.
fn exchange<T: Send, F>(
    peer: &SyncPeer,
    command: &str,
    cancel: &CancellationToken,
    write: F,
) -> Result<(T, Vec<u8>)>
where
    F: FnOnce(&mut BufWriter<std::process::ChildStdin>) -> Result<T> + Send,
{
    let mut command_process = Command::new("ssh");
    command_process
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=8",
            "--",
        ])
        .arg(&peer.ssh_target)
        .arg(remote_command(peer, command))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command_process.process_group(0);
    }
    let mut child = command_process.spawn()?;
    let input = child.stdin.take().context("SSH stdin unavailable")?;
    let output = child.stdout.take().context("SSH stdout unavailable")?;
    let error = child.stderr.take().context("SSH stderr unavailable")?;
    std::thread::scope(|scope| -> Result<(T, Vec<u8>)> {
        let writer = scope.spawn(move || {
            let mut input = BufWriter::new(input);
            let result = write(&mut input);
            if result.is_ok() {
                input.flush()?;
            }
            result
        });
        let stdout = scope.spawn(move || drain(output, 1024 * 1024));
        let stderr = scope.spawn(move || drain(error, 64 * 1024));
        let start = Instant::now();
        let mut exited = None;
        let status = loop {
            if exited.is_none() {
                match child.try_wait() {
                    Ok(Some(status)) => exited = Some(status),
                    Err(error) => {
                        stop_transport(&mut child);
                        break Err(error.into());
                    }
                    Ok(None) => {}
                }
            }
            if writer.is_finished() && stdout.is_finished() && stderr.is_finished() {
                if let Some(status) = exited {
                    break Ok(status);
                }
            }
            if cancel.is_cancelled() || start.elapsed() >= Duration::from_secs(120) {
                stop_transport(&mut child);
                break Err(anyhow::anyhow!(
                    "SSH replication interrupted; remote receipt unknown, debt retained"
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let written = writer
            .join()
            .map_err(|_| anyhow::anyhow!("replication writer panicked"))?;
        let (stdout, truncated) = stdout
            .join()
            .map_err(|_| anyhow::anyhow!("replication stdout reader panicked"))??;
        let (stderr, _) = stderr
            .join()
            .map_err(|_| anyhow::anyhow!("replication stderr reader panicked"))??;
        let status = status?;
        ensure!(
            status.success(),
            "SSH replication unconfirmed ({}): {}",
            status,
            String::from_utf8_lossy(&stderr)
        );
        ensure!(
            !truncated,
            "replication receipt exceeded 1MiB; not acknowledged"
        );
        Ok((written?, stdout))
    })
}

fn confirm(
    store: &Store,
    node: &mut Node,
    peer: &SyncPeer,
    receipt: &Receipt,
    sent: &CognitionReceipt,
    units: &Digest,
) -> Result<()> {
    ensure!(
        receipt.receiver_node_id == peer.node_id && receipt.cognition_ack.peer_id == peer.node_id,
        "replication response is from a different Node"
    );
    ensure!(
        receipt.units_fingerprint == *units
            && receipt.cognition_ack.fingerprint == sent.fingerprint
            && receipt.cognition_ack.high_water == sent.high_water,
        "replication receipt does not match the sent closure"
    );
    ensure!(
        sent.complete && receipt.cognition_ack.verified && receipt.cognition_ack.missing.is_empty(),
        "receiver retained an incomplete or conflicting recovery candidate; not acknowledged"
    );
    store.record_cognition_ack(&receipt.cognition_ack)?;
    node.confirm_sync_peer(
        peer,
        &SyncConfirmation {
            cognition_fingerprint: sent.fingerprint.clone(),
            units_fingerprint: units.clone(),
            confirmed_at: chrono::Utc::now(),
        },
    )?;
    Ok(())
}

fn push_all(data_dir: &Path, store: &Store, cancel: &CancellationToken) -> Result<Value> {
    let mut node = open_node(data_dir, store)?;
    let peers = node.sync_peers()?;
    if peers.is_empty() {
        return Ok(
            json!({"no_peers_unprotected":true,"peers":[],"global_strong_consistency":false,"zero_loss":false}),
        );
    }
    stage_declared_recovery_sources(data_dir, store, &node)?;
    let (header, units) = header(&node)?;
    let mut results = Vec::with_capacity(peers.len());
    for state in peers {
        if cancel.is_cancelled() {
            break;
        }
        let peer = &state.peer;
        let result: Result<Value> = (|| {
            let export = store.export_cognition(&options(data_dir, peer.workspaces.clone()))?;
            if state.confirmation.as_ref().is_some_and(|c| {
                c.cognition_fingerprint == export.manifest.fingerprint
                    && c.units_fingerprint == units
            }) {
                return Ok(
                    json!({"peer_id":peer.node_id,"outcome":"already_confirmed","confirmation":state.confirmation}),
                );
            }
            let (sent, response) = exchange(peer, "receive", cancel, |writer| {
                let sent = write_transfer(writer, &header, &export)?;
                store.note_cognition_export(peer.node_id, &sent)?;
                Ok(sent)
            })?;
            let received: Receipt =
                serde_json::from_slice(&response).context("decode peer verification receipt")?;
            confirm(store, &mut node, peer, &received, &sent, &units)?;
            Ok(json!({"peer_id":peer.node_id,"outcome":"verified","receipt":received}))
        })();
        match result {
            Ok(value) => results.push(value),
            Err(error) => {
                node.fail_sync_peer(peer, format!("{error:#}"))?;
                results.push(json!({"peer_id":peer.node_id,"outcome":"unconfirmed","error":format!("{error:#}"),"replication_debt_retained":true}));
            }
        }
    }
    Ok(
        json!({"no_peers_unprotected":results.is_empty(),"peers":results,"global_strong_consistency":false,"zero_loss":false}),
    )
}

pub async fn run(command: SyncCommand, data_dir: PathBuf) -> Result<()> {
    tokio::fs::create_dir_all(&data_dir).await?;
    let store = Arc::new(Store::open(
        data_dir.join("shenko.db"),
        data_dir.join("cas"),
    )?);
    let mut node = open_node(&data_dir, &store)?;
    match command {
        SyncCommand::Identity => println!(
            "{}",
            json!({"node_id":node.local_node_id()?,"subject_identity":node.local_identity()?,"roles":node.role_status()?})
        ),
        SyncCommand::Status => {
            let states = node.sync_peers()?;
            if !states.is_empty() {
                stage_declared_recovery_sources(&data_dir, &store, &node)?;
            }
            let (_, units) = header(&node)?;
            let mut peers = Vec::new();
            for state in states {
                let cognition = store.cognition_peer_status(
                    state.peer.node_id,
                    &options(&data_dir, state.peer.workspaces.clone()),
                )?;
                let units_confirmed = state
                    .confirmation
                    .as_ref()
                    .is_some_and(|c| c.units_fingerprint == units);
                peers.push(json!({"route":state,"cognition":cognition,"installation_packages_confirmed":units_confirmed,"current_peer_liveness":"not_proven_by_historical_ack"}));
            }
            println!(
                "{}",
                json!({"node_id":node.local_node_id()?,"local_durable":true,"no_peers_unprotected":peers.is_empty(),"peers":peers,"workspace_default":"not_replicated","local_credential_stores_and_grants":"not_discovered_or_copied","global_strong_consistency":false,"zero_loss":false})
            );
        }
        SyncCommand::Enroll {
            host,
            remote_data_dir,
            remote_binary,
            workspace,
            remote_workspace_root,
        } => {
            let mut peer = SyncPeer {
                node_id: EntityId::new(),
                ssh_target: host,
                remote_binary,
                remote_data_dir,
                workspaces: workspace,
                remote_workspace_root,
            };
            let (_, bytes) = exchange(&peer, "identity", &CancellationToken::new(), |_| Ok(()))?;
            let value: Value = serde_json::from_slice(&bytes)?;
            peer.node_id = serde_json::from_value(
                value
                    .get("node_id")
                    .context("peer omitted actual Node ID")?
                    .clone(),
            )?;
            ensure!(
                Some(peer.node_id) != node.local_node_id()?,
                "peer is this same Node installation"
            );
            node.register_sync_peer(&peer)?;
            println!(
                "{}",
                json!({"peer":peer,"replicated":false,"next":"shenko sync push; start also reconciles configured peers"})
            );
        }
        SyncCommand::Export { peer_id, workspace } => {
            stage_declared_recovery_sources(&data_dir, &store, &node)?;
            let (header, _) = header(&node)?;
            let export = store.export_cognition(&options(&data_dir, workspace))?;
            let receipt = write_transfer(std::io::stdout().lock(), &header, &export)?;
            if let Some(peer) = peer_id {
                store.note_cognition_export(peer, &receipt)?;
            }
        }
        SyncCommand::Receive { workspace_root } => {
            drop(node);
            let receipt = receive(std::io::stdin().lock(), &data_dir, &store, workspace_root)?;
            println!("{}", serde_json::to_string(&receipt)?);
        }
        SyncCommand::Ack => {
            let receipt: Receipt =
                serde_json::from_reader(std::io::stdin().lock().take(1024 * 1024 + 1))?;
            ensure!(
                receipt.receiver_node_id == receipt.cognition_ack.peer_id,
                "ACK Node identity mismatch"
            );
            ensure!(
                receipt.cognition_ack.verified && receipt.cognition_ack.missing.is_empty(),
                "receiver did not verify the complete closure; debt retained"
            );
            store.record_cognition_ack(&receipt.cognition_ack)?;
            let status = store
                .cognition_peer_status(receipt.receiver_node_id, &options(&data_dir, Vec::new()))?;
            ensure!(
                status.acked_fingerprint.as_ref() == Some(&receipt.cognition_ack.fingerprint)
                    && status.acked_high_water == Some(receipt.cognition_ack.high_water),
                "ACK did not match a previously exported closure; debt retained"
            );
            let (_, units) = header(&node)?;
            if units == receipt.units_fingerprint {
                if let Some(state) = node
                    .sync_peers()?
                    .into_iter()
                    .find(|p| p.peer.node_id == receipt.receiver_node_id)
                {
                    node.confirm_sync_peer(
                        &state.peer,
                        &SyncConfirmation {
                            cognition_fingerprint: receipt.cognition_ack.fingerprint.clone(),
                            units_fingerprint: units,
                            confirmed_at: chrono::Utc::now(),
                        },
                    )?;
                }
            }
            println!("{}", json!({"ack":receipt.cognition_ack}));
        }
        SyncCommand::Sources => {
            stage_declared_recovery_sources(&data_dir, &store, &node)?;
            println!(
                "{}",
                json!({"sources":store.recovery_sources()?,"provider_activated":false})
            );
        }
        SyncCommand::RecoverSource { source_id, output } => {
            let source = store.restore_sqlite_recovery_source(&source_id, &output)?;
            println!(
                "{}",
                json!({"source":source,"output":output,"verified":true,"provider_activated":false})
            );
        }
        SyncCommand::Push => {
            drop(node);
            let value = tokio::task::spawn_blocking(move || {
                push_all(&data_dir, &store, &CancellationToken::new())
            })
            .await??;
            println!("{value}");
            if value["peers"]
                .as_array()
                .is_some_and(|p| p.iter().any(|v| v["outcome"] == "unconfirmed"))
            {
                bail!("some peers remain unconfirmed; local work and replication debt retained");
            }
        }
    }
    Ok(())
}

pub fn spawn_runtime(
    store: Arc<Store>,
    data_dir: PathBuf,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! { _ = cancel.cancelled() => break, _ = tick.tick() => {} }
            let data_dir = data_dir.clone();
            let store = Arc::clone(&store);
            let cancel = cancel.clone();
            match tokio::task::spawn_blocking(move || push_all(&data_dir, &store, &cancel)).await {
                Ok(Ok(status)) => {
                    if status["peers"]
                        .as_array()
                        .is_some_and(|p| p.iter().any(|v| v["outcome"] == "unconfirmed"))
                    {
                        tracing::warn!(status=%status,"replication debt retained; offline subject remains available");
                    }
                }
                Ok(Err(error)) => {
                    tracing::error!(error=%error,"replication snapshot unavailable; no recoverability claim")
                }
                Err(error) => {
                    tracing::error!(error=%error,"replication worker panicked; no ACK fabricated");
                    break;
                }
            }
        }
    })
}

pub fn observe(data_dir: &Path, store: &Store, runtime: EntityId, primary: bool) -> Result<()> {
    let mut node = open_node(data_dir, store)?;
    let mut roles = vec![NodeRole::Executor, NodeRole::Recovery, NodeRole::Replica];
    if primary {
        roles.push(NodeRole::Primary);
    }
    node.observe_runtime_roles(runtime, roles, store.get_existing_agent_id()?,
        if primary { "actual subject task and native recovery/replication consumers started; provider readiness remains separate" }
        else { "native deterministic recovery and replication consumers started; no subject inference loop or provider readiness claimed" }.into())?;
    Ok(())
}

pub fn node_command(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let store = Store::open(data_dir.join("shenko.db"), data_dir.join("cas"))?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(shenko_node::MAX_NODE_REQUEST as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= shenko_node::MAX_NODE_REQUEST,
        "Node request exceeds 16MiB"
    );
    let request = serde_json::from_slice(&bytes)?;
    println!("{}", open_node(data_dir, &store)?.handle(request)?);
    Ok(())
}
