//! Raw session parent rows and their events commit through one WriteBatch:
//! a replay is a no-op, a different fact at a taken seq rolls the batch back.
use std::path::Path;

use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};

fn batch(session: EntityId, agent: EntityId, events: &[(EntityId, i64, &str)]) -> WriteBatch {
    let mut batch = WriteBatch::new();
    batch.raw_session_upserts.push(RawSession {
        session_id: session,
        agent_id: agent,
        started_at: "2026-10-25T00:00:00Z".into(),
        data: "{}".into(),
    });
    for (event_id, seq, text) in events {
        batch.session_event_upserts.push(RawSessionEvent {
            event_id: *event_id,
            session_id: session,
            seq: *seq,
            event_type: "user".into(),
            role: "user".into(),
            data: serde_json::json!({ "v": 1, "text": text }).to_string(),
        });
    }
    batch
}

/// (event_id, seq, data) rows of a session, read straight from SQLite.
fn rows(db: &Path, session: EntityId) -> Vec<(String, i64, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn
        .prepare("SELECT event_id, seq, data FROM session_events WHERE session_id = ?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([session.to_string()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn session_exists(db: &Path, session: EntityId) -> bool {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM raw_sessions WHERE session_id = ?1", [session.to_string()], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
        == 1
}

#[test]
fn session_and_events_commit_together_replay_is_noop_and_seq_conflict_rolls_back() {
    let dir = std::env::temp_dir().join(format!("shenko-raw-session-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
    let (session, agent) = (EntityId::new(), EntityId::new());
    let (first, second) = (EntityId::new(), EntityId::new());

    // Empty store: one batch creates the parent row and its events.
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    store.commit(batch(session, agent, &[(first, 0, "hello"), (second, 1, "again")])).unwrap();
    // Replaying the same facts is idempotent.
    store.commit(batch(session, agent, &[(first, 0, "hello"), (second, 1, "again")])).unwrap();

    // A different event at a taken seq, together with a brand-new session in
    // the same batch: conflict, and nothing from the batch lands.
    let other_session = EntityId::new();
    let mut conflicting = batch(session, agent, &[(EntityId::new(), 1, "rewritten")]);
    conflicting.raw_session_upserts.push(RawSession {
        session_id: other_session,
        agent_id: agent,
        started_at: "2026-10-25T00:00:01Z".into(),
        data: "{}".into(),
    });
    assert!(store.commit(conflicting).is_err());
    drop(store);

    let events = rows(&db, session);
    let ids: Vec<_> = events.iter().map(|(id, _, _)| id.clone()).collect();
    assert_eq!(ids, vec![first.to_string(), second.to_string()]);
    assert!(events[1].2.contains("again"), "seq 1 keeps its original event");
    assert!(!session_exists(&db, other_session), "rolled-back session row never landed");

    // A different agent claiming the existing session is refused too.
    let store = Store::open(db.clone(), cas).unwrap();
    assert!(store.commit(batch(session, EntityId::new(), &[])).is_err());
    drop(store);
    assert_eq!(rows(&db, session).len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}
