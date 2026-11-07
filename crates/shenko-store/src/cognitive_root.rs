//! 自有版本化根的账本实现 — SPEC §51.4、§10.1、§10.2.1
//!
//! 认知根的版本链只增不减：每次捕获或回退都在链尾追加一个新节点，
//! 把当前权威根推进到新版本。回退不倒回指针，而是追加一个内容与历史
//! 节点相同的新节点，按内容寻址复用已有对象。
//!
//! 写入被 [`RootFence`] 围栏住：代际、控制器 epoch 与选择器 revision
//! 必须与当前 head 匹配，否则拒绝——并发的代际切换让写入冲突，不悄悄记
//! 在另一个控制器下。
//!
//! 根内容和态度使用唯一的认知储存（`cognition_contents` / `cognition_attitudes`）：
//! 这里不另建第二份内容库、修订链或注册表。
//!
//! 外部效应只能对账，不能因为回滚而消失。落地记录（`cognitive_root_applications`）
//! 只追加，同一版根可以对账多次。

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use shenko_types::cognition::{AttitudeConfidence, AttitudeRole, ContentRecord, ReasonPointer};
use shenko_types::cognitive_root::{
    CognitiveRootCapture, CognitiveRootContent, CognitiveRootHead, CognitiveRootNode,
    CognitiveRootReinstatement, CognitiveRootRequest, CognitionSelectorRef,
    ExternalEffectDisposition, HomeContentRef, RootApplication, RootApplicationScope,
    RootApplicationState, RootChainEntry, RootFence, RootRef, RootRequestKind,
    SoftwareSlotRef, RuntimeStateParticipant, COGNITIVE_ROOT_CODEC,
    COGNITIVE_ROOT_CODEC_MAJOR, COGNITIVE_ROOT_NAMESPACE,
};
use shenko_types::commit::CommitRef;
use shenko_types::error::ShenkoError;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use crate::store::{storage_error, Store};

/// 认知根的账本表。只建自己的表，不改任何已有行。
pub const COGNITIVE_ROOT_SCHEMA: &str = r#"
-- 认知原子表（与 atoms.rs 共享，IF NOT EXISTS 保证幂等）。
CREATE TABLE IF NOT EXISTS cognition_contents (
    digest TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL DEFAULT 0,
    media_type TEXT NOT NULL DEFAULT 'application/json',
    store_namespace TEXT NOT NULL DEFAULT 'cognition',
    locator TEXT NOT NULL DEFAULT '',
    codec_name TEXT NOT NULL DEFAULT 'shenko.cognition.content',
    codec_major INTEGER NOT NULL DEFAULT 1,
    codec_digest TEXT NOT NULL DEFAULT '',
    object_json TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    registered_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS cognition_attitudes (
    attitude_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    digest TEXT NOT NULL REFERENCES cognition_contents(digest),
    role_kind TEXT NOT NULL DEFAULT 'assertion',
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

CREATE TABLE IF NOT EXISTS cognitive_root_nodes (
    root_revision INTEGER PRIMARY KEY,
    agent_id TEXT NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1,
    content_digest TEXT NOT NULL,
    content_record_json TEXT NOT NULL,
    attitude_id TEXT NOT NULL,
    fence_json TEXT NOT NULL,
    reinstated_from_json TEXT,
    recorded_commit_ledger TEXT NOT NULL,
    recorded_commit_seq INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cognitive_root_nodes_agent
    ON cognitive_root_nodes(agent_id);
CREATE INDEX IF NOT EXISTS idx_cognitive_root_nodes_content
    ON cognitive_root_nodes(content_digest);

CREATE TABLE IF NOT EXISTS cognitive_root_requests (
    request_id TEXT PRIMARY KEY,
    agent_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    root_revision INTEGER NOT NULL,
    content_digest TEXT NOT NULL,
    target_json TEXT,
    recorded_commit_ledger TEXT NOT NULL,
    recorded_commit_seq INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cognitive_root_requests_agent
    ON cognitive_root_requests(agent_id);

CREATE TABLE IF NOT EXISTS cognitive_root_effects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    root_revision INTEGER NOT NULL,
    effect_id TEXT NOT NULL,
    disposition TEXT NOT NULL,
    UNIQUE(root_revision, effect_id)
);

CREATE TABLE IF NOT EXISTS cognitive_root_applications (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    root_revision INTEGER NOT NULL,
    scope TEXT NOT NULL,
    state_json TEXT NOT NULL,
    updated_commit_ledger TEXT NOT NULL,
    updated_commit_seq INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_cognitive_root_applications_revision
    ON cognitive_root_applications(root_revision);
"#;

impl Store {
    // ── capture: 从当前状态捕获一版新根 ──────────────────────────────────

    /// 捕获当前家园文件、软件槽、单元选择器与运行状态，写一版新根。
    ///
    /// 在同一事务里：
    /// 1. 把根内容序列化、封存进 CAS（按摘要去重）。
    /// 2. 写一条态度行（角色 `Assertion`，指向该内容，带理由与置信）。
    /// 3. 在 `cognitive_root_nodes` 追加一行。
    /// 4. 记录外部效应对账。
    /// 5. 写 `cognitive_root_requests`（幂等身份）。
    pub fn capture_cognitive_root(
        &self,
        capture: CognitiveRootCapture,
    ) -> Result<CognitiveRootNode, ShenkoError> {
        capture.content.validate()?;
        let content_bytes = capture.content.canonical_bytes()?;
        let content_digest = Digest::blake3(&content_bytes);

        // Seal root content in CAS.
        let sealed = self.cas.seal_object(
            &content_bytes,
            "application/json",
            COGNITIVE_ROOT_NAMESPACE,
            COGNITIVE_ROOT_CODEC,
            COGNITIVE_ROOT_CODEC_MAJOR,
        )?;

        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;

                // Idempotent request check.
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT root_revision FROM cognitive_root_requests WHERE request_id=?1",
                        [capture.request_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some(rev) = existing {
                    let node = self.read_root_node_tx(&tx, rev as u64)?
                        .ok_or_else(|| ShenkoError::storage("idempotent request references missing node"))?;
                    tx.commit().map_err(storage_error)?;
                    return Ok(node);
                }

                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);

                // Fence check.
                let agent_id = self.require_agent_id_tx(&tx)?;
                self.verify_root_fence_tx(&tx, &capture.fence, agent_id)?;

                // Next revision.
                let next_rev = self.next_root_revision_tx(&tx)?;

                // Register content in cognition_contents (via CAS-sealed object).
                let content_record = self.register_content_record_tx(&tx, &sealed.object)?;

                // Write attitude row.
                let attitude_id = EntityId::new();
                let attitude = self.insert_root_attitude_tx(
                    &tx,
                    commit,
                    agent_id,
                    attitude_id,
                    &content_digest,
                    &capture.reasons,
                    capture.confidence,
                )?;

                // Insert root node row.
                let node = CognitiveRootNode {
                    schema_version: CognitiveRootNode::SCHEMA_VERSION,
                    agent_id,
                    root_revision: next_rev,
                    content: content_record,
                    attitude_id: attitude.attitude_id,
                    recorded_commit: commit,
                    fence: capture.fence.clone(),
                    reinstated_from: None,
                    recorded_at_ms: now_ms(),
                };
                self.insert_root_node_tx(&tx, &node)?;

                // Effects.
                self.insert_root_effects_tx(&tx, next_rev, &capture.effects)?;

                // Request record.
                self.insert_root_request_tx(&tx, &CognitiveRootRequest {
                    request_id: capture.request_id,
                    agent_id,
                    kind: RootRequestKind::Capture,
                    root_revision: next_rev,
                    content: content_digest.clone(),
                    target: None,
                    recorded_commit: commit,
                    recorded_at_ms: node.recorded_at_ms,
                })?;

                // Commit (node + attitude + request + effects).
                self.ledger.record_commit(&tx, seq, 3 + capture.effects.len())?;
                tx.commit().map_err(storage_error)?;
                Ok(node)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    // ── reinstate: 在链尾追加一个内容与历史节点相同的新节点 ───────────────

    /// 回退：在链尾追加一个内容与所选历史节点完全相同的新节点。
    ///
    /// `target` 指向要恢复的历史节点；新节点的 `reinstated_from` 记下来源。
    /// 日志和链只增不减（§51.4）。
    pub fn reinstate_cognitive_root(
        &self,
        reinstatement: CognitiveRootReinstatement,
    ) -> Result<CognitiveRootNode, ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;

                // Idempotent request check.
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT root_revision FROM cognitive_root_requests WHERE request_id=?1",
                        [reinstatement.request_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(storage_error)?;
                if let Some(rev) = existing {
                    let node = self.read_root_node_tx(&tx, rev as u64)?
                        .ok_or_else(|| ShenkoError::storage("idempotent request references missing node"))?;
                    tx.commit().map_err(storage_error)?;
                    return Ok(node);
                }

                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);

                let agent_id = self.require_agent_id_tx(&tx)?;
                self.verify_root_fence_tx(&tx, &reinstatement.fence, agent_id)?;

                // Verify target exists.
                let target_node = self
                    .read_root_node_tx(&tx, reinstatement.target.root_revision)?
                    .ok_or_else(|| {
                        ShenkoError::conflict(format!(
                            "reinstatement target revision {} does not exist",
                            reinstatement.target.root_revision
                        ))
                    })?;
                if target_node.content.digest() != &reinstatement.target.content {
                    return Err(ShenkoError::conflict(
                        "reinstatement target content digest mismatch",
                    ));
                }

                let next_rev = self.next_root_revision_tx(&tx)?;

                // Reuse the same content (same digest → same content row).
                let content_record = self.register_content_record_tx(&tx, &target_node.content.object)?;

                // Build reasons: always include a Content pointer to the current head's content.
                let mut reasons = reinstatement.reasons.clone();
                if let Some(current_head) = self.read_latest_root_node_tx(&tx)? {
                    let superseded_digest = current_head.content.digest().clone();
                    if !reasons.iter().any(|r| r.stops_at(&superseded_digest)) {
                        reasons.push(ReasonPointer::content(superseded_digest));
                    }
                }

                let attitude_id = EntityId::new();
                let attitude = self.insert_root_attitude_tx(
                    &tx,
                    commit,
                    agent_id,
                    attitude_id,
                    target_node.content.digest(),
                    &reasons,
                    reinstatement.confidence,
                )?;

                let node = CognitiveRootNode {
                    schema_version: CognitiveRootNode::SCHEMA_VERSION,
                    agent_id,
                    root_revision: next_rev,
                    content: content_record,
                    attitude_id: attitude.attitude_id,
                    recorded_commit: commit,
                    fence: reinstatement.fence.clone(),
                    reinstated_from: Some(reinstatement.target.clone()),
                    recorded_at_ms: now_ms(),
                };
                self.insert_root_node_tx(&tx, &node)?;

                self.insert_root_effects_tx(&tx, next_rev, &reinstatement.effects)?;

                self.insert_root_request_tx(&tx, &CognitiveRootRequest {
                    request_id: reinstatement.request_id,
                    agent_id,
                    kind: RootRequestKind::Reinstate,
                    root_revision: next_rev,
                    content: target_node.content.digest().clone(),
                    target: Some(reinstatement.target.clone()),
                    recorded_commit: commit,
                    recorded_at_ms: node.recorded_at_ms,
                })?;

                self.ledger.record_commit(&tx, seq, 3 + reinstatement.effects.len())?;
                tx.commit().map_err(storage_error)?;
                Ok(node)
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    // ── reads ────────────────────────────────────────────────────────────

    /// 当前权威根。
    pub fn cognitive_root_head(
        &self,
        agent_id: EntityId,
    ) -> Result<Option<CognitiveRootHead>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let node = self.read_latest_root_node_conn(&conn, agent_id)?;
        match node {
            None => Ok(None),
            Some(node) => {
                let counts = self.root_counts_conn(&conn, agent_id)?;
                Ok(Some(CognitiveRootHead {
                    node,
                    node_count: counts.0,
                    distinct_content_count: counts.1,
                    latest_recorded_at_ms: counts.2,
                }))
            }
        }
    }

    /// 读一个特定版本的根节点。
    pub fn cognitive_root_node(
        &self,
        revision: u64,
    ) -> Result<Option<CognitiveRootNode>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        self.read_root_node_conn(&conn, revision)
    }

    /// 版本链的摘要视图（从新到旧）。
    pub fn cognitive_root_chain(
        &self,
        agent_id: EntityId,
        limit: u32,
    ) -> Result<Vec<RootChainEntry>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT root_revision, content_digest, reinstated_from_json, recorded_at_ms
                 FROM cognitive_root_nodes WHERE agent_id=?1
                 ORDER BY root_revision DESC LIMIT ?2",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![agent_id.to_string(), limit], |row| {
                let rev: i64 = row.get(0)?;
                let digest_str: String = row.get(1)?;
                let reinstated_json: Option<String> = row.get(2)?;
                let recorded_at_ms: i64 = row.get(3)?;
                Ok((rev, digest_str, reinstated_json, recorded_at_ms))
            })
            .map_err(storage_error)?;

        let mut entries = Vec::new();
        for row in rows {
            let (rev, digest_str, reinstated_json, recorded_at_ms) =
                row.map_err(storage_error)?;
            let reinstated_from: Option<RootRef> = reinstated_json
                .map(|json| serde_json::from_str(&json))
                .transpose()
                .map_err(|e| ShenkoError::storage(e.to_string()))?;
            entries.push(RootChainEntry {
                root_revision: rev as u64,
                content: Digest(digest_str),
                reinstated_from,
                recorded_at_ms,
            });
        }
        Ok(entries)
    }

    /// 一次根的落地请求。
    pub fn cognitive_root_request(
        &self,
        request_id: EntityId,
    ) -> Result<Option<CognitiveRootRequest>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        conn.query_row(
            "SELECT request_id, agent_id, kind, root_revision, content_digest,
                    target_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms
             FROM cognitive_root_requests WHERE request_id=?1",
            [request_id.to_string()],
            |row| {
                let request_id: String = row.get(0)?;
                let agent_id: String = row.get(1)?;
                let kind: String = row.get(2)?;
                let rev: i64 = row.get(3)?;
                let digest: String = row.get(4)?;
                let target_json: Option<String> = row.get(5)?;
                let ledger: String = row.get(6)?;
                let seq: i64 = row.get(7)?;
                let at: i64 = row.get(8)?;
                Ok((request_id, agent_id, kind, rev, digest, target_json, ledger, seq, at))
            },
        )
        .optional()
        .map_err(storage_error)?
        .map(|(request_id, agent_id, kind, rev, digest, target_json, ledger, seq, at)| {
            Ok(CognitiveRootRequest {
                request_id: request_id.parse().map_err(|_| ShenkoError::storage("invalid request_id"))?,
                agent_id: agent_id.parse().map_err(|_| ShenkoError::storage("invalid agent_id"))?,
                kind: match kind.as_str() {
                    "capture" => RootRequestKind::Capture,
                    "reinstate" => RootRequestKind::Reinstate,
                    _ => return Err(ShenkoError::storage(format!("unknown root request kind: {kind}"))),
                },
                root_revision: rev as u64,
                content: Digest(digest),
                target: target_json.map(|json| serde_json::from_str(&json))
                    .transpose()
                    .map_err(|e| ShenkoError::storage(e.to_string()))?,
                recorded_commit: CommitRef::new(
                    ledger.parse().map_err(|_| ShenkoError::storage("invalid ledger"))?,
                    shenko_types::ids::LedgerSeq(seq),
                ),
                recorded_at_ms: at,
            })
        })
        .transpose()
    }

    // ── application: 落地记录 ────────────────────────────────────────────

    /// 记录一次根的落地结果（只追加）。
    pub fn record_root_application(
        &self,
        application: &RootApplication,
    ) -> Result<(), ShenkoError> {
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let seq = self.ledger.allocate_seq(&tx)?;
                let commit = CommitRef::new(self.ledger.ledger_id(), seq);
                let state_json =
                    serde_json::to_string(&application.state).map_err(storage_error)?;
                tx.execute(
                    "INSERT INTO cognitive_root_applications
                     (root_revision, scope, state_json, updated_commit_ledger, updated_commit_seq)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        application.root_revision as i64,
                        application.scope.name(),
                        state_json,
                        commit.ledger_id.to_string(),
                        commit.seq.0 as i64,
                    ],
                )
                .map_err(storage_error)?;
                self.ledger.record_commit(&tx, seq, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(())
            },
            |e| matches!(e, ShenkoError::Storage(_)),
        )
    }

    /// 一个版本的所有落地记录。
    pub fn root_applications(
        &self,
        root_revision: u64,
    ) -> Result<Vec<RootApplication>, ShenkoError> {
        let conn = self.ledger.conn.lock();
        let mut stmt = conn
            .prepare(
                "SELECT root_revision, scope, state_json, updated_commit_ledger, updated_commit_seq
                 FROM cognitive_root_applications WHERE root_revision=?1
                 ORDER BY id",
            )
            .map_err(storage_error)?;
        let rows = stmt
            .query_map(params![root_revision as i64], |row| {
                let rev: i64 = row.get(0)?;
                let scope: String = row.get(1)?;
                let state_json: String = row.get(2)?;
                let ledger: String = row.get(3)?;
                let seq: i64 = row.get(4)?;
                Ok((rev, scope, state_json, ledger, seq))
            })
            .map_err(storage_error)?;
        let mut result = Vec::new();
        for row in rows {
            let (rev, scope, state_json, ledger, seq) = row.map_err(storage_error)?;
            let scope = match scope.as_str() {
                "home_files" => RootApplicationScope::HomeFiles,
                _ => return Err(ShenkoError::storage(format!("unknown application scope: {scope}"))),
            };
            let state: RootApplicationState =
                serde_json::from_str(&state_json).map_err(|e| ShenkoError::storage(e.to_string()))?;
            result.push(RootApplication {
                root_revision: rev as u64,
                scope,
                state,
                updated_commit: CommitRef::new(
                    ledger.parse().map_err(|_| ShenkoError::storage("invalid ledger"))?,
                    shenko_types::ids::LedgerSeq(seq),
                ),
            });
        }
        Ok(result)
    }

    // ── internal helpers ─────────────────────────────────────────────────

    fn require_agent_id_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<EntityId, ShenkoError> {
        let raw: String = tx
            .query_row(
                "SELECT agent_id FROM subject_branch_heads LIMIT 1",
                [],
                |row| row.get(0),
            )
            .map_err(|_| ShenkoError::conflict("no subject head — cannot write cognitive root"))?;
        raw.parse()
            .map_err(|_| ShenkoError::storage("invalid agent_id in head"))
    }

    fn verify_root_fence_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        fence: &RootFence,
        agent_id: EntityId,
    ) -> Result<(), ShenkoError> {
        let head_json: String = tx
            .query_row(
                "SELECT data FROM subject_branch_heads WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|_| ShenkoError::conflict("subject head missing for fence check"))?;
        let head: shenko_types::subject::SubjectBranchHead =
            serde_json::from_str(&head_json).map_err(storage_error)?;

        // The selector_revision check requires reading the activation selector row.
        let selector_rev: Option<i64> = tx
            .query_row(
                "SELECT selector_revision FROM activation_selectors WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?;
        let actual_selector_rev = selector_rev.unwrap_or(0) as u64;

        if !fence.covers(&head, actual_selector_rev) {
            return Err(ShenkoError::conflict(format!(
                "root fence mismatch: fence generation={:?} epoch={:?} sel_rev={} vs head generation={:?} epoch={:?} actual_sel_rev={}",
                fence.generation, fence.epoch, fence.selector_revision,
                head.software_generation, head.controller_epoch, actual_selector_rev
            )));
        }
        Ok(())
    }

    fn next_root_revision_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<u64, ShenkoError> {
        let max: Option<i64> = tx
            .query_row(
                "SELECT MAX(root_revision) FROM cognitive_root_nodes",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage_error)?
            .flatten();
        Ok(max.map(|v| v as u64 + 1).unwrap_or(1))
    }

    fn register_content_record_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        object: &shenko_types::object::ObjectRef,
    ) -> Result<ContentRecord, ShenkoError> {
        let now = now_ms();
        // Only insert if not already present (content-addressed dedup).
        let object_json = serde_json::to_string(object).map_err(storage_error)?;
        tx.execute(
            "INSERT OR IGNORE INTO cognition_contents
             (digest, bytes, media_type, store_namespace, locator,
              codec_name, codec_major, codec_digest, object_json, registered_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                object.digest.0.as_str(),
                object.bytes as i64,
                object.media_type.as_str(),
                object.store_namespace.as_str(),
                object.locator.as_str(),
                object.codec.name.as_str(),
                object.codec.major as i64,
                object.codec.digest.0.as_str(),
                object_json,
                now,
            ],
        )
        .map_err(storage_error)?;

        let (obj_json, reg_at): (String, i64) = tx
            .query_row(
                "SELECT object_json, registered_at_ms FROM cognition_contents WHERE digest=?1",
                [object.digest.0.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage_error)?;
        let obj: shenko_types::object::ObjectRef =
            serde_json::from_str(&obj_json).map_err(storage_error)?;
        Ok(ContentRecord {
            object: obj,
            registered_at_ms: reg_at,
        })
    }

    fn insert_root_attitude_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        commit: CommitRef,
        agent_id: EntityId,
        attitude_id: EntityId,
        content_digest: &Digest,
        reasons: &[ReasonPointer],
        confidence: AttitudeConfidence,
    ) -> Result<shenko_types::cognition::ContentAttitude, ShenkoError> {
        let role = AttitudeRole::Assertion;
        let role_json = serde_json::to_string(&role).map_err(storage_error)?;
        let reasons_json = serde_json::to_string(reasons).map_err(storage_error)?;
        let confidence_json = serde_json::to_string(&Some(confidence)).map_err(storage_error)?;
        let now = now_ms();

        tx.execute(
            "INSERT INTO cognition_attitudes
             (attitude_id, agent_id, digest, role_kind, role_json, reasons_json,
              confidence_json, knowledge_kind_hint, supersedes,
              created_commit_ledger, created_commit_seq, created_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,NULL,?8,?9,?10)",
            params![
                attitude_id.to_string(),
                agent_id.to_string(),
                content_digest.0.as_str(),
                role.kind(),
                role_json,
                reasons_json,
                confidence_json,
                commit.ledger_id.to_string(),
                commit.seq.0 as i64,
                now,
            ],
        )
        .map_err(storage_error)?;

        Ok(shenko_types::cognition::ContentAttitude {
            attitude_id,
            agent_id,
            digest: content_digest.clone(),
            role,
            reasons: reasons.to_vec(),
            confidence: Some(confidence),
            knowledge_kind_hint: None,
            supersedes: None,
            created_commit: commit,
            created_at_ms: now,
        })
    }

    fn insert_root_node_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        node: &CognitiveRootNode,
    ) -> Result<(), ShenkoError> {
        let content_record_json =
            serde_json::to_string(&node.content).map_err(storage_error)?;
        let fence_json = serde_json::to_string(&node.fence).map_err(storage_error)?;
        let reinstated_json = node
            .reinstated_from
            .as_ref()
            .map(|r| serde_json::to_string(r))
            .transpose()
            .map_err(storage_error)?;

        tx.execute(
            "INSERT INTO cognitive_root_nodes
             (root_revision, agent_id, schema_version, content_digest,
              content_record_json, attitude_id, fence_json,
              reinstated_from_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                node.root_revision as i64,
                node.agent_id.to_string(),
                node.schema_version as i64,
                node.content.digest().0.as_str(),
                content_record_json,
                node.attitude_id.to_string(),
                fence_json,
                reinstated_json,
                node.recorded_commit.ledger_id.to_string(),
                node.recorded_commit.seq.0 as i64,
                node.recorded_at_ms,
            ],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn insert_root_effects_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        root_revision: u64,
        effects: &[ExternalEffectDisposition],
    ) -> Result<(), ShenkoError> {
        for effect in effects {
            tx.execute(
                "INSERT OR IGNORE INTO cognitive_root_effects
                 (root_revision, effect_id, disposition)
                 VALUES (?1, ?2, ?3)",
                params![
                    root_revision as i64,
                    effect.effect_id.to_string(),
                    serde_json::to_string(&effect.disposition)
                        .map_err(storage_error)?
                        .trim_matches('"'),
                ],
            )
            .map_err(storage_error)?;
        }
        Ok(())
    }

    fn insert_root_request_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        request: &CognitiveRootRequest,
    ) -> Result<(), ShenkoError> {
        let target_json = request
            .target
            .as_ref()
            .map(|t| serde_json::to_string(t))
            .transpose()
            .map_err(storage_error)?;
        tx.execute(
            "INSERT INTO cognitive_root_requests
             (request_id, agent_id, kind, root_revision, content_digest,
              target_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                request.request_id.to_string(),
                request.agent_id.to_string(),
                request.kind.name(),
                request.root_revision as i64,
                request.content.0.as_str(),
                target_json,
                request.recorded_commit.ledger_id.to_string(),
                request.recorded_commit.seq.0 as i64,
                request.recorded_at_ms,
            ],
        )
        .map_err(storage_error)?;
        Ok(())
    }

    fn read_root_node_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        revision: u64,
    ) -> Result<Option<CognitiveRootNode>, ShenkoError> {
        tx.query_row(
            "SELECT root_revision, agent_id, schema_version, content_digest,
                    content_record_json, attitude_id, fence_json,
                    reinstated_from_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms
             FROM cognitive_root_nodes WHERE root_revision=?1",
            [revision as i64],
            decode_root_node,
        )
        .optional()
        .map_err(storage_error)?
        .map(|raw| inflate_root_node(raw))
        .transpose()
    }

    fn read_root_node_conn(
        &self,
        conn: &rusqlite::Connection,
        revision: u64,
    ) -> Result<Option<CognitiveRootNode>, ShenkoError> {
        conn.query_row(
            "SELECT root_revision, agent_id, schema_version, content_digest,
                    content_record_json, attitude_id, fence_json,
                    reinstated_from_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms
             FROM cognitive_root_nodes WHERE root_revision=?1",
            [revision as i64],
            decode_root_node,
        )
        .optional()
        .map_err(storage_error)?
        .map(|raw| inflate_root_node(raw))
        .transpose()
    }

    fn read_latest_root_node_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
    ) -> Result<Option<CognitiveRootNode>, ShenkoError> {
        tx.query_row(
            "SELECT root_revision, agent_id, schema_version, content_digest,
                    content_record_json, attitude_id, fence_json,
                    reinstated_from_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms
             FROM cognitive_root_nodes ORDER BY root_revision DESC LIMIT 1",
            [],
            decode_root_node,
        )
        .optional()
        .map_err(storage_error)?
        .map(|raw| inflate_root_node(raw))
        .transpose()
    }

    fn read_latest_root_node_conn(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
    ) -> Result<Option<CognitiveRootNode>, ShenkoError> {
        conn.query_row(
            "SELECT root_revision, agent_id, schema_version, content_digest,
                    content_record_json, attitude_id, fence_json,
                    reinstated_from_json, recorded_commit_ledger, recorded_commit_seq, recorded_at_ms
             FROM cognitive_root_nodes WHERE agent_id=?1 ORDER BY root_revision DESC LIMIT 1",
            [agent_id.to_string()],
            decode_root_node,
        )
        .optional()
        .map_err(storage_error)?
        .map(|raw| inflate_root_node(raw))
        .transpose()
    }

    fn root_counts_conn(
        &self,
        conn: &rusqlite::Connection,
        agent_id: EntityId,
    ) -> Result<(u64, u64, i64), ShenkoError> {
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM cognitive_root_nodes WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let distinct: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT content_digest) FROM cognitive_root_nodes WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let latest: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(recorded_at_ms), 0) FROM cognitive_root_nodes WHERE agent_id=?1",
                [agent_id.to_string()],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        Ok((total as u64, distinct as u64, latest))
    }
}

// ── free functions ───────────────────────────────────────────────────────

type RawRootNode = (
    i64, String, i64, String, String, String, String, Option<String>, String, i64, i64,
);

fn decode_root_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRootNode> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn inflate_root_node(raw: RawRootNode) -> Result<CognitiveRootNode, ShenkoError> {
    let (rev, agent_id, schema_version, _content_digest, content_record_json,
         attitude_id, fence_json, reinstated_json, ledger, seq, at) = raw;
    let content: ContentRecord =
        serde_json::from_str(&content_record_json).map_err(|e| ShenkoError::storage(e.to_string()))?;
    let fence: RootFence =
        serde_json::from_str(&fence_json).map_err(|e| ShenkoError::storage(e.to_string()))?;
    let reinstated_from: Option<RootRef> = reinstated_json
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|e| ShenkoError::storage(e.to_string()))?;
    Ok(CognitiveRootNode {
        schema_version: schema_version as u32,
        agent_id: agent_id.parse().map_err(|_| ShenkoError::storage("invalid agent_id"))?,
        root_revision: rev as u64,
        content,
        attitude_id: attitude_id.parse().map_err(|_| ShenkoError::storage("invalid attitude_id"))?,
        recorded_commit: CommitRef::new(
            ledger.parse().map_err(|_| ShenkoError::storage("invalid ledger"))?,
            shenko_types::ids::LedgerSeq(seq),
        ),
        fence,
        reinstated_from,
        recorded_at_ms: at,
    })
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
