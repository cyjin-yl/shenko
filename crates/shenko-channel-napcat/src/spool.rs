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
use shenko_types::ids::EntityId;

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
        if persistent {
            let mode: String = connection.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
            ensure!(mode.eq_ignore_ascii_case("wal"), "接收库无法启用 WAL");
        }
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let sync: i64 = connection.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        let fk: i64 = connection.pragma_query_value(None, "foreign_keys", |r| r.get(0))?;
        ensure!(sync == 2 && fk == 1, "接收库耐久配置读回失败");
        let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
        ensure!(version <= 1, "接收库版本较新，不能用旧 reader 写入");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        tx.pragma_update(None, "application_id", APPLICATION_ID)?;
        tx.pragma_update(None, "user_version", 1)?;
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
            let spool_id = EntityId::new();
            let (dedup_key, weak_dedup) = dedup_key(cursor.account, &raw, spool_id)?;
            let duplicate: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM ingress_events WHERE dedup_key=?1)",
                [&dedup_key],
                |r| r.get(0),
            )?;
            if duplicate {
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
                "INSERT INTO ingress_events (spool_id,dedup_key,body) VALUES (?1,?2,?3)",
                params![
                    event.spool_id.to_string(),
                    event.dedup_key,
                    serde_json::to_string(&event)?
                ],
            )?;
            event.seq = u64::try_from(tx.last_insert_rowid())?;
            save_event(tx, &event)?;
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
            "SELECT body FROM ingress_events WHERE consumed=0 AND seq>?1 ORDER BY seq LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![i64::try_from(after)?, i64::try_from(max)?], |r| {
            r.get::<_, String>(0)
        })?;
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
            "SELECT count(*) FROM ingress_events WHERE consumed=0",
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

fn dedup_key(account: u64, raw: &serde_json::Value, id: EntityId) -> Result<(String, bool)> {
    // 只有消息事件的非空平台 message_id 被视为稳定身份；time 不是 ID。
    let post_type = raw["post_type"].as_str();
    let message_id = raw.get("message_id").filter(|v| match v {
        serde_json::Value::Number(n) => n.as_i64().is_some_and(|n| n != 0),
        serde_json::Value::String(s) => !s.is_empty() && s != "0",
        _ => false,
    });
    let conversation = match raw["message_type"].as_str() {
        Some("group") => raw.get("group_id").map(|v| serde_json::json!(["group", v])),
        Some("private") => raw
            .get("user_id")
            .map(|v| serde_json::json!(["private", v, raw.get("group_id")])),
        _ => None,
    };
    if matches!(post_type, Some("message" | "message_sent"))
        && message_id.is_some()
        && conversation.is_some()
    {
        let identity = serde_json::json!([
            "napcat",
            account.to_string(),
            "onebot_v11",
            conversation,
            post_type,
            raw.get("sub_type"),
            message_id,
            raw.get("message_revision")
        ]);
        // 同一身份不同原文保留为独立冲突/修订证据，不覆盖；相同重投去重。
        let digest = blake3::hash(&serde_json::to_vec(raw)?);
        Ok((format!("{identity}:{digest}"), false))
    } else {
        // 弱去重事件仅拥有本接收记录的稳定身份，不冒称跨重连强去重。
        Ok((format!("napcat:{account}:onebot_v11:spool:{id}"), true))
    }
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
        let (client, mut rx, actions) =
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
        assert_eq!(rx.try_recv().unwrap().ingress_id, Some(event.spool_id));
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
        assert_eq!(gaps[3].status, GapStatus::Open);
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
        let revision = spool.ingest(edit).unwrap().unwrap();
        let notice = json!({"post_type":"notice", "time":1, "user_id":7});
        let first = spool.ingest(notice.clone()).unwrap().unwrap();
        let second = spool.ingest(notice).unwrap().unwrap();
        assert_ne!(group.dedup_key, private.dedup_key);
        assert_ne!(group.dedup_key, revision.dedup_key);
        assert!(first.weak_dedup && second.weak_dedup);
        assert_ne!(first.spool_id, second.spool_id);
        drop(spool);
        let spool = IngressSpool::open(scratch.db(), 9).unwrap();
        assert_eq!(spool.pending_count().unwrap(), 5);
        assert_eq!(
            spool.event(first.spool_id).unwrap().dedup_key,
            first.dedup_key
        );
        assert_eq!(spool.ingest(message()).unwrap().map(|e| e.spool_id), None);
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
