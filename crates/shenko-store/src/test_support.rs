//! Observe the SQLite writer owned by a test installation.

use rusqlite::hooks::{AuthContext, Authorization};

use crate::Store;

pub fn set_sqlite_authorizer<F>(store: &Store, authorizer: Option<F>)
where
    F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
{
    store.ledger.conn.lock().authorizer(authorizer);
}

/// Work actually performed by one SQLite statement on the Store writer.
/// Rows count SQLite TRACE_ROW deliveries, including the row on which a
/// borrowed-value capture refuses; they do not count retained owned cells.
#[derive(Debug, serde::Serialize)]
pub struct SqliteStatementWork {
    pub sql: String,
    pub rows: u64,
    pub fullscan_steps: i32,
    pub vm_steps: i32,
    pub profile_nanoseconds: u64,
}

#[derive(Default)]
struct SqliteWork {
    rows: std::collections::BTreeMap<usize, u64>,
    statements: Vec<SqliteStatementWork>,
}

struct SqliteWorkObserver<'a> {
    store: &'a Store,
    state: Box<SqliteWork>,
}

unsafe extern "C" fn observe_sqlite_work(
    event: std::ffi::c_uint,
    context: *mut std::ffi::c_void,
    statement: *mut std::ffi::c_void,
    detail: *mut std::ffi::c_void,
) -> std::ffi::c_int {
    use rusqlite::ffi;
    // SQLite invokes this synchronously under the Store connection lock.
    // The observer owns the stable Box until it detaches this callback.
    let state = unsafe { &mut *context.cast::<SqliteWork>() };
    if event == ffi::SQLITE_TRACE_ROW as u32 {
        *state.rows.entry(statement as usize).or_default() += 1;
    } else if event == ffi::SQLITE_TRACE_PROFILE as u32 {
        let stmt = statement.cast::<ffi::sqlite3_stmt>();
        // PROFILE supplies a live statement and a pointer to elapsed ns.
        // Reset statement counters so cached executions remain independent.
        let (sql, fullscan_steps, vm_steps, profile_nanoseconds) = unsafe {
            let sql = ffi::sqlite3_sql(stmt);
            (
                if sql.is_null() { String::new() } else {
                    std::ffi::CStr::from_ptr(sql).to_string_lossy().into_owned()
                },
                ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1),
                ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_VM_STEP, 1),
                *detail.cast::<u64>(),
            )
        };
        state.statements.push(SqliteStatementWork {
            sql, rows: state.rows.remove(&(statement as usize)).unwrap_or(0),
            fullscan_steps, vm_steps, profile_nanoseconds,
        });
    }
    0
}

impl Drop for SqliteWorkObserver<'_> {
    fn drop(&mut self) {
        let conn = self.store.ledger.conn.lock();
        // Detach before freeing the callback context, also during unwinding.
        unsafe { rusqlite::ffi::sqlite3_trace_v2(conn.handle(), 0, None, std::ptr::null_mut()); }
    }
}

/// Observe a normal public Store consumer without changing its authorizer,
/// preupdate callback, budgets or connection ownership. This module exists
/// only with test-support; SQLite already supplies these counters and hooks.
/// Use on an otherwise idle fixture with no existing trace observer.
pub fn with_sqlite_work_observation<T>(store: &Store, run: impl FnOnce() -> T)
    -> (T, Vec<SqliteStatementWork>)
{
    use rusqlite::ffi;
    let mut observer = SqliteWorkObserver { store, state: Box::default() };
    {
        let conn = store.ledger.conn.lock();
        // Cached statements may predate this window. Discard their old work
        // before installing the observer, without executing any SQL.
        unsafe {
            let db = conn.handle();
            let mut stmt = ffi::sqlite3_next_stmt(db, std::ptr::null_mut());
            while !stmt.is_null() {
                ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_FULLSCAN_STEP, 1);
                ffi::sqlite3_stmt_status(stmt, ffi::SQLITE_STMTSTATUS_VM_STEP, 1);
                stmt = ffi::sqlite3_next_stmt(db, stmt);
            }
            let status = ffi::sqlite3_trace_v2(db,
                (ffi::SQLITE_TRACE_ROW | ffi::SQLITE_TRACE_PROFILE) as u32,
                Some(observe_sqlite_work),
                (&mut *observer.state as *mut SqliteWork).cast());
            assert_eq!(status, ffi::SQLITE_OK, "install SQLite work observer");
        }
    }
    let result = run();
    let statements = std::mem::take(&mut observer.state.statements);
    drop(observer);
    (result, statements)
}
