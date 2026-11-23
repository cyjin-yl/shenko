//! §51 acceptance journey over the real Store and CAS.
//!
//! Sequence: open the Store, publish the `forgetting` metapackage into the
//! content/attitude ledger (body plus adopted member content), apply one real
//! S1 subject step that queues a memory save, restart, reopen, settle the
//! queued memory, evaluate forgetting (record the projection attitude), settle
//! the semantic forget, then assert the receipts and that CAS retention still
//! verifies every byte the history names.
//!
//! All durable state here is the product's own: the SQLite ledger and the
//! content-addressed store. The only fixture is the shared canonical S0/S1
//! construction already used by the other Store journeys; it builds real
//! sealed plans/decisions, not a mock of durable state.
//!
//! This test was authored in a source packet and was NOT executed there.

use anyhow::{Context, Result};
use shenko_store::{
    AttitudeDraft, ContentWrite, MemoryIntentRecord, RecordedAttitude, RetentionLimits, Store,
    StorePublication,
};
use shenko_store::roots::CasWriteFence;
use shenko_types::cognition::{
    AdoptionEvidence, AdoptionTarget, AttitudeConfidence, AttitudeRole, ConfidenceState,
    ProjectionStage, ReasonAdmission, ReasonPointer, ReasonReading,
};
use shenko_types::ids::{EntityId, RecordRevision};
use shenko_types::object::ObjectRef;
use shenko_types::subject::{
    MemoryIntent, MemoryIntentKind, StepDecision, SubjectStep, SubjectStepState,
};
use std::sync::Arc;

#[path = "support/subject_genesis.rs"]
mod subject_genesis;
#[path = "support/canonical_native.rs"]
mod canonical_native;
use canonical_native::{committed_candidate_with, recorded_result_with_docket};
use subject_genesis::initialize_subject_in_with_route;

const METAPACKAGE_UNIT: &str = "forgetting";
const METAPACKAGE_REVISION: &str = "1";
const METAPACKAGE_MEMBER_A: &str = "forgetting.awareness.relative-runtime";
const METAPACKAGE_MEMBER_B: &str = "forgetting.fact.retrieval-decay";
const MEMORY_SCOPE: &str = "personal";
const SOURCE_NOTE: &str = "retained research note: forgetting is measured in subject-relative runtime, \
not wall clock; retrieval and use are separate signals";
const FACT_TEXT: &str = "the quiet harbor beacon was used on the first evening";

struct PublishedMetapackage {
    source: RecordedAttitude,
    body: RecordedAttitude,
    body_bytes: Vec<u8>,
    members: Vec<(String, Vec<u8>, RecordedAttitude)>,
}

/// Publish the metapackage exactly as §51.3 describes it: one body that names
/// the member set, one source record the body stops at, and one adoption
/// attitude per member. The body and its members are immutable CAS content;
/// their attitudes are ledger rows. Copying content never adopts it: the
/// adoption attitude is a separate write with its own reason pointers.
fn publish_metapackage(store: &Store, agent_id: EntityId) -> Result<PublishedMetapackage> {
    let source = store.record_content_attitude(
        agent_id,
        ContentWrite::new(SOURCE_NOTE.as_bytes().to_vec(), "text/plain"),
        AttitudeDraft::assertion(Vec::new(), AttitudeConfidence::Unknown),
    )?;
    let source_reason = ReasonPointer::content(source.content.object.digest.clone());

    let body_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "kind": "metapackage",
        "unit_id": METAPACKAGE_UNIT,
        "revision": METAPACKAGE_REVISION,
        "provides": ["cap.forgetting/1"],
        "members": [
            {"member_id": METAPACKAGE_MEMBER_A, "kind": "awareness", "revision": "1"},
            {"member_id": METAPACKAGE_MEMBER_B, "kind": "fact", "revision": "1"},
        ],
    }))?;
    let body = store.record_content_attitude(
        agent_id,
        ContentWrite::new(body_bytes.clone(), "application/json"),
        AttitudeDraft::assertion(vec![source_reason.clone()], AttitudeConfidence::Unknown),
    )?;

    let members = [
        (
            METAPACKAGE_MEMBER_A,
            "awareness",
            "notice when the subject-relative runtime since the last retrieval has grown; this is a \
             notice, never a command to forget",
        ),
        (
            METAPACKAGE_MEMBER_B,
            "fact",
            "retention measured on subject-relative runtime falls steeply first and then slowly",
        ),
    ]
    .into_iter()
    .map(|(member_id, kind, statement)| {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "unit_id": METAPACKAGE_UNIT,
            "member_id": member_id,
            "kind": kind,
            "revision": "1",
            "statement": statement,
        }))?;
        let recorded = store.record_content_attitude(
            agent_id,
            ContentWrite::new(bytes.clone(), "application/json"),
            AttitudeDraft::adoption(
                AdoptionTarget::unit(METAPACKAGE_UNIT)
                    .with_version(METAPACKAGE_REVISION)
                    .with_member(member_id),
                vec![source_reason.clone()],
                AttitudeConfidence::Unknown,
            ),
        )?;
        Ok((member_id.to_string(), bytes, recorded))
    })
    .collect::<Result<Vec<_>>>()?;

    Ok(PublishedMetapackage {
        source,
        body,
        body_bytes,
        members,
    })
}

/// Assert the published metapackage is readable from this handle: the body
/// bytes are unchanged, each member carries an authenticated adoption
/// attitude whose reason pointers resolve locally, and the attitude roles name
/// the metapackage version and member slot.
fn assert_metapackage_published(
    store: &Store,
    agent_id: EntityId,
    published: &PublishedMetapackage,
) -> Result<()> {
    let body_digest = &published.body.content.object.digest;
    assert_eq!(store.content_bytes(body_digest)?, published.body_bytes);
    let body_attitude = store
        .latest_attitude(agent_id, body_digest, Some("assertion"))?
        .context("published metapackage body attitude")?;
    assert_eq!(body_attitude.role, AttitudeRole::Assertion);
    assert_eq!(body_attitude.created_commit, published.body.commit);
    assert_eq!(
        body_attitude.confidence,
        Some(AttitudeConfidence::Unknown)
    );
    assert_eq!(
        body_attitude.reading(),
        ReasonReading::Conclusion,
        "the body stops at the source content, so the mechanical reading is a conclusion"
    );

    for (member_id, bytes, recorded) in &published.members {
        let digest = &recorded.content.object.digest;
        assert_eq!(store.content_bytes(digest)?, *bytes);
        let adoption = store
            .adoption_for(agent_id, METAPACKAGE_UNIT, digest)?
            .with_context(|| format!("adoption projection for {member_id}"))?;
        assert!(adoption.adopted);
        assert_eq!(adoption.unit_id, METAPACKAGE_UNIT);
        assert_eq!(&adoption.content_digest, digest);
        assert_eq!(adoption.confidence, ConfidenceState::Unknown);
        assert!(
            matches!(adoption.evidence, AdoptionEvidence::Undetermined { .. }),
            "no domain verifier was consulted, so the evidence stays undetermined"
        );
        assert!(!adoption.reasons.is_empty());
        assert!(
            adoption
                .reasons
                .iter()
                .all(|report| report.admission == ReasonAdmission::PresentLocal),
            "every member reason must resolve against this installation"
        );
        assert_eq!(adoption.recorded_commit, recorded.commit);
        assert!(store.is_adopted(agent_id, METAPACKAGE_UNIT, digest)?);

        let attitude = store
            .latest_attitude(agent_id, digest, Some("adoption"))?
            .with_context(|| format!("retained adoption attitude for {member_id}"))?;
        assert_eq!(
            attitude.role,
            AttitudeRole::Adoption(
                AdoptionTarget::unit(METAPACKAGE_UNIT)
                    .with_version(METAPACKAGE_REVISION)
                    .with_member(member_id.as_str())
            )
        );
        assert_eq!(attitude.created_commit, recorded.commit);
    }
    Ok(())
}

/// Re-seal the candidate decision with one memory-save intent. This is the
/// same shape `attach_wait` uses for a policy intent: the sealed decision is
/// replaced by a new immutable object, and the step keeps pointing at the
/// version it will commit. Nothing durable is edited in place.
fn attach_memory_save(
    store: &Store,
    step: &mut SubjectStep,
    intent_id: EntityId,
    content_ref: ObjectRef,
) -> Result<()> {
    let decision_ref = step
        .decision_ref
        .as_ref()
        .context("committed candidate has no decision")?;
    let mut decision: StepDecision = serde_json::from_slice(
        &store.cas.read_object(&decision_ref.digest)?,
    )
    .context("decode the candidate decision")?;
    decision.memory_intents.push(MemoryIntent {
        intent_id,
        kind: MemoryIntentKind::Save,
        scope: MEMORY_SCOPE.to_string(),
        content_ref: Some(content_ref),
    });
    step.decision_ref = Some(
        store
            .cas
            .seal_object(
                &serde_json::to_vec(&decision)?,
                "application/json",
                "decisions",
                "StepDecision",
                8,
            )?
            .object,
    );
    Ok(())
}

#[test]
fn metapackage_step_restart_forgetting_receipts_and_cas_retention() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("shenko.db");
    let cas = directory.path().join("cas");

    // 1. Real Store open, real genesis.
    let store = Arc::new(Store::open(database.clone(), cas.clone())?);
    let agent_id = initialize_subject_in_with_route(&store, b"forgetting acceptance", None)?;
    let ledger_id = store.ledger.ledger_id();

    // 2. Publish the metapackage (content + adoption attitudes).
    let published = publish_metapackage(&store, agent_id)?;
    assert_metapackage_published(&store, agent_id, &published)?;
    let publish_receipts = [
        &published.source.commit,
        &published.body.commit,
        &published.members[0].2.commit,
        &published.members[1].2.commit,
    ];
    for receipt in publish_receipts {
        assert_eq!(receipt.ledger_id, ledger_id);
    }
    for pair in publish_receipts.windows(2) {
        assert!(
            pair[1].seq.0 > pair[0].seq.0,
            "publishes append in ledger order"
        );
    }
    let last_publish_seq = publish_receipts[3].seq.0;

    // 3. Apply one real subject step. The S0/S1 fixture seals a genuine plan,
    // model outcome and decision; the decision then names a memory save whose
    // content is the sealed fact.
    let (head, step) = store.load_subject(agent_id)?;
    let first_step_id = step.step_id;
    let successor_step_id = EntityId::new();
    let save_intent = EntityId::new();
    let fact_object = store
        .cas
        .seal_object(FACT_TEXT.as_bytes(), "text/plain", "memory", "raw", 1)?
        .object;
    let recorded = recorded_result_with_docket(&store, &head, step, None, &[])?;
    let mut candidate = committed_candidate_with(&store, recorded, successor_step_id, vec![]);
    attach_memory_save(&store, &mut candidate, save_intent, fact_object.clone())?;
    let step_receipt = store.commit_step_and_advance_head(&candidate, &head, successor_step_id)?;
    assert_eq!(step_receipt.updated.len(), 3);
    assert_eq!(
        step_receipt
            .updated
            .iter()
            .find(|updated| updated.entity_id == agent_id)
            .context("the S1 receipt advances the branch head")?
            .record_revision,
        RecordRevision(1)
    );
    assert_eq!(step_receipt.commit_ref.ledger_id, ledger_id);
    assert!(
        step_receipt.commit_ref.seq.0 > last_publish_seq,
        "the applied step appends after the publication"
    );
    assert_eq!(
        store.memory_intent_state(save_intent)?,
        Some("accepted".to_string())
    );
    let queued = store
        .committed_intent(save_intent)?
        .context("queued subject memory intent")?;
    assert_eq!(queued.kind, "memory");
    assert_eq!(queued.state, "ready");
    assert_eq!(queued.step_id, first_step_id);

    // 4. Restart and reopen the same database/CAS.
    drop(store);
    let store = Arc::new(Store::open(database.clone(), cas.clone())?);
    assert_eq!(store.ledger.ledger_id(), ledger_id);
    assert_metapackage_published(&store, agent_id, &published)?;

    let (restored, successor) = store.load_subject(agent_id)?;
    assert_eq!(restored.committed_step_no, 1);
    assert_eq!(restored.committed_step_id, Some(first_step_id));
    assert_eq!(restored.committed_at, step_receipt.commit_ref);
    assert_eq!(restored.next_step_id, successor_step_id);
    assert_eq!(successor.step_id, successor_step_id);
    assert_eq!(successor.state, SubjectStepState::Planned);
    let committed = store
        .get_step(&restored.branch_id, first_step_id)?
        .context("committed first step after reopen")?;
    assert_eq!(committed.state, SubjectStepState::Committed);
    assert_eq!(committed.commit_ref, Some(step_receipt.commit_ref));

    // 5. The reopened subject settles the queued memory exactly once, through
    // the same dispatcher/settler the product uses.
    store.start_subject_dispatcher()?;
    assert_eq!(store.settle_subject_memory()?, 1);
    let entry = store
        .memory_entry(save_intent)?
        .context("settled memory entry")?;
    assert_eq!(entry.agent_id, agent_id);
    assert_eq!(entry.content, FACT_TEXT);
    assert_eq!(entry.kind, "note");
    assert_eq!(entry.scope, MEMORY_SCOPE);
    assert!(!entry.deleted);
    assert_eq!(entry.record_revision, 0);
    assert_eq!(
        store.memory_intent_state(save_intent)?,
        Some("committed".to_string())
    );
    assert_eq!(
        store
            .committed_intent(save_intent)?
            .context("settled subject memory intent")?
            .state,
        "settled"
    );
    let settled_seq = entry.updated_commit_seq.context("entry commit seq")?;
    assert!(settled_seq > step_receipt.commit_ref.seq.0);
    assert_eq!(store.search_memory("beacon", 10)?.len(), 1);
    assert_eq!(store.list_memory(10)?.len(), 1);

    // 6. The subject evaluates forgetting: it records the fact as cognition
    // content, seals its evaluation evidence, and writes the projection stage
    // attitude with reason pointers. The stage changes default prominence, not
    // the content bytes.
    let fact_attitude = store.record_attitude_for_digest(
        agent_id,
        fact_object.clone(),
        AttitudeDraft::assertion(
            vec![ReasonPointer::source(
                published.source.content.object.digest.clone(),
                Some("retained research note".to_string()),
            )],
            AttitudeConfidence::Unknown,
        ),
    )?;
    assert_eq!(fact_attitude.attitude.reading(), ReasonReading::Fact);

    let evaluation_bytes = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "kind": "forgetting_evaluation",
        "unit_id": METAPACKAGE_UNIT,
        "member_id": METAPACKAGE_MEMBER_A,
        "evaluated_stage": "forgotten",
        "basis": "the subject records its own evaluation; the kernel supplies no wall-clock forgetting formula",
    }))?;
    let evaluation = store
        .cas
        .seal_object(
            &evaluation_bytes,
            "application/json",
            "cognition",
            "forgetting-evaluation",
            1,
        )?
        .object;
    let digests_before_forget = store.cas.object_digests()?;
    let projection_receipt = store.record_attitude_for_digest(
        agent_id,
        fact_object.clone(),
        AttitudeDraft::projection(
            ProjectionStage::Forgotten,
            vec![
                ReasonPointer::source(
                    evaluation.digest.clone(),
                    Some("forgetting evaluation".to_string()),
                ),
                ReasonPointer::content(published.body.content.object.digest.clone()),
            ],
            AttitudeConfidence::Unknown,
        ),
    )?;
    let projection = store
        .latest_attitude(agent_id, &fact_object.digest, Some("projection"))?
        .context("projection attitude for the forgotten fact")?;
    assert_eq!(
        projection.role,
        AttitudeRole::Projection(ProjectionStage::Forgotten)
    );
    assert_eq!(projection.created_commit, projection_receipt.commit);
    assert_eq!(projection.confidence, Some(AttitudeConfidence::Unknown));
    assert_eq!(
        projection.reading(),
        ReasonReading::Conclusion,
        "the reason list includes the metapackage body, so the subject's derivation participates"
    );
    let admission = store.reason_admission(&projection.reasons)?;
    assert_eq!(admission.len(), 2);
    assert!(
        admission
            .iter()
            .all(|report| report.admission == ReasonAdmission::PresentLocal),
        "both reason pointers resolve on this installation: {admission:?}"
    );
    assert!(projection_receipt.commit.seq.0 > settled_seq);

    // Hold the real shared CAS publication capability across the forget and
    // the retention audit. This is the same lease every Store mutation takes
    // while it seals and publishes references.
    let publication = StorePublication::acquire(cas.clone())?;

    // 7. Settle the semantic forget. The fact stops appearing in the default
    // projection; its row, text and provenance stay readable.
    let forget_intent = EntityId::new();
    let request = MemoryIntentRecord::accepted(
        forget_intent,
        agent_id,
        "forget",
        MEMORY_SCOPE,
        "",
        None,
        Some(save_intent),
    );
    store.admit_memory_intent(&request, None)?;
    assert_eq!(
        store.memory_intent_state(forget_intent)?,
        Some("accepted".to_string())
    );
    let forget_receipt = store.commit_memory_forget(forget_intent)?;
    assert_eq!(forget_receipt.updated.len(), 1);
    assert_eq!(forget_receipt.updated[0].entity_id, save_intent);
    assert_eq!(forget_receipt.updated[0].record_revision, RecordRevision(1));
    assert_eq!(forget_receipt.commit_ref.ledger_id, ledger_id);
    assert!(forget_receipt.commit_ref.seq.0 > projection_receipt.commit.seq.0);
    assert_eq!(
        store.memory_intent_state(forget_intent)?,
        Some("committed".to_string())
    );

    let forgotten = store
        .memory_entry(save_intent)?
        .context("a semantic forget keeps the row readable")?;
    assert!(forgotten.deleted);
    assert_eq!(forgotten.content, FACT_TEXT);
    assert_eq!(forgotten.record_revision, 1);
    assert!(store.search_memory("beacon", 10)?.is_empty());
    assert!(store.list_memory(10)?.is_empty());
    let retired = store.search_memory_including_retired("beacon", 10)?;
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].entry_id, save_intent);
    assert_eq!(retired[0].content, FACT_TEXT);

    // 8. GC retention: forgetting is not deletion. The CAS object set is
    // unchanged, every named object still reads back byte-for-byte, and the
    // retention inventory verifies the forgotten content and its closure.
    assert_eq!(
        store.cas.object_digests()?,
        digests_before_forget,
        "a semantic forget, archive move or projection stage must not delete CAS objects"
    );
    let verified = [
        (&published.source.content.object.digest, SOURCE_NOTE.as_bytes()),
        (&published.body.content.object.digest, published.body_bytes.as_slice()),
        (
            &published.members[0].2.content.object.digest,
            published.members[0].1.as_slice(),
        ),
        (
            &published.members[1].2.content.object.digest,
            published.members[1].1.as_slice(),
        ),
        (&fact_object.digest, FACT_TEXT.as_bytes()),
        (&evaluation.digest, evaluation_bytes.as_slice()),
    ];
    let snapshot = store.retained_object_snapshot(RetentionLimits::default())?;
    assert!(snapshot.complete, "retention gaps: {:?}", snapshot.gaps);
    assert!(snapshot.gaps.is_empty());
    assert_eq!(snapshot.ledger_id, ledger_id);
    assert!(
        snapshot.source_through.0 >= forget_receipt.commit_ref.seq.0,
        "the inventory covers the commit that settled the forget"
    );
    for &(digest, bytes) in &verified {
        assert_eq!(store.cas.read_object(digest)?, bytes);
        assert!(
            snapshot.verified_objects.contains(digest),
            "retention inventory must verify {digest}"
        );
    }
    assert_eq!(
        store.content_bytes(&fact_object.digest)?,
        FACT_TEXT.as_bytes(),
        "the ledger still reads the forgotten original"
    );

    // The independent reader path (no Store::open, no migrations) reaches the
    // same complete inventory and verifies the forgotten object.
    let inspected =
        Store::inspect_retained_objects(&database, &cas, RetentionLimits::default())?;
    assert!(
        inspected.complete,
        "independent retention gaps: {:?}",
        inspected.gaps
    );
    assert_eq!(inspected.ledger_id, ledger_id);
    assert!(inspected.verified_objects.contains(&fact_object.digest));

    // A live publication holds the content store's shared write lease.
    // Reclamation's exclusive fence must be refused for as long as that
    // custody lasts, so objects this history still names cannot be collected
    // out from under it; releasing the capability frees the fence.
    let refusal = CasWriteFence::acquire(&cas, 0)
        .expect_err("a live publication must keep the reclamation fence out");
    assert!(
        format!("{refusal:#}").contains("CAS_ROOT_OWNER_BUSY"),
        "the refusal says which custody blocked reclamation: {refusal:#}"
    );
    drop(publication);
    drop(store);
    drop(CasWriteFence::acquire(&cas, 0).expect("the write lease is released with the publisher"));

    // 9. A second restart proves the tombstone, the projection attitude, the
    // adoption attitudes and the retained bytes all survive together.
    let store = Store::open(database, cas)?;
    assert_eq!(store.ledger.ledger_id(), ledger_id);
    assert_metapackage_published(&store, agent_id, &published)?;
    let forgotten = store
        .memory_entry(save_intent)?
        .context("forgotten entry after the second restart")?;
    assert!(forgotten.deleted);
    assert_eq!(forgotten.content, FACT_TEXT);
    assert_eq!(forgotten.record_revision, 1);
    assert!(store.search_memory("beacon", 10)?.is_empty());
    assert_eq!(
        store.search_memory_including_retired("beacon", 10)?.len(),
        1
    );
    let projection = store
        .latest_attitude(agent_id, &fact_object.digest, Some("projection"))?
        .context("projection survives restart")?;
    assert_eq!(
        projection.role,
        AttitudeRole::Projection(ProjectionStage::Forgotten)
    );
    assert_eq!(projection.created_commit, projection_receipt.commit);
    assert_eq!(
        store.memory_intent_state(forget_intent)?,
        Some("committed".to_string())
    );
    assert_eq!(store.cas.object_digests()?, digests_before_forget);
    for &(digest, bytes) in &verified {
        assert_eq!(store.cas.read_object(digest)?, bytes);
    }
    let after_restart = store.retained_object_snapshot(RetentionLimits::default())?;
    assert!(
        after_restart.complete,
        "retention gaps after restart: {:?}",
        after_restart.gaps
    );
    assert!(after_restart.verified_objects.contains(&fact_object.digest));
    assert_eq!(after_restart.ledger_id, ledger_id);
    Ok(())
}
