//! Authenticated bounded E1 delivery, separate from chosen control and archives.
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use shenko_node::{Identity, Node, Request, SyncPeer};
use shenko_store::{NativeEvidenceDeliveryOutcome, Store};
use shenko_types::{ids::EntityId, native_evidence::*, object::Digest};
use tokio_util::sync::CancellationToken;

use super::{peer_route, rpc};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Ineligible {
    NoVerifiedSelf,
    DifferentSelf,
    NoLocalSubject,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum EvidencePageResponse {
    Manifest {
        page: NativeEvidencePage,
    },
    NotEligible {
        schema_version: u32,
        scope: String,
        node_id: EntityId,
        identity: Option<Identity>,
        reason: Ineligible,
    },
}

pub(super) fn serve(
    store: &Store,
    node: &mut Node,
    subject_id: EntityId,
    origin_proof: &Digest,
    scope: &str,
    cursor: Option<&NativeEvidenceCursor>,
    page_items: Option<u16>,
) -> Result<EvidencePageResponse> {
    ensure!(scope == NATIVE_EVIDENCE_SCOPE, "unsupported native E1 requested scope");
    let node_id = node
        .local_node_id()?
        .context("native E1 source has no initialized Node")?;
    let identity = node.local_identity()?;
    let reason = match &identity {
        None => Some(Ineligible::NoVerifiedSelf),
        Some(identity)
            if identity.agent_id != subject_id || &identity.origin_proof != origin_proof =>
        {
            Some(Ineligible::DifferentSelf)
        }
        Some(_) if store.installation_binding()?.subject.is_none() => {
            Some(Ineligible::NoLocalSubject)
        }
        Some(_) => None,
    };
    if let Some(reason) = reason {
        return Ok(EvidencePageResponse::NotEligible {
            schema_version: NATIVE_EVIDENCE_SCHEMA_VERSION,
            scope: NATIVE_EVIDENCE_SCOPE.into(),
            node_id,
            identity,
            reason,
        });
    }
    let identity = identity.context("eligible native E1 source lost its self identity")?;
    let source = NativeEvidenceSource {
        node_id,
        ledger_id: store.ledger.ledger_id(),
        agent_id: identity.agent_id,
        branch_id: identity.branch_id,
        origin_proof: identity.origin_proof.clone(),
    };
    let page = store.native_evidence_page(
        &source,
        cursor,
        page_items.map_or(NATIVE_EVIDENCE_PAGE_ITEMS, usize::from),
    )?;
    // No arbitrary root is accepted from the wire. The Store has verified
    // every advertised source receipt, its original T1 and original closure.
    for item in &page.items {
        if let NativeEvidenceAvailability::Sealed { evidence_ref } = &item.availability {
            let objects = store.native_evidence_objects(item)?;
            node.retain_native_evidence_objects(evidence_ref, &objects)?;
        }
    }
    ensure!(
        node.local_identity()?.as_ref() == Some(&identity),
        "native E1 source identity changed while publishing"
    );
    ensure!(
        serde_json::to_vec(&page)?.len() <= shenko_node::CONTROL_PAGE_BYTES,
        "native E1 manifest exceeds bounded page size"
    );
    Ok(EvidencePageResponse::Manifest { page })
}

#[derive(Debug, Serialize)]
pub(super) struct EvidenceSyncReport {
    peer_id: EntityId,
    source: Option<NativeEvidenceSource>,
    not_eligible: Option<serde_json::Value>,
    pages: usize,
    applied: usize,
    already_applied: usize,
    reconciliation_required: usize,
    retained_delivery_debts: u64,
    complete: bool,
}

impl EvidenceSyncReport {
    pub(super) fn has_debt(&self) -> bool {
        self.reconciliation_required != 0 || self.retained_delivery_debts != 0
    }
    pub(super) fn scanned(&self) -> bool {
        self.complete
    }
}

fn validate_page(
    page: &NativeEvidencePage,
    peer: &SyncPeer,
    local: &shenko_node::Identity,
    requested: Option<&NativeEvidenceCursor>,
    previous: Option<&NativeEvidenceSource>,
) -> Result<()> {
    ensure!(
        serde_json::to_vec(page)?.len() <= shenko_node::CONTROL_PAGE_BYTES,
        "native E1 manifest exceeds the receiving page byte budget"
    );
    ensure!(
        page.schema_version == NATIVE_EVIDENCE_SCHEMA_VERSION && page.scope == NATIVE_EVIDENCE_SCOPE,
        "unsupported native E1 manifest scope"
    );
    ensure!(
        page.source.node_id == peer.node_id,
        "authenticated native E1 route returned a different Node ID"
    );
    ensure!(
        page.source.agent_id == local.agent_id && page.source.origin_proof == local.origin_proof,
        "native E1 source is not the verified local self"
    );
    ensure!(
        page.cursor.scope == NATIVE_EVIDENCE_SCOPE
            && page.cursor.ledger_id == page.source.ledger_id
            && page.items.len() <= NATIVE_EVIDENCE_PAGE_ITEMS,
        "native E1 manifest identity or page budget differs"
    );
    if let Some(requested) = requested {
        ensure!(
            &page.cursor == requested,
            "native E1 page skipped its requested cursor"
        );
    } else {
        ensure!(
            page.cursor.after.is_none(),
            "first native E1 page skipped its prefix"
        );
    }
    if let Some(previous) = previous {
        ensure!(
            &page.source == previous,
            "native E1 source changed during pagination"
        );
    }
    let mut after = page.cursor.after;
    for item in &page.items {
        ensure!(
            item.position.sequence > 0
                && item.position.sequence <= page.cursor.through
                && after.is_none_or(|after| item.position > after)
                && item.source_commit.ledger_id == page.source.ledger_id
                && item.source_commit.seq.0 == item.position.sequence,
            "native E1 manifest position is not a strictly ordered source fact"
        );
        after = Some(item.position);
    }
    if let Some(next) = &page.next {
        ensure!(
            !page.items.is_empty()
                && next.scope == page.cursor.scope
                && next.ledger_id == page.cursor.ledger_id
                && next.through == page.cursor.through
                && next.after == after,
            "native E1 continuation changed its fixed snapshot or skipped a fact"
        );
    }
    Ok(())
}

/// The caller obtains the peer from this installation's registered SSH routes.
/// Losing a reply can only repeat factual intake, never arm a new operation.
pub(super) fn pull(
    store: &Store,
    node: &mut Node,
    peer: &SyncPeer,
    cancel: &CancellationToken,
) -> Result<EvidenceSyncReport> {
    let mut report = EvidenceSyncReport {
        peer_id: peer.node_id,
        source: None,
        not_eligible: None,
        pages: 0,
        applied: 0,
        already_applied: 0,
        reconciliation_required: 0,
        retained_delivery_debts: store.native_evidence_delivery_debt_count(peer.node_id)?,
        complete: false,
    };
    let Some(local) = node.local_identity()? else {
        report.not_eligible =
            Some(serde_json::json!({"local_reason":"receiver_has_no_verified_self"}));
        return Ok(report);
    };
    let mut cursor = None;
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "native E1 transfer cancelled; retained delivery debt remains"
        );
        let registered =
            peer_route(node, peer.node_id)?.context("native E1 peer is no longer registered")?;
        ensure!(
            serde_json::to_value(&registered)? == serde_json::to_value(peer)?,
            "native E1 SSH route changed during transfer"
        );
        let response = rpc(
            peer,
            &Request::NativeEvidencePage {
                subject_id: local.agent_id,
                origin_proof: local.origin_proof.clone(),
                scope: NATIVE_EVIDENCE_SCOPE.into(),
                cursor: cursor.clone(),
                page_items: None,
            },
            cancel,
        )?;
        ensure!(
            serde_json::to_vec(&response)?.len() <= shenko_node::CONTROL_PAGE_BYTES,
            "native E1 response exceeds the receiving byte budget"
        );
        let response: EvidencePageResponse =
            serde_json::from_value(response).context("decode bounded native E1 manifest")?;
        let page = match response {
            EvidencePageResponse::Manifest { page } => page,
            EvidencePageResponse::NotEligible {
                schema_version,
                scope,
                node_id,
                identity,
                reason,
            } => {
                ensure!(
                    schema_version == NATIVE_EVIDENCE_SCHEMA_VERSION
                        && scope == NATIVE_EVIDENCE_SCOPE && node_id == peer.node_id && cursor.is_none(),
                    "native E1 eligibility response changed its authenticated Node or interrupted a snapshot"
                );
                let same = identity.as_ref().is_some_and(|identity| {
                    identity.agent_id == local.agent_id
                        && identity.origin_proof == local.origin_proof
                });
                ensure!(
                    match reason {
                        Ineligible::NoVerifiedSelf => identity.is_none(),
                        Ineligible::DifferentSelf => identity.is_some() && !same,
                        Ineligible::NoLocalSubject => same,
                    },
                    "native E1 eligibility reason differs from the actual Node identity"
                );
                report.not_eligible = Some(
                    serde_json::json!({"node_id":node_id,"identity":identity,"reason":reason}),
                );
                return Ok(report);
            }
        };
        validate_page(&page, peer, &local, cursor.as_ref(), report.source.as_ref())?;
        for item in &page.items {
            ensure!(
                !cancel.is_cancelled(),
                "native E1 transfer cancelled; source facts remain unconfirmed"
            );
            let delivery = NativeEvidenceDelivery {
                source: page.source.clone(),
                item: item.clone(),
            };
            let retained = store.queue_native_evidence_delivery(&delivery)?;
            let objects = match &item.availability {
                NativeEvidenceAvailability::Sealed { evidence_ref } => node
                    .pull_native_evidence_objects(evidence_ref, &mut |request| {
                        rpc(peer, &request, cancel)
                    })
                    .with_context(|| {
                        format!(
                            "native E1 {} transfer incomplete; local delivery {} remains pending",
                            item.position.receipt_id, retained.delivery_id
                        )
                    })?,
                NativeEvidenceAvailability::Gap { .. } => Vec::new(),
            };
            match store.receive_native_evidence(retained.delivery_id, &objects)? {
                NativeEvidenceDeliveryOutcome::Applied { .. } => report.applied += 1,
                NativeEvidenceDeliveryOutcome::AlreadyApplied { .. } => report.already_applied += 1,
                NativeEvidenceDeliveryOutcome::ReconciliationRequired { .. } => {
                    report.reconciliation_required += 1
                }
            }
        }
        report.pages += 1;
        report.source = Some(page.source);
        cursor = page.next;
        if cursor.is_none() {
            report.retained_delivery_debts =
                store.native_evidence_delivery_debt_count(peer.node_id)?;
            report.complete = true;
            return Ok(report);
        }
    }
}

pub(super) fn run_once(
    data_dir: &std::path::Path,
    store: &Store,
    peer_id: EntityId,
) -> Result<EvidenceSyncReport> {
    let mut node = super::open_node(data_dir, store)?;
    let peer = peer_route(&node, peer_id)?.context("native E1 peer is not registered")?;
    pull(store, &mut node, &peer, &CancellationToken::new())
}
