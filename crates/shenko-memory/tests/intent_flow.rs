//! Intent acceptance → commit → search (SPEC §12.2 / §12.3).
use shenko_memory::{MemoryStore, ProcessOutcome};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::subject::{MemoryIntent, MemoryIntentKind};
use std::sync::Arc;

fn store(tag: &str) -> Arc<Store> {
    let dir = std::env::temp_dir().join(format!(
        "shenko-intent-{}-{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap())
}

fn intent(kind: MemoryIntentKind) -> MemoryIntent {
    MemoryIntent {
        intent_id: EntityId::new(),
        kind,
        scope: "personal".to_string(),
        content_ref: None,
    }
}

#[test]
fn accepted_intent_commits_and_is_searchable_by_chinese_substring() {
    let store = store("flow");
    let agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));
    let intent = intent(MemoryIntentKind::Save);

    let accepted = memory
        .accept_intent_with_content(intent.clone(), Some("Shenko 深刻理解自己并持续思考".into()))
        .unwrap();
    assert_eq!(accepted.status, "accepted");
    assert_eq!(accepted.intent_id, intent.intent_id);
    assert_eq!(
        store.memory_intent_state(intent.intent_id).unwrap().as_deref(),
        Some("accepted")
    );

    match memory.process_intent(intent.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => {
            assert!(entry.content.contains("深刻"));
            assert_eq!(entry.scope, "personal");
        }
        other => panic!("expected committed, got {other:?}"),
    }

    assert_eq!(
        store.memory_intent_state(intent.intent_id).unwrap().as_deref(),
        Some("committed")
    );

    let hits = memory.search("深刻", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].content.contains("深刻"));

    assert!(matches!(
        memory.process_intent(intent.intent_id).unwrap(),
        ProcessOutcome::AlreadyProcessed(_)
    ));
    assert!(memory.pending_intents().is_empty());
}

#[test]
fn unsupported_intent_kind_is_recorded_as_failed() {
    let store = store("recall");
    let memory = MemoryStore::with_store(EntityId::new(), Arc::clone(&store));
    let intent = intent(MemoryIntentKind::Recall);

    memory
        .accept_intent_with_content(intent.clone(), Some("anything".into()))
        .unwrap();
    match memory.process_intent(intent.intent_id).unwrap() {
        ProcessOutcome::Failed(reason) => assert!(reason.contains("recall")),
        other => panic!("expected failed, got {other:?}"),
    }
    assert_eq!(
        store.memory_intent_state(intent.intent_id).unwrap().as_deref(),
        Some("failed")
    );
}
