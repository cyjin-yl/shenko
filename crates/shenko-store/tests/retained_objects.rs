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
