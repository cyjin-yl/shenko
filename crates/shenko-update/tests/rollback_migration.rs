//! C21 rollback: the A/B state migration a rollback performs, on real
//! separate Store copies.
//!
//! Contract basis — `docs/specs/self-update.md`: "New facts, revocations,
//! tombstones and external debt cannot disappear because a workspace or
//! software pointer moves backwards" and "Current facts pass through typed
//! recovery into the previous copy; conflicts and unreadable formats are
//! retained/refused, not treated as successful import".
//!
//! Both generations here are real directories with their own SQLite database,
//! taken from one consistent snapshot with the same SQLite backup the
//! `prepare` path uses, sharing the installation CAS. Nothing is a mock: the
//! newer generation commits facts through the real memory APIs, the migration
//! is `import_rollback_delta`, and every assertion reads the retained copy
//! back through its own reopened `Store`.

use std::fs;
use std::path::PathBuf;

use shenko_store::memory::MemoryEntry;
use shenko_store::{MemoryIntentRecord, Store};
use shenko_types::ids::EntityId;

use shenko_update::migration::{self, import_rollback_delta};
use shenko_update::slots;

/// One installation: a shared CAS plus the two generation copies of the same
/// initial state.
struct Installation {
    root: PathBuf,
    cas: PathBuf,
    /// The retained generation a rollback goes back to.
    older: PathBuf,
    /// The currently selected generation.
    newer: PathBuf,
    agent: EntityId,
    baseline_intent: EntityId,
    baseline_entry: EntityId,
}

impl Installation {
    fn cleanup(self) {
        let _ = fs::remove_dir_all(&self.root);
    }

    fn open_older(&self) -> Store {
        Store::open(self.older.clone(), self.cas.clone()).unwrap()
    }

    fn open_newer(&self) -> Store {
        Store::open(self.newer.clone(), self.cas.clone()).unwrap()
    }
}

fn installation() -> Installation {
    let root = std::env::temp_dir().join(format!("shenko-rollback-migration-{}", EntityId::new()));
    let cas = root.join("cas");
    fs::create_dir_all(&cas).unwrap();
    let shared_dir = root.join("shared");
    fs::create_dir_all(&shared_dir).unwrap();
    let shared_db = shared_dir.join("shenko.db");
    let agent = EntityId::new();
    // The initial consistent state every generation of this installation
    // starts from: one committed fact, written through the real save path.
    let (baseline_intent, baseline_entry) = {
        let shared = Store::open(shared_db.clone(), cas.clone()).unwrap();
        save(&shared, agent, "shared baseline")
    };
    let older = root.join("gen-a").join("shenko.db");
    let newer = root.join("gen-b").join("shenko.db");
    fs::create_dir_all(older.parent().unwrap()).unwrap();
    fs::create_dir_all(newer.parent().unwrap()).unwrap();
    slots::backup_database(&shared_db, &older).unwrap();
    slots::backup_database(&shared_db, &newer).unwrap();
    Installation { root, cas, older, newer, agent, baseline_intent, baseline_entry }
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

/// Commit a save and then a semantic forget of the same fact: a fact and its
/// tombstone, both produced only by the newer generation.
fn save_and_forget(store: &Store, agent: EntityId, content: &str) -> EntityId {
    let (_, entry_id) = save(store, agent, content);
    forget(store, agent, entry_id);
    entry_id
}

/// Retire one existing fact through the real semantic-forget path.
fn forget(store: &Store, agent: EntityId, entry_id: EntityId) -> EntityId {
    let forget = EntityId::new();
    let record = MemoryIntentRecord::accepted(
        forget,
        agent,
        "forget",
        "personal",
        "",
        None,
        Some(entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    store.commit_memory_forget(forget).unwrap();
    forget
}

/// A rollback moves the newer generation's closure into the retained copy
/// before anything is selected. The facts keep their original identities, the
/// newer generation's own tombstones stay tombstones, the retained copy's
/// baseline is untouched, an exact retry changes nothing, and the newer
/// generation is only read.
#[test]
fn a_rollback_import_carries_the_newer_generations_facts_into_the_retained_copy() {
    let install = installation();
    let (new_intent, new_entry, forgotten_entry) = {
        let newer = install.open_newer();
        let (intent, entry) = save(&newer, install.agent, "recorded on the newer generation");
        let forgotten = save_and_forget(
            &newer,
            install.agent,
            "created and forgotten on the newer generation",
        );
        (intent, entry, forgotten)
    };
    // The newer generation also retires the fact both copies started with:
    // the retirement has to survive the rollback, not be undone by it.
    let baseline_forget = {
        let newer = install.open_newer();
        forget(&newer, install.agent, install.baseline_entry)
    };
    let source_watermark = migration::watermark_of(&install.newer).unwrap();
    let retained_ledger = migration::watermark_of(&install.older)
        .unwrap()
        .expect("the retained copy has its own committed history")
        .ledger_id;

    let report = import_rollback_delta(&install.newer, &install.older, &install.cas).unwrap();
    assert!(report.blocked_units.is_empty(), "{:?}", report.blocked_units);
    assert!(report.rows_written() > 0, "the newer generation's facts must actually be written");
    assert_eq!(
        report.uncovered_source_commits, 0,
        "the snapshot must cover the source it was taken from"
    );
    assert_eq!(report.source_watermark_before, source_watermark);
    assert_eq!(report.source_watermark_after, source_watermark);
    assert_eq!(report.watermark, source_watermark);
    // Importing facts is not a new installation: the retained copy keeps the
    // ledger identity it had, it does not adopt the source's.
    assert_eq!(
        migration::watermark_of(&install.older).unwrap().unwrap().ledger_id,
        retained_ledger
    );
    // The typed snapshot stays beside the retained copy as recovery evidence,
    // and it is named as a complete archive, never as a staging file.
    let retained = PathBuf::from(report.archive_path.clone().expect("the retained archive is named"));
    assert!(retained.is_file(), "the retained archive must exist: {}", retained.display());
    assert!(retained.to_string_lossy().ends_with(".shka"));
    assert_eq!(retained.parent(), install.older.parent());
    let staging = fs::read_dir(install.older.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().contains(".stage"))
        .count();
    assert_eq!(staging, 0, "a complete snapshot must not stay under a staging name");

    let older = install.open_older();
    let carried = older.memory_entry(new_entry).unwrap().expect("the newer fact reached the retained copy");
    assert_eq!(carried.content, "recorded on the newer generation");
    assert_eq!(carried.agent_id, install.agent);
    assert_eq!(
        older.memory_intent_state(new_intent).unwrap().as_deref(),
        Some("committed"),
        "the request that produced the fact keeps its terminal state"
    );
    let tombstone = older
        .memory_entry(forgotten_entry)
        .unwrap()
        .expect("a fact forgotten by the newer generation stays readable in the retained copy");
    assert!(
        tombstone.deleted,
        "a rollback must not revive a fact the newer generation forgot"
    );
    assert!(
        older.list_memory(64).unwrap().iter().all(|entry| entry.entry_id != forgotten_entry),
        "the retained copy must not surface the forgotten fact either"
    );
    let baseline = older.memory_entry(install.baseline_entry).unwrap().expect("the baseline stays readable");
    assert_eq!(baseline.content, "shared baseline", "a retirement never rewrites the fact");
    assert!(
        baseline.deleted,
        "the newer generation's retirement of a shared fact must survive the rollback"
    );
    assert_eq!(baseline.record_revision, 1);
    assert_eq!(
        older.memory_intent_state(install.baseline_intent).unwrap().as_deref(),
        Some("committed")
    );
    assert_eq!(
        older.memory_intent_state(baseline_forget).unwrap().as_deref(),
        Some("committed"),
        "the request that retired the shared fact travelled with it"
    );
    let live: Vec<String> = older.list_memory(64).unwrap().into_iter().map(|entry| entry.content).collect();
    assert_eq!(live.len(), 1, "only the still-live fact is surfaced, without duplicates: {live:?}");
    assert_eq!(live[0], "recorded on the newer generation");
    drop(older);

    let retry = import_rollback_delta(&install.newer, &install.older, &install.cas).unwrap();
    assert!(retry.blocked_units.is_empty(), "{:?}", retry.blocked_units);
    assert_eq!(
        retry.rows_written(),
        0,
        "an exact retry rewrites nothing and duplicates no fact"
    );
    let older = install.open_older();
    assert_eq!(older.list_memory(64).unwrap().len(), 1);
    assert!(older.memory_entry(install.baseline_entry).unwrap().unwrap().deleted);
    assert_eq!(
        older.memory_entry(new_entry).unwrap().unwrap().content,
        "recorded on the newer generation"
    );
    drop(older);

    // The source is a fact about the newer generation, not a workspace to be
    // rewritten: the migration only read it, and no second generation exists.
    assert_eq!(migration::watermark_of(&install.newer).unwrap(), source_watermark);
    let newer = install.open_newer();
    assert_eq!(
        newer.memory_entry(new_entry).unwrap().unwrap().content,
        "recorded on the newer generation"
    );
    assert_eq!(
        newer.memory_intent_state(new_intent).unwrap().as_deref(),
        Some("committed")
    );
    drop(newer);
    install.cleanup();
}

/// Two generations answered the same request differently. The migration must
/// report the typed conflict and keep the retained copy's own value instead
/// of overwriting it with the newer generation's, and the refused unit must
/// not leave a half-imported fact behind.
#[test]
fn a_rollback_import_refuses_a_diverged_fact_and_keeps_the_local_one() {
    let install = installation();
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    {
        let older = install.open_older();
        let local = MemoryIntentRecord::accepted(
            intent,
            install.agent,
            "save",
            "personal",
            "the retained copy's own answer",
            None,
            Some(entry_id),
        );
        older.admit_memory_intent(&local, None).unwrap();
    }
    {
        let newer = install.open_newer();
        let remote = MemoryIntentRecord::accepted(
            intent,
            install.agent,
            "save",
            "personal",
            "the newer generation's answer",
            None,
            Some(entry_id),
        );
        newer.admit_memory_intent(&remote, None).unwrap();
        let mut entry = MemoryEntry::new(install.agent, "the newer generation's answer");
        entry.entry_id = entry_id;
        entry.kind = "note".into();
        newer.commit_memory_intent(&entry, intent).unwrap();
    }

    let report = import_rollback_delta(&install.newer, &install.older, &install.cas).unwrap();
    assert!(
        report.blocked_units.iter().any(|unit| unit.contains("typed recovery conflict")),
        "a diverged fact must block the rollback, not be merged: {:?}",
        report.blocked_units
    );
    assert!(
        report.blocked_units.iter().any(|unit| unit.contains(&intent.to_string())),
        "the refusal must name the fact it kept: {:?}",
        report.blocked_units
    );

    let older = install.open_older();
    let local = older.memory_intent(intent).unwrap().expect("the retained copy keeps its own request");
    assert_eq!(local.record.content, "the retained copy's own answer");
    assert!(
        older.memory_entry(entry_id).unwrap().is_none(),
        "a refused unit must not leave a half-imported fact"
    );
    assert_eq!(
        older.memory_intent_state(install.baseline_intent).unwrap().as_deref(),
        Some("committed"),
        "the rest of the retained copy is unchanged"
    );
    drop(older);
    install.cleanup();
}
