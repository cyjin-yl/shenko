//! Durable contact / person directory — SPEC §15.2, §15.2.1, §16.
//!
//! Three append-only tables keep identity facts, merge/split history and
//! owner-set admin facts. Nothing here rewrites an admitted row:
//!
//! * `person_claims` — every claim about a person as an append-only
//!   revision chain. One fact slot holds all revisions; the highest is
//!   current. Replaying the same fact returns the original revision;
//!   changing it appends a new one, so alias changes, relationship revisions
//!   and rejected claims stay auditable.
//! * `person_identity_events` — created/merged/split history. The current
//!   owner of a claim is computed by replaying these events over the
//!   original attribution, never by rewriting attribution (SPEC §15.2:
//!   "错误合并可拆分，历史来源不被改写").
//! * `admin_facts` — an explicit human-set fact that is never derived from
//!   familiarity, group status, platform roles or voiceprint matches. Its
//!   absence is not a chat admission condition (§16: ordinary users can
//!   talk). Only the owner's local path writes it.
//!
//! The Life voiceprint registry lives in `life_voiceprint_ranks` keyed by
//! `(agent, recording_scope, rank_id, model_revision)`: rank and cluster
//! numbers are recording-local and every registry revision is kept (§15.2.1).
//!
//! Conflict unit for the cognitive archive: for one person, the person's
//! profile claim, every claim attributed to them, every identity event that
//! mentions them and the admin facts referencing them; for one platform
//! account, the account claim and its admin fact.
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use shenko_types::commit::CommitRef;
use shenko_types::contacts::*;
use shenko_types::error::{ErrorScope, ProtocolError, RetryStrategy, ShenkoError};
use shenko_types::ids::{EntityId, LedgerSeq};

use crate::store::Store;

/// Additive durable tables; never rewrites already committed rows.
pub const CONTACTS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS person_claims (
    claim_id TEXT PRIMARY KEY,
    slot_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    person_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    claim_key TEXT NOT NULL,
    revision INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    provenance_json TEXT NOT NULL,
    confidence_json TEXT NOT NULL,
    state TEXT NOT NULL,
    scope_json TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    UNIQUE(slot_id, revision)
);
CREATE INDEX IF NOT EXISTS idx_person_claims_person
    ON person_claims(agent_id, person_id);
CREATE INDEX IF NOT EXISTS idx_person_claims_slot
    ON person_claims(agent_id, slot_id);
CREATE INDEX IF NOT EXISTS idx_person_claims_kind_key
    ON person_claims(agent_id, kind, claim_key);

CREATE TABLE IF NOT EXISTS person_identity_events (
    event_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    source_person_id TEXT NOT NULL,
    target_person_id TEXT NOT NULL,
    moved_slot_ids_json TEXT NOT NULL,
    returned_slot_ids_json TEXT NOT NULL,
    reason TEXT,
    provenance_json TEXT NOT NULL,
    occurred_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_person_identity_events_agent
    ON person_identity_events(agent_id, occurred_at_ms, commit_seq);
CREATE INDEX IF NOT EXISTS idx_person_identity_events_person
    ON person_identity_events(agent_id, source_person_id, target_person_id);

CREATE TABLE IF NOT EXISTS admin_facts (
    agent_id TEXT NOT NULL,
    subject_kind TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    granted INTEGER NOT NULL,
    granted_by TEXT NOT NULL,
    reason TEXT,
    updated_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (agent_id, subject_kind, subject_key)
);

CREATE TABLE IF NOT EXISTS life_voiceprint_ranks (
    agent_id TEXT NOT NULL,
    recording_scope TEXT NOT NULL,
    rank_id TEXT NOT NULL,
    model_revision TEXT NOT NULL,
    cluster_id TEXT,
    confirmed_identity TEXT,
    data TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    commit_seq INTEGER NOT NULL,
    PRIMARY KEY (agent_id, recording_scope, rank_id, model_revision)
);
CREATE INDEX IF NOT EXISTS idx_life_ranks_rank
    ON life_voiceprint_ranks(agent_id, rank_id, model_revision);
"#;

fn contacts_error(code: &str, message: impl Into<String>) -> ShenkoError {
    ShenkoError::Protocol(ProtocolError {
        code: code.to_owned(),
        scope: ErrorScope::Inbox,
        retry: RetryStrategy::NeverAutomatically,
        operation_id: None,
        commit_ref: None,
        details: serde_json::json!({ "detail": message.into() }),
    })
}

fn invalid(message: impl Into<String>) -> ShenkoError {
    contacts_error("contacts_invalid_input", message)
}

fn db(error: rusqlite::Error) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

fn encode<T: serde::Serialize>(value: &T) -> Result<String, ShenkoError> {
    serde_json::to_string(value).map_err(ShenkoError::Serde)
}

fn decode<T: serde::de::DeserializeOwned>(json: &str, what: &str) -> Result<T, ShenkoError> {
    serde_json::from_str(json).map_err(|e| {
        ShenkoError::storage(format!("contacts: corrupt stored {what}: {e}"))
    })
}

#[derive(Debug, Clone, serde::Serialize)]
struct ClaimRow {
    claim_id: String,
    slot_id: String,
    person_id: String,
    kind: String,
    claim_key: String,
    revision: i64,
    payload_json: String,
    provenance_json: String,
    confidence_json: String,
    state: String,
    scope_json: String,
    created_at_ms: i64,
    commit_seq: i64,
}

impl ClaimRow {
    fn to_claim(&self) -> Result<PersonClaim, ShenkoError> {
        Ok(PersonClaim {
            claim_id: self.claim_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt claim id")
            })?,
            slot_id: self.slot_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt slot id")
            })?,
            agent_id: EntityId::nil(),
            person_id: self.person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt person id")
            })?,
            kind: serde_json::from_value(serde_json::Value::String(self.kind.clone()))
                .map_err(|_| ShenkoError::storage("contacts: corrupt claim kind"))?,
            claim_key: self.claim_key.clone(),
            revision: self.revision as u64,
            payload: serde_json::from_str(&self.payload_json)
                .map_err(|e| ShenkoError::storage(format!("contacts: corrupt payload: {e}")))?,
            provenance: decode(&self.provenance_json, "provenance")?,
            confidence: decode(&self.confidence_json, "confidence")?,
            state: serde_json::from_value(serde_json::Value::String(self.state.clone()))
                .map_err(|_| ShenkoError::storage("contacts: corrupt claim state"))?,
            scope: decode(&self.scope_json, "scope")?,
            created_at_ms: self.created_at_ms,
            commit_seq: self.commit_seq as u64,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct IdentityEventRow {
    event_id: String,
    kind: String,
    source_person_id: String,
    target_person_id: String,
    moved_slot_ids_json: String,
    returned_slot_ids_json: String,
    reason: Option<String>,
    provenance_json: String,
    occurred_at_ms: i64,
    commit_seq: i64,
}

impl IdentityEventRow {
    fn to_event(&self) -> Result<PersonIdentityEvent, ShenkoError> {
        let moved: Vec<EntityId> = decode(&self.moved_slot_ids_json, "moved slot ids")?;
        let returned: Vec<EntityId> = decode(&self.returned_slot_ids_json, "returned slot ids")?;
        Ok(PersonIdentityEvent {
            event_id: self.event_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt event id")
            })?,
            agent_id: EntityId::nil(),
            kind: serde_json::from_value(serde_json::Value::String(self.kind.clone()))
                .map_err(|_| ShenkoError::storage("contacts: corrupt event kind"))?,
            source_person_id: self.source_person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt source person id")
            })?,
            target_person_id: self.target_person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt target person id")
            })?,
            moved_slot_ids: moved,
            returned_slot_ids: returned,
            reason: self.reason.clone(),
            provenance: decode(&self.provenance_json, "provenance")?,
            occurred_at_ms: self.occurred_at_ms,
            commit_seq: self.commit_seq as u64,
        })
    }
}

fn read_claim_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<ClaimRow>, ShenkoError> {
    let mut stmt = conn.prepare(sql).map_err(db)?;
    let rows = stmt
        .query_map(params, |row| {
            Ok(ClaimRow {
                claim_id: row.get(0)?,
                slot_id: row.get(1)?,
                person_id: row.get(2)?,
                kind: row.get(3)?,
                claim_key: row.get(4)?,
                revision: row.get(5)?,
                payload_json: row.get(6)?,
                provenance_json: row.get(7)?,
                confidence_json: row.get(8)?,
                state: row.get(9)?,
                scope_json: row.get(10)?,
                created_at_ms: row.get(11)?,
                commit_seq: row.get(12)?,
            })
        })
        .map_err(db)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(db)?);
    }
    Ok(out)
}

fn read_event_rows(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<IdentityEventRow>, ShenkoError> {
    let mut stmt = conn.prepare(sql).map_err(db)?;
    let rows = stmt
        .query_map(params, |row| {
            Ok(IdentityEventRow {
                event_id: row.get(0)?,
                kind: row.get(1)?,
                source_person_id: row.get(2)?,
                target_person_id: row.get(3)?,
                moved_slot_ids_json: row.get(4)?,
                returned_slot_ids_json: row.get(5)?,
                reason: row.get(6)?,
                provenance_json: row.get(7)?,
                occurred_at_ms: row.get(8)?,
                commit_seq: row.get(9)?,
            })
        })
        .map_err(db)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(db)?);
    }
    Ok(out)
}

impl Store {
    fn contacts_events_in(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
    ) -> Result<Vec<PersonIdentityEvent>, ShenkoError> {
        let rows = read_event_rows(
            conn,
            "SELECT event_id, kind, source_person_id, target_person_id, \
                    moved_slot_ids_json, returned_slot_ids_json, reason, \
                    provenance_json, occurred_at_ms, commit_seq \
               FROM person_identity_events \
              WHERE agent_id=?1 \
              ORDER BY occurred_at_ms, commit_seq, event_id",
            params![agent_id.to_string()],
        )?;
        rows.iter().map(IdentityEventRow::to_event).collect()
    }

    fn contacts_events(&self, agent_id: EntityId) -> Result<Vec<PersonIdentityEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        self.contacts_events_in(&conn, agent_id)
    }

    /// Current owner of one claim slot: replay identity events over the
    /// original attribution. Events are append-only, so a merge followed by
    /// a split restores the original owner for the returned slots.
    fn resolve_slot(
        &self,
        events: &[PersonIdentityEvent],
        original_person_id: PersonId,
        slot_id: EntityId,
    ) -> PersonId {
        let mut owner = original_person_id;
        for event in events {
            match event.kind {
                PersonIdentityEventKind::Merged if event.moved_slot_ids.contains(&slot_id) => {
                    owner = event.target_person_id;
                }
                PersonIdentityEventKind::Split if event.returned_slot_ids.contains(&slot_id) => {
                    owner = event.source_person_id;
                }
                _ => {}
            }
        }
        owner
    }

    /// Current owner of the person's profile: the canonical person id after
    /// merges/splits. A person with no profile claim is its own owner.
    pub fn resolve_person(
        &self,
        agent_id: EntityId,
        person_id: PersonId,
    ) -> Result<PersonId, ShenkoError> {
        let profile_slot = person_slot_id(agent_id, PersonClaimKind::Profile, person_id, "profile");
        let conn = self.ledger.conn.lock();
        let current = self.current_revision_of(&conn, agent_id, &profile_slot)?;
        match current {
            Some(row) => {
                let events = self.contacts_events_in(&conn, agent_id)?;
                let original = row.person_id.parse::<PersonId>().map_err(|_| {
                    ShenkoError::storage("contacts: corrupt person id")
                })?;
                Ok(self.resolve_slot(&events, original, profile_slot))
            }
            None => Ok(person_id),
        }
    }

    fn current_revision_of(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
        slot_id: &EntityId,
    ) -> Result<Option<ClaimRow>, ShenkoError> {
        let rows = read_claim_rows(
            conn,
            "SELECT claim_id, slot_id, person_id, kind, claim_key, revision, \
                    payload_json, provenance_json, confidence_json, state, \
                    scope_json, created_at_ms, commit_seq \
               FROM person_claims \
              WHERE agent_id=?1 AND slot_id=?2 \
              ORDER BY revision DESC LIMIT 1",
            params![agent_id.to_string(), slot_id.to_string()],
        )?;
        Ok(rows.into_iter().next())
    }

    /// Every claim slot's current revision owned by `person_id` after
    /// resolution. Rejected slots are excluded from views; their history
    /// stays readable through [`Store::person_slot_history`].
    pub fn person_claims(
        &self,
        agent_id: EntityId,
        person_id: PersonId,
    ) -> Result<Vec<PersonClaim>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let events = self.contacts_events_in(&conn, agent_id)?;
        let rows = read_claim_rows(
            &conn,
            "SELECT c.claim_id, c.slot_id, c.person_id, c.kind, c.claim_key, c.revision, \
                    c.payload_json, c.provenance_json, c.confidence_json, c.state, \
                    c.scope_json, c.created_at_ms, c.commit_seq \
               FROM person_claims c \
               JOIN (SELECT slot_id, MAX(revision) AS max_rev FROM person_claims \
                      WHERE agent_id=?1 GROUP BY slot_id) latest \
                 ON latest.slot_id = c.slot_id AND latest.max_rev = c.revision \
              WHERE c.agent_id=?1",
            params![agent_id.to_string()],
        )?;
        let mut claims = Vec::new();
        for row in &rows {
            let original: PersonId = row.person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt person id")
            })?;
            let slot_id: EntityId = row.slot_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt slot id")
            })?;
            if self.resolve_slot(&events, original, slot_id) != person_id {
                continue;
            }
            let mut claim = row.to_claim()?;
            claim.agent_id = agent_id;
            if claim.state == PersonClaimState::Rejected {
                continue;
            }
            claims.push(claim);
        }
        claims.sort_by(|a, b| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then(a.claim_key.cmp(&b.claim_key))
                .then(a.revision.cmp(&b.revision))
        });
        Ok(claims)
    }

    /// All revisions of one slot, oldest first. This is how alias changes and
    /// relationship revisions stay auditable after the fact.
    pub fn person_slot_history(
        &self,
        agent_id: EntityId,
        slot_id: EntityId,
    ) -> Result<Vec<PersonClaim>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let rows = read_claim_rows(
            &conn,
            "SELECT claim_id, slot_id, person_id, kind, claim_key, revision, \
                    payload_json, provenance_json, confidence_json, state, \
                    scope_json, created_at_ms, commit_seq \
               FROM person_claims \
              WHERE agent_id=?1 AND slot_id=?2 \
              ORDER BY revision",
            params![agent_id.to_string(), slot_id.to_string()],
        )?;
        let mut out = Vec::new();
        for row in rows {
            let mut claim = row.to_claim()?;
            claim.agent_id = agent_id;
            out.push(claim);
        }
        Ok(out)
    }

    /// Admit one claim revision.
    ///
    /// Idempotent on the full fact: resubmitting the identical revision
    /// returns the original receipt with `replayed = true`; any change
    /// appends a new revision of the same slot. `Profile` claims create the
    /// person; every other kind requires the person to exist.
    pub fn admit_person_claim(
        &self,
        agent_id: EntityId,
        input: &PersonClaimInput,
    ) -> Result<PersonClaimReceipt, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_admit_person_claim(agent_id, input),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_person_claim(
        &self,
        agent_id: EntityId,
        input: &PersonClaimInput,
    ) -> Result<PersonClaimReceipt, ShenkoError> {
        let person_id = input.person_id;
        let slot_id = input.slot_id.unwrap_or_else(|| {
            global_slot_id(agent_id, input.kind, &input.claim_key)
        });
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let seq = self.ledger.allocate_seq(&tx)?;
        let ledger_id = self.ledger.ledger_id();

        if input.kind != PersonClaimKind::Profile {
            let profile_slot =
                person_slot_id(agent_id, PersonClaimKind::Profile, person_id, "profile");
            let current = self.current_revision_of(&tx, agent_id, &profile_slot)?;
            let events = self.read_events_in(&tx, agent_id)?;
            match current {
                Some(row) => {
                    let original: PersonId = row.person_id.parse().map_err(|_| {
                        ShenkoError::storage("contacts: corrupt person id")
                    })?;
                    let owner = self.resolve_slot(&events, original, profile_slot);
                    if owner != person_id {
                        tx.rollback().map_err(db)?;
                        return Err(contacts_error(
                            "contacts_person_merged",
                            format!(
                                "person {person_id} was merged into {owner}; \
                                 record facts against the current person",
                            ),
                        ));
                    }
                }
                None => {
                    tx.rollback().map_err(db)?;
                    return Err(contacts_error(
                        "contacts_person_unknown",
                        format!("no person {person_id}; create a Profile claim first"),
                    ));
                }
            }
        } else if input.claim_key != "profile" {
            tx.rollback().map_err(db)?;
            return Err(invalid("a Profile claim must use claim_key 'profile'"));
        }

        let latest = self.current_revision_of(&tx, agent_id, &slot_id)?;
        let now = chrono::Utc::now().timestamp_millis();
        let revision = latest.as_ref().map(|row| row.revision as u64 + 1).unwrap_or(1);
        let same = latest.as_ref().is_some_and(|row| {
            row.person_id == person_id.to_string()
                && row.kind == input.kind.as_str()
                && row.claim_key == input.claim_key
                && row.payload_json
                    == serde_json::to_string(&input.payload).unwrap_or_default()
                && row.provenance_json
                    == serde_json::to_string(&input.provenance).unwrap_or_default()
                && row.confidence_json
                    == serde_json::to_string(&input.confidence).unwrap_or_default()
                && row.state == input.state.as_str()
                && row.scope_json
                    == serde_json::to_string(&input.scope).unwrap_or_default()
        });
        if same {
            let row = latest.expect("same implies a row");
            tx.rollback().map_err(db)?;
            let revision = row.revision as u64;
            return Ok(PersonClaimReceipt {
                claim_id: row.claim_id.parse().map_err(|_| {
                    ShenkoError::storage("contacts: corrupt claim id")
                })?,
                slot_id,
                person_id,
                revision,
                replayed: true,
                commit_ref: CommitRef::new(ledger_id, LedgerSeq(row.commit_seq as u64)),
            });
        }

        let claim_id = EntityId::new();
        tx.execute(
            "INSERT INTO person_claims \
                (claim_id, slot_id, agent_id, person_id, kind, claim_key, revision, \
                 payload_json, provenance_json, confidence_json, state, scope_json, \
                 created_at_ms, commit_seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                claim_id.to_string(),
                slot_id.to_string(),
                agent_id.to_string(),
                person_id.to_string(),
                input.kind.as_str(),
                input.claim_key,
                revision as i64,
                encode(&input.payload)?,
                encode(&input.provenance)?,
                encode(&input.confidence)?,
                input.state.as_str(),
                encode(&input.scope)?,
                now,
                seq.0 as i64,
            ],
        )
        .map_err(db)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(db)?;

        Ok(PersonClaimReceipt {
            claim_id,
            slot_id,
            person_id,
            revision,
            replayed: false,
            commit_ref: CommitRef::new(ledger_id, seq),
        })
    }

    fn read_events_in(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
    ) -> Result<Vec<PersonIdentityEvent>, ShenkoError> {
        let rows = read_event_rows(
            conn,
            "SELECT event_id, kind, source_person_id, target_person_id, \
                    moved_slot_ids_json, returned_slot_ids_json, reason, \
                    provenance_json, occurred_at_ms, commit_seq \
               FROM person_identity_events \
              WHERE agent_id=?1 \
              ORDER BY occurred_at_ms, commit_seq, event_id",
            params![agent_id.to_string()],
        )?;
        rows.iter().map(IdentityEventRow::to_event).collect()
    }

    /// Merge `source` into `target`: every slot currently owned by `source`
    /// moves to `target`. The original attributions are not rewritten; the
    /// event is the record, and a later split can return slots.
    pub fn merge_persons(
        &self,
        agent_id: EntityId,
        source: PersonId,
        target: PersonId,
        reason: Option<String>,
        provenance: Provenance,
    ) -> Result<PersonIdentityEvent, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_merge_persons(agent_id, source, target, reason, provenance),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_merge_persons(
        &self,
        agent_id: EntityId,
        source: PersonId,
        target: PersonId,
        reason: Option<String>,
        provenance: Provenance,
    ) -> Result<PersonIdentityEvent, ShenkoError> {
        if source == target {
            return Err(invalid("cannot merge a person into itself"));
        }
        let source_profile = person_slot_id(agent_id, PersonClaimKind::Profile, source, "profile");
        let target_profile = person_slot_id(agent_id, PersonClaimKind::Profile, target, "profile");
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let events = self.read_events_in(&tx, agent_id)?;

        let source_current = self.current_revision_of(&tx, agent_id, &source_profile)?;
        let target_current = self.current_revision_of(&tx, agent_id, &target_profile)?;
        let source_owner = match &source_current {
            Some(row) => {
                let original: PersonId = row.person_id.parse().map_err(|_| {
                    ShenkoError::storage("contacts: corrupt person id")
                })?;
                self.resolve_slot(&events, original, source_profile)
            }
            None => source,
        };
        let target_owner = match &target_current {
            Some(row) => {
                let original: PersonId = row.person_id.parse().map_err(|_| {
                    ShenkoError::storage("contacts: corrupt person id")
                })?;
                self.resolve_slot(&events, original, target_profile)
            }
            None => target,
        };
        if source_current.is_none() || target_current.is_none() {
            tx.rollback().map_err(db)?;
            return Err(contacts_error(
                "contacts_person_unknown",
                "both merge participants must have a current profile",
            ));
        }
        if source_owner == target && source_owner != source {
            // Already merged: replay returns the original event so a
            // recovered attempt cannot fork the history.
            let rows = read_event_rows(
                &tx,
                "SELECT event_id, kind, source_person_id, target_person_id, \
                        moved_slot_ids_json, returned_slot_ids_json, reason, \
                        provenance_json, occurred_at_ms, commit_seq \
                   FROM person_identity_events \
                  WHERE agent_id=?1 AND kind='merged' AND source_person_id=?2 AND target_person_id=?3 \
                  ORDER BY occurred_at_ms DESC, commit_seq DESC LIMIT 1",
                params![agent_id.to_string(), source.to_string(), target.to_string()],
            )?;
            tx.rollback().map_err(db)?;
            let Some(row) = rows.into_iter().next() else {
                return Err(contacts_error(
                    "contacts_person_already_merged",
                    format!("person {source} already resolves to {source_owner}"),
                ));
            };
            return row.to_event();
        }
        if source_owner != source {
            tx.rollback().map_err(db)?;
            return Err(contacts_error(
                "contacts_person_already_merged",
                format!("person {source} already resolves to {source_owner}"),
            ));
        }
        if target_owner != target {
            tx.rollback().map_err(db)?;
            return Err(contacts_error(
                "contacts_person_already_merged",
                format!("person {target} already resolves to {target_owner}"),
            ));
        }

        // Move every slot whose current owner is the source.
        let rows = read_claim_rows(
            &tx,
            "SELECT c.claim_id, c.slot_id, c.person_id, c.kind, c.claim_key, c.revision, \
                    c.payload_json, c.provenance_json, c.confidence_json, c.state, \
                    c.scope_json, c.created_at_ms, c.commit_seq \
               FROM person_claims c \
               JOIN (SELECT slot_id, MAX(revision) AS max_rev FROM person_claims \
                      WHERE agent_id=?1 GROUP BY slot_id) latest \
                 ON latest.slot_id = c.slot_id AND latest.max_rev = c.revision \
              WHERE c.agent_id=?1",
            params![agent_id.to_string()],
        )?;
        let mut moved = Vec::new();
        for row in &rows {
            let original: PersonId = row.person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt person id")
            })?;
            let slot_id: EntityId = row.slot_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt slot id")
            })?;
            if self.resolve_slot(&events, original, slot_id) == source {
                moved.push(slot_id);
            }
        }
        if !moved.contains(&source_profile) {
            moved.push(source_profile);
        }

        let seq = self.ledger.allocate_seq(&tx)?;
        let ledger_id = self.ledger.ledger_id();
        let event = PersonIdentityEvent {
            event_id: EntityId::new(),
            agent_id,
            kind: PersonIdentityEventKind::Merged,
            source_person_id: source,
            target_person_id: target,
            moved_slot_ids: moved.clone(),
            returned_slot_ids: vec![],
            reason,
            provenance,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            commit_seq: seq.0,
        };
        tx.execute(
            "INSERT INTO person_identity_events \
                (event_id, agent_id, kind, source_person_id, target_person_id, \
                 moved_slot_ids_json, returned_slot_ids_json, reason, \
                 provenance_json, occurred_at_ms, commit_seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                event.event_id.to_string(),
                agent_id.to_string(),
                "merged",
                source.to_string(),
                target.to_string(),
                encode(&event.moved_slot_ids)?,
                encode(&event.returned_slot_ids)?,
                event.reason,
                encode(&event.provenance)?,
                event.occurred_at_ms,
                seq.0 as i64,
            ],
        )
        .map_err(db)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(db)?;
        Ok(event)
    }

    /// Split: return the named slots from `target` back to their original
    /// owner `source`. At least one slot must currently be owned by `target`
    /// and originally attributed to `source`. Including the source's profile
    /// slot resurrects the person as a separate identity.
    pub fn split_persons(
        &self,
        agent_id: EntityId,
        target: PersonId,
        source: PersonId,
        slot_ids: &[EntityId],
        reason: Option<String>,
        provenance: Provenance,
    ) -> Result<PersonIdentityEvent, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_split_persons(agent_id, target, source, slot_ids, reason, provenance),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_split_persons(
        &self,
        agent_id: EntityId,
        target: PersonId,
        source: PersonId,
        slot_ids: &[EntityId],
        reason: Option<String>,
        provenance: Provenance,
    ) -> Result<PersonIdentityEvent, ShenkoError> {
        if source == target {
            return Err(invalid("cannot split a person from itself"));
        }
        if slot_ids.is_empty() {
            return Err(invalid("split requires at least one slot to return"));
        }
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let events = self.read_events_in(&tx, agent_id)?;

        let mut validated: Vec<(EntityId, PersonId)> = Vec::new();
        for slot_id in slot_ids {
            let slot: Option<String> = tx
                .query_row(
                    "SELECT person_id FROM person_claims WHERE agent_id=?1 AND slot_id=?2 LIMIT 1",
                    params![agent_id.to_string(), slot_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db)?;
            let Some(original) = slot else {
                tx.rollback().map_err(db)?;
                return Err(contacts_error(
                    "contacts_slot_unknown",
                    format!("no claim slot {slot_id}"),
                ));
            };
            let original: PersonId = original.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt person id")
            })?;
            let current = self.resolve_slot(&events, original, *slot_id);
            if current != target {
                tx.rollback().map_err(db)?;
                return Err(contacts_error(
                    "contacts_slot_not_owned_by_target",
                    format!("slot {slot_id} is owned by {current}, not {target}"),
                ));
            }
            if original != source {
                tx.rollback().map_err(db)?;
                return Err(contacts_error(
                    "contacts_slot_not_attributed_to_source",
                    format!("slot {slot_id} was never attributed to {source}"),
                ));
            }
            validated.push((*slot_id, current));
        }

        // Recovery replay: every requested slot is already back with the
        // source, so the split already happened and its event is returned.
        let pending: Vec<EntityId> = validated
            .iter()
            .filter(|(_, current)| *current == target)
            .map(|(slot, _)| *slot)
            .collect();
        if pending.is_empty() {
            let rows = read_event_rows(
                &tx,
                "SELECT event_id, kind, source_person_id, target_person_id, \
                        moved_slot_ids_json, returned_slot_ids_json, reason, \
                        provenance_json, occurred_at_ms, commit_seq \
                   FROM person_identity_events \
                  WHERE agent_id=?1 AND kind='split' AND source_person_id=?2 AND target_person_id=?3 \
                  ORDER BY occurred_at_ms DESC, commit_seq DESC LIMIT 1",
                params![agent_id.to_string(), source.to_string(), target.to_string()],
            )?;
            tx.rollback().map_err(db)?;
            let Some(row) = rows.into_iter().next() else {
                return Err(contacts_error(
                    "contacts_slot_already_returned",
                    "the requested slots are already with their source, \
                     but no split event records it",
                ));
            };
            return row.to_event();
        }

        let seq = self.ledger.allocate_seq(&tx)?;
        let ledger_id = self.ledger.ledger_id();
        let event = PersonIdentityEvent {
            event_id: EntityId::new(),
            agent_id,
            kind: PersonIdentityEventKind::Split,
            source_person_id: source,
            target_person_id: target,
            moved_slot_ids: vec![],
            returned_slot_ids: pending,
            reason,
            provenance,
            occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            commit_seq: seq.0,
        };
        tx.execute(
            "INSERT INTO person_identity_events \
                (event_id, agent_id, kind, source_person_id, target_person_id, \
                 moved_slot_ids_json, returned_slot_ids_json, reason, \
                 provenance_json, occurred_at_ms, commit_seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                event.event_id.to_string(),
                agent_id.to_string(),
                "split",
                source.to_string(),
                target.to_string(),
                encode(&event.moved_slot_ids)?,
                encode(&event.returned_slot_ids)?,
                event.reason,
                encode(&event.provenance)?,
                event.occurred_at_ms,
                seq.0 as i64,
            ],
        )
        .map_err(db)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(db)?;
        Ok(event)
    }

    /// Identity events mentioning `person_id` as source or target, oldest first.
    pub fn person_identity_events(
        &self,
        agent_id: EntityId,
        person_id: PersonId,
    ) -> Result<Vec<PersonIdentityEvent>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let rows = read_event_rows(
            &conn,
            "SELECT event_id, kind, source_person_id, target_person_id, \
                    moved_slot_ids_json, returned_slot_ids_json, reason, \
                    provenance_json, occurred_at_ms, commit_seq \
               FROM person_identity_events \
              WHERE agent_id=?1 AND (source_person_id=?2 OR target_person_id=?2) \
              ORDER BY occurred_at_ms, commit_seq, event_id",
            params![agent_id.to_string(), person_id.to_string()],
        )?;
        rows.iter().map(IdentityEventRow::to_event).collect()
    }

    /// The current revision of every slot in the agent, resolved. Used by the
    /// lookups below; bounded by the caller's limit.
    fn current_claim_rows(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
    ) -> Result<Vec<(ClaimRow, PersonId, PersonId)>, ShenkoError> {
        let rows = read_claim_rows(
            conn,
            "SELECT c.claim_id, c.slot_id, c.person_id, c.kind, c.claim_key, c.revision, \
                    c.payload_json, c.provenance_json, c.confidence_json, c.state, \
                    c.scope_json, c.created_at_ms, c.commit_seq \
               FROM person_claims c \
               JOIN (SELECT slot_id, MAX(revision) AS max_rev FROM person_claims \
                      WHERE agent_id=?1 GROUP BY slot_id) latest \
                 ON latest.slot_id = c.slot_id AND latest.max_rev = c.revision \
              WHERE c.agent_id=?1",
            params![agent_id.to_string()],
        )?;
        let events = self.read_events_in(conn, agent_id)?;
        let mut out = Vec::new();
        for row in rows {
            let original: PersonId = row.person_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt person id")
            })?;
            let slot_id: EntityId = row.slot_id.parse().map_err(|_| {
                ShenkoError::storage("contacts: corrupt slot id")
            })?;
            let owner = self.resolve_slot(&events, original, slot_id);
            out.push((row, original, owner));
        }
        Ok(out)
    }

    /// Every person (current after resolution), most recently updated first.
    pub fn persons(&self, agent_id: EntityId, limit: usize) -> Result<Vec<PersonView>, ShenkoError> {
        let mut owners: Vec<PersonId> = {
            let conn = self.ledger.conn.lock();
            let mut seen: Vec<PersonId> = Vec::new();
            for (row, _, owner) in self.current_claim_rows(&conn, agent_id)? {
                if row.kind != PersonClaimKind::Profile.as_str() || seen.contains(&owner) {
                    continue;
                }
                seen.push(owner);
            }
            seen
        };
        let mut views = Vec::new();
        for owner in owners.drain(..) {
            if views.len() >= limit {
                break;
            }
            if let Some(view) = self.person_view(agent_id, owner)? {
                views.push(view);
            }
        }
        views.sort_by(|a, b| {
            b.claims
                .first()
                .map(|c| c.created_at_ms)
                .unwrap_or(0)
                .cmp(&a.claims.first().map(|c| c.created_at_ms).unwrap_or(0))
        });
        Ok(views)
    }

    /// The resolved view of one person, including the owner-set admin fact.
    pub fn person_view(
        &self,
        agent_id: EntityId,
        person_id: PersonId,
    ) -> Result<Option<PersonView>, ShenkoError> {
        let canonical = self.resolve_person(agent_id, person_id)?;
        let claims = self.person_claims(agent_id, canonical)?;
        if claims.is_empty() {
            return Ok(None);
        }
        let mut merged_from: Vec<PersonId> = {
            let conn = self.ledger.conn.lock();
            let mut merged = Vec::new();
            for (_claim, original, owner) in self.current_claim_rows(&conn, agent_id)? {
                if owner == canonical && original != canonical {
                    merged.push(original);
                }
            }
            merged
        };
        merged_from.sort();
        merged_from.dedup();

        let admin = self.admin_fact(agent_id, AdminSubjectKind::Person, &canonical.to_string())?;
        Ok(Some(PersonView {
            person_id: canonical,
            merged_from,
            claims,
            admin,
        }))
    }

    /// Persons bound to the given platform account (current attribution).
    pub fn person_by_platform_account(
        &self,
        agent_id: EntityId,
        account: &PlatformAccount,
    ) -> Result<Vec<PersonView>, ShenkoError> {
        let key = account.key();
        let owners: Vec<PersonId> = {
            let conn = self.ledger.conn.lock();
            let mut owners = Vec::new();
            for (row, _, owner) in self.current_claim_rows(&conn, agent_id)? {
                if row.kind == PersonClaimKind::PlatformAccount.as_str() && row.claim_key == key {
                    if !owners.contains(&owner) {
                        owners.push(owner);
                    }
                }
            }
            owners
        };
        let mut views = Vec::new();
        for owner in owners {
            if let Some(view) = self.person_view(agent_id, owner)? {
                views.push(view);
            }
        }
        Ok(views)
    }

    /// Persons whose current alias text matches `alias` (case-insensitive).
    pub fn person_by_alias(
        &self,
        agent_id: EntityId,
        alias: &str,
    ) -> Result<Vec<PersonView>, ShenkoError> {
        let wanted = alias.trim().to_lowercase();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let owners: Vec<PersonId> = {
            let conn = self.ledger.conn.lock();
            let mut owners = Vec::new();
            for (row, _, owner) in self.current_claim_rows(&conn, agent_id)? {
                if row.kind != PersonClaimKind::Alias.as_str() {
                    continue;
                }
                let text = serde_json::from_str::<serde_json::Value>(&row.payload_json)
                    .ok()
                    .and_then(|v| v.get("alias").and_then(|a| a.as_str()).map(str::to_owned));
                if text.is_some_and(|t| t.trim().eq_ignore_ascii_case(&wanted))
                    && !owners.contains(&owner)
                {
                    owners.push(owner);
                }
            }
            owners
        };
        let mut views = Vec::new();
        for owner in owners {
            if let Some(view) = self.person_view(agent_id, owner)? {
                views.push(view);
            }
        }
        Ok(views)
    }

    /// Record one explicitly human admin fact. Only the owner's local path
    /// may call this; the model-facing surface is read-only.
    pub fn set_admin_fact(
        &self,
        agent_id: EntityId,
        input: &AdminFactInput,
    ) -> Result<AdminFact, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_set_admin_fact(agent_id, input),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_set_admin_fact(
        &self,
        agent_id: EntityId,
        input: &AdminFactInput,
    ) -> Result<AdminFact, ShenkoError> {
        if input.granted_by.trim().is_empty() {
            return Err(invalid(
                "admin facts require an explicit human actor; \
                 they are never derived from familiarity, group status or voiceprints",
            ));
        }
        let kind = match input.subject_kind {
            AdminSubjectKind::Person => "person",
            AdminSubjectKind::PlatformAccount => "platform_account",
        };
        let mut conn = self.ledger.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db)?;
        let existing: Option<(i64, String, Option<String>, i64, i64)> = tx
            .query_row(
                "SELECT granted, granted_by, reason, updated_at_ms, commit_seq \
                   FROM admin_facts \
                  WHERE agent_id=?1 AND subject_kind=?2 AND subject_key=?3",
                params![agent_id.to_string(), kind, input.subject_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(db)?;
        let now = chrono::Utc::now().timestamp_millis();
        if let Some((granted, by, reason, updated, seq)) = existing {
            if (granted != 0) == input.granted {
                // Same decision already recorded: nothing new to commit.
                tx.rollback().map_err(db)?;
                return Ok(AdminFact {
                    agent_id,
                    subject_kind: input.subject_kind,
                    subject_key: input.subject_key.clone(),
                    granted: granted != 0,
                    granted_by: by,
                    reason,
                    updated_at_ms: updated,
                    commit_seq: seq as u64,
                });
            }
        }
        let seq = self.ledger.allocate_seq(&tx)?;
        tx.execute(
            "INSERT OR REPLACE INTO admin_facts \
                (agent_id, subject_kind, subject_key, granted, granted_by, reason, \
                 updated_at_ms, commit_seq) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                agent_id.to_string(),
                kind,
                input.subject_key,
                input.granted as i64,
                input.granted_by,
                input.reason,
                now,
                seq.0 as i64,
            ],
        )
        .map_err(db)?;
        self.ledger.record_commit(&tx, seq, 1)?;
        tx.commit().map_err(db)?;
        Ok(AdminFact {
            agent_id,
            subject_kind: input.subject_kind,
            subject_key: input.subject_key.clone(),
            granted: input.granted,
            granted_by: input.granted_by.clone(),
            reason: input.reason.clone(),
            updated_at_ms: now,
            commit_seq: seq.0,
        })
    }

    /// The current admin fact for one subject, when one exists.
    pub fn admin_fact(
        &self,
        agent_id: EntityId,
        kind: AdminSubjectKind,
        subject_key: &str,
    ) -> Result<Option<AdminFact>, ShenkoError> {
        let kind = match kind {
            AdminSubjectKind::Person => "person",
            AdminSubjectKind::PlatformAccount => "platform_account",
        };
        let conn = self.ledger.conn.lock();
        let row: Option<(i64, String, Option<String>, i64, i64)> = conn
            .query_row(
                "SELECT granted, granted_by, reason, updated_at_ms, commit_seq \
                   FROM admin_facts \
                  WHERE agent_id=?1 AND subject_kind=?2 AND subject_key=?3",
                params![agent_id.to_string(), kind, subject_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .optional()
            .map_err(db)?;
        Ok(row.map(|(granted, by, reason, updated, seq)| AdminFact {
            agent_id,
            subject_kind: match kind {
                "person" => AdminSubjectKind::Person,
                _ => AdminSubjectKind::PlatformAccount,
            },
            subject_key: subject_key.to_owned(),
            granted: granted != 0,
            granted_by: by,
            reason,
            updated_at_ms: updated,
            commit_seq: seq as u64,
        }))
    }

    /// Every admin fact of the agent.
    pub fn list_admin_facts(&self, agent_id: EntityId) -> Result<Vec<AdminFact>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT subject_kind, subject_key, granted, granted_by, reason, \
                        updated_at_ms, commit_seq \
                   FROM admin_facts WHERE agent_id=?1 ORDER BY updated_at_ms",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map(params![agent_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(db)?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, key, granted, by, reason, updated, seq) = row.map_err(db)?;
            out.push(AdminFact {
                agent_id,
                subject_kind: if kind == "person" {
                    AdminSubjectKind::Person
                } else {
                    AdminSubjectKind::PlatformAccount
                },
                subject_key: key,
                granted: granted != 0,
                granted_by: by,
                reason,
                updated_at_ms: updated,
                commit_seq: seq as u64,
            });
        }
        Ok(out)
    }

    /// Admit one Life voiceprint rank record: the record itself plus one
    /// link claim per candidate person (candidate links for suspected and
    /// unresolved candidates, a confirmed link only when the record carries
    /// an explicit confirmation for that person). The recording scope and
    /// model revision stay distinct on every row; nothing is overwritten
    /// when a new revision arrives (§15.2.1).
    pub fn admit_life_rank(
        &self,
        agent_id: EntityId,
        record: &LifeRankRecord,
    ) -> Result<LifeRankReceipt, ShenkoError> {
        self.ledger.with_mutation(
            || self.try_admit_life_rank(agent_id, record),
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    fn try_admit_life_rank(
        &self,
        agent_id: EntityId,
        record: &LifeRankRecord,
    ) -> Result<LifeRankReceipt, ShenkoError> {
        if record.rank_id.trim().is_empty() || record.recording_scope.trim().is_empty() {
            return Err(invalid("life ranks require a rank id and a recording scope"));
        }
        if record.model_revision.trim().is_empty() {
            return Err(invalid(
                "life ranks require an explicit model_revision; \
                 without it the registry revision is unknown",
            ));
        }
        let ledger_id = self.ledger.ledger_id();
        let seq = {
            let mut conn = self.ledger.conn.lock();
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT data FROM life_voiceprint_ranks \
                      WHERE agent_id=?1 AND recording_scope=?2 AND rank_id=?3 \
                        AND model_revision=?4",
                    params![
                        agent_id.to_string(),
                        record.recording_scope,
                        record.rank_id,
                        record.model_revision,
                    ],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db)?;
            if let Some(stored) = existing {
                if stored == encode(record)? {
                    let seq: i64 = tx
                        .query_row(
                            "SELECT commit_seq FROM life_voiceprint_ranks \
                              WHERE agent_id=?1 AND recording_scope=?2 AND rank_id=?3 \
                                AND model_revision=?4",
                            params![
                                agent_id.to_string(),
                                record.recording_scope,
                                record.rank_id,
                                record.model_revision,
                            ],
                            |row| row.get(0),
                        )
                        .map_err(db)?;
                    tx.commit().map_err(db)?;
                    return Ok(LifeRankReceipt {
                        recording_scope: record.recording_scope.clone(),
                        rank_id: record.rank_id.clone(),
                        model_revision: record.model_revision.clone(),
                        links: 0,
                        replayed: true,
                        commit_ref: CommitRef::new(ledger_id, LedgerSeq(seq as u64)),
                    });
                }
                tx.rollback().map_err(db)?;
                return Err(contacts_error(
                    "contacts_life_rank_conflict",
                    "a different fact already holds this rank identity and revision",
                ));
            }

            let seq = self.ledger.allocate_seq(&tx)?;
            tx.execute(
                "INSERT INTO life_voiceprint_ranks \
                    (agent_id, recording_scope, rank_id, model_revision, cluster_id, \
                     confirmed_identity, data, recorded_at_ms, commit_seq) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    agent_id.to_string(),
                    record.recording_scope,
                    record.rank_id,
                    record.model_revision,
                    record.cluster_id,
                    record.confirmed_identity,
                    encode(record)?,
                    chrono::Utc::now().timestamp_millis(),
                    seq.0 as i64,
                ],
            )
            .map_err(db)?;
            self.ledger.record_commit(&tx, seq, 1)?;
            tx.commit().map_err(db)?;
            seq
        };
        // Link claims, one per candidate, on their own admissions so each
        // gets its own ledger commit and remains individually auditable.
        let mut links = 0usize;
        let mut linked = Vec::new();
        for candidate in &record.candidates {
            let Some(life_id) = candidate.life_person_id.as_deref() else {
                continue;
            };
            if linked.contains(&life_id) {
                continue;
            }
            linked.push(life_id);
            let person = life_person_id(life_id);
            let confirmed = record.confirmed_identity.as_deref() == Some(life_id)
                && candidate.status == LifeCandidateStatus::Confirmed;
            let claim_key = format!(
                "life_rank{}{}{}{}",
                CONTACTS_KEY_SEPARATOR,
                record.recording_scope,
                CONTACTS_KEY_SEPARATOR,
                record.rank_id
            );
            let payload = serde_json::json!({
                "rank_id": record.rank_id,
                "cluster_id": record.cluster_id,
                "recording_scope": record.recording_scope,
                "model_revision": record.model_revision,
                "life_person_id": life_id,
                "candidate": candidate,
                "link": if confirmed { "confirmed" } else { "candidate" },
            });
            let input = PersonClaimInput {
                person_id: person,
                kind: PersonClaimKind::LifeIdentity,
                claim_key: claim_key.clone(),
                payload,
                provenance: Provenance {
                    source: ClaimSource::LifeAdapter {
                        life_person_id: Some(life_id.to_owned()),
                        model_revision: Some(record.model_revision.clone()),
                    },
                    evidence_ref: candidate.evidence_ref.clone(),
                    observed_at_ms: None,
                },
                confidence: Confidence::inferred(candidate.score),
                state: if confirmed {
                    PersonClaimState::Confirmed
                } else {
                    PersonClaimState::Candidate
                },
                scope: ClaimScope {
                    kind: ClaimScopeKind::Recording,
                    reference: Some(record.recording_scope.clone()),
                },
                slot_id: Some(global_slot_id(
                    agent_id,
                    PersonClaimKind::LifeIdentity,
                    &format!("{life_id}{CONTACTS_KEY_SEPARATOR}{claim_key}"),
                )),
            };
            // A candidate whose person is not in the directory stays
            // readable from the rank record itself; only a real storage or
            // protocol failure aborts the import.
            match self.admit_person_claim(agent_id, &input) {
                Ok(_) => links += 1,
                Err(ShenkoError::Protocol(error)) if error.code == "contacts_person_unknown" => {}
                Err(error) => return Err(error),
            }
        }

        Ok(LifeRankReceipt {
            recording_scope: record.recording_scope.clone(),
            rank_id: record.rank_id.clone(),
            model_revision: record.model_revision.clone(),
            links,
            replayed: false,
            commit_ref: CommitRef::new(ledger_id, seq),
        })
    }

    /// One Life rank record at an exact registry revision.
    pub fn life_rank(
        &self,
        agent_id: EntityId,
        recording_scope: &str,
        rank_id: &str,
        model_revision: &str,
    ) -> Result<Option<LifeRankRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let data: Option<String> = conn
            .query_row(
                "SELECT data FROM life_voiceprint_ranks \
                  WHERE agent_id=?1 AND recording_scope=?2 AND rank_id=?3 AND model_revision=?4",
                params![
                    agent_id.to_string(),
                    recording_scope,
                    rank_id,
                    model_revision,
                ],
                |row| row.get(0),
            )
            .optional()
            .map_err(db)?;
        data.as_deref().map(|data| decode::<LifeRankRecord>(data, "life voiceprint rank")).transpose()
    }

    /// Every registry revision of one rank, oldest first.
    pub fn life_rank_history(
        &self,
        agent_id: EntityId,
        recording_scope: &str,
        rank_id: &str,
    ) -> Result<Vec<LifeRankRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT data FROM life_voiceprint_ranks \
                  WHERE agent_id=?1 AND recording_scope=?2 AND rank_id=?3 \
                  ORDER BY recorded_at_ms, commit_seq",
            )
            .map_err(db)?;
        let rows = stmt
            .query_map(
                params![agent_id.to_string(), recording_scope, rank_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(db)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(decode::<LifeRankRecord>(&row.map_err(db)?, "life voiceprint rank")?);
        }
        Ok(out)
    }
}
