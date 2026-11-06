//! Partition, rejoin and fact merge across independent node processes.
//!
//! SPEC §5.4.1–§5.4.3, §22.4, C17. Every node in the journey below is a real
//! `shenko-node` child process with its own private SQLite installation: node
//! identity, membership, votes, partition branches and merged facts are always
//! read back by a *later* process, never from in-memory state. The test is the
//! transport: it delivers exactly the messages a partitioned node could send,
//! and its absence is the partition.
//!
//! What this consumer does not claim: it does not measure peer liveness, it
//! does not exercise SSH, and both nodes run the same binary, so the recorded
//! heterogeneity comparison is `false` here (a different artifact on another
//! OS/arch is a deployment fact this fixture cannot invent).

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_node::{
    ControlEntry, Event, Identity, Manifest, MemberChangeRequest, MemberProposal, Node,
    PeerExperience, RawEvent, Request, VoteRequest, VoteResponse, running_manifest,
};
use shenko_types::{ids::EntityId, object::Digest};
use std::io::Write;
use std::path::{Path, PathBuf};

const BIN: &str = env!("CARGO_BIN_EXE_shenko-node");

fn child_manifest() -> Result<Manifest> {
    let output = std::process::Command::new(BIN).arg("--manifest").output()?;
    ensure!(
        output.status.success(),
        "manifest invocation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

/// One request to one node process: one process per request.
fn run(dir: &Path, request: &Request) -> Result<Value> {
    let bytes = serde_json::to_vec(request)?;
    let mut child = std::process::Command::new(BIN)
        .arg(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("spawn node process")?;
    {
        let mut stdin = child.stdin.take().context("node stdin")?;
        stdin.write_all(&bytes)?;
    }
    let output = child.wait_with_output()?;
    ensure!(
        output.status.success(),
        "node process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("decode node response")
}

fn private_dir(root: &Path, name: &str) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = root.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

/// A node installation that is only ever touched through separate processes.
struct NodeProcess {
    dir: PathBuf,
    id: EntityId,
    identity: Identity,
}

impl NodeProcess {
    fn join(root: &Path, name: &str, identity: Identity) -> Result<Self> {
        let dir = private_dir(root, name)?;
        let id = EntityId::new();
        run(
            &dir,
            &Request::Initialize {
                node_id: id,
                identity: None,
            },
        )?;
        let joined = run(
            &dir,
            &Request::Join {
                operation_id: EntityId::new(),
                expected_node: id,
                identity: identity.clone(),
                manifest: child_manifest()?,
            },
        )?;
        ensure!(joined["node_id"] == json!(id), "join recorded another node");
        run(
            &dir,
            &Request::Register {
                node_id: id,
                target_id: format!("process:{name}"),
                binding_digest: Digest::blake3(name.as_bytes()),
            },
        )?;
        Ok(Self { dir, id, identity })
    }

    fn call(&self, request: &Request) -> Result<Value> {
        run(&self.dir, request)
    }

    fn campaign(&self) -> Result<(VoteRequest, String)> {
        let campaign = self.call(&Request::Campaign)?;
        let request: VoteRequest = serde_json::from_value(
            campaign
                .get("request")
                .context("campaign omitted its vote request")?
                .clone(),
        )?;
        let term = campaign
            .get("term")
            .and_then(Value::as_u64)
            .context("campaign omitted its term")?
            .to_string();
        Ok((request, term))
    }

    /// Deliver one vote request to this node.
    fn vote(&self, request: &VoteRequest) -> Result<VoteResponse> {
        Ok(serde_json::from_value(self.call(&Request::Vote {
            request: request.clone(),
        })?)?)
    }

    fn config(&self) -> Result<Value> {
        let members = self.call(&Request::Members)?;
        members
            .get("member_configuration")
            .cloned()
            .context("members omitted the configuration")
    }

    fn control_log(&self) -> Result<Vec<ControlEntry>> {
        Ok(serde_json::from_value(
            self.call(&Request::ControlLog)?["entries"].clone(),
        )?)
    }

    fn adopt(&self, config: &Value, entries: Vec<ControlEntry>) -> Result<Value> {
        self.call(&Request::PushConfig {
            config: serde_json::from_value(config.clone())?,
            entries,
        })
    }

    fn election_status(&self) -> Result<Value> {
        self.call(&Request::ElectionStatus)
    }

    /// The identity this installation persisted, read by a fresh process.
    fn persisted_identity(&self) -> Result<Identity> {
        let status = self.call(&Request::Status { operation_id: None })?;
        ensure!(
            status["node_id"] == json!(self.id),
            "node id changed across processes"
        );
        let persisted: Identity =
            serde_json::from_value(status["metadata"]["identity"].clone())?;
        ensure!(
            persisted == self.identity,
            "persisted identity differs from the joined identity"
        );
        Ok(persisted)
    }

    /// Real experience produced while this node was on its own branch.
    fn execute(&self, input: &[u8]) -> Result<EntityId> {
        let task_id = EntityId::new();
        let created = self.call(&Request::Create {
            task_id,
            node_id: self.id,
            input: input.to_vec(),
        })?;
        self.call(&Request::Execute {
            ticket: serde_json::from_value(created["ticket"].clone())?,
            input: input.to_vec(),
        })?;
        Ok(task_id)
    }

    /// The peer's exported experience, pulled the way a rejoin reads it.
    fn experience(&self) -> Result<PeerExperience> {
        let mut events = Vec::new();
        let mut cursor = "0".to_string();
        let mut high_water = "0".to_string();
        loop {
            let page = self.call(&Request::Export {
                after: cursor.clone(),
                limit: 128,
            })?;
            let through = page
                .get("through")
                .and_then(Value::as_str)
                .context("peer export omitted its cursor")?
                .to_string();
            high_water = through.clone();
            let batch: Vec<Event> = serde_json::from_value(page["events"].clone())?;
            let count = batch.len();
            events.extend(batch);
            if through == cursor || count == 0 {
                break;
            }
            cursor = through;
        }
        let mut receipts = Vec::new();
        let mut cursor = "0".to_string();
        loop {
            let page = self.call(&Request::ExportReceipts {
                after: cursor.clone(),
                limit: 128,
            })?;
            let through = page
                .get("through")
                .and_then(Value::as_str)
                .context("peer receipt export omitted its cursor")?
                .to_string();
            let rows: Vec<Value> = serde_json::from_value(page["receipts"].clone())?;
            let count = rows.len();
            for row in rows {
                receipts.push(
                    serde_json::from_value(
                        row.get("receipt")
                            .context("peer receipt row omitted its receipt")?
                            .clone(),
                    )
                    .context("peer receipt row")?,
                );
            }
            if through == cursor || count == 0 {
                break;
            }
            cursor = through;
        }
        let status = self.call(&Request::Status { operation_id: None })?;
        let peer_branch_id: Option<EntityId> = status
            .pointer("/metadata/identity/branch_id")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .context("peer branch id")?;
        let peer_manifest: Manifest = serde_json::from_value(
            status
                .get("running_manifest")
                .context("peer status omitted its running manifest")?
                .clone(),
        )?;
        Ok(PeerExperience {
            peer_node_id: self.id,
            peer_branch_id,
            events,
            receipts,
            cognition_fingerprint: None,
            cognition_high_water: None,
            cognition_origin: None,
            peer_high_water: Some(high_water),
            peer_manifest: Some(peer_manifest),
            note: "experience pulled from an independent node process".into(),
        })
    }
}

fn merge_branch(
    node: &NodeProcess,
    branch_id: EntityId,
    reconciliation_id: EntityId,
) -> Result<Value> {
    node.call(&Request::MergeBranch {
        branch_id,
        reconciliation_id,
        note: "rejoined the verified peer and merged the partition branch".into(),
    })
}

fn branch(status: &Value, branch_id: EntityId) -> Result<Value> {
    status["branches"]
        .as_array()
        .context("election status omitted branches")?
        .iter()
        .find(|fact| fact["branch_id"] == json!(branch_id))
        .cloned()
        .with_context(|| format!("branch {branch_id} was not recorded"))
}

#[test]
fn partitioned_domain_rejoins_and_merges_its_facts() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path();
    let a_identity = Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: Digest::blake3(b"node a origin"),
    };
    let b_identity = Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: Digest::blake3(b"node b origin"),
    };
    let a = NodeProcess::join(root, "a", a_identity.clone())?;
    let b = NodeProcess::join(root, "b", b_identity.clone())?;
    assert_eq!(a.persisted_identity()?, a_identity);
    assert_eq!(b.persisted_identity()?, b_identity);
    assert_ne!(a.id, b.id, "one node identity per installation");

    // A fresh installation is a one-member domain: its own vote is a real
    // quorum certificate, not a pretend one.
    let (genesis, genesis_term) = a.campaign()?;
    assert!(
        genesis.proposal.is_none(),
        "a primary campaign carries no member proposal"
    );
    let genesis_outcome = a.call(&Request::Tally { term: genesis_term })?;
    assert_eq!(genesis_outcome["elected"], true, "{genesis_outcome}");
    assert_eq!(
        a.election_status()?["committed_primary_claim"]["epoch"],
        json!(1),
        "the genesis claim carries the first fencing epoch"
    );

    // Join B to the connected domain: the added voter must first hold the base
    // configuration, then it votes on the joint change, then on the final one.
    let base = a.config()?;
    b.adopt(&base, a.control_log()?)?;
    let joint = a.call(&Request::MemberChange {
        request: MemberChangeRequest {
            operation_id: EntityId::new(),
            expected_revision: base["revision"]
                .as_u64()
                .context("base configuration revision")?,
            add: vec![b.id],
            remove: vec![],
            reason: "the second node joins the connected domain".into(),
        },
    })?;
    let joint_request: VoteRequest = serde_json::from_value(joint["request"].clone())?;
    let joint_response = b.vote(&joint_request)?;
    assert_eq!(joint_response.vote_granted, true, "{joint_response:?}");
    let recorded = a.call(&Request::RecordVote {
        request: joint_request,
        response: joint_response,
    })?;
    assert_eq!(
        recorded["outcome"], "accepted",
        "an added voter's proposal vote must be countable: {recorded}"
    );
    let joint_proposal: MemberProposal = serde_json::from_value(joint["proposal"].clone())?;
    let joint_committed = a.call(&Request::TallyMemberChange {
        proposal: joint_proposal.clone(),
    })?;
    assert_eq!(joint_committed["quorum_met"], true, "{joint_committed}");

    let joint_config = a.config()?;
    let joint_new = joint_config["joint_new"]
        .as_array()
        .context("the joint configuration keeps the new set")?;
    assert_eq!(joint_new.len(), 2, "{joint_config}");
    assert!(
        joint_new.contains(&json!(a.id)) && joint_new.contains(&json!(b.id)),
        "the joint configuration keeps both sets: {joint_config}"
    );
    b.adopt(&joint_config, a.control_log()?)?;
    let final_round = a.call(&Request::FinalMemberChange {
        proposal: joint_proposal,
    })?;
    let final_request: VoteRequest = serde_json::from_value(final_round["request"].clone())?;
    let final_response = b.vote(&final_request)?;
    assert_eq!(final_response.vote_granted, true, "{final_response:?}");
    let final_recorded = a.call(&Request::RecordVote {
        request: final_request,
        response: final_response,
    })?;
    assert_eq!(final_recorded["outcome"], "accepted", "{final_recorded}");
    let final_proposal: MemberProposal = serde_json::from_value(final_round["proposal"].clone())?;
    let final_committed = a.call(&Request::TallyMemberChange {
        proposal: final_proposal,
    })?;
    assert_eq!(final_committed["quorum_met"], true, "{final_committed}");
    let committed = a.config()?;
    let members = committed["members"]
        .as_array()
        .context("committed members")?;
    assert_eq!(members.len(), 2, "{committed}");
    assert!(members.contains(&json!(a.id)) && members.contains(&json!(b.id)));
    assert_eq!(committed["joint_new"], Value::Null);
    b.adopt(&committed, a.control_log()?)?;
    assert_eq!(
        b.config()?,
        committed,
        "both members hold the same committed configuration"
    );
    assert_eq!(
        b.call(&Request::Members)?["quorum"]["required"],
        json!(2),
        "a two-member domain needs both voters; visible nodes are not the quorum"
    );

    // Single primary in the connected domain: a real two-member election.
    let (election, election_term) = a.campaign()?;
    let election_response = b.vote(&election)?;
    assert_eq!(election_response.vote_granted, true, "{election_response:?}");
    a.call(&Request::RecordVote {
        request: election,
        response: election_response,
    })?;
    let elected = a.call(&Request::Tally {
        term: election_term,
    })?;
    assert_eq!(elected["elected"], true, "{elected}");
    assert_eq!(
        elected["claim"]["epoch"],
        json!(2),
        "a new claim takes the next fencing epoch"
    );
    // The leader replicates the committed claim; otherwise the follower keeps
    // the previous primary as its committed fact.
    let log = a.control_log()?;
    let claim_entry = log.last().context("no committed control entry")?;
    assert_eq!(
        claim_entry.kind,
        shenko_node::ControlKind::Primary,
        "the newest committed entry is the primary claim"
    );
    let appended = b.call(&Request::Append {
        request: shenko_node::AppendRequest {
            schema: shenko_node::ELECTION_WIRE_SCHEMA,
            leader_id: a.id,
            prev_index: log
                .iter()
                .rev()
                .nth(1)
                .map(|entry| entry.index)
                .unwrap_or(0),
            prev_term: log
                .iter()
                .rev()
                .nth(1)
                .map(|entry| entry.term)
                .unwrap_or(0),
            entries: vec![(*claim_entry).clone()],
            proposal: None,
        },
    })?;
    assert_eq!(appended["accepted"], json!(1), "{appended}");
    let follower = b.election_status()?;
    assert_eq!(
        follower["committed_primary_claim"]["leader_node_id"],
        json!(a.id),
        "the follower learns the domain claim"
    );
    assert_eq!(follower["committed_primary_claim"]["epoch"], json!(2));
    assert_eq!(a.election_status()?["this_node_is_primary"], json!(true));
    assert_eq!(
        b.election_status()?["this_node_is_primary"],
        json!(false),
        "the domain has one primary"
    );

    // Partition: the transport no longer carries B's vote, so A reaches no
    // quorum of the committed configuration and says so.
    let (_, partitioned_term) = a.campaign()?;
    let unreachable = a.call(&Request::Tally {
        term: partitioned_term,
    })?;
    assert_eq!(unreachable["elected"], false, "{unreachable}");
    assert_eq!(
        unreachable["reason"],
        json!("no quorum of the committed configuration")
    );
    let a_branch = EntityId::new();
    let opened = a.call(&Request::AvailabilityBranch {
        branch_id: a_branch,
        subject_id: Some(a_identity.agent_id),
        reason: "partitioned: no reachable quorum of the committed configuration".into(),
    })?;
    assert_eq!(opened["kind"], json!("availability"));
    assert_eq!(
        opened["global_quorum"],
        json!(false),
        "a partition branch never claims a global quorum"
    );
    assert_eq!(opened["merged"], Value::Null);

    // B observes the same partition from its own side and stays honest too.
    let (b_campaign, b_term) = b.campaign()?;
    assert!(b_campaign.proposal.is_none());
    let b_outcome = b.call(&Request::Tally { term: b_term })?;
    assert_eq!(b_outcome["elected"], false, "{b_outcome}");
    let b_branch = EntityId::new();
    let b_opened = b.call(&Request::AvailabilityBranch {
        branch_id: b_branch,
        subject_id: Some(b_identity.agent_id),
        reason: "partitioned: the peer's votes are not reachable".into(),
    })?;
    assert_eq!(b_opened["global_quorum"], json!(false));

    // Real experience on both branches while the domain is split.
    a.execute(b"partitioned work on a")?;
    b.execute(b"partitioned work on b")?;

    // Rejoin A: merge B's experience, then archive the partition branch with
    // the merge fact (never reset it).
    let b_experience = b.experience()?;
    assert!(
        !b_experience.events.is_empty(),
        "the partitioned node produced real events"
    );
    let a_merge = a.call(&Request::Reconcile {
        request: b_experience.clone(),
    })?;
    assert_eq!(a_merge["peer_node_id"], json!(b.id));
    assert_eq!(
        a_merge["imported_events"],
        json!(b_experience.events.len() as u64)
    );
    assert_eq!(a_merge["rejected_events"], json!(0));
    assert_eq!(
        a_merge["peer_high_water"],
        serde_json::to_value(&b_experience.peer_high_water)?
    );
    assert_eq!(
        a_merge["heterogeneous"],
        json!(false),
        "both processes run the same artifact in this fixture"
    );
    assert_eq!(
        a_merge["peer_manifest"]["artifact_digest"],
        serde_json::to_value(&b_experience.peer_manifest.as_ref().unwrap().artifact_digest)?
    );
    let a_reconciliation: EntityId =
        serde_json::from_value(a_merge["reconciliation_id"].clone())?;
    let a_merged = merge_branch(&a, a_branch, a_reconciliation)?;
    assert_eq!(
        a_merged["kind"],
        json!("availability"),
        "the archived branch keeps the role it played"
    );
    assert_eq!(
        a_merged["merged"]["reconciliation_id"],
        json!(a_reconciliation)
    );
    assert_eq!(a_merged["merged"]["peer_node_id"], json!(b.id));
    assert_eq!(
        a_merged["merged"]["primary"]["epoch"],
        json!(2),
        "the merge records the committed claim, not a new one"
    );
    assert_eq!(
        a_merged["global_quorum"],
        json!(true),
        "the domain still holds its committed primary claim"
    );
    // A replayed rejoin is the same merge, not a second one.
    let replayed = merge_branch(&a, a_branch, a_reconciliation)?;
    assert_eq!(
        replayed["merged"]["merged_at"],
        a_merged["merged"]["merged_at"]
    );
    assert_eq!(
        branch(&a.election_status()?, a_branch)?["merged"]["reconciliation_id"],
        json!(a_reconciliation),
        "the merge fact survived the process that recorded it"
    );

    // Rejoin B symmetrically. B merged the domain's facts but is not the
    // primary: its merged branch records the domain claim without promoting
    // this node.
    let a_experience = a.experience()?;
    let b_merge = b.call(&Request::Reconcile {
        request: a_experience,
    })?;
    let b_reconciliation: EntityId =
        serde_json::from_value(b_merge["reconciliation_id"].clone())?;
    let b_merged = merge_branch(&b, b_branch, b_reconciliation)?;
    assert!(
        b_merged["merged"].is_object(),
        "the archived branch carries its merge fact: {b_merged}"
    );
    assert_eq!(
        b_merged["merged"]["primary"]["leader_node_id"],
        json!(a.id),
        "the merged fact names the domain primary, not this node"
    );
    assert_eq!(b_merged["merged"]["primary"]["epoch"], json!(2));
    assert_eq!(
        b.election_status()?["this_node_is_primary"],
        json!(false),
        "merging the domain claim does not make this node the primary"
    );

    // Replaying the same peer experience cannot duplicate retained evidence.
    let replay = a.call(&Request::Reconcile {
        request: b_experience.clone(),
    })?;
    assert_eq!(replay["imported_events"], json!(0));
    assert_eq!(
        replay["duplicate_events"],
        json!(b_experience.events.len() as u64)
    );
    assert_eq!(
        replay["retained_conflicts"],
        a_merge["retained_conflicts"],
        "a replay retains no new parallel facts"
    );

    // A branch already merged by one reconciliation cannot be re-closed by a
    // record that does not cover it.
    let unrelated = a.call(&Request::Reconcile {
        request: PeerExperience {
            peer_node_id: b.id,
            peer_branch_id: None,
            events: vec![],
            receipts: vec![],
            cognition_fingerprint: None,
            cognition_high_water: None,
            cognition_origin: None,
            peer_high_water: None,
            peer_manifest: None,
            note: "no facts at all".into(),
        },
    })?;
    assert_eq!(unrelated["imported_events"], json!(0));
    let unrelated_reconciliation: EntityId =
        serde_json::from_value(unrelated["reconciliation_id"].clone())?;
    let refused = merge_branch(&a, a_branch, unrelated_reconciliation);
    assert!(
        refused.is_err(),
        "an unrelated reconciliation cannot close a merged branch"
    );
    Ok(())
}

#[test]
fn merge_accepts_only_verified_facts_and_retains_conflicts_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let dir = private_dir(root.path(), "local")?;
    let identity = Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: Digest::blake3(b"merge fixture origin"),
    };
    let mut node = Node::open(&dir)?;
    let local_id = EntityId::new();
    node.handle(Request::Initialize {
        node_id: local_id,
        identity: Some(identity.clone()),
    })?;
    let peer_id = EntityId::new();
    let event_id = EntityId::new();
    let manifest = running_manifest()?;

    let event = |payload: Value| -> Result<Event> {
        let raw = RawEvent {
            schema: 1,
            event_id,
            agent_id: identity.agent_id,
            branch_id: identity.branch_id,
            node_id: peer_id,
            epoch: "1".into(),
            local_seq: "1".into(),
            causal_parents: vec![],
            payload,
        };
        Ok(Event {
            digest: Digest::blake3(&serde_json::to_vec(&raw)?),
            raw,
        })
    };
    let experience = |events: Vec<Event>| PeerExperience {
        peer_node_id: peer_id,
        peer_branch_id: Some(identity.branch_id),
        events,
        receipts: vec![],
        cognition_fingerprint: None,
        cognition_high_water: None,
        cognition_origin: None,
        peer_high_water: Some("1".into()),
        peer_manifest: Some(manifest.clone()),
        note: "verification fixture".into(),
    };

    let first =
        node.reconcile_peer_experience(&experience(vec![event(
            json!({"kind": "probe", "value": 1}),
        )?]))?;
    assert_eq!(first.imported_events, 1);
    assert_eq!(first.retained_conflicts, 0);

    // The same event identity with different content is a parallel fact, kept
    // side by side rather than overwriting the first one.
    let conflicting = experience(vec![event(json!({"kind": "probe", "value": 2}))?]);
    let second = node.reconcile_peer_experience(&conflicting)?;
    assert_eq!(second.imported_events, 0);
    assert_eq!(second.parallel_facts.len(), 1);
    assert_eq!(second.retained_conflicts, 1);
    let replay = node.reconcile_peer_experience(&conflicting)?;
    assert_eq!(
        replay.retained_conflicts, 1,
        "replaying the same parallel fact does not grow the conflict journal"
    );
    assert_eq!(replay.parallel_facts.len(), 1);

    // A tampered digest is not merged at all.
    let mut tampered = event(json!({"kind": "probe", "value": 3}))?;
    tampered.digest = Digest::blake3(b"not the content");
    let rejected = node.reconcile_peer_experience(&experience(vec![tampered]))?;
    assert_eq!(rejected.rejected_events, 1);
    assert_eq!(rejected.imported_events, 0);
    let exported = node.handle(Request::Export {
        after: "0".into(),
        limit: 128,
    })?;
    assert_eq!(
        exported["events"].as_array().map(|events| events.len()),
        Some(1),
        "only the verified event is in the journal"
    );
    drop(node);

    // The retained conflict is durable, not in-memory state.
    let mut reopened = Node::open(&dir)?;
    let status = reopened.handle(Request::ElectionStatus)?;
    assert_eq!(status["parallel_facts_retained"], json!("1"));
    assert_eq!(status["parallel_facts_are_evidence"], json!(true));
    Ok(())
}
