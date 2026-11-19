//! Read-only retention inventory of one committed Store database.
//!
//! This is deliberately not a GC producer or a deletion permit. Other slots,
//! Node's independent object database and seal-to-root publication exclusion
//! are outside this snapshot. The check consumer reports that scope explicitly.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, types::ValueRef};
use serde::Serialize;
use shenko_types::{commit::CommitRef, ids::{EntityId, LedgerSeq}, object::{Digest, ObjectRef}};

use crate::{ContentStore, Store};

mod artifacts;
mod carriers;
mod closure;

/// Finite audit budgets, independent of mutation/admission budgets. Exceeding
/// one makes the report incomplete; it never rejects a business transaction.
#[derive(Debug, Clone, Serialize)]
pub struct RetentionLimits {
    pub rows: u64,
    pub row_bytes: u64,
    pub json_bytes: u64,
    pub read_bytes: u64,
    pub references: usize,
    pub depth: usize,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self { rows: 1_000_000, row_bytes: 8 * 1024 * 1024,
            json_bytes: 64 * 1024 * 1024, read_bytes: 8 * 1024 * 1024 * 1024,
            references: 65_536, depth: 64 }
    }
}

impl RetentionLimits {
    fn validate(&self) -> Result<()> {
        let max = Self::default();
        ensure!(self.rows > 0 && self.rows <= max.rows
            && self.row_bytes > 0 && self.row_bytes <= max.row_bytes
            && self.json_bytes > 0 && self.json_bytes <= max.json_bytes
            && self.read_bytes > 0 && self.read_bytes <= max.read_bytes
            && self.references > 0 && self.references <= max.references
            && self.depth > 0 && self.depth <= max.depth,
            "retention audit limits must be positive and within the reader's finite maxima");
        Ok(())
    }
}

/// Provenance contains identities and hashes, never the original row payload,
/// a historical locator, an account credential or a provider response.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RetentionOrigin {
    Row { table: String, column: String, ordinal: u64, value_digest: Digest },
    Object { digest: Digest },
    Schema { table: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionReference {
    pub digest: Digest,
    /// Typed ObjectRefs are required CAS members. The storage classification
    /// distinguishes local artifact locators from implementation identities
    /// and Node-inline or manifest-assembled artifacts. No classification
    /// authorizes fetching a historical locator.
    pub object: Option<ObjectRef>,
    /// Some normalized cognition columns retain a CAS member by digest even
    /// before another carrier supplies its full ObjectRef. Other bare digests
    /// can name external files or implementation identities.
    pub required: bool,
    pub storage: RetentionStorage,
    pub origin: RetentionOrigin,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionStorage {
    CasObject,
    CasDigest,
    CasArtifact,
    OptionalDigest,
    ExternalArtifact,
    NodeInlineArtifact,
    ManifestAssembledArtifact,
}

#[derive(Debug, Clone, Serialize)]
pub struct RetentionGap {
    pub kind: String,
    pub origin: RetentionOrigin,
    pub digest: Option<Digest>,
}

#[derive(Debug, Serialize)]
pub struct RetainedTable {
    pub name: String,
    pub rows_read: u64,
    /// The exact ordered SQL values seen by this snapshot, with type/length
    /// framing. None means a bounded or unsupported row was omitted.
    pub values_digest: Option<Digest>,
}

#[derive(Debug, Serialize)]
pub struct RetainedObjectSnapshot {
    pub schema_version: u32,
    pub scope: String,
    pub database: Option<PathBuf>,
    pub cas_directory: PathBuf,
    pub ledger_id: EntityId,
    pub source_through: LedgerSeq,
    pub retained_commit_watermarks: Vec<CommitRef>,
    pub sqlite_schema_cookie: i64,
    pub schema_digest: Digest,
    pub limits: RetentionLimits,
    pub rows_read: u64,
    pub bytes_read: u64,
    pub json_bytes_read: u64,
    pub tables: Vec<RetainedTable>,
    pub derived_projections: Vec<String>,
    pub references: Vec<RetentionReference>,
    pub verified_objects: BTreeSet<Digest>,
    pub external_or_absent_digests: BTreeSet<Digest>,
    pub gaps: Vec<RetentionGap>,
    pub omitted_gap_count: u64,
    /// Completeness concerns this database's supported reference carriers.
    /// It does NOT attest all-slot coverage or a stable deletion epoch.
    pub complete: bool,
}

struct Reader<'a> {
    cas: &'a ContentStore,
    report: RetainedObjectSnapshot,
    queue: VecDeque<RetentionReference>,
    expanded: BTreeSet<(Digest, closure::JsonReading)>,
    measured: BTreeMap<Digest, u64>,
    exhausted: bool,
}

impl Reader<'_> {
    fn gap(&mut self, kind: &str, origin: RetentionOrigin, digest: Option<Digest>) {
        self.report.complete = false;
        if self.report.gaps.len() < 256 {
            self.report.gaps.push(RetentionGap { kind: kind.into(), origin, digest });
        } else {
            self.report.omitted_gap_count += 1;
        }
    }

    fn charge(&mut self, bytes: u64, origin: &RetentionOrigin) -> bool {
        match self.report.bytes_read.checked_add(bytes) {
            Some(total) if total <= self.report.limits.read_bytes => {
                self.report.bytes_read = total;
                true
            }
            _ => {
                self.gap("read_budget_exceeded", origin.clone(), None);
                self.exhausted = true;
                false
            }
        }
    }
}

impl Store {
    /// One live handle's consistent, committed SQLite view. CAS verification
    /// is read-only but has no cross-process deletion exclusion.
    pub fn retained_object_snapshot(&self, limits: RetentionLimits) -> Result<RetainedObjectSnapshot> {
        limits.validate()?;
        let catalogue = current_catalogue()?;
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction()?;
        let report = read_snapshot(&tx, &self.cas, limits, &catalogue)?;
        ensure!(report.ledger_id == self.ledger.ledger_id(), "retention snapshot ledger changed");
        tx.commit()?;
        Ok(report)
    }

    /// The CLI's zero-touch path: use the existing stable DB/WAL tuple reader,
    /// not Store::open, migrations, bootstrap preparation or a writer takeover.
    pub fn inspect_retained_objects(
        database: &Path, cas_directory: &Path, limits: RetentionLimits,
    ) -> Result<RetainedObjectSnapshot> {
        limits.validate()?;
        let catalogue = current_catalogue()?;
        let cas = ContentStore::open_existing(cas_directory.to_path_buf())?;
        let mut report = Self::inspect_database(database, |conn| {
            read_snapshot(conn, &cas, limits.clone(), &catalogue)
        })?;
        report.database = Some(database.to_path_buf());
        Ok(report)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TableLayout {
    kind: String,
    columns: Vec<(String, String, i64)>,
}

fn quoted(name: &str) -> String { format!("\"{}\"", name.replace('"', "\"\"")) }

fn table_layouts(conn: &Connection) -> Result<BTreeMap<String, TableLayout>> {
    let mut query = conn.prepare("SELECT name,type FROM pragma_table_list WHERE schema='main' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let mut rows = query.query([])?;
    let mut tables = BTreeMap::new();
    while let Some(row) = rows.next()? {
        ensure!(tables.len() < 512, "retention schema exceeds the table budget");
        let name: String = row.get(0)?;
        let kind: String = row.get(1)?;
        ensure!(name.len() <= 256, "retention table name exceeds its budget");
        let mut columns = conn.prepare(&format!("PRAGMA main.table_xinfo({})", quoted(&name)))?;
        let mut column_rows = columns.query([])?;
        let mut fields = Vec::new();
        while let Some(column) = column_rows.next()? {
            ensure!(fields.len() < 256, "retention schema exceeds the column budget");
            let field: String = column.get(1)?;
            let declared: String = column.get(2)?;
            ensure!(field.len() <= 256 && declared.len() <= 256, "retention column metadata exceeds its budget");
            fields.push((field, declared.to_uppercase(), column.get(6)?));
        }
        // Additive migrations can leave an equivalent column in another
        // physical position. Names/types/hidden semantics, not creation order,
        // define the supported carrier layout.
        fields.sort();
        tables.insert(name, TableLayout { kind, columns: fields });
    }
    Ok(tables)
}

fn current_catalogue() -> Result<BTreeMap<String, TableLayout>> {
    let conn = Connection::open_in_memory()?;
    crate::store::initialize_schema(&conn)?;
    table_layouts(&conn)
}

fn frame(hash: &mut blake3::Hasher, tag: &[u8], bytes: &[u8]) {
    hash.update(tag);
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn schema_digest(conn: &Connection) -> Result<Digest> {
    let mut query = conn.prepare("SELECT type,name,tbl_name,sql FROM main.sqlite_schema ORDER BY type,name")?;
    let mut rows = query.query([])?;
    let mut hash = blake3::Hasher::new();
    let mut bytes = 0u64;
    while let Some(row) = rows.next()? {
        for index in 0..4 {
            match row.get_ref(index)? {
                ValueRef::Null => frame(&mut hash, b"null", &[]),
                ValueRef::Text(text) => {
                    bytes = bytes.checked_add(text.len() as u64).context("schema length overflow")?;
                    ensure!(bytes <= 8 * 1024 * 1024, "retention schema exceeds its byte budget");
                    frame(&mut hash, b"text", text);
                }
                _ => anyhow::bail!("invalid SQLite schema carrier"),
            }
        }
    }
    Ok(Digest(format!("blake3:{}", hash.finalize().to_hex())))
}

fn read_snapshot(
    conn: &Connection, cas: &ContentStore, limits: RetentionLimits,
    catalogue: &BTreeMap<String, TableLayout>,
) -> Result<RetainedObjectSnapshot> {
    let encoding: String = conn.pragma_query_value(None, "encoding", |row| row.get(0))?;
    ensure!(encoding == "UTF-8", "retention audit does not support this database text encoding");
    let ledger: String = conn.query_row("SELECT ledger_id FROM ledger_identity WHERE singleton=1", [], |row| row.get(0))?;
    let through: u64 = conn.query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| row.get(0))?;
    let mut watermarks = conn.prepare("SELECT ledger_id,MAX(seq) FROM commit_log GROUP BY ledger_id ORDER BY ledger_id")?;
    let mut rows = watermarks.query([])?;
    let mut retained_commit_watermarks = Vec::new();
    while let Some(row) = rows.next()? {
        ensure!(retained_commit_watermarks.len() < 65_536, "retention commit origins exceed their budget");
        let id: String = row.get(0)?;
        retained_commit_watermarks.push(CommitRef::new(id.parse()?, LedgerSeq(row.get(1)?)));
    }
    drop(rows);
    drop(watermarks);
    let layouts = table_layouts(conn)?;
    let mut reader = Reader {
        cas,
        report: RetainedObjectSnapshot {
            schema_version: 1, scope: "single_committed_database".into(), database: None,
            cas_directory: cas.base().to_path_buf(), ledger_id: ledger.parse()?,
            source_through: LedgerSeq(through), retained_commit_watermarks,
            sqlite_schema_cookie: conn.pragma_query_value(None, "schema_version", |row| row.get(0))?,
            schema_digest: schema_digest(conn)?, limits, rows_read: 0, bytes_read: 0, json_bytes_read: 0,
            tables: Vec::new(), derived_projections: Vec::new(),
            references: Vec::new(), verified_objects: BTreeSet::new(),
            external_or_absent_digests: BTreeSet::new(), gaps: Vec::new(), omitted_gap_count: 0,
            complete: true,
        },
        queue: VecDeque::new(), expanded: BTreeSet::new(), measured: BTreeMap::new(), exhausted: false,
    };
    for (name, layout) in &layouts {
        let origin = RetentionOrigin::Schema { table: name.clone() };
        if catalogue.get(name) != Some(layout) {
            reader.gap("unsupported_table_layout", origin, None);
            continue;
        }
        if layout.kind != "table" {
            // Only known rebuildable projections are excluded, and their
            // names stay visible rather than silently disappearing.
            reader.report.derived_projections.push(name.clone());
            continue;
        }
        if reader.exhausted {
            reader.gap("table_not_scanned_after_budget", origin, None);
            continue;
        }
        read_table(conn, name, layout, &mut reader)?;
    }
    for (name, layout) in catalogue {
        if layout.kind == "table" && !layouts.contains_key(name) {
            reader.gap("missing_supported_table", RetentionOrigin::Schema { table: name.clone() }, None);
        }
    }
    reader.expand_closure()?;
    Ok(reader.report)
}

fn read_table(conn: &Connection, name: &str, layout: &TableLayout, reader: &mut Reader<'_>) -> Result<()> {
    let columns: Vec<_> = layout.columns.iter().map(|(name, _, _)| quoted(name)).collect();
    // octet_length(column) uses SQLite's length metadata before loading large
    // text/BLOB bodies. The entire row is refused before any body is selected.
    let size = columns.iter().map(|column| format!("COALESCE(octet_length({column}),0)"))
        .collect::<Vec<_>>().join("+");
    let values = columns.iter().map(|column| format!("CASE WHEN ({size})<=?1 THEN {column} END"))
        .collect::<Vec<_>>().join(",");
    let mut query = conn.prepare(&format!("SELECT ({size}),{values} FROM main.{}", quoted(name)))?;
    let mut rows = query.query([reader.report.limits.row_bytes])?;
    let mut count = 0;
    let mut hash = blake3::Hasher::new();
    let mut complete = true;
    while let Some(row) = rows.next()? {
        let origin = RetentionOrigin::Schema { table: name.to_owned() };
        if reader.exhausted {
            reader.gap("table_not_scanned_after_budget", origin, None);
            complete = false;
            break;
        }
        if reader.report.rows_read >= reader.report.limits.rows {
            reader.gap("row_budget_exceeded", origin, None);
            reader.exhausted = true;
            complete = false;
            break;
        }
        count += 1;
        reader.report.rows_read += 1;
        let size: u64 = row.get(0)?;
        if size > reader.report.limits.row_bytes {
            reader.gap("row_size_exceeded", origin, None);
            complete = false;
            continue;
        }
        if !reader.charge(size, &origin) { complete = false; break; }
        for (index, (column, _, _)) in layout.columns.iter().enumerate() {
            frame(&mut hash, b"column", column.as_bytes());
            let value = row.get_ref(index + 1)?;
            match value {
                ValueRef::Null => frame(&mut hash, b"null", &[]),
                ValueRef::Integer(number) => frame(&mut hash, b"integer", &number.to_le_bytes()),
                ValueRef::Real(number) => frame(&mut hash, b"real", &number.to_bits().to_le_bytes()),
                ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
                    let is_blob = matches!(value, ValueRef::Blob(_));
                    frame(&mut hash, if is_blob { b"blob" } else { b"text" }, bytes);
                    let origin = RetentionOrigin::Row { table: name.into(), column: column.clone(),
                        ordinal: count, value_digest: Digest::blake3(bytes) };
                    if is_blob {
                        if name == "context_window_derivatives" && column == "bytes" {
                            // Embedded derivatives are data, not CAS pointers.
                            // Their declared JSON form can still retain typed refs.
                            let media = layout.columns.iter().position(|(name, _, _)| name == "media_type")
                                .context("derivative media type missing")?;
                            let media: String = row.get(media + 1)?;
                            if closure::is_json(&media) { reader.json(bytes, &origin, true, 0, None); }
                        } else {
                            reader.gap("unsupported_blob_carrier", origin, None);
                        }
                    } else if carriers::required_digest(name, column) {
                        reader.required_digest(bytes, &origin);
                    } else if name == "memory_intents" && column == "content_ref" {
                        reader.memory_locator(bytes, &origin);
                    } else {
                        reader.json(bytes, &origin, carriers::required_json(name, column), 0, None);
                    }
                }
            }
        }
        frame(&mut hash, b"row_end", &[]);
    }
    reader.report.tables.push(RetainedTable { name: name.into(), rows_read: count,
        values_digest: complete.then(|| Digest(format!("blake3:{}", hash.finalize().to_hex()))) });
    Ok(())
}
