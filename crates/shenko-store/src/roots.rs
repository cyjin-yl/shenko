//! Shared-CAS publication ownership and authenticated excluded root enumeration.

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use shenko_types::content_root::{ContentRootGuard, ReaderPin, RootLease};
use shenko_types::error::ShenkoError;
use shenko_types::cognitive_root::{
    CognitiveRootContent, UnitSelectorSnapshot, COGNITIVE_ROOT_CODEC,
    COGNITIVE_ROOT_CODEC_MAJOR, HOME_CONTENT_CODEC, HOME_CONTENT_NAMESPACE,
    SOFTWARE_SLOT_CODEC, SOFTWARE_SLOT_NAMESPACE, UNIT_SELECTOR_CODEC,
    UNIT_SELECTOR_CODEC_MAJOR, UNIT_SELECTOR_NAMESPACE,
};
use shenko_types::object::{Digest, ObjectRef, SchemaRef};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use crate::cas::ContentStore;

/// The lease file that keeps writers and reclamation apart.
///
/// It lives in the content store's own directory: the store is what the
/// writers share, so the lease has to be shared with them.
pub const WRITE_LEASE_FILE: &str = ".write-lease";

const READ_PINS_DIR: &str = ".read-pins";
const MAX_READ_PIN_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ReadPinRecord<T> {
    schema_version: u32,
    pin_id: shenko_types::ids::EntityId,
    objects: T,
}

/// A live reader's exact object roots, shared with collectors in other processes.
///
/// The writer lease covers creation; the small locked record then keeps only
/// these objects and their closure alive without excluding all reclamation.
/// A crash releases its descriptor lock, so an abandoned pin is not a permanent
/// root. Pin records carry no recovery authority and are not archived facts.
#[derive(Debug)]
pub struct CasReadPin {
    file: fs::File,
    path: PathBuf,
}

impl CasReadPin {
    /// The caller must keep its publication lease from sealing the objects
    /// through this handoff. Acquiring a pin after an unleased seal is too late.
    pub fn acquire<'a>(
        cas: &ContentStore,
        objects: impl IntoIterator<Item = &'a ObjectRef>,
    ) -> Result<Self> {
        let _publication = CasWriteLease::acquire(cas.base())?;
        let objects: Vec<_> = objects.into_iter().collect();
        ensure!(!objects.is_empty(), "a reader pin needs an object");
        for object in &objects {
            cas.verified_object_path(object)?;
        }
        let directory = cas.base().join(READ_PINS_DIR);
        fs::create_dir_all(&directory)?;
        ensure!(fs::symlink_metadata(&directory)?.file_type().is_dir(), "reader pin directory is not a directory");
        let pin_id = shenko_types::ids::EntityId::new();
        let path = directory.join(pin_id.to_string());
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        let pin = Self { file, path };
        lock(&pin.file, LeaseMode::Shared, u64::MAX)?;
        serde_json::to_writer(&pin.file, &ReadPinRecord { schema_version: 1, pin_id, objects })?;
        ensure!(pin.file.metadata()?.len() <= MAX_READ_PIN_BYTES, "reader pin exceeds its metadata bound");
        Ok(pin)
    }
}

impl Drop for CasReadPin {
    fn drop(&mut self) {
        // Only the unpredictable record this pin created; the descriptor stays
        // locked until after removal. A failed cleanup leaves an inactive pin.
        let _ = fs::remove_file(&self.path);
    }
}

/// The closure of all live reader pins. The installation's exclusive CAS
/// fence must cover this snapshot through deletion, just as for ledger roots.
pub fn pinned_objects(cas: &ContentStore) -> Result<RootObjects> {
    use std::io::Read;
    let directory = cas.base().join(READ_PINS_DIR);
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RootObjects::default()),
        Err(error) => return Err(error.into()),
    };
    ensure!(fs::symlink_metadata(&directory)?.file_type().is_dir(), "reader pin directory is not a directory");
    let mut objects = Vec::new();
    for entry in entries {
        let entry = entry?;
        let pin_id: shenko_types::ids::EntityId = entry.file_name().to_str()
            .context("reader pin name is not UTF-8")?.parse().context("invalid reader pin name")?;
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // Open the reparse point itself, then reject it below.
            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = match options.open(entry.path()) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "reader pin is not a regular file");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
            ensure!(metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
                "reader pin is a reparse point");
        }
        let live = loop {
            match file.try_lock() {
                Ok(()) => break false,
                Err(std::fs::TryLockError::WouldBlock) => break true,
                Err(std::fs::TryLockError::Error(error))
                    if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        };
        if !live { continue; }
        ensure!(file.metadata()?.len() <= MAX_READ_PIN_BYTES, "reader pin exceeds its metadata bound");
        let record: ReadPinRecord<Vec<ObjectRef>> = serde_json::from_reader(
            std::io::BufReader::new(file.take(MAX_READ_PIN_BYTES + 1)),
        )?;
        ensure!(record.schema_version == 1 && record.pin_id == pin_id, "reader pin identity or version mismatch");
        ensure!(!record.objects.is_empty(), "a reader pin needs an object");
        objects.extend(record.objects);
    }
    Ok(object_closure(cas, objects))
}

/// One owner's write exclusion.
///
/// An exclusion is acquired before the root snapshot is taken and released
/// only after the collector has finished acting on that snapshot. While it is
/// held, the owner cannot publish a new reference, so the roots it reported
/// stay complete.
pub trait RootExclusion: Send + std::fmt::Debug {
    /// Identity of the excluded owner, for the refusal detail.
    fn label(&self) -> String;
}

/// An exclusive write lease on one ledger database.
///
/// The lease is a real SQLite write transaction: while it is held, no other
/// connection — in this process or in another one — can start a write
/// transaction on that database. A writer that tries is refused by SQLite
/// itself, so the collector never removes an object that a concurrent commit
/// is about to name.
pub struct SqliteWriteLease {
    db_path: PathBuf,
    conn: Connection,
}

impl SqliteWriteLease {
    /// Take the lease without waiting.
    ///
    /// A busy database is refused, never waited on: the collector reports the
    /// live writer instead of racing it. An absent database is refused too —
    /// a lease on a file that is not there excludes nothing.
    pub fn try_acquire(db_path: &Path) -> Result<Self> {
        ensure!(
            db_path.is_file(),
            "CAS_ROOT_OWNER_MISSING: the ledger database {} is not there to exclude",
            db_path.display()
        );
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open ledger database {}", db_path.display()))?;
        conn.busy_timeout(Duration::from_millis(0))
            .with_context(|| format!("configure ledger database {}", db_path.display()))?;
        conn.execute_batch("BEGIN IMMEDIATE").map_err(|error| {
            anyhow!(
                "CAS_ROOT_OWNER_BUSY: the ledger database {} has a live writer ({error})",
                db_path.display()
            )
        })?;
        Ok(Self {
            db_path: db_path.to_path_buf(),
            conn,
        })
    }

    /// The connection this lease holds. Reads through it see the same
    /// snapshot the write transaction protects.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The ledger sequence this lease observed.
    pub fn revision(&self) -> Result<u64> {
        let seq: i64 = self
            .conn
            .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| {
                row.get(0)
            })
            .map_err(|error| anyhow!("read ledger sequence: {error}"))?;
        Ok(u64::try_from(seq).unwrap_or_default())
    }

    /// The database this lease excludes.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

impl Drop for SqliteWriteLease {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("ROLLBACK");
    }
}

impl std::fmt::Debug for SqliteWriteLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteWriteLease")
            .field("db_path", &self.db_path)
            .finish_non_exhaustive()
    }
}

impl RootExclusion for SqliteWriteLease {
    fn label(&self) -> String {
        format!("ledger:{}", self.db_path.display())
    }
}

/// A writer's lease on the shared content store.
///
/// A writer holds the shared lease while it seals objects and publishes the
/// reference that names them. Reclamation holds the exclusive lease from the
/// root snapshot through the last delete, so a reference can never be
/// published against an object reclamation is about to remove. Without this
/// lease a content-addressed name that already existed keeps its old
/// modification time, and a collector that only re-reads modification times
/// would remove an object that just became reachable.
pub struct CasWriteLease {
    file: fs::File,
    identity: shenko_types::installation::InstallationDomain,
}

impl CasWriteLease {
    /// Take the shared lease. A writer waits for it: reclamation is finite.
    pub fn acquire(cas_base: &Path) -> Result<Self> {
        let identity = shenko_types::installation::InstallationDomain::open(&cas_base.canonicalize()?)?;
        let file = lease_file(cas_base)?;
        lock(&file, LeaseMode::Shared, u64::MAX)?;
        let lease = Self { file, identity };
        lease.validate_cas(cas_base)?;
        Ok(lease)
    }

    /// Authenticate already-held custody without acquiring a second lock or
    /// creating a marker beneath a physical/import/SQL exclusion.
    pub(crate) fn validate_cas(&self, cas_base: &Path) -> Result<()> {
        let other = shenko_types::installation::InstallationDomain::open(&cas_base.canonicalize()?)?;
        self.identity.validate_same_root(&other)
            .context("CAS publication custody belongs to another physical content store")
    }
}

impl std::fmt::Debug for CasWriteLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CasWriteLease").finish_non_exhaustive()
    }
}

impl RootExclusion for CasWriteLease {
    fn label(&self) -> String {
        "content-store:shared-writer".into()
    }
}

impl RootLease for CasWriteLease {}

impl ReaderPin for CasReadPin {}

/// The store's own implementation of the narrow durable-root capability.
///
/// The contract lives in `shenko-types`, so a crate that only publishes
/// references never depends on this one. The lease and the pin are the real
/// filesystem artifacts of this store: the advisory lease on the content
/// store's own directory, and the locked record under its read-pin
/// directory.
impl ContentRootGuard for ContentStore {
    fn acquire_write_lease(&self) -> Result<Box<dyn RootLease>, ShenkoError> {
        CasWriteLease::acquire(self.base())
            .map(|lease| Box::new(lease) as Box<dyn RootLease>)
            .map_err(|error| ShenkoError::storage(format!("CAS_ROOT_LEASE_UNAVAILABLE: {error:#}")))
    }

    fn acquire_reader_pin(
        &self,
        objects: &[&ObjectRef],
    ) -> Result<Box<dyn ReaderPin>, ShenkoError> {
        CasReadPin::acquire(self, objects.iter().copied())
            .map(|pin| Box::new(pin) as Box<dyn ReaderPin>)
            .map_err(|error| ShenkoError::storage(format!("CAS_ROOT_READER_PIN_UNAVAILABLE: {error:#}")))
    }
}

/// Reclamation's exclusive lease on the shared content store.
pub struct CasWriteFence {
    file: fs::File,
}

impl CasWriteFence {
    /// Take the exclusive lease.
    ///
    /// `wait_ms` bounds how long a live writer may delay reclamation. Zero
    /// refuses immediately; the collector then reports the live writer
    /// instead of waiting inside a task.
    pub fn acquire(cas_base: &Path, wait_ms: u64) -> Result<Self> {
        let file = lease_file(cas_base)?;
        lock(&file, LeaseMode::Exclusive, wait_ms)?;
        Ok(Self { file })
    }
}

impl std::fmt::Debug for CasWriteFence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CasWriteFence").finish_non_exhaustive()
    }
}

impl RootExclusion for CasWriteFence {
    fn label(&self) -> String {
        "content-store:exclusive-fence".into()
    }
}

/// The lease file of one content store.
fn lease_file(cas_base: &Path) -> Result<fs::File> {
    let path = cas_base.join(WRITE_LEASE_FILE);
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("open content store write lease {}", path.display()))
}

/// The platform's real shared/exclusive file reservation.
#[derive(Clone, Copy)]
enum LeaseMode {
    Shared,
    Exclusive,
}

/// Take a native file lease. As with InstallationDomain, std supplies flock on
/// Unix and LockFileEx on Windows; unsupported platforms return an error.
/// No backend grants an unlocked successful capability.
///
/// `wait_ms == u64::MAX` uses the platform's blocking operation. Finite fences
/// use nonblocking operations and preserve the collector's refusal deadline.
fn lock(file: &fs::File, mode: LeaseMode, wait_ms: u64) -> Result<()> {
    if wait_ms == u64::MAX {
        loop {
            let result = match mode {
                LeaseMode::Shared => file.lock_shared(),
                LeaseMode::Exclusive => file.lock(),
            };
            match result {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => bail!("CAS_ROOT_LEASE_UNAVAILABLE: content store write lease: {error}"),
            }
        }
    }
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let result = match mode {
            LeaseMode::Shared => file.try_lock_shared(),
            LeaseMode::Exclusive => file.try_lock(),
        };
        match result {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {},
            Err(std::fs::TryLockError::Error(error))
                if error.kind() == std::io::ErrorKind::Interrupted => {},
            Err(std::fs::TryLockError::Error(error)) => {
                bail!("CAS_ROOT_LEASE_UNAVAILABLE: content store write lease: {error}");
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "CAS_ROOT_OWNER_BUSY: a writer holds the content store's write lease; reclamation must not race it"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The CAS objects one ledger database still reaches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootObjects {
    /// Objects that are present in the CAS and reachable from the ledger.
    pub digests: BTreeSet<Digest>,
    /// Required objects that are absent, unreadable or fail integrity checks.
    ///
    /// A missing digest is reported, never rooted: rooting it would make
    /// every reclamation refuse a store that already lost an object, and
    /// dropping it silently would hide the loss.
    pub missing: BTreeSet<Digest>,
}

impl RootObjects {
    /// Merge another database's objects into this set.
    pub fn merge(&mut self, other: RootObjects) {
        self.digests.extend(other.digests);
        self.missing.extend(other.missing);
    }
}

/// Every CAS object a ledger database still reaches.
///
/// The walk is read-only: it allocates no export order, writes no
/// fingerprint and mutates no row. It reads the replicated archive rows and
/// the refused-receipt conflicts — which are deliberately not part of the
/// archive — in one consistent snapshot, then expands the JSON closure of
/// every object those rows name.
pub fn referenced_objects(conn: &Connection, cas: &ContentStore) -> Result<RootObjects> {
    let rows = crate::cognition::archive_rows(conn)?;
    let mut roots = prepared_transition_roots(conn)?;
    // These are installation-local owners, outside the replicated catalogue.
    // Their original readers authenticate the SQL binding and CAS evidence
    // through this same excluded connection; no Store opening or recovery runs.
    roots.extend(crate::local_model_health::local_model_health_roots(conn, cas)?);
    if let Some(local) = crate::local_installation::read_provisioning(conn, cas)? {
        roots.push(local.home_manifest_ref);
    }
    let mut external: Vec<Digest> = Vec::new();
    let mut missing = BTreeSet::new();
    for row in &rows {
        for (column, value) in &row.values {
            if let Some(value) = value.as_deref() {
                if row.table == "cognitive_root_nodes" && column == "content_record_json" {
                    let content: shenko_types::cognition::ContentRecord = serde_json::from_str(value)?;
                    roots.push(cognitive_root_reference(content.object));
                    continue;
                }
                if row.table == "channel_record_migrations"
                    && matches!(column.as_str(), "source_digest" | "target_digest")
                {
                    // Intent-only migrations have no CAS rewrite. A decision
                    // rewrite names both JSON objects as plain TEXT, including
                    // the original creation decision still read by obligations.
                    if value.is_empty() {
                        ensure!(
                            row.value("record_kind") == Some("subject_intent"),
                            "CAS_ROOT_REFERENCE_INVALID: migration {} is missing {column}",
                            row.key
                        );
                        continue;
                    }
                    let digest = Digest(value.to_owned());
                    ensure!(
                        digest.canonical_blake3_hex().is_some(),
                        "CAS_ROOT_REFERENCE_INVALID: migration {} has invalid {column}",
                        row.key
                    );
                    if let Some(mut object) = bare_object(cas, &digest) {
                        object.media_type = "application/json".into();
                        roots.push(object);
                    } else {
                        missing.insert(digest);
                    }
                    continue;
                }
                let (declared, optional) = crate::cognition::declared_text_object_roots(
                    [(row.table.as_str(), column.as_str(), value)],
                );
                roots.extend(declared);
                external.extend(optional);
            }
        }
    }
    for row in crate::receipt_admission::conflict_rows(conn)? {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&row) {
            crate::cognition::collect_references(&json, &mut roots, &mut external);
        }
    }
    cognitive_root_rows(conn, &mut roots, &mut external)?;
    for digest in &external {
        if let Some(object) = bare_object(cas, digest) {
            roots.push(object);
        }
    }
    let mut objects = object_closure(cas, roots);
    objects.missing.extend(missing);
    Ok(objects)
}

/// Durable preparations exist before any applied inbox event. Read their
/// typed owner references in the caller's excluded SQLite snapshot.
fn prepared_transition_roots(conn: &Connection) -> Result<Vec<ObjectRef>> {
    let mut statement = conn.prepare(
        "SELECT action_id, admission, transition_ref FROM control_prepared_transitions ORDER BY rowid",
    )?;
    let mut rows = statement.query([])?;
    let mut roots = Vec::new();
    while let Some(row) = rows.next()? {
        let action: String = row.get(0)?;
        let admission: shenko_types::authority::CommitAdmission =
            serde_json::from_str(&row.get::<_, String>(1)?)?;
        let transition: ObjectRef = serde_json::from_str(&row.get::<_, String>(2)?)?;
        ensure!(admission.action_id.to_string() == action,
            "CAS_ROOT_REFERENCE_INVALID: prepared action identity differs");
        roots.extend([admission.payload_ref, transition]);
    }
    Ok(roots)
}

/// Existing main-only cognitive-root owners are not replication catalogue
/// additions. Their immutable rows join the same excluded GC snapshot.
fn cognitive_root_rows(
    conn: &Connection,
    roots: &mut Vec<ObjectRef>,
    external: &mut Vec<Digest>,
) -> Result<()> {
    for (table, query, columns) in [
        ("cognitive_root_nodes", "SELECT content_record_json, fence_json, reinstated_from_json FROM cognitive_root_nodes ORDER BY root_revision",
            &["content_record_json", "fence_json", "reinstated_from_json"][..]),
        ("cognitive_root_requests", "SELECT content_digest, target_json FROM cognitive_root_requests ORDER BY request_id",
            &["content_digest", "target_json"][..]),
        ("cognitive_root_effects", "SELECT disposition FROM cognitive_root_effects ORDER BY id",
            &["disposition"][..]),
        ("cognitive_root_applications", "SELECT state_json FROM cognitive_root_applications ORDER BY id",
            &["state_json"][..]),
    ] {
        let mut statement = conn.prepare(query)?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            for (index, column) in columns.iter().enumerate() {
                if let Some(value) = row.get::<_, Option<String>>(index)? {
                    if table == "cognitive_root_nodes" && *column == "content_record_json" {
                        let content: shenko_types::cognition::ContentRecord = serde_json::from_str(&value)?;
                        roots.push(cognitive_root_reference(content.object));
                    } else {
                        let (declared, optional) = crate::cognition::declared_text_object_roots(
                            [(table, *column, value.as_str())],
                        );
                        roots.extend(declared);
                        external.extend(optional);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Complete ObjectRef-shaped members of a serialized value, using the same
/// typed-member rule as the closure collector. Bare digest and codec IDs do
/// not name objects. The caller still verifies availability in its own CAS.
pub fn json_object_references(value: &serde_json::Value) -> Vec<ObjectRef> {
    let mut roots = Vec::new();
    crate::cognition::collect_object_references(value, &mut roots);
    roots
}

/// A root node supplies schema authority even if the bytes were first
/// registered as an opaque content alias in the shared content table.
pub(crate) fn cognitive_root_reference(mut object: ObjectRef) -> ObjectRef {
    object.media_type = "application/json".into();
    object.codec = SchemaRef {
        name: COGNITIVE_ROOT_CODEC.into(),
        major: COGNITIVE_ROOT_CODEC_MAJOR,
        digest: Digest::blake3(COGNITIVE_ROOT_CODEC.as_bytes()),
    };
    object
}

/// The runtime manifest carries its module digest and length separately;
/// memories and continuation blobs already contain complete ObjectRefs.
/// Only format /2 promises that the publisher sealed the module in CAS.
/// Historical /1 records the externally supplied module's identity instead.
#[derive(Deserialize)]
struct WasmStateManifestModule<'a> {
    #[serde(borrow)]
    format: &'a str,
    module: Digest,
    module_bytes: u64,
}

/// The verified closure of the given roots inside the CAS.
pub fn object_closure(cas: &ContentStore, roots: Vec<ObjectRef>) -> RootObjects {
    object_closure_with(cas, roots, |_, _| {})
}

/// Visit verified declarations without a second CAS traversal. The boolean
/// distinguishes schemas that declare required children from generic JSON.
/// Collectors need no metadata allocation; archive writers retain declarations.
pub(crate) fn object_closure_with(
    cas: &ContentStore,
    roots: Vec<ObjectRef>,
    on_verified: impl FnMut(&ObjectRef, bool),
) -> RootObjects {
    let mut budget = ClosureBudget::default();
    object_closure_visit(cas, roots, &mut budget, on_verified)
}

/// The same schema-aware traversal with finite transport inventory limits.
/// Account for each distinct object's declared bytes before reading its payload,
/// including schema checks that inspect a child before it reaches the queue.
pub(crate) fn object_closure_with_limits(
    cas: &ContentStore,
    roots: Vec<ObjectRef>,
    max_objects: usize,
    max_bytes: u64,
    on_verified: impl FnMut(&ObjectRef, bool),
) -> Result<RootObjects> {
    let mut budget = ClosureBudget {
        limits: if max_objects == usize::MAX && max_bytes == u64::MAX { None } else { Some((max_objects, max_bytes)) },
        ..Default::default()
    };
    let closure = object_closure_visit(cas, roots, &mut budget, on_verified);
    if let Some(error) = budget.error { bail!(error); }
    Ok(closure)
}

#[derive(Default)]
struct ClosureBudget {
    limits: Option<(usize, u64)>,
    objects: BTreeMap<Digest, u64>,
    bytes: u64,
    error: Option<&'static str>,
}

impl ClosureBudget {
    fn queue(&mut self, count: usize) -> bool {
        if self.limits.is_some_and(|(max_objects, _)| count > max_objects) {
            self.error = Some("CAS closure reference budget exceeded");
        }
        self.error.is_none()
    }

    fn admit(&mut self, digest: &Digest, bytes: u64) -> bool {
        let Some((max_objects, max_bytes)) = self.limits else { return true; };
        if self.error.is_some() { return false; }
        let previous = self.objects.get(digest).copied().unwrap_or(0);
        if self.objects.contains_key(digest) && bytes <= previous { return true; }
        if !self.objects.contains_key(digest) && self.objects.len() >= max_objects {
            self.error = Some("CAS closure object budget exceeded");
            return false;
        }
        let Some(total) = self.bytes.checked_add(bytes - previous).filter(|total| *total <= max_bytes) else {
            self.error = Some("CAS closure byte budget exceeded");
            return false;
        };
        self.objects.insert(digest.clone(), bytes);
        self.bytes = total;
        true
    }


}

fn object_closure_visit(
    cas: &ContentStore,
    roots: Vec<ObjectRef>,
    budget: &mut ClosureBudget,
    mut on_verified: impl FnMut(&ObjectRef, bool),
) -> RootObjects {
    let mut out = RootObjects::default();
    if !budget.queue(roots.len()) { return out; }
    // A later declared schema can reveal more roots than an earlier opaque
    // or generic JSON alias. Cache integrity and expansion authority separately.
    let mut verified: BTreeMap<Digest, (u64, u8)> = BTreeMap::new();
    let mut queue: VecDeque<ObjectRef> = roots.into_iter().collect();
    while let Some(object) = queue.pop_front() {
        if !budget.queue(queue.len().saturating_add(1)) || !budget.admit(&object.digest, object.bytes) { break; }
        let expansion = if crate::cognition::is_json_reference(&object.media_type) {
            match (object.codec.name.as_str(), object.codec.major) {
                (COGNITIVE_ROOT_CODEC, COGNITIVE_ROOT_CODEC_MAJOR) => 2,
                (UNIT_SELECTOR_CODEC, UNIT_SELECTOR_CODEC_MAJOR) => 4,
                ("shenko-wasm-state", 1) => 8,
                _ => 1,
            }
        } else {
            0
        };
        if let Some((bytes, expanded)) = verified.get(&object.digest) {
            if *bytes != object.bytes {
                out.missing.insert(object.digest);
                continue;
            }
            if expansion == 0 || expanded & expansion != 0 {
                continue;
            }
        }
        // Refuse inconsistent lengths before allocating or hashing payloads.
        if budget.limits.is_some() && cas.object_size(&object.digest) != Some(object.bytes) {
            out.missing.insert(object.digest);
            continue;
        }
        if expansion == 0 {
            // Opaque artifacts may be large: verify through the existing
            // streaming reader instead of allocating their full contents.
            if cas.verified_object_path(&object).is_err() {
                out.missing.insert(object.digest);
                continue;
            }
            on_verified(&object, false);
            out.digests.insert(object.digest.clone());
            verified.insert(object.digest, (object.bytes, 0));
            continue;
        }
        let bytes = match cas.read_object(&object.digest) {
            Ok(bytes) if bytes.len() as u64 == object.bytes
                && Digest::blake3(&bytes) == object.digest => bytes,
            _ => {
                out.missing.insert(object.digest);
                continue;
            }
        };
        let json: serde_json::Value = match serde_json::from_slice(&bytes) {
            Ok(json) => json,
            Err(_) => {
                out.missing.insert(object.digest);
                continue;
            }
        };
        on_verified(&object, expansion > 1);
        out.digests.insert(object.digest.clone());
        let entry = verified.entry(object.digest.clone()).or_insert((object.bytes, 0));
        let already_json = entry.1 != 0;
        entry.1 |= expansion;
        if expansion == 2 {
            match serde_json::from_value::<CognitiveRootContent>(json) {
                Ok(content) if content.validate().is_ok() => {
                    enqueue_cognitive_root(cas, &content, &mut queue, &mut out);
                }
                _ => {
                    out.missing.insert(object.digest);
                    continue;
                }
            }
            continue;
        } else if expansion == 4 {
            match serde_json::from_value::<UnitSelectorSnapshot>(json) {
                Ok(snapshot) if snapshot.schema_version == UnitSelectorSnapshot::SCHEMA_VERSION => {
                    for unit in snapshot.entries {
                        enqueue_required_object(
                            cas, &unit.artifact_digest, None, &mut queue, &mut out,
                        );
                    }
                }
                _ => {
                    out.missing.insert(object.digest);
                    continue;
                }
            }
            continue;
        } else if expansion == 8 {
            let module = match WasmStateManifestModule::deserialize(&json) {
                Ok(module) => module,
                Err(_) => {
                    out.missing.insert(object.digest);
                    continue;
                }
            };
            match module.format {
                "shenko-wasm-state/1" => {},
                "shenko-wasm-state/2" => queue.push_back(ObjectRef {
                    digest: module.module,
                    bytes: module.module_bytes,
                    media_type: "application/octet-stream".into(),
                    store_namespace: object.store_namespace.clone(),
                    locator: String::new(),
                    codec: SchemaRef {
                        name: "raw".into(), major: 1, digest: Digest::blake3(b"raw"),
                    },
                }),
                _ => {
                    out.missing.insert(object.digest);
                    continue;
                }
            }
            // Memories and continuation bytes carry complete references in
            // both formats. Bare module/graph identities in /1 grant no CAS
            // publication authority, so do not promote those strings to roots.
            queue.extend(json_object_references(&json));
            continue;
        }
        if already_json {
            continue;
        }
        let mut nested: Vec<ObjectRef> = Vec::new();
        let mut external: Vec<Digest> = Vec::new();
        crate::cognition::collect_references(&json, &mut nested, &mut external);
        queue.extend(nested);
        for digest in external {
            if let Some(object) = bare_object(cas, &digest) {
                queue.push_back(object);
            }
        }
    }
    out
}

/// Verify the declared children before a new cognitive-root name is published.
pub(crate) fn cognitive_root_objects(cas: &ContentStore, content: &CognitiveRootContent) -> RootObjects {
    let mut pending = VecDeque::new();
    let mut declared = RootObjects::default();
    enqueue_cognitive_root(cas, content, &mut pending, &mut declared);
    let mut closure = object_closure(cas, pending.into());
    closure.missing.extend(declared.missing);
    closure
}

fn enqueue_cognitive_root(
    cas: &ContentStore,
    content: &CognitiveRootContent,
    pending: &mut VecDeque<ObjectRef>,
    objects: &mut RootObjects,
) {
    for (digest, namespace, codec) in [
        (&content.home.manifest, HOME_CONTENT_NAMESPACE, HOME_CONTENT_CODEC),
        (&content.software.manifest, SOFTWARE_SLOT_NAMESPACE, SOFTWARE_SLOT_CODEC),
        (&content.units.snapshot, UNIT_SELECTOR_NAMESPACE, UNIT_SELECTOR_CODEC),
    ] { enqueue_required_object(cas, digest, Some((namespace, codec, 1)), pending, objects); }
    if let Some(artifact) = &content.software.artifact_digest {
        enqueue_required_object(cas, artifact, None, pending, objects);
    }
    if let Some(runtime) = content.runtime.captured() {
        enqueue_required_object(
            cas, &runtime.manifest, Some(("seed-wasm-state", "shenko-wasm-state", 1)),
            pending, objects,
        );
        enqueue_required_object(cas, &runtime.module, None, pending, objects);
        for memory in &runtime.memories {
            enqueue_required_object(cas, memory, None, pending, objects);
        }
    }
}

fn enqueue_required_object(
    cas: &ContentStore,
    digest: &Digest,
    schema: Option<(&str, &str, u32)>,
    pending: &mut VecDeque<ObjectRef>,
    objects: &mut RootObjects,
) {
    let Some(mut object) = bare_object(cas, digest) else {
        objects.missing.insert(digest.clone());
        return;
    };
    if let Some((namespace, codec, major)) = schema {
        object.media_type = "application/json".into();
        object.store_namespace = namespace.into();
        object.codec = SchemaRef {
            name: codec.into(), major, digest: Digest::blake3(codec.as_bytes()),
        };
    }
    pending.push_back(object);
}

/// A digest that exists in the CAS, seen as a plain object.
fn bare_object(cas: &ContentStore, digest: &Digest) -> Option<ObjectRef> {
    if !cas.object_exists(digest) {
        return None;
    }
    let bytes = cas.object_size(digest)?;
    Some(ObjectRef {
        digest: digest.clone(),
        bytes,
        media_type: "application/octet-stream".into(),
        store_namespace: "replicated".into(),
        locator: String::new(),
        codec: SchemaRef {
            name: "raw".into(),
            major: 1,
            digest: Digest::blake3(b"raw"),
        },
    })
}

