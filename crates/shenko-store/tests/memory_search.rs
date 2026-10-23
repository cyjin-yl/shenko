//! Memory persistence + search (SPEC §12.2 intent→committed, §12.3 中文检索).
use shenko_store::{MemoryEntry, Store};
use shenko_types::ids::EntityId;
use std::path::PathBuf;

fn tmp_store(tag: &str) -> Store {
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "shenko-mem-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Store::open(dir.join("memory.db"), dir.join("cas")).unwrap()
}

#[test]
fn chinese_substring_search_returns_entry() {
    let store = tmp_store("cjk");
    let agent = EntityId::new();
    let mut entry = MemoryEntry::new(agent, "我的深刻记忆：主体持续思考，不因空闲停止");
    entry.scope = "personal".to_string();
    store.insert_memory_entry(&entry).unwrap();

    // "深刻" is a substring, not an FTS unicode61 token: this must still work.
    let hits = store.search_memory("深刻", 10).unwrap();
    assert_eq!(hits.len(), 1, "CJK substring search should find the entry");
    assert_eq!(hits[0].entry_id, entry.entry_id);
    assert!(hits[0].content.contains("深刻"));

    assert!(store.search_memory("不存在的短语", 10).unwrap().is_empty());
    assert_eq!(store.list_memory(10).unwrap().len(), 1);
}

#[test]
fn ascii_search_works_with_fts_or_like_fallback() {
    let store = tmp_store("ascii");
    let agent = EntityId::new();
    let entry = MemoryEntry::new(agent, "the quick brown fox remembers the quiet morning");
    store.insert_memory_entry(&entry).unwrap();

    let hits = store.search_memory("quick", 5).unwrap();
    assert_eq!(hits.len(), 1, "ASCII search should find the entry");
    eprintln!(
        "ascii search exercised {} path (fts5_available={})",
        if store.capabilities.fts5_available { "FTS5-or-like" } else { "LIKE-fallback" },
        store.capabilities.fts5_available
    );
}

#[test]
fn insert_rejects_nothing_and_supports_scope_limit() {
    let store = tmp_store("scope");
    let agent = EntityId::new();
    for i in 0..3 {
        let mut e = MemoryEntry::new(agent, format!("entry {i}"));
        e.scope = if i == 0 { "shared".to_string() } else { "personal".to_string() };
        store.insert_memory_entry(&e).unwrap();
    }
    let latest = store.list_memory(2).unwrap();
    assert_eq!(latest.len(), 2);
    assert_eq!(latest[0].content, "entry 2");
}
