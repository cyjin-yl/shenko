//! The live seat registry — the only path between durable seat records and
//! live adapters. SPEC §17.1, §35.5, §41.11.
//!
//! Every observation flows through here so that question persistence is not
//! optional: a question a model could answer is durably recorded *before*
//! any answer path can see it, bound to the lease, seat incarnation and task
//! revision that observed it.
//!
//! Every answer flows through here in the order SPEC §41.11 requires:
//!
//! 1. **commit the decision** — a compare-and-swap on the question revision,
//!    the seat incarnation, the task revision and the writer epoch the
//!    decider actually read. The question moves to `decision_committed`, so a
//!    second decision for the same question is refused and a crash leaves a
//!    durable decision instead of nothing.
//! 2. **plan the effect** — the exact bytes that will be written, with their
//!    digest. Nothing is sent yet.
//! 3. **preflight** — the adapter re-reads the *live* protocol or screen
//!    state. A question that is gone, a program that changed or a pane that
//!    was reused refuses the write.
//! 4. **arm** — the digest and the carrying operation are recorded, and the
//!    question moves to `responding`. From here the effect may have reached
//!    the remote side, so recovery never re-sends it.
//! 5. **execute** — the transport runs outside any database transaction.
//! 6. **record the evidence** — a confirmed answer ends `answered`, an
//!    unconfirmed one ends `answer_unknown`, a refusal before the write ends
//!    `not_sent` and a remote rejection ends `withdrawn`.
//!
//! The writer authority travels *with* the effect ([`SeatWriteFence`]): the
//! adapters do not re-check a lease and then write, they carry the epoch the
//! decision was made under.

use crate::seat::{
    AnswerConfirmation, PreflightRefusalKind, SeatAdapter, SeatAnswer, SeatHandle, SeatObservation,
    SeatPreflight, SeatQuestionCandidate, SeatResponse, SeatWriteFence, question_fingerprint,
};
use anyhow::{Context, Result, ensure};
use shenko_store::{
    SeatAnswerDecisionRecord, SeatAnswerDecisionState, SeatAnswerFact, SeatAnswerOutcome,
    SeatConfidenceFact, SeatDecisionExpectation, SeatOptionFact, SeatQuestionFact,
    SeatQuestionRecord, SeatQuestionState, Store,
};
use shenko_types::commit::{CommitRef, LedgerSeq};
use shenko_types::ids::{EntityId, RecordRevision};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Durable bindings of one attached seat.
#[derive(Debug, Clone, Copy)]
pub struct SeatAttachment {
    /// The task this seat serves.
    pub task_id: EntityId,
    /// The durable seat record id.
    pub seat_record_id: EntityId,
    /// The v2 writer lease whose holder attached the seat.
    pub lease_id: EntityId,
}

struct LiveSeat {
    adapter: Arc<dyn SeatAdapter>,
    attachment: SeatAttachment,
    handle: SeatHandle,
    /// Semantic question id → durable question record id. Rebuilt by
    /// fingerprint match after a restart, so it is process-local only.
    durable_questions: Mutex<BTreeMap<String, EntityId>>,
}

/// Registry of every live seat in this process.
pub struct SeatRuntime {
    store: Arc<Store>,
    seats: Mutex<BTreeMap<String, LiveSeat>>,
}

/// One answer request, with the authority the decider acted under.
#[derive(Debug, Clone)]
pub struct SeatRespondRequest {
    /// The adapter-local location id of the seat.
    pub location_id: String,
    /// The semantic question id the observation reported.
    pub semantic_question_id: String,
    /// The decision itself.
    pub answer: SeatAnswer,
    /// Who decided: the subject, or a child with an explicit decision scope.
    pub deciding_actor: String,
    /// The delegation that gave the decider its decision scope.
    pub delegation_grant_ref: Option<String>,
    /// The authority revision the decision was made under.
    pub authority_revision: Option<u64>,
    /// The trusted operation that carries the effect.
    pub operation_id: EntityId,
}

/// What one answered question produced: the transport result and the durable
/// facts the answer left behind.
#[derive(Debug, Clone)]
pub struct SeatRespondOutcome {
    pub response: SeatResponse,
    /// The durable decision record.
    pub decision_id: EntityId,
    /// The durable question record the decision belongs to.
    pub question_id: EntityId,
    /// The decision's final state.
    pub decision_state: SeatAnswerDecisionState,
    /// The question's final state.
    pub question_state: SeatQuestionState,
    /// The digest of the exact bytes that were written (empty when nothing
    /// was written).
    pub effect_input_digest: String,
    /// Why the answer was not applied, when it was not.
    pub refusal: Option<String>,
}

impl SeatRuntime {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store, seats: Mutex::new(BTreeMap::new()) }
    }

    pub fn store(&self) -> Arc<Store> {
        Arc::clone(&self.store)
    }

    /// Attach one live adapter to its durable bindings. Returns the handle
    /// the adapter discovered; the runtime keys the seat by its location id.
    pub async fn attach(
        &self,
        adapter: Arc<dyn SeatAdapter>,
        attachment: SeatAttachment,
    ) -> Result<SeatHandle> {
        let mut discovered = adapter.discover().await.context("seat adapter discover 失败")?;
        ensure!(
            !discovered.is_empty(),
            "seat adapter 没有发现任何 seat"
        );
        let handle = discovered.remove(0);
        let key = handle.location_id.clone();
        let seat = LiveSeat {
            adapter,
            attachment,
            handle: handle.clone(),
            durable_questions: Mutex::new(BTreeMap::new()),
        };
        self.seats.lock().await.insert(key, seat);
        Ok(handle)
    }

    /// Handles of every attached seat.
    pub async fn handles(&self) -> Vec<SeatHandle> {
        self.seats
            .lock()
            .await
            .values()
            .map(|seat| seat.handle.clone())
            .collect()
    }

    async fn live(
        &self,
        location_id: &str,
    ) -> Result<(Arc<dyn SeatAdapter>, SeatHandle, SeatAttachment)> {
        let seats = self.seats.lock().await;
        let seat = seats
            .get(location_id)
            .with_context(|| format!("seat 未挂接到运行时：{location_id}"))?;
        Ok((Arc::clone(&seat.adapter), seat.handle.clone(), seat.attachment))
    }

    /// Observe one seat. New questions are durably recorded before the
    /// observation is returned; the observation carries the same candidates
    /// the adapter produced, with their durable ids resolvable through
    /// [`SeatRuntime::respond`].
    ///
    /// A question whose meaning changed is a *new* revision: the old record
    /// is superseded and the new one gets its own durable id, because the
    /// answer to the old wording is not an answer to the new one. A closed
    /// question that reappears also gets a new durable id.
    pub async fn observe(&self, location_id: &str) -> Result<SeatObservation> {
        let (adapter, handle, attachment) = self.live(location_id).await?;
        let mut observation = adapter.observe(&handle).await?;
        let mut unknown = observation.unknown.clone();
        let seats = self.seats.lock().await;
        let Some(live) = seats.get(location_id) else {
            return Ok(observation);
        };
        for question in &observation.questions {
            let fingerprint = question_fingerprint(question);
            let mut durable = live.durable_questions.lock().await;
            let existing = durable
                .get(&question.question_id)
                .and_then(|id| self.store.seat_question(*id).ok().flatten());
            let mut superseded: Option<(EntityId, RecordRevision)> = None;
            match &existing {
                Some(record)
                    if record.question.fingerprint == fingerprint
                        && !record.state.is_closed() =>
                {
                    // The same question, unchanged: the durable record stays.
                    continue;
                }
                Some(record) => {
                    // The wording changed, or the question was closed and
                    // appeared again: this is a new question identity.
                    superseded = Some((record.question_id, record.record_revision));
                }
                None => {}
            }
            let record = SeatQuestionRecord {
                schema_version: 2,
                question_id: EntityId::new(),
                seat_id: attachment.seat_record_id,
                task_id: attachment.task_id,
                seat_incarnation: shenko_types::ids::SeatIncarnation(0),
                task_revision: shenko_types::ids::TaskRevision(0),
                lease_id: attachment.lease_id,
                lease_epoch: 0,
                question_revision: 0,
                question: fact_from_candidate(question),
                state: SeatQuestionState::Open,
                record_revision: RecordRevision(0),
                created_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
                updated_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
            };
            match self.store.open_seat_question(record) {
                Ok(committed) => {
                    if let Some((old_id, old_revision)) = superseded {
                        // The old revision keeps its history and is closed as
                        // superseded; it is never silently rewritten.
                        let _ = self.store.mark_seat_question_state(
                            old_id,
                            old_revision,
                            &[SeatQuestionState::Open],
                            SeatQuestionState::Superseded,
                        );
                    }
                    durable.insert(question.question_id.clone(), committed.question_id);
                }
                Err(error) => {
                    // The question could not be made durable, so it can never
                    // be answered. The observation says so instead of
                    // pretending the question is available.
                    unknown.push(format!(
                        "问题未能持久化，因此不可回答：{}",
                        format!("{error:#}")
                    ));
                }
            }
        }
        observation.unknown = unknown;
        Ok(observation)
    }

    /// Answer one open question on one seat.
    ///
    /// The order is the SPEC §41.11 order, and it is what makes the answer
    /// safe: the durable decision is committed *before* any input reaches
    /// the remote side, the live state is re-checked immediately before the
    /// write, the exact effect is armed with its digest, and the transport
    /// runs outside every database transaction. A crash at any point leaves a
    /// durable fact — `decision_committed`, `responding` or a terminal
    /// decision — and the answer is never replayed.
    pub async fn respond(&self, request: SeatRespondRequest) -> Result<SeatRespondOutcome> {
        let (adapter, handle, attachment) = self.live(&request.location_id).await?;
        let durable_id = {
            let seats = self.seats.lock().await;
            let live = seats
                .get(&request.location_id)
                .with_context(|| format!("seat 未挂接到运行时：{}", request.location_id))?;
            let durable = live.durable_questions.lock().await;
            *durable
                .get(&request.semantic_question_id)
                .with_context(|| {
                    format!(
                        "问题在运行时中没有持久记录：{}",
                        request.semantic_question_id
                    )
                })?
        };

        // The writer fence is re-verified before anything is decided: a
        // lease that is not a current v2 writer fence never reaches the
        // transport.
        let lease = self
            .store
            .check_seat_write(attachment.lease_id, attachment.task_id)
            .context("seat 写预检失败（lease 非当前或非 v2 fence）")?;
        let question = self
            .store
            .seat_question(durable_id)?
            .with_context(|| format!("持久问题记录不存在：{durable_id}"))?;
        let seat = self
            .store
            .seat(attachment.seat_record_id)?
            .with_context(|| format!("持久 seat 记录不存在：{}", attachment.seat_record_id))?;
        let task = self
            .store
            .get_task(attachment.task_id)?
            .with_context(|| format!("持久 task 记录不存在：{}", attachment.task_id))?;
        let candidate = candidate_from_fact(&request.semantic_question_id, &question.question);
        let expectation = SeatDecisionExpectation {
            question_revision: question.question_revision,
            question_record_revision: question.record_revision,
            seat_incarnation: seat.incarnation,
            seat_revision: seat.record_revision,
            task_revision: task.task_revision,
            lease_epoch: lease.lease_epoch,
        };
        let decision = SeatAnswerDecisionRecord {
            schema_version: 2,
            decision_id: EntityId::new(),
            question_id: durable_id,
            seat_id: attachment.seat_record_id,
            task_id: attachment.task_id,
            lease_id: attachment.lease_id,
            lease_epoch: lease.lease_epoch,
            fence_capability: lease.fence_capability.clone(),
            expected_question_revision: expectation.question_revision,
            expected_question_record_revision: expectation.question_record_revision,
            expected_seat_incarnation: expectation.seat_incarnation,
            expected_seat_revision: expectation.seat_revision,
            expected_task_revision: expectation.task_revision,
            deciding_actor: request.deciding_actor.clone(),
            delegation_grant_ref: request.delegation_grant_ref.clone(),
            authority_revision: request.authority_revision,
            answer: SeatAnswerFact {
                option_id: request.answer.option_id.clone(),
                text: request.answer.text.clone(),
                allow: request.answer.allow,
                reason: request.answer.reason.clone(),
            },
            exact_effect_input_digest: String::new(),
            transport: None,
            confirmation: None,
            closure_evidence: None,
            operation_id: None,
            state: SeatAnswerDecisionState::Committed,
            record_revision: RecordRevision(0),
            created_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
            updated_commit: CommitRef::new(EntityId::nil(), LedgerSeq::ZERO),
        };
        // 1. The durable claim: no input has been sent yet, and a stale
        //    question, seat, task or writer epoch is refused here.
        let decision = self
            .store
            .commit_seat_answer_decision(&expectation, decision)?;
        // The claim moved the question to `decision_committed`, which bumped
        // its row revision. Every later compare-and-swap uses the revision
        // that the claim actually produced.
        let question = self
            .store
            .seat_question(durable_id)?
            .with_context(|| format!("持久问题记录不存在：{durable_id}"))?;

        let fence = SeatWriteFence {
            lease_id: attachment.lease_id,
            lease_epoch: lease.lease_epoch,
            fence_capability: lease.fence_capability.clone(),
            seat_id: attachment.seat_record_id,
            seat_incarnation: seat.incarnation,
            task_id: attachment.task_id,
            task_revision: task.task_revision,
            question_id: durable_id,
            question_revision: question.question_revision,
            question_record_revision: question.record_revision,
        };

        // 2. The exact bytes. `None` means the decision cannot be mapped to
        //    an effect without guessing: nothing is written.
        let (plan, refusal) = match adapter.plan_effect(&handle, &candidate, &request.answer).await {
            Ok(Some(plan)) => (Some(plan), None),
            Ok(None) => (
                None,
                Some("无法在不猜测的前提下把这个决定映射为远端输入；未写入任何内容".to_string()),
            ),
            Err(error) => (
                None,
                Some(format!("无法把这个决定映射为远端输入：{error:#}")),
            ),
        };

        let Some(plan) = plan else {
            let reason = refusal.unwrap_or_default();
            let (decision, question) = self.store.complete_seat_answer_decision(
                decision.decision_id,
                SeatAnswerOutcome::NotSent { reason: reason.clone() },
            )?;
            return Ok(SeatRespondOutcome {
                response: SeatResponse {
                    adapter: adapter.adapter_id(),
                    transport: crate::seat::AnswerTransport::None,
                    confirmation: AnswerConfirmation::NotSent { reason: reason.clone() },
                    detail: None,
                },
                decision_id: decision.decision_id,
                question_id: question.question_id,
                decision_state: decision.state,
                question_state: question.state,
                effect_input_digest: String::new(),
                refusal: Some(reason),
            });
        };

        // 3. Re-check the live state right before the write.
        let preflight = adapter
            .preflight(&handle, &candidate, &plan)
            .await
            .map_err(|error| anyhow::anyhow!("seat preflight 失败：{error:#}"))?;
        if let SeatPreflight::Refused { kind, reason } = preflight {
            // The question may be gone on the remote side; that is a durable
            // fact about the question, not only a refusal of this write.
            if kind == PreflightRefusalKind::QuestionGone {
                let _ = self.store.mark_seat_question_state(
                    durable_id,
                    question.record_revision,
                    &[SeatQuestionState::DecisionCommitted],
                    SeatQuestionState::ExternallyResolved,
                );
            }
            let (decision, question) = self.store.complete_seat_answer_decision(
                decision.decision_id,
                SeatAnswerOutcome::NotSent { reason: reason.clone() },
            )?;
            return Ok(SeatRespondOutcome {
                response: SeatResponse {
                    adapter: adapter.adapter_id(),
                    transport: plan.transport,
                    confirmation: AnswerConfirmation::NotSent { reason: reason.clone() },
                    detail: Some("现场复核拒绝了这次写入；没有向远端发送任何内容".into()),
                },
                decision_id: decision.decision_id,
                question_id: question.question_id,
                decision_state: decision.state,
                question_state: question.state,
                effect_input_digest: String::new(),
                refusal: Some(reason),
            });
        }

        // 4. Arm the exact effect: from here it may have been sent.
        let armed = self.store.arm_seat_answer_decision(
            decision.decision_id,
            question.record_revision,
            plan.digest.clone(),
            request.operation_id,
        )?;

        // 5. The transport, outside every database transaction.
        let response = match adapter.execute_effect(&handle, &plan, &fence).await {
            Ok(response) => response,
            Err(error) => {
                // The write may or may not have reached the remote side: the
                // outcome is unknown, and it is never replayed.
                let reason = format!("传输结果未知：{error:#}");
                let (decision, question) = self.store.complete_seat_answer_decision(
                    armed.decision_id,
                    SeatAnswerOutcome::AnswerUnknown { reason: reason.clone() },
                )?;
                return Ok(SeatRespondOutcome {
                    response: SeatResponse {
                        adapter: adapter.adapter_id(),
                        transport: plan.transport,
                        confirmation: AnswerConfirmation::Unknown { reason: reason.clone() },
                        detail: None,
                    },
                    decision_id: decision.decision_id,
                    question_id: question.question_id,
                    decision_state: decision.state,
                    question_state: question.state,
                    effect_input_digest: plan.digest,
                    refusal: Some(reason),
                });
            }
        };

        // 6. The evidence the remote side produced.
        let outcome = match &response.confirmation {
            AnswerConfirmation::Confirmed => SeatAnswerOutcome::Answered {
                evidence: response
                    .detail
                    .clone()
                    .unwrap_or_else(|| "远端确认了这次回答".into()),
            },
            AnswerConfirmation::Unconfirmed => SeatAnswerOutcome::AnswerUnknown {
                reason: response
                    .detail
                    .clone()
                    .unwrap_or_else(|| "写入完成但没有观察到远端确认".into()),
            },
            AnswerConfirmation::Unknown { reason } => {
                SeatAnswerOutcome::AnswerUnknown { reason: reason.clone() }
            }
            AnswerConfirmation::Rejected { reason } => {
                SeatAnswerOutcome::Rejected { reason: reason.clone() }
            }
            AnswerConfirmation::NotSent { reason } => {
                SeatAnswerOutcome::NotSent { reason: reason.clone() }
            }
        };
        let (decision, question) = match self
            .store
            .complete_seat_answer_decision(armed.decision_id, outcome)
        {
            Ok(closed) => closed,
            Err(error) => {
                // The effect was already sent; the durable close-out failed.
                // The decision stays `armed`, which is the honest fact: the
                // effect may have been applied and is never replayed.
                let reason = format!("效果已送出，但持久化收尾失败：{error:#}");
                return Ok(SeatRespondOutcome {
                    response,
                    decision_id: armed.decision_id,
                    question_id: durable_id,
                    decision_state: armed.state,
                    question_state: SeatQuestionState::Responding,
                    effect_input_digest: armed.exact_effect_input_digest,
                    refusal: Some(reason),
                });
            }
        };
        let refusal = match decision.state {
            SeatAnswerDecisionState::Answered => None,
            SeatAnswerDecisionState::AnswerUnknown => Some(
                decision
                    .closure_evidence
                    .clone()
                    .unwrap_or_else(|| "答案是否生效未知".into()),
            ),
            SeatAnswerDecisionState::Rejected => decision.closure_evidence.clone(),
            SeatAnswerDecisionState::NotSent => decision.closure_evidence.clone(),
            SeatAnswerDecisionState::Committed | SeatAnswerDecisionState::Armed => None,
        };
        Ok(SeatRespondOutcome {
            response,
            decision_id: decision.decision_id,
            question_id: question.question_id,
            decision_state: decision.state,
            question_state: question.state,
            effect_input_digest: decision.exact_effect_input_digest,
            refusal,
        })
    }

    /// Type free text into a seat (steer), under the same writer fence.
    pub async fn submit(&self, location_id: &str, text: &str) -> Result<SeatResponse> {
        let (adapter, handle, attachment) = self.live(location_id).await?;
        let lease = self
            .store
            .check_seat_write(attachment.lease_id, attachment.task_id)
            .context("seat 写预检失败（lease 非当前或非 v2 fence）")?;
        let seat = self
            .store
            .seat(attachment.seat_record_id)?
            .with_context(|| format!("持久 seat 记录不存在：{}", attachment.seat_record_id))?;
        let task = self
            .store
            .get_task(attachment.task_id)?
            .with_context(|| format!("持久 task 记录不存在：{}", attachment.task_id))?;
        let fence = SeatWriteFence {
            lease_id: attachment.lease_id,
            lease_epoch: lease.lease_epoch,
            fence_capability: lease.fence_capability.clone(),
            seat_id: attachment.seat_record_id,
            seat_incarnation: seat.incarnation,
            task_id: attachment.task_id,
            task_revision: task.task_revision,
            question_id: EntityId::nil(),
            question_revision: 0,
            question_record_revision: RecordRevision(0),
        };
        adapter.submit(&handle, text, &fence).await
    }

    /// Interrupt the remote agent (Ctrl-C class), under the same writer fence.
    pub async fn interrupt(&self, location_id: &str) -> Result<SeatResponse> {
        let (adapter, handle, attachment) = self.live(location_id).await?;
        let lease = self
            .store
            .check_seat_write(attachment.lease_id, attachment.task_id)
            .context("seat 写预检失败（lease 非当前或非 v2 fence）")?;
        let seat = self
            .store
            .seat(attachment.seat_record_id)?
            .with_context(|| format!("持久 seat 记录不存在：{}", attachment.seat_record_id))?;
        let task = self
            .store
            .get_task(attachment.task_id)?
            .with_context(|| format!("持久 task 记录不存在：{}", attachment.task_id))?;
        let fence = SeatWriteFence {
            lease_id: attachment.lease_id,
            lease_epoch: lease.lease_epoch,
            fence_capability: lease.fence_capability.clone(),
            seat_id: attachment.seat_record_id,
            seat_incarnation: seat.incarnation,
            task_id: attachment.task_id,
            task_revision: task.task_revision,
            question_id: EntityId::nil(),
            question_revision: 0,
            question_record_revision: RecordRevision(0),
        };
        adapter.interrupt(&handle, &fence).await
    }

    /// Detach a seat. PTY seats observe their close-time facts; other
    /// adapters simply stop being reachable through the runtime.
    pub async fn detach(&self, location_id: &str) -> Result<()> {
        let seat = self.seats.lock().await.remove(location_id);
        if let Some(seat) = seat {
            if let Some(pty) = seat
                .adapter
                .as_any()
                .downcast_ref::<crate::seat::ssh_pty::SshPtySeat>()
            {
                pty.close().await;
            }
        }
        Ok(())
    }
}

fn fact_from_candidate(question: &SeatQuestionCandidate) -> SeatQuestionFact {
    SeatQuestionFact {
        source_kind: question.source_kind.as_str().to_string(),
        question_kind: question.question_kind.as_str().to_string(),
        native_request_id: question.native_request_id.clone(),
        native_tool_use_id: question.native_tool_use_id.clone(),
        native_payload: question.native_payload.clone(),
        title: question.title.clone(),
        text: question.text.clone(),
        options: question
            .options
            .iter()
            .map(|option| SeatOptionFact {
                option_id: option.option_id.clone(),
                label: option.label.clone(),
                meaning: option.meaning.clone(),
                native_value: option.native_value.clone(),
            })
            .collect(),
        selected_option: question.selected_option.clone(),
        confidence: SeatConfidenceFact {
            level: question.confidence.level.as_str().to_string(),
            basis: question.confidence.basis.clone(),
        },
        unknown: question.unknown.clone(),
        observed_screen_revision: question.observed_screen_revision,
        expires_at_ms: question.expires_at_ms,
        fingerprint: question_fingerprint(question),
    }
}

fn candidate_from_fact(
    semantic_id: &str,
    fact: &SeatQuestionFact,
) -> SeatQuestionCandidate {
    SeatQuestionCandidate {
        question_id: semantic_id.to_string(),
        source_kind: crate::seat::QuestionSourceKind::parse(&fact.source_kind)
            .unwrap_or(crate::seat::QuestionSourceKind::TerminalInference),
        question_kind: crate::seat::QuestionKind::parse(&fact.question_kind)
            .unwrap_or(crate::seat::QuestionKind::Information),
        native_request_id: fact.native_request_id.clone(),
        native_tool_use_id: fact.native_tool_use_id.clone(),
        native_payload: fact.native_payload.clone(),
        title: fact.title.clone(),
        text: fact.text.clone(),
        options: fact
            .options
            .iter()
            .map(|option| crate::seat::SeatOption {
                option_id: option.option_id.clone(),
                label: option.label.clone(),
                meaning: option.meaning.clone(),
                native_value: option.native_value.clone(),
            })
            .collect(),
        selected_option: fact.selected_option.clone(),
        confidence: crate::seat::Confidence {
            level: crate::seat::ConfidenceLevel::parse(&fact.confidence.level)
                .unwrap_or(crate::seat::ConfidenceLevel::Low),
            basis: fact.confidence.basis.clone(),
        },
        unknown: fact.unknown.clone(),
        observed_screen_revision: fact.observed_screen_revision,
        expires_at_ms: fact.expires_at_ms,
    }
}
