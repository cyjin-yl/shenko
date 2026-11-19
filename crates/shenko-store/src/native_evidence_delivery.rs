//! Independent factual E1 intake. No control grant, task mutation or send permit.
use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use shenko_types::{
    authority::CommitAdmissionObject,
    commit::CommitRef,
    error::ShenkoError,
    evidence::EvidenceReceipt,
    ids::EntityId,
    native_evidence::*,
    object::{Digest, ObjectRef},
    operation::OperationRecord,
    subject::SubjectGenesis,
};

use crate::{
    Store,
    native_evidence::{NativeEvidenceFact, read_materialization, verify_materialization},
    native_facts::{invalid, same},
    store::storage_error,
};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS native_evidence_deliveries (
    delivery_id TEXT PRIMARY KEY, source_node_id TEXT NOT NULL, receipt_id TEXT NOT NULL,
    state TEXT NOT NULL, data TEXT NOT NULL, updated_commit_seq INTEGER NOT NULL
);
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeEvidenceDeliveryState {
    Pending,
    Blocked,
    Gap,
    Applied,
}

/// Installation-local transfer/application bookkeeping. It is excluded from
/// peer cognition and from every canonical row transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceDeliveryRecord {
    pub schema_version: u32,
    pub delivery_id: EntityId,
    pub delivery: NativeEvidenceDelivery,
    pub state: NativeEvidenceDeliveryState,
    pub first_seen: CommitRef,
    pub recorded_at: CommitRef,
    pub materialized_at: Option<CommitRef>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum NativeEvidenceDeliveryOutcome {
    Applied {
        source_commit: CommitRef,
        local_commit: CommitRef,
    },
    AlreadyApplied {
        source_commit: CommitRef,
        local_commit: CommitRef,
    },
    ReconciliationRequired {
        delivery_id: EntityId,
        reason: String,
    },
}

fn same_self(conn: &Connection, source: &NativeEvidenceSource) -> Result<(), ShenkoError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT data FROM subject_genesis WHERE subject_id=?1",
            [source.agent_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(storage_error)?;
    let genesis: SubjectGenesis = serde_json::from_str(
        &raw.ok_or_else(|| invalid("E1 source self is not retained locally"))?,
    )
    .map_err(invalid)?;
    if genesis.subject_id != source.agent_id
        || Digest::blake3(&serde_json::to_vec(&genesis)?) != source.origin_proof
    {
        return Err(invalid(
            "authenticated E1 source differs from the retained self origin",
        ));
    }
    Ok(())
}

fn validate_item(item: &NativeEvidenceItem) -> Result<(), ShenkoError> {
    if item.position.sequence == 0 || item.source_commit.seq.0 != item.position.sequence {
        return Err(invalid(
            "original E1 manifest has a different source position",
        ));
    }
    match &item.availability {
        NativeEvidenceAvailability::Sealed { evidence_ref } => {
            if !matches!(evidence_ref.codec.major, 1..=3) {
                return Err(invalid("unsupported typed object for original E1"));
            }
            crate::native_facts::typed_json(
                evidence_ref,
                crate::native_evidence::CODEC,
                evidence_ref.codec.major,
            )
        }
        NativeEvidenceAvailability::Gap { reason }
            if !reason.is_empty() && reason.len() <= 4096 =>
        {
            Ok(())
        }
        NativeEvidenceAvailability::Gap { .. } => Err(invalid("invalid E1 gap reason")),
    }
}

fn validate_source(
    item: &NativeEvidenceItem,
    source: &NativeEvidenceSource,
) -> Result<(), ShenkoError> {
    validate_item(item)?;
    if item.source_commit.ledger_id != source.ledger_id {
        return Err(invalid(
            "E1 source commit belongs to a different source ledger",
        ));
    }
    Ok(())
}

fn verify_item(
    item: &NativeEvidenceItem,
    fact: &NativeEvidenceFact,
    source: &NativeEvidenceSource,
) -> Result<(), ShenkoError> {
    validate_source(item, source)?;
    if item.position.receipt_id != fact.receipt.receipt_id
        || item.operation_id != fact.operation.operation_id
        || item.attempt_id != fact.attempt.attempt_id
        || item.source_commit != fact.receipt.received_commit
        || source.agent_id != fact.arm.admission.subject_id
    {
        return Err(invalid(
            "E1 page descriptor differs from its original typed fact",
        ));
    }
    Ok(())
}

fn read_delivery(
    conn: &Connection,
    id: EntityId,
) -> Result<Option<NativeEvidenceDeliveryRecord>, ShenkoError> {
    let raw: Option<(String, String, String, String, i64)> = conn.query_row(
        "SELECT source_node_id,receipt_id,state,data,updated_commit_seq FROM native_evidence_deliveries WHERE delivery_id=?1",
        [id.to_string()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    ).optional().map_err(storage_error)?;
    raw.map(|(source, receipt, state, raw, seq)| {
        let record: NativeEvidenceDeliveryRecord = serde_json::from_str(&raw).map_err(invalid)?;
        if record.schema_version != 1
            || record.delivery_id != id
            || source != record.delivery.source.node_id.to_string()
            || receipt != record.delivery.item.position.receipt_id.to_string()
            || state != state_name(record.state)
            || i64::try_from(record.recorded_at.seq.0).ok() != Some(seq)
        {
            return Err(invalid("local E1 delivery columns differ"));
        }
        Ok(record)
    })
    .transpose()
}

fn state_name(state: NativeEvidenceDeliveryState) -> &'static str {
    match state {
        NativeEvidenceDeliveryState::Pending => "pending",
        NativeEvidenceDeliveryState::Blocked => "blocked",
        NativeEvidenceDeliveryState::Gap => "gap",
        NativeEvidenceDeliveryState::Applied => "applied",
    }
}

fn write_delivery(
    tx: &Transaction<'_>,
    record: &NativeEvidenceDeliveryRecord,
) -> Result<(), ShenkoError> {
    tx.execute("INSERT INTO native_evidence_deliveries(delivery_id,source_node_id,receipt_id,state,data,updated_commit_seq)
        VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(delivery_id) DO UPDATE SET state=excluded.state,data=excluded.data,updated_commit_seq=excluded.updated_commit_seq",
        params![record.delivery_id.to_string(), record.delivery.source.node_id.to_string(),
            record.delivery.item.position.receipt_id.to_string(), state_name(record.state), serde_json::to_string(record)?,
            i64::try_from(record.recorded_at.seq.0).map_err(invalid)?]).map_err(storage_error)?;
    Ok(())
}

impl Store {
    /// Only an actual source observation is advertised. A receiver's local
    /// materialization receipt is not a new source observation. Original
    /// paired-T1 observations without a supported seal are explicit gaps.
    pub fn native_evidence_page(
        &self,
        source: &NativeEvidenceSource,
        cursor: Option<&NativeEvidenceCursor>,
        page_items: usize,
    ) -> Result<NativeEvidencePage, ShenkoError> {
        if !(1..=NATIVE_EVIDENCE_PAGE_ITEMS).contains(&page_items) {
            return Err(invalid(
                "native E1 page item budget is outside the protocol bound",
            ));
        }
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let binding = crate::store::read_installation_binding(&tx).map_err(invalid)?;
        let subject = binding
            .subject
            .ok_or_else(|| invalid("E1 source has no actual installation subject"))?;
        if source.ledger_id != binding.ledger_id
            || source.agent_id != subject.agent_id
            || source.branch_id != subject.branch_id
            || source.origin_proof != subject.origin_proof
        {
            return Err(invalid("E1 source Node and Store identities differ"));
        }
        let head: u64 = tx
            .query_row("SELECT COALESCE(MAX(seq),0) FROM commit_log", [], |row| {
                row.get(0)
            })
            .map_err(storage_error)?;
        let cursor = cursor.cloned().unwrap_or(NativeEvidenceCursor {
            scope: NATIVE_EVIDENCE_SCOPE.into(),
            ledger_id: binding.ledger_id,
            after: None,
            through: head,
        });
        if cursor.scope != NATIVE_EVIDENCE_SCOPE
            || cursor.ledger_id != binding.ledger_id
            || cursor.through > head
            || cursor
                .after
                .is_some_and(|after| after.sequence > cursor.through)
        {
            return Err(invalid(
                "E1 enumeration cursor no longer names this installation snapshot",
            ));
        }
        let after = cursor.after.map_or(0, |position| position.sequence);
        let after_id = cursor
            .after
            .map_or_else(String::new, |position| position.receipt_id.to_string());
        // Physical seq is used only to enumerate this Store. The original
        // received_commit, local source commit_log and canonical T1 marker
        // must all be present. An ordinary copied peer row cannot qualify.
        let mut statement = tx.prepare("SELECT e.receipt_id,e.data,e.updated_commit_seq,o.data
            FROM evidence_receipts e JOIN operations o ON o.operation_id=e.operation_id
            WHERE (e.updated_commit_seq>?1 OR (e.updated_commit_seq=?1 AND e.receipt_id>?2))
              AND e.updated_commit_seq<=?3
              AND json_extract(e.data,'$.received_commit.seq')=e.updated_commit_seq
              AND EXISTS(SELECT 1 FROM commit_log c WHERE c.seq=e.updated_commit_seq
                  AND c.ledger_id=json_extract(e.data,'$.received_commit.ledger_id'))
              AND EXISTS(SELECT 1 FROM control_materializations m
                  WHERE json_extract(m.data,'$.logical_result.ledger_id')=json_extract(o.data,'$.prepared_commit.ledger_id')
                    AND json_extract(m.data,'$.logical_result.seq')=json_extract(o.data,'$.prepared_commit.seq'))
            ORDER BY e.updated_commit_seq,e.receipt_id LIMIT ?4").map_err(storage_error)?;
        let rows = statement
            .query_map(
                params![
                    i64::try_from(after).map_err(invalid)?,
                    after_id,
                    i64::try_from(cursor.through).map_err(invalid)?,
                    page_items as i64 + 1
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let more = rows.len() > page_items;
        let mut items = Vec::new();
        for (id, raw, sequence, operation) in rows.into_iter().take(page_items) {
            let receipt: EvidenceReceipt = serde_json::from_str(&raw).map_err(invalid)?;
            let operation: OperationRecord = serde_json::from_str(&operation).map_err(invalid)?;
            if receipt.record_schema_version != 1
                || id != receipt.receipt_id.to_string()
                || receipt.operation_id != Some(operation.operation_id)
            {
                return Err(invalid("source E1 row identity or schema differs"));
            }
            let arm = self
                .local_native_fact(&tx, operation.prepared_commit, "native.arm")?
                .ok_or_else(|| invalid("enumerated E1 has no actual source T1"))?;
            if arm.admission.subject_id != source.agent_id {
                return Err(invalid("E1 enumeration crossed self identities"));
            }
            let availability = match read_materialization(&tx, receipt.receipt_id)? {
                Some(marker) => {
                    let (fact, _) = self.read_native_delivery_fact(&tx, &marker.evidence_ref)?;
                    verify_materialization(&tx, &marker, &fact)?;
                    if marker.committed_at != receipt.received_commit || marker.logical_result != marker.committed_at
                        || !same(&receipt, &fact.receipt)?
                    { return Err(invalid("receiver materialization cannot advertise a source observation")); }
                    NativeEvidenceAvailability::Sealed { evidence_ref: marker.evidence_ref }
                }
                None => NativeEvidenceAvailability::Gap { reason: "original paired-T1 E1 retained, but no supported first/Unknown-successor seal; reconciliation required".into() },
            };
            let item = NativeEvidenceItem {
                position: NativeEvidencePosition {
                    sequence,
                    receipt_id: receipt.receipt_id,
                },
                operation_id: operation.operation_id,
                attempt_id: receipt
                    .attempt_id
                    .ok_or_else(|| invalid("native E1 omitted its attempt"))?,
                source_commit: receipt.received_commit,
                availability,
            };
            validate_source(&item, source)?;
            items.push(item);
        }
        let next = if more {
            Some(NativeEvidenceCursor {
                after: items.last().map(|item| item.position),
                ..cursor.clone()
            })
        } else {
            None
        };
        Ok(NativeEvidencePage {
            schema_version: NATIVE_EVIDENCE_SCHEMA_VERSION,
            scope: NATIVE_EVIDENCE_SCOPE.into(),
            source: source.clone(),
            cursor,
            items,
            next,
        })
    }

    /// The adapter may publish only this installation's actual sealed E1.
    /// Arbitrary caller-provided CAS roots never enter the Node transport.
    pub fn native_evidence_objects(
        &self,
        item: &NativeEvidenceItem,
    ) -> Result<Vec<CommitAdmissionObject>, ShenkoError> {
        validate_item(item)?;
        let NativeEvidenceAvailability::Sealed { evidence_ref } = &item.availability else {
            return Err(invalid("E1 gap has no transport closure"));
        };
        let conn = self.ledger.conn.lock();
        let marker = read_materialization(&conn, item.position.receipt_id)?
            .ok_or_else(|| invalid("E1 has no local source receipt"))?;
        let (fact, _) = self.read_native_delivery_fact(&conn, evidence_ref)?;
        verify_materialization(&conn, &marker, &fact)?;
        if marker.evidence_ref != *evidence_ref
            || marker.committed_at != marker.logical_result
            || marker.logical_result != item.source_commit
            || fact.operation.operation_id != item.operation_id
            || fact.attempt.attempt_id != item.attempt_id
        {
            return Err(invalid("only an original source E1 can be published"));
        }
        let (objects, missing) = self
            .collect_closure_bounded(
                std::slice::from_ref(evidence_ref),
                NATIVE_EVIDENCE_CLOSURE_OBJECTS,
                NATIVE_EVIDENCE_CLOSURE_BYTES,
            )
            .map_err(invalid)?;
        if !missing.is_empty() {
            return Err(invalid("original E1 closure is incomplete"));
        }
        objects
            .into_iter()
            .map(|object| {
                let bytes = self.cas.read_object(&object.digest)?;
                if bytes.len() as u64 != object.bytes || Digest::blake3(&bytes) != object.digest {
                    return Err(invalid("E1 closure bytes changed"));
                }
                Ok(CommitAdmissionObject {
                    reference: ObjectRef {
                        digest: object.digest,
                        bytes: object.bytes,
                        media_type: object.media_type,
                        store_namespace: object.store_namespace,
                        locator: String::new(),
                        codec: object.codec,
                    },
                    bytes,
                })
            })
            .collect()
    }

    /// Persist the authenticated manifest before any object download. Pending
    /// transfer and refused predecessor facts survive disconnect/restart.
    pub fn queue_native_evidence_delivery(
        &self,
        delivery: &NativeEvidenceDelivery,
    ) -> Result<NativeEvidenceDeliveryRecord, ShenkoError> {
        validate_source(&delivery.item, &delivery.source)?;
        let id = EntityId::derived(
            "native-evidence-delivery-v1",
            &serde_json::to_vec(&(
                delivery.source.node_id,
                delivery.source.ledger_id,
                delivery.source.agent_id,
                &delivery.source.origin_proof,
                &delivery.item,
            ))?,
        );
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                same_self(&tx, &delivery.source)?;
                if let Some(record) = read_delivery(&tx, id)? {
                    return Ok(record);
                }
                let commit =
                    CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                let (state, reason) = match &delivery.item.availability {
                    NativeEvidenceAvailability::Sealed { .. } => (
                        NativeEvidenceDeliveryState::Pending,
                        "original object transfer or application pending".into(),
                    ),
                    NativeEvidenceAvailability::Gap { reason } => {
                        (NativeEvidenceDeliveryState::Gap, reason.clone())
                    }
                };
                let record = NativeEvidenceDeliveryRecord {
                    schema_version: 1,
                    delivery_id: id,
                    delivery: delivery.clone(),
                    state,
                    first_seen: commit,
                    recorded_at: commit,
                    materialized_at: None,
                    reason: Some(reason),
                };
                write_delivery(&tx, &record)?;
                self.ledger.record_commit(&tx, commit.seq, 1)?;
                tx.commit().map_err(storage_error)?;
                Ok(record)
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }

    pub fn native_evidence_delivery(
        &self,
        id: EntityId,
    ) -> Result<Option<NativeEvidenceDeliveryRecord>, ShenkoError> {
        read_delivery(&self.ledger.conn.lock(), id)
    }

    pub fn native_evidence_delivery_debt_count(
        &self,
        source_node: EntityId,
    ) -> Result<u64, ShenkoError> {
        self.ledger.conn.lock().query_row(
            "SELECT count(*) FROM native_evidence_deliveries WHERE source_node_id=?1 AND state!='applied'",
            [source_node.to_string()], |row| row.get(0)).map_err(storage_error)
    }

    /// Bounded operator-visible local debt. The total makes truncation
    /// explicit; neither the list nor a source page is a remote acknowledgement.
    pub fn native_evidence_delivery_debts(
        &self,
    ) -> Result<(Vec<NativeEvidenceDeliveryRecord>, u64), ShenkoError> {
        let mut conn = self.ledger.conn.lock();
        let tx = conn.transaction().map_err(storage_error)?;
        let total: u64 = tx
            .query_row(
                "SELECT count(*) FROM native_evidence_deliveries WHERE state!='applied'",
                [],
                |row| row.get(0),
            )
            .map_err(storage_error)?;
        let mut statement = tx.prepare("SELECT delivery_id FROM native_evidence_deliveries WHERE state!='applied' ORDER BY updated_commit_seq,delivery_id LIMIT 64").map_err(storage_error)?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(storage_error)?;
        let records = ids
            .into_iter()
            .map(|id| {
                read_delivery(&tx, id.parse().map_err(invalid)?)?
                    .ok_or_else(|| invalid("enumerated E1 delivery disappeared"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((records, total))
    }

    /// CAS bytes are inert until the receiver verifies its own actual T1
    /// receipt. No source marker or copied `applied` event satisfies that test.
    pub fn receive_native_evidence(
        &self,
        id: EntityId,
        objects: &[CommitAdmissionObject],
    ) -> Result<NativeEvidenceDeliveryOutcome, ShenkoError> {
        let retained = self
            .native_evidence_delivery(id)?
            .ok_or_else(|| invalid("E1 manifest was not retained before transfer"))?;
        let NativeEvidenceAvailability::Sealed { evidence_ref } =
            &retained.delivery.item.availability
        else {
            return Ok(NativeEvidenceDeliveryOutcome::ReconciliationRequired {
                delivery_id: id,
                reason: retained.reason.unwrap_or_default(),
            });
        };
        let mut total = 0u64;
        if objects.len() > NATIVE_EVIDENCE_CLOSURE_OBJECTS {
            return Err(invalid("E1 object count exceeds transport budget"));
        }
        for object in objects {
            total = total
                .checked_add(object.reference.bytes)
                .filter(|total| *total <= NATIVE_EVIDENCE_CLOSURE_BYTES)
                .ok_or_else(|| invalid("E1 object bytes exceed transport budget"))?;
            if object.bytes.len() as u64 != object.reference.bytes
                || Digest::blake3(&object.bytes) != object.reference.digest
            {
                return Err(invalid("E1 object digest or length differs"));
            }
            self.cas.seal_object(
                &object.bytes,
                &object.reference.media_type,
                &object.reference.store_namespace,
                &object.reference.codec.name,
                object.reference.codec.major,
            )?;
        }
        let (closure, missing) = self
            .collect_closure_bounded(
                std::slice::from_ref(evidence_ref),
                NATIVE_EVIDENCE_CLOSURE_OBJECTS,
                NATIVE_EVIDENCE_CLOSURE_BYTES,
            )
            .map_err(invalid)?;
        let digests: BTreeSet<_> = closure.iter().map(|object| &object.digest).collect();
        if !missing.is_empty()
            || objects
                .iter()
                .any(|object| !digests.contains(&object.reference.digest))
        {
            return Err(invalid(
                "E1 transfer is incomplete or contains objects outside its original closure",
            ));
        }
        self.ledger.with_mutation(
            || {
                let mut conn = self.ledger.conn.lock();
                let tx = conn
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .map_err(storage_error)?;
                let mut record = read_delivery(&tx, id)?
                    .ok_or_else(|| invalid("retained E1 delivery disappeared"))?;
                same_self(&tx, &record.delivery.source)?;
                let commit =
                    CommitRef::new(self.ledger.ledger_id(), self.ledger.allocate_seq(&tx)?);
                tx.execute_batch("SAVEPOINT native_e1_delivery")
                    .map_err(storage_error)?;
                let mut verified_receipt = None;
                let applied = (|| {
                    let (fact, _) = self.read_native_delivery_fact(&tx, evidence_ref)?;
                    verify_item(&record.delivery.item, &fact, &record.delivery.source)?;
                    verified_receipt = Some(fact.receipt.clone());
                    if let Some(marker) = read_materialization(&tx, fact.receipt.receipt_id)? {
                        if marker.evidence_ref != *evidence_ref {
                            return Err(invalid(
                                "existing E1 materialization differs from the original seal",
                            ));
                        }
                        verify_materialization(&tx, &marker, &fact)?;
                        // A later local observation/case decision is not rolled
                        // back merely because the original first E1 is replayed.
                        return Ok((false, marker.committed_at, 0));
                    }
                    let entities = if evidence_ref.codec.major == 3 {
                        self.materialize_native_successor(&tx, evidence_ref, commit)?
                    } else {
                        self.materialize_native_evidence(&tx, evidence_ref, commit)?;
                        // E1, operation, attempt, local receipt and, for an
                        // uncertain first fold, one newly opened local case.
                        4 + usize::from(fact.operation.state
                            == shenko_types::operation::OperationState::Reconciling)
                    };
                    Ok((true, commit, entities))
                })();
                match applied {
                    Ok((fresh, local, entities)) => {
                        tx.execute_batch("RELEASE native_e1_delivery")
                            .map_err(storage_error)?;
                        if record.state != NativeEvidenceDeliveryState::Applied {
                            record.state = NativeEvidenceDeliveryState::Applied;
                            record.materialized_at = Some(local);
                            record.recorded_at = commit;
                            record.reason = None;
                            write_delivery(&tx, &record)?;
                            self.ledger.record_commit(
                                &tx,
                                commit.seq,
                                entities + 1, // Include the delivery state row.
                            )?;
                            tx.commit().map_err(storage_error)?;
                        } else if fresh {
                            return Err(invalid("applied E1 delivery lost its actual receipt"));
                        }
                        if fresh {
                            Ok(NativeEvidenceDeliveryOutcome::Applied {
                                source_commit: record.delivery.item.source_commit,
                                local_commit: local,
                            })
                        } else {
                            Ok(NativeEvidenceDeliveryOutcome::AlreadyApplied {
                                source_commit: record.delivery.item.source_commit,
                                local_commit: local,
                            })
                        }
                    }
                    Err(error @ ShenkoError::Conflict(_)) => {
                        tx.execute_batch(
                            "ROLLBACK TO native_e1_delivery; RELEASE native_e1_delivery",
                        )
                        .map_err(storage_error)?;
                        let reason = error.to_string();
                        // Preserve distinct original collisions through the same
                        // local conflict journal as direct E1 admission. Its local
                        // receipt is separate from the unchanged source seal.
                        let collision = if let Some(receipt) = &verified_receipt {
                            match crate::receipt_admission::decide(&tx, receipt)
                                .map_err(storage_error)?
                            {
                                crate::receipt_admission::Admission::Conflict(conflict) => {
                                    crate::receipt_admission::stage(
                                        &tx,
                                        &self.ledger,
                                        commit.seq,
                                        receipt,
                                        &conflict,
                                    )?
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };
                        if collision.is_some()
                            || record.state != NativeEvidenceDeliveryState::Blocked
                            || record.reason.as_ref() != Some(&reason)
                        {
                            record.state = NativeEvidenceDeliveryState::Blocked;
                            record.recorded_at = commit;
                            record.reason = Some(reason.clone());
                            write_delivery(&tx, &record)?;
                            self.ledger.record_commit(
                                &tx,
                                commit.seq,
                                1 + usize::from(collision.is_some()),
                            )?;
                            tx.commit().map_err(storage_error)?;
                        }
                        Ok(NativeEvidenceDeliveryOutcome::ReconciliationRequired {
                            delivery_id: id,
                            reason,
                        })
                    }
                    Err(error) => Err(error),
                }
            },
            |error| matches!(error, ShenkoError::Storage(_)),
        )
    }
}
