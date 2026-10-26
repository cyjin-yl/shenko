use shenko_store::{SessionEventFilter, Store, StoredSessionEvent};
use shenko_types::ids::EntityId;
use shenko_types::store::{RawSession, RawSessionEvent, WriteBatch};

fn session(session_id: EntityId, agent_id: EntityId) -> RawSession {
    RawSession {
        session_id,
        agent_id,
        started_at: "2026-10-26T00:00:00Z".into(),
        data: "{}".into(),
    }
}

fn event(
    event_id: EntityId,
    session_id: EntityId,
    seq: i64,
    event_type: &str,
    role: &str,
    data: serde_json::Value,
) -> RawSessionEvent {
    RawSessionEvent {
        event_id,
        session_id,
        seq,
        event_type: event_type.into(),
        role: role.into(),
        data: data.to_string(),
    }
}

fn ids(events: &[StoredSessionEvent]) -> Vec<EntityId> {
    events.iter().map(|event| event.event_id).collect()
}

#[test]
fn raw_history_cjk_mixed_failure_filters_and_refs_survive_reopen() {
    let dir = std::env::temp_dir().join(format!("shenko-history-search-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
    let (session_id, agent_id) = (EntityId::new(), EntityId::new());
    let (one, two, three, failed, mixed) = (
        EntityId::new(),
        EntityId::new(),
        EntityId::new(),
        EntityId::new(),
        EntityId::new(),
    );
    let failure_data = serde_json::json!({
        "v": 1,
        "text": "工具失败 failed E_FAIL",
        "outcome": "failed",
        "detail": {"ok": false}
    })
    .to_string();
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    assert!(store.capabilities.fts5_available);
    let mut batch = WriteBatch::new();
    batch
        .raw_session_upserts
        .push(session(session_id, agent_id));
    batch.session_event_upserts = vec![
        event(
            one,
            session_id,
            1,
            "user",
            "user",
            serde_json::json!({"text":"深"}),
        ),
        event(
            two,
            session_id,
            2,
            "user",
            "user",
            serde_json::json!({"text":"深刻"}),
        ),
        event(
            three,
            session_id,
            3,
            "user",
            "user",
            serde_json::json!({"text":"深刻度"}),
        ),
        RawSessionEvent {
            data: failure_data.clone(),
            ..event(
                failed,
                session_id,
                4,
                "tool_result",
                "tool",
                serde_json::json!({}),
            )
        },
        event(
            mixed,
            session_id,
            5,
            "user",
            "user",
            serde_json::json!({"text":"Qwen3.8-27B 历史 Alpha"}),
        ),
    ];
    store.commit(batch).unwrap();
    let other_session = EntityId::new();
    let mut other = WriteBatch::new();
    other
        .raw_session_upserts
        .push(session(other_session, agent_id));
    other.session_event_upserts.push(event(
        EntityId::new(),
        other_session,
        1,
        "user",
        "user",
        serde_json::json!({"text":"深刻其他"}),
    ));
    store.commit(other).unwrap();

    let search = |query: &str| {
        store
            .search_session_events(session_id, query, &SessionEventFilter::default())
            .unwrap()
    };
    assert_eq!(ids(&search("深")), vec![one, two, three]);
    assert_eq!(ids(&search("刻")), vec![two, three]);
    assert_eq!(ids(&search("深刻度")), vec![three]);
    assert_eq!(ids(&search("Qwen3.8")), vec![mixed]);
    assert_eq!(ids(&search("历史 Alpha")), vec![mixed]);
    assert_eq!(ids(&search("E_FAIL")), vec![failed]);
    let tool_only = SessionEventFilter {
        event_types: vec!["tool_result".into()],
        roles: vec!["tool".into()],
        ..Default::default()
    };
    let failures = store
        .search_session_events(session_id, "失败", &tool_only)
        .unwrap();
    assert_eq!(ids(&failures), vec![failed]);
    assert_eq!(failures[0].data, failure_data);
    assert!(!failures[0].data.contains("\"outcome\":\"success\""));
    let page = store
        .list_session_events(
            session_id,
            &SessionEventFilter {
                from_seq: 2,
                to_seq: 4,
                limit: 2,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(ids(&page), vec![two, three]);
    let original = store.session_event(failed).unwrap().unwrap();
    assert_eq!(original.seq, 4);
    assert_eq!(original.data, failure_data);
    drop(store);

    let reopened = Store::open(db, cas).unwrap();
    assert_eq!(
        ids(&reopened
            .search_session_events(session_id, "深刻", &SessionEventFilter::default())
            .unwrap()),
        vec![two, three]
    );
    let reread = reopened.session_event(failed).unwrap().unwrap();
    assert_eq!(reread, original);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn raw_history_fts_rebuilds_from_committed_events() {
    let dir = std::env::temp_dir().join(format!("shenko-history-rebuild-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let (db, cas) = (dir.join("shenko.db"), dir.join("cas"));
    let (session_id, agent_id, event_id) = (EntityId::new(), EntityId::new(), EntityId::new());
    let store = Store::open(db.clone(), cas.clone()).unwrap();
    let mut batch = WriteBatch::new();
    batch
        .raw_session_upserts
        .push(session(session_id, agent_id));
    batch.session_event_upserts.push(event(
        event_id,
        session_id,
        1,
        "user",
        "user",
        serde_json::json!({"text":"ReconstructibleToken remains in the raw row"}),
    ));
    store.commit(batch).unwrap();
    let original = store.session_event(event_id).unwrap().unwrap();
    drop(store);

    // Damage only the derived index. The committed event is untouched.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("DROP TABLE session_events_fts")
        .unwrap();
    let reopened = Store::open(db.clone(), cas).unwrap();
    assert_eq!(
        ids(&reopened
            .search_session_events(
                session_id,
                "ReconstructibleToken",
                &SessionEventFilter::default(),
            )
            .unwrap()),
        vec![event_id]
    );
    let indexed = rusqlite::Connection::open(&db).unwrap().query_row(
        "SELECT count(*) FROM session_events_fts WHERE session_events_fts MATCH 'ReconstructibleToken'",
        [],
        |row| row.get::<_, i64>(0),
    ).unwrap();
    assert_eq!(
        indexed, 1,
        "the reopened FTS index, not only LIKE fallback, contains the row"
    );
    assert_eq!(reopened.session_event(event_id).unwrap().unwrap(), original);
    drop(reopened);
    let _ = std::fs::remove_dir_all(dir);
}
