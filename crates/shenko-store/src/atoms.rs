//! 认知原子的账本实现：内容登记、态度行、理由指针 — SPEC §51.1, §51.3, §51.4, §51.9
//!
//! 这里只有一套储存：`cognition_contents` 登记 CAS 里的不可变内容，
//! `cognition_attitudes` 是只追加的态度行。没有任何领域可以在别处再建一份态度表，
//! 也没有第二套修订顺序。
//!
//! 三条硬约束：
//!
//! * 内容按摘要去重。字节没变就是同一版，不另开一版（[`Store::record_content_attitude`]）。
//! * 态度只追加。supersedes 指向上一行，不 UPDATE、不 DELETE。
//! * 置信从第一版就写，未知是显式的 [`AttitudeConfidence::Unknown`]；历史行没有记录
//!   置信时读成 [`ConfidenceState::Absent`]，不补默认数字，也不冒充已判断的未知。
//!
//! 外部效应的未知状态、任务、操作、回执、对账案例、发布结果、渠道事实都是它们自己的
//! 机制记录，**不**迁进这里；它们只能是一条态度的理由终点。

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use shenko_types::cognition::{
    AdoptionEvidence, AdoptionEvidenceVerifier, AdoptionTarget, AttitudeConfidence, AttitudeRole,
    ContentAdoption, ContentAttitude, ContentRecord, ContentRevisionDisclosure, ContentRevisionEntry,
    ProjectionStage, ReasonAdmission, ReasonAdmissionReport, ReasonPointer,
};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::object::{Digest, ObjectRef};

use crate::store::Store;

/// 内容封存的 CAS namespace。
pub const COGNITION_CONTENT_NAMESPACE: &str = "cognition";
/// 历史行原文回填时的 CAS namespace。它与新写入的内容分开，便于追溯来源。
pub const COGNITION_LEGACY_NAMESPACE: &str = "cognition-legacy";
/// 内容编解码器名。major 变化才表示同一摘要的读法不兼容。
pub const COGNITION_CONTENT_CODEC: &str = "shenko.cognition.content";
pub const COGNITION_CONTENT_CODEC_MAJOR: u32 = 1;

/// 认知原子表。只建自己的表，不改任何已有行（SPEC §51.1 加法迁移）。
pub const COGNITION_ATOMS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS cognition_contents (
    digest TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL,
    media_type TEXT NOT NULL,
    store_namespace TEXT NOT NULL,
    locator TEXT NOT NULL,
    codec_name TEXT NOT NULL,
    codec_major INTEGER NOT NULL,
    codec_digest TEXT NOT NULL,
    -- The exact typed reference. The archive closure walks these values to
    -- carry the content bytes, so a recovery archive never names content it
    -- cannot hand over (SPEC §51.1 immutable content, §5.4 recovery).
    object_json TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    registered_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Append-only attitude rows: this schema never updates or deletes one.
CREATE TABLE IF NOT EXISTS cognition_attitudes (
    attitude_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    digest TEXT NOT NULL REFERENCES cognition_contents(digest),
    role_kind TEXT NOT NULL,
    role_json TEXT NOT NULL,
    adoption_unit_id TEXT,
    definition_slot TEXT,
    supersedes_content TEXT,
    reasons_json TEXT NOT NULL,
    confidence_json TEXT,
    knowledge_kind_hint TEXT,
    supersedes TEXT,
    created_commit_ledger TEXT NOT NULL,
    created_commit_seq INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_cognition_attitudes_digest
    ON cognition_attitudes(agent_id, digest, created_commit_seq);
CREATE INDEX IF NOT EXISTS idx_cognition_attitudes_adoption
    ON cognition_attitudes(agent_id, adoption_unit_id, digest, created_commit_seq);
CREATE INDEX IF NOT EXISTS idx_cognition_attitudes_definition
    ON cognition_attitudes(agent_id, definition_slot, created_commit_seq);
CREATE INDEX IF NOT EXISTS idx_cognition_attitudes_revision
    ON cognition_attitudes(agent_id, supersedes_content, created_commit_seq);

-- 历史行的原文回填映射：只追加，只按 (source_table, source_key) 记一次。
-- 它引用 cognition_contents，不是第二份内容储存，也绝不改历史行的列或字节。
CREATE TABLE IF NOT EXISTS cognition_legacy_content (
    source_table TEXT NOT NULL,
    source_key TEXT NOT NULL,
    content_digest TEXT NOT NULL REFERENCES cognition_contents(digest),
    media_type TEXT NOT NULL,
    sealed_at_ms INTEGER NOT NULL,
    registered_commit_seq INTEGER NOT NULL,
    PRIMARY KEY (source_table, source_key)
);
"#;

/// 要封存的一版内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentWrite {
    pub bytes: Vec<u8>,
    pub media_type: String,
}

impl ContentWrite {
    pub fn text(text: &str) -> Self {
        Self {
            bytes: text.as_bytes().to_vec(),
            media_type: "text/plain".to_string(),
        }
    }

    pub fn new(bytes: Vec<u8>, media_type: impl Into<String>) -> Self {
        Self {
            bytes,
            media_type: media_type.into(),
        }
    }
}

/// 要写下的一条态度。
///
/// `confidence` 不是 `Option`：新写的态度从第一版就带置信，尚未判断时用
/// [`AttitudeConfidence::Unknown`]。读出来的行才是 `Option`，因为历史行可能从未记录过。
#[derive(Debug, Clone, PartialEq)]
pub struct AttitudeDraft {
    pub role: AttitudeRole,
    pub reasons: Vec<ReasonPointer>,
    pub confidence: AttitudeConfidence,
    pub knowledge_kind_hint: Option<String>,
    pub supersedes: Option<EntityId>,
}

impl AttitudeDraft {
    pub fn new(
        role: AttitudeRole,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Self {
        Self {
            role,
            reasons,
            confidence,
            knowledge_kind_hint: None,
            supersedes: None,
        }
    }

    pub fn assertion(reasons: Vec<ReasonPointer>, confidence: AttitudeConfidence) -> Self {
        Self::new(AttitudeRole::Assertion, reasons, confidence)
    }

    pub fn adoption(
        target: AdoptionTarget,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Self {
        Self::new(AttitudeRole::Adoption(target), reasons, confidence)
    }

    /// 一次修订。理由里必须已经指出被取代的那一版：修订是带理由的新内容，
    /// 不是悄悄换掉字节（SPEC §51.4）。缺少这条理由时这里就拒绝，不自造理由。
    pub fn revision(
        supersedes: Digest,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Result<Self, ShenkoError> {
        if !reasons.iter().any(|reason| reason.stops_at(&supersedes)) {
            return Err(ShenkoError::conflict(
                "a revision must carry a content reason pointing at the version it supersedes",
            ));
        }
        Ok(Self::new(
            AttitudeRole::Revision { supersedes },
            reasons,
            confidence,
        ))
    }

    pub fn definition(
        slot: impl Into<String>,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Self {
        Self::new(AttitudeRole::Definition(slot.into()), reasons, confidence)
    }

    pub fn projection(
        stage: ProjectionStage,
        reasons: Vec<ReasonPointer>,
        confidence: AttitudeConfidence,
    ) -> Self {
        Self::new(AttitudeRole::Projection(stage), reasons, confidence)
    }

    pub fn unfinished(reasons: Vec<ReasonPointer>, confidence: AttitudeConfidence) -> Self {
        Self::new(AttitudeRole::Unfinished, reasons, confidence)
    }

    pub fn effect_unknown(reasons: Vec<ReasonPointer>, confidence: AttitudeConfidence) -> Self {
        Self::new(AttitudeRole::EffectUnknown, reasons, confidence)
    }

    pub fn with_supersedes(mut self, attitude_id: EntityId) -> Self {
        self.supersedes = Some(attitude_id);
        self
    }

    pub fn with_kind_hint(mut self, hint: impl Into<String>) -> Self {
        self.knowledge_kind_hint = Some(hint.into());
        self
    }

    /// 写入前的机械校验。这里不判断内容的意义，只拒绝无法诚实落账的草稿。
    pub fn validate(&self) -> Result<(), ShenkoError> {
        if let AttitudeConfidence::Stated { value } = self.confidence {
            if !value.is_finite() {
                return Err(ShenkoError::conflict(
                    "attitude confidence must be a finite value",
                ));
            }
        }
        if let Some(target) = self.role.adoption_target() {
            if target.unit_id.trim().is_empty() {
                return Err(ShenkoError::conflict("adoption requires a unit id"));
            }
            if target.unit_id.trim_start().starts_with("cap.") {
                return Err(ShenkoError::conflict(
                    "adoption names a unit id; `cap.` names a capability contract (SPEC §51.8)",
                ));
            }
        }
        if let Some(slot) = self.role.definition_slot() {
            if slot.trim().is_empty() {
                return Err(ShenkoError::conflict("a definition requires a slot name"));
            }
        }
        if let Some(hint) = &self.knowledge_kind_hint {
            if hint.trim().is_empty() {
                return Err(ShenkoError::conflict(
                    "a knowledge kind hint is either absent or nonempty",
                ));
            }
        }
        Ok(())
    }
}

/// 一次写入的结果：态度行、它指向的内容、以及写下它的那条账本行。
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAttitude {
    pub attitude: ContentAttitude,
    pub content: ContentRecord,
    pub commit: CommitRef,
}

/// `cognition_attitudes` 的一行原文，解码前保持 SQL 里的确切文本。
struct RawAttitude {
    attitude_id: String,
    agent_id: String,
    digest: String,
    role_json: String,
    reasons_json: String,
    confidence_json: Option<String>,
    knowledge_kind_hint: Option<String>,
    supersedes: Option<String>,
    created_commit_ledger: String,
    created_commit_seq: i64,
    created_at_ms: i64,
}

const ATTITUDE_COLUMNS: &str = "attitude_id,agent_id,digest,role_json,reasons_json,\
     confidence_json,knowledge_kind_hint,supersedes,created_commit_ledger,\
     created_commit_seq,created_at_ms";

impl Store {
    /// 封存一版内容并写下一条态度。字节相同不会另开一版（按摘要去重）。
    pub fn record_content_attitude(
        &self,
        agent_id: EntityId,
        write: ContentWrite,
        draft: AttitudeDraft,
    ) -> Result<RecordedAttitude, ShenkoError> {
        let sealed = self.cas.seal_object(
            &write.bytes,
            &write.media_type,
            COGNITION_CONTENT_NAMESPACE,
            COGNITION_CONTENT_CODEC,
            COGNITION_CONTENT_CODEC_MAJOR,
        )?;
        self.record_attitude_for_digest(agent_id, sealed.object, draft)
    }

    /// 为已经在 CAS 里的一版内容写态度（例如内容就是某个证据对象的 raw_ref）。
    ///
    /// 它自开一条账本行。要在调用方已有的事务里追加，用
    /// [`Store::record_attitude_for_digest_tx`]。
    pub fn record_attitude_for_digest(
        &self,
        agent_id: EntityId,
        object: ObjectRef,
        draft: AttitudeDraft,
    ) -> Result<RecordedAttitude, ShenkoError> {
        self.verify_sealed_content(&object)?;
        draft.validate()?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(db_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let (content, attitude, inserted) =
                    self.insert_attitude(&tx, commit, agent_id, &object, &draft)?;
                // 一条账本行：内容登记（若新）与态度行，与其它写入共用同一行序。
                self.ledger.record_commit(&tx, seq, 1 + inserted)?;
                tx.commit().map_err(db_error)?;
                Ok(RecordedAttitude {
                    attitude,
                    content,
                    commit,
                })
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )
    }

    /// 在调用方**已有**的 IMMEDIATE 事务里写一条态度。
    ///
    /// 它不另开事务、不重新取变更锁、不分配新的账本行：调用方的 `current_commit`
    /// 就是这次写入的提交引用，所以决策、head 与态度要么一起提交，要么一起回滚。
    /// 调用方自己把这两行计入它的 `entity_count`。
    ///
    /// `object` 必须是本机已经封存、且长度与摘要相符的引用：这里只核对，不封存。
    pub fn record_attitude_for_digest_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        current_commit: CommitRef,
        agent_id: EntityId,
        object: ObjectRef,
        draft: AttitudeDraft,
    ) -> Result<ContentAttitude, ShenkoError> {
        self.verify_sealed_content(&object)?;
        draft.validate()?;
        let (_content, attitude, _inserted) =
            self.insert_attitude(tx, current_commit, agent_id, &object, &draft)?;
        Ok(attitude)
    }

    /// 只登记内容（机制登记，不产生态度，也不产生置信）。
    ///
    /// 用于把别处已经封存、已经由别的机制记录的内容纳入认知账本；重复调用按摘要去重。
    pub fn register_content(&self, object: &ObjectRef) -> Result<ContentRecord, ShenkoError> {
        self.verify_sealed_content(object)?;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(db_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let inserted = Self::insert_content_row(&tx, object, now_ms())?;
                let content = Self::read_content(&tx, &object.digest)?.ok_or_else(|| {
                    ShenkoError::storage("cognition content registration disappeared")
                })?;
                if inserted {
                    self.ledger.record_commit(&tx, seq, 1)?;
                }
                tx.commit().map_err(db_error)?;
                Ok(content)
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )
    }

    /// 把历史行里的原文封存进 CAS，并只追加一条映射记录。
    ///
    /// 历史行自己的列和字节一个字都不改：不可变行的归档语义（整行/冲突单元）因此保持
    /// 原样。重复回填同一 `(source_table, source_key)` 是核对后的空操作；已有的映射
    /// 指向别的摘要时拒绝覆盖。
    pub fn register_legacy_content(
        &self,
        source_table: &str,
        source_key: &str,
        bytes: &[u8],
        media_type: &str,
    ) -> Result<(ContentRecord, CommitRef), ShenkoError> {
        if source_table.trim().is_empty() || source_key.is_empty() {
            return Err(ShenkoError::conflict(
                "legacy content mapping requires a source table and key",
            ));
        }
        let sealed = self.cas.seal_object(
            bytes,
            media_type,
            COGNITION_LEGACY_NAMESPACE,
            COGNITION_CONTENT_CODEC,
            COGNITION_CONTENT_CODEC_MAJOR,
        )?;
        let object = sealed.object;
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(db_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT content_digest FROM cognition_legacy_content
                          WHERE source_table=?1 AND source_key=?2",
                        params![source_table, source_key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db_error)?;
                if let Some(existing) = previous {
                    if existing != object.digest.0 {
                        return Err(ShenkoError::conflict(
                            "legacy content mapping already names a different digest",
                        ));
                    }
                }
                let inserted = Self::insert_content_row(&tx, &object, now_ms())?;
                let mapped = tx.execute(
                    "INSERT OR IGNORE INTO cognition_legacy_content
                        (source_table,source_key,content_digest,media_type,sealed_at_ms,registered_commit_seq)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        source_table,
                        source_key,
                        object.digest.0,
                        object.media_type,
                        now_ms(),
                        seq.0 as i64,
                    ],
                )?;
                let content = Self::read_content(&tx, &object.digest)?.ok_or_else(|| {
                    ShenkoError::storage("legacy content registration disappeared")
                })?;
                if inserted + mapped == 0 {
                    // 已经回填过并且核对一致：这是空操作，不新开账本行。
                    // 交回当初那条映射记录的提交引用，不冒充一条新提交。
                    let recorded: i64 = tx
                        .query_row(
                            "SELECT registered_commit_seq FROM cognition_legacy_content
                              WHERE source_table=?1 AND source_key=?2",
                            params![source_table, source_key],
                            |row| row.get(0),
                        )
                        .map_err(db_error)?;
                    tx.commit().map_err(db_error)?;
                    return Ok((
                        content,
                        CommitRef::new(
                            self.ledger.ledger_id(),
                            shenko_types::ids::LedgerSeq(recorded as u64),
                        ),
                    ));
                }
                self.ledger.record_commit(&tx, seq, inserted + mapped)?;
                tx.commit().map_err(db_error)?;
                Ok((content, commit))
            },
            |error: &ShenkoError| {
                matches!(
                    error,
                    ShenkoError::Io(_) | ShenkoError::Serde(_) | ShenkoError::Storage(_)
                )
            },
        )
    }

    /// 已登记内容的一行。
    pub fn content(&self, digest: &Digest) -> Result<Option<ContentRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        Self::read_content(&conn, digest)
    }

    /// 内容的原始字节。内容不可变，所以这永远是当初写下的字节。
    pub fn content_bytes(&self, digest: &Digest) -> Result<Vec<u8>, ShenkoError> {
        if self.content(digest)?.is_none() {
            return Err(ShenkoError::not_found(format!(
                "cognition content {digest} is not registered"
            )));
        }
        self.cas.read_object(digest)
    }

    /// 历史行原文的映射（如果回填过）。
    pub fn legacy_content(
        &self,
        source_table: &str,
        source_key: &str,
    ) -> Result<Option<ContentRecord>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let digest: Option<String> = conn
            .query_row(
                "SELECT content_digest FROM cognition_legacy_content
                  WHERE source_table=?1 AND source_key=?2",
                params![source_table, source_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        match digest {
            None => Ok(None),
            Some(digest) => {
                let digest = parse_digest(&digest)?;
                Self::read_content(&conn, &digest)
            }
        }
    }

    /// 一条态度行。
    pub fn attitude(&self, attitude_id: EntityId) -> Result<Option<ContentAttitude>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!("SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes WHERE attitude_id=?1");
        let raw = conn
            .query_row(&sql, params![attitude_id.to_string()], raw_attitude)
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        raw.map(decode_attitude).transpose()
    }

    /// 某个主体对某一版内容写下的全部态度，按账本顺序（旧→新）。
    pub fn attitudes_for(
        &self,
        agent_id: EntityId,
        digest: &Digest,
    ) -> Result<Vec<ContentAttitude>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND digest=?2
              ORDER BY created_commit_seq ASC, attitude_id ASC"
        );
        let mut statement = conn
            .prepare(&sql)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let raws = statement
            .query_map(params![agent_id.to_string(), digest.0], raw_attitude)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        raws.into_iter().map(decode_attitude).collect()
    }

    /// 最新的一条态度；`role_kind` 给出时只看那个机制角色（如 `projection`）。
    ///
    /// 返回的 [`ContentAttitude`] 带完整的 `role`，调用方自己从里面取阶段或目标。
    pub fn latest_attitude(
        &self,
        agent_id: EntityId,
        digest: &Digest,
        role_kind: Option<&str>,
    ) -> Result<Option<ContentAttitude>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND digest=?2 AND (?3 IS NULL OR role_kind=?3)
              ORDER BY created_commit_seq DESC, attitude_id DESC LIMIT 1"
        );
        let raw = conn
            .query_row(&sql, params![agent_id.to_string(), digest.0, role_kind], raw_attitude)
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        raw.map(decode_attitude).transpose()
    }

    /// 某个单元对某一版内容的最新采纳投影。
    ///
    /// 未通过验证者时证据一律是 [`AdoptionEvidence::Undetermined`]：采纳态度存在是
    /// 账本事实，证据是否被本机核实是另一件事。要拿到核实裁决用
    /// [`Store::adoption_for_verified`]。
    ///
    /// 只看**本机账本**写下的采纳（`created_commit.ledger_id` 是本机的）。从别的节点
    /// 导入的采纳保留为那个节点的记录：复制成功不等于本机已采纳，更不等于本机已启用
    /// 该单元或执行过它的试验（SPEC §51.8）。
    pub fn adoption_for(
        &self,
        agent_id: EntityId,
        unit_id: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>, ShenkoError> {
        self.adoption_projection(agent_id, unit_id, digest)
    }

    /// 用领域自己的验证者取采纳投影。只有真正拥有本地试验/观察验证的领域能返回
    /// `Some(true/false)`；`None` 保持未判定。
    pub fn adoption_for_verified(
        &self,
        verifier: &dyn AdoptionEvidenceVerifier,
        agent_id: EntityId,
        unit_id: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>, ShenkoError> {
        match self.adoption_projection(agent_id, unit_id, digest)? {
            None => Ok(None),
            Some(mut adoption) => {
                let decision = verifier.verify_adoption(agent_id, digest, &reasons_of(&adoption))?;
                adoption.evidence = match decision {
                    Some(true) => AdoptionEvidence::VerifiedLocal,
                    Some(false) => AdoptionEvidence::RefutedLocal,
                    None => AdoptionEvidence::Undetermined {
                        detail: "the verifying domain could not decide locally".to_string(),
                    },
                };
                Ok(Some(adoption))
            }
        }
    }

    /// 是否有采纳态度（不表示证据已核实，也不表示本机已启用该单元）。
    pub fn is_adopted(
        &self,
        agent_id: EntityId,
        unit_id: &str,
        digest: &Digest,
    ) -> Result<bool, ShenkoError> {
        Ok(self.adoption_for(agent_id, unit_id, digest)?.is_some())
    }

    /// 每条理由在本机的可核性。它不判断这条内容是否成立。
    pub fn reason_admission(
        &self,
        reasons: &[ReasonPointer],
    ) -> Result<Vec<ReasonAdmissionReport>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut reports = Vec::with_capacity(reasons.len());
        for reason in reasons {
            let admission = Self::admit_reason(&conn, self, reason)?;
            reports.push(ReasonAdmissionReport {
                reason: reason.clone(),
                admission,
            });
        }
        Ok(reports)
    }

    /// 一版内容之前的修订链，从新到旧。每一项是一次修订动作（引入的那一版）。
    pub fn revision_chain(
        &self,
        agent_id: EntityId,
        digest: &Digest,
    ) -> Result<Vec<ContentRevisionEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND digest=?2 AND role_kind='revision'
              ORDER BY created_commit_seq DESC, attitude_id DESC LIMIT 1"
        );
        let mut chain = Vec::new();
        let mut current = digest.clone();
        // A corrupt chain that names a cycle must not hang a reader; the walk
        // stops at a bounded depth and reports only what it actually walked.
        for _ in 0..4096 {
            let raw: Option<RawAttitude> = conn
                .query_row(&sql, params![agent_id.to_string(), current.0], raw_attitude)
                .optional()
                .map_err(|error| ShenkoError::storage(error.to_string()))?;
            let Some(raw) = raw else { break };
            let attitude = decode_attitude(raw)?;
            let Some(parent) = attitude.role.revision_parent().cloned() else {
                break;
            };
            chain.push(ContentRevisionEntry {
                digest: attitude.digest.clone(),
                attitude_id: attitude.attitude_id,
                created_at_ms: attitude.created_at_ms,
            });
            current = parent;
        }
        Ok(chain)
    }

    /// 统一的修订露出：旧版本数量与最新修改时间。全文要按版本显式读。
    ///
    /// 这里只给事实。默认露出多少、按什么顺序，归已安装的 `taste` 宏包（SPEC §51.1）。
    pub fn revision_disclosure(
        &self,
        agent_id: EntityId,
        digest: &Digest,
    ) -> Result<ContentRevisionDisclosure, ShenkoError> {
        let content = self.content(digest)?.ok_or_else(|| {
            ShenkoError::not_found(format!("cognition content {digest} is not registered"))
        })?;
        let chain = self.revision_chain(agent_id, digest)?;
        let latest_modified_at_ms = chain
            .iter()
            .map(|entry| entry.created_at_ms)
            .max()
            .unwrap_or(content.registered_at_ms);
        Ok(ContentRevisionDisclosure {
            latest_digest: digest.clone(),
            old_version_count: chain.len() as u32,
            latest_modified_at_ms,
        })
    }

    /// 某个定义槽当前有效的那一版定义：内容 + 写下它的态度。
    ///
    /// 旧内容按这一版定义重新理解；旧定义仍在 [`Store::definition_history`] 里可按
    /// 版本读到，字节不改写（SPEC §51.4）。
    pub fn effective_definition(
        &self,
        agent_id: EntityId,
        slot: &str,
    ) -> Result<Option<(ContentRecord, ContentAttitude)>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND definition_slot=?2 AND role_kind='definition'
              ORDER BY created_commit_seq DESC, attitude_id DESC LIMIT 1"
        );
        let raw: Option<RawAttitude> = conn
            .query_row(&sql, params![agent_id.to_string(), slot], raw_attitude)
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let Some(raw) = raw else { return Ok(None) };
        let attitude = decode_attitude(raw)?;
        let Some(content) = Self::read_content(&conn, &attitude.digest)? else {
            return Ok(None);
        };
        Ok(Some((content, attitude)))
    }

    /// 一个定义槽的全部历史版本，按账本顺序（旧→新）。
    pub fn definition_history(
        &self,
        agent_id: EntityId,
        slot: &str,
    ) -> Result<Vec<ContentAttitude>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND definition_slot=?2 AND role_kind='definition'
              ORDER BY created_commit_seq ASC, attitude_id ASC"
        );
        let mut statement = conn
            .prepare(&sql)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let raws = statement
            .query_map(params![agent_id.to_string(), slot], raw_attitude)
            .and_then(|rows| rows.collect::<rusqlite::Result<Vec<_>>>())
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        raws.into_iter().map(decode_attitude).collect()
    }

    // ---- internals -------------------------------------------------------

    fn adoption_projection(
        &self,
        agent_id: EntityId,
        unit_id: &str,
        digest: &Digest,
    ) -> Result<Option<ContentAdoption>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let sql = format!(
            "SELECT {ATTITUDE_COLUMNS} FROM cognition_attitudes
              WHERE agent_id=?1 AND adoption_unit_id=?2 AND digest=?3 AND role_kind='adoption'
                AND created_commit_ledger=?4
              ORDER BY created_commit_seq DESC, attitude_id DESC LIMIT 1"
        );
        let raw: Option<RawAttitude> = conn
            .query_row(
                &sql,
                params![
                    agent_id.to_string(),
                    unit_id,
                    digest.0,
                    self.ledger.ledger_id().to_string()
                ],
                raw_attitude,
            )
            .optional()
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let Some(raw) = raw else { return Ok(None) };
        let attitude = decode_attitude(raw)?;
        drop(conn);
        let reasons = self.reason_admission(&attitude.reasons)?;
        Ok(Some(ContentAdoption {
            attitude_id: attitude.attitude_id,
            unit_id: unit_id.to_string(),
            content_digest: attitude.digest.clone(),
            adopted: true,
            confidence: attitude.confidence_state(),
            reasons,
            evidence: AdoptionEvidence::Undetermined {
                detail: "no local verifier was consulted for this adoption".to_string(),
            },
            recorded_commit: attitude.created_commit,
            recorded_at_ms: attitude.created_at_ms,
        }))
    }

    fn admit_reason(
        conn: &Connection,
        store: &Store,
        reason: &ReasonPointer,
    ) -> Result<ReasonAdmission, ShenkoError> {
        use shenko_types::cognition::ReasonTermination;
        let db = |error: rusqlite::Error| ShenkoError::storage(error.to_string());
        match &reason.termination {
            ReasonTermination::Observation(observation) => {
                let in_session: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM session_events WHERE event_id=?1",
                        params![observation.event_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db)?;
                let in_inbox: Option<i64> = conn
                    .query_row(
                        "SELECT 1 FROM inbox_events WHERE event_id=?1",
                        params![observation.event_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db)?;
                if in_session.is_none() && in_inbox.is_none() {
                    return Ok(ReasonAdmission::AbsentLocal {
                        detail: "the observation event is not in this installation's session or inbox events".to_string(),
                    });
                }
                if let Some(receipt_id) = observation.receipt_id {
                    let receipt: Option<i64> = conn
                        .query_row(
                            "SELECT 1 FROM evidence_receipts WHERE receipt_id=?1",
                            params![receipt_id.to_string()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(db)?;
                    if receipt.is_none() {
                        return Ok(ReasonAdmission::DetailMismatch {
                            detail: format!(
                                "observation names receipt {receipt_id}, which this installation does not hold"
                            ),
                        });
                    }
                }
                Ok(ReasonAdmission::PresentLocal)
            }
            ReasonTermination::Source(source) => {
                if store.cas.object_exists(&source.digest) {
                    Ok(ReasonAdmission::PresentLocal)
                } else {
                    Ok(ReasonAdmission::AbsentLocal {
                        detail: match &source.locator {
                            Some(locator) => format!(
                                "source object {} is not in the local CAS (locator {locator} is external or remote)",
                                source.digest
                            ),
                            None => format!(
                                "source object {} is not in the local CAS",
                                source.digest
                            ),
                        },
                    })
                }
            }
            ReasonTermination::Receipt(receipt) => {
                let data: Option<String> = conn
                    .query_row(
                        "SELECT data FROM evidence_receipts WHERE receipt_id=?1",
                        params![receipt.receipt_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(db)?;
                let Some(data) = data else {
                    return Ok(ReasonAdmission::AbsentLocal {
                        detail: format!(
                            "evidence receipt {} is not in this installation (foreign or not yet received)",
                            receipt.receipt_id
                        ),
                    });
                };
                if let Some(level) = receipt.observation_level {
                    let stored: Option<String> = serde_json::from_str::<serde_json::Value>(&data)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("observation_level")
                                .and_then(|level| level.as_str())
                                .map(str::to_owned)
                        });
                    let wanted = serde_json::to_value(level)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned));
                    if stored != wanted {
                        return Ok(ReasonAdmission::DetailMismatch {
                            detail: format!(
                                "receipt {} records observation level {:?}, not {:?}",
                                receipt.receipt_id, stored, wanted
                            ),
                        });
                    }
                }
                Ok(ReasonAdmission::PresentLocal)
            }
            ReasonTermination::Reconciliation(case) => Ok(ReasonAdmission::AbsentLocal {
                detail: format!(
                    "reconciliation case {} is a mechanism reference owned by the effect ledger; this store keeps no case row to resolve it",
                    case.case_id
                ),
            }),
            ReasonTermination::Content(digest) => {
                if Self::read_content(conn, digest)?.is_some() {
                    Ok(ReasonAdmission::PresentLocal)
                } else if store.cas.object_exists(digest) {
                    Ok(ReasonAdmission::DetailMismatch {
                        detail: format!(
                            "object {digest} is sealed but not registered as cognition content"
                        ),
                    })
                } else {
                    Ok(ReasonAdmission::AbsentLocal {
                        detail: format!("content {digest} is not held by this installation"),
                    })
                }
            }
            ReasonTermination::Perspective(perspective) => {
                if Self::read_content(conn, &perspective.understanding)?.is_none() {
                    return Ok(ReasonAdmission::AbsentLocal {
                        detail: format!(
                            "perspective understanding {} is not registered content here",
                            perspective.understanding
                        ),
                    });
                }
                if let Some(perception) = &perspective.perception {
                    if Self::read_content(conn, perception)?.is_none() {
                        return Ok(ReasonAdmission::DetailMismatch {
                            detail: format!(
                                "perspective names perception {perception}, which is not registered content here"
                            ),
                        });
                    }
                }
                Ok(ReasonAdmission::PresentLocal)
            }
        }
    }

    fn verify_sealed_content(&self, object: &ObjectRef) -> Result<(), ShenkoError> {
        object
            .digest
            .canonical_blake3_hex()
            .ok_or_else(|| {
                ShenkoError::conflict(
                    "cognition content digest must be canonical blake3 lowercase hex",
                )
            })?;
        let size = self.cas.object_size(&object.digest).ok_or_else(|| {
            ShenkoError::not_found(format!(
                "cognition content {} is not sealed in this CAS",
                object.digest
            ))
        })?;
        if size != object.bytes {
            return Err(ShenkoError::conflict(format!(
                "cognition content {} is {size} bytes, the reference claims {}",
                object.digest, object.bytes
            )));
        }
        Ok(())
    }

    /// 内容登记（按摘要去重）+ 态度行。返回 (内容, 态度, 是否新登记内容)。
    fn insert_attitude(
        &self,
        tx: &rusqlite::Transaction<'_>,
        current_commit: CommitRef,
        agent_id: EntityId,
        object: &ObjectRef,
        draft: &AttitudeDraft,
    ) -> Result<(ContentRecord, ContentAttitude, usize), ShenkoError> {
        let now = now_ms();
        let inserted = Self::insert_content_row(tx, object, now)?;
        if let Some(parent) = draft.role.revision_parent() {
            if parent == &object.digest {
                return Err(ShenkoError::conflict(
                    "unchanged bytes do not open a new revision (SPEC §51.1)",
                ));
            }
            if Self::read_content(tx, parent)?.is_none() {
                return Err(ShenkoError::not_found(format!(
                    "revision parent {parent} is not registered cognition content"
                )));
            }
        }
        let attitude = ContentAttitude {
            attitude_id: EntityId::new(),
            agent_id,
            digest: object.digest.clone(),
            role: draft.role.clone(),
            reasons: draft.reasons.clone(),
            // 新写的态度总是显式带置信：尚未判断写 Unknown，不留空。
            confidence: Some(draft.confidence),
            knowledge_kind_hint: draft.knowledge_kind_hint.clone(),
            supersedes: draft.supersedes,
            created_commit: current_commit,
            created_at_ms: now,
        };
        tx.execute(
            "INSERT INTO cognition_attitudes
                (attitude_id,agent_id,digest,role_kind,role_json,adoption_unit_id,
                 definition_slot,supersedes_content,reasons_json,confidence_json,
                 knowledge_kind_hint,supersedes,created_commit_ledger,created_commit_seq,
                 created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
            params![
                attitude.attitude_id.to_string(),
                agent_id.to_string(),
                object.digest.0,
                attitude.role.kind(),
                serde_json::to_string(&attitude.role)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?,
                attitude
                    .role
                    .adoption_target()
                    .map(|target| target.unit_id.clone()),
                attitude.role.definition_slot().map(str::to_owned),
                attitude.role.revision_parent().map(|parent| parent.0.clone()),
                serde_json::to_string(&attitude.reasons)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?,
                serde_json::to_string(&draft.confidence)
                    .map_err(|error| ShenkoError::storage(error.to_string()))?,
                attitude.knowledge_kind_hint,
                attitude.supersedes.map(|id| id.to_string()),
                current_commit.ledger_id.to_string(),
                current_commit.seq.0 as i64,
                now,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))?;
        let content = Self::read_content(tx, &object.digest)?
            .ok_or_else(|| ShenkoError::storage("cognition content registration disappeared"))?;
        Ok((content, attitude, inserted))
    }

    fn insert_content_row(
        tx: &rusqlite::Transaction<'_>,
        object: &ObjectRef,
        now: i64,
    ) -> Result<usize, ShenkoError> {
        let object_json = serde_json::to_string(object)
            .map_err(|error| ShenkoError::storage(error.to_string()))?;
        tx.execute(
            "INSERT OR IGNORE INTO cognition_contents
                (digest,bytes,media_type,store_namespace,locator,codec_name,codec_major,
                 codec_digest,object_json,registered_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                object.digest.0,
                object.bytes as i64,
                object.media_type,
                object.store_namespace,
                object.locator,
                object.codec.name,
                object.codec.major as i64,
                object.codec.digest.0,
                object_json,
                now,
            ],
        )
        .map_err(|error| ShenkoError::storage(error.to_string()))
    }

    fn read_content(
        conn: &Connection,
        digest: &Digest,
    ) -> Result<Option<ContentRecord>, ShenkoError> {
        conn.query_row(
            "SELECT digest,bytes,media_type,store_namespace,locator,codec_name,codec_major,
                    codec_digest,registered_at_ms
               FROM cognition_contents WHERE digest=?1",
            params![digest.0],
            |row| {
                Ok(ContentRecord {
                    object: ObjectRef {
                        digest: Digest(row.get::<_, String>(0)?),
                        bytes: row.get::<_, i64>(1)? as u64,
                        media_type: row.get(2)?,
                        store_namespace: row.get(3)?,
                        locator: row.get(4)?,
                        codec: shenko_types::object::SchemaRef {
                            name: row.get(5)?,
                            major: row.get::<_, i64>(6)? as u32,
                            digest: Digest(row.get::<_, String>(7)?),
                        },
                    },
                    registered_at_ms: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(|error| ShenkoError::storage(error.to_string()))
    }
}

fn raw_attitude(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAttitude> {
    Ok(RawAttitude {
        attitude_id: row.get(0)?,
        agent_id: row.get(1)?,
        digest: row.get(2)?,
        role_json: row.get(3)?,
        reasons_json: row.get(4)?,
        confidence_json: row.get(5)?,
        knowledge_kind_hint: row.get(6)?,
        supersedes: row.get(7)?,
        created_commit_ledger: row.get(8)?,
        created_commit_seq: row.get(9)?,
        created_at_ms: row.get(10)?,
    })
}

fn decode_attitude(raw: RawAttitude) -> Result<ContentAttitude, ShenkoError> {
    let attitude_id = parse_id(&raw.attitude_id, "attitude id")?;
    let agent_id = parse_id(&raw.agent_id, "attitude agent")?;
    let role: AttitudeRole =
        serde_json::from_str(&raw.role_json).map_err(|error| {
            ShenkoError::storage(format!("cognition attitude role: {error}"))
        })?;
    let reasons: Vec<ReasonPointer> = serde_json::from_str(&raw.reasons_json)
        .map_err(|error| ShenkoError::storage(format!("cognition attitude reasons: {error}")))?;
    // NULL 置信列就是「没有记录」：不补 0.0，也不补一个假的未知。
    let confidence: Option<AttitudeConfidence> = match raw.confidence_json.as_deref() {
        None => None,
        Some(text) => Some(serde_json::from_str(text).map_err(|error| {
            ShenkoError::storage(format!("cognition attitude confidence: {error}"))
        })?),
    };
    let supersedes = match raw.supersedes.as_deref() {
        None => None,
        Some(id) => Some(parse_id(id, "superseded attitude")?),
    };
    let created_commit = CommitRef::new(
        parse_id(&raw.created_commit_ledger, "commit ledger")?,
        shenko_types::ids::LedgerSeq(raw.created_commit_seq as u64),
    );
    Ok(ContentAttitude {
        attitude_id,
        agent_id,
        digest: Digest(raw.digest),
        role,
        reasons,
        confidence,
        knowledge_kind_hint: raw.knowledge_kind_hint,
        supersedes,
        created_commit,
        created_at_ms: raw.created_at_ms,
    })
}

/// 采纳投影里的核验报告要交回给验证者看理由，不是看布尔。
fn reasons_of(adoption: &ContentAdoption) -> Vec<ReasonPointer> {
    adoption
        .reasons
        .iter()
        .map(|report| report.reason.clone())
        .collect()
}

fn parse_id(text: &str, what: &str) -> Result<EntityId, ShenkoError> {
    text.parse::<EntityId>()
        .map_err(|error| ShenkoError::storage(format!("cognition {what}: {error}")))
}

fn parse_digest(text: &str) -> Result<Digest, ShenkoError> {
    let digest = Digest(text.to_string());
    digest
        .canonical_blake3_hex()
        .ok_or_else(|| ShenkoError::storage("cognition digest is not canonical blake3 hex"))?;
    Ok(digest)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// SQLite 失败在这里仍是存储失败，不升级成协议事实。
fn db_error(error: rusqlite::Error) -> ShenkoError {
    ShenkoError::storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use shenko_types::cognition::{ConfidenceState, ReasonReading};

    fn store(name: &str) -> (std::path::PathBuf, Store) {
        let root = std::env::temp_dir().join(format!("shenko-atoms-{name}-{}", EntityId::new()));
        std::fs::create_dir_all(&root).unwrap();
        let store = Store::open(root.join("store.db"), root.join("cas")).unwrap();
        (root, store)
    }

    #[test]
    fn identical_bytes_do_not_open_a_revision() {
        let (_root, store) = store("dedupe");
        let agent = EntityId::new();
        let first = store
            .record_content_attitude(
                agent,
                ContentWrite::text("同一个字节"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let second = store
            .record_content_attitude(
                agent,
                ContentWrite::text("同一个字节"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        assert_eq!(first.content.digest(), second.content.digest());
        // 两个字节相同的登记都指向同一版内容；没有多出内容行。
        assert_eq!(first.content.registered_at_ms, second.content.registered_at_ms);
        let revision = AttitudeDraft::revision(
            first.content.digest().clone(),
            vec![ReasonPointer::content(first.content.digest().clone())],
            AttitudeConfidence::unknown(),
        );
        assert!(revision.is_ok());
        // 修订必须给出旧版本作为理由：没有指向旧 digest 的 Content 指针就拒绝。
        assert!(AttitudeDraft::revision(
            first.content.digest().clone(),
            vec![ReasonPointer::observation(EntityId::new(), None)],
            AttitudeConfidence::unknown(),
        )
        .is_err());
    }

    #[test]
    fn unknown_and_absent_confidence_stay_distinct() {
        let (_root, store) = store("confidence");
        let agent = EntityId::new();
        let recorded = store
            .record_content_attitude(
                agent,
                ContentWrite::text("尚未判断的一条"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let read = store.attitude(recorded.attitude.attitude_id).unwrap().unwrap();
        assert_eq!(read.confidence, Some(AttitudeConfidence::Unknown));
        assert_eq!(read.confidence_state(), ConfidenceState::Unknown);
        // 历史行的 NULL 列读作 Absent，不冒称已判断的未知。
        assert_eq!(ConfidenceState::of(None), ConfidenceState::Absent);
    }

    #[test]
    fn adoption_is_explicit_and_evidence_is_never_inferred() {
        let (_root, store) = store("adoption");
        let agent = EntityId::new();
        let content = store
            .record_content_attitude(
                agent,
                ContentWrite::text("宏包里的内容单元"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let digest = content.content.digest().clone();
        assert!(!store.is_adopted(agent, "humanlike-mind", &digest).unwrap());

        store
            .record_attitude_for_digest(
                agent,
                content.content.object.clone(),
                AttitudeDraft::adoption(
                    AdoptionTarget::unit("humanlike-mind"),
                    vec![ReasonPointer::observation(EntityId::new(), None)],
                    AttitudeConfidence::unknown(),
                ),
            )
            .unwrap();
        assert!(store.is_adopted(agent, "humanlike-mind", &digest).unwrap());

        // 采纳存在≠证据已核实：没有验证者时保持未判定。
        let unverified = store.adoption_for(agent, "humanlike-mind", &digest).unwrap().unwrap();
        assert!(unverified.adopted);
        assert!(matches!(
            unverified.evidence,
            AdoptionEvidence::Undetermined { .. }
        ));
        // 理由指向不存在的观察事件：如实记为缺席，不冒充本机证据。
        assert!(matches!(
            unverified.reasons[0].admission,
            ReasonAdmission::AbsentLocal { .. }
        ));

        // 能力合同标识不得当单元 id 用（§51.8）。
        assert!(store
            .record_attitude_for_digest(
                agent,
                content.content.object.clone(),
                AttitudeDraft::adoption(
                    AdoptionTarget::unit("cap.forgetting"),
                    vec![],
                    AttitudeConfidence::unknown(),
                ),
            )
            .is_err());
    }

    #[test]
    fn definition_revision_keeps_old_versions_readable() {
        let (_root, store) = store("definition");
        let agent = EntityId::new();
        let first = store
            .record_content_attitude(
                agent,
                ContentWrite::text("fact 意味着：停下来的理由"),
                AttitudeDraft::definition("fact.meaning", vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let second = store
            .record_content_attitude(
                agent,
                ContentWrite::text("fact 意味着：理由停在自己观察到的原件上"),
                AttitudeDraft::definition("fact.meaning", vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let (content, attitude) = store.effective_definition(agent, "fact.meaning").unwrap().unwrap();
        assert_eq!(content.digest(), second.content.digest());
        assert_eq!(attitude.attitude_id, second.attitude.attitude_id);
        let history = store.definition_history(agent, "fact.meaning").unwrap();
        assert_eq!(history.len(), 2);
        // 旧定义字节仍在，可按版本读到。
        assert_eq!(
            store.content_bytes(first.content.digest()).unwrap(),
            "fact 意味着：停下来的理由".as_bytes().to_vec()
        );
    }

    #[test]
    fn revision_disclosure_counts_old_versions() {
        let (_root, store) = store("disclosure");
        let agent = EntityId::new();
        let base = store
            .record_content_attitude(
                agent,
                ContentWrite::text("第一版"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let second = store
            .record_content_attitude(
                agent,
                ContentWrite::text("第二版"),
                AttitudeDraft::revision(
                    base.content.digest().clone(),
                    vec![ReasonPointer::content(base.content.digest().clone())],
                    AttitudeConfidence::unknown(),
                )
                .unwrap(),
            )
            .unwrap();
        let third = store
            .record_content_attitude(
                agent,
                ContentWrite::text("第三版"),
                AttitudeDraft::revision(
                    second.content.digest().clone(),
                    vec![ReasonPointer::content(second.content.digest().clone())],
                    AttitudeConfidence::unknown(),
                )
                .unwrap(),
            )
            .unwrap();
        let disclosure = store
            .revision_disclosure(agent, third.content.digest())
            .unwrap();
        assert_eq!(disclosure.latest_digest, *third.content.digest());
        assert_eq!(disclosure.old_version_count, 2);
        assert_eq!(
            disclosure.latest_modified_at_ms,
            third.attitude.created_at_ms
        );
        let chain = store.revision_chain(agent, third.content.digest()).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].digest, *third.content.digest());
        assert_eq!(chain[1].digest, *second.content.digest());
        // 原文一版都没有被改写。
        assert_eq!(
            store.content_bytes(base.content.digest()).unwrap(),
            "第一版".as_bytes().to_vec()
        );
    }

    #[test]
    fn legacy_inline_bytes_are_mapped_without_touching_the_row() {
        let (_root, store) = store("legacy");
        let (content, commit) = store
            .register_legacy_content("legacy_notes", "note-1", b"inline body", "text/plain")
            .unwrap();
        assert!(commit.seq.0 > 0);
        assert_eq!(
            store.legacy_content("legacy_notes", "note-1").unwrap().unwrap().digest(),
            content.digest()
        );
        // 重复回填是核对后的空操作，且不会覆盖成别的摘要。
        let (again, _) = store
            .register_legacy_content("legacy_notes", "note-1", b"inline body", "text/plain")
            .unwrap();
        assert_eq!(again.digest(), content.digest());
        // 同一个历史键已经指向别的摘要时拒绝覆盖，绝不改写已有的原始字节。
        assert!(store
            .register_legacy_content("legacy_notes", "note-1", b"different", "text/plain")
            .is_err());
        // 映射不产生态度，也不产生置信。
        assert!(store.attitudes_for(EntityId::new(), content.digest()).unwrap().is_empty());
        assert_eq!(
            store.content_bytes(content.digest()).unwrap(),
            b"inline body".to_vec()
        );
    }

    #[test]
    fn reading_comes_from_reason_termination_not_a_stored_kind() {
        let (_root, store) = store("reading");
        let agent = EntityId::new();
        let original = store
            .record_content_attitude(
                agent,
                ContentWrite::text("原始材料"),
                AttitudeDraft::assertion(vec![], AttitudeConfidence::unknown()),
            )
            .unwrap();
        let observed = store
            .record_content_attitude(
                agent,
                ContentWrite::text("我看到的那次读数"),
                AttitudeDraft::assertion(
                    vec![ReasonPointer::source(original.content.digest().clone(), None)],
                    AttitudeConfidence::unknown(),
                ),
            )
            .unwrap();
        assert_eq!(observed.attitude.reading(), ReasonReading::Fact);

        let inferred = store
            .record_content_attitude(
                agent,
                ContentWrite::text("由此推出来的结论"),
                AttitudeDraft::assertion(
                    vec![ReasonPointer::content(observed.content.digest().clone())],
                    AttitudeConfidence::unknown(),
                )
                .with_kind_hint("insight"),
            )
            .unwrap();
        assert_eq!(inferred.attitude.reading(), ReasonReading::Conclusion);
        // knowledge_kind 只是提示，不参与读法。
        assert_eq!(
            inferred.attitude.knowledge_kind_hint.as_deref(),
            Some("insight")
        );
    }
}
