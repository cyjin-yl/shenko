//! Model-independent cognition/package replication over the node's real SSH path.
use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use shenko_node::{
    AdoptRequest, FencingToken, MemberChangeRequest, MemberChangeRound, Node,
    NodeCapabilityProfile, NodeRole, PeerExperience, SyncConfirmation, SyncPeer, UnitExport,
    UnitRequest, VoteOutcome, VoteResponse,
};
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

const MAGIC: &[u8] = b"shenko-node-sync/2\n";
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
    /// Verified election, membership and authority facts of this Node's domain.
    Election {
        #[command(subcommand)]
        command: ElectionCommand,
    },
    /// Committed member configuration and membership changes by quorum.
    Members {
        #[command(subcommand)]
        command: MembersCommand,
    },
    /// Adopt a verified existing self; the local experience is retained.
    Adopt {
        #[arg(long)]
        identity_file: PathBuf,
        /// Node ID whose authenticated channel vouched for the identity.
        #[arg(long)]
        trusted_node: EntityId,
        /// How the trusted identity was actually verified.
        #[arg(long)]
        provenance: String,
        #[arg(long)]
        reason: String,
    },
    /// Merge a configured peer's verified experience as parallel facts.
    Reconcile {
        #[arg(long)]
        peer: EntityId,
    },
    /// List captured raw recovery sources; no provider is activated.
    Sources,
    /// Extract a retained data database at a new path without replaying it.
    RecoverSource {
        source_id: String,
        #[arg(long)]
        output: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub enum ElectionCommand {
    /// Print the committed election, membership and authority facts.
    Status,
    /// Campaign for primary: solicit member votes, commit the claim on quorum.
    Campaign,
}

#[derive(Debug, Subcommand)]
pub enum MembersCommand {
    /// Show the committed member configuration and its quorum requirement.
    Show,
    /// Propose adding members; joint then final configuration on quorum.
    Add { node_ids: Vec<EntityId> },
    /// Propose removing members; joint then final configuration on quorum.
    Remove { node_ids: Vec<EntityId> },
    /// Push the committed configuration and control log to one peer.
    Push { #[arg(long)] peer: EntityId },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    schema: u32,
    source_node_id: EntityId,
    units: Vec<UnitExport>,
    /// Self-declared capability facts of the sending Node. Facts, not
    /// liveness, election authority or model reachability.
    capabilities: NodeCapabilityProfile,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    receiver_node_id: EntityId,
    units_fingerprint: Digest,
    cognition_ack: CognitionAck,
    unit_statuses: Vec<Value>,
    /// Receiver's self-declared capability facts at receipt time.
    capabilities: NodeCapabilityProfile,
}

pub fn open_node(data_dir: &Path, store: &Store) -> Result<Node> {
    let identity = bound_subject_identity(store)?;
    open_node_inner(data_dir, identity, true)
}

/// The subject identity this installation's durable facts currently bind, if
/// exactly one local subject exists. The binding is projected from committed
/// genesis/head facts; nothing writes it directly.
fn bound_subject_identity(store: &Store) -> Result<Option<shenko_node::Identity>> {
    Ok(store
        .installation_binding()?
        .subject
        .map(|subject| shenko_node::Identity {
            agent_id: subject.agent_id,
            branch_id: subject.branch_id,
            origin_proof: subject.origin_proof,
        }))
}

/// Node opener for the adoption command: adoption is the operation that
/// reconciles a node/subject divergence, so it must not deadlock on the
/// binding equality check that every other path enforces. The command's own
/// coherence check — `ensure_adopt_is_coherent` — runs before any adoption
/// is recorded.
fn open_node_for_adoption(data_dir: &Path, store: &Store) -> Result<Node> {
    let identity = bound_subject_identity(store)?;
    open_node_inner(data_dir, identity, false)
}

fn open_node_inner(
    data_dir: &Path,
    identity: Option<shenko_node::Identity>,
    enforce_subject_binding: bool,
) -> Result<Node> {
    let mut node = Node::open(&data_dir.join("node"))?;
    if node.local_node_id()?.is_none() {
        node.handle(shenko_node::Request::Initialize {
            node_id: EntityId::new(),
            identity: identity.clone(),
        })?;
    } else if let Some(identity) = identity {
        match node.local_identity()? {
            Some(old) => ensure!(
                !enforce_subject_binding || old == identity,
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

/// The installation-level half of `adopt_identity` (SPEC §5.4): the node may
/// adopt the trusted identity only when the store's subject binding stays
/// coherent with it — either no local subject is bound yet (the node carries
/// the adopted self until the subject's retained facts arrive; the binding
/// is derived from durable subject facts, never written speculatively), or
/// the trusted identity IS the bound self, which is also the path that
/// re-converges an already diverged node/binding pair. Any other adoption
/// would report success while leaving `open_node`'s binding check broken on
/// every later start, so it is refused before the node is touched.
fn ensure_adopt_is_coherent(
    store: &Store,
    trusted: &shenko_node::Identity,
    operation: &str,
) -> Result<()> {
    if let Some(bound) = bound_subject_identity(store)? {
        ensure!(
            bound == *trusted,
            "{operation} refused: this installation's subject is bound to agent {} branch {}, \
             a different self than the trusted identity; the adoption cannot transition the \
             binding without the adopted subject's own durable genesis and retained facts, \
             and succeeding anyway would leave the node diverged from the installation. \
             Reconcile the adopted subject's facts first, or adopt the bound identity to \
             re-converge.",
            bound.agent_id,
            bound.branch_id
        );
    }
    Ok(())
}

fn options(data_dir: &Path, workspace_roots: Vec<PathBuf>) -> CognitionSnapshotOptions {
    CognitionSnapshotOptions {
        memory_root: Some(data_dir.join("memory")),
        workspace_roots,
        include_git_history: true,
        // Peer exports do not carry the complete local boundary database.
        local_boundary_preserved: false,
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

fn header(node: &mut Node) -> Result<(Vec<u8>, Digest)> {
    let bytes = serde_json::to_vec(&Header {
        schema: 2,
        source_node_id: node.local_node_id()?.context("node not initialized")?,
        units: node.export_installation_units()?,
        capabilities: node.capability_profile()?,
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
    ensure!(
        metadata.schema == 2,
        "unsupported node replication schema"
    );
    // Bind the advertised capability facts to the authenticated source Node
    // before importing; the durable advertisement is fact even when the
    // recovery closure itself is later rejected.
    let mut node = open_node(data_dir, store)?;
    ensure!(
        metadata.capabilities.node_id == metadata.source_node_id,
        "capability advertisement does not name the transfer source"
    );
    node.record_peer_capability(
        &metadata.capabilities,
        "received node-sync transfer header over authenticated channel",
    )?;
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
        capabilities: node.capability_profile()?,
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

/// The peer's raw node-request entry point: one JSON Request on stdin, one
/// JSON response on stdout. The remote side resolves its own credentials;
/// nothing grant-like crosses this boundary.
fn node_remote_command(peer: &SyncPeer) -> String {
    [
        peer.remote_binary.clone(),
        "--data-dir".into(),
        peer.remote_data_dir.to_string_lossy().into_owned(),
        "node".into(),
    ]
    .iter()
    .map(|s| quote(s))
    .collect::<Vec<_>>()
    .join(" ")
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
    exchange_remote(peer, &remote_command(peer, command), cancel, write)
}

fn exchange_remote<T: Send, F>(
    peer: &SyncPeer,
    remote: &str,
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
        .arg(remote)
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

/// One authenticated node-request round trip over the peer's real SSH path.
/// The response is the peer's durable fact, not a liveness claim.
fn rpc(
    peer: &SyncPeer,
    request: &shenko_node::Request,
    cancel: &CancellationToken,
) -> Result<Value> {
    let bytes = serde_json::to_vec(request)?;
    let ((), response) = exchange_remote(peer, &node_remote_command(peer), cancel, |writer| {
        writer.write_all(&bytes)?;
        Ok(())
    })?;
    serde_json::from_slice(&response).context("decode peer node response")
}

fn peer_route(node: &Node, voter: EntityId) -> Result<Option<SyncPeer>> {
    Ok(node
        .sync_peers()?
        .into_iter()
        .find(|state| state.peer.node_id == voter)
        .map(|state| state.peer))
}

/// Ask every voter of a round for its vote over its authenticated route and
/// record each verified response. Unreachable or refusing members are
/// reported; they are never counted.
fn solicit_member_votes(
    node: &mut Node,
    round: &MemberChangeRound,
    cancel: &CancellationToken,
) -> Result<Vec<VoteOutcome>> {
    let mut outcomes = Vec::with_capacity(round.members.len());
    for voter in &round.members {
        let Some(peer) = peer_route(node, *voter)? else {
            outcomes.push(VoteOutcome {
                voter: *voter,
                outcome: "no_route".into(),
                reason: "no registered sync route for this member".into(),
            });
            continue;
        };
        let result: Result<VoteOutcome> = (|| {
            let value = rpc(
                &peer,
                &shenko_node::Request::Vote {
                    request: round.request.clone(),
                },
                cancel,
            )?;
            let response: VoteResponse =
                serde_json::from_value(value).context("decode vote response")?;
            let granted = response.vote_granted;
            let reason = response.reason.clone();
            node.record_vote_response(&round.request, &response)?;
            Ok(VoteOutcome {
                voter: *voter,
                outcome: (if granted { "granted" } else { "refused" }).into(),
                reason,
            })
        })();
        match result {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => {
                node.fail_sync_peer(&peer, format!("{error:#}"))?;
                outcomes.push(VoteOutcome {
                    voter: *voter,
                    outcome: "unreachable".into(),
                    reason: format!("{error:#}"),
                });
            }
        }
    }
    Ok(outcomes)
}

/// Ensure a committed member configuration, campaign for primary, solicit
/// votes over each configured member's authenticated route, and commit the
/// primary claim when the committed configuration's quorum is satisfied.
///
/// When no reachable quorum exists this opens an explicit availability
/// branch and returns `None`: the caller keeps its own subject available
/// without claiming to be the domain's primary (SPEC §5.4.1).
pub fn establish_primary(
    data_dir: &Path,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<Option<FencingToken>> {
    let mut node = open_node(data_dir, store)?;
    node.ensure_member_configuration()?;
    if node.is_primary()? && node.fencing_token()?.is_some() {
        // A committed claim for this node already fences its authority.
        return Ok(node.fencing_token()?);
    }
    let campaign = node.campaign()?;
    let request = campaign.request.clone();
    let term = campaign.term;
    let mut outcomes = Vec::with_capacity(campaign.members.len());
    for voter in &campaign.members {
        let Some(peer) = peer_route(&node, *voter)? else {
            outcomes.push(VoteOutcome {
                voter: *voter,
                outcome: "no_route".into(),
                reason: "no registered sync route for this member".into(),
            });
            continue;
        };
        let result: Result<VoteOutcome> = (|| {
            let value = rpc(
                &peer,
                &shenko_node::Request::Vote {
                    request: request.clone(),
                },
                cancel,
            )?;
            let response: VoteResponse =
                serde_json::from_value(value).context("decode vote response")?;
            let granted = response.vote_granted;
            let reason = response.reason.clone();
            node.record_vote_response(&request, &response)?;
            Ok(VoteOutcome {
                voter: *voter,
                outcome: (if granted { "granted" } else { "refused" }).into(),
                reason,
            })
        })();
        match result {
            Ok(outcome) => outcomes.push(outcome),
            Err(error) => {
                node.fail_sync_peer(&peer, format!("{error:#}"))?;
                outcomes.push(VoteOutcome {
                    voter: *voter,
                    outcome: "unreachable".into(),
                    reason: format!("{error:#}"),
                });
            }
        }
    }
    let outcome = node.tally(term)?;
    if outcome.elected {
        return Ok(node.fencing_token()?);
    }
    tracing::warn!(term = term, ?outcomes, "election found no reachable quorum; opening availability branch");
    node.open_availability_branch(
        EntityId::new(),
        store.get_existing_agent_id()?,
        &format!("election term {term} found no reachable quorum of the committed configuration"),
    )?;
    Ok(None)
}

fn member_change(
    data_dir: &Path,
    store: &Store,
    cancel: &CancellationToken,
    add: Vec<EntityId>,
    remove: Vec<EntityId>,
) -> Result<Value> {
    ensure!(
        !add.is_empty() || !remove.is_empty(),
        "membership change must add or remove at least one member"
    );
    let mut node = open_node(data_dir, store)?;
    node.ensure_member_configuration()?;
    let revision = node
        .member_configuration()?
        .context("no committed member configuration")?
        .revision;
    let joint = node.propose_member_change(MemberChangeRequest {
        operation_id: EntityId::new(),
        expected_revision: revision,
        add,
        remove,
        reason: "operator membership change over authenticated control path".into(),
    })?;
    let joint_votes = solicit_member_votes(&mut node, &joint, cancel)?;
    let joint_outcome = node.tally_member_change(&joint.proposal)?;
    if !joint_outcome.quorum_met {
        bail!(
            "joint membership change found no quorum; votes={joint_votes:?} reason={}",
            joint_outcome.reason
        );
    }
    let fin = node.propose_final_member_change(&joint.proposal)?;
    let final_votes = solicit_member_votes(&mut node, &fin, cancel)?;
    let final_outcome = node.tally_member_change(&fin.proposal)?;
    if !final_outcome.quorum_met {
        bail!(
            "final membership change found no quorum; votes={final_votes:?} reason={}",
            final_outcome.reason
        );
    }
    Ok(json!({
        "joint": {"votes": joint_votes, "outcome": joint_outcome},
        "final": {"votes": final_votes, "outcome": final_outcome},
        "member_configuration": node.member_configuration()?,
    }))
}

fn push_member_configuration(
    data_dir: &Path,
    store: &Store,
    cancel: &CancellationToken,
    peer_id: EntityId,
) -> Result<Value> {
    let mut node = open_node(data_dir, store)?;
    let config = node
        .member_configuration()?
        .context("no committed member configuration to push")?;
    let entries = node.control_log()?;
    let peer = peer_route(&node, peer_id)?
        .with_context(|| format!("no registered sync route for peer {peer_id}"))?;
    let adopted = rpc(
        &peer,
        &shenko_node::Request::PushConfig {
            config: config.clone(),
            entries: entries.clone(),
        },
        cancel,
    )?;
    Ok(json!({"peer": peer_id, "adopted": adopted}))
}

fn reconcile_with_peer(
    data_dir: &Path,
    store: &Store,
    cancel: &CancellationToken,
    peer_id: EntityId,
) -> Result<Value> {
    let mut node = open_node(data_dir, store)?;
    let peer = peer_route(&node, peer_id)?
        .with_context(|| format!("no registered sync route for peer {peer_id}"))?;
    // Pull the peer's verified experience over its authenticated route,
    // paging both durable journals. Nothing is merged by last-write-wins:
    // the node retains conflicts side by side for the subject to judge.
    let mut events = Vec::new();
    let mut cursor = "0".to_string();
    loop {
        let value = rpc(
            &peer,
            &shenko_node::Request::Export {
                after: cursor.clone(),
                limit: 128,
            },
            cancel,
        )?;
        let page: Vec<shenko_node::Event> = serde_json::from_value(
            value
                .get("events")
                .context("peer export omitted events")?
                .clone(),
        )?;
        let through = value
            .get("through")
            .and_then(Value::as_str)
            .context("peer export omitted cursor")?
            .to_string();
        let last = page.last().cloned();
        events.extend(page);
        if through == cursor || last.is_none() {
            break;
        }
        cursor = through;
    }
    let mut receipts = Vec::new();
    let mut cursor = "0".to_string();
    loop {
        let value = rpc(
            &peer,
            &shenko_node::Request::ExportReceipts {
                after: cursor.clone(),
                limit: 128,
            },
            cancel,
        )?;
        let rows: Vec<Value> = serde_json::from_value(
            value
                .get("receipts")
                .context("peer export omitted receipts")?
                .clone(),
        )
        .context("peer receipt page")?;
        let mut page = Vec::with_capacity(rows.len());
        for row in rows {
            page.push(
                serde_json::from_value(
                    row.get("receipt")
                        .context("peer receipt row omitted receipt")
                        .cloned()?,
                )
                .context("peer receipt row")?,
            );
        }
        let through = value
            .get("through")
            .and_then(Value::as_str)
            .context("peer export omitted cursor")?
            .to_string();
        let last = page.last().cloned();
        receipts.extend(page);
        if through == cursor || last.is_none() {
            break;
        }
        cursor = through;
    }
    let peer_identity = rpc(
        &peer,
        &shenko_node::Request::Status { operation_id: None },
        cancel,
    )?;
    let peer_branch: Option<EntityId> = peer_identity
        .pointer("/metadata/identity/branch_id")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .context("peer branch id")?;
    let experience = PeerExperience {
        peer_node_id: peer_id,
        peer_branch_id: peer_branch,
        events,
        receipts,
        cognition_fingerprint: None,
        cognition_high_water: None,
        note: "experience pulled over authenticated node route; conflicts retained as parallel facts".into(),
    };
    let record = node.reconcile_peer_experience(&experience)?;
    Ok(json!({"peer": peer_id, "record": record}))
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
    ensure!(
        receipt.capabilities.node_id == peer.node_id,
        "receipt capability advertisement names a different Node"
    );
    store.record_cognition_ack(&receipt.cognition_ack)?;
    node.record_peer_capability(
        &receipt.capabilities,
        "verified node-sync receipt bound to this recovery closure",
    )?;
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
    let (header, units) = header(&mut node)?;
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
    let store = Arc::new(crate::open_installation_store(&data_dir)?);
    // Adoption is the divergence-reconciliation entry: it opens the node
    // without the binding gate (a diverged installation must still be able
    // to reach it) and refuses before any write when the adoption would
    // leave the node diverged from the subject binding.
    let mut node = if matches!(command, SyncCommand::Adopt { .. }) {
        open_node_for_adoption(&data_dir, &store)?
    } else {
        open_node(&data_dir, &store)?
    };
    match command {
        SyncCommand::Identity => println!(
            "{}",
            json!({"node_id":node.local_node_id()?,"subject_identity":node.local_identity()?,"roles":node.role_status()?,"capabilities":node.capability_profile()?,"coordination":node.coordination_candidates()?})
        ),
        SyncCommand::Status => {
            let states = node.sync_peers()?;
            if !states.is_empty() {
                stage_declared_recovery_sources(&data_dir, &store, &node)?;
            }
            let (_, units) = header(&mut node)?;
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
                peers.push(json!({"route":state,"cognition":cognition,"installation_packages_confirmed":units_confirmed,"capability_fact":node.capability_facts()?.into_iter().find(|f| f.node_id == state.peer.node_id),"current_peer_liveness":"not_proven_by_historical_ack"}));
            }
            println!(
                "{}",
                json!({"node_id":node.local_node_id()?,"local_durable":true,"no_peers_unprotected":peers.is_empty(),"peers":peers,"capabilities":node.capability_profile()?,"coordination":node.coordination_candidates()?,"workspace_default":"not_replicated","local_credential_stores_and_grants":"not_discovered_or_copied","global_strong_consistency":false,"zero_loss":false})
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
            // A peer built before capability advertisements exists: accept the
            // missing fact rather than pretend capabilities were observed.
            if let Some(capabilities) = value.get("capabilities") {
                let capabilities: NodeCapabilityProfile =
                    serde_json::from_value(capabilities.clone())?;
                ensure!(
                    capabilities.node_id == peer.node_id,
                    "identity reply capability advertisement names a different Node"
                );
                node.record_peer_capability(
                    &capabilities,
                    "enrollment identity reply over authenticated SSH",
                )?;
            }
            node.register_sync_peer(&peer)?;
            println!(
                "{}",
                json!({"peer":peer,"replicated":false,"next":"shenko sync push; start also reconciles configured peers"})
            );
        }
        SyncCommand::Export { peer_id, workspace } => {
            stage_declared_recovery_sources(&data_dir, &store, &node)?;
            let (header, _) = header(&mut node)?;
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
            let (_, units) = header(&mut node)?;
            if units == receipt.units_fingerprint {
                if let Some(state) = node
                    .sync_peers()?
                    .into_iter()
                    .find(|p| p.peer.node_id == receipt.receiver_node_id)
                {
                    ensure!(
                        receipt.capabilities.node_id == state.peer.node_id,
                        "ACK capability advertisement names a different Node"
                    );
                    node.record_peer_capability(
                        &receipt.capabilities,
                        "sync ack receipt over authenticated channel",
                    )?;
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
        SyncCommand::Election { command } => match command {
            ElectionCommand::Status => {
                println!("{}", node.election_status()?);
            }
            ElectionCommand::Campaign => {
                drop(node);
                let fence = tokio::task::spawn_blocking(move || {
                    establish_primary(&data_dir, &store, &CancellationToken::new())
                })
                .await??;
                match fence {
                    Some(token) => println!(
                        "{}",
                        json!({"elected": true, "fencing_token": token, "availability_branch": false})
                    ),
                    None => println!(
                        "{}",
                        json!({"elected": false, "fencing_token": null, "availability_branch": true})
                    ),
                }
            }
        },
        SyncCommand::Members { command } => match command {
            MembersCommand::Show => {
                println!(
                    "{}",
                    json!({"member_configuration":node.member_configuration()?,"committed_primary_claim":node.committed_primary_claim()?,"branches":node.branches()?})
                );
            }
            MembersCommand::Add { node_ids } => {
                drop(node);
                let value = tokio::task::spawn_blocking(move || {
                    member_change(&data_dir, &store, &CancellationToken::new(), node_ids, Vec::new())
                })
                .await??;
                println!("{value}");
            }
            MembersCommand::Remove { node_ids } => {
                drop(node);
                let value = tokio::task::spawn_blocking(move || {
                    member_change(&data_dir, &store, &CancellationToken::new(), Vec::new(), node_ids)
                })
                .await??;
                println!("{value}");
            }
            MembersCommand::Push { peer } => {
                drop(node);
                let value = tokio::task::spawn_blocking(move || {
                    push_member_configuration(&data_dir, &store, &CancellationToken::new(), peer)
                })
                .await??;
                println!("{value}");
            }
        },
        SyncCommand::Adopt {
            identity_file,
            trusted_node,
            provenance,
            reason,
        } => {
            let trusted_identity: shenko_node::Identity =
                serde_json::from_reader(std::io::BufReader::new(
                    std::fs::File::open(&identity_file)
                        .with_context(|| format!("read trusted identity {}", identity_file.display()))?,
                ))
                .context("decode trusted identity")?;
            ensure_adopt_is_coherent(&store, &trusted_identity, "sync adopt")?;
            let record = node.adopt_identity(AdoptRequest {
                operation_id: EntityId::new(),
                trusted_identity,
                trusted_node_id: trusted_node,
                provenance,
                reason,
            })?;
            println!("{}", serde_json::to_string(&record)?);
        }
        SyncCommand::Reconcile { peer } => {
            drop(node);
            let value = tokio::task::spawn_blocking(move || {
                reconcile_with_peer(&data_dir, &store, &CancellationToken::new(), peer)
            })
            .await??;
            println!("{value}");
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

/// Record what this runtime actually started. The `Primary` role is recorded
/// only with the committed election's fencing token; `None` records an honest
/// executor/recovery/replica observation with no domain-primary claim —
/// e.g. a deterministic bootstrap or an availability branch after a
/// partition (SPEC §5.4.3).
pub fn observe(
    data_dir: &Path,
    store: &Store,
    runtime: EntityId,
    fence: Option<&FencingToken>,
) -> Result<()> {
    let mut node = open_node(data_dir, store)?;
    let mut roles = vec![NodeRole::Executor, NodeRole::Recovery, NodeRole::Replica];
    let details = match fence {
        Some(_) => {
            roles.push(NodeRole::Primary);
            "actual subject task and native recovery/replication consumers started as the committed primary; provider readiness remains separate"
        }
        None => {
            "native consumers started without a committed election fence; no domain-primary role claimed"
        }
    };
    node.observe_runtime_roles(
        runtime,
        roles,
        store.get_existing_agent_id()?,
        details.into(),
        fence,
    )?;
    Ok(())
}

pub fn node_command(data_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let store = crate::open_installation_store(data_dir)?;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(shenko_node::MAX_NODE_REQUEST as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= shenko_node::MAX_NODE_REQUEST,
        "Node request exceeds 16MiB"
    );
    let request = serde_json::from_slice(&bytes)?;
    // The same contract the CLI enforces: identity-writing requests are
    // refused before the node records anything it cannot keep coherent with
    // the store's subject binding. `adopt` is the divergence-recovery op and
    // reaches the node even when the pair already diverged; `join` can only
    // ever write the bound identity and is rejected outright when it tries
    // to adopt a different self.
    match &request {
        shenko_node::Request::Adopt { request: adopt } => {
            ensure_adopt_is_coherent(&store, &adopt.trusted_identity, "sync adopt")?;
        }
        shenko_node::Request::Join { identity, .. } => {
            ensure_adopt_is_coherent(&store, identity, "node join")?;
        }
        _ => {}
    }
    let mut node = match &request {
        shenko_node::Request::Adopt { .. } => open_node_for_adoption(data_dir, &store)?,
        _ => open_node(data_dir, &store)?,
    };
    println!("{}", node.handle(request)?);
    Ok(())
}
