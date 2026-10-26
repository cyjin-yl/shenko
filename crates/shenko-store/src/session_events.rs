//! Read-only access to committed raw session events — SPEC §12.1, §12.3.
//! Writes stay on the ledger commit path; this adds no schema or writer.
use rusqlite::{OptionalExtension, Row, params};

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

use crate::store::Store;

/// One committed `session_events` row, exactly as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSessionEvent {
    pub event_id: EntityId,
    pub session_id: EntityId,
    pub seq: i64,
    pub event_type: String,
    pub role: String,
    pub data: String,
}

/// Bounded chronological read parameters over one raw session.
///
/// Empty type/role filters mean all values. The JSON1 predicates keep filter
/// values as data; neither a query nor a role can become SQL syntax.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionEventFilter {
    pub from_seq: i64,
    pub to_seq: i64,
    pub limit: usize,
    pub event_types: Vec<String>,
    pub roles: Vec<String>,
}

impl Default for SessionEventFilter {
    fn default() -> Self {
        Self {
            from_seq: 0,
            to_seq: i64::MAX,
            limit: 20,
            event_types: vec![],
            roles: vec![],
        }
    }
}

pub(crate) const COLUMNS: &str = "event_id, session_id, seq, event_type, role, data";
const ALIASED_COLUMNS: &str = "e.event_id, e.session_id, e.seq, e.event_type, e.role, e.data";

pub(crate) fn row(r: &Row<'_>) -> rusqlite::Result<(String, String, i64, String, String, String)> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
    ))
}

pub(crate) fn parse_row(
    (event_id, session_id, seq, event_type, role, data): (
        String,
        String,
        i64,
        String,
        String,
        String,
    ),
) -> Result<StoredSessionEvent, ShenkoError> {
    let id = |s: &str| {
        s.parse::<EntityId>()
            .map_err(|e| ShenkoError::storage(format!("bad id {s}: {e}")))
    };
    Ok(StoredSessionEvent {
        event_id: id(&event_id)?,
        session_id: id(&session_id)?,
        seq,
        event_type,
        role,
        data,
    })
}

impl Store {
    /// Events of `session_id` with `from <= seq <= to`, in seq order.
    pub fn session_events_range(
        &self,
        session_id: EntityId,
        from: i64,
        to: i64,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM session_events
                 WHERE session_id = ?1 AND seq BETWEEN ?2 AND ?3 ORDER BY seq"
            ))
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![session_id.to_string(), from, to], row)
            .map_err(|e| ShenkoError::storage(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(parse_row).collect()
    }

    pub fn session_event(
        &self,
        event_id: EntityId,
    ) -> Result<Option<StoredSessionEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let found = conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM session_events WHERE event_id = ?1"),
                params![event_id.to_string()],
                row,
            )
            .optional()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        drop(conn);
        found.map(parse_row).transpose()
    }

    /// Highest committed seq of `session_id`, if any.
    pub fn session_leaf_seq(&self, session_id: EntityId) -> Result<Option<i64>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT MAX(seq) FROM session_events WHERE session_id = ?1",
            params![session_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| ShenkoError::storage(e.to_string()))
    }

    /// List committed raw events in stable seq order.
    pub fn list_session_events(
        &self,
        session_id: EntityId,
        filter: &SessionEventFilter,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let (types, roles, limit) = filter_json(filter)?;
        let sql = format!(
            "SELECT {ALIASED_COLUMNS} FROM session_events e
             WHERE e.session_id = ?1 AND e.seq BETWEEN ?2 AND ?3
               AND (json_array_length(?4) = 0 OR e.event_type IN
                    (SELECT value FROM json_each(?4)))
               AND (json_array_length(?5) = 0 OR e.role IN
                    (SELECT value FROM json_each(?5)))
             ORDER BY e.seq LIMIT ?6"
        );
        self.query_events(
            &sql,
            params![
                session_id.to_string(),
                filter.from_seq,
                filter.to_seq,
                types,
                roles,
                limit
            ],
        )
    }

    /// Search the raw payload without making the derived index a truth source.
    /// CJK and mixed queries use literal substring matching because unicode61
    /// does not segment Han text. Other queries use FTS5, with the same raw
    /// substring fallback if the index is unavailable or damaged.
    pub fn search_session_events(
        &self,
        session_id: EntityId,
        query: &str,
        filter: &SessionEventFilter,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(vec![]);
        }
        if crate::memory::contains_cjk(query) {
            return self.search_session_events_like(session_id, query, filter);
        }
        match self.search_session_events_fts(session_id, query, filter) {
            Ok(events) => Ok(events),
            Err(error) => {
                tracing::debug!(%error, "raw history FTS failed; using raw substring fallback");
                self.search_session_events_like(session_id, query, filter)
            }
        }
    }

    fn search_session_events_fts(
        &self,
        session_id: EntityId,
        query: &str,
        filter: &SessionEventFilter,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let (types, roles, limit) = filter_json(filter)?;
        let quoted = format!("\"{}\"", query.replace('"', "\"\""));
        let sql = format!(
            "SELECT {ALIASED_COLUMNS}
               FROM session_events_fts f
               JOIN session_events e ON e.rowid = f.rowid
             WHERE session_events_fts MATCH ?1
               AND e.session_id = ?2 AND e.seq BETWEEN ?3 AND ?4
               AND (json_array_length(?5) = 0 OR e.event_type IN
                    (SELECT value FROM json_each(?5)))
               AND (json_array_length(?6) = 0 OR e.role IN
                    (SELECT value FROM json_each(?6)))
             ORDER BY e.seq LIMIT ?7"
        );
        self.query_events(
            &sql,
            params![
                quoted,
                session_id.to_string(),
                filter.from_seq,
                filter.to_seq,
                types,
                roles,
                limit
            ],
        )
    }

    fn search_session_events_like(
        &self,
        session_id: EntityId,
        query: &str,
        filter: &SessionEventFilter,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let (types, roles, limit) = filter_json(filter)?;
        let sql = format!(
            "SELECT {ALIASED_COLUMNS} FROM session_events e
             WHERE e.session_id = ?1 AND e.seq BETWEEN ?2 AND ?3
               AND e.data LIKE ?4 ESCAPE '\\'
               AND (json_array_length(?5) = 0 OR e.event_type IN
                    (SELECT value FROM json_each(?5)))
               AND (json_array_length(?6) = 0 OR e.role IN
                    (SELECT value FROM json_each(?6)))
             ORDER BY e.seq LIMIT ?7"
        );
        self.query_events(
            &sql,
            params![
                session_id.to_string(),
                filter.from_seq,
                filter.to_seq,
                crate::memory::like_pattern(query),
                types,
                roles,
                limit
            ],
        )
    }

    fn query_events(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<StoredSessionEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare_cached(sql)
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        let rows = stmt
            .query_map(params, row)
            .map_err(|e| ShenkoError::storage(e.to_string()))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| ShenkoError::storage(e.to_string()))?;
        drop(stmt);
        drop(conn);
        rows.into_iter().map(parse_row).collect()
    }
}

fn filter_json(filter: &SessionEventFilter) -> Result<(String, String, i64), ShenkoError> {
    Ok((
        serde_json::to_string(&filter.event_types)
            .map_err(|e| ShenkoError::other(e.to_string()))?,
        serde_json::to_string(&filter.roles).map_err(|e| ShenkoError::other(e.to_string()))?,
        i64::try_from(filter.limit)
            .map_err(|_| ShenkoError::other("history limit exceeds SQLite INTEGER"))?,
    ))
}
