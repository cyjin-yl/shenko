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
    // An identical concurrent acceptance may return an idempotent receipt or
    // explicitly reject, but must not replace the durable binding.
    let _ = second.accept_intent_with_content(intent.clone(), Some(text.into()));
    let original = match first.process_intent(intent.intent_id).unwrap() {
        ProcessOutcome::Committed(entry) => entry.entry_id,
        other => panic!("expected original commit, got {other:?}"),
    };
    assert!(matches!(second.process_intent(intent.intent_id).unwrap(), ProcessOutcome::AlreadyProcessed(_)));
    let _ = store.fail_memory_intent(intent.intent_id, "late stale failure");
    assert_eq!(store.memory_intent_state(intent.intent_id).unwrap().as_deref(), Some("committed"));
    let found = first.search("深刻", 10).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].entry_id, original);
    assert_eq!(found[0].content, text);
}

#[test]
fn accepted_memory_with_bound_cas_content_recovers_after_reopen() {
    let tag = "pending-ref-reopen";
    let dir = std::env::temp_dir().join(format!("shenko-intent-{}-{tag}", std::process::id()));
    let store = store(tag);
    let agent = EntityId::new();
    let content = "可恢复的深刻正文";
    let object = store.cas.seal_object(content.as_bytes(), "text/plain", "memory", "raw", 1).unwrap().object;
    let mut request = intent(MemoryIntentKind::Save);
    request.content_ref = Some(object.clone());
    let first = MemoryStore::with_store(agent, Arc::clone(&store));
    first.accept_intent(request.clone()).unwrap();

    // A reused locator with a different digest is not the same request.
    let mut conflict = request.clone();
    conflict.content_ref.as_mut().unwrap().digest = shenko_types::object::Digest::blake3(b"different");
    assert!(first.accept_intent(conflict).is_err());
    drop(first);
    drop(store);

    let reopened = Arc::new(Store::open(dir.join("intent.db"), dir.join("cas")).unwrap());
    let recovered = MemoryStore::with_store(agent, Arc::clone(&reopened));
    assert_eq!(recovered.process_all().unwrap(), 1);
    let facts = recovered.search("深刻", 10).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].content, content);
    assert_eq!(reopened.memory_intent_state(request.intent_id).unwrap().as_deref(), Some("committed"));
}

#[test]
fn replicated_pending_memory_carries_its_object_binding_and_content() {
    let source = store("replication-source");
    let target = store("replication-target");
    let agent = EntityId::new();
    let body = "复制后仍可核实的深刻正文";
    let object = source.cas.seal_object(body.as_bytes(), "text/plain", "memory", "raw", 1).unwrap().object;
    let mut request = intent(MemoryIntentKind::Save);
    request.content_ref = Some(object.clone());
    let memory = MemoryStore::with_store(agent, Arc::clone(&source));
    memory.accept_intent(request.clone()).unwrap();
    let mut archive = Vec::new();
    source.write_cognition_archive(&Default::default(), None, &mut archive).unwrap();
    let receipt = target.import_cognition_archive(&archive[..], &Default::default()).unwrap();
    assert!(receipt.ack(EntityId::new()).verified, "reference and source bytes must travel with the intent");

    let recovered = MemoryStore::with_store(agent, Arc::clone(&target));
    assert_eq!(recovered.process_all().unwrap(), 1);
    let facts = recovered.search("深刻", 10).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].content, body);
    assert_eq!(target.cas.read_object(&object.digest).unwrap(), body.as_bytes());
}

#[test]
fn missing_content_object_remains_recoverable_instead_of_failed() {
    let store = store("missing-object");
    let agent = EntityId::new();
    let object = store.cas.seal_object(b"present", "text/plain", "memory", "raw", 1).unwrap().object;
    let mut request = intent(MemoryIntentKind::Save);
    let mut missing = object;
    missing.digest = shenko_types::object::Digest::blake3(b"not installed in this store");
    request.content_ref = Some(missing);
    let memory = MemoryStore::with_store(agent, Arc::clone(&store));
    memory.accept_intent(request.clone()).unwrap();
    assert!(memory.process_intent(request.intent_id).is_err());
    assert_eq!(store.memory_intent_state(request.intent_id).unwrap().as_deref(), Some("accepted"));
    assert!(memory.search("深刻", 10).unwrap().is_empty());
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
