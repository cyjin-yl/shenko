//! One generation retiring a fact the other already held — docs/specs/nodes.md#spec-5-4, docs/specs/memory.md#spec-12-4.
//!
//! `docs/specs/self-update.md`: "New facts, revocations, tombstones and
//! external debt cannot disappear because a workspace or software pointer
//! moves backwards" and "conflicts and unreadable formats are
//! retained/refused, not treated as successful import".
//!
//! A semantic forget moves a fact forward, and a peer that has no delivered
//! baseline for the row still owns that move when the archive carries its own
//! committed `forget` request and nothing but the retirement changed. Any
//! other difference keeps the conflict and the local row exactly as it was.

use std::path::PathBuf;

use shenko_store::cognition::{
    CognitionImportOptions, CognitionImportOutcome, CognitionSnapshotOptions,
};
use shenko_store::memory::MemoryEntry;
use shenko_store::store::Store;
use shenko_store::MemoryIntentRecord;
use shenko_types::ids::EntityId;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-retirement-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn archive_of(store: &Store) -> Vec<u8> {
    let mut bytes = Vec::new();
    store
        .write_cognition_archive(&CognitionSnapshotOptions::default(), None, &mut bytes)
        .unwrap();
    bytes
}

fn import(store: &Store, archive: &[u8], peer: EntityId) -> CognitionImportOutcome {
    store
        .import_cognition_archive(archive, &CognitionImportOptions {
            peer_id: Some(peer),
            ..Default::default()
        })
        .unwrap()
}

/// Commit one memory fact through the real save path.
fn save(store: &Store, agent: EntityId, content: &str) -> (EntityId, EntityId) {
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let record = MemoryIntentRecord::accepted(
        intent,
        agent,
        "save",
        "personal",
        content,
        None,
        Some(entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    let mut entry = MemoryEntry::new(agent, content);
    entry.entry_id = entry_id;
    entry.kind = "note".into();
    store.commit_memory_intent(&entry, intent).unwrap();
    (intent, entry_id)
}

/// Forget one entry through the real semantic-forget path.
fn forget(store: &Store, agent: EntityId, entry_id: EntityId) -> EntityId {
    let intent = EntityId::new();
    let record = MemoryIntentRecord::accepted(
        intent,
        agent,
        "forget",
        "personal",
        "",
        None,
        Some(entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    store.commit_memory_forget(intent).unwrap();
    intent
}

/// Two generations that both hold the same fact, created through one import of
/// the same seed snapshot. Their rows are byte-identical, as two copies of one
/// installation are.
fn two_generations(dir: &std::path::Path) -> (Store, Store, EntityId, EntityId, EntityId) {
    let agent = EntityId::new();
    let seed = Store::open(dir.join("seed.db"), dir.join("seed-cas")).unwrap();
    let (_, entry_id) = save(&seed, agent, "shared fact");
    let snapshot = archive_of(&seed);
    drop(seed);
    let older = Store::open(dir.join("older.db"), dir.join("older-cas")).unwrap();
    let newer = Store::open(dir.join("newer.db"), dir.join("newer-cas")).unwrap();
    let seed_peer = EntityId::new();
    for store in [&older, &newer] {
        let outcome = import(store, &snapshot, seed_peer);
        assert!(outcome.conflicts.is_empty(), "{:?}", outcome.conflicts);
        assert!(outcome.inserted > 0, "the seed fact and its request travel together");
        assert_eq!(outcome.updated, 0);
    }
    (older, newer, agent, entry_id, seed_peer)
}

/// The newer generation forgot a fact the retained copy still holds live. The
/// archive proves who retired it, so the retirement is admitted — the entry's
/// own content, provenance and identity stay the retained copy's — and an
/// exact retry changes nothing.
#[test]
fn a_proven_retirement_reaches_the_copy_that_never_saw_it() {
    let dir = tmp_dir();
    let (older, newer, agent, entry_id, _seed_peer) = two_generations(&dir);
    let forget_intent = forget(&newer, agent, entry_id);
    assert!(
        newer.memory_entry(entry_id).unwrap().unwrap().deleted,
        "the newer generation really retired the fact"
    );
    let snapshot = archive_of(&newer);
    let newer_peer = EntityId::new();

    let outcome = import(&older, &snapshot, newer_peer);
    assert!(
        outcome.conflicts.is_empty(),
        "a provable retirement is a forward move, not a conflict: {:?}",
        outcome.conflicts
    );
    assert!(outcome.ack(newer_peer).verified);
    let retired = older.memory_entry(entry_id).unwrap().expect("the fact stays readable");
    assert!(retired.deleted, "the committed retirement must reach the retained copy");
    assert_eq!(retired.content, "shared fact", "a retirement never rewrites the fact");
    assert_eq!(retired.record_revision, 1, "exactly the forget's own revision advance");
    assert_eq!(older.list_memory(16).unwrap().len(), 0, "the fact is no longer surfaced");
    assert_eq!(
        older.memory_intent_state(forget_intent).unwrap().as_deref(),
        Some("committed")
    );

    // The same archive again: nothing is rewritten and no conflict appears.
    let again = import(&older, &snapshot, newer_peer);
    assert!(again.conflicts.is_empty(), "{:?}", again.conflicts);
    assert_eq!(again.inserted, 0);
    assert_eq!(again.updated, 0, "an admitted retirement is idempotent");
    assert_eq!(older.memory_entry(entry_id).unwrap().unwrap().record_revision, 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The newer generation forgot the fact while the retained copy edited its
/// text. That is a genuine disagreement about the fact itself, so the unit
/// keeps its conflict: the local row stays live with its own text, and the
/// refused request is not left behind.
#[test]
fn a_retirement_never_overwrites_an_unrelated_edit() {
    let dir = tmp_dir();
    let (older, newer, agent, entry_id, _seed_peer) = two_generations(&dir);
    drop(older);
    {
        // The retained copy's own edit of the fact this archive would retire.
        let conn = rusqlite::Connection::open(dir.join("older.db")).unwrap();
        conn.execute(
            "UPDATE memory_entries SET content=?1 WHERE entry_id=?2",
            rusqlite::params!["the retained copy's own text", entry_id.to_string()],
        )
        .unwrap();
    }
    let older = Store::open(dir.join("older.db"), dir.join("older-cas")).unwrap();

    let forget_intent = forget(&newer, agent, entry_id);
    let outcome = import(&older, &archive_of(&newer), EntityId::new());
    assert!(
        outcome.conflicts.iter().any(|conflict| conflict.table == "memory_entries"),
        "a diverged fact must still conflict: {:?}",
        outcome.conflicts
    );
    let kept = older.memory_entry(entry_id).unwrap().expect("the fact stays");
    assert!(!kept.deleted, "a retirement must not overwrite a concurrent edit");
    assert_eq!(kept.content, "the retained copy's own text");
    assert_eq!(kept.record_revision, 0);
    assert!(
        older.memory_intent_state(forget_intent).unwrap().is_none(),
        "the refused unit leaves no request behind"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
