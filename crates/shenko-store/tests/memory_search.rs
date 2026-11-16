//! Memory persistence + search (docs/specs/memory.md#spec-12-2 intent→committed, docs/specs/context-policies.md#spec-12-3 中文检索).
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

/// The entry writer persists the supplied `deleted` flag: re-writing an
/// unchanged forgotten entry must keep its tombstone instead of resurrecting
/// it in search/list.
#[test]
fn unchanged_tombstone_rewrite_stays_hidden() {
    let store = tmp_store("tombstone");
    let agent = EntityId::new();

    // Forge a genuinely forgotten entry through the real forget path.
    let entry = MemoryEntry::new(agent, "被遗忘后仍要保留原文");
    store.insert_memory_entry(&entry).unwrap();
    let intent = EntityId::new();
    let record = shenko_store::MemoryIntentRecord::accepted(
        intent, agent, "forget", "personal", "", None, Some(entry.entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    store.commit_memory_forget(intent).unwrap();
    let forgotten = store.memory_entry(entry.entry_id).unwrap().unwrap();
    assert!(forgotten.deleted);

    // Passing the unchanged read-back entry to the writer must not resurrect it.
    store.insert_memory_entry(&forgotten).unwrap();
    let kept = store.memory_entry(entry.entry_id).unwrap().unwrap();
    assert!(kept.deleted, "an unchanged tombstone rewrite must stay deleted");
    assert_eq!(kept.content, "被遗忘后仍要保留原文");
    assert!(store.search_memory("被遗忘", 10).unwrap().is_empty());
    assert!(store.list_memory(10).unwrap().is_empty());
}

/// An operation no worker can settle is refused AT ADMISSION.
///
/// Admitting it would leave the request `accepted` forever: no entry, no
/// failure, no owner, and invisible to the worker that settles admitted
/// requests. The refusal is typed and names the operation.
#[test]
fn unsupported_memory_intent_kind_is_refused_at_admission() {
    use shenko_types::error::ShenkoError;
    let store = tmp_store("unsupported-kind");
    let agent = EntityId::new();
    let intent = EntityId::new();
    let record = shenko_store::MemoryIntentRecord::accepted(
        intent,
        agent,
        "purge-everything",
        "personal",
        "an operation the store cannot settle",
        None,
        Some(EntityId::new()),
    );

    let error = store.admit_memory_intent(&record, None).unwrap_err();
    assert!(
        matches!(error, ShenkoError::Conflict(_)),
        "an unsupported operation is refused with the module's typed error: {error:?}"
    );
    assert!(
        error.to_string().contains("purge-everything"),
        "the refusal names the operation it refused: {error}"
    );

    // Nothing durable exists: the request was refused, never accepted and
    // then forgotten. There is no accepted row to replay either.
    assert!(store.memory_intent(intent).unwrap().is_none());
    assert!(store.memory_intent_state(intent).unwrap().is_none());
    assert!(store.pending_library_memory_intents(agent, 10).unwrap().is_empty());
    let replay = store.admit_memory_intent(&record, None).unwrap_err();
    assert!(
        matches!(replay, ShenkoError::Conflict(_)),
        "the replay of a refused request is refused the same way: {replay:?}"
    );
    assert!(store.memory_intent(intent).unwrap().is_none());

    // The refusal leaves the agent's memory path usable.
    let supported = EntityId::new();
    let supported_record = shenko_store::MemoryIntentRecord::accepted(
        supported,
        agent,
        "save",
        "personal",
        "a supported operation",
        None,
        Some(EntityId::new()),
    );
    store.admit_memory_intent(&supported_record, None).unwrap();
    assert_eq!(
        store.pending_library_memory_intents(agent, 10).unwrap(),
        vec![supported]
    );
}

/// Every operation the store admits is one the pending query selects.
///
/// An admitted request is always visible to its worker. Older unsupported
/// requests also stay visible so the worker can record their failure.
#[test]
fn every_supported_operation_is_admitted_and_visible_to_the_worker() {
    let store = tmp_store("supported-kinds");
    let agent = EntityId::new();
    let mut admitted = Vec::new();
    for kind in shenko_store::SUPPORTED_MEMORY_OP_KINDS {
        let intent = EntityId::new();
        let record = shenko_store::MemoryIntentRecord::accepted(
            intent,
            agent,
            kind,
            "personal",
            format!("a {kind} request"),
            None,
            Some(EntityId::new()),
        );
        store.admit_memory_intent(&record, None).unwrap();
        admitted.push(intent);
    }
    assert_eq!(admitted.len(), shenko_store::SUPPORTED_MEMORY_OP_KINDS.len());

    let pending = store.pending_library_memory_intents(agent, 10).unwrap();
    assert_eq!(
        pending.len(),
        admitted.len(),
        "every admitted request is visible to the worker: {pending:?}"
    );
    for intent in &admitted {
        assert!(pending.contains(intent), "admitted request {intent} is pending");
        assert_eq!(
            store.memory_intent_state(*intent).unwrap().as_deref(),
            Some("accepted")
        );
    }
}
