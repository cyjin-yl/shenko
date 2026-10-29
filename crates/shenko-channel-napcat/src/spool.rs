//! NapCat 独立接收域 I：原始事件、去重身份、水位和 gap 同库持久化。
//! 不与主体控制 ledger L 跨库提交；ack 只是 L 中已提交处置的镜像。
use std::{borrow::Cow, path::Path};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use shenko_types::ids::EntityId;

const APPLICATION_ID: i64 = 0x534e4947;
const UNSIGNED_WIRE_INDEX: &str = "CREATE INDEX ingress_unsigned_wire_id ON ingress_events ((body -> '$.raw.message_id'), seq) WHERE json_type(body, '$.raw.message_id') = 'integer'";

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
        // Reject the published incompatible v3 before changing journal mode or
        // opening a writer transaction. Recheck after acquiring the lock too.
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!((0..=2).contains(&version),
            "接收库版本不是本 reader 支持的 v1/v2；保留原库及 WAL，使用支持该版本的恢复工具");
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
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!((0..=2).contains(&version),
            "接收库版本已改变；保留原库及 WAL，使用支持该版本的恢复工具");
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
                    let key = legacy_v1_source_identity(account, &event.raw).unwrap_or_else(|| event.dedup_key.clone());
                    let original = first_source_event(&tx, &key)?;
                    let kind = original.as_ref().map_or(ObservationKind::Original, |first| observation_kind(&first.raw, &event.raw));
                    let hash = blake3::hash(&serde_json::to_vec(&event.raw)?);
                    save_observation(&tx, &key, original.as_ref().map_or(event.spool_id, |e| e.spool_id), kind, &event.raw, &hash)?;
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
        // SQLite maintains this additive index even for legacy writers. It
        // indexes JSON text, never a rounded SQLite INTEGER/REAL conversion;
        // no event, eligibility, observation or ACK is rewritten.
        let existing_index: Option<String> = tx.query_row(
            "SELECT sql FROM sqlite_master WHERE type='index' AND name='ingress_unsigned_wire_id'",
            [], |r| r.get(0),
        ).optional()?;
        if let Some(existing_index) = existing_index {
            ensure!(existing_index == UNSIGNED_WIRE_INDEX, "接收库 unsigned 索引定义不匹配");
        } else {
            tx.execute_batch(UNSIGNED_WIRE_INDEX)?;
        }
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", 2)?;
        tx.commit()?;
        Ok(Self {
            backend: Mutex::new(Backend {
                connection,
                failed: false,
            }),
            persistent,
        })
    }

    pub fn is_persistent(&self) -> bool {
        self.persistent
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
            let hash = blake3::hash(&serde_json::to_vec(&raw)?);
            let identity = source_identity(cursor.account, &raw);
            if let Some(key) = identity.as_deref() {
                // Preserve the existing strong-source routing when one is
                // already present, including its observation history.
                if let Some(original) = first_source_event(tx, key)? {
                    save_observation(tx, key, original.spool_id, observation_kind(&original.raw, &raw), &raw, &hash)?;
                    return Ok(None);
                }
                if let Some(wire_id) = high_unsigned_wire_id(&raw) {
                    if let Some((stored_key, original)) = first_unsigned_source_event(tx, cursor.account, key, &wire_id)? {
                        save_observation(tx, &stored_key, original.spool_id, observation_kind(&original.raw, &raw), &raw, &hash)?;
                        return Ok(None);
                    }
                }
            }
            let spool_id = EntityId::new();
            let (dedup_key, weak_dedup) = match &identity {
                Some(key) => (format!("{key}:{hash}"), false),
                None => (format!("napcat:{}:onebot_v11:spool:{spool_id}", cursor.account), true),
            };
            let source_key = identity.unwrap_or_else(|| dedup_key.clone());
            if weak_dedup {
                if let Some(original) = first_source_event(tx, &source_key)? {
                    save_observation(tx, &source_key, original.spool_id, observation_kind(&original.raw, &raw), &raw, &hash)?;
                    return Ok(None);
                }
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
            save_observation(tx, &source_key, event.spool_id, ObservationKind::Original, &event.raw, &hash)?;
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
        let mut stmt = backend.connection.prepare(
            "SELECT body FROM ingress_events WHERE eligible=1 AND consumed=0 AND seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, i64::try_from(max)?], |r| {
            r.get::<_, String>(0)
        })?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    /// Raw repeat/conflict evidence; never returned by the delivery scan.
    pub fn observations(&self, id: EntityId) -> Result<Vec<IngressObservation>> {
        let backend = self.backend.lock();
        ensure!(!backend.failed, "接收库 handle 已失效，必须重开");
        let mut stmt = backend.connection.prepare(
            "SELECT o.body FROM ingress_observations o JOIN ingress_events e ON e.source_key=o.source_key WHERE e.spool_id=?1 ORDER BY o.rowid",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| r.get::<_, String>(0))?;
        rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
    }

    pub fn mark_delivered(&self, id: EntityId) -> Result<()> {
        self.mutate(|tx| {
            let mut event = read_event(tx, id)?;
            event.delivered_at = Some(Utc::now());
            save_event(tx, &event)
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
            save_event(tx, &event)
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
        let count: i64 = backend.connection.query_row(
            "SELECT count(*) FROM ingress_events WHERE eligible=1 AND consumed=0",
            [],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(count)?)
    }
    pub fn gap_count(&self) -> Result<u64> {
        Ok(self.cursor()?.gap_count)
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

// The v1 -> v2 migration must retain the predecessor's signed-only grouping.
// New ingest supports u64, but must not silently coalesce historical v1 rows.
fn legacy_v1_source_identity(account: u64, raw: &serde_json::Value) -> Option<String> {
    if let Some(serde_json::Value::Number(number)) = raw.get("message_id") {
        if number.as_i64().is_none() { return None; }
    }
    source_identity(account, raw)
}

fn high_unsigned_wire_id(raw: &serde_json::Value) -> Option<Cow<'_, str>> {
    match raw.get("message_id")? {
        serde_json::Value::Number(number) => number.as_u64()
            .filter(|id| *id > i64::MAX as u64).map(|id| Cow::Owned(id.to_string())),
        serde_json::Value::String(text) => {
            let id = text.parse::<u64>().ok()?;
            if id > i64::MAX as u64 && !text.starts_with('0') && text.bytes().all(|byte| byte.is_ascii_digit()) {
                Some(Cow::Borrowed(text))
            } else { None }
        }
        _ => None,
    }
}

/// Resolve the first actual historical row for this full source identity. The
/// indexed text token identifies candidates; Rust checks the exact account,
/// typed conversation and canonical ID before binding an observation.
fn first_unsigned_source_event(
    connection: &Connection, account: u64, identity: &str, wire_id: &str,
) -> Result<Option<(String, SpooledEvent)>> {
    let mut stmt = connection.prepare(
        "SELECT seq,spool_id,dedup_key,source_key,body FROM ingress_events INDEXED BY ingress_unsigned_wire_id \
         WHERE json_type(body, '$.raw.message_id') = 'integer' AND (body -> '$.raw.message_id') = ?1 ORDER BY seq",
    )?;
    let mut rows = stmt.query([wire_id])?;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let spool_id: String = row.get(1)?;
        let dedup_key: String = row.get(2)?;
        let stored_key: Option<String> = row.get(3)?;
        let body: String = row.get(4)?;
        let event: SpooledEvent = serde_json::from_str(&body)?;
        if source_identity(account, &event.raw).as_deref() != Some(identity) { continue; }
        ensure!(i64::try_from(event.seq)? == seq && event.spool_id.to_string() == spool_id
            && event.dedup_key == dedup_key, "旧接收事件身份与存储列不一致 seq={seq}");
        let stored_key = stored_key.context("旧接收事件缺少来源 key")?;
        if stored_key != identity {
            let weak_key = format!("napcat:{account}:onebot_v11:spool:{}", event.spool_id);
            ensure!(event.weak_dedup && stored_key == event.dedup_key && stored_key == weak_key,
                "旧接收事件来源 key 无法安全绑定 seq={seq}");
        }
        return Ok(Some((stored_key, event)));
    }
    Ok(None)
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

fn save_observation(tx: &Transaction<'_>, key: &str, event_id: EntityId, kind: ObservationKind, raw: &serde_json::Value, hash: &blake3::Hash) -> Result<()> {
    let observation = IngressObservation { event_id, kind, raw: raw.clone() };
    tx.execute("INSERT OR IGNORE INTO ingress_observations VALUES (?1,?2,?3)",
        params![key, hash.to_string(), serde_json::to_string(&observation)?])?;
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

    fn historical_event(seq: u64, raw: serde_json::Value, consumed: bool) -> SpooledEvent {
        let spool_id = EntityId::new();
        SpooledEvent {
            spool_id,
            dedup_key: format!("napcat:9:onebot_v11:spool:{spool_id}"),
            seq,
            received_at: Utc::now(),
            consumed,
            weak_dedup: true,
            delivered_at: None,
            disposition_ref: consumed.then(|| "control-ledger/old-disposition".to_owned()),
            raw,
        }
    }

    // These layouts are written directly as their actual historical schemas;
    // the v1 fixture has no observation or source-key columns.
    fn historical_spool(path: &Path, version: i64, events: &[SpooledEvent]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("
            CREATE TABLE ingress_cursor (singleton INTEGER PRIMARY KEY CHECK(singleton=1), body TEXT NOT NULL);
            CREATE TABLE ingress_events (seq INTEGER PRIMARY KEY AUTOINCREMENT, spool_id TEXT NOT NULL UNIQUE,
                dedup_key TEXT NOT NULL UNIQUE, consumed INTEGER NOT NULL DEFAULT 0, body TEXT NOT NULL);
            CREATE INDEX ingress_pending ON ingress_events(consumed,seq);
            CREATE TABLE ingress_gaps (gap_id TEXT PRIMARY KEY, body TEXT NOT NULL);
        ").unwrap();
        if version == 2 {
            conn.execute_batch("
                ALTER TABLE ingress_events ADD COLUMN source_key TEXT;
                ALTER TABLE ingress_events ADD COLUMN eligible INTEGER NOT NULL DEFAULT 1;
                CREATE INDEX ingress_source ON ingress_events(source_key,seq);
                CREATE INDEX ingress_delivery ON ingress_events(eligible,consumed,seq);
                CREATE TABLE ingress_observations (source_key TEXT NOT NULL, raw_hash TEXT NOT NULL,
                    body TEXT NOT NULL, PRIMARY KEY(source_key,raw_hash));
            ").unwrap();
        }
        let mut cursor = IngressCursor::new("napcat", 9);
        cursor.last_seq = events.last().map_or(0, |event| event.seq);
        conn.execute("INSERT INTO ingress_cursor VALUES (1,?1)", [serde_json::to_string(&cursor).unwrap()]).unwrap();
        for event in events {
            let body = serde_json::to_string(event).unwrap();
            if version == 2 {
                conn.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body,source_key,eligible) \
                    VALUES (?1,?2,?3,?4,?5,?3,1)",
                    params![i64::try_from(event.seq).unwrap(), event.spool_id.to_string(), event.dedup_key,
                        event.consumed, body]).unwrap();
                let original = IngressObservation { event_id: event.spool_id, kind: ObservationKind::Original, raw: event.raw.clone() };
                conn.execute("INSERT INTO ingress_observations VALUES (?1,?2,?3)", params![event.dedup_key,
                    blake3::hash(&serde_json::to_vec(&event.raw).unwrap()).to_string(), serde_json::to_string(&original).unwrap()]).unwrap();
            } else {
                conn.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body) VALUES (?1,?2,?3,?4,?5)",
                    params![i64::try_from(event.seq).unwrap(), event.spool_id.to_string(), event.dedup_key, event.consumed, body]).unwrap();
            }
        }
        conn.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
    }

    fn historical_rows(path: &Path) -> Vec<(i64, String, String, i64, Option<String>, i64, String)> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT seq,spool_id,dedup_key,consumed,source_key,eligible,body FROM ingress_events ORDER BY seq").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?))).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    }

    fn historical_observations(path: &Path) -> Vec<(i64, String, String, String)> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn.prepare("SELECT rowid,source_key,raw_hash,body FROM ingress_observations ORDER BY rowid").unwrap();
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
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
    fn fresh_first_unsigned_value_is_strong_across_string_replay() {
        let spool = IngressSpool::in_memory(9).unwrap();
        let id = i64::MAX as u64 + 1;
        let mut raw = message();
        raw["message_id"] = json!(id);
        let first = spool.ingest(raw.clone()).unwrap().unwrap();
        assert!(!first.weak_dedup);
        raw["message_id"] = json!(id.to_string());
        assert!(spool.ingest(raw).unwrap().is_none());
        assert_eq!(spool.pending_count().unwrap(), 1);
        assert_eq!(spool.observations(first.spool_id).unwrap()[1].kind, ObservationKind::MetadataOnly);
    }

    #[test]
    fn numeric_replay_matches_historical_weak_without_changing_rows() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let first = historical_event(1, raw.clone(), false);
        let second = historical_event(2, raw.clone(), true);
        historical_spool(&scratch.db(), 2, &[first.clone(), second.clone()]);
        let before = historical_rows(&scratch.db());
        let old_observations = historical_observations(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert!(spool.ingest(raw.clone()).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), before);
        assert_eq!(historical_observations(&scratch.db()), old_observations);
        assert_eq!(spool.cursor().unwrap().last_seq, 2);
        assert_eq!(spool.consume(10).unwrap().iter().map(|event| event.spool_id).collect::<Vec<_>>(), vec![first.spool_id]);
        assert_eq!(spool.event(second.spool_id).unwrap().disposition_ref.as_deref(), Some("control-ledger/old-disposition"));
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert!(spool.ingest(raw).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), before);
    }

    #[test]
    fn decimal_string_revision_is_observation_of_historical_numeric_row() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let old = historical_event(1, raw.clone(), false);
        historical_spool(&scratch.db(), 2, &[old.clone()]);
        let before = historical_rows(&scratch.db());
        let old_observations = historical_observations(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        raw["message_id"] = json!(u64::MAX.to_string());
        raw["message_revision"] = json!(1);
        assert!(spool.ingest(raw.clone()).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), before);
        let observations = spool.observations(old.spool_id).unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1].event_id, old.spool_id);
        assert_eq!(observations[1].kind, ObservationKind::Revision);
        assert_eq!(observations[1].raw, raw);
        assert_eq!(&historical_observations(&scratch.db())[..1], old_observations.as_slice());
    }

    #[test]
    fn existing_strong_source_keeps_its_observation_routing() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let weak = historical_event(1, raw.clone(), false);
        historical_spool(&scratch.db(), 2, &[weak.clone()]);
        raw["message_id"] = json!(u64::MAX.to_string());
        let mut strong = historical_event(2, raw.clone(), false);
        let key = source_identity(9, &raw).unwrap();
        strong.dedup_key = format!("{key}:{}", blake3::hash(&serde_json::to_vec(&raw).unwrap()));
        strong.weak_dedup = false;
        let conn = Connection::open(scratch.db()).unwrap();
        conn.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body,source_key,eligible) \
            VALUES (?1,?2,?3,?4,?5,?6,1)", params![2, strong.spool_id.to_string(), strong.dedup_key,
                strong.consumed, serde_json::to_string(&strong).unwrap(), key]).unwrap();
        let original = IngressObservation { event_id: strong.spool_id, kind: ObservationKind::Original, raw: raw.clone() };
        conn.execute("INSERT INTO ingress_observations VALUES (?1,?2,?3)", params![key,
            blake3::hash(&serde_json::to_vec(&raw).unwrap()).to_string(), serde_json::to_string(&original).unwrap()]).unwrap();
        let mut cursor = read_cursor(&conn).unwrap();
        cursor.advance(2);
        conn.execute("UPDATE ingress_cursor SET body=?1 WHERE singleton=1", [serde_json::to_string(&cursor).unwrap()]).unwrap();
        drop(conn);
        let before = historical_rows(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        raw["time"] = json!(2);
        assert!(spool.ingest(raw.clone()).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), before);
        assert_eq!(spool.observations(weak.spool_id).unwrap().len(), 1);
        let observations = spool.observations(strong.spool_id).unwrap();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[1].kind, ObservationKind::MetadataOnly);
        assert_eq!(observations[1].raw, raw);
    }

    #[test]
    fn legacy_wire_id_does_not_collapse_typed_conversations() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        raw["group_id"] = json!("7");
        let old = historical_event(1, raw.clone(), false);
        historical_spool(&scratch.db(), 2, &[old.clone()]);
        let before = historical_rows(&scratch.db());
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        raw["group_id"] = json!(7);
        let fresh = spool.ingest(raw).unwrap().unwrap();
        assert!(!fresh.weak_dedup);
        assert_ne!(fresh.spool_id, old.spool_id);
        assert_eq!(&historical_rows(&scratch.db())[..1], before.as_slice());
        assert_eq!(spool.pending_count().unwrap(), 2);
    }

    #[test]
    fn v1_backfill_does_not_coalesce_old_weak_unsigned_rows() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let first = historical_event(1, raw.clone(), false);
        let second = historical_event(2, raw.clone(), false);
        historical_spool(&scratch.db(), 1, &[first.clone(), second.clone()]);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        let rows = historical_rows(&scratch.db());
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].4.as_deref(), Some(first.dedup_key.as_str()));
        assert_eq!(rows[1].4.as_deref(), Some(second.dedup_key.as_str()));
        assert_eq!(rows[0].5, 1);
        assert_eq!(rows[1].5, 1);
        assert!(spool.ingest(raw).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), rows);
        assert_eq!(spool.pending_count().unwrap(), 2);
    }

    #[test]
    fn historical_spool_rejects_a_different_account() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let old = historical_event(1, raw, false);
        historical_spool(&scratch.db(), 2, &[old]);
        let before = historical_rows(&scratch.db());
        assert!(IngressSpool::open(scratch.db(), 10).is_err());
        assert_eq!(historical_rows(&scratch.db()), before);
    }

    #[test]
    fn legacy_writer_inserts_are_kept_in_the_additive_index() {
        let scratch = Scratch::new();
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let old = historical_event(1, raw.clone(), false);
        // Write the same v2 row/cursor/observation shapes through a second
        // SQLite connection, after the new handle built its index.
        let mut conn = Connection::open(scratch.db()).unwrap();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).unwrap();
        tx.execute("INSERT INTO ingress_events (seq,spool_id,dedup_key,consumed,body,source_key,eligible) \
            VALUES (?1,?2,?3,?4,?5,?3,1)", params![1, old.spool_id.to_string(), old.dedup_key,
                old.consumed, serde_json::to_string(&old).unwrap()]).unwrap();
        let original = IngressObservation { event_id: old.spool_id, kind: ObservationKind::Original, raw: old.raw.clone() };
        tx.execute("INSERT INTO ingress_observations VALUES (?1,?2,?3)", params![old.dedup_key,
            blake3::hash(&serde_json::to_vec(&old.raw).unwrap()).to_string(), serde_json::to_string(&original).unwrap()]).unwrap();
        let mut cursor = read_cursor(&tx).unwrap();
        cursor.advance(1);
        save_cursor(&tx, &cursor).unwrap();
        tx.commit().unwrap();
        drop(conn);
        let before = historical_rows(&scratch.db());
        assert!(spool.ingest(raw).unwrap().is_none());
        assert_eq!(historical_rows(&scratch.db()), before);
        assert_eq!(spool.cursor().unwrap().last_seq, 1);
        assert_eq!(spool.consume(10).unwrap()[0].spool_id, old.spool_id);
    }

    #[test]
    fn published_v3_database_is_rejected_without_reinterpretation() {
        let scratch = Scratch::new();
        let mut raw = message();
        raw["message_id"] = json!(u64::MAX);
        let old = historical_event(1, raw, false);
        historical_spool(&scratch.db(), 2, &[old.clone()]);
        let conn = Connection::open(scratch.db()).unwrap();
        conn.execute_batch("
            ALTER TABLE ingress_observations ADD COLUMN canonical_source_key TEXT;
            UPDATE ingress_observations SET canonical_source_key=source_key;
            CREATE TABLE ingress_identity_reconciliation (spool_id TEXT PRIMARY KEY REFERENCES ingress_events(spool_id),
                source_key TEXT NOT NULL,body TEXT NOT NULL);
            CREATE TABLE ingress_reconciliation_evidence (source_key TEXT NOT NULL,evidence_hash TEXT NOT NULL,
                body TEXT NOT NULL,PRIMARY KEY(source_key,evidence_hash));
            CREATE TABLE ingress_reconciliation_releases (release_seq INTEGER PRIMARY KEY AUTOINCREMENT,
                spool_id TEXT NOT NULL REFERENCES ingress_events(spool_id),source_key TEXT NOT NULL,
                agent_id TEXT NOT NULL,control_ledger_id TEXT NOT NULL);
        ").unwrap();
        conn.execute("INSERT INTO ingress_identity_reconciliation VALUES (?1,?2,?3)",
            params![old.spool_id.to_string(), old.dedup_key, "legacy reconciliation"]).unwrap();
        let agent = EntityId::new();
        let ledger = EntityId::new();
        let evidence = serde_json::to_string(&json!({
            "kind":"committed_disposition", "event_id":old.spool_id, "agent_id":agent,
            "commit":{"ledger_id":ledger,"seq":1}
        })).unwrap();
        conn.execute("INSERT INTO ingress_reconciliation_evidence VALUES (?1,?2,?3)",
            params![old.dedup_key, blake3::hash(evidence.as_bytes()).to_string(), evidence]).unwrap();
        conn.execute("INSERT INTO ingress_reconciliation_releases (spool_id,source_key,agent_id,control_ledger_id) \
            VALUES (?1,?2,?3,?4)", params![old.spool_id.to_string(), old.dedup_key, agent.to_string(), ledger.to_string()]).unwrap();
        conn.pragma_update(None, "user_version", 3).unwrap();
        let mode_before: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        let evidence_before: String = conn.query_row("SELECT body FROM ingress_reconciliation_evidence", [], |r| r.get(0)).unwrap();
        let release_before: (i64, String, String, String, String) = conn.query_row(
            "SELECT release_seq,spool_id,source_key,agent_id,control_ledger_id FROM ingress_reconciliation_releases", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).unwrap();
        drop(conn);
        let events_before = historical_rows(&scratch.db());
        let observations_before = historical_observations(&scratch.db());
        let error = IngressSpool::open(scratch.db(), 9).err().unwrap();
        assert!(error.to_string().contains("v1/v2"));
        let conn = Connection::open(scratch.db()).unwrap();
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(version, 3);
        assert_eq!(conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).unwrap(), mode_before);
        assert_eq!(conn.query_row("SELECT body FROM ingress_reconciliation_evidence", [], |r| r.get::<_, String>(0)).unwrap(), evidence_before);
        let release_after: (i64, String, String, String, String) = conn.query_row(
            "SELECT release_seq,spool_id,source_key,agent_id,control_ledger_id FROM ingress_reconciliation_releases", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        ).unwrap();
        assert_eq!(release_after, release_before);
        drop(conn);
        assert_eq!(historical_rows(&scratch.db()), events_before);
        assert_eq!(historical_observations(&scratch.db()), observations_before);
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
