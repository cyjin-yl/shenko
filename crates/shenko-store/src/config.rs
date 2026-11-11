//! Durable configuration, owner claim and Playground business state.
//! docs/specs/configuration-playground.md#spec-43-1 (one configuration service), docs/specs/configuration-playground.md#spec-43-2 (Playground works and their
//! business state), docs/specs/plugins.md#spec-44-4 (the first owner claim).
//!
//! These are ordinary rows in the same SQLite database as tasks, operations
//! and evidence. Every write goes through the ledger's single mutation line,
//! allocates a real commit sequence and records it in `commit_log`, so a
//! revision always names the commit that produced it. There is no second
//! ledger, no second registry database and no in-memory truth here: the
//! runtime registries (which plugin owns which namespace) live with the
//! plugins, this module owns only the facts those registries change.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use chrono::{DateTime, Utc};

use shenko_types::commit::CommitRef;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

use crate::store::Store;

/// Schema owned by this module. Applied by [`ensure_config_schema`] from the
/// shared migration step; it is additive and never rewrites an existing row.
pub const CONFIG_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS owner_grants (
    installation_id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS owner_admin_tokens (
    installation_id TEXT PRIMARY KEY,
    token_hash TEXT NOT NULL,
    issued_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS config_entries (
    namespace TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    desired_revision INTEGER NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (namespace, logical_key)
);

CREATE TABLE IF NOT EXISTS config_changes (
    change_id TEXT PRIMARY KEY,
    origin_identity TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    namespace TEXT NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_config_changes_origin
    ON config_changes(origin_identity, logical_key);

CREATE TABLE IF NOT EXISTS config_adoptions (
    namespace TEXT NOT NULL,
    logical_key TEXT NOT NULL,
    consumer_id TEXT NOT NULL,
    adopted_revision INTEGER NOT NULL,
    record_revision INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (namespace, logical_key, consumer_id)
);
"#;

/// Apply this module's schema. Idempotent: an existing installation keeps its
/// rows and only gains a missing table.
pub fn ensure_config_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(CONFIG_SCHEMA).context("config schema")?;
    Ok(())
}

/// A configuration key name filter: `Some` names one key, `None` means "every
/// key" (only legal when both filters are `None`).
pub type OptionalKeyName<'a> = Option<&'a str>;

/// The installation-scoped owner grant — docs/specs/plugins.md#spec-44-4.
///
/// The grant records WHO was recognized as the installer. It never carries a
/// credential: the bearer secret lives in `owner_admin_tokens`, which is
/// installation-local and is not part of the replicated closure.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerGrant {
    pub schema_version: u32,
    pub grant_id: EntityId,
    pub installation_id: EntityId,
    pub claim_id: EntityId,
    pub human_principal_ref: ObjectRef,
    pub authority_ref: ObjectRef,
    pub committed_at: CommitRef,
}

/// What a claim attempt did. `AlreadyClaimed` never carries a new grant: the
/// installation keeps the first recognized owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimOutcome {
    /// The claim created the first owner grant.
    Granted,
    /// The same claim identity was replayed; the original grant stands.
    Replayed,
    /// Another claim already owns this installation; this caller got nothing.
    AlreadyClaimed,
}

/// The saved desired value of one configuration key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigEntry {
    pub schema_version: u32,
    pub namespace: String,
    pub logical_key: String,
    pub desired_revision: u64,
    /// CAS-sealed desired value. `None` means the key exists but carries no
    /// value yet (a removed value is an explicit value, not an absent row).
    pub desired_value_ref: Option<ObjectRef>,
    pub actor_and_authority_ref: ObjectRef,
    pub updated_commit: CommitRef,
}

/// One consumer's actual adoption of a desired revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAdoption {
    pub consumer_id: String,
    pub adopted_revision: u64,
    pub adopted_at_commit: CommitRef,
    pub detail: String,
}

/// The durable receipt of one configuration change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReceipt {
    pub receipt_id: EntityId,
    pub change_id: EntityId,
    /// applied | rejected | failed | unknown — never a bare "ok".
    pub outcome: String,
    pub detail: String,
    pub observation_refs: Vec<ObjectRef>,
    pub recorded_at: CommitRef,
}

/// The process incarnation that owns one apply attempt — docs/specs/configuration-playground.md#spec-43-1 (CFG02).
///
/// The attempt owner is committed in the SAME transaction as the change, so
/// "an apply was started for this intent" is a durable fact rather than an
/// inference from a crash. Recovery asks it exactly one question: is the owner
/// of this attempt still alive? A provably dead owner makes the effect
/// unproven — never repeatable; a live or unprovable owner means another
/// process still owns the attempt and this one must not settle it.
///
/// A bare pid is not identity: it is paired with the kernel start token so a
/// reused pid is never mistaken for the original owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigAttemptOwner {
    pub schema_version: u32,
    pub incarnation: String,
    pub pid: u32,
    /// `/proc/<pid>/stat` field 22 — the same-process test.
    pub process_start_token: String,
    pub started_at: DateTime<Utc>,
}

/// One durable configuration change — docs/specs/configuration-playground.md#spec-43-1.
///
/// A change is a fact about an intent and its result, not a phase machine:
/// `state` is `recorded` until an apply attempt produces a receipt, and a
/// failed apply keeps the saved desired revision separate from what any
/// consumer actually adopted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigChange {
    pub schema_version: u32,
    pub change_id: EntityId,
    pub namespace: String,
    pub logical_key: String,
    /// The idempotency identity: an origin event or task identity. The pair
    /// `(origin_identity, logical_key)` is unique; a different payload under
    /// the same pair is a conflict, never a silent overwrite.
    pub origin_identity: String,
    pub actor_and_authority_ref: ObjectRef,
    pub target_refs: Vec<ObjectRef>,
    pub desired_patch_ref: Option<ObjectRef>,
    pub base_snapshot_ref: Option<ObjectRef>,
    pub candidate_snapshot_ref: Option<ObjectRef>,
    pub coordinating_task_id: Option<EntityId>,
    pub operation_refs: Vec<EntityId>,
    pub observation_refs: Vec<ObjectRef>,
    pub recovery_program_refs: Vec<ObjectRef>,
    pub activation_refs: Vec<ObjectRef>,
    pub expected_base_revision: u64,
    pub desired_revision: u64,
    /// applying | applied | failed | unknown — `recorded` only on rows that
    /// predate attempt ownership (see `attempt`).
    pub state: String,
    /// The process that owns this change's apply attempt, committed with the
    /// change itself and cleared when the receipt is recorded. `None` means no
    /// attempt ownership was ever proven for this row.
    #[serde(default)]
    pub attempt: Option<ConfigAttemptOwner>,
    pub adopted: Vec<ConfigAdoption>,
    pub receipt: Option<ConfigReceipt>,
    pub record_revision: u64,
    pub created_commit: CommitRef,
    pub updated_commit: CommitRef,
}

/// The draft a caller submits; the store assigns identity-independent facts
/// (the desired revision and the commits) while committing it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigChangeDraft {
    pub change_id: EntityId,
    pub namespace: String,
    pub logical_key: String,
    pub origin_identity: String,
    pub actor_and_authority_ref: ObjectRef,
    pub target_refs: Vec<ObjectRef>,
    pub desired_patch_ref: Option<ObjectRef>,
    pub base_snapshot_ref: Option<ObjectRef>,
    pub candidate_snapshot_ref: Option<ObjectRef>,
    pub coordinating_task_id: Option<EntityId>,
    pub operation_refs: Vec<EntityId>,
    pub observation_refs: Vec<ObjectRef>,
    pub recovery_program_refs: Vec<ObjectRef>,
    pub activation_refs: Vec<ObjectRef>,
    pub expected_base_revision: u64,
    /// The attempt the committer is about to start. It moves into the change
    /// row in the same transaction, so "an apply was started" never depends on
    /// a second write that a crash could skip.
    #[serde(default)]
    pub attempt: Option<ConfigAttemptOwner>,
}

impl ConfigChange {
    fn from_draft(draft: &ConfigChangeDraft, desired_revision: u64, commit: CommitRef) -> Self {
        Self {
            schema_version: 1,
            change_id: draft.change_id,
            namespace: draft.namespace.clone(),
            logical_key: draft.logical_key.clone(),
            origin_identity: draft.origin_identity.clone(),
            actor_and_authority_ref: draft.actor_and_authority_ref.clone(),
            target_refs: draft.target_refs.clone(),
            desired_patch_ref: draft.desired_patch_ref.clone(),
            base_snapshot_ref: draft.base_snapshot_ref.clone(),
            candidate_snapshot_ref: draft.candidate_snapshot_ref.clone(),
            coordinating_task_id: draft.coordinating_task_id,
            operation_refs: draft.operation_refs.clone(),
            observation_refs: draft.observation_refs.clone(),
            recovery_program_refs: draft.recovery_program_refs.clone(),
            activation_refs: draft.activation_refs.clone(),
            expected_base_revision: draft.expected_base_revision,
            desired_revision,
            state: if draft.attempt.is_some() { "applying".into() } else { "recorded".into() },
            attempt: draft.attempt.clone(),
            adopted: Vec::new(),
            receipt: None,
            record_revision: 0,
            created_commit: commit,
            updated_commit: commit,
        }
    }
}

impl Store {
    /// Recognize the first owner of this installation — docs/specs/plugins.md#spec-44-4.
    ///
    /// The write is conditional on the installation having no grant: a second
    /// browser receives `AlreadyClaimed` and no authority. The same claim
    /// identity replayed returns the original grant, so a lost response never
    /// creates a second owner.
    pub fn claim_owner(
        &self,
        installation_id: EntityId,
        claim_id: EntityId,
        human_principal_ref: ObjectRef,
        authority_ref: ObjectRef,
        admin_token_hash: Digest,
    ) -> Result<(ClaimOutcome, Option<OwnerGrant>)> {
        // Read the existing grant before the writer is claimed, so a second
        // claim is answered without touching the mutation line.
        if let Some(existing) = self.owner_grant(installation_id)? {
            if existing.claim_id == claim_id {
                return Ok((ClaimOutcome::Replayed, Some(existing)));
            }
            return Ok((ClaimOutcome::AlreadyClaimed, Some(existing)));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        "SELECT data FROM owner_grants WHERE installation_id=?1",
                        [installation_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(raw) = stored {
                    // Another claim won between the read and the write.
                    let existing: OwnerGrant = serde_json::from_str(&raw)?;
                    tx.commit()?;
                    let outcome = if existing.claim_id == claim_id {
                        ClaimOutcome::Replayed
                    } else {
                        ClaimOutcome::AlreadyClaimed
                    };
                    return Ok((outcome, Some(existing)));
                }
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let grant = OwnerGrant {
                    schema_version: 1,
                    grant_id: EntityId::new(),
                    installation_id,
                    claim_id,
                    human_principal_ref,
                    authority_ref,
                    committed_at: commit,
                };
                tx.execute(
                    "INSERT INTO owner_grants(installation_id,grant_id,claim_id,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,0,?4,?5)",
                    params![
                        installation_id.to_string(),
                        grant.grant_id.to_string(),
                        claim_id.to_string(),
                        serde_json::to_string(&grant)?,
                        seq.0 as i64,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO owner_admin_tokens(installation_id,token_hash) VALUES(?1,?2)
                     ON CONFLICT(installation_id) DO UPDATE SET token_hash=excluded.token_hash",
                    params![installation_id.to_string(), admin_token_hash.0],
                )?;
                self.ledger.record_commit(&tx, seq, 2)?;
                tx.commit()?;
                Ok((ClaimOutcome::Granted, Some(grant)))
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// The recognized owner of this installation, if any.
    pub fn owner_grant(&self, installation_id: EntityId) -> Result<Option<OwnerGrant>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM owner_grants WHERE installation_id=?1",
                [installation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => {
                let grant: OwnerGrant = serde_json::from_str(&raw)?;
                ensure!(
                    grant.installation_id == installation_id,
                    "owner grant installation binding mismatch"
                );
                Ok(Some(grant))
            }
        }
    }

    /// Replace the installation's admin bearer secret. The stored value is a
    /// digest, never the secret itself.
    pub fn rotate_admin_token(&self, installation_id: EntityId, token_hash: Digest) -> Result<()> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "INSERT INTO owner_admin_tokens(installation_id,token_hash) VALUES(?1,?2)
                     ON CONFLICT(installation_id) DO UPDATE SET token_hash=excluded.token_hash",
                    params![installation_id.to_string(), token_hash.0],
                )?;
                let seq = self.ledger.allocate_seq(&tx)?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit()?;
                Ok(())
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>(),
        )
    }

    /// Check an admin bearer secret against the installation's stored digest.
    /// A missing grant or a missing token is "not authorized", never "allowed".
    pub fn verify_admin_token(&self, installation_id: EntityId, token: &str) -> Result<bool> {
        if token.is_empty() {
            return Ok(false);
        }
        let conn = self.ledger.conn.lock();
        let stored: Option<String> = conn
            .query_row(
                "SELECT token_hash FROM owner_admin_tokens WHERE installation_id=?1",
                [installation_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match stored {
            None => false,
            Some(hash) => hash == Digest::blake3(token.as_bytes()).0,
        })
    }

    /// The saved desired value of one configuration key.
    pub fn config_entry(&self, namespace: &str, logical_key: &str) -> Result<Option<ConfigEntry>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM config_entries WHERE namespace=?1 AND logical_key=?2",
                params![namespace, logical_key],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        }
    }

    /// Every saved configuration key of this installation.
    pub fn list_config_entries(&self) -> Result<Vec<ConfigEntry>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM config_entries ORDER BY namespace, logical_key",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<ConfigEntry>(&row?)?);
        }
        Ok(out)
    }

    /// The consumers that actually adopted a desired revision.
    pub fn config_adoptions(&self, namespace: &str, logical_key: &str) -> Result<Vec<ConfigAdoption>> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM config_adoptions WHERE namespace=?1 AND logical_key=?2
             ORDER BY consumer_id",
        )?;
        let rows = stmt.query_map(params![namespace, logical_key], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str::<ConfigAdoption>(&row?)?);
        }
        Ok(out)
    }

    /// One durable configuration change.
    pub fn config_change(&self, change_id: EntityId) -> Result<Option<ConfigChange>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM config_changes WHERE change_id=?1",
                [change_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        }
    }

    /// The change already recorded for an origin identity and logical key.
    pub fn config_change_by_origin(
        &self,
        origin_identity: &str,
        logical_key: &str,
    ) -> Result<Option<ConfigChange>> {
        let conn = self.ledger.conn.lock();
        let raw: Option<String> = conn
            .query_row(
                "SELECT data FROM config_changes WHERE origin_identity=?1 AND logical_key=?2",
                params![origin_identity, logical_key],
                |row| row.get(0),
            )
            .optional()?;
        match raw {
            None => Ok(None),
            Some(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        }
    }

    /// Every committed configuration change of one key (or of the whole
    /// installation) that has no receipt yet, newest first — docs/specs/configuration-playground.md#spec-43-1
    /// (CFG02 recovery).
    ///
    /// `state` is part of the row's JSON, not a column, so the filter runs
    /// after decoding: the alternative would be a second representation of
    /// the same fact in the schema.
    pub fn unsettled_config_changes(
        &self,
        namespace: OptionalKeyName<'_>,
        logical_key: OptionalKeyName<'_>,
    ) -> Result<Vec<ConfigChange>> {
        let conn = self.ledger.conn.lock();
        let mut out = Vec::new();
        let mut collect = |raw: String, out: &mut Vec<ConfigChange>| -> Result<()> {
            let change: ConfigChange = serde_json::from_str(&raw)?;
            if change.receipt.is_none() {
                out.push(change);
            }
            Ok(())
        };
        match (namespace, logical_key) {
            (Some(namespace), Some(logical_key)) => {
                let mut stmt = conn.prepare(
                    "SELECT data FROM config_changes WHERE namespace=?1 AND logical_key=?2
                     ORDER BY updated_commit_seq DESC",
                )?;
                let rows = stmt.query_map(params![namespace, logical_key], |row| {
                    row.get::<_, String>(0)
                })?;
                for row in rows {
                    collect(row?, &mut out)?;
                }
            }
            (None, None) => {
                let mut stmt = conn.prepare(
                    "SELECT data FROM config_changes ORDER BY updated_commit_seq DESC",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                for row in rows {
                    collect(row?, &mut out)?;
                }
            }
            _ => anyhow::bail!(
                "unsettled_config_changes needs both the namespace and the logical key, or neither"
            ),
        }
        Ok(out)
    }

    /// Commit a configuration change together with its new desired revision.
    ///
    /// The desired revision, the change record and the entry move in ONE
    /// ledger commit, so a reader never sees a revision without its change.
    /// The `(origin_identity, logical_key)` pair is unique: the same pair with
    /// the same change identity is a replay and returns the original change;
    /// the same pair with a different payload is a conflict.
    pub fn commit_config_change(&self, draft: &ConfigChangeDraft) -> Result<ConfigChange> {
        if let Some(existing) = self.config_change_by_origin(&draft.origin_identity, &draft.logical_key)? {
            if existing.change_id == draft.change_id {
                return Ok(existing);
            }
            anyhow::bail!(
                "CONFIG_LOGICAL_KEY_REUSED: '{}' under origin '{}' already belongs to change {}",
                draft.logical_key,
                draft.origin_identity,
                existing.change_id
            );
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let stored: Option<String> = tx
                    .query_row(
                        "SELECT data FROM config_changes WHERE origin_identity=?1 AND logical_key=?2",
                        params![draft.origin_identity, draft.logical_key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(raw) = stored {
                    let existing: ConfigChange = serde_json::from_str(&raw)?;
                    tx.commit()?;
                    if existing.change_id == draft.change_id {
                        return Ok(existing);
                    }
                    anyhow::bail!(
                        "CONFIG_LOGICAL_KEY_REUSED: '{}' under origin '{}' already belongs to change {}",
                        draft.logical_key,
                        draft.origin_identity,
                        existing.change_id
                    );
                }
                let current: Option<i64> = tx
                    .query_row(
                        "SELECT desired_revision FROM config_entries WHERE namespace=?1 AND logical_key=?2",
                        params![draft.namespace, draft.logical_key],
                        |row| row.get(0),
                    )
                    .optional()?;
                let current_revision = current.unwrap_or(0) as u64;
                if draft.expected_base_revision != current_revision {
                    anyhow::bail!(
                        "BASE_CONFIG_STALE: expected base revision {} but the saved desired revision is {}",
                        draft.expected_base_revision,
                        current_revision
                    );
                }
                let desired_revision = current_revision + 1;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let change = ConfigChange::from_draft(draft, desired_revision, commit);
                let entry = ConfigEntry {
                    schema_version: 1,
                    namespace: draft.namespace.clone(),
                    logical_key: draft.logical_key.clone(),
                    desired_revision,
                    desired_value_ref: draft.desired_patch_ref.clone(),
                    actor_and_authority_ref: draft.actor_and_authority_ref.clone(),
                    updated_commit: commit,
                };
                tx.execute(
                    "INSERT INTO config_entries(namespace,logical_key,desired_revision,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,0,?4,?5)
                     ON CONFLICT(namespace,logical_key) DO UPDATE SET
                       desired_revision=excluded.desired_revision,
                       record_revision=config_entries.record_revision+1,
                       data=excluded.data,
                       updated_commit_seq=excluded.updated_commit_seq",
                    params![
                        draft.namespace,
                        draft.logical_key,
                        desired_revision as i64,
                        serde_json::to_string(&entry)?,
                        seq.0 as i64,
                    ],
                )?;
                tx.execute(
                    "INSERT INTO config_changes(change_id,origin_identity,logical_key,namespace,record_revision,data,updated_commit_seq)
                     VALUES(?1,?2,?3,?4,0,?5,?6)",
                    params![
                        draft.change_id.to_string(),
                        draft.origin_identity,
                        draft.logical_key,
                        draft.namespace,
                        serde_json::to_string(&change)?,
                        seq.0 as i64,
                    ],
                )?;
                self.ledger.record_commit(&tx, seq, 2)?;
                tx.commit()?;
                Ok(change)
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// Record what a consumer actually adopted, and finish the change with its
    /// durable receipt. Adoption and the saved desired revision are separate
    /// facts: a consumer may still run an older revision.
    pub fn finish_config_change(
        &self,
        change_id: EntityId,
        state: &str,
        receipt: ConfigReceipt,
        adopted: Vec<ConfigAdoption>,
    ) -> Result<ConfigChange> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT data FROM config_changes WHERE change_id=?1",
                        [change_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(raw) = raw else {
                    anyhow::bail!("config change {change_id} is not committed");
                };
                let mut change: ConfigChange = serde_json::from_str(&raw)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                change.state = state.to_string();
                // The attempt is over: whatever happened, this row no longer
                // has an owner that could still be mid-apply.
                change.attempt = None;
                change.receipt = Some(receipt);
                change.adopted = adopted.clone();
                change.record_revision += 1;
                change.updated_commit = commit;
                tx.execute(
                    "UPDATE config_changes SET record_revision=?2, data=?3, updated_commit_seq=?4
                     WHERE change_id=?1",
                    params![
                        change_id.to_string(),
                        change.record_revision as i64,
                        serde_json::to_string(&change)?,
                        seq.0 as i64,
                    ],
                )?;
                for adoption in &adopted {
                    tx.execute(
                        "INSERT INTO config_adoptions(namespace,logical_key,consumer_id,adopted_revision,record_revision,data,updated_commit_seq)
                         VALUES(?1,?2,?3,?4,0,?5,?6)
                         ON CONFLICT(namespace,logical_key,consumer_id) DO UPDATE SET
                           adopted_revision=excluded.adopted_revision,
                           record_revision=config_adoptions.record_revision+1,
                           data=excluded.data,
                           updated_commit_seq=excluded.updated_commit_seq",
                        params![
                            change.namespace,
                            change.logical_key,
                            adoption.consumer_id,
                            adoption.adopted_revision as i64,
                            serde_json::to_string(adoption)?,
                            seq.0 as i64,
                        ],
                    )?;
                }
                self.ledger.record_commit(&tx, seq, 1 + adopted.len())?;
                tx.commit()?;
                Ok(change)
            },
            |error: &anyhow::Error| error.is::<rusqlite::Error>() || error.is::<serde_json::Error>(),
        )
    }

    /// Seal a JSON value into the object store under this installation's
    /// namespace. Used for actor/authority, patches and observations so a
    /// durable row references exact bytes instead of an inline blob.
    pub fn seal_config_object(
        &self,
        value: &Value,
        codec: &str,
    ) -> Result<ObjectRef> {
        let bytes = serde_json::to_vec(value)?;
        match self.cas.seal_object(&bytes, "application/json", "config", codec, 1) {
            Ok(sealed) => Ok(sealed.object),
            Err(error) => {
                self.close_external_writer();
                Err(error.into())
            }
        }
    }
}
