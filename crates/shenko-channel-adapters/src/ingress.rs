//! Durable ingress ledger of one adapter account — SPEC §15.3, §35.6.
//!
//! Every adapter monitor writes the classified event here **before** it
//! advances the platform cursor, and the row stays replayable until the
//! subject's disposition commit acknowledges it. That ordering is the whole
//! point of this module: the provider cursor (a QQ gateway `s`, an iLink
//! `get_updates_buf`, a Feishu replay window) is the only thing the platform
//! remembers for us, so advancing it before the event is durable would turn a
//! crash into a permanently lost message.
//!
//! The ledger is per account and per channel. It never becomes a second
//! subject ledger: it holds the provider's own bytes plus the normalized
//! envelope, and the durable intent/operation/evidence stay in the Store.
//! Identities come from [`shenko_types::channel::envelope_identity_key`], the
//! same key the subject admission path derives its event id from, so a
//! redelivery meets the control ledger's disposition check instead of being
//! admitted twice.
//!
//! A monitor that cannot record an event returns the error to its run loop and
//! does **not** advance the cursor: reconnecting replays the same window. The
//! row is never fabricated and an unanswered event is never silently dropped.

use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use shenko_types::channel::{
    ChannelEnvelope, envelope_event_id, envelope_identity_key,
};
use shenko_types::ids::EntityId;

/// Marks the file as a Shenko adapter ingress ledger. A database that carries
/// another application id (or any unrelated table) is refused instead of being
/// reinterpreted as one.
const APPLICATION_ID: i64 = 0x53484b41; // "SHKA"

/// The ledger layout this reader understands. A newer file is refused with the
/// original database intact; there is no guessed downgrade parse.
const SCHEMA_VERSION: i64 = 1;

/// What one `record` call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// A new event was durably appended under this identity.
    Appended { seq: u64, event_id: EntityId },
    /// This exact external event is already in the ledger. The caller must
    /// treat it as delivered-for-now, not as a second message.
    Duplicate { seq: u64, event_id: EntityId },
}

impl Recorded {
    /// The durable event identity, whether appended or already present.
    pub fn event_id(&self) -> EntityId {
        match self {
            Self::Appended { event_id, .. } | Self::Duplicate { event_id, .. } => *event_id,
        }
    }

    /// The ledger sequence, whether appended or already present.
    pub fn seq(&self) -> u64 {
        match self {
            Self::Appended { seq, .. } | Self::Duplicate { seq, .. } => *seq,
        }
    }

    /// Whether this call created the row.
    pub fn is_new(&self) -> bool {
        matches!(self, Self::Appended { .. })
    }
}

/// One durable adapter event: the provider's raw frame plus the classified
/// envelope, exactly as the monitor saw them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredAdapterEvent {
    /// Ledger sequence (append order, not a platform cursor).
    pub seq: u64,
    /// The replay-stable identity derived from the envelope key.
    pub event_id: EntityId,
    /// The `InboxEvent.source` label this event arrives on.
    pub source: String,
    /// The classified envelope the subject will see.
    pub envelope: ChannelEnvelope,
    /// The provider's own payload, kept as evidence.
    pub raw: Value,
    /// When this process recorded the event.
    pub received_at: DateTime<Utc>,
    /// When it was handed to subject admission. Not an acknowledgement.
    pub delivered_at: Option<DateTime<Utc>>,
    /// The subject disposition commit that acknowledged it.
    pub disposition_ref: Option<String>,
}

struct Backend {
    connection: Connection,
    failed: bool,
}

/// The durable ingress ledger of one adapter account.
pub struct AdapterIngress {
    backend: Mutex<Backend>,
    channel: String,
    account: String,
    persistent: bool,
    path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for AdapterIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdapterIngress")
            .field("channel", &self.channel)
            .field("account", &self.account)
            .field("persistent", &self.persistent)
            .field("path", &self.path)
            .finish()
    }
}

impl AdapterIngress {
    /// Open (or create) the ledger of one account as a real local file.
    ///
    /// Tests and non-canonical embedders use [`Self::in_memory`]; the runtime
    /// always opens a file inside its own data directory.
    pub fn open(
        path: impl AsRef<Path>,
        channel: &str,
        account: &str,
    ) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err("adapter ingress ledger needs a local file path".to_string());
        }
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .map_err(|error| format!("open adapter ingress ledger failed: {error}"))?;
        let mut ledger = Self::initialize(connection, channel, account, true)?;
        ledger.path = Some(path);
        Ok(ledger)
    }

    /// A ledger that lives only in memory. Reception is then not durable, so
    /// only tests and throwaway embedders may use it.
    pub fn in_memory(channel: &str, account: &str) -> Result<Self, String> {
        Self::initialize(Connection::open_in_memory().map_err(sql)?, channel, account, false)
    }

    fn initialize(
        connection: Connection,
        channel: &str,
        account: &str,
        persistent: bool,
    ) -> Result<Self, String> {
        if channel.is_empty() || account.is_empty() {
            return Err("adapter ingress ledger needs a channel and an account".to_string());
        }
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(sql)?;
        let app: i64 = connection
            .pragma_query_value(None, "application_id", |row| row.get(0))
            .map_err(sql)?;
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        if !(app == APPLICATION_ID || (app == 0 && tables == 0)) {
            return Err("refusing to reuse a non-adapter database as an ingress ledger".to_string());
        }
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql)?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(format!(
                "adapter ingress ledger schema v{version} is newer than this reader (v{SCHEMA_VERSION}); \
                 the original file is left untouched"
            ));
        }
        if persistent {
            let mode: String = connection
                .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
                .map_err(sql)?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err("adapter ingress ledger could not enable WAL".to_string());
            }
        }
        connection
            .pragma_update(None, "synchronous", "FULL")
            .map_err(sql)?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(sql)?;

        let tx = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql)?;
        let version: i64 = tx
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql)?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(format!(
                "adapter ingress ledger schema changed under us (v{version}); the original file is left untouched"
            ));
        }
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS adapter_ingress (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT NOT NULL UNIQUE,
                 identity TEXT NOT NULL UNIQUE,
                 source TEXT NOT NULL,
                 body TEXT NOT NULL,
                 received_at TEXT NOT NULL,
                 delivered_at TEXT,
                 disposition_ref TEXT
             );
             CREATE INDEX IF NOT EXISTS adapter_ingress_unacked
                 ON adapter_ingress(disposition_ref, seq);
             CREATE TABLE IF NOT EXISTS adapter_ingress_owner (
                 singleton INTEGER PRIMARY KEY CHECK(singleton=1),
                 channel TEXT NOT NULL,
                 account TEXT NOT NULL
             );",
        )
        .map_err(sql)?;
        let owner: Option<(String, String)> = tx
            .query_row(
                "SELECT channel, account FROM adapter_ingress_owner WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sql)?;
        match owner {
            Some((stored_channel, stored_account)) => {
                if stored_channel != channel || stored_account != account {
                    return Err(format!(
                        "adapter ingress ledger belongs to {stored_channel}/{stored_account}, not {channel}/{account}"
                    ));
                }
            }
            None => {
                tx.execute(
                    "INSERT INTO adapter_ingress_owner VALUES (1, ?1, ?2)",
                    params![channel, account],
                )
                .map_err(sql)?;
            }
        }
        tx.pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(sql)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(sql)?;
        tx.commit().map_err(sql)?;
        Ok(Self {
            backend: Mutex::new(Backend {
                connection,
                failed: false,
            }),
            channel: channel.to_string(),
            account: account.to_string(),
            persistent,
            path: None,
        })
    }

    /// The channel id this ledger belongs to.
    pub fn channel(&self) -> &str {
        &self.channel
    }

    /// The account scope this ledger belongs to.
    pub fn account(&self) -> &str {
        &self.account
    }

    /// Whether the ledger is a real file (durable) rather than in memory.
    pub fn is_persistent(&self) -> bool {
        self.persistent
    }

    /// The file this ledger was opened from, when it is persistent.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Run one write in an immediate transaction. After an ambiguous SQLite
    /// failure the handle is closed: only a reopen may read back the outcome.
    fn mutate<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut backend = self.backend.lock();
        if backend.failed {
            return Err(
                "adapter ingress ledger handle is invalid; reopen to read back the outcome"
                    .to_string(),
            );
        }
        let output = (|| {
            let tx = backend
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(sql)?;
            let value = operation(&tx)?;
            tx.commit()
                .map_err(|error| format!("adapter ingress commit outcome unknown: {error}"))?;
            Ok(value)
        })();
        if output.is_err() {
            backend.failed = true;
        }
        output
    }

    /// Durably record one classified event.
    ///
    /// The monitor calls this **before** it advances its provider cursor. An
    /// envelope without a platform message id has no replay-stable identity and
    /// is refused: the caller then replays instead of inventing an id.
    pub fn record(
        &self,
        source: &str,
        envelope: &ChannelEnvelope,
        raw: &Value,
    ) -> Result<Recorded, String> {
        if source.is_empty() {
            return Err("adapter ingress event needs a source label".to_string());
        }
        if envelope.provider != self.channel {
            return Err(format!(
                "adapter ingress event provider {} does not belong to ledger {}",
                envelope.provider, self.channel
            ));
        }
        if envelope.account != self.account {
            return Err(format!(
                "adapter ingress event account {} does not belong to ledger {}",
                envelope.account, self.account
            ));
        }
        if envelope.message_id.trim().is_empty() {
            return Err(
                "adapter ingress event has no platform message id; it cannot be replayed by identity"
                    .to_string(),
            );
        }
        let identity = envelope_identity_key(source, envelope);
        let event_id = envelope_event_id(source, envelope);
        let body = serde_json::to_string(&StoredAdapterEvent {
            seq: 0,
            event_id,
            source: source.to_string(),
            envelope: envelope.clone(),
            raw: raw.clone(),
            received_at: Utc::now(),
            delivered_at: None,
            disposition_ref: None,
        })
        .map_err(codec)?;
        self.mutate(|tx| {
            let inserted = tx
                .execute(
                    "INSERT OR IGNORE INTO adapter_ingress
                         (event_id, identity, source, body, received_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        event_id.to_string(),
                        identity,
                        source,
                        body,
                        Utc::now().to_rfc3339(),
                    ],
                )
                .map_err(sql)?;
            let seq: u64 = tx
                .query_row(
                    "SELECT seq FROM adapter_ingress WHERE event_id=?1",
                    [event_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(sql)?
                .try_into()
                .map_err(|error| format!("adapter ingress seq out of range: {error}"))?;
            Ok(if inserted == 1 {
                Recorded::Appended { seq, event_id }
            } else {
                Recorded::Duplicate { seq, event_id }
            })
        })
    }

    /// One page of events whose disposition is not committed yet.
    ///
    /// `after` is a scan watermark owned by the caller, not an
    /// acknowledgement; resetting it to zero simply re-reads the same page.
    pub fn unacked(&self, after: u64, max: usize) -> Result<Vec<StoredAdapterEvent>, String> {
        let connection = self.backend.lock();
        if connection.failed {
            return Err(
                "adapter ingress ledger handle is invalid; reopen to read back the outcome"
                    .to_string(),
            );
        }
        let mut statement = connection
            .connection
            .prepare(
                "SELECT seq, body, delivered_at, disposition_ref FROM adapter_ingress
                 WHERE disposition_ref IS NULL AND seq > ?1 ORDER BY seq LIMIT ?2",
            )
            .map_err(sql)?;
        let rows = statement
            .query_map(params![i64::try_from(after).unwrap_or(i64::MAX), i64::try_from(max).unwrap_or(0)], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(sql)?;
        let mut events = Vec::new();
        for row in rows {
            let (seq, body, delivered_at, disposition_ref) = row.map_err(sql)?;
            let mut event: StoredAdapterEvent = serde_json::from_str(&body).map_err(codec)?;
            event.seq = seq
                .try_into()
                .map_err(|error| format!("adapter ingress seq out of range: {error}"))?;
            event.delivered_at = delivered_at.and_then(|text| {
                DateTime::parse_from_rfc3339(&text)
                    .ok()
                    .map(|at| at.with_timezone(&Utc))
            });
            event.disposition_ref = disposition_ref;
            events.push(event);
        }
        Ok(events)
    }

    /// Mark one event as handed to subject admission. Delivery is not an
    /// acknowledgement: the row stays until the disposition commit lands.
    pub fn mark_delivered(&self, event_id: EntityId) -> Result<(), String> {
        self.mutate(|tx| {
            tx.execute(
                "UPDATE adapter_ingress SET delivered_at=?2
                 WHERE event_id=?1 AND delivered_at IS NULL",
                params![event_id.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Acknowledge one event with the subject disposition commit that settled
    /// it. An unknown identity is an error, never a silent no-op.
    pub fn ack(&self, event_id: EntityId, disposition_ref: &str) -> Result<(), String> {
        if disposition_ref.is_empty() {
            return Err("adapter ingress ack needs a disposition reference".to_string());
        }
        self.mutate(|tx| {
            let updated = tx
                .execute(
                    "UPDATE adapter_ingress SET disposition_ref=?2
                     WHERE event_id=?1 AND disposition_ref IS NULL",
                    params![event_id.to_string(), disposition_ref],
                )
                .map_err(sql)?;
            if updated == 0 {
                let known: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM adapter_ingress WHERE event_id=?1",
                        [event_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                if known.is_none() {
                    return Err(format!("adapter ingress event {event_id} is not in this ledger"));
                }
            }
            Ok(())
        })
    }

    /// One event, by identity.
    pub fn event(&self, event_id: EntityId) -> Result<Option<StoredAdapterEvent>, String> {
        let connection = self.backend.lock();
        if connection.failed {
            return Err(
                "adapter ingress ledger handle is invalid; reopen to read back the outcome"
                    .to_string(),
            );
        }
        let body: Option<String> = connection
            .connection
            .query_row(
                "SELECT body FROM adapter_ingress WHERE event_id=?1",
                [event_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(sql)?;
        let Some(body) = body else {
            return Ok(None);
        };
        let event: StoredAdapterEvent = serde_json::from_str(&body).map_err(codec)?;
        Ok(Some(event))
    }

    /// How many events are still waiting for a disposition commit.
    pub fn pending_count(&self) -> Result<u64, String> {
        let connection = self.backend.lock();
        if connection.failed {
            return Err(
                "adapter ingress ledger handle is invalid; reopen to read back the outcome"
                    .to_string(),
            );
        }
        let count: i64 = connection
            .connection
            .query_row(
                "SELECT count(*) FROM adapter_ingress WHERE disposition_ref IS NULL",
                [],
                |row| row.get(0),
            )
            .map_err(sql)?;
        count
            .try_into()
            .map_err(|error| format!("adapter ingress count out of range: {error}"))
    }

    /// The highest ledger sequence, or zero when empty.
    pub fn last_seq(&self) -> Result<u64, String> {
        let connection = self.backend.lock();
        if connection.failed {
            return Err(
                "adapter ingress ledger handle is invalid; reopen to read back the outcome"
                    .to_string(),
            );
        }
        let seq: Option<i64> = connection
            .connection
            .query_row("SELECT max(seq) FROM adapter_ingress", [], |row| row.get(0))
            .map_err(sql)?;
        Ok(seq.unwrap_or(0).max(0) as u64)
    }
}

/// One ledger shared by a monitor and the runtime's delivery loop.
pub type SharedAdapterIngress = Arc<AdapterIngress>;

fn sql(error: rusqlite::Error) -> String {
    format!("adapter ingress sqlite error: {error}")
}

fn codec(error: serde_json::Error) -> String {
    format!("adapter ingress envelope encoding error: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shenko_types::channel::{ChannelActor, ChannelContent, ChannelReplyTarget};

    fn envelope(channel: &str, account: &str, message_id: &str) -> ChannelEnvelope {
        ChannelEnvelope {
            schema_version: 1,
            provider: channel.to_string(),
            account: account.to_string(),
            tenant: None,
            transport_identity_domain: "test".to_string(),
            conversation: Some("chat-1".to_string()),
            thread: None,
            message_id: message_id.to_string(),
            source_event_revision: None,
            occurred_at: None,
            received_at: Utc::now(),
            actor: ChannelActor {
                platform_id: "open-1".to_string(),
                display_name: Some("甲".to_string()),
                alias: None,
                person_ref: None,
                role_grant_ref: None,
                group_role: None,
            },
            content: ChannelContent {
                text: "在吗".to_string(),
                segments: vec![],
                mentions: vec![],
                reply_to: None,
            },
            reply_target: Some(ChannelReplyTarget {
                kind: "chat".to_string(),
                id: "chat-1".to_string(),
                label: "会话 chat-1".to_string(),
                account: account.to_string(),
                tenant: None,
                conversation: Some("chat-1".to_string()),
            }),
            attachments: vec![],
            raw_ref: None,
        }
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shenko-adapter-ingress-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A platform redelivery (same account, same message id, same revision)
    /// must not become a second subject message, and the identity must be the
    /// one the runtime's admission path derives.
    #[test]
    fn redelivery_is_one_event_with_the_shared_identity() {
        let dir = scratch();
        let path = dir.join("ingress.sqlite");
        let ledger = AdapterIngress::open(&path, "feishu", "feishu:app").unwrap();
        let raw = json!({"header":{"event_id":"e-1"},"event":{"message":{"message_id":"om_1"}}});
        let envelope = envelope("feishu", "feishu:app", "om_1");
        let first = ledger.record("feishu", &envelope, &raw).unwrap();
        assert!(first.is_new());
        assert_eq!(
            first.event_id(),
            shenko_types::channel::envelope_event_id("feishu", &envelope)
        );
        let second = ledger.record("feishu", &envelope, &raw).unwrap();
        assert!(!second.is_new());
        assert_eq!(second.seq(), first.seq());
        assert_eq!(ledger.pending_count().unwrap(), 1);
        // The bytes survive a reopen: the ledger, not the process, holds them.
        drop(ledger);
        let reopened = AdapterIngress::open(&path, "feishu", "feishu:app").unwrap();
        let stored = reopened.event(first.event_id()).unwrap().unwrap();
        assert_eq!(stored.raw, raw);
        assert_eq!(stored.envelope.message_id, "om_1");
        assert_eq!(reopened.unacked(0, 16).unwrap().len(), 1);
    }

    /// A distinct declared revision of the same platform message is a second
    /// real event, not a duplicate.
    #[test]
    fn a_declared_revision_is_a_second_event() {
        let ledger = AdapterIngress::in_memory("feishu", "feishu:app").unwrap();
        let raw = json!({});
        let original = envelope("feishu", "feishu:app", "om_1");
        let mut revised = original.clone();
        revised.source_event_revision = Some("v2".to_string());
        assert!(ledger.record("feishu", &original, &raw).unwrap().is_new());
        assert!(ledger.record("feishu", &revised, &raw).unwrap().is_new());
        assert_eq!(ledger.pending_count().unwrap(), 2);
    }

    /// Delivery is not acknowledgement: the row leaves the pending set only
    /// when the subject disposition commit is recorded, and an unknown id is an
    /// error rather than a silent success.
    #[test]
    fn ack_follows_the_disposition_commit() {
        let ledger = AdapterIngress::in_memory("ilink", "ilink:bot").unwrap();
        let raw = json!({"message_id":"m-1"});
        let envelope = envelope("ilink", "ilink:bot", "m-1");
        let recorded = ledger.record("ilink", &envelope, &raw).unwrap();
        ledger.mark_delivered(recorded.event_id()).unwrap();
        assert_eq!(ledger.pending_count().unwrap(), 1);
        assert!(ledger.ack(EntityId::new(), "disposition").is_err());
        ledger.ack(recorded.event_id(), "disposition").unwrap();
        assert_eq!(ledger.pending_count().unwrap(), 0);
        assert!(ledger.unacked(0, 16).unwrap().is_empty());
        let stored = ledger.event(recorded.event_id()).unwrap().unwrap();
        assert!(stored.delivered_at.is_some());
        assert_eq!(stored.disposition_ref.as_deref(), Some("disposition"));
    }

    /// An event with no platform message id has no replay-stable identity, so
    /// it is refused instead of being stored under an invented one.
    #[test]
    fn an_event_without_platform_identity_is_refused() {
        let ledger = AdapterIngress::in_memory("qq-official", "qq:app").unwrap();
        let envelope = envelope("qq-official", "qq:app", "");
        assert!(ledger.record("qqofficial", &envelope, &json!({})).is_err());
        assert_eq!(ledger.pending_count().unwrap(), 0);
        // A different account or provider cannot be smuggled into this ledger.
        assert!(
            ledger
                .record("qqofficial", &envelope("qq-official", "qq:other", "m"), &json!({}))
                .is_err()
        );
    }

    /// A database that belongs to something else, or was written by a newer
    /// reader, is refused with the original file left as it was.
    #[test]
    fn foreign_and_newer_ledgers_are_refused() {
        let dir = scratch();
        let foreign = dir.join("foreign.sqlite");
        {
            let connection = Connection::open(&foreign).unwrap();
            connection
                .execute_batch("CREATE TABLE other(one TEXT);")
                .unwrap();
        }
        assert!(AdapterIngress::open(&foreign, "feishu", "feishu:app").is_err());

        let newer = dir.join("newer.sqlite");
        {
            let connection = Connection::open(&newer).unwrap();
            connection.pragma_update(None, "application_id", APPLICATION_ID).unwrap();
            connection.pragma_update(None, "user_version", SCHEMA_VERSION + 1).unwrap();
        }
        let error = AdapterIngress::open(&newer, "feishu", "feishu:app").unwrap_err();
        assert!(error.contains("newer than this reader"), "{error}");
        // The refused file keeps its version: nothing was rewritten.
        let connection = Connection::open(&newer).unwrap();
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION + 1);
        // The same file cannot be claimed by another account afterwards.
        let ok = dir.join("ok.sqlite");
        drop(AdapterIngress::open(&ok, "feishu", "feishu:app").unwrap());
        assert!(AdapterIngress::open(&ok, "feishu", "feishu:other").is_err());
    }
}
