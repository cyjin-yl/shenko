//! Evidence-based forgetting — SPEC §51.7 bounded cycle cases.
//!
//! These are store-level regressions: a referenced publication must survive an
//! expired evaluation, an unreferenced one is unpublished with a tombstone and
//! a receipt, decay needs a measured active clock, and missing evidence stays
//! unevaluated instead of being guessed away.

use std::path::PathBuf;

use shenko_store::atoms::{
    ContentWrite, COGNITION_CONTENT_CODEC, COGNITION_CONTENT_CODEC_MAJOR,
    COGNITION_CONTENT_NAMESPACE,
};
use shenko_store::forgetting::{
    ForgettingClock, ForgettingDecisionKind, ForgettingEvidence, ForgettingEvidenceKind,
    ForgettingPolicy,
};
use shenko_store::{AttitudeDraft, RetentionLimits, Store};
use shenko_types::cognition::{AttitudeConfidence, ReasonPointer};
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

fn tmp_store(tag: &str) -> Store {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "shenko-forgetting-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Store::open(dir.join("forgetting.db"), dir.join("cas")).unwrap()
}

/// Register content with no attitude and no other durable carrier. This is the
/// only shape the cycle may ever physically unpublish.
fn register_unreferenced(store: &Store, text: &str) -> ObjectRef {
    let sealed = store
        .cas
        .seal_object(
            text.as_bytes(),
            "text/plain",
            COGNITION_CONTENT_NAMESPACE,
            COGNITION_CONTENT_CODEC,
            COGNITION_CONTENT_CODEC_MAJOR,
        )
        .unwrap();
    store.register_content(&sealed.object).unwrap();
    sealed.object
}

fn policy(decay_after_active_ms: i64, release: bool) -> ForgettingPolicy {
    ForgettingPolicy {
        decay_after_active_ms,
        max_candidates: 32,
        max_evidence_per_candidate: 32,
        fence_wait_ms: 5_000,
        reference_limits: RetentionLimits::default(),
        release_unreferenced_bytes: release,
    }
}

fn exact_tombstones(store: &Store, digest: &Digest) -> Vec<shenko_store::research::RetentionTombstone> {
    store
        .retention_tombstones(Some("object"))
        .unwrap()
        .into_iter()
        .filter(|tombstone| tombstone.target_id == digest.0)
        .collect()
}

#[test]
fn a_referenced_publication_is_retained_not_unpublished() {
    let store = tmp_store("referenced");
    let agent = EntityId::new();
    let recorded = store
        .record_content_attitude(
            agent,
            ContentWrite::text("a fact the subject asserted"),
            AttitudeDraft::assertion(
                vec![ReasonPointer::source(Digest::blake3(b"supporting source"), None)],
                AttitudeConfidence::Unknown,
            ),
        )
        .unwrap();
    let digest = recorded.content.digest().clone();
    store
        .record_forgetting_evidence(&ForgettingEvidence::expiry(agent, digest.clone(), 0, 10, 1))
        .unwrap();

    let cycle = store
        .run_forgetting_cycle(agent, &policy(1_000, true), ForgettingClock::new(100, 2).unwrap())
        .unwrap();

    assert_eq!(cycle.candidates, 1);
    assert_eq!(cycle.retained, 1, "{:?}", cycle);
    assert_eq!(cycle.forgotten, 0);
    let outcome = cycle.outcomes.first().expect("the retained decision is reported");
    assert_eq!(outcome.decision, ForgettingDecisionKind::Retained);
    assert!(
        !outcome.references.is_empty(),
        "the attitude must be reported as the retention exception"
    );
    assert!(store.content(&digest).unwrap().is_some(), "publication stays");
    assert!(store.cas.object_exists(&digest), "bytes stay");
    assert!(exact_tombstones(&store, &digest).is_empty());

    let receipts = store
        .forgetting_receipts(agent, Some(&digest), 10)
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].decision, ForgettingDecisionKind::Retained);
    assert!(
        receipts[0].references_json.contains("cognition_attitudes"),
        "the receipt records the actual exception origin: {}",
        receipts[0].references_json
    );
}

#[test]
fn an_unreferenced_expired_publication_is_unpublished_with_tombstone_and_receipt() {
    let store = tmp_store("unreferenced");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "an unremembered scratch publication");
    let digest = object.digest.clone();
    store
        .record_forgetting_evidence(&ForgettingEvidence::expiry(agent, digest.clone(), 0, 10, 1))
        .unwrap();

    let cycle = store
        .run_forgetting_cycle(agent, &policy(1_000, true), ForgettingClock::new(100, 2).unwrap())
        .unwrap();

    assert_eq!(cycle.candidates, 1);
    assert_eq!(cycle.forgotten, 1, "{:?}", cycle);
    assert_eq!(cycle.retained, 0);
    let outcome = cycle.outcomes.first().expect("the forgotten decision is reported");
    assert_eq!(outcome.decision, ForgettingDecisionKind::Forgotten);
    assert!(outcome.object_released);
    assert!(store.content(&digest).unwrap().is_none(), "registration is gone");
    assert!(!store.cas.object_exists(&digest), "unreferenced bytes are released");

    let tombstones = exact_tombstones(&store, &digest);
    assert_eq!(tombstones.len(), 1);
    assert_eq!(tombstones[0].scope, "cognition_content");
    let receipts = store
        .forgetting_receipts(agent, Some(&digest), 10)
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].decision, ForgettingDecisionKind::Forgotten);
}

#[test]
fn a_legacy_mapping_is_a_retention_exception() {
    let store = tmp_store("legacy-mapping");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "bytes shared with a historical row");
    let digest = object.digest.clone();
    let (mapped, _) = store
        .register_legacy_content("historical_notes", "row-1", b"bytes shared with a historical row", "text/plain")
        .unwrap();
    assert_eq!(mapped.digest(), &digest, "the mapping names the same bytes");
    store
        .record_forgetting_evidence(&ForgettingEvidence::expiry(agent, digest.clone(), 0, 10, 1))
        .unwrap();

    let cycle = store
        .run_forgetting_cycle(agent, &policy(1_000, true), ForgettingClock::new(100, 2).unwrap())
        .unwrap();

    assert_eq!(cycle.retained, 1, "{:?}", cycle);
    assert_eq!(cycle.forgotten, 0);
    assert!(store.content(&digest).unwrap().is_some());
    assert!(store.cas.object_exists(&digest));
    let receipts = store
        .forgetting_receipts(agent, Some(&digest), 10)
        .unwrap();
    assert!(
        receipts[0].references_json.contains("cognition_legacy_content"),
        "the mapping is reported as the exception: {}",
        receipts[0].references_json
    );
}

#[test]
fn decay_requires_a_refresh_and_a_measured_active_clock() {
    let store = tmp_store("decay");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "content refreshed once");
    let digest = object.digest.clone();
    store
        .record_forgetting_evidence(
            &ForgettingEvidence::refresh(
                agent,
                digest.clone(),
                ForgettingEvidenceKind::Use,
                10,
                1,
            )
            .unwrap(),
        )
        .unwrap();

    let fresh = store
        .run_forgetting_cycle(agent, &policy(50, true), ForgettingClock::new(59, 2).unwrap())
        .unwrap();
    assert_eq!(fresh.kept, 1, "{:?}", fresh);
    assert_eq!(fresh.forgotten, 0);
    assert!(store.content(&digest).unwrap().is_some());

    let stale = store
        .run_forgetting_cycle(agent, &policy(50, true), ForgettingClock::new(60, 3).unwrap())
        .unwrap();
    assert_eq!(stale.forgotten, 1, "{:?}", stale);
    assert!(store.content(&digest).unwrap().is_none());
    assert!(!store.cas.object_exists(&digest));
}

#[test]
fn a_no_release_policy_unpublishes_the_registration_but_keeps_the_bytes() {
    let store = tmp_store("no-release");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "bytes kept by policy");
    let digest = object.digest.clone();
    store
        .record_forgetting_evidence(&ForgettingEvidence::expiry(agent, digest.clone(), 0, 10, 1))
        .unwrap();

    let cycle = store
        .run_forgetting_cycle(agent, &policy(1_000, false), ForgettingClock::new(100, 2).unwrap())
        .unwrap();
    assert_eq!(cycle.forgotten, 1, "{:?}", cycle);
    let outcome = cycle.outcomes.first().expect("the forgotten decision is reported");
    assert!(!outcome.object_released);
    assert!(store.content(&digest).unwrap().is_none());
    assert!(store.cas.object_exists(&digest), "policy did not authorise byte release");
    assert_eq!(exact_tombstones(&store, &digest).len(), 1);
}

#[test]
fn content_without_evidence_is_unevaluated_and_untouched() {
    let store = tmp_store("unevaluated");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "no evidence was ever recorded");
    let digest = object.digest.clone();

    let cycle = store
        .run_forgetting_cycle(
            agent,
            &policy(1, true),
            ForgettingClock::new(1_000_000, 2).unwrap(),
        )
        .unwrap();

    assert_eq!(cycle.candidates, 1);
    assert_eq!(cycle.unevaluated, 1, "{:?}", cycle);
    assert_eq!(cycle.forgotten, 0);
    assert_eq!(cycle.retained, 0);
    let outcome = cycle.outcomes.first().expect("the unevaluated decision is reported");
    assert_eq!(outcome.decision, ForgettingDecisionKind::Unevaluated);
    assert!(store.content(&digest).unwrap().is_some());
    assert!(store.cas.object_exists(&digest));
    assert!(exact_tombstones(&store, &digest).is_empty());

    let receipts = store
        .forgetting_receipts(agent, Some(&digest), 10)
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].decision, ForgettingDecisionKind::Unevaluated);
}

#[test]
fn a_clock_that_precedes_its_evidence_claims_no_decay() {
    let store = tmp_store("clock-regression");
    let agent = EntityId::new();
    let object = register_unreferenced(&store, "used at active time ten");
    let digest = object.digest.clone();
    store
        .record_forgetting_evidence(
            &ForgettingEvidence::refresh(
                agent,
                digest.clone(),
                ForgettingEvidenceKind::Reference,
                10,
                1,
            )
            .unwrap(),
        )
        .unwrap();

    let cycle = store
        .run_forgetting_cycle(agent, &policy(1, true), ForgettingClock::new(5, 2).unwrap())
        .unwrap();
    assert_eq!(cycle.unevaluated, 1, "{:?}", cycle);
    assert_eq!(cycle.forgotten, 0);
    assert!(store.content(&digest).unwrap().is_some());
}
