//! NapCat persistent ingress spool -- SPEC §35.6
//! Durable cursor, dedup, gap recording.
use std::collections::HashSet;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use shenko_types::ids::EntityId;
use chrono::{DateTime, Utc};

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
        Self { cursor_id: EntityId::new(), provider: provider.to_string(), account, last_seq: 0, gap_count: 0, updated_at: Utc::now() }
    }
    pub fn advance(&mut self, seq: u64) { if seq > self.last_seq { self.last_seq = seq; self.updated_at = Utc::now(); } }
    pub fn record_gap(&mut self) { self.gap_count += 1; self.updated_at = Utc::now(); }
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
pub enum GapStatus { Open, Reconciling, FilledPartially, Acknowledged }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpooledEvent {
    pub spool_id: EntityId,
    pub dedup_key: String,
    pub seq: u64,
    pub received_at: DateTime<Utc>,
    pub consumed: bool,
    pub raw: serde_json::Value,
}

pub struct IngressSpool {
    cursor: Mutex<IngressCursor>,
    gaps: Mutex<Vec<IngressGap>>,
    events: Mutex<std::collections::VecDeque<SpooledEvent>>,
    seen_keys: Mutex<HashSet<String>>,
    next_seq: std::sync::atomic::AtomicU64,
}

impl IngressSpool {
    pub fn new(account: u64) -> Self {
        Self {
            cursor: Mutex::new(IngressCursor::new("napcat", account)),
            gaps: Mutex::new(vec![]),
            events: Mutex::new(std::collections::VecDeque::new()),
            seen_keys: Mutex::new(HashSet::new()),
            next_seq: std::sync::atomic::AtomicU64::new(1),
        }
    }

    pub fn ingest(&self, raw: serde_json::Value) -> Option<SpooledEvent> {
        let account = self.cursor.lock().account;
        let conversation = raw.get("group_id").or_else(|| raw.get("user_id"))
            .and_then(|v| v.as_u64()).unwrap_or(0);
        let message_id = raw.get("message_id").and_then(|v| v.as_i64())
            .unwrap_or_else(|| raw.get("time").and_then(|v| v.as_i64()).unwrap_or(0));
        let dedup_key = format!("napcat:{}:{}:{}", account, conversation, message_id);

        let mut seen = self.seen_keys.lock();
        if seen.contains(&dedup_key) {
            debug!(dedup_key=%dedup_key, "duplicate event dropped");
            return None;
        }
        seen.insert(dedup_key.clone());
        drop(seen);

        let seq = self.next_seq.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let event = SpooledEvent { spool_id: EntityId::new(), dedup_key, seq, received_at: Utc::now(), consumed: false, raw };
        self.events.lock().push_back(event.clone());
        self.cursor.lock().advance(seq);
        Some(event)
    }

    pub fn consume(&self, max: usize) -> Vec<SpooledEvent> {
        let mut events = self.events.lock();
        let mut result = vec![];
        while result.len() < max {
            match events.pop_front() {
                Some(mut ev) => { ev.consumed = true; result.push(ev); }
                None => break,
            }
        }
        result
    }

    pub fn record_disconnect(&self) {
        let account = self.cursor.lock().account;
        let mut gaps = self.gaps.lock();
        for g in gaps.iter_mut() {
            if g.status == GapStatus::Open { g.status = GapStatus::Acknowledged; g.reconnected_at = Some(Utc::now()); }
        }
        gaps.push(IngressGap { gap_id: EntityId::new(), provider: "napcat".to_string(), account, disconnected_at: Utc::now(), reconnected_at: None, status: GapStatus::Open });
        self.cursor.lock().record_gap();
        warn!(account, "disconnect gap recorded");
    }

    pub fn record_reconnect(&self) {
        for g in self.gaps.lock().iter_mut() {
            if g.status == GapStatus::Open { g.reconnected_at = Some(Utc::now()); g.status = GapStatus::Reconciling; }
        }
    }

    pub fn cursor(&self) -> IngressCursor { self.cursor.lock().clone() }
    pub fn pending_count(&self) -> usize { self.events.lock().len() }
    pub fn gap_count(&self) -> u64 { self.cursor.lock().gap_count }
}
