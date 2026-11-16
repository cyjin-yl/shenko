//! Resident following and explicit minority-component startup.
use super::*;
use shenko_node::{
    ComponentAcknowledgement, ComponentCandidate, ComponentCertificate, ComponentObservation,
    ComponentPresence, ComponentProposal, ComponentUnavailable,
};

fn available(condition: bool, reason: &str) -> Result<()> {
    if !condition {
        return Err(ComponentUnavailable(reason.into()).into());
    }
    Ok(())
}

fn component_rpc(
    peer: &SyncPeer,
    request: &shenko_node::Request,
    cancel: &CancellationToken,
) -> Result<Value> {
    rpc(peer, request, cancel).map_err(|error| {
        ComponentUnavailable(format!(
            "authenticated component exchange incomplete: {error:#}"
        ))
        .into()
    })
}

pub enum RuntimeSelection {
    Selected {
        fence: Option<FencingToken>,
        authority: Arc<crate::runtime_authority::RuntimeAuthority>,
        heartbeat: tokio::task::JoinHandle<Result<()>>,
    },
    Stopped,
}

type SelectedAuthority = (
    Option<FencingToken>,
    Arc<crate::runtime_authority::RuntimeAuthority>,
);

/// The owner lock remains held while this process follows. It admits no
/// subject or native consumers until a real authority/branch is selected.
pub async fn establish_runtime(
    data_dir: &Path,
    store: &Arc<Store>,
    cancel: &CancellationToken,
    runtime_id: EntityId,
    profile: &shenko_seed::ModelProfile,
) -> Result<RuntimeSelection> {
    let profile_digest = Digest::blake3(&serde_json::to_vec(profile)?);
    let candidate = profile.configured.then_some(ComponentCandidate {
        runtime_id,
        profile_digest,
        preference: None,
    });
    // This is willingness to start the configured binding, not a successful
    // inference or a claim about comparative model intelligence.
    tokio::task::spawn_blocking({
        let data_dir = data_dir.to_owned();
        let candidate = candidate.clone();
        move || {
            Node::open(&data_dir.join("node"))?.advertise_component_runtime(runtime_id, candidate)
        }
    })
    .await??;
    let heartbeat_cancel = cancel.child_token();
    let stop_heartbeat = heartbeat_cancel.clone().drop_guard();
    let heartbeat = {
        let data_dir = data_dir.to_owned();
        let heartbeat_cancel = heartbeat_cancel.clone();
        // The observation worker is part of this owned runtime. Its durable
        // authority does not expire with presence, so worker failure must
        // cancel the runtime explicitly, including an unexpected task drop.
        let stop_runtime_on_exit = cancel.clone().drop_guard();
        tokio::spawn(async move {
            let _stop_runtime_on_exit = stop_runtime_on_exit;
            while !heartbeat_cancel.is_cancelled() {
                let path = data_dir.clone();
                let candidate = candidate.clone();
                tokio::task::spawn_blocking(move || {
                    Node::open(&path.join("node"))?.refresh_component_runtime(runtime_id, candidate)
                })
                .await
                .context("runtime observation task failed")?
                .context("runtime observation could not be renewed")?;
                tokio::select! {
                    _ = heartbeat_cancel.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
            }
            Ok(())
        })
    };
    let selected: Result<Option<SelectedAuthority>> = async {
      let mut last_reason = None;
      loop {
        let worker_cancel = cancel.child_token();
        let mut selection = tokio::task::spawn_blocking({
            let data_dir = data_dir.to_owned();
            let store = Arc::clone(store);
            let selection_cancel = worker_cancel.clone();
            let runtime_cancel = cancel.clone();
            move || -> Result<SelectedAuthority> {
                let selected = match establish_primary(&data_dir, &store, &selection_cancel) {
                    Err(error)
                        if error.downcast_ref::<PrimaryNotSelected>().is_none()
                            && error.downcast_ref::<ComponentUnavailable>().is_none() =>
                    {
                        return Err(error);
                    }
                    selected => selected,
                };
                let authority = crate::runtime_authority::RuntimeAuthority::attach(
                    &data_dir,
                    &store,
                    runtime_cancel,
                )?;
                // Factual learned results may materialize on a follower.
                // Only a selected runtime may resume unchosen local preparation.
                let forked = if selected.is_ok() {
                    authority.recover_before_consumers()?
                } else {
                    authority.recover_before_following()?;
                    false
                };
                let mut fence = selected?;
                if forked {
                    fence = None;
                }
                Ok((fence, authority))
            }
        });
        let result = tokio::select! {
            _completed = &mut selection => _completed?,
            _stop_signal = tokio::signal::ctrl_c() => {
                cancel.cancel(); worker_cancel.cancel();
                let _ = selection.await;
                _stop_signal?;
                break Ok(None);
            },
            _ = cancel.cancelled() => {
                worker_cancel.cancel();
                let _ = selection.await;
                break Ok(None);
            },
        };
        match result {
            Ok(selected) => break Ok(Some(selected)),
            Err(error)
                if error.downcast_ref::<PrimaryNotSelected>().is_some()
                    || error.downcast_ref::<ComponentUnavailable>().is_some()
                    || crate::runtime_authority::requires_reconstruction(&error) =>
            {
                let reason = error.to_string();
                if last_reason.as_ref() != Some(&reason) {
                    tracing::info!(%reason, "resident follower retains history and waits for authority");
                    // The resident follower is a queryable runtime fact. It
                    // holds the durable history and waits for authority; it
                    // starts no subject task, no native recovery consumer and
                    // no replica worker, so it never claims those duties.
                    if let Err(error) = crate::node_sync::observe_follower(
                        data_dir,
                        store.as_ref(),
                        runtime_id,
                        &reason,
                    ) {
                        // The observation is a queryable fact, not the
                        // follower's reason to exist. A journal that cannot
                        // be written is reported, never a reason to stop
                        // following.
                        tracing::warn!(%error, "resident follower role observation could not be recorded");
                    }
                    last_reason = Some(reason);
                }
            }
            Err(error) => break Err(error),
        }
        tokio::select! {
            _stop_signal = tokio::signal::ctrl_c() => { _stop_signal?; cancel.cancel(); break Ok(None); },
            _ = cancel.cancelled() => break Ok(None),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
        }
      }
    }.await;
    if !cancel.is_cancelled()
        && let Ok(Some((fence, authority))) = selected
    {
        stop_heartbeat.disarm();
        return Ok(RuntimeSelection::Selected {
            fence,
            authority,
            heartbeat,
        });
    }
    heartbeat_cancel.cancel();
    let joined = heartbeat
        .await
        .map_err(anyhow::Error::from)
        .and_then(std::convert::identity)
        .map_err(crate::runtime_authority::observation_failure);
    match selected {
        Err(error) => Err(match joined {
            Ok(()) => error,
            Err(observation) => {
                observation.context(format!("runtime selection also failed: {error:#}"))
            }
        }),
        Ok(_) => {
            joined?;
            Ok(RuntimeSelection::Stopped)
        }
    }
}

pub(super) fn observe_component(
    node: &mut Node,
    cancel: &CancellationToken,
) -> Result<ComponentObservation> {
    let own = node.local_node_id()?.context("node not initialized")?;
    let config = node
        .member_configuration()?
        .context("missing member configuration")?;
    let mut reachable = BTreeSet::from([own]);
    let mut complete = true;
    for member in config.voters() {
        if member == own {
            continue;
        }
        let Some(peer) = peer_route(node, member)? else {
            continue;
        };
        let value = rpc_observed(
            &peer,
            &shenko_node::Request::ComponentPresence,
            cancel,
            &mut reachable,
        );
        match value {
            Ok(value) => match serde_json::from_value::<ComponentPresence>(value) {
                Ok(presence)
                    if presence.node_id == member
                        && presence.configuration_revision == config.revision
                        && presence.configuration_digest == config.digest => {}
                _ => complete = false,
            },
            Err(_) if reachable.contains(&member) => complete = false,
            Err(_) => {}
        }
        if cancel.is_cancelled() {
            bail!("component observation cancelled");
        }
    }
    Ok(ComponentObservation {
        presence: node.component_presence()?,
        reachable: reachable.into_iter().collect(),
        complete,
    })
}

/// SSH command reachability does not mean the fenced subject process is still
/// resident. An expired process observation permits a fresh quorum election;
/// it never proves the previous process or its physical effects have stopped.
pub(super) fn primary_is_resident(
    node: &Node,
    fence: &FencingToken,
    cancel: &CancellationToken,
) -> Result<bool> {
    let Some(peer) = peer_route(node, fence.leader_id)? else {
        return Ok(false);
    };
    let Ok(value) = rpc(&peer, &shenko_node::Request::ComponentPresence, cancel) else {
        return Ok(false);
    };
    let Ok(presence) = serde_json::from_value::<ComponentPresence>(value) else {
        return Ok(false);
    };
    let config = node
        .member_configuration()?
        .context("missing member configuration")?;
    let identity = node.local_identity()?.context("missing local identity")?;
    Ok(presence.node_id == fence.leader_id
        && presence.runtime_id.is_some()
        && presence.term == fence.term
        && presence.configuration_revision == fence.config_revision
        && presence.identity.agent_id == identity.agent_id
        && presence.identity.origin_proof == identity.origin_proof
        && presence.configuration_revision == config.revision
        && presence.configuration_digest == config.digest)
}

fn confirm(
    node: &mut Node,
    certificate: &ComponentCertificate,
    cancel: &CancellationToken,
) -> Result<()> {
    certificate.validate()?;
    let observed = observe_component(node, cancel)?;
    available(
        observed.complete && observed.reachable == certificate.proposal.members(),
        "component reachability changed",
    )?;
    available(
        observed.presence.committed.as_ref() == Some(certificate),
        "local component selection changed",
    )?;
    for agreed in &certificate.proposal.observations {
        let member = agreed.presence.node_id;
        let presence = if member == observed.presence.node_id {
            observed.presence.clone()
        } else {
            let peer = peer_route(node, member)?.context("component route disappeared")?;
            serde_json::from_value::<ComponentPresence>(component_rpc(
                &peer,
                &shenko_node::Request::ComponentPresence,
                cancel,
            )?)?
        };
        available(
            presence.node_id == member && presence.committed.as_ref() == Some(certificate),
            "component member no longer confirms the same coordinator",
        )?;
        available(
            presence.runtime_id == agreed.presence.runtime_id
                && presence.candidate == agreed.presence.candidate,
            "component runtime candidacy expired or changed",
        )?;
    }
    Ok(())
}

pub fn confirm_component_authority(
    data_dir: &Path,
    store: &Store,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut node = open_node(data_dir, store)?;
    let presence = node.component_presence()?;
    let Some(certificate) = presence.committed else {
        return Ok(());
    };
    ensure!(
        certificate.proposal.coordinator == presence.node_id,
        "component follower cannot admit subject work"
    );
    match confirm(&mut node, &certificate, cancel) {
        Err(error) if error.downcast_ref::<ComponentUnavailable>().is_some() => Err(
            shenko_node::ControlAdmissionError::AuthorityUnavailable(format!(
                "component confirmation unavailable: {error:#}"
            ))
            .into(),
        ),
        other => other,
    }
}

/// Reuse only a unanimously retained selection whose full observed component
/// is unchanged. It is not a stored global-primary shortcut.
pub(super) fn resume_component(
    data_dir: &Path,
    store: &Store,
    node: &mut Node,
    cancel: &CancellationToken,
) -> Result<Option<Option<FencingToken>>> {
    let presence = node.component_presence()?;
    let Some(certificate) = presence.committed else {
        return Ok(None);
    };
    match confirm(node, &certificate, cancel) {
        Ok(()) => {}
        Err(error) if error.downcast_ref::<ComponentUnavailable>().is_some() => return Ok(None),
        Err(error) => return Err(error),
    }
    if certificate.proposal.coordinator != presence.node_id {
        // A component follower still learns the global control prefix. It
        // holds the same durable history, and it must not keep waiting on a
        // stale control fact while its coordinator holds the component.
        match super::catch_up_global_control(node, cancel) {
            Ok(outcomes) => {
                tracing::debug!(?outcomes, "component follower caught up the global control prefix")
            }
            Err(error) => {
                tracing::warn!(%error, "component follower could not refresh the global control prefix")
            }
        }
        return Err(PrimaryNotSelected(format!(
            "component {} follows coordinator {}",
            certificate.proposal.decision_id, certificate.proposal.coordinator
        ))
        .into());
    }
    materialize_component(data_dir, store, node, &certificate)?;
    Ok(Some(None))
}

fn materialize_component(
    data_dir: &Path,
    store: &Store,
    node: &mut Node,
    certificate: &ComponentCertificate,
) -> Result<()> {
    let reservation = Node::open(&data_dir.join("node"))?.reserve_authority()?;
    let snapshot = reservation.snapshot().clone();
    available(
        snapshot.component.as_ref() == Some(certificate)
            && certificate.proposal.coordinator == snapshot.node_id,
        "component changed before branch materialization",
    )?;
    drop(reservation);
    let reason = format!(
        "unanimous availability component {} selected {} without global quorum",
        certificate.proposal.decision_id, certificate.proposal.coordinator
    );
    node.open_availability_branch_if_current(
        certificate.proposal.branch_id,
        Some(snapshot.identity.agent_id),
        &reason,
        &snapshot,
    )?;
    crate::runtime_branches::fork_availability_branch(
        data_dir,
        store,
        certificate.proposal.branch_id,
        &reason,
        &snapshot.identity,
    )
}

pub(super) fn establish_component(
    data_dir: &Path,
    store: &Store,
    node: &mut Node,
    cancel: &CancellationToken,
) -> Result<Option<FencingToken>> {
    let mut local = observe_component(node, cancel)?;
    available(
        local.complete && local.reachable.len() > 1,
        "component observation unavailable",
    )?;
    let members = local.reachable.clone();
    let own = local.presence.node_id;
    let mut observations = Vec::with_capacity(members.len());
    local.presence.committed = None;
    for member in &members {
        if *member == own {
            observations.push(local.clone());
            continue;
        }
        let peer =
            peer_route(node, *member)?.context("component member has no authenticated route")?;
        let mut observation: ComponentObservation = serde_json::from_value(component_rpc(
            &peer,
            &shenko_node::Request::ComponentObserve,
            cancel,
        )?)?;
        ensure!(
            observation.presence.node_id == *member,
            "component observation route identity mismatch"
        );
        observation.presence.committed = None;
        observations.push(observation);
    }
    let coordinator = observations
        .iter()
        .filter_map(|observation| {
            observation.presence.candidate.as_ref().map(|candidate| {
                (
                    candidate.preference.as_ref().map(|p| p.rank),
                    observation.presence.node_id,
                )
            })
        })
        .max_by(|(a_rank, a_id), (b_rank, b_id)| a_rank.cmp(b_rank).then_with(|| b_id.cmp(a_id)))
        .ok_or_else(|| {
            ComponentUnavailable("no resident configured candidate in the component".into())
        })?
        .1;
    if coordinator != own {
        return Err(PrimaryNotSelected(format!(
            "observed component awaits preferred coordinator {coordinator}"
        ))
        .into());
    }
    let decision_id = EntityId::new();
    let proposal = ComponentProposal {
        decision_id,
        branch_id: EntityId::derived(
            "availability-component-branch-v1",
            decision_id.to_string().as_bytes(),
        ),
        coordinator,
        observations,
    };
    proposal
        .validate()
        .map_err(|error| ComponentUnavailable(error.to_string()))?;
    let mut acknowledgements = Vec::with_capacity(members.len());
    for member in &members {
        let acknowledgement: ComponentAcknowledgement = if *member == own {
            let observed = observe_component(node, cancel)?;
            node.prepare_component(&proposal, &observed)?
        } else {
            let peer = peer_route(node, *member)?.context("component route disappeared")?;
            serde_json::from_value(component_rpc(
                &peer,
                &shenko_node::Request::PrepareComponent {
                    proposal: proposal.clone(),
                },
                cancel,
            )?)?
        };
        ensure!(
            acknowledgement.node_id == *member,
            "component acknowledgement route mismatch"
        );
        acknowledgements.push(acknowledgement);
    }
    let certificate = ComponentCertificate {
        proposal: Box::new(proposal),
        acknowledgements,
    };
    certificate.validate()?;
    // Publish to followers before local activation. A lost commit reply never
    // permits activation from a partial or silently reduced certificate.
    for member in members.iter().filter(|member| **member != own) {
        let peer = peer_route(node, *member)?.context("component route disappeared")?;
        let learned: ComponentCertificate = serde_json::from_value(component_rpc(
            &peer,
            &shenko_node::Request::CommitComponent {
                certificate: certificate.clone(),
            },
            cancel,
        )?)?;
        ensure!(learned == certificate, "component commit reply mismatch");
    }
    node.commit_component(&certificate)?;
    confirm(node, &certificate, cancel)?;
    materialize_component(data_dir, store, node, &certificate)?;
    Ok(None)
}
