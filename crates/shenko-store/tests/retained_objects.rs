//! One bounded read refuses an oversized carrier, then a full read of the
//! same committed original finds its embedded JSON reference without writes.
use anyhow::{Context, Result};
use shenko_store::{RetentionLimits, Store};
use shenko_types::{ids::EntityId, store::{RawSession, RawSessionEvent, WriteBatch}};

#[test]
fn bounded_retention_refusal_keeps_the_original_rows_and_nested_cas_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("shenko.db");
    let cas = directory.path().join("cas");
    let store = Store::open(database.clone(), cas.clone())?;
    let leaf = store.cas.seal_object(b"original retained output", "text/plain", "retention-test", "raw", 1)?.object;
    let envelope = store.cas.seal_object(
        &serde_json::to_vec(&serde_json::json!({"cell":{"type":"text","value":serde_json::to_string(&leaf)?}}))?,
        "application/json", "retention-test", "row-carrier", 1,
    )?.object;
    let session = EntityId::new();
    let event = EntityId::new();
    let original = serde_json::to_string(&serde_json::json!({"evidence":&envelope}))?;
    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id: session, agent_id: EntityId::new(), started_at: "2026-11-16T00:00:00Z".into(), data: "{}".into(),
    });
    batch.session_event_upserts.push(RawSessionEvent {
        event_id: event, session_id: session, seq: 0, event_type: "retention_observation".into(),
        role: "system".into(), data: original.clone(),
    });
    store.commit(batch)?;
    let through = store.current_commit_seq()?;
    let objects = store.cas.object_digests()?;
    let limits = RetentionLimits { row_bytes: 64, ..RetentionLimits::default() };
    let refused = store.retained_object_snapshot(limits)?;
    assert!(!refused.complete);
    assert!(refused.gaps.iter().any(|gap| gap.kind == "row_size_exceeded"));
    assert_eq!(store.current_commit_seq()?, through);
    assert_eq!(store.session_events_range(session, 0, i64::MAX)?.first()
        .context("original event was lost")?.data, original);
    let paths = [database.clone(), database.with_extension("db-wal"), database.with_extension("db-shm")];
    let before = paths.iter().map(|path| if path.exists() { std::fs::read(path).map(Some) } else { Ok(None) })
        .collect::<std::io::Result<Vec<_>>>()?;
    let report = Store::inspect_retained_objects(&database, &cas, RetentionLimits::default())?;
    assert!(report.complete, "{:?}", report.gaps);
    assert_eq!(report.ledger_id, store.ledger.ledger_id());
    assert_eq!(report.source_through.0, through);
    assert!(report.verified_objects.contains(&envelope.digest));
    assert!(report.verified_objects.contains(&leaf.digest));
    for (path, bytes) in paths.iter().zip(before) {
        let after = if path.exists() { Some(std::fs::read(path)?) } else { None };
        assert_eq!(after, bytes, "audit changed {}", path.display());
    }
    assert_eq!(store.cas.object_digests()?, objects);
    assert_eq!(store.cas.read_object(&leaf.digest)?, b"original retained output");
    assert_eq!(store.current_commit_seq()?, through);
    Ok(())
}

#[test]
fn source_descriptions_are_typed_only_in_attitude_reasons() -> Result<()> {
    use shenko_store::{AttitudeDraft, ContentWrite};
    use shenko_types::{cognition::{AttitudeConfidence, ReasonPointer}, object::Digest};

    let directory = tempfile::tempdir()?;
    let database = directory.path().join("shenko.db");
    let store = Store::open(database.clone(), directory.path().join("cas"))?;
    let source = store.cas.seal_object(b"original source", "text/plain", "retention-test", "raw", 1)?.object;
    let perception = store.cas.seal_object(b"source interpretation", "text/plain", "retention-test", "raw", 1)?.object;
    let mut missing = source.clone();
    missing.digest = Digest::blake3(b"absent required content");
    let reason = ReasonPointer::source(source.digest.clone(), Some(serde_json::to_string(&missing)?))
        .with_perception(perception.digest.clone());
    let recorded = store.record_content_attitude(
        EntityId::new(), ContentWrite::text("remembered fact"),
        AttitudeDraft::assertion(vec![reason], AttitudeConfidence::Unknown),
    )?;
    let report = store.retained_object_snapshot(RetentionLimits::default())?;
    assert!(report.complete, "{:?}", report.gaps);
    assert!(report.verified_objects.contains(&source.digest));
    assert!(report.verified_objects.contains(&perception.digest));
    assert!(!report.references.iter().any(|reference| reference.digest == missing.digest),
        "a SourceRef locator is a description, even when its text looks like JSON");

    // The same path spelling inside an arbitrary event is not a typed reason.
    let session = EntityId::new();
    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id: session, agent_id: EntityId::new(), started_at: "2026-11-23T00:00:00Z".into(), data: "{}".into(),
    });
    batch.session_event_upserts.push(RawSessionEvent {
        event_id: EntityId::new(), session_id: session, seq: 0,
        event_type: "untyped_source_descriptor".into(), role: "system".into(),
        data: serde_json::to_string(&ReasonPointer::source(source.digest.clone(), Some("source description".into())))?,
    });
    store.commit(batch)?;
    let untyped = store.retained_object_snapshot(RetentionLimits::default())?;
    assert!(!untyped.complete);
    assert!(untyped.gaps.iter().any(|gap| gap.kind == "unclassified_artifact_location"));

    // Simulate a persisted future carrier with an extra required edge. The
    // typed reader must refuse its unknown shape and still find that edge.
    let future = serde_json::json!([{
        "termination": {"source": {
            "digest": source.digest, "locator": "source description", "extra": missing,
        }},
        "via_perception": perception.digest,
    }]);
    rusqlite::Connection::open(&database)?.execute(
        "UPDATE cognition_attitudes SET reasons_json=?1 WHERE attitude_id=?2",
        rusqlite::params![serde_json::to_string(&future)?, recorded.attitude.attitude_id.to_string()],
    )?;
    let unsupported = store.retained_object_snapshot(RetentionLimits::default())?;
    assert!(!unsupported.complete);
    assert!(unsupported.gaps.iter().any(|gap| gap.kind == "invalid_source_reference"));
    assert!(unsupported.references.iter().any(|reference| reference.required && reference.digest == missing.digest));
    assert!(unsupported.gaps.iter().any(|gap|
        gap.kind == "referenced_object_unreadable" && gap.digest.as_ref() == Some(&missing.digest)));
    Ok(())
}
