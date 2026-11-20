//! Independent-process lock observations around real archive publication and
//! delayed reads. Parent runs these only after integrating the foundation.
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};
use shenko_store::memory::MemoryEntry;
use shenko_store::roots::CasWriteFence;
use shenko_store::{MemoryIntentRecord, Store, StorePublication};
use shenko_types::ids::EntityId;
use shenko_types::installation::InstallationDomain;
use shenko_types::object::Digest;

const MODE: &str = "SHENKO_COGNITION_OWNER_CHILD_MODE";
const ROOT: &str = "SHENKO_COGNITION_OWNER_CHILD_ROOT";
const CAS: &str = "SHENKO_COGNITION_OWNER_PROBE_CAS";
const BODY: &str = "the original imported content survives delayed application";

fn phase(root: &Path, name: impl std::fmt::Display) {
    eprintln!("cognition-owner unix_ms={} pid={} phase={name} root={}",
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis(),
        std::process::id(), root.display());
}

fn bounded(selector: &str, mode: &str, root: &Path, cas: Option<&Path>) {
    phase(root, format_args!("child-{mode}-spawn"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", selector, "--nocapture"])
        .env(MODE, mode).env(ROOT, root)
        .stdout(Stdio::inherit()).stderr(Stdio::inherit());
    if let Some(cas) = cas { command.env(CAS, cas); }
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                phase(root, format_args!("child-{mode}-reaped"));
                assert!(status.success(), "child {mode} failed: {status}; retained fixture {}", root.display());
                return;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let killed = child.kill();
                let reaped = child.wait();
                panic!("child {mode}: {result:?}; kill={killed:?}; reap={reaped:?}; retained fixture {}", root.display());
            }
        }
    }
}

fn probe(selector: &str, root: &Path, cas: &Path, busy: bool) {
    bounded(selector, if busy { "busy" } else { "free" }, root, Some(cas));
}

// The same native selector supplies a bounded worker and independent fence
// observers. A probe never opens Store beneath its exclusive CAS fence.
fn enter(selector: &str, baselines: &[(&str, &str)]) -> Option<PathBuf> {
    match std::env::var(MODE).ok().as_deref() {
        Some("worker") => Some(PathBuf::from(std::env::var_os(ROOT).unwrap())),
        Some(mode @ ("busy" | "free")) => {
            let cas = PathBuf::from(std::env::var_os(CAS).unwrap());
            let result = CasWriteFence::acquire(&cas, 0);
            if mode == "busy" {
                assert!(format!("{:#}", result.unwrap_err()).contains("CAS_ROOT_OWNER_BUSY"));
            } else {
                drop(result.unwrap());
            }
            None
        }
        None => {
            // Keep FULL-durability SQLite fixture I/O on the persistent
            // project bind used by this exact compiled source.
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../build/cognition-publication-ownership-fixtures")
                .join(format!("shenko-cognition-owner-{}", EntityId::new()));
            fs::create_dir_all(&root).unwrap();
            phase(&root, "root-created");
            // Only empty durable schemas are prepared here. Every Store and
            // its startup CAS/physical exclusions retire before child spawn.
            // The worker still performs all payload seals, memory writes,
            // archive creation, opens/reopens and ownership observations.
            for &(database, cas) in baselines {
                let db = root.join(database);
                fs::create_dir_all(db.parent().unwrap()).unwrap();
                phase(&root, format_args!("baseline-open-begin database={database}"));
                drop(Store::open(db, root.join(cas)).unwrap());
                phase(&root, format_args!("baseline-open-end database={database}"));
            }
            phase(&root, "baselines-ready");
            bounded(selector, "worker", &root, None);
            fs::remove_dir_all(root).unwrap();
            None
        }
        Some(mode) => panic!("unexpected native child mode {mode}"),
    }
}

fn source(root: &Path) -> (Store, EntityId, Digest) {
    phase(root, "source-open-begin");
    let source = Store::open(root.join("source.db"), root.join("source-cas")).unwrap();
    phase(root, "source-open-end");
    let owner = StorePublication::acquire(source.cas.base().to_owned()).unwrap();
    phase(root, "source-seal-begin");
    let object = source.cas.seal_object(BODY.as_bytes(), "text/plain", "memory", "raw", 1).unwrap().object;
    phase(root, "source-seal-end");
    let agent = EntityId::new();
    let intent = EntityId::new();
    let entry_id = EntityId::new();
    let record = MemoryIntentRecord::accepted(intent, agent, "save", "personal", "", Some(object.locator.clone()), Some(entry_id));
    phase(root, "source-memory-admit-begin");
    source.admit_memory_intent(&record, Some(&object)).unwrap();
    phase(root, "source-memory-admit-end");
    let mut entry = MemoryEntry::new(agent, BODY);
    entry.entry_id = entry_id;
    entry.kind = "note".into();
    phase(root, "source-memory-commit-begin");
    source.commit_memory_intent(&entry, intent).unwrap();
    phase(root, "source-memory-commit-end");
    drop(owner);
    (source, entry_id, object.digest)
}

struct FirstRead<'a> {
    bytes: Cursor<&'a [u8]>,
    selector: &'a str,
    root: &'a Path,
    cas: &'a Path,
    observed: bool,
}

impl Read for FirstRead<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if !self.observed {
            probe(self.selector, self.root, self.cas, true);
            self.observed = true;
        }
        self.bytes.read(bytes)
    }
}

#[test]
fn prepared_import_owns_first_read_delay_apply_abandon_and_errors() {
    const SELECTOR: &str = "prepared_import_owns_first_read_delay_apply_abandon_and_errors";
    let Some(root) = enter(SELECTOR, &[
        ("source.db", "source-cas"),
        ("target.db", "target-cas"),
        ("other.db", "other-cas"),
    ]) else { return; };
    let (source, entry_id, digest) = source(&root);
    let mut archive = Vec::new();
    phase(&root, "source-archive-begin");
    source.write_cognition_archive(&Default::default(), None, &mut archive).unwrap();
    phase(&root, "source-archive-end");
    drop(source);
    let target_cas = root.join("target-cas");
    let target_db = root.join("target.db");
    phase(&root, "target-open-begin");
    let target = Store::open(target_db.clone(), target_cas.clone()).unwrap();
    phase(&root, "target-open-end");
    let options = CognitionImportOptions::default();
    let mut first = FirstRead { bytes: Cursor::new(&archive), selector: SELECTOR,
        root: &root, cas: &target_cas, observed: false };
    phase(&root, "first-prepare-begin");
    let prepared = target.prepare_cognition_import(&mut first, &options).unwrap();
    phase(&root, "first-prepare-end");
    assert!(first.observed);
    probe(SELECTOR, &root, &target_cas, true);
    assert_eq!(target.cas.read_object(&digest).unwrap(), BODY.as_bytes());
    drop(prepared);
    probe(SELECTOR, &root, &target_cas, false);
    phase(&root, "truncated-prepare-begin");
    assert!(target.prepare_cognition_import(&archive[..archive.len() / 2], &options).is_err());
    phase(&root, "truncated-prepare-end");
    probe(SELECTOR, &root, &target_cas, false);
    let prepared = target.prepare_cognition_import(&archive[..], &options).unwrap();
    phase(&root, "other-open-begin");
    let other = Store::open(root.join("other.db"), root.join("other-cas")).unwrap();
    phase(&root, "other-open-end");
    assert!(other.apply_prepared_cognition_import_under(prepared, None).is_err());
    probe(SELECTOR, &root, &target_cas, false);
    let prepared = target.prepare_cognition_import(&archive[..], &options).unwrap();
    phase(&root, "apply-prepared-begin");
    let outcome = target.apply_prepared_cognition_import_under(prepared, None).unwrap();
    phase(&root, "apply-prepared-end");
    assert!(outcome.gaps.iter().all(|gap| !gap.blocking));
    assert!(outcome.conflicts.is_empty());
    probe(SELECTOR, &root, &target_cas, false);
    drop(target);
    phase(&root, "target-reopen-begin");
    let reopened = Store::open(target_db, target_cas).unwrap();
    phase(&root, "target-reopen-end");
    assert_eq!(reopened.memory_entry(entry_id).unwrap().unwrap().content, BODY);
    assert_eq!(reopened.cas.read_object(&digest).unwrap(), BODY.as_bytes());
}

#[test]
fn borrowed_import_and_export_retain_original_chain_owner() {
    const SELECTOR: &str = "borrowed_import_and_export_retain_original_chain_owner";
    let Some(root) = enter(SELECTOR, &[
        ("source.db", "source-cas"),
        ("installation/shenko.db", "installation/cas"),
    ]) else { return; };
    let (source, entry_id, digest) = source(&root);
    phase(&root, "source-export-begin");
    let export = source.export_cognition(&CognitionSnapshotOptions::default()).unwrap();
    phase(&root, "source-export-end");
    assert!(export.manifest.objects.iter().any(|object| object.digest == digest));
    drop(source);
    probe(SELECTOR, &root, &root.join("source-cas"), true);
    let mut archive = Vec::new();
    export.write_archive(&mut archive).unwrap();
    let mut repeated = Vec::new();
    export.write_archive(&mut repeated).unwrap();
    assert_eq!(archive, repeated);
    drop(export);
    probe(SELECTOR, &root, &root.join("source-cas"), false);

    let installation = root.join("installation");
    fs::create_dir_all(&installation).unwrap();
    let cas = installation.join("cas");
    let db = installation.join("shenko.db");
    let owner = StorePublication::acquire(cas.clone()).unwrap();
    let domain = InstallationDomain::open(&installation).unwrap();
    let permit = domain.acquire().unwrap();
    phase(&root, "installed-target-open-begin");
    let target = Store::open_under(db.clone(), &owner, Some(&permit)).unwrap();
    phase(&root, "installed-target-open-end");
    phase(&root, "borrowed-prepare-begin");
    let prepared = target.prepare_cognition_import_under(&archive[..], &Default::default(), &owner).unwrap();
    phase(&root, "borrowed-prepare-end");
    drop(owner);
    probe(SELECTOR, &root, &cas, true);
    permit.validate(&domain).unwrap();
    phase(&root, "borrowed-apply-begin");
    target.apply_prepared_cognition_import_under(prepared, Some(&permit)).unwrap();
    phase(&root, "borrowed-apply-end");
    permit.validate(&domain).unwrap();
    drop(permit);
    probe(SELECTOR, &root, &cas, false);
    let owner = StorePublication::acquire(cas.clone()).unwrap();
    let permit = domain.acquire().unwrap();
    phase(&root, "borrowed-export-begin");
    let export = target.export_cognition_under(&Default::default(), &owner, Some(&permit)).unwrap();
    phase(&root, "borrowed-export-end");
    drop(target);
    drop(permit);
    drop(owner);
    probe(SELECTOR, &root, &cas, true);
    struct FailedWriter;
    impl Write for FailedWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> { Err(io::Error::other("original consumer failure")) }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    assert!(export.write_archive(FailedWriter).is_err());
    probe(SELECTOR, &root, &cas, true);
    let mut readback = Vec::new();
    export.write_archive(&mut readback).unwrap();
    drop(export);
    probe(SELECTOR, &root, &cas, false);
    phase(&root, "installed-target-reopen-begin");
    let reopened = Store::open(db, cas).unwrap();
    phase(&root, "installed-target-reopen-end");
    assert_eq!(reopened.memory_entry(entry_id).unwrap().unwrap().content, BODY);
    assert_eq!(reopened.cas.read_object(&digest).unwrap(), BODY.as_bytes());
}
