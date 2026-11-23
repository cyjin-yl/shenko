//! The default recall projection of a revised memory fact.
//!
//! SPEC §51.1/§51.4 and docs/specs/memory.md §2: a revision keeps every
//! version's bytes. A default recall shows the older revisions only as their
//! count plus the latest modification time, while the full text stays
//! reachable through the explicit lineage/read paths. UNRUN: the parent runs
//! this in the shared development container; this packet ran no build or test.
use std::sync::Arc;

use shenko_memory::{MemoryStore, backend::RecallQuery};
use shenko_store::{MemoryEntry, Store};
use shenko_types::ids::EntityId;

fn root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "shenko-memory-revision-{name}-{}",
        EntityId::new()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn a_revised_fact_shows_old_count_and_latest_time_not_old_text() {
    let root = root("projection");
    let store = Arc::new(Store::open(root.join("ledger.db"), root.join("cas")).unwrap());
    let agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));

    // A three-revision chain written through the ordinary entry commit.
    let mut first = MemoryEntry::new(agent, "修订面测试 第一版正文");
    first.created_at_ms = 1_000;
    let mut second = MemoryEntry::new(agent, "修订面测试 第二版正文");
    second.created_at_ms = 2_000;
    let mut third = MemoryEntry::new(agent, "修订面测试 第三版正文");
    third.created_at_ms = 3_000;
    first.superseded_by = Some(second.entry_id);
    second.supersedes = Some(first.entry_id);
    second.superseded_by = Some(third.entry_id);
    third.supersedes = Some(second.entry_id);
    store.insert_memory_entry(&first).unwrap();
    store.insert_memory_entry(&second).unwrap();
    store.insert_memory_entry(&third).unwrap();

    let mut query = RecallQuery::new("修订面测试");
    query.limit = 10;
    let hits = memory.recall(&query).unwrap();
    assert_eq!(hits.len(), 1, "default recall surfaces the live head only");
    let hit = &hits[0];
    assert_eq!(hit.entry.entry_id, third.entry_id);
    let revision = hit
        .revision
        .as_ref()
        .expect("a revised fact carries its projection");
    assert_eq!(revision.latest_entry_id, third.entry_id);
    assert_eq!(revision.old_version_count, 2);
    assert_eq!(revision.latest_modified_at_ms, 3_000);

    // The projection is display data: it never rewrites or deletes a
    // revision, and the original texts stay reachable through lineage.
    let lineage = store.memory_supersession_lineage(first.entry_id).unwrap();
    assert_eq!(lineage.len(), 3);
    assert_eq!(lineage[0].content, "修订面测试 第一版正文");
    assert_eq!(lineage[1].content, "修订面测试 第二版正文");
    assert_eq!(lineage[2].content, "修订面测试 第三版正文");

    // A fact with one revision reports no projection instead of a zero count.
    let mut single = MemoryEntry::new(agent, "只此一版 没有历史");
    single.created_at_ms = 4_000;
    store.insert_memory_entry(&single).unwrap();
    let hits = memory.recall(&RecallQuery::new("只此一版")).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].revision.is_none());

    // include_retired keeps every revision with the same projection; an older
    // statement is still not promoted back to current.
    query.include_retired = true;
    let hits = memory.recall(&query).unwrap();
    assert_eq!(hits.len(), 3);
    for hit in &hits {
        let revision = hit
            .revision
            .as_ref()
            .expect("retained revisions keep the projection");
        assert_eq!(revision.latest_entry_id, third.entry_id);
        assert_eq!(revision.old_version_count, 2);
    }
    let _ = std::fs::remove_dir_all(&root);
}
