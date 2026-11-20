//! Scoped recall must not lose older facts behind a global candidate prefix.
use std::sync::Arc;

use shenko_memory::{MemoryStore, backend::RecallQuery};
use shenko_store::{MemoryEntry, Store};
use shenko_types::ids::EntityId;

#[test]
fn newer_unrelated_facts_do_not_crowd_out_scoped_recall() {
    let root = std::path::PathBuf::from(
        std::env::var_os("SHENKO_NATIVE_ARTIFACT_ROOT")
            .expect("parent must provide a persistent native artifact root"),
    ).join(format!("scoped-recall-{}", EntityId::new()));
    std::fs::create_dir(&root).unwrap();
    let store = Arc::new(Store::open(root.join("ledger.db"), root.join("cas")).unwrap());
    assert!(store.capabilities.fts5_available, "exercise the actual FTS path");
    let agent = EntityId::new();
    let other_agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));
    let mut expected = Vec::new();
    for _ in 0..2 {
        let mut entry = MemoryEntry::new(agent, "scopedneedle 精确命中");
        entry.scope = "project".into();
        entry.kind = "commitment".into();
        store.insert_memory_entry(&entry).unwrap();
        expected.push(entry.entry_id);
    }
    // More than 1024 newer rows match the text but each violates exactly
    // one requested selector; a global candidate prefix loses the old facts.
    let mut conn = rusqlite::Connection::open(root.join("ledger.db")).unwrap();
    let tx = conn.transaction().unwrap();
    for index in 0..1100 {
        let (owner, kind, scope) = match index % 3 {
            0 => (other_agent, "commitment", "project"),
            1 => (agent, "note", "project"),
            _ => (agent, "commitment", "personal"),
        };
        tx.execute(
            "INSERT INTO memory_entries(entry_id,agent_id,kind,scope,content,created_at_ms)
             VALUES(?1,?2,?3,?4,'scopedneedle 精确命中',?5)",
            rusqlite::params![EntityId::new().to_string(), owner.to_string(), kind, scope,
                chrono::Utc::now().timestamp_millis() + index],
        ).unwrap();
    }
    tx.commit().unwrap();
    let mut retired = MemoryEntry::new(agent, "scopedneedle 精确命中");
    retired.scope = "project".into();
    retired.kind = "commitment".into();
    retired.deleted = true;
    store.insert_memory_entry(&retired).unwrap();

    // ASCII uses FTS rank, CJK uses literal LIKE, and empty text uses listing.
    // All three must apply the same ownership/kind/scope/retirement boundary.
    for text in ["scopedneedle", "精确命中", ""] {
        let mut query = RecallQuery::new(text);
        query.knowledge_kinds = vec!["commitment".into()];
        query.scopes = vec!["project".into()];
        query.limit = 2;
        let hits = memory.recall(&query).unwrap();
        let mut ids = hits.iter().map(|hit| hit.entry.entry_id).collect::<Vec<_>>();
        ids.sort();
        let mut wanted = expected.clone();
        wanted.sort();
        assert_eq!(ids, wanted, "query {text:?}");
        query.limit = 1;
        let limited = memory.recall(&query).unwrap();
        assert_eq!(limited.len(), 1, "query {text:?}");
        assert!(expected.contains(&limited[0].entry.entry_id));
        query.limit = 3;
        query.include_retired = true;
        let with_retired = memory.recall(&query).unwrap();
        assert_eq!(with_retired.len(), 3, "query {text:?}");
        assert!(with_retired.iter().any(|hit| hit.entry.entry_id == retired.entry_id));
    }
    // Readback keeps its established id-based access to retired content.
    assert!(memory.read(retired.entry_id).unwrap().unwrap().deleted);
}
