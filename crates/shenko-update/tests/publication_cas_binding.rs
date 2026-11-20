//! Real borrowed-owner checks. Each test runs its Store/permit operations in
//! an owned child with a finite deadline, so a hidden reacquisition fails
//! instead of hanging the test suite. No reclamation or model is involved.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use shenko_store::memory::MemoryEntry;
use shenko_store::roots::CasWriteFence;
use shenko_store::{MemoryIntentRecord, Store, StorePublication};
use shenko_types::ids::EntityId;
use shenko_types::installation::{InstallationDomain, MUTATION_LOCK_FILENAME};
use shenko_update::{migration, slots};

const CHILD_ROOT: &str = "SHENKO_UPDATE_PUBLICATION_BINDING_CHILD_ROOT";
const CAS_MISMATCH: &str = "CAS publication capability belongs to another physical content store";
const NEW_FACT: &str = "committed after the retained generation was copied";

fn save(store: &Store, agent: EntityId, content: &str) -> (EntityId, EntityId) {
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let record = MemoryIntentRecord::accepted(
        intent, agent, "save", "personal", content, None, Some(entry_id),
    );
    store.admit_memory_intent(&record, None).unwrap();
    let mut entry = MemoryEntry::new(agent, content);
    entry.entry_id = entry_id;
    entry.kind = "note".into();
    store.commit_memory_intent(&entry, intent).unwrap();
    (intent, entry_id)
}

fn fixture() -> PathBuf {
    let root = std::env::temp_dir().join(format!("shenko-cas-binding-{}", EntityId::new()));
    let installation = root.join("installation");
    let source = installation.join("versions/source");
    let target = installation.join("versions/target");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    let cas = installation.join("cas");
    let agent = EntityId::new();
    {
        let store = Store::open(source.join(slots::SLOT_DB_FILENAME), cas.clone()).unwrap();
        slots::publish_current_slot(&installation, &source).unwrap();
        save(&store, agent, "shared baseline");
    }
    slots::backup_database(&source.join(slots::SLOT_DB_FILENAME), &target.join(slots::SLOT_DB_FILENAME)).unwrap();
    {
        let store = Store::open(source.join(slots::SLOT_DB_FILENAME), cas).unwrap();
        let (intent, entry) = save(&store, agent, NEW_FACT);
        fs::write(root.join("intent-id"), intent.to_string()).unwrap();
        fs::write(root.join("entry-id"), entry.to_string()).unwrap();
    }
    fs::write(target.join(slots::SLOT_MANIFEST_FILENAME), b"retained original manifest").unwrap();
    // The wrong capability names another existing physical CAS. Creation is
    // deliberately outside the installed-tree no-side-effects observation.
    drop(StorePublication::acquire(root.join("other-cas")).unwrap());
    root
}

fn tree_bytes(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, directory: &Path, bytes: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(!metadata.file_type().is_symlink(), "unexpected fixture symlink: {}", path.display());
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if metadata.is_dir() {
                bytes.insert(relative, None);
                visit(root, &path, bytes);
            } else {
                assert!(metadata.is_file());
                bytes.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut bytes = BTreeMap::new();
    visit(root, root, &mut bytes);
    bytes
}

fn run_bounded_child(selector: &str, root: &Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", selector, "--nocapture"])
        .env(CHILD_ROOT, root)
        .stdout(Stdio::inherit()).stderr(Stdio::inherit())
        .spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "native child failed: {status}; retained fixture: {}", root.display());
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let killed = child.kill();
                let reaped = child.wait();
                panic!("native child exceeded deadline or failed observation: {result:?}; kill={killed:?}; reap={reaped:?}; retained fixture: {}", root.display());
            }
        }
    }
}

fn manifest(cas: &Path) -> slots::SlotManifest {
    slots::SlotManifest {
        schema_version: slots::SlotManifest::SCHEMA_VERSION,
        generation: "target".into(), cas_dir: cas.to_string_lossy().into_owned(),
        artifact_path: None, artifact_digest: None, copied_through_seq: None,
        created_at: "2026-11-20T00:00:00Z".into(),
    }
}

#[test]
fn borrowed_publication_rejects_wrong_cas_without_mutation() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let installation = root.join("installation");
        let cas = installation.join("cas");
        let target = installation.join("versions/target");
        let publication = StorePublication::acquire(root.join("other-cas")).unwrap();
        let domain = InstallationDomain::open(&installation).unwrap();
        let permit = domain.acquire().unwrap();
        let wrong = publication.validate_cas(&cas).unwrap_err();
        assert!(format!("{wrong:#}").contains(CAS_MISMATCH));
        // Each error must be the physical CAS mismatch, not a later schema,
        // permit, archive or selector error after opening the installed DB.
        let wrong = slots::write_slot_manifest_under(&target, &manifest(&cas), &publication, Some(&permit)).unwrap_err();
        assert!(format!("{wrong:#}").contains(CAS_MISMATCH));
        let wrong = slots::publish_current_slot_under(&installation, &target, &publication, &permit).unwrap_err();
        assert!(format!("{wrong:#}").contains(CAS_MISMATCH));
        let source_db = installation.join("versions/source").join(slots::SLOT_DB_FILENAME);
        let target_db = target.join(slots::SLOT_DB_FILENAME);
        for physical in [Some(&permit), None] {
            for rollback in [false, true] {
                let result = if rollback {
                    migration::import_rollback_delta_under(&source_db, &target_db, &cas, &publication, physical)
                } else {
                    migration::import_delta_under(&source_db, &target_db, &cas, &publication, physical)
                };
                assert!(format!("{:#}", result.unwrap_err()).contains(CAS_MISMATCH));
            }
        }
        // Missing-path rejection cannot create a new CAS or a marker either.
        let absent = root.join("must-remain-absent");
        assert!(publication.validate_cas(&absent).is_err());
        assert!(!absent.exists());
        permit.validate(&domain).unwrap();
        return;
    }
    let root = fixture();
    let installation = root.join("installation");
    // An independent exclusive owner of the requested CAS makes any hidden
    // shared reacquisition block; the owned-child deadline bounds that fault.
    let fence = CasWriteFence::acquire(&installation.join("cas"), 0).unwrap();
    let before = tree_bytes(&installation);
    run_bounded_child("borrowed_publication_rejects_wrong_cas_without_mutation", &root);
    assert_eq!(tree_bytes(&installation), before, "wrong-CAS validation changed installed bytes or membership");
    assert!(!installation.join("cas").join(MUTATION_LOCK_FILENAME).exists());
    assert!(!root.join("other-cas").join(MUTATION_LOCK_FILENAME).exists());
    drop(fence);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn borrowed_publication_accepts_physical_alias_with_original_permit() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        let installation = root.join("installation");
        let cas = installation.join("cas");
        let target = installation.join("versions/target");
        let publication = StorePublication::acquire(cas.clone()).unwrap();
        let domain = InstallationDomain::open(&installation).unwrap();
        let permit = domain.acquire().unwrap();
        let mut aliases = vec![cas.join(".")];
        #[cfg(unix)]
        {
            let alias = root.join("cas-alias");
            std::os::unix::fs::symlink(&cas, &alias).unwrap();
            aliases.push(alias);
        }
        let source_db = installation.join("versions/source").join(slots::SLOT_DB_FILENAME);
        let target_db = target.join(slots::SLOT_DB_FILENAME);
        let watermark = migration::watermark_of(&source_db).unwrap();
        let retained_ledger = migration::watermark_of(&target_db).unwrap().unwrap().ledger_id;
        for alias in aliases {
            publication.validate_cas(&alias).unwrap();
            slots::write_slot_manifest_under(&target, &manifest(&alias), &publication, Some(&permit)).unwrap();
            assert_eq!(slots::read_slot_manifest(&target).unwrap().cas_dir, alias.to_string_lossy());
            let report = migration::import_delta_under(&source_db, &target_db, &alias, &publication, Some(&permit)).unwrap();
            assert!(report.blocked_units.is_empty(), "{:?}", report.blocked_units);
            assert_eq!(report.watermark, watermark);
            assert_eq!(report.source_watermark_before, watermark);
            assert_eq!(report.source_watermark_after, watermark);
            assert_eq!(report.uncovered_source_commits, 0);
            let repeated = migration::import_rollback_delta_under(&source_db, &target_db, &alias, &publication, Some(&permit)).unwrap();
            assert!(repeated.blocked_units.is_empty(), "{:?}", repeated.blocked_units);
            assert_eq!(repeated.rows_written(), 0, "same-source repeated import must remain exact");
            permit.validate(&domain).unwrap();
        }
        slots::publish_current_slot_under(&installation.join("."), &target, &publication, &permit).unwrap();
        assert_eq!(slots::resolve_live_slot(&installation).unwrap(), Some(target.canonicalize().unwrap()));
        let retained = Store::open_under(target_db, &publication, Some(&permit)).unwrap();
        let entry: EntityId = fs::read_to_string(root.join("entry-id")).unwrap().parse().unwrap();
        let intent: EntityId = fs::read_to_string(root.join("intent-id")).unwrap().parse().unwrap();
        assert_eq!(retained.memory_entry(entry).unwrap().unwrap().content, NEW_FACT);
        assert_eq!(retained.memory_intent_state(intent).unwrap().as_deref(), Some("committed"));
        assert_eq!(migration::watermark_of(&target.join(slots::SLOT_DB_FILENAME)).unwrap().unwrap().ledger_id, retained_ledger);
        let busy = CasWriteFence::acquire(&cas, 0).unwrap_err();
        assert!(format!("{busy:#}").contains("CAS_ROOT_OWNER_BUSY"));
        assert!(!cas.join(MUTATION_LOCK_FILENAME).exists(), "read-only directory identity must not create a physical lock marker");
        return;
    }
    let root = fixture();
    run_bounded_child("borrowed_publication_accepts_physical_alias_with_original_permit", &root);
    // The child retired its original physical permit and publication owner.
    let fence = CasWriteFence::acquire(&root.join("installation/cas"), 0).unwrap();
    drop(fence);
    fs::remove_dir_all(root).unwrap();
}
