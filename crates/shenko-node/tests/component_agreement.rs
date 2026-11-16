use anyhow::{Context, Result, ensure};
use shenko_node::{
    ComponentCandidate, ComponentCertificate, ComponentObservation, ComponentProposal, Identity,
    MemberChangeRequest, Node, Request,
};
use shenko_types::{ids::EntityId, object::Digest};

fn publish(leader: &Node, peers: &mut [Node]) -> Result<()> {
    let configuration = leader
        .member_configuration()?
        .context("configuration missing")?;
    let entries = leader.control_log()?;
    for peer in peers {
        peer.adopt_member_configuration(&configuration, &entries)?;
    }
    Ok(())
}

fn persist(leader: &mut Node, peers: &mut [Node]) -> Result<()> {
    let request = leader
        .pending_control_replication()?
        .context("pending control missing")?;
    for peer in peers {
        let id = peer.local_node_id()?.context("peer missing")?;
        let acknowledgement =
            leader.replicate_control_to(id, &request, &mut |request| peer.handle(request))?;
        ensure!(
            acknowledgement.accepted,
            "acceptor refused: {}",
            acknowledgement.reason
        );
        leader.record_control_acknowledgement(&request, &acknowledgement)?;
    }
    ensure!(
        leader.commit_prepared_control(&request)?.committed,
        "control did not commit"
    );
    Ok(())
}

fn elect(leader: &mut Node, peers: &mut [Node]) -> Result<()> {
    let campaign = leader.campaign()?;
    for peer in peers.iter_mut() {
        let response = peer.handle_vote_request(&campaign.request)?;
        leader.record_vote_response(&campaign.request, &response)?;
    }
    if !leader.tally(campaign.term)?.elected {
        persist(leader, peers)?;
    }
    ensure!(leader.tally(campaign.term)?.elected, "election missing");
    publish(leader, peers)
}

fn proposal(a: &Node, b: &Node, coordinator: EntityId) -> Result<ComponentProposal> {
    let mut observations = vec![a.component_presence()?, b.component_presence()?];
    observations.sort_by_key(|observation| observation.node_id);
    let members = observations
        .iter()
        .map(|observation| observation.node_id)
        .collect::<Vec<_>>();
    let decision_id = EntityId::new();
    Ok(ComponentProposal {
        decision_id,
        branch_id: EntityId::derived(
            "availability-component-branch-v1",
            decision_id.to_string().as_bytes(),
        ),
        coordinator,
        observations: observations
            .into_iter()
            .map(|mut presence| {
                presence.committed = None;
                ComponentObservation {
                    presence,
                    reachable: members.clone(),
                    complete: true,
                }
            })
            .collect(),
    })
}

fn observed(node: &Node, proposal: &ComponentProposal) -> Result<ComponentObservation> {
    Ok(ComponentObservation {
        presence: node.component_presence()?,
        reachable: proposal.members(),
        complete: true,
    })
}

#[test]
fn minority_agreement_survives_reopen_and_supersession_fences_original_branch() -> Result<()> {
    let root = tempfile::tempdir()?;
    let identity = Identity {
        agent_id: EntityId::new(),
        branch_id: EntityId::new(),
        origin_proof: Digest::blake3(b"component shared origin"),
    };
    let mut nodes = Vec::new();
    for index in 0..5 {
        let mut node = Node::open(&root.path().join(index.to_string()))?;
        node.handle(Request::Initialize {
            node_id: EntityId::new(),
            identity: Some(identity.clone()),
        })?;
        nodes.push(node);
    }
    // Build the actual five-member configuration through both Joint and Final
    // persistence quorums. No direct SQL fixture fabricates global membership.
    let (leader, peers) = nodes.split_first_mut().context("leader missing")?;
    leader.ensure_member_configuration()?;
    elect(leader, &mut [])?;
    publish(leader, peers)?;
    let joint = leader.propose_member_change(MemberChangeRequest {
        operation_id: EntityId::new(),
        expected_revision: 1,
        add: peers
            .iter()
            .map(|peer| peer.local_node_id()?.context("member missing"))
            .collect::<Result<Vec<_>>>()?,
        remove: vec![],
        reason: "five-node durable component fixture".into(),
    })?;
    for peer in peers.iter_mut() {
        let response = peer.handle_vote_request(&joint.request)?;
        leader.record_vote_response(&joint.request, &response)?;
    }
    leader.tally_member_change(&joint.proposal)?;
    persist(leader, peers)?;
    leader.tally_member_change(&joint.proposal)?;
    publish(leader, peers)?;
    elect(leader, peers)?;
    let final_round = leader.propose_final_member_change(&joint.proposal)?;
    for peer in peers.iter_mut() {
        let response = peer.handle_vote_request(&final_round.request)?;
        leader.record_vote_response(&final_round.request, &response)?;
    }
    leader.tally_member_change(&final_round.proposal)?;
    persist(leader, peers)?;
    leader.tally_member_change(&final_round.proposal)?;
    publish(leader, peers)?;
    for node in &mut nodes {
        node.campaign()?; // No votes/acceptances: none has a new global fence.
        node.advertise_component_candidate(ComponentCandidate {
            runtime_id: EntityId::new(),
            profile_digest: Digest::blake3(b"configured fixture model, not inference evidence"),
            preference: None,
        })?;
    }
    let a_id = nodes[0].local_node_id()?.context("a missing")?;
    let b_id = nodes[1].local_node_id()?.context("b missing")?;
    let coordinator = a_id.min(b_id);
    let (a, remaining) = nodes.split_first_mut().context("a missing")?;
    let b = &mut remaining[0];
    a.open_availability_branch(
        identity.branch_id,
        Some(identity.agent_id),
        "previous isolated episode",
    )?;
    let first = proposal(a, b, coordinator)?;
    let mut omitted = first.clone();
    omitted.observations[0].reachable.pop();
    assert!(
        omitted.validate().is_err(),
        "a reachable peer cannot be omitted"
    );
    let a_observed = observed(a, &first)?;
    let a_ack = a.prepare_component(&first, &a_observed)?;
    let old_guard = Node::open(&root.path().join("0"))?.reserve_authority()?;
    assert!(
        !old_guard.snapshot().is_current_availability(),
        "acknowledgement fences prior singleton branch before reply"
    );
    drop(old_guard);
    let partial = ComponentCertificate {
        proposal: Box::new(first.clone()),
        acknowledgements: vec![a_ack.clone()],
    };
    assert!(
        a.commit_component(&partial).is_err(),
        "partial component agreement is not authority"
    );
    let b_observed = observed(b, &first)?;
    let b_ack = b.prepare_component(&first, &b_observed)?;
    let mut acknowledgements = vec![a_ack, b_ack];
    acknowledgements.sort_by_key(|ack| ack.node_id);
    let certificate = ComponentCertificate {
        proposal: Box::new(first.clone()),
        acknowledgements,
    };
    a.commit_component(&certificate)?;
    b.commit_component(&certificate)?;
    drop(nodes);

    let mut a = Node::open(&root.path().join("0"))?;
    let mut b = Node::open(&root.path().join("1"))?;
    assert_eq!(a.component_presence()?.committed, Some(certificate.clone()));
    assert_eq!(
        b.commit_component(&certificate)?,
        certificate,
        "lost commit reply is an exact replay"
    );
    let (coordinator_node, follower_node) = if coordinator == a_id {
        (&mut a, &mut b)
    } else {
        (&mut b, &mut a)
    };
    coordinator_node.open_availability_branch(
        first.branch_id,
        Some(identity.agent_id),
        "agreed component",
    )?;
    assert!(
        follower_node
            .open_availability_branch(
                first.branch_id,
                Some(identity.agent_id),
                "cannot impersonate coordinator"
            )
            .is_err()
    );
    let historical = coordinator_node.branches()?;
    assert!(
        historical
            .iter()
            .any(|branch| branch.branch_id == first.branch_id && !branch.global_quorum)
    );

    // Another accepted view supersedes the first; replaying its old complete
    // certificate cannot regain authority after either participant restarts.
    let second = proposal(&a, &b, coordinator)?;
    let next_a = a.prepare_component(&second, &observed(&a, &second)?)?;
    assert!(a.commit_component(&certificate).is_err());
    let next_b = b.prepare_component(&second, &observed(&b, &second)?)?;
    let mut acknowledgements = vec![next_a, next_b];
    acknowledgements.sort_by_key(|ack| ack.node_id);
    let next = ComponentCertificate {
        proposal: Box::new(second),
        acknowledgements,
    };
    a.commit_component(&next)?;
    b.commit_component(&next)?;
    assert!(
        a.fencing_token()?.is_none() && b.fencing_token()?.is_none(),
        "component never creates a global fence"
    );
    let coordinator_node = if coordinator == a_id { &mut a } else { &mut b };
    coordinator_node.relinquish_component_if_current(
        &next,
        "original prepared transition cannot materialize on this branch",
    )?;
    assert!(coordinator_node.component_presence()?.committed.is_none());
    assert!(coordinator_node.commit_component(&next).is_err());
    let retirement = coordinator_node.handle(Request::Status {
        operation_id: Some(EntityId::derived(
            "component-retirement-v1",
            format!("{}:{coordinator}", next.proposal.decision_id).as_bytes(),
        )),
    })?;
    assert_eq!(
        retirement["operation"]["certificate"],
        serde_json::to_value(&next)?
    );
    let third = proposal(&a, &b, coordinator)?;
    let mut acknowledgements = vec![
        a.prepare_component(&third, &observed(&a, &third)?)?,
        b.prepare_component(&third, &observed(&b, &third)?)?,
    ];
    acknowledgements.sort_by_key(|ack| ack.node_id);
    let renewed = ComponentCertificate {
        proposal: Box::new(third),
        acknowledgements,
    };
    a.commit_component(&renewed)?;
    b.commit_component(&renewed)?;
    a.campaign()?;
    assert!(
        a.component_presence()?.committed.is_none(),
        "new global term invalidates old component selection"
    );
    assert!(a.commit_component(&renewed).is_err());
    let retained = a.handle(Request::Status {
        operation_id: Some(EntityId::derived(
            "component-commit-v1",
            format!("{}:{a_id}", first.decision_id).as_bytes(),
        )),
    })?;
    assert_eq!(
        retained["operation"],
        serde_json::to_value(&certificate)?,
        "superseded selection keeps its original full agreement evidence"
    );
    let historical = if coordinator == a_id {
        a.branches()?
    } else {
        b.branches()?
    };
    assert!(
        historical
            .iter()
            .any(|branch| branch.branch_id == first.branch_id && !branch.global_quorum),
        "superseded branch experience remains retained"
    );
    let old_runtime = a
        .component_presence()?
        .runtime_id
        .context("observed runtime missing")?;
    let replacement_runtime = EntityId::new();
    a.advertise_component_runtime(replacement_runtime, None)?;
    assert!(a.refresh_component_runtime(old_runtime, None).is_err());
    assert_eq!(
        a.component_presence()?.runtime_id,
        Some(replacement_runtime)
    );
    assert!(
        a.component_presence()?.candidate.is_none(),
        "process presence is not model readiness"
    );
    Ok(())
}
