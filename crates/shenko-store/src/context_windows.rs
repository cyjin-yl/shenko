//! Committed context windows.
//!
//! The store keeps each scope's window chain opaquely (policy crates own the
//! body format) and enforces the commit transaction: a window is appended
//! only while its expected predecessor is still the head. Derivatives live in
//! the same transaction as their window, so a window never commits without
//! the frames it references. Appends ride the ledger's single mutation line.
use rusqlite::{OptionalExtension, params};
use anyhow::{Context as _, ensure};
use std::path::Path;

use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;

use crate::store::Store;

pub(crate) const CONTEXT_WINDOWS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS context_windows (
    window_id TEXT PRIMARY KEY,
    scope_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    body TEXT NOT NULL,
    commit_seq INTEGER NOT NULL,
    UNIQUE (scope_id, generation)
);
CREATE TABLE IF NOT EXISTS context_window_derivatives (
    window_id TEXT NOT NULL REFERENCES context_windows(window_id),
    idx INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (window_id, idx)
);
CREATE TABLE IF NOT EXISTS context_window_scopes (
    scope_id TEXT PRIMARY KEY,
    model_generation INTEGER NOT NULL,
    profile_digest TEXT
);
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDerivative {
    pub media_type: String,
    pub blake3: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContextWindow {
    pub window_id: EntityId,
    pub generation: i64,
    pub body: String,
    pub derivatives: Vec<StoredDerivative>,
}

/// Stored derivative identity without its payload. `bytes` is the durable
/// `length(bytes)` of the stored BLOB; the recorded `blake3` was verified by
/// the writer at commit, so metadata readers never materialize the payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDerivativeMeta {
    pub media_type: String,
    pub blake3: String,
    pub bytes: u64,
}

/// A committed window's stored facts without derivative payloads. `body`
/// holds every other committed fact (policy, base, entries, call-state
/// baselines, cost) verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWindowMeta {
    pub window_id: EntityId,
    pub generation: i64,
    pub body: String,
    pub derivatives: Vec<StoredDerivativeMeta>,
}

/// One bounded page of the scope's window metadata, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWindowPage {
    /// Windows committed under this scope, regardless of this page's bounds.
    pub total: i64,
    /// More committed windows exist below this page's oldest generation.
    pub has_older: bool,
    pub windows: Vec<StoredWindowMeta>,
}

/// Subject binding, model binding and committed window metadata observed in
/// one stable private database snapshot. No live Store is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectContextWindowInspection {
    pub scope_id: EntityId,
    pub model_generation: i64,
    pub profile_digest: Option<String>,
    pub page: StoredWindowPage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowAppend {
    Appended,
    HeadChanged,
    ModelGenerationChanged { current: i64 },
}

fn storage(e: impl std::fmt::Display) -> ShenkoError {
    ShenkoError::storage(e.to_string())
}

fn read_derivatives(conn: &rusqlite::Connection, window_id: EntityId) -> Result<Vec<StoredDerivative>, ShenkoError> {
    conn.prepare_cached(
        "SELECT media_type, blake3, bytes FROM context_window_derivatives
         WHERE window_id = ?1 ORDER BY idx",
    )
    .map_err(storage)?
    .query_map(params![window_id.to_string()], |r| {
        Ok(StoredDerivative { media_type: r.get(0)?, blake3: r.get(1)?, bytes: r.get(2)? })
    })
    .map_err(storage)?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(storage)
}

fn read_window(
    conn: &rusqlite::Connection,
    id: String,
    generation: i64,
    body: String,
) -> Result<StoredContextWindow, ShenkoError> {
    let window_id = id.parse::<EntityId>().map_err(storage)?;
    let derivatives = read_derivatives(conn, window_id)?;
    for d in &derivatives {
        if blake3::hash(&d.bytes).to_hex().as_str() != d.blake3 {
            return Err(storage(format!("derivative of window {window_id} fails blake3")));
    }

        }
    Ok(StoredContextWindow { window_id, generation, body, derivatives })
}

/// Read metadata through the caller's transaction; never fetch derivative BLOBs.
fn read_window_meta_page(
    conn: &rusqlite::Connection,
    scope_id: EntityId,
    before: Option<i64>,
    limit: i64,
) -> Result<StoredWindowPage, ShenkoError> {
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM context_windows WHERE scope_id = ?1",
            params![scope_id.to_string()],
            |r| r.get(0),
        )
        .map_err(storage)?;
    let limit = limit.max(0);
    // One extra row proves whether older windows remain without a second
    // scan or another round-trip.
    let rows = conn
        .prepare_cached(
            "SELECT window_id, generation, body FROM context_windows
             WHERE scope_id = ?1 AND generation < ?2
             ORDER BY generation DESC LIMIT ?3",
        )
        .map_err(storage)?
        .query_map(
            params![scope_id.to_string(), before.unwrap_or(i64::MAX), limit.saturating_add(1)],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(storage)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(storage)?;
    let has_older = rows.len() as i64 > limit;
    let mut windows = Vec::with_capacity(rows.len().min(limit as usize));
    for (id, generation, body) in rows.into_iter().take(limit as usize) {
        let window_id = id.parse::<EntityId>().map_err(storage)?;
        let derivatives = conn
            .prepare_cached(
                "SELECT media_type, blake3, length(bytes) FROM context_window_derivatives
                 WHERE window_id = ?1 ORDER BY idx",
            )
            .map_err(storage)?
            .query_map(params![window_id.to_string()], |r| {
                Ok(StoredDerivativeMeta {
                    media_type: r.get(0)?,
                    blake3: r.get(1)?,
                    bytes: u64::try_from(r.get::<_, i64>(2)?)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                })
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        windows.push(StoredWindowMeta { window_id, generation, body, derivatives });
    }
    Ok(StoredWindowPage { total, has_older, windows })
}

fn inspect_window_page(
    conn: &rusqlite::Connection,
    before: Option<i64>,
    limit: i64,
    check_subject: impl FnOnce(&crate::SubjectBinding) -> anyhow::Result<()>,
) -> anyhow::Result<Option<SubjectContextWindowInspection>> {
    let binding = crate::store::read_installation_binding(conn)?;
    let Some(subject) = binding.subject else { return Ok(None) };
    check_subject(&subject)?;
    let scope_id = subject.branch_id;
    let profile = conn.query_row(
        "SELECT model_generation,profile_digest FROM context_window_scopes WHERE scope_id=?1",
        params![scope_id.to_string()],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    ).optional()?;
    let (model_generation, profile_digest) = profile.unwrap_or((0, None));
    let page = read_window_meta_page(conn, scope_id, before, limit)?;
    Ok(Some(SubjectContextWindowInspection { scope_id, model_generation, profile_digest, page }))
}

fn verify_selected_context_slot(
    conn: &rusqlite::Connection,
    agent_id: EntityId,
    db_path: &Path,
    data_dir: &Path,
    cas_dir: &Path,
) -> anyhow::Result<()> {
    use shenko_types::activation::{ActivationOrigin, ActivationPhase, ActivationRecord, RouteAuthority};
    use shenko_types::object::Digest;

    let selector = crate::activation::read_selector(conn, agent_id)?
        .context("context windows requires the selected slot's activation selector")?;
    let (revision, epoch): (u64, u64) = conn.query_row(
        "SELECT selector_revision,active_epoch FROM activation_selectors WHERE agent_id=?1",
        [agent_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (head, _) = crate::store::read_subject(conn, agent_id)?;
    ensure!(selector.agent_id == agent_id
        && selector.selector_revision == revision && selector.active_epoch == epoch
        && selector.generation == head.software_generation
        && selector.active_epoch == head.controller_epoch.0,
        "context windows activation selector does not own the observed subject head");

    // A candidate commits its selector before the source mirrors it. During
    // that interval the source's old head/selector/route are still mutually
    // consistent. Its durable handoff record, written before the candidate
    // CAS, makes the original selector expectation explicitly unresolved.
    // Do not let historical records for another revision (including a later
    // rollback to the same named generation) veto a verified selection.
    let mut handoffs = conn.prepare(
        "SELECT data FROM activation_records WHERE agent_id=?1 AND phase='handoff'",
    )?;
    for raw in handoffs.query_map([agent_id.to_string()], |row| row.get::<_, String>(0))? {
        let pending: ActivationRecord = serde_json::from_str(&raw?)?;
        ensure!(pending.agent_id == agent_id && pending.phase == ActivationPhase::Handoff,
            "context windows activation handoff projection is inconsistent");
        ensure!(pending.from_generation != selector.generation
            || pending.expected_selector_revision != selector.selector_revision,
            "context windows handoff {} selection requires reconciliation at selector revision {}",
            pending.activation_id, selector.selector_revision);
    }

    // The route is immutable content named by THIS snapshot's selector.
    // Attach read-only: a missing CAS is an error, never an initialization.
    let cas = crate::ContentStore::open_existing(cas_dir.to_owned())?;
    let bytes = cas.read_object(&selector.route_authority_ref.digest)?;
    ensure!(bytes.len() as u64 == selector.route_authority_ref.bytes
        && Digest::blake3(&bytes) == selector.route_authority_ref.digest,
        "context windows activation route content identity mismatch");
    let route: RouteAuthority = serde_json::from_slice(&bytes)?;
    ensure!(route.schema_version == RouteAuthority::SCHEMA_VERSION,
        "context windows activation route schema is unsupported");
    // A process takeover advances the selector's revision and epoch while
    // retaining this generation's original sealed route. It never changes
    // the route's identity, software generation or activation origin.
    let origin_matches = match &selector.origin {
        ActivationOrigin::Genesis { genesis_ref } => {
            route.activation_id.is_none() && *genesis_ref == head.genesis_ref
        }
        ActivationOrigin::Activation { activation_id } => {
            route.activation_id == Some(*activation_id)
        }
    };
    ensure!(route.agent_id == selector.agent_id && route.generation == selector.generation
        && route.active_epoch > 0 && route.active_epoch <= selector.active_epoch
        && route.selector_revision <= selector.selector_revision && origin_matches,
        "context windows activation route does not bind the observed selector");
    let root = data_dir.canonicalize()?;
    let slot = db_path.parent().context("context database has no slot directory")?.canonicalize()?;
    ensure!(slot == root || slot.parent() == Some(root.join("versions").as_path()),
        "context windows selected slot is outside this installation");
    ensure!(Path::new(&route.data_dir).canonicalize()? == root
        && Path::new(&route.slot_dir).canonicalize()? == slot,
        "context windows launcher slot disagrees with the committed activation route");
    Ok(())
}

impl Store {
    /// The scope's window chain, oldest first, with derivatives verified
    /// against their recorded blake3.
    pub fn context_windows(&self, scope_id: EntityId) -> Result<Vec<StoredContextWindow>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let rows = conn
            .prepare_cached(
                "SELECT window_id, generation, body FROM context_windows
                 WHERE scope_id = ?1 ORDER BY generation",
            )
            .map_err(storage)?
            .query_map(params![scope_id.to_string()], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage)?;
        let mut windows = Vec::with_capacity(rows.len());
        for (id, generation, body) in rows {
            windows.push(read_window(&conn, id, generation, body)?);
        }
        Ok(windows)
    }

    /// Only the current window and its derivatives, verified like `all`.
    pub fn context_window_head(&self, scope_id: EntityId) -> Result<Option<StoredContextWindow>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT window_id, generation, body FROM context_windows
             WHERE scope_id = ?1 ORDER BY generation DESC LIMIT 1",
            params![scope_id.to_string()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)),
        )
        .optional()
        .map_err(storage)?
        .map(|(id, generation, body)| read_window(&conn, id, generation, body))
        .transpose()
    }

    /// Bounded page of the scope's window metadata, newest generation first:
    /// `before` excludes windows with `generation >= before`; at most `limit`
    /// windows are returned. Bodies stay JSON text and derivative payloads
    /// stay untouched — only their recorded media type, blake3 and durable
    /// byte length are read — so request cost is bounded by the page, not by
    /// the retained archive.
    pub fn context_window_meta_page(
        &self,
        scope_id: EntityId,
        before: Option<i64>,
        limit: i64,
    ) -> Result<StoredWindowPage, ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
            .map_err(storage)?;
        read_window_meta_page(&tx, scope_id, before, limit)
    }

    /// Inspect one slot's locally owned subject branch using the same
    /// zero-touch, WAL-aware staging as installation identity inspection.
    /// This raw metadata reader does not claim that the slot is activated;
    /// it needs no CAS. Installation CLI queries use the active reader below.
    /// Missing/old schemas and unstable snapshots are errors, never an excuse
    /// to initialize the live installation or fall back to stale main bytes.
    pub fn inspect_subject_context_windows(
        db_path: &Path,
        before: Option<i64>,
        limit: i64,
    ) -> anyhow::Result<Option<SubjectContextWindowInspection>> {
        Self::inspect_database(db_path, |conn| {
            inspect_window_page(conn, before, limit, |_| Ok(()))
        })
    }

    /// Inspect the installation's selected slot, refusing stale launcher
    /// pointers. Selector, head, profile and windows share one stable staged
    /// transaction; only the selector's sealed route is read from existing
    /// CAS, with its digest and length verified. Nothing in the installation
    /// is created or written, including when route verification fails.
    pub fn inspect_active_subject_context_windows(
        db_path: &Path,
        data_dir: &Path,
        cas_dir: &Path,
        before: Option<i64>,
        limit: i64,
    ) -> anyhow::Result<Option<SubjectContextWindowInspection>> {
        Self::inspect_database(db_path, |conn| {
            inspect_window_page(conn, before, limit, |subject| {
                verify_selected_context_slot(conn, subject.agent_id, db_path, data_dir, cas_dir)
            })
        })
    }

    /// Append `window` as generation `head + 1` only if the current head is
    /// `expected_head` (None: the chain is empty) and the model generation is
    /// still `expected_model_generation` (None only for genesis creation).
    /// A storage error closes the writer: reopen and read the chain to learn
    /// whether the append landed.
    pub fn append_context_window(
        &self,
        scope_id: EntityId,
        expected_head: Option<EntityId>,
        expected_model_generation: Option<i64>,
        window: &StoredContextWindow,
    ) -> Result<ContextWindowAppend, ShenkoError> {
        self.append_window(scope_id, expected_head, expected_model_generation, None, window)
    }

    /// Atomically invalidate an old model's window prefix. The new head is a
    /// full-raw root: an old compressed/image projection cannot leak into a
    /// different binding merely because automatic compaction is unavailable.
    pub fn rebind_context_model_generation(
        &self,
        scope_id: EntityId,
        expected_head: EntityId,
        expected_model_generation: i64,
        generation: i64,
        profile_digest: &str,
        window: &StoredContextWindow,
    ) -> Result<ContextWindowAppend, ShenkoError> {
        if generation <= expected_model_generation || profile_digest.is_empty() {
            return Err(ShenkoError::conflict("context model generation must increase"));
        }
        self.append_window(scope_id, Some(expected_head), Some(expected_model_generation), Some((generation, profile_digest)), window)
    }

    fn append_window(
        &self,
        scope_id: EntityId,
        expected_head: Option<EntityId>,
        expected_model_generation: Option<i64>,
        new_model_generation: Option<(i64, &str)>,
        window: &StoredContextWindow,
    ) -> Result<ContextWindowAppend, ShenkoError> {
        for (idx, d) in window.derivatives.iter().enumerate() {
            if blake3::hash(&d.bytes).to_hex().as_str() != d.blake3 {
                return Err(ShenkoError::conflict(format!(
                    "derivative {idx} of window {} fails blake3",
                    window.window_id
                )));
            }
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(storage)?;
                let head: Option<(String, i64)> = tx
                    .query_row(
                        "SELECT window_id, generation FROM context_windows
                         WHERE scope_id = ?1 ORDER BY generation DESC LIMIT 1",
                        params![scope_id.to_string()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .map_err(storage)?;
                if head.as_ref().map(|(id, _)| id.clone()) != expected_head.map(|id| id.to_string()) {
                    return Ok(ContextWindowAppend::HeadChanged);
                }
                if let Some(expected) = expected_model_generation {
                    let current: i64 = tx
                        .query_row(
                            "SELECT model_generation FROM context_window_scopes WHERE scope_id = ?1",
                            params![scope_id.to_string()],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(storage)?
                        .unwrap_or(0);
                    if current != expected {
                        return Ok(ContextWindowAppend::ModelGenerationChanged { current });
                    }
                }
                let generation = head.map_or(0, |(_, generation)| generation + 1);
                if generation != window.generation {
                    return Err(storage(format!(
                        "window {} claims generation {} but the chain is at {generation}",
                        window.window_id, window.generation
                    )));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                tx.execute(
                    "INSERT INTO context_windows (window_id, scope_id, generation, body, commit_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        window.window_id.to_string(),
                        scope_id.to_string(),
                        generation,
                        window.body,
                        seq.0 as i64
                    ],
                )
                .map_err(storage)?;
                for (idx, d) in window.derivatives.iter().enumerate() {
                    tx.execute(
                        "INSERT INTO context_window_derivatives (window_id, idx, media_type, blake3, bytes)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![window.window_id.to_string(), idx as i64, d.media_type, d.blake3, d.bytes],
                    )
                    .map_err(storage)?;
                }
                if let Some((generation, profile_digest)) = new_model_generation {
                    tx.execute(
                        "INSERT INTO context_window_scopes(scope_id,model_generation,profile_digest) VALUES(?1,?2,?3)
                         ON CONFLICT(scope_id) DO UPDATE SET model_generation=excluded.model_generation,
                         profile_digest=excluded.profile_digest",
                        params![scope_id.to_string(), generation, profile_digest],
                    ).map_err(storage)?;
                }
                tx.execute(
                    "INSERT INTO commit_log (ledger_id, seq, incarnation, entity_count)
                     VALUES (?1, ?2, ?3, 1)",
                    params![
                        self.ledger.ledger_id().to_string(),
                        seq.0 as i64,
                        self.capabilities.incarnation.to_string()
                    ],
                )
                .map_err(storage)?;
                tx.commit().map_err(storage)?;
                Ok(ContextWindowAppend::Appended)
            },
            |_| true,
        )
    }

    /// Model/template/tokenizer/encoder binding generation of the scope (0 if unset).
    pub fn context_model_generation(&self, scope_id: EntityId) -> Result<i64, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT model_generation FROM context_window_scopes WHERE scope_id = ?1",
            params![scope_id.to_string()],
            |r| r.get(0),
        )
        .optional()
        .map(|g| g.unwrap_or(0))
        .map_err(storage)
    }

    pub fn context_profile(&self, scope_id: EntityId) -> Result<Option<(i64, String)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT model_generation,profile_digest FROM context_window_scopes WHERE scope_id=?1",
            params![scope_id.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        ).optional().map_err(storage).map(|row| {
            row.and_then(|(generation, profile)| profile.map(|profile| (generation, profile)))
        })
    }

}
