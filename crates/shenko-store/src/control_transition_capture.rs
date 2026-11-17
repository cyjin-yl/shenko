//! Physical row-write observation for the canonical savepoint. SQLite's
//! preupdate callback includes ordinary/trigger/REPLACE/WITHOUT ROWID writes
//! and incremental blob writes. Postimages are read by their original keys
//! before rollback, so a blob callback's delete-shaped event is not mistaken
//! for an actual row deletion.
use super::*;
use rusqlite::hooks::{Action, PreUpdateCase};

pub(super) struct TouchedRow {
    pub key_values: Vec<Cell>,
    pub before: Option<Vec<Cell>>,
}

pub(super) struct Writes {
    pub tables: BTreeSet<String>,
    pub rows: BTreeMap<String, BTreeMap<String, TouchedRow>>,
    pub remaining: usize,
    pub refusal: Option<ShenkoError>,
}

impl Default for Writes {
    fn default() -> Self {
        Self {
            tables: BTreeSet::new(),
            rows: BTreeMap::new(),
            remaining: MAX_CAPTURE_BYTES,
            refusal: None,
        }
    }
}

pub(super) struct Observer<'a> {
    connection: &'a Connection,
    writes: Arc<Mutex<Writes>>,
}

impl<'a> Observer<'a> {
    pub fn start(
        connection: &'a Connection,
        tables: BTreeMap<String, Table>,
        projections: BTreeSet<String>,
        retain_rows: bool,
    ) -> Self {
        let writes = Arc::new(Mutex::new(Writes::default()));
        let observed = Arc::clone(&writes);
        connection.preupdate_hook(Some(
            move |_action: Action, database: &str, name: &str, case: &PreUpdateCase| {
                let mut writes = observed.lock();
                if writes.refusal.is_some() {
                    return;
                }
                if let Err(error) =
                    writes.observe(database, name, case, &tables, &projections, retain_rows)
                {
                    writes.refusal = Some(error);
                }
            },
        ));
        Self { connection, writes }
    }

    pub fn finish(self) -> Writes {
        let writes = Arc::clone(&self.writes);
        drop(self);
        std::mem::take(&mut *writes.lock())
    }
}

impl Drop for Observer<'_> {
    fn drop(&mut self) {
        self.connection
            .preupdate_hook(None::<fn(Action, &str, &str, &PreUpdateCase)>);
    }
}

impl Writes {
    fn observe(
        &mut self,
        database: &str,
        name: &str,
        case: &PreUpdateCase,
        tables: &BTreeMap<String, Table>,
        projections: &BTreeSet<String>,
        retain_rows: bool,
    ) -> Result<(), ShenkoError> {
        if database != "main" {
            return Err(conflict("physical write accessed a non-main database"));
        }
        // An invalidation projection is exempt from frozen facts only when
        // the real row trigger maintains it. This independent check remains
        // active if a callback removes the SQL authorizer before a direct write.
        if execution_projection(name) {
            let depth = match case {
                PreUpdateCase::Delete(access) => access.get_query_depth(),
                PreUpdateCase::Insert(access) => access.get_query_depth(),
                PreUpdateCase::Update { old_value_accessor, .. } => old_value_accessor.get_query_depth(),
                PreUpdateCase::Unknown => 0,
            };
            if depth <= 0 {
                return Err(conflict("direct physical write to a local execution projection"));
            }
        }
        self.tables.insert(name.to_owned());
        if !retain_rows {
            return Ok(());
        }
        let Some(table) = tables.get(name) else {
            if projections.contains(name) {
                return Ok(());
            }
            return Err(conflict(format!(
                "physical write touched unrepresented table {name}"
            )));
        };
        if table.key.is_empty() {
            return Err(conflict(format!("transition touched unkeyed table {name}")));
        }
        if matches!(case, PreUpdateCase::Unknown) {
            return Err(conflict("unknown physical row-write kind"));
        }
        let rows = self.rows.entry(name.to_owned()).or_default();
        for old in [true, false] {
            let count = match (case, old) {
                (PreUpdateCase::Delete(access), true) => access.get_column_count(),
                (PreUpdateCase::Insert(access), false) => access.get_column_count(),
                (
                    PreUpdateCase::Update {
                        old_value_accessor: access,
                        ..
                    },
                    true,
                ) => access.get_column_count(),
                (
                    PreUpdateCase::Update {
                        new_value_accessor: access,
                        ..
                    },
                    false,
                ) => access.get_column_count(),
                _ => continue,
            };
            if usize::try_from(count).ok() != Some(table.columns.len()) {
                return Err(conflict(
                    "physical row differs from the captured table layout",
                ));
            }
            charge_image(
                &mut self.remaining,
                table
                    .columns
                    .len()
                    .saturating_mul(std::mem::size_of::<Cell>()),
            )?;
            let mut key_values = vec![Cell::Null; table.columns.len()];
            for &index in &table.key {
                key_values[index] = captured_cell(value(case, old, index)?, &mut self.remaining)?;
            }
            let key = key_with_budget(table, &key_values, &mut self.remaining)?;
            if rows.contains_key(&key) {
                continue;
            }
            charge_image(&mut self.remaining, 128)?;
            let before = if old {
                charge_image(
                    &mut self.remaining,
                    table
                        .columns
                        .len()
                        .saturating_mul(std::mem::size_of::<Cell>()),
                )?;
                let mut cells = Vec::with_capacity(table.columns.len());
                for index in 0..table.columns.len() {
                    cells.push(if table.local_sequence == Some(index) {
                        Cell::LocalSequence
                    } else {
                        captured_cell(value(case, true, index)?, &mut self.remaining)?
                    });
                }
                Some(cells)
            } else {
                None
            };
            rows.insert(key, TouchedRow { key_values, before });
        }
        Ok(())
    }
}

fn value(case: &PreUpdateCase, old: bool, index: usize) -> Result<ValueRef<'_>, ShenkoError> {
    let index = i32::try_from(index).map_err(invalid)?;
    match (case, old) {
        (PreUpdateCase::Delete(access), true) => access.get_old_column_value(index),
        (PreUpdateCase::Insert(access), false) => access.get_new_column_value(index),
        (
            PreUpdateCase::Update {
                old_value_accessor: access,
                ..
            },
            true,
        ) => access.get_old_column_value(index),
        (
            PreUpdateCase::Update {
                new_value_accessor: access,
                ..
            },
            false,
        ) => access.get_new_column_value(index),
        _ => return Err(invalid("physical row accessor has no requested image")),
    }
    .map_err(storage_error)
}

fn captured_cell(value: ValueRef<'_>, remaining: &mut usize) -> Result<Cell, ShenkoError> {
    charge_image(
        remaining,
        match value {
            ValueRef::Text(bytes) | ValueRef::Blob(bytes) => bytes.len(),
            _ => 0,
        },
    )?;
    Cell::read(value)
}
