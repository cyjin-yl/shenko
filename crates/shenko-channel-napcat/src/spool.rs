//! NapCat 独立接收域 I：原始事件、去重身份、水位和 gap 同库持久化。
//! 不与主体控制 ledger L 跨库提交；ack 只是 L 中已提交处置的镜像。
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;
use shenko_types::object::SealedObjectRef;

const APPLICATION_ID: i64 = 0x534e4947;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressCursor {
    pub cursor_id: EntityId,
    pub provider: String,
    pub account: u64,
    pub last_seq: u64,
    pub gap_count: u64,
    pub updated_at: DateTime<Utc>,
}

impl IngressCursor {
    pub fn new(provider: &str, account: u64) -> Self {
        Self {
            cursor_id: EntityId::new(),
            provider: provider.to_string(),
            account,
            last_seq: 0,
            gap_count: 0,
            updated_at: Utc::now(),
        }
    }
    pub fn advance(&mut self, seq: u64) {
        if seq > self.last_seq {
            self.last_seq = seq;
            self.updated_at = Utc::now();
        }
    }
    pub fn record_gap(&mut self) {
        self.gap_count += 1;
        self.updated_at = Utc::now();
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressGap {
    pub gap_id: EntityId,
    pub provider: String,
    pub account: u64,
    pub disconnected_at: DateTime<Utc>,
    pub reconnected_at: Option<DateTime<Utc>>,
    pub status: GapStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapStatus {
    Open,
    Reconciling,
    FilledPartially,
    Acknowledged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpooledEvent {
    pub spool_id: EntityId,
    pub dedup_key: String,
    pub seq: u64,
    pub received_at: DateTime<Utc>,
    /// 仅在显式 ack 后为 true，不代表送入 channel 或模型输入。
    pub consumed: bool,
    pub weak_dedup: bool,
    pub delivered_at: Option<DateTime<Utc>>,
    /// 控制 ledger 已提交的处置引用；本库不拥有主体处置真相。
    pub disposition_ref: Option<String>,
    pub raw: serde_json::Value,
}

/// Non-executable evidence for a repeated stable source identity. The original
/// event remains immutable; metadata changes, conflicts and revisions are not commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressObservation {
    pub event_id: EntityId,
    pub kind: ObservationKind,
    pub raw: serde_json::Value,
}

/// A classification of an actual pre-migration spool row. The event itself,
/// including its old identity and disposition, remains available via `event`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressIdentityReconciliation {
    pub source_event_id: EntityId,
    pub original_event_id: EntityId,
    pub kind: ObservationKind,
    /// An older independent eligibility hold is not lifted by this migration.
    pub original_was_eligible: bool,
    /// Multiple formerly independent rows could already have crossed the
    /// delivery boundary, even when their delivered_at fields are empty.
    pub delivery_uncertain: bool,
}

/// A certificate must cover the exact historical row and all writer/consumer
/// epochs that could have delivered it, including already in-flight work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressNeverDispatchedCertificate {
    pub cursor_id: EntityId,
    pub account: u64,
    pub agent_id: EntityId,
    pub control_ledger_id: EntityId,
    pub original_event_id: EntityId,
    pub source_event_id: EntityId,
    pub source_seq: u64,
    pub raw_hash: String,
    pub proof: SealedObjectRef,
}

/// Trusted authority boundary: implementations must authenticate and read back
/// the sealed proof, bind every certificate field, and prove both no historical
/// dispatch and a durable fence against future delivery from all old epochs.
/// Absence from the control ledger or a missing delivered_at is NOT such proof.
pub trait IngressNeverDispatchedVerifier {
    fn verify(&self, certificate: &IngressNeverDispatchedCertificate, event: &SpooledEvent) -> Result<()>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IngressDeliveryEvidence {
    CommittedDisposition { event_id: EntityId, agent_id: EntityId, commit: CommitRef },
    NeverDispatched { certificate: IngressNeverDispatchedCertificate },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressReconciliationState {
    SingleOriginal,
    AwaitingEvidence,
    PriorEligibilityHold,
    OriginalReleased,
    DispositionObserved,
    ContradictoryEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressReconciliationStatus {
    pub original_event_id: EntityId,
    pub state: IngressReconciliationState,
    /// Includes the original when its delivery/disposition is not proven.
    pub unresolved_event_ids: Vec<EntityId>,
    pub evidence: Vec<IngressDeliveryEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    Original,
    MetadataOnly,
    Conflict,
    Revision,
}

struct Backend {
    connection: Connection,
    failed: bool,
    control_scope: Option<(EntityId, EntityId)>,
}

pub struct IngressSpool {
    backend: Mutex<Backend>,
    persistent: bool,
}

impl IngressSpool {
    /// 兼容旧调用的内存后端，仅用于测试/临时实例；生产必须显式 open。
    pub fn new(account: u64) -> Self {
        Self::in_memory(account).expect("创建临时 NapCat 接收库失败")
    }

    pub fn in_memory(account: u64) -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?, account, false)
    }

    /// 必须使用独立的本地文件，不可传入主体 ledger，也不接受内存 URI。
    /// 这里只声明本地事务/进程重开语义，不宣称已验证设备掉电耐久性。
    pub fn open(path: impl AsRef<Path>, account: u64) -> Result<Self> {
        let path = path.as_ref();
        ensure!(
            !path.as_os_str().is_empty() && path != Path::new(":memory:"),
            "持久接收库需要本地文件路径"
        );
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .context("打开 NapCat 持久接收库失败")?;
        Self::initialize(connection, account, true)
    }

    fn initialize(mut connection: Connection, account: u64, persistent: bool) -> Result<Self> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        let app: i64 = connection.pragma_query_value(None, "application_id", |r| r.get(0))?;
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        ensure!(
            app == APPLICATION_ID || (app == 0 && tables == 0),
            "拒绝把主体或其他数据库当作接收库"
        );
        if persistent {
            let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            ensure!(mode.eq_ignore_ascii_case("wal"), "接收库无法启用 WAL");
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let sync: i64 = connection.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let fk: i64 = connection.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        ensure!(sync == 2 && fk == 1, "接收库耐久配置读回失败");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Read the schema version after acquiring the writer lock: another
        // opener may have completed the upgrade while this handle was waiting.
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 3, "接收库版本较新，不能用旧 reader 写入");
        tx.execute_batch("
            CREATE TABLE IF NOT EXISTS ingress_cursor (singleton INTEGER PRIMARY KEY CHECK(singleton=1), body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS ingress_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                spool_id TEXT NOT NULL UNIQUE,
                dedup_key TEXT NOT NULL UNIQUE,
                consumed INTEGER NOT NULL DEFAULT 0,
                body TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS ingress_pending ON ingress_events(consumed, seq);
            CREATE TABLE IF NOT EXISTS ingress_gaps (gap_id TEXT PRIMARY KEY, body TEXT NOT NULL);
        ")?;
        if version < 2 {
            tx.execute_batch("
                ALTER TABLE ingress_events ADD COLUMN source_key TEXT;
                ALTER TABLE ingress_events ADD COLUMN eligible INTEGER NOT NULL DEFAULT 1;
            ")?;
        }
        tx.execute_batch("
            CREATE INDEX IF NOT EXISTS ingress_source ON ingress_events(source_key, seq);
            CREATE INDEX IF NOT EXISTS ingress_delivery ON ingress_events(eligible, consumed, seq);
            CREATE TABLE IF NOT EXISTS ingress_observations (
                source_key TEXT NOT NULL, raw_hash TEXT NOT NULL, body TEXT NOT NULL,
                PRIMARY KEY(source_key, raw_hash)
            );
        ")?;
        if version < 3 {
            // Keep the original observation key and body intact. A separate
            // grouping key makes all prior observations visible after identities
            // coalesce, including identical raws under different old keys.
            tx.execute_batch("
                ALTER TABLE ingress_observations ADD COLUMN canonical_source_key TEXT;
                UPDATE ingress_observations SET canonical_source_key=source_key;
            ")?;
        }
        tx.execute_batch("
            CREATE INDEX IF NOT EXISTS ingress_observation_canonical
                ON ingress_observations(canonical_source_key, raw_hash);
            CREATE TABLE IF NOT EXISTS ingress_identity_reconciliation (
                spool_id TEXT PRIMARY KEY REFERENCES ingress_events(spool_id),
                source_key TEXT NOT NULL,
                body TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS ingress_reconciliation_source
                ON ingress_identity_reconciliation(source_key);
            CREATE TABLE IF NOT EXISTS ingress_reconciliation_evidence (
                source_key TEXT NOT NULL,
                evidence_hash TEXT NOT NULL,
                body TEXT NOT NULL,
                PRIMARY KEY(source_key,evidence_hash)
            );
            CREATE TABLE IF NOT EXISTS ingress_reconciliation_releases (
                release_seq INTEGER PRIMARY KEY AUTOINCREMENT,
                spool_id TEXT NOT NULL REFERENCES ingress_events(spool_id),
                source_key TEXT NOT NULL,
                agent_id TEXT NOT NULL,
                control_ledger_id TEXT NOT NULL
            );
            CREATE UNIQUE INDEX IF NOT EXISTS ingress_releases_unique_event
                ON ingress_reconciliation_releases(spool_id);
        ")?;
        if version < 2 {
            // Preserve v1 rows and raw bytes. Quarantine later observations of the
            // same identity, including any already consumed rows; never invent ACKs.
            let mut after = 0;
            loop {
                let page: Vec<String> = {
                    let mut stmt = tx.prepare("SELECT body FROM ingress_events WHERE seq>?1 ORDER BY seq LIMIT 256")?;
                    stmt.query_map([after], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
                };
                if page.is_empty() { break; }
                for body in page {
                    let event: SpooledEvent = serde_json::from_str(&body)?;
                    after = i64::try_from(event.seq)?;
                    let key = source_identity(account, &event.raw).unwrap_or_else(|| event.dedup_key.clone());
                    let original = first_source_event(&tx, &key)?;
                    let kind = original.as_ref().map_or(ObservationKind::Original, |first| observation_kind(&first.raw, &event.raw));
                    save_observation(&tx, &key, original.as_ref().map_or(event.spool_id, |e| e.spool_id), kind, &event.raw)?;
                    tx.execute("UPDATE ingress_events SET source_key=?1,eligible=?2 WHERE spool_id=?3",
                        params![key, original.is_none(), event.spool_id.to_string()])?;
                }
            }
        }
        let existing: Option<String> = tx
            .query_row(
                "SELECT body FROM ingress_cursor WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(body) = existing {
            let cursor: IngressCursor = serde_json::from_str(&body)?;
            ensure!(
                cursor.account == account && cursor.provider == "napcat",
                "接收库账号不匹配"
            );
        } else {
            tx.execute(
                "INSERT INTO ingress_cursor VALUES (1, ?1)",
                [serde_json::to_string(&IngressCursor::new(
                    "napcat", account,
                ))?],
            )?;
        }
        if version < 3 {
            migrate_unsigned_identities(&tx, account)?;
        }
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", 3)?;
        tx.commit()?;
        Ok(Self {
            backend: Mutex::new(Backend {
                connection,
                failed: false,
                control_scope: None,
            }),
            persistent,
        })
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// Released historical rows may only be dispatched for the ledger/subject
    /// against which their fenced certificates were verified.
    pub fn bind_control_store(&self, store: &shenko_store::store::Store, agent_id: EntityId) -> Result<()> {
        let mut backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let scope = (agent_id, store.ledger.ledger_id());
        ensure!(backend.control_scope.is_none() || backend.control_scope == Some(scope),
            "接收库 handle 已绑定其他主体或控制账本");
        backend.control_scope = Some(scope);
        Ok(())
    }

    /// SQL/提交结果不明确后封闭当前 handle；只能重开读回，不 fallback 内存。
    fn mutate<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut backend = self.backend.lock();
        ensure!(
            !backend.failed,
            "接收库 handle 已失效，必须重开核对提交结果"
        );
        let result = (|| {
            let tx = backend
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let output = operation(&tx)?;
            tx.commit()
                .context("接收库提交失败或结果未知，必须重开核对")?;
            Ok(output)
        })();
        if result.is_err() {
            backend.failed = true;
        }
        result
    }

    pub fn ingest(&self, raw: serde_json::Value) -> Result<Option<SpooledEvent>> {
        self.mutate(|tx| {
            let mut cursor = read_cursor(tx)?;
            let spool_id = EntityId::new();
            let (dedup_key, weak_dedup) = dedup_key(cursor.account, &raw, spool_id)?;
            let source_key = source_identity(cursor.account, &raw).unwrap_or_else(|| dedup_key.clone());
            if let Some(original) = first_source_event(tx, &source_key)? {
                save_observation(tx, &source_key, original.spool_id, observation_kind(&original.raw, &raw), &raw)?;
                return Ok(None);
            }
            let mut event = SpooledEvent {
                spool_id,
                dedup_key,
                seq: 0,
                received_at: Utc::now(),
                consumed: false,
                weak_dedup,
                delivered_at: None,
                disposition_ref: None,
                raw,
            };
            tx.execute(
                "INSERT INTO ingress_events (spool_id,dedup_key,body,source_key) VALUES (?1,?2,?3,?4)",
                params![
                    event.spool_id.to_string(),
                    event.dedup_key,
                    serde_json::to_string(&event)?,
                    source_key,
                ],
            )?;
            event.seq = u64::try_from(tx.last_insert_rowid())?;
            save_event(tx, &event)?;
            save_observation(tx, &source_key, event.spool_id, ObservationKind::Original, &event.raw)?;
            cursor.advance(event.seq);
            save_cursor(tx, &cursor)?;
            Ok(Some(event))
        })
    }

    /// 非破坏性取件。可重投，既不删除原文，也不将交付视为主体已处理。
    pub fn consume(&self, max: usize) -> Result<Vec<SpooledEvent>> {
        self.pending_after(0, max)
    }

    /// 重开后按序分页重投；after 只用于本次扫描，不是消费确认水位。
    pub fn pending_after(&self, after: u64, max: usize) -> Result<Vec<SpooledEvent>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let (agent, ledger) = scope_params(backend.control_scope);
        let mut stmt = backend.connection.prepare(
            "SELECT e.body FROM ingress_events e WHERE e.eligible=1 AND e.consumed=0 AND e.seq>?1 \
             AND NOT EXISTS (SELECT 1 FROM ingress_reconciliation_releases r WHERE r.spool_id=e.spool_id \
                 AND NOT (r.agent_id IS ?2 AND r.control_ledger_id IS ?3)) ORDER BY e.seq LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, agent, ledger, i64::try_from(max)?], |r| {
            r.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    // The delivery loop snapshots the release horizon before each scan. A row
    // released after that snapshot must be handled by the release stream even
    // if its original seq is behind the loop's process-local watermark.
    pub(crate) fn release_horizon(&self) -> Result<u64> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let seq: i64 = backend.connection.query_row(
            "SELECT coalesce(max(release_seq),0) FROM ingress_reconciliation_releases", [], |r| r.get(0),
        )?;
        Ok(u64::try_from(seq)?)
    }

    pub(crate) fn control_scope(&self) -> Result<Option<(EntityId, EntityId)>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        Ok(backend.control_scope)
    }

    pub(crate) fn releases_between(&self, after: u64, through: u64, max: usize) -> Result<Vec<(u64, Option<SpooledEvent>)>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let (agent, ledger) = scope_params(backend.control_scope);
        let mut stmt = backend.connection.prepare(
            "SELECT q.release_seq,e.body,e.eligible,e.consumed,q.agent_id,q.control_ledger_id FROM ingress_reconciliation_releases q \
             JOIN ingress_events e ON e.spool_id=q.spool_id \
             WHERE q.release_seq>?1 AND q.release_seq<=?2 ORDER BY q.release_seq LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, i64::try_from(through)?, i64::try_from(max)?], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?, r.get::<_, bool>(3)?,
                r.get::<_, String>(4)?, r.get::<_, String>(5)?))
        })?;
        rows.map(|row| {
            let (seq, body, eligible, consumed, release_agent, release_ledger) = row?;
            let allowed = agent.as_deref() == Some(release_agent.as_str()) && ledger.as_deref() == Some(release_ledger.as_str());
            let event = if eligible && !consumed && allowed { Some(serde_json::from_str(&body)?) } else { None };
            Ok((u64::try_from(seq)?, event))
        }).collect()
    }

    pub(crate) fn pending_after_release_horizon(&self, after: u64, horizon: u64, max: usize,
        scope: Option<(EntityId, EntityId)>) -> Result<Vec<SpooledEvent>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        // Use the loop's snapshot: binding midway through this scan must not
        // make a released row visible before its release cursor is processed.
        let (agent, ledger) = scope_params(scope);
        let mut stmt = backend.connection.prepare(
            "SELECT e.body FROM ingress_events e WHERE e.eligible=1 AND e.consumed=0 AND e.seq>?1 \
             AND NOT EXISTS (SELECT 1 FROM ingress_reconciliation_releases r \
                 WHERE r.spool_id=e.spool_id AND (r.release_seq>?2 \
                     OR NOT (r.agent_id IS ?3 AND r.control_ledger_id IS ?4))) ORDER BY e.seq LIMIT ?5",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, i64::try_from(horizon)?, agent, ledger, i64::try_from(max)?], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// The channel permit is reserved before entering this synchronous SQLite
    /// transaction. Recheck eligibility under the writer lock so a concurrent
    /// reconciliation cannot revoke it between selection and channel send.
    pub(crate) fn dispatch_if_pending(&self, id: EntityId, send: impl FnOnce()) -> Result<bool> {
        let scope = self.backend.lock().control_scope;
        let (agent, ledger) = scope_params(scope);
        self.mutate(|tx| {
            let pending: Option<bool> = tx.query_row(
                "SELECT e.eligible=1 AND e.consumed=0 AND NOT EXISTS \
                 (SELECT 1 FROM ingress_reconciliation_releases r WHERE r.spool_id=e.spool_id \
                  AND NOT (r.agent_id IS ?2 AND r.control_ledger_id IS ?3)) FROM ingress_events e WHERE e.spool_id=?1",
                params![id.to_string(), agent, ledger], |r| r.get(0),
            ).optional()?;
            if pending == Some(true) {
                send();
                Ok(true)
            } else {
                Ok(false)
            }
        })
    }

    /// Raw repeat/conflict evidence; never returned by the delivery scan.
    pub fn observations(&self, id: EntityId) -> Result<Vec<IngressObservation>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let mut stmt = backend.connection.prepare(
            "SELECT o.body FROM ingress_observations o JOIN ingress_events e ON e.source_key=o.canonical_source_key WHERE e.spool_id=?1 ORDER BY o.rowid",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Historical rows brought together by the unsigned identity migration.
    /// An uncertain group is held out of pending delivery pending reconciliation
    /// with the control ledger; no prior dispatch or disposition is fabricated.
    pub fn identity_reconciliation(&self, id: EntityId) -> Result<Vec<IngressIdentityReconciliation>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let mut stmt = backend.connection.prepare(
            "SELECT r.body FROM ingress_identity_reconciliation r \
             JOIN ingress_events selected ON selected.source_key=r.source_key \
             JOIN ingress_events historical ON historical.spool_id=r.spool_id \
             WHERE selected.spool_id=?1 ORDER BY historical.seq",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn reconciliation_status(&self, id: EntityId) -> Result<IngressReconciliationStatus> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let key = reconciliation_key(&backend.connection, id)?;
        reconciliation_status(&backend.connection, &key)
    }

    /// Enumerate duplicate groups for bounded control-ledger reconciliation.
    pub fn reconciliation_groups_after(&self, after: u64, max: usize) -> Result<Vec<(u64, EntityId)>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let mut stmt = backend.connection.prepare(
            "SELECT e.seq,e.spool_id FROM ingress_events e \
             JOIN ingress_identity_reconciliation r ON r.spool_id=e.spool_id \
             WHERE e.seq>?1 AND NOT EXISTS (SELECT 1 FROM ingress_events x \
                 WHERE x.source_key=e.source_key AND x.seq<e.seq) \
             AND EXISTS (SELECT 1 FROM ingress_events x WHERE x.source_key=e.source_key AND x.seq>e.seq) \
             ORDER BY e.seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, i64::try_from(max)?], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (seq, id) = row?;
            Ok((u64::try_from(seq)?, id.parse()?))
        }).collect()
    }

    /// Read actual non-deferred S1 receipts from the control ledger. Missing
    /// receipts say nothing about prior dispatch. The receipt and the delivery
    /// decision are persisted together without changing any event's old ACK.
    pub fn reconcile_from_store(
        &self, store: &shenko_store::store::Store, agent_id: EntityId, id: EntityId,
    ) -> Result<IngressReconciliationStatus> {
        self.bind_control_store(store, agent_id)?;
        let rows = self.identity_reconciliation(id)?;
        ensure!(!rows.is_empty(), "接收事件没有身份对账记录");
        let mut evidence = Vec::new();
        for row in rows {
            if let Some(commit) = store.event_disposition_commit(agent_id, row.source_event_id)? {
                evidence.push(IngressDeliveryEvidence::CommittedDisposition {
                    event_id: row.source_event_id, agent_id, commit,
                });
            }
        }
        if evidence.is_empty() { return self.reconciliation_status(id); }
        self.mutate(|tx| {
            let key = reconciliation_key(tx, id)?;
            for item in &evidence { save_reconciliation_evidence(tx, &key, item)?; }
            apply_reconciliation_policy(tx, &key)
        })
    }

    /// Accept a later externally verifiable, fenced non-dispatch certificate.
    /// The caller supplies the actual evidence authority, not a boolean guess.
    /// Its proof must establish that no old worker can subsequently deliver.
    pub fn submit_never_dispatched(
        &self, store: &shenko_store::store::Store, agent_id: EntityId,
        certificate: IngressNeverDispatchedCertificate,
        verifier: &dyn IngressNeverDispatchedVerifier,
    ) -> Result<IngressReconciliationStatus> {
        let event = self.event(certificate.source_event_id)?;
        let cursor = self.cursor()?;
        let rows = self.identity_reconciliation(certificate.source_event_id)?;
        validate_certificate_binding(&cursor, &rows, &event, &certificate)?;
        ensure!(certificate.agent_id == agent_id && certificate.control_ledger_id == store.ledger.ledger_id(),
            "未投递证据与主体或控制账本不匹配");
        ensure!(!event.consumed && event.delivered_at.is_none(), "该事件已有投递或处置事实");
        verifier.verify(&certificate, &event)?;
        // Capture any already committed S1 before trying to release an original.
        self.reconcile_from_store(store, agent_id, certificate.source_event_id)?;
        self.mutate(|tx| {
            let key = reconciliation_key(tx, certificate.source_event_id)?;
            let event = read_event(tx, certificate.source_event_id)?;
            let rows = read_reconciliation_rows(tx, &key)?;
            validate_certificate_binding(&read_cursor(tx)?, &rows, &event, &certificate)?;
            ensure!(!event.consumed && event.delivered_at.is_none(), "该事件已有投递或处置事实");
            let existing = reconciliation_status(tx, &key)?;
            ensure!(!existing.evidence.iter().any(|item| matches!(item,
                IngressDeliveryEvidence::CommittedDisposition { event_id, .. } if *event_id == certificate.source_event_id)),
                "该事件已有已提交处置，不能声称未投递");
            ensure!(!existing.evidence.iter().any(|item| matches!(item,
                IngressDeliveryEvidence::NeverDispatched { certificate: previous }
                    if previous.agent_id != certificate.agent_id || previous.control_ledger_id != certificate.control_ledger_id)),
                "对账组不能混用不同主体或控制账本的未投递证据");
            let evidence = IngressDeliveryEvidence::NeverDispatched { certificate: certificate.clone() };
            save_reconciliation_evidence(tx, &key, &evidence)?;
            apply_reconciliation_policy(tx, &key)
        })
    }

    pub fn mark_delivered(&self, id: EntityId) -> Result<()> {
        self.mutate(|tx| {
            let mut event = read_event(tx, id)?;
            event.delivered_at = Some(Utc::now());
            save_event(tx, &event)?;
            refresh_reconciliation_for_event(tx, id)
        })
    }

    /// 只有控制 ledger 中处置已提交后才可调用；不是 channel ACK。
    /// 这是可滞后的镜像，恢复时仍须由主体 ledger 覆盖旧镜像。
    pub fn ack(&self, id: EntityId, disposition_ref: &str) -> Result<()> {
        ensure!(
            !disposition_ref.trim().is_empty(),
            "ack 必须提供已提交处置引用"
        );
        self.mutate(|tx| {
            let mut event = read_event(tx, id)?;
            if event.consumed {
                ensure!(
                    event.disposition_ref.as_deref() == Some(disposition_ref),
                    "ack 处置引用冲突"
                );
                return Ok(());
            }
            event.consumed = true;
            event.disposition_ref = Some(disposition_ref.to_owned());
            save_event(tx, &event)?;
            refresh_reconciliation_for_event(tx, id)
        })
    }

    pub fn event(&self, id: EntityId) -> Result<SpooledEvent> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        read_event(&backend.connection, id)
    }

    pub fn record_disconnect(&self) -> Result<()> {
        self.mutate(|tx| {
            let mut cursor = read_cursor(tx)?;
            let gap = IngressGap {
                gap_id: EntityId::new(),
                provider: cursor.provider.clone(),
                account: cursor.account,
                disconnected_at: Utc::now(),
                reconnected_at: None,
                status: GapStatus::Open,
            };
            tx.execute(
                "INSERT INTO ingress_gaps VALUES (?1,?2)",
                params![gap.gap_id.to_string(), serde_json::to_string(&gap)?],
            )?;
            cursor.record_gap();
            save_cursor(tx, &cursor)
        })
    }

    pub fn record_reconnect(&self) -> Result<()> {
        self.mutate(|tx| {
            for mut gap in read_gaps(tx)? {
                if gap.status == GapStatus::Open {
                    gap.reconnected_at = Some(Utc::now());
                    gap.status = GapStatus::Reconciling;
                    tx.execute(
                        "UPDATE ingress_gaps SET body=?1 WHERE gap_id=?2",
                        params![serde_json::to_string(&gap)?, gap.gap_id.to_string()],
                    )?;
                }
            }
            Ok(())
        })
    }

    pub fn gaps(&self) -> Result<Vec<IngressGap>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        read_gaps(&backend.connection)
    }

    pub fn cursor(&self) -> Result<IngressCursor> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        read_cursor(&backend.connection)
    }

    pub fn pending_count(&self) -> Result<usize> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let (agent, ledger) = scope_params(backend.control_scope);
        let count: i64 = backend.connection.query_row(
            "SELECT count(*) FROM ingress_events e WHERE e.eligible=1 AND e.consumed=0 \
             AND NOT EXISTS (SELECT 1 FROM ingress_reconciliation_releases r WHERE r.spool_id=e.spool_id \
                 AND NOT (r.agent_id IS ?1 AND r.control_ledger_id IS ?2))",
            params![agent, ledger],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }
    pub fn gap_count(&self) -> Result<u64> {
        Ok(self.cursor()?.gap_count)
    }
}

fn scope_params(scope: Option<(EntityId, EntityId)>) -> (Option<String>, Option<String>) {
    match scope {
        Some((agent, ledger)) => (Some(agent.to_string()), Some(ledger.to_string())),
        None => (None, None),
    }
}

fn read_cursor(connection: &Connection) -> Result<IngressCursor> {
    let body: String = connection.query_row(
        "SELECT body FROM ingress_cursor WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&body)?)
}
fn save_cursor(tx: &Transaction<'_>, cursor: &IngressCursor) -> Result<()> {
    tx.execute(
        "UPDATE ingress_cursor SET body=?1 WHERE singleton=1",
        [serde_json::to_string(cursor)?],
    )?;
    Ok(())
}
fn read_event(connection: &Connection, id: EntityId) -> Result<SpooledEvent> {
    let body: Option<String> = connection
        .query_row(
            "SELECT body FROM ingress_events WHERE spool_id=?1",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    match body {
        Some(body) => Ok(serde_json::from_str(&body)?),
        None => bail!("接收事件不存在: {id}"),
    }
}
fn save_event(tx: &Transaction<'_>, event: &SpooledEvent) -> Result<()> {
    tx.execute(
        "UPDATE ingress_events SET consumed=?1,body=?2 WHERE spool_id=?3",
        params![
            event.consumed,
            serde_json::to_string(event)?,
            event.spool_id.to_string()
        ],
    )?;
    Ok(())
}
fn read_gaps(connection: &Connection) -> Result<Vec<IngressGap>> {
    let mut stmt = connection.prepare("SELECT body FROM ingress_gaps ORDER BY rowid")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn reconciliation_key(connection: &Connection, id: EntityId) -> Result<String> {
    connection.query_row(
        "SELECT source_key FROM ingress_identity_reconciliation WHERE spool_id=?1",
        [id.to_string()], |r| r.get(0),
    ).optional()?.context("接收事件没有身份对账记录")
}

fn read_reconciliation_rows(connection: &Connection, key: &str) -> Result<Vec<IngressIdentityReconciliation>> {
    let mut stmt = connection.prepare(
        "SELECT r.body FROM ingress_identity_reconciliation r \
         JOIN ingress_events e ON e.spool_id=r.spool_id WHERE r.source_key=?1 ORDER BY e.seq",
    )?;
    let rows = stmt.query_map([key], |r| r.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}

fn validate_certificate_binding(
    cursor: &IngressCursor, rows: &[IngressIdentityReconciliation], event: &SpooledEvent,
    certificate: &IngressNeverDispatchedCertificate,
) -> Result<()> {
    let original = rows.first().context("空身份对账组")?.source_event_id;
    ensure!(rows.len() > 1 && certificate.source_event_id != original
        && rows.iter().any(|row| row.source_event_id == certificate.source_event_id
            && row.original_event_id == original)
        && certificate.original_event_id == original && certificate.source_event_id == event.spool_id
        && certificate.cursor_id == cursor.cursor_id && certificate.account == cursor.account
        && certificate.source_seq == event.seq
        && certificate.raw_hash == blake3::hash(&serde_json::to_vec(&event.raw)?).to_string(),
        "未投递证据与接收库、事件或来源组不匹配");
    Ok(())
}

fn save_reconciliation_evidence(tx: &Transaction<'_>, key: &str, evidence: &IngressDeliveryEvidence) -> Result<()> {
    let id = match evidence {
        IngressDeliveryEvidence::CommittedDisposition { event_id, .. } => *event_id,
        IngressDeliveryEvidence::NeverDispatched { certificate } => certificate.source_event_id,
    };
    ensure!(reconciliation_key(tx, id)? == key, "对账证据不属于该来源组");
    let body = serde_json::to_string(evidence)?;
    let hash = blake3::hash(body.as_bytes()).to_string();
    tx.execute("INSERT OR IGNORE INTO ingress_reconciliation_evidence VALUES (?1,?2,?3)",
        params![key, hash, body])?;
    Ok(())
}

fn reconciliation_status(connection: &Connection, key: &str) -> Result<IngressReconciliationStatus> {
    use std::collections::{HashMap, HashSet};

    let rows = read_reconciliation_rows(connection, key)?;
    let original = rows.first().context("空身份对账组")?.source_event_id;
    ensure!(rows.iter().all(|row| row.original_event_id == original
        && row.original_was_eligible == rows[0].original_was_eligible), "对账组原始事件或原资格不一致");
    let cursor = read_cursor(connection)?;
    let mut events = Vec::with_capacity(rows.len());
    for row in &rows { events.push(read_event(connection, row.source_event_id)?); }
    let mut stmt = connection.prepare(
        "SELECT body FROM ingress_reconciliation_evidence WHERE source_key=?1 ORDER BY rowid",
    )?;
    let records = stmt.query_map([key], |r| r.get::<_, String>(0))?;
    let evidence: Vec<IngressDeliveryEvidence> = records.map(|row| Ok(serde_json::from_str(&row?)?)).collect::<Result<_>>()?;
    let mut never = HashSet::new();
    let mut commits: HashMap<EntityId, Vec<CommitRef>> = HashMap::new();
    let mut proof_scope = None;
    let mut scope_conflict = false;
    for item in &evidence {
        match item {
            IngressDeliveryEvidence::CommittedDisposition { event_id, commit, .. } => {
                ensure!(rows.iter().any(|row| row.source_event_id == *event_id), "已提交处置不属于对账组");
                commits.entry(*event_id).or_default().push(*commit);
            }
            IngressDeliveryEvidence::NeverDispatched { certificate } => {
                let event = events.iter().find(|event| event.spool_id == certificate.source_event_id)
                    .context("未投递证据缺少对应事件")?;
                validate_certificate_binding(&cursor, &rows, event, certificate)?;
                never.insert(certificate.source_event_id);
                let scope = (certificate.agent_id, certificate.control_ledger_id);
                if proof_scope.is_some_and(|previous| previous != scope) { scope_conflict = true; }
                proof_scope = Some(scope);
            }
        }
    }
    let contradiction = scope_conflict || events.iter().any(|event| {
        let observed_commits = commits.get(&event.spool_id);
        (never.contains(&event.spool_id)
            && (event.delivered_at.is_some() || event.consumed || observed_commits.is_some()))
            || (event.consumed && observed_commits.is_some_and(|refs| {
                !refs.iter().any(|commit| event.disposition_ref.as_deref() == Some(commit.to_string().as_str()))
            }))
    });
    let unresolved_event_ids = events.iter().filter(|event| {
        !never.contains(&event.spool_id) && !commits.contains_key(&event.spool_id)
    }).map(|event| event.spool_id).collect();
    let state = if rows.len() == 1 { IngressReconciliationState::SingleOriginal }
        else if contradiction { IngressReconciliationState::ContradictoryEvidence }
        else if !commits.is_empty() { IngressReconciliationState::DispositionObserved }
        else if !rows[0].original_was_eligible { IngressReconciliationState::PriorEligibilityHold }
        else if !events[0].consumed && events.iter().skip(1).all(|event| never.contains(&event.spool_id)) {
            IngressReconciliationState::OriginalReleased
        } else { IngressReconciliationState::AwaitingEvidence };
    Ok(IngressReconciliationStatus { original_event_id: original, state, unresolved_event_ids, evidence })
}

fn apply_reconciliation_policy(tx: &Transaction<'_>, key: &str) -> Result<IngressReconciliationStatus> {
    let status = reconciliation_status(tx, key)?;
    if status.state == IngressReconciliationState::SingleOriginal { return Ok(status); }
    let was_eligible: bool = tx.query_row("SELECT eligible FROM ingress_events WHERE spool_id=?1",
        [status.original_event_id.to_string()], |r| r.get(0))?;
    let release = status.state == IngressReconciliationState::OriginalReleased;
    tx.execute("UPDATE ingress_events SET eligible=CASE WHEN spool_id=?1 THEN ?2 ELSE 0 END WHERE source_key=?3",
        params![status.original_event_id.to_string(), release, key])?;
    if release && !was_eligible {
        let (agent, ledger) = status.evidence.iter().find_map(|item| match item {
            IngressDeliveryEvidence::NeverDispatched { certificate } => Some((certificate.agent_id, certificate.control_ledger_id)),
            _ => None,
        }).context("释放事件缺少绑定主体和账本的证据")?;
        tx.execute("INSERT INTO ingress_reconciliation_releases (spool_id,source_key,agent_id,control_ledger_id) VALUES (?1,?2,?3,?4)",
            params![status.original_event_id.to_string(), key, agent.to_string(), ledger.to_string()])?;
    }
    Ok(status)
}

fn refresh_reconciliation_for_event(tx: &Transaction<'_>, id: EntityId) -> Result<()> {
    let key: Option<String> = tx.query_row(
        "SELECT source_key FROM ingress_identity_reconciliation WHERE spool_id=?1", [id.to_string()], |r| r.get(0),
    ).optional()?;
    if let Some(key) = key { apply_reconciliation_policy(tx, &key)?; }
    Ok(())
}

/// Rebind only identities that the old signed-only parser missed. All changes,
/// including the schema version, are committed by the caller's transaction.
fn migrate_unsigned_identities(tx: &Transaction<'_>, account: u64) -> Result<()> {
    let mut after = 0_i64;
    loop {
        let page: Vec<(i64, String, String, i64, Option<String>, String)> = {
            let mut stmt = tx.prepare(
                "SELECT seq,spool_id,dedup_key,consumed,source_key,body \
                 FROM ingress_events WHERE seq>?1 ORDER BY seq LIMIT 256",
            )?;
            let rows = stmt.query_map([after], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if page.is_empty() { break; }
        for (seq, spool_id, dedup_key, consumed, old_key, body) in page {
            after = seq;
            let event: SpooledEvent = serde_json::from_str(&body)?;
            let high_unsigned = event.raw.get("message_id")
                .and_then(serde_json::Value::as_u64)
                .is_some_and(|id| id > i64::MAX as u64);
            if !high_unsigned { continue; }
            let Some(key) = source_identity(account, &event.raw) else { continue; };
            ensure!(i64::try_from(event.seq)? == seq && event.spool_id.to_string() == spool_id
                && event.dedup_key == dedup_key && (if event.consumed { 1 } else { 0 }) == consumed,
                "接收事件身份或处置与存储列不一致，无法迁移 seq={seq}");
            let old_key = old_key.context("接收事件缺少旧来源身份，无法迁移")?;
            if old_key != key {
                // A v2 weak source is unique to this spool row. Do not guess if
                // unexpected legacy state would make moving observations ambiguous.
                let count: i64 = tx.query_row(
                    "SELECT count(*) FROM ingress_events WHERE source_key=?1", [&old_key], |r| r.get(0),
                )?;
                let expected_weak = format!("napcat:{account}:onebot_v11:spool:{}", event.spool_id);
                ensure!(event.weak_dedup && old_key == event.dedup_key && old_key == expected_weak && count == 1,
                    "旧弱来源身份不唯一或不匹配，无法迁移 seq={seq}");
                tx.execute("UPDATE ingress_observations SET canonical_source_key=?1 WHERE source_key=?2",
                    params![key, old_key])?;
                tx.execute("UPDATE ingress_events SET source_key=?1 WHERE spool_id=?2",
                    params![key, spool_id])?;
            }
            // This marker is completed below, before the transaction can commit.
            tx.execute("INSERT INTO ingress_identity_reconciliation (spool_id,source_key,body) VALUES (?1,?2,'')",
                params![spool_id, key])?;
        }
    }

    let mut after_key = String::new();
    loop {
        let keys: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT source_key FROM ingress_identity_reconciliation \
                 WHERE source_key>?1 ORDER BY source_key LIMIT 256",
            )?;
            let rows = stmt.query_map([&after_key], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        if keys.is_empty() { break; }
        for key in keys {
            after_key = key.clone();
            let original = first_source_event(tx, &key)?.context("迁移来源缺少原始事件")?;
            let original_was_eligible: bool = tx.query_row(
                "SELECT eligible FROM ingress_events WHERE spool_id=?1", [original.spool_id.to_string()], |r| r.get(0),
            )?;
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM ingress_events WHERE source_key=?1", [&key], |r| r.get(0),
            )?;
            let uncertain = count > 1;
            if uncertain {
                // The old writer sent into the channel before mark_delivered.
                // Even an empty delivered_at cannot prove that another spool ID
                // was not dispatched. Hold the entire group, not just later rows.
                tx.execute("UPDATE ingress_events SET eligible=0 WHERE source_key=?1", [&key])?;
            }
            let mut after_seq = 0_i64;
            loop {
                let page: Vec<(i64, String, String, i64, String)> = {
                    let mut stmt = tx.prepare(
                        "SELECT seq,spool_id,dedup_key,consumed,body FROM ingress_events \
                         WHERE source_key=?1 AND seq>?2 ORDER BY seq LIMIT 256",
                    )?;
                    let rows = stmt.query_map(params![key, after_seq], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                if page.is_empty() { break; }
                for (seq, spool_id, dedup_key, consumed, body) in page {
                    after_seq = seq;
                    let event: SpooledEvent = serde_json::from_str(&body)?;
                    ensure!(i64::try_from(event.seq)? == seq && event.spool_id.to_string() == spool_id
                        && event.dedup_key == dedup_key && (if event.consumed { 1 } else { 0 }) == consumed,
                        "接收事件身份或处置与存储列不一致，无法迁移 seq={seq}");
                    ensure!(source_identity(account, &event.raw).as_deref() == Some(key.as_str()),
                        "来源组包含与身份不符的事件，无法迁移 seq={seq}");
                    let record = IngressIdentityReconciliation {
                        source_event_id: event.spool_id,
                        original_event_id: original.spool_id,
                        kind: if event.spool_id == original.spool_id { ObservationKind::Original }
                            else { observation_kind(&original.raw, &event.raw) },
                        original_was_eligible,
                        delivery_uncertain: uncertain,
                    };
                    tx.execute("INSERT INTO ingress_identity_reconciliation (spool_id,source_key,body) \
                        VALUES (?1,?2,?3) ON CONFLICT(spool_id) DO UPDATE SET source_key=excluded.source_key,body=excluded.body",
                        params![spool_id, key, serde_json::to_string(&record)?])?;
                }
            }
            let mut after_observation = 0_i64;
            loop {
                let page: Vec<(i64, String)> = {
                    let mut stmt = tx.prepare(
                        "SELECT rowid,body FROM ingress_observations \
                         WHERE canonical_source_key=?1 AND rowid>?2 ORDER BY rowid LIMIT 256",
                    )?;
                    let rows = stmt.query_map(params![key, after_observation], |r| Ok((r.get(0)?, r.get(1)?)))?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                if page.is_empty() { break; }
                for (rowid, body) in page {
                    after_observation = rowid;
                    let observation: IngressObservation = serde_json::from_str(&body)?;
                    let observed_source: Option<Option<String>> = tx.query_row(
                        "SELECT source_key FROM ingress_events WHERE spool_id=?1",
                        [observation.event_id.to_string()], |r| r.get(0),
                    ).optional()?;
                    ensure!(observed_source.flatten().as_deref() == Some(key.as_str())
                        && source_identity(account, &observation.raw).as_deref() == Some(key.as_str()),
                        "旧观察与来源事件不一致，无法迁移 observation={rowid}");
                }
            }
        }
    }
    Ok(())
}

fn source_identity(account: u64, raw: &serde_json::Value) -> Option<String> {
    // Platform time and display names are not identities. Explicit revisions are
    // observations of this message, quarantined until an actual revision policy exists.
    let message_id = raw.get("message_id").filter(|v| match v {
        serde_json::Value::Number(n) => n.as_i64().is_some_and(|n| n != 0)
            || n.as_u64().is_some_and(|n| n != 0),
        serde_json::Value::String(s) => !s.is_empty() && s != "0",
        _ => false,
    })?;
    let conversation = match raw["message_type"].as_str() {
        Some("group") => serde_json::json!(["group", raw.get("group_id").filter(|v| !v.is_null())?]),
        Some("private") => serde_json::json!(["private", raw.get("user_id").filter(|v| !v.is_null())?, raw.get("group_id")]),
        _ => return None,
    };
    if !matches!(raw["post_type"].as_str(), Some("message" | "message_sent")) { return None; }
    // OneBot message_id accepts string or integer wire representations.
    let message_id = message_id.as_str().map(str::to_owned).unwrap_or_else(|| message_id.to_string());
    Some(serde_json::json!(["napcat", account.to_string(), "onebot_v11", conversation, message_id]).to_string())
}

fn dedup_key(account: u64, raw: &serde_json::Value, id: EntityId) -> Result<(String, bool)> {
    match source_identity(account, raw) {
        Some(identity) => Ok((format!("{identity}:{}", blake3::hash(&serde_json::to_vec(raw)?)), false)),
        // No stable source identity means no strong cross-reconnect dedup claim.
        None => Ok((format!("napcat:{account}:onebot_v11:spool:{id}"), true)),
    }
}

fn first_source_event(connection: &Connection, key: &str) -> Result<Option<SpooledEvent>> {
    let body: Option<String> = connection.query_row(
        "SELECT body FROM ingress_events WHERE source_key=?1 ORDER BY seq LIMIT 1", [key], |r| r.get(0),
    ).optional()?;
    body.map(|body| Ok(serde_json::from_str(&body)?)).transpose()
}

fn observation_kind(original: &serde_json::Value, raw: &serde_json::Value) -> ObservationKind {
    if original.get("message_revision") != raw.get("message_revision") { return ObservationKind::Revision; }
    // Compare command-bearing content and actor, not sender nick/card/role or
    // reception time. Conservative conflicts are retained, never auto-executed.
    let semantic = |v: &serde_json::Value| serde_json::json!([
        v.get("user_id"), v.get("message"), v.get("raw_message"), v.get("anonymous"), v.get("sub_type")
    ]);
    if semantic(original) == semantic(raw) { ObservationKind::MetadataOnly } else { ObservationKind::Conflict }
}

fn save_observation(tx: &Transaction<'_>, key: &str, event_id: EntityId, kind: ObservationKind, raw: &serde_json::Value) -> Result<()> {
    let hash = blake3::hash(&serde_json::to_vec(raw)?).to_string();
    let observation = IngressObservation { event_id, kind, raw: raw.clone() };
    tx.execute("INSERT OR IGNORE INTO ingress_observations (source_key,raw_hash,body,canonical_source_key) \
        SELECT ?1,?2,?3,?1 WHERE NOT EXISTS (SELECT 1 FROM ingress_observations \
        WHERE canonical_source_key=?1 AND raw_hash=?2)",
        params![key, hash, serde_json::to_string(&observation)?])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("shenko-ingress-{}", EntityId::new()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn db(&self) -> std::path::PathBuf {
            self.0.join("ingress.sqlite")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn message() -> serde_json::Value {
        json!({"post_type":"message", "message_type":"group", "group_id":7, "user_id":8, "message_id":42, "time":1})
    }

    struct LegacyRow {
        event: SpooledEvent,
        source_key: String,
        eligible: bool,
    }

    fn legacy_row(seq: u64, raw: serde_json::Value, weak: bool) -> LegacyRow {
        let spool_id = EntityId::new();
        let (strong_key, _) = dedup_key(9, &raw, spool_id).unwrap();
        let dedup_key = if weak {
            format!("napcat:9:onebot_v11:spool:{spool_id}")
        } else {
            strong_key
        };
        let source_key = if weak { dedup_key.clone() } else { source_identity(9, &raw).unwrap() };
        LegacyRow {
            event: SpooledEvent {
                spool_id, dedup_key, seq, received_at: Utc::now(), consumed: false,
                weak_dedup: weak, delivered_at: None, disposition_ref: None, raw,
            },
            source_key,
            eligible: true,
        }
    }

    // Construct the actual pre-upgrade layouts, rather than writing v3 data
    // and merely changing user_version. v1 did not have observations yet.
    fn legacy_spool(path: &Path, version: i64, rows: &[LegacyRow]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("
            CREATE TABLE ingress_cursor (singleton INTEGER PRIMARY KEY CHECK(singleton=1), body TEXT NOT NULL);
            CREATE TABLE ingress_events (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, spool_id TEXT NOT NULL UNIQUE,
                dedup_key TEXT NOT NULL UNIQUE, consumed INTEGER NOT NULL DEFAULT 0, body TEXT NOT NULL
            );
            CREATE INDEX ingress_pending ON ingress_events(consumed, seq);
            CREATE TABLE ingress_gaps (gap_id TEXT PRIMARY KEY, body TEXT NOT NULL);
        ").unwrap();
        if version == 2 {
            conn.execute_batch("
                ALTER TABLE ingress_events ADD COLUMN source_key TEXT;
                ALTER TABLE ingress_events ADD COLUMN eligible INTEGER NOT NULL DEFAULT 1;
                CREATE INDEX ingress_source ON ingress_events(source_key, seq);
                CREATE INDEX ingress_delivery ON ingress_events(eligible, consumed, seq);
                CREATE TABLE ingress_observations (
                    source_key TEXT NOT NULL, raw_hash TEXT NOT NULL, body TEXT NOT NULL,
                    PRIMARY KEY(source_key, raw_hash)
                );
            ").unwrap();
        }
        let mut cursor = IngressCursor::new("napcat", 9);
        cursor.last_seq = rows.last().map_or(0, |r| r.event.seq);
        cursor.gap_count = 1;
        conn.execute("INSERT INTO ingress_cursor VALUES (1, ?1)", [serde_json::to_string(&cursor).unwrap()]).unwrap();
        let gap = IngressGap {
            gap_id: EntityId::new(), provider: "napcat".into(), account: 9,
            disconnected_at: Utc::now(), reconnected_at: None, status: GapStatus::Open,
        };
        conn.execute("INSERT INTO ingress_gaps VALUES (?1,?2)",
            params![gap.gap_id.to_string(), serde_json::to_string(&gap).unwrap()]).unwrap();
        for row in rows {
            let event = &row.event;
            if version == 2 {
                conn.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body,source_key,eligible) \
                    VALUES (?1,?2,?3,?4,?5,?6,?7)",
                    params![i64::try_from(event.seq).unwrap(), event.spool_id.to_string(), event.dedup_key,
                        event.consumed, serde_json::to_string(event).unwrap(), row.source_key, row.eligible]).unwrap();
                let observation = IngressObservation {
                    event_id: event.spool_id, kind: ObservationKind::Original, raw: event.raw.clone(),
                };
                conn.execute("INSERT OR IGNORE INTO ingress_observations VALUES (?1,?2,?3)",
                    params![row.source_key, blake3::hash(&serde_json::to_vec(&event.raw).unwrap()).to_string(),
                        serde_json::to_string(&observation).unwrap()]).unwrap();
            } else {
                conn.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body) VALUES (?1,?2,?3,?4,?5)",
                    params![i64::try_from(event.seq).unwrap(), event.spool_id.to_string(), event.dedup_key,
                        event.consumed, serde_json::to_string(event).unwrap()]).unwrap();
            }
        }
        conn.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
    }

    fn event_rows(path: &Path) -> Vec<(i64, String, String, i64, String)> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT seq,spool_id,dedup_key,consumed,body FROM ingress_events ORDER BY seq").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    fn observation_rows(path: &Path) -> Vec<(i64, String, String, String)> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT rowid,source_key,raw_hash,body FROM ingress_observations ORDER BY rowid").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    // This authority owns the test's entire historical writer lifetime: the
    // duplicate fixture was inserted directly and no delivery loop existed.
    // The production verifier must establish its own real writer/consumer fence.
    struct FixtureEvidenceAuthority<'a> {
        store: &'a shenko_store::store::Store,
        never_sent: EntityId,
        commit: CommitRef,
    }
    impl IngressNeverDispatchedVerifier for FixtureEvidenceAuthority<'_> {
        fn verify(&self, certificate: &IngressNeverDispatchedCertificate, event: &SpooledEvent) -> Result<()> {
            ensure!(certificate.source_event_id == self.never_sent && event.spool_id == self.never_sent);
            ensure!(certificate.proof.sealed_commit == self.commit);
            self.store.cas.verified_object_path(&certificate.proof.object)?;
            let bytes = self.store.cas.read_object(&certificate.proof.object.digest)?;
            let facts: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(facts == json!({
                "cursor_id":certificate.cursor_id, "account":certificate.account,
                "agent_id":certificate.agent_id, "control_ledger_id":certificate.control_ledger_id,
                "original_event_id":certificate.original_event_id, "source_event_id":certificate.source_event_id,
                "source_seq":certificate.source_seq, "raw_hash":certificate.raw_hash,
            }));
            Ok(())
        }
    }

    fn fixture_certificate(
        store: &shenko_store::store::Store, cursor: &IngressCursor, agent: EntityId,
        original: EntityId, event: &SpooledEvent,
    ) -> IngressNeverDispatchedCertificate {
        let raw_hash = blake3::hash(&serde_json::to_vec(&event.raw).unwrap()).to_string();
        let facts = json!({"cursor_id":cursor.cursor_id, "account":cursor.account,
            "agent_id":agent, "control_ledger_id":store.ledger.ledger_id(),
            "original_event_id":original, "source_event_id":event.spool_id, "source_seq":event.seq, "raw_hash":raw_hash});
        let mut proof = store.cas.seal_object(&serde_json::to_vec(&facts).unwrap(),
            "application/json", "test", "test-owned-ingress-fence", 1).unwrap();
        proof.sealed_commit = store.commit(shenko_types::store::WriteBatch::new()).unwrap().commit_ref;
        IngressNeverDispatchedCertificate {
            cursor_id: cursor.cursor_id, account: cursor.account, agent_id: agent,
            control_ledger_id: store.ledger.ledger_id(), original_event_id: original,
            source_event_id: event.spool_id, source_seq: event.seq, raw_hash, proof,
        }
    }

    #[test]
    fn recover_after_process_exit() {
        // 同一测试的子进程分支在提交后直接退出，不跑连接析构/关闭检查点。
        const CHILD_PATH: &str = "SHENKO_INGRESS_RECOVERY_CHILD";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let spool = IngressSpool::open(path, 9).unwrap();
            let event = spool.ingest(message()).unwrap().unwrap();
            spool.mark_delivered(event.spool_id).unwrap();
            spool.record_disconnect().unwrap();
            spool.record_disconnect().unwrap();
            std::process::exit(0);
        }
        let scratch = Scratch::new();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "spool::tests::recover_after_process_exit",
                "--nocapture",
            ])
            .env(CHILD_PATH, scratch.db())
            .status()
            .unwrap();
        assert!(status.success());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        let cursor = spool.cursor().unwrap();
        assert_eq!(cursor.last_seq, 1);
        assert_eq!(cursor.gap_count, 2);
        let event = spool.consume(10).unwrap().pop().unwrap();
        assert!(!event.consumed && event.delivered_at.is_some());
        assert_eq!(spool.consume(10).unwrap()[0].spool_id, event.spool_id);
        assert!(spool.ingest(message()).unwrap().is_none());
        assert!(
            spool
                .gaps()
                .unwrap()
                .iter()
                .all(|g| g.status == GapStatus::Open)
        );
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let config = crate::NapCatConfig {
            bot_qq: 9,
            ws_url: "not-a-url".into(),
            ..Default::default()
        };
        let (client, _rx, actions) =
            crate::NapCatClient::open_persistent(config.clone(), scratch.db()).unwrap();
        let result = runtime.block_on(crate::NapCatClient::run_connection_loop(
            config,
            client.event_sender(),
            actions,
            client.pending_actions.clone(),
            client.spool.clone(),
            tokio_util::sync::CancellationToken::new(),
        ));
        assert!(result.is_err()); // URL 在本地拒绝，绝不连接外部服务。
        // Invalid URL may win the concurrent select before delivery; durable I
        // remains authoritative regardless of which future was polled first.
        assert_eq!(client.spool.consume(1).unwrap()[0].spool_id, event.spool_id);
        assert_eq!(client.spool.pending_count().unwrap(), 1);
        drop(client);
        spool.record_reconnect().unwrap();
        spool.record_disconnect().unwrap();
        spool
            .ack(event.spool_id, "control-ledger/step/committed-disposition")
            .unwrap();
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.cursor().unwrap().cursor_id, cursor.cursor_id);
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(spool.event(event.spool_id).unwrap().raw, message());
        assert!(spool.ingest(message()).unwrap().is_none());
        let gaps = spool.gaps().unwrap();
        assert_eq!(gaps[0].status, GapStatus::Reconciling);
        assert_eq!(gaps[1].status, GapStatus::Reconciling);
        assert_eq!(gaps[2].status, GapStatus::Reconciling);
        assert_eq!(gaps[3].status, GapStatus::Reconciling);
        assert_eq!(gaps[4].status, GapStatus::Open);
    }

    #[test]
    fn identities_do_not_collapse_distinct_source_events() {
        let scratch = Scratch::new();
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        let group = spool.ingest(message()).unwrap().unwrap();
        let private = json!({"post_type":"message", "message_type":"private", "user_id":7, "message_id":42, "time":1});
        let private = spool.ingest(private).unwrap().unwrap();
        let mut edit = message();
        edit["raw_message"] = json!("测试修订");
        assert!(spool.ingest(edit.clone()).unwrap().is_none());
        let notice = json!({"post_type":"notice", "time":1, "user_id":7});
        let first = spool.ingest(notice.clone()).unwrap().unwrap();
        let second = spool.ingest(notice).unwrap().unwrap();
        assert_ne!(group.dedup_key, private.dedup_key);
        let observations = spool.observations(group.spool_id).unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1].kind, ObservationKind::Conflict);
        assert_eq!(observations[1].raw, edit);
        assert!(first.weak_dedup && second.weak_dedup);
        assert_ne!(first.spool_id, second.spool_id);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 4);
        assert_eq!(spool.observations(group.spool_id).unwrap()[1].kind, ObservationKind::Conflict);
        assert_eq!(
            spool.event(first.spool_id).unwrap().dedup_key,
            first.dedup_key
        );
        assert_eq!(spool.ingest(message()).unwrap().map(|e| e.spool_id), None);
    }

    #[test]
    fn integer_identity_boundaries_and_unsigned_revisions() {
        let spool = IngressSpool::in_memory(9).unwrap();
        for id in [json!(i64::MIN), json!(-1), json!(1), json!(i64::MAX),
            json!(i64::MAX as u64 + 1), json!(u64::MAX)] {
            let mut raw = message();
            raw["message_id"] = id.clone();
            let event = spool.ingest(raw.clone()).unwrap().unwrap();
            assert!(!event.weak_dedup);
            raw["message_id"] = json!(id.to_string());
            assert!(spool.ingest(raw).unwrap().is_none());
        }
        for id in [json!(0), json!("0"), json!(""), json!(1.0), json!(null)] {
            let mut raw = message();
            raw["message_id"] = id;
            assert!(spool.ingest(raw.clone()).unwrap().unwrap().weak_dedup);
            assert!(spool.ingest(raw).unwrap().unwrap().weak_dedup);
        }
        let mut raw = message();
        raw["message_id"] = json!("00");
        assert!(!spool.ingest(raw.clone()).unwrap().unwrap().weak_dedup);
        assert!(spool.ingest(raw).unwrap().is_none());

        let spool = IngressSpool::in_memory(9).unwrap();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        raw["raw_message"] = json!("first");
        let original = spool.ingest(raw.clone()).unwrap().unwrap();
        let mut revision = raw.clone();
        revision["message_revision"] = json!(1);
        assert!(spool.ingest(revision).unwrap().is_none());
        let mut conflict = raw.clone();
        conflict["raw_message"] = json!("changed");
        assert!(spool.ingest(conflict).unwrap().is_none());
        let mut metadata = raw.clone();
        metadata["time"] = json!(2);
        metadata["message_id"] = json!(u64::MAX.to_string());
        assert!(spool.ingest(metadata).unwrap().is_none());
        let kinds: Vec<_> = spool.observations(original.spool_id).unwrap().into_iter().map(|o| o.kind).collect();
        assert_eq!(kinds, vec![ObservationKind::Original, ObservationKind::Revision,
            ObservationKind::Conflict, ObservationKind::MetadataOnly]);
        let mut other_group = raw.clone();
        other_group["group_id"] = json!(8);
        assert!(spool.ingest(other_group).unwrap().is_some());
        assert_ne!(source_identity(9, &raw), source_identity(10, &raw));
    }

    #[test]
    fn v2_migration_preserves_history_and_reconciles_string_and_numeric_rows() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        raw["raw_message"] = json!("original");
        let first = legacy_row(1, raw.clone(), true);
        let mut conflict_raw = raw.clone();
        conflict_raw["raw_message"] = json!("conflict");
        let mut conflict = legacy_row(2, conflict_raw, true);
        conflict.event.consumed = true;
        conflict.event.delivered_at = Some(Utc::now());
        conflict.event.disposition_ref = Some("ledger/conflict-disposition".into());
        let mut revision_raw = raw.clone();
        revision_raw["message_id"] = json!(u64::MAX.to_string());
        revision_raw["message_revision"] = json!(2);
        revision_raw["raw_message"] = json!("revision");
        let mut revision = legacy_row(3, revision_raw, false);
        revision.event.delivered_at = Some(Utc::now());
        let mut singleton_raw = raw.clone();
        singleton_raw["message_id"] = json!(u64::MAX - 1);
        let singleton = legacy_row(4, singleton_raw, true);
        let ids = [first.event.spool_id, conflict.event.spool_id, revision.event.spool_id, singleton.event.spool_id];
        let rows = [first, conflict, revision, singleton];
        legacy_spool(&scratch.db(), 2, &rows);
        // A prior observation of the string identity belongs to the old root;
        // it must remain byte-for-byte intact even when an earlier root appears.
        let mut old_metadata = rows[2].event.raw.clone();
        old_metadata["time"] = json!(99);
        let old_observation = IngressObservation {
            event_id: ids[2], kind: ObservationKind::MetadataOnly, raw: old_metadata.clone(),
        };
        let conn = Connection::open(scratch.db()).unwrap();
        conn.execute("INSERT INTO ingress_observations VALUES (?1,?2,?3)",
            params![rows[2].source_key, blake3::hash(&serde_json::to_vec(&old_metadata).unwrap()).to_string(),
                serde_json::to_string(&old_observation).unwrap()]).unwrap();
        let cursor_before: String = conn.query_row("SELECT body FROM ingress_cursor", [], |r| r.get(0)).unwrap();
        let gap_before: String = conn.query_row("SELECT body FROM ingress_gaps", [], |r| r.get(0)).unwrap();
        drop(conn);
        let events_before = event_rows(&scratch.db());
        let observations_before = observation_rows(&scratch.db());

        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(event_rows(&scratch.db()), events_before);
        assert_eq!(observation_rows(&scratch.db()), observations_before);
        let conn = Connection::open(scratch.db()).unwrap();
        assert_eq!(conn.query_row("SELECT body FROM ingress_cursor", [], |r| r.get::<_, String>(0)).unwrap(), cursor_before);
        assert_eq!(conn.query_row("SELECT body FROM ingress_gaps", [], |r| r.get::<_, String>(0)).unwrap(), gap_before);
        drop(conn);
        assert_eq!(spool.consume(10).unwrap().iter().map(|e| e.spool_id).collect::<Vec<_>>(), vec![ids[3]]);
        assert!(spool.event(ids[0]).unwrap().weak_dedup);
        assert_eq!(spool.event(ids[1]).unwrap().disposition_ref.as_deref(), Some("ledger/conflict-disposition"));
        let reconciliation = spool.identity_reconciliation(ids[2]).unwrap();
        assert_eq!(reconciliation.iter().map(|r| r.source_event_id).collect::<Vec<_>>(), vec![ids[0], ids[1], ids[2]]);
        assert!(reconciliation.iter().all(|r| r.original_event_id == ids[0] && r.delivery_uncertain));
        assert_eq!(reconciliation.iter().map(|r| r.kind.clone()).collect::<Vec<_>>(),
            vec![ObservationKind::Original, ObservationKind::Conflict, ObservationKind::Revision]);
        let historical = spool.observations(ids[0]).unwrap();
        assert_eq!(historical.len(), 4);
        assert_eq!(historical[3].event_id, ids[2]);
        assert_eq!(historical[3].raw, old_metadata);
        assert!(spool.ingest(raw.clone()).unwrap().is_none());
        assert!(spool.ingest(rows[2].event.raw.clone()).unwrap().is_none());
        assert_eq!(spool.observations(ids[0]).unwrap().len(), 4);
        let mut later_revision = raw;
        later_revision["message_revision"] = json!(3);
        assert!(spool.ingest(later_revision).unwrap().is_none());
        assert_eq!(spool.observations(ids[0]).unwrap().last().unwrap().kind, ObservationKind::Revision);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.consume(10).unwrap()[0].spool_id, ids[3]);
        assert_eq!(spool.pending_count().unwrap(), 1);
        assert_eq!(spool.identity_reconciliation(ids[0]).unwrap().len(), 3);
        assert_eq!(spool.observations(ids[1]).unwrap().len(), 5);
        assert!(spool.ingest(rows[3].event.raw.clone()).unwrap().is_none());
        let mut singleton_string = rows[3].event.raw.clone();
        singleton_string["message_id"] = json!((u64::MAX - 1).to_string());
        assert!(spool.ingest(singleton_string).unwrap().is_none());
    }

    #[test]
    fn v2_earlier_string_identity_and_distinct_conversations() {
        let scratch = Scratch::new();
        let mut string_raw = message();
        string_raw["message_id"] = json!(u64::MAX.to_string());
        string_raw["raw_message"] = json!("original");
        let mut first = legacy_row(1, string_raw.clone(), false);
        first.event.consumed = true;
        first.event.disposition_ref = Some("ledger/original".into());
        let mut numeric_raw = string_raw;
        numeric_raw["message_id"] = json!(u64::MAX);
        numeric_raw["message_revision"] = json!(1);
        let later = legacy_row(2, numeric_raw.clone(), true);
        let mut other_group = numeric_raw;
        other_group["group_id"] = json!(8);
        let separate = legacy_row(3, other_group, true);
        let rows = [first, later, separate];
        legacy_spool(&scratch.db(), 2, &rows);
        let before = event_rows(&scratch.db());
        let observations_before = observation_rows(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(event_rows(&scratch.db()), before);
        assert_eq!(observation_rows(&scratch.db()), observations_before);
        assert_eq!(spool.consume(10).unwrap()[0].spool_id, rows[2].event.spool_id);
        assert_eq!(spool.pending_count().unwrap(), 1);
        let group = spool.identity_reconciliation(rows[1].event.spool_id).unwrap();
        assert_eq!(group[0].original_event_id, rows[0].event.spool_id);
        assert_eq!(group[1].kind, ObservationKind::Revision);
        assert!(group.iter().all(|r| r.delivery_uncertain));
        assert!(spool.ingest(rows[1].event.raw.clone()).unwrap().is_none());
        assert_eq!(spool.observations(rows[0].event.spool_id).unwrap().len(), 2);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.event(rows[0].event.spool_id).unwrap().disposition_ref.as_deref(), Some("ledger/original"));
        assert_eq!(spool.pending_count().unwrap(), 1);
        assert_eq!(spool.identity_reconciliation(rows[1].event.spool_id).unwrap().len(), 2);
    }

    #[test]
    fn unknown_previous_delivery_holds_all_rows_even_without_delivery_timestamps() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(i64::MAX as u64 + 1);
        let rows = [legacy_row(1, raw.clone(), true), legacy_row(2, raw.clone(), true)];
        legacy_spool(&scratch.db(), 2, &rows);
        let old_observations = observation_rows(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(observation_rows(&scratch.db()), old_observations);
        assert_eq!(spool.observations(rows[0].event.spool_id).unwrap().len(), 2);
        let group = spool.identity_reconciliation(rows[1].event.spool_id).unwrap();
        assert_eq!(group.len(), 2);
        assert!(group.iter().all(|r| r.delivery_uncertain));
        assert_eq!(group[1].kind, ObservationKind::MetadataOnly);
        assert!(spool.ingest(raw).unwrap().is_none());
        spool.ack(rows[1].event.spool_id, "ledger/later-disposition").unwrap();
        assert!(!spool.event(rows[0].event.spool_id).unwrap().consumed);
        assert_eq!(spool.pending_count().unwrap(), 0);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.event(rows[1].event.spool_id).unwrap().disposition_ref.as_deref(), Some("ledger/later-disposition"));
        assert_eq!(spool.pending_count().unwrap(), 0);
    }

    #[test]
    fn v1_upgrade_handles_duplicates_singletons_and_unidentifiable_events() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        raw["raw_message"] = json!("original");
        let first = legacy_row(1, raw.clone(), true);
        let mut changed = raw.clone();
        changed["raw_message"] = json!("conflict");
        let second = legacy_row(2, changed, true);
        let mut singleton = raw.clone();
        singleton["message_id"] = json!(i64::MAX as u64 + 1);
        let third = legacy_row(3, singleton, true);
        let mut unidentified = raw;
        unidentified["message_type"] = json!("unknown");
        let fourth = legacy_row(4, unidentified, true);
        let rows = [first, second, third, fourth];
        legacy_spool(&scratch.db(), 1, &rows);
        let before = event_rows(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(event_rows(&scratch.db()), before);
        assert_eq!(spool.consume(10).unwrap().iter().map(|e| e.spool_id).collect::<Vec<_>>(),
            vec![rows[2].event.spool_id, rows[3].event.spool_id]);
        let group = spool.identity_reconciliation(rows[0].event.spool_id).unwrap();
        assert_eq!(group.len(), 2);
        assert_eq!(group[1].kind, ObservationKind::Conflict);
        assert!(group.iter().all(|r| r.delivery_uncertain));
        assert!(spool.identity_reconciliation(rows[3].event.spool_id).unwrap().is_empty());
        assert!(spool.ingest(rows[0].event.raw.clone()).unwrap().is_none());
        assert!(spool.ingest(rows[2].event.raw.clone()).unwrap().is_none());
        assert!(spool.ingest(rows[3].event.raw.clone()).unwrap().unwrap().weak_dedup);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 3);
        assert_eq!(spool.identity_reconciliation(rows[0].event.spool_id).unwrap().len(), 2);
    }

    #[test]
    fn unsafe_legacy_binding_and_wrong_account_roll_back_schema_migration() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let mut bad = legacy_row(1, raw.clone(), true);
        bad.source_key = "not-the-weak-spool-key".into();
        legacy_spool(&scratch.db(), 2, &[bad]);
        let events_before = event_rows(&scratch.db());
        let observations_before = observation_rows(&scratch.db());
        assert!(IngressSpool::open(scratch.db(), 9).is_err());
        let conn = Connection::open(scratch.db()).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        let columns: i64 = conn.query_row("SELECT count(*) FROM pragma_table_info('ingress_observations') \
            WHERE name='canonical_source_key'", [], |r| r.get(0)).unwrap();
        assert_eq!(columns, 0);
        let tables: i64 = conn.query_row("SELECT count(*) FROM sqlite_master WHERE name='ingress_identity_reconciliation'", [], |r| r.get(0)).unwrap();
        assert_eq!(tables, 0);
        drop(conn);
        assert_eq!(event_rows(&scratch.db()), events_before);
        assert_eq!(observation_rows(&scratch.db()), observations_before);

        let inconsistent = scratch.0.join("bad-observation.sqlite");
        let row = legacy_row(1, raw.clone(), true);
        let id = row.event.spool_id;
        legacy_spool(&inconsistent, 2, &[row]);
        let conn = Connection::open(&inconsistent).unwrap();
        let mut wrong_raw = raw.clone();
        wrong_raw["message_id"] = json!(u64::MAX - 1);
        let wrong = IngressObservation { event_id: id, kind: ObservationKind::Original, raw: wrong_raw.clone() };
        conn.execute("UPDATE ingress_observations SET raw_hash=?1,body=?2", params![
            blake3::hash(&serde_json::to_vec(&wrong_raw).unwrap()).to_string(), serde_json::to_string(&wrong).unwrap(),
        ]).unwrap();
        drop(conn);
        let inconsistent_before = event_rows(&inconsistent);
        let observations_before = observation_rows(&inconsistent);
        assert!(IngressSpool::open(&inconsistent, 9).is_err());
        assert_eq!(event_rows(&inconsistent), inconsistent_before);
        assert_eq!(observation_rows(&inconsistent), observations_before);
        let conn = Connection::open(&inconsistent).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        drop(conn);

        let other = scratch.0.join("wrong-account.sqlite");
        let row = legacy_row(1, raw, true);
        legacy_spool(&other, 2, &[row]);
        let before = event_rows(&other);
        assert!(IngressSpool::open(&other, 10).is_err());
        let conn = Connection::open(&other).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 2);
        drop(conn);
        assert_eq!(event_rows(&other), before);
        assert_eq!(IngressSpool::open(&other, 9).unwrap().pending_count().unwrap(), 1);
    }

    #[test]
    fn every_duplicate_requires_matching_fenced_evidence_before_release() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let rows = [legacy_row(1, raw.clone(), true), legacy_row(2, raw.clone(), true), legacy_row(3, raw, true)];
        let ids = [rows[0].event.spool_id, rows[1].event.spool_id, rows[2].event.spool_id];
        legacy_spool(&scratch.db(), 2, &rows);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        let store = shenko_store::store::Store::open(scratch.0.join("ledger.sqlite"), scratch.0.join("cas")).unwrap();
        let agent = EntityId::new();
        let cursor = spool.cursor().unwrap();
        let first_proof = fixture_certificate(&store, &cursor, agent, ids[0], &rows[1].event);
        let first_authority = FixtureEvidenceAuthority { store: &store, never_sent: ids[1], commit: first_proof.proof.sealed_commit };
        let partial = spool.submit_never_dispatched(&store, agent, first_proof, &first_authority).unwrap();
        assert_eq!(partial.state, IngressReconciliationState::AwaitingEvidence);
        assert_eq!(partial.unresolved_event_ids, vec![ids[0], ids[2]]);
        assert_eq!(spool.pending_count().unwrap(), 0);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.reconciliation_status(ids[0]).unwrap().state, IngressReconciliationState::AwaitingEvidence);
        let second_proof = fixture_certificate(&store, &cursor, agent, ids[0], &rows[2].event);
        let second_authority = FixtureEvidenceAuthority { store: &store, never_sent: ids[2], commit: second_proof.proof.sealed_commit };
        let complete = spool.submit_never_dispatched(&store, agent, second_proof, &second_authority).unwrap();
        assert_eq!(complete.state, IngressReconciliationState::OriginalReleased);
        assert_eq!(complete.unresolved_event_ids, vec![ids[0]]);
        assert_eq!(complete.evidence.len(), 2);
        assert_eq!(spool.consume(10).unwrap()[0].spool_id, ids[0]);
        assert_eq!(spool.pending_count().unwrap(), 1);

        let held_path = scratch.0.join("previous-hold.sqlite");
        let mut held_first = legacy_row(1, rows[0].event.raw.clone(), true);
        held_first.eligible = false;
        let held_later = legacy_row(2, rows[0].event.raw.clone(), true);
        let held_ids = [held_first.event.spool_id, held_later.event.spool_id];
        let certificate_event = held_later.event.clone();
        legacy_spool(&held_path, 2, &[held_first, held_later]);
        let held = IngressSpool::open(&held_path, 9).unwrap();
        let proof = fixture_certificate(&store, &held.cursor().unwrap(), agent, held_ids[0], &certificate_event);
        let authority = FixtureEvidenceAuthority { store: &store, never_sent: held_ids[1], commit: proof.proof.sealed_commit };
        let status = held.submit_never_dispatched(&store, agent, proof, &authority).unwrap();
        assert_eq!(status.state, IngressReconciliationState::PriorEligibilityHold);
        assert_eq!(held.pending_count().unwrap(), 0);
    }

    #[tokio::test]
    async fn fenced_evidence_releases_original_behind_live_delivery_watermark() {
        use std::sync::Arc;
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let first = legacy_row(1, raw.clone(), true);
        let duplicate = legacy_row(2, raw.clone(), true);
        let later = legacy_row(3, message(), false);
        let ids = [first.event.spool_id, duplicate.event.spool_id, later.event.spool_id];
        legacy_spool(&scratch.db(), 2, &[first, duplicate, later]);
        let spool = Arc::new(IngressSpool::open(scratch.db(), 9).unwrap());
        let store = shenko_store::store::Store::open(scratch.0.join("ledger.sqlite"), scratch.0.join("cas")).unwrap();
        let agent = EntityId::new();
        let initial = spool.reconcile_from_store(&store, agent, ids[0]).unwrap();
        assert_eq!(initial.state, IngressReconciliationState::AwaitingEvidence);
        assert_eq!(initial.unresolved_event_ids, vec![ids[0], ids[1]]);
        assert_eq!(spool.reconciliation_groups_after(0, 10).unwrap(), vec![(1, ids[0])]);

        let cursor = spool.cursor().unwrap();
        let duplicate = spool.event(ids[1]).unwrap();
        let certificate = fixture_certificate(&store, &cursor, agent, ids[0], &duplicate);
        let authority = FixtureEvidenceAuthority { store: &store, never_sent: ids[1], commit: certificate.proof.sealed_commit };
        let mut wrong = certificate.clone();
        wrong.cursor_id = EntityId::new();
        assert!(spool.submit_never_dispatched(&store, agent, wrong, &authority).is_err());
        let mut invalid_proof = certificate.clone();
        invalid_proof.proof.object = store.cas.seal_object(b"{}", "application/json", "test", "wrong-proof", 1).unwrap().object;
        assert!(spool.submit_never_dispatched(&store, agent, invalid_proof, &authority).is_err());
        assert_eq!(spool.reconciliation_status(ids[0]).unwrap().state, IngressReconciliationState::AwaitingEvidence);
        assert!(spool.reconciliation_status(ids[0]).unwrap().evidence.is_empty());

        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker = tokio::spawn(crate::client::NapCatClient::delivery_loop(tx, Arc::clone(&spool), cancel.clone()));
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.ingress_id, Some(ids[2]));
        let released = spool.submit_never_dispatched(&store, agent, certificate.clone(), &authority).unwrap();
        assert_eq!(released.state, IngressReconciliationState::OriginalReleased);
        assert_eq!(released.unresolved_event_ids, vec![ids[0]]);
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.ingress_id, Some(ids[0]));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(350), rx.recv()).await.is_err());
        spool.submit_never_dispatched(&store, agent, certificate, &authority).unwrap();
        assert_eq!(spool.release_horizon().unwrap(), 1);
        cancel.cancel();
        worker.await.unwrap().unwrap();
        drop(spool);
        let spool = Arc::new(IngressSpool::open(scratch.db(), 9).unwrap());
        assert_eq!(spool.reconciliation_status(ids[1]).unwrap().state, IngressReconciliationState::OriginalReleased);
        assert_eq!(spool.consume(10).unwrap().iter().map(|event| event.spool_id).collect::<Vec<_>>(), vec![ids[2]]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let cancel = tokio_util::sync::CancellationToken::new();
        let worker = tokio::spawn(crate::client::NapCatClient::delivery_loop(tx, Arc::clone(&spool), cancel.clone()));
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.ingress_id, Some(ids[2]));
        spool.bind_control_store(&store, agent).unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(event.ingress_id, Some(ids[0]));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(350), rx.recv()).await.is_err());
        assert_eq!(spool.consume(10).unwrap().iter().map(|event| event.spool_id).collect::<Vec<_>>(), vec![ids[0], ids[2]]);
        assert!(spool.ingest(raw).unwrap().is_none());
        cancel.cancel();
        worker.await.unwrap().unwrap();
        // A late contradictory delivery fact must not erase either the fact or
        // the prior certificate, and immediately fences the released original.
        spool.mark_delivered(ids[1]).unwrap();
        let status = spool.reconciliation_status(ids[0]).unwrap();
        assert_eq!(status.state, IngressReconciliationState::ContradictoryEvidence);
        assert_eq!(status.evidence.len(), 1);
        assert_eq!(spool.pending_count().unwrap(), 1);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        spool.bind_control_store(&store, agent).unwrap();
        assert_eq!(spool.reconciliation_status(ids[0]).unwrap().state, IngressReconciliationState::ContradictoryEvidence);
        assert_eq!(spool.pending_count().unwrap(), 1);
    }

    #[test]
    fn commit_failure_fences_handle_and_reopen_recovers() {
        let scratch = Scratch::new();
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        // 在真实 SQLite COMMIT 路径注入拒绝，验证事件和 cursor 同时回滚。
        spool.backend.lock().connection.commit_hook(Some(|| true));
        let error = spool.ingest(message()).unwrap_err();
        assert!(error.to_string().contains("提交失败"));
        assert!(spool.ingest(message()).is_err());
        assert!(spool.record_disconnect().is_err());
        assert!(spool.consume(1).is_err());
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.cursor().unwrap().last_seq, 0);
        assert_eq!(spool.pending_count().unwrap(), 0);
        assert_eq!(spool.ingest(message()).unwrap().unwrap().seq, 1);
        assert!(IngressSpool::open(scratch.0.join("missing").join("spool.sqlite"), 9).is_err());
    }
}
