//! Observe the SQLite writer owned by a test installation.

use rusqlite::hooks::{AuthContext, Authorization};

use crate::Store;

pub fn set_sqlite_authorizer<F>(store: &Store, authorizer: Option<F>)
where
    F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
{
    store.ledger.conn.lock().authorizer(authorizer);
}
