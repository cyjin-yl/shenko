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

#[test]
fn replay_after_reopen_preserves_the_original_memory() {
    let tag = "replay-after-reopen";
    let dir = std::env::temp_dir().join(format!("shenko-intent-{}-{tag}", std::process::id()));
    let store = store(tag);
    let agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));
    let intent = intent(MemoryIntentKind::Save);
    let text = "用户的深刻记忆是蓝色书签";
    memory.accept_intent_with_content(intent.clone(), Some(text.into())).unwrap();
    let original = match memory.process_intent(intent.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected original commit, got {other:?}"),
    };
    drop(memory);
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap());
    let memory = MemoryStore::with_store(agent, Arc::clone(&reopened));
    if memory.accept_intent_with_content(intent.clone(), Some(text.into())).is_ok() {
        memory.process_intent(intent.intent_id).unwrap();
    }
    assert_eq!(reopened.memory_intent_state(intent.intent_id).unwrap().as_deref(), Some("committed"));
    let matches = memory.search("深刻", 10).unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].entry_id, original);
    assert_eq!(matches[0].content, text);

    assert!(memory.accept_intent_with_content(intent.clone(), Some("改写原始事实".into())).is_err());
    assert_eq!(memory.search("深刻", 10).unwrap()[0].entry_id, original);
    assert_eq!(reopened.memory_intent_state(intent.intent_id).unwrap().as_deref(), Some("committed"));
}

#[test]
fn stale_worker_and_late_failure_cannot_replace_a_committed_memory() {
    let tag = "stale-worker";
    let dir = std::env::temp_dir().join(format!("shenko-intent-{}-{tag}", std::process::id()));
    let store = store(tag);
    let other_store = Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap());
    let agent = EntityId::new();
    let first = MemoryStore::with_store(agent, Arc::clone(&store));
    let second = MemoryStore::with_store(agent, other_store);
    let intent = intent(MemoryIntentKind::Save);
    let text = "共享的深刻事实";
    first.accept_intent_with_content(intent.clone(), Some(text.into())).unwrap();
    assert_eq!(second.accept_intent_with_content(intent.clone(), Some(text.into())).unwrap().status, "accepted");
    let original = match first.process_intent(intent.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected original commit, got {other:?}"),
    };
    assert_eq!(second.process_all().unwrap(), 0);
    assert!(second.pending_intents().is_empty());
    assert!(matches!(second.process_intent(intent.intent_id).unwrap(), ProcessOutcome::AlreadyProcessed(_)));
    let _ = store.fail_memory_intent(intent.intent_id, "late stale failure");
    assert_eq!(store.memory_intent_state(intent.intent_id).unwrap().as_deref(), Some("committed"));
    let found = first.search("深刻", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entry_id, original);
    assert_eq!(found[0].content, text);
}

#[test]
fn concurrent_equal_admissions_share_one_durable_entry_identity() {
    let tag = "concurrent-accept";
    let dir = std::env::temp_dir().join(format!("shenko-intent-{}-{tag}", std::process::id()));
    let first_store = store(tag);
    let second_store = Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap());
    let agent = EntityId::new();
    let first = Arc::new(MemoryStore::with_store(agent, first_store));
    let second = Arc::new(MemoryStore::with_store(agent, second_store));
    let request = intent(MemoryIntentKind::Save);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            first.accept_intent_with_content(request.clone(), Some("并发的深刻事实".into())).unwrap();
        });
        let b = scope.spawn(|| {
            barrier.wait();
            second.accept_intent_with_content(request.clone(), Some("并发的深刻事实".into())).unwrap();
        });
        a.join().unwrap();
        b.join().unwrap();
    });
    assert_eq!(first.pending_intents()[0].entry_id, second.pending_intents()[0].entry_id);
    let first_id = match first.process_intent(request.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected one commit, got {other:?}"),
    };
    assert!(matches!(second.process_intent(request.intent_id).unwrap(), ProcessOutcome::AlreadyProcessed(_)));
    let facts = first.search("深刻", 10).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].entry_id, first_id);
}

/// SPEC §12.4 semantic forget: the durable intent settles the target entry
/// in one commit, default search/list stop surfacing it, and the original
/// text stays readable — including after a store reopen. A replay of the
/// same intent resolves against the committed fact instead of re-forgetting.
#[test]
fn forget_hides_from_search_but_keeps_original_text_after_reopen() {
    let tag = "forget-hides";
    let dir = std::env::temp_dir().join(format!("shenko-intent-{}-{tag}", std::process::id()));
    let store = store(tag);
    let agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));

    let save = intent(MemoryIntentKind::Save);
    let text = "主体的深刻经历：第一次自主整理记忆";
    memory.accept_intent_with_content(save.clone(), Some(text.into())).unwrap();
    let entry_id = match memory.process_intent(save.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected save commit, got {other:?}"),
    };
    assert_eq!(memory.search("深刻", 10).unwrap().len(), 1);

    // A Forget intent names the entry it retires as its durable binding.
    let forget = intent(MemoryIntentKind::Forget);
    assert_eq!(
        memory.accept_forget(forget.clone(), entry_id).unwrap().status,
        "accepted"
    );
    match memory.process_intent(forget.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => {
            assert_eq!(entry.entry_id, entry_id);
            assert!(entry.deleted, "read-back must report the entry forgotten");
            assert_eq!(entry.content, text);
        }
        other => panic!("expected forget commit, got {other:?}"),
    }
    assert_eq!(
        store.memory_intent_state(forget.intent_id).unwrap().as_deref(),
        Some("committed")
    );

    // Default recall no longer surfaces it; the original text is still there.
    assert!(memory.search("深刻", 10).unwrap().is_empty());
    assert!(memory.list(10).unwrap().is_empty());
    let kept = memory.read(entry_id).unwrap().expect("forgotten entry stays durable");
    assert_eq!(kept.content, text);
    assert!(kept.deleted);

    // Reopen: the forget survives, the replay of the same intent is a no-op.
    drop(memory);
    drop(store);
    let reopened = Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap());
    let memory = MemoryStore::with_store(agent, Arc::clone(&reopened));
    assert_eq!(
        memory.accept_forget(forget.clone(), entry_id).unwrap().status,
        "committed"
    );
    assert!(matches!(
        memory.process_intent(forget.intent_id).unwrap(),
        ProcessOutcome::AlreadyProcessed(_)
    ));
    assert!(memory.search("深刻", 10).unwrap().is_empty());
    let kept = memory.read(entry_id).unwrap().expect("forgotten entry survives reopen");
    assert_eq!(kept.content, text);
    assert!(kept.deleted);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A forget bound to a missing or foreign entry cannot commit; the intent
/// stays accepted and retryable so a later-arriving replicated entry can
/// still satisfy it, and no other entry is touched.
#[test]
fn forget_of_missing_or_foreign_entry_stays_unsettled() {
    let store = store("forget-missing");
    let agent = EntityId::new();
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));

    let save = intent(MemoryIntentKind::Save);
    memory
        .accept_intent_with_content(save.clone(), Some("保留的深刻事实".into()))
        .unwrap();
    let kept_id = match memory.process_intent(save.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected save commit, got {other:?}"),
    };

    // Missing target: no commit, the intent remains accepted for reconciliation.
    let missing = intent(MemoryIntentKind::Forget);
    memory.accept_forget(missing.clone(), EntityId::new()).unwrap();
    assert!(memory.process_intent(missing.intent_id).is_err());
    assert_eq!(
        store.memory_intent_state(missing.intent_id).unwrap().as_deref(),
        Some("accepted")
    );

    // Foreign target: a forget admitted under one agent cannot retire
    // another agent's entry.
    let foreign = MemoryStore::with_store(EntityId::new(), Arc::clone(&store));
    let foreign_forget = intent(MemoryIntentKind::Forget);
    foreign
        .accept_forget(foreign_forget.clone(), kept_id)
        .unwrap();
    assert!(foreign.process_intent(foreign_forget.intent_id).is_err());
    assert_eq!(
        store.memory_intent_state(foreign_forget.intent_id).unwrap().as_deref(),
        Some("accepted")
    );

    let kept = memory.read(kept_id).unwrap().expect("untouched entry");
    assert!(!kept.deleted);
    assert_eq!(memory.search("深刻", 10).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("shenko-intent-{}-forget-missing", std::process::id())),
    );
}
