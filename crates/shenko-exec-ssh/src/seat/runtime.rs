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
//!
//! After a process restart, [`SeatRuntime::reattach_active_seats`] rebuilds
//! this registry from the durable records (R070): transports that outlive
//! the process (a local tmux pane) are re-discovered under the recorded
//! writer lease, and `durable_questions` is re-seeded by recomputing each
//! stored question's semantic id. Managed processes and PTYs die with the
//! process that spawned them; their records stay durable-only. [`observe`]
//! additionally adopts a still-open stored question by fingerprint match,
//! so re-observing one never writes a duplicate record.

use crate::seat::{
    AnswerConfirmation, PreflightRefusalKind, QuestionSourceKind, SeatAdapter, SeatAnswer,
    SeatHandle, SeatObservation, SeatOption, SeatPreflight, SeatQuestionCandidate, SeatResponse,
    SeatWriteFence, inferred_question_id, question_fingerprint,
};
use anyhow::{Context, Result, ensure};
use shenko_store::{
    SeatAnswerDecisionRecord, SeatAnswerDecisionState, SeatAnswerFact, SeatAnswerOutcome,
    SeatConfidenceFact, SeatDecisionExpectation, SeatLocation, SeatOptionFact, SeatQuestionFact,
    SeatQuestionRecord, SeatQuestionState, SeatRecord, SeatStatus, SeatFleetEntry, Store,
};
use shenko_types::commit::CommitRef;
use shenko_types::ids::{EntityId, LedgerSeq, RecordRevision};
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct SeatFleetView {
    #[serde(flatten)]
    pub seat: SeatFleetEntry,
    /// None means an attached transport could not be checked. Presence in the
    /// runtime registry alone never justifies a claim that it is alive.
    pub transport_live: Option<bool>,
    pub durable_only: bool,
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

/// What one startup reattachment pass did — SPEC §17.1, R070.
#[derive(Debug, Default)]
pub struct SeatReattachReport {
    /// Live seats now reachable through the runtime again.
    pub reattached: Vec<SeatHandle>,
    /// Seats whose incarnation was bumped before reattach: the pane's
    /// recorded occupant was provably replaced in place, so every fence
    /// recorded under the old incarnation now refuses to write.
    pub incarnation_bumped: Vec<EntityId>,
    /// Seats that could not be reattached, with the reason. The durable
    /// records stay readable; nothing was rewritten, retired or re-opened.
    pub skipped: Vec<(EntityId, String)>,
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

    /// Read durable task seats and observe the transports already attached to
    /// this runtime. No attach, lease acquisition, input or question mutation.
    pub async fn fleet_view(&self, task_id: Option<EntityId>) -> Result<(Vec<SeatFleetView>, bool)> {
        let mut durable = self.store.seat_fleet_summary(task_id)?;
        let truncated = durable.len() > shenko_store::SEAT_FLEET_LIMIT;
        durable.truncate(shenko_store::SEAT_FLEET_LIMIT);
        let attached: BTreeMap<_, _> = self.seats.lock().await.values()
            .filter(|live| durable.iter().any(|seat| seat.seat_id == live.attachment.seat_record_id))
            .map(|live| (live.attachment.seat_record_id,
                (Arc::clone(&live.adapter), live.handle.clone())))
            .collect();
        let mut views = Vec::with_capacity(durable.len());
        for mut seat in durable {
            let transport_live = match attached.get(&seat.seat_id) {
                None => Some(false),
                Some((adapter, handle)) => {
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        adapter.transport_live(handle),
                    ).await {
                        Ok(Ok(live)) => {
                            seat.last_observed_at = Some(chrono::Utc::now());
                            Some(live)
                        }
                        // A failed/deadline-limited observation is unknown,
                        // not evidence that a remote process stopped.
                        Ok(Err(_)) | Err(_) => None,
                    }
                }
            };
            views.push(SeatFleetView {
                seat,
                transport_live,
                durable_only: transport_live != Some(true),
            });
        }
        Ok((views, truncated))
    }

    /// Rebind one task's durable seats to live adapters after a restart —
    /// SPEC §17.1, R070.
    ///
    /// For every *active* seat record of `task_id` the current writer lease
    /// is reused verbatim (no re-acquisition, so the lease can never displace
    /// a writer recorded elsewhere), and `durable_questions` is rebuilt from
    /// the durable question records: the semantic question id is recomputed
    /// deterministically from the stored fact (see
    /// [`semantic_question_id_for_fact`]), and [`SeatRuntime::observe`]
    /// additionally adopts any still-open stored question by fingerprint
    /// match, so a re-observed question never creates a duplicate durable
    /// record and `respond` can answer questions recorded before the
    /// restart.
    ///
    /// Only transports that outlive this process can be reattached: a local
    /// tmux pane. Managed OMP/Claude processes and SSH PTYs die with the
    /// process that spawned them, so their records stay durable-only and are
    /// reported as skipped — nothing is fabricated, retired or re-opened.
    /// When the pane's foreground program provably changed in place the
    /// seat's incarnation is bumped first, so every fence recorded under the
    /// old occupant refuses to write before this seat is observable again.
    pub async fn reattach_from_store(&self, task_id: EntityId) -> Result<SeatReattachReport> {
        let mut report = SeatReattachReport::default();
        for record in self.store.seat_task_seats(task_id)? {
            self.reattach_record(record, &mut report).await;
        }
        Ok(report)
    }

    /// Startup rebuild: reattach every active seat record in the store,
    /// regardless of which task owns it (R070).
    pub async fn reattach_active_seats(&self) -> Result<SeatReattachReport> {
        let mut report = SeatReattachReport::default();
        for record in self.store.active_seat_records()? {
            self.reattach_record(record, &mut report).await;
        }
        Ok(report)
    }

    async fn reattach_record(&self, record: SeatRecord, report: &mut SeatReattachReport) {
        let skip = |report: &mut SeatReattachReport, reason: String| {
            report.skipped.push((record.seat_id, reason));
        };
        if record.status != SeatStatus::Active {
            return skip(report, format!("seat retired at {}", record.updated_commit.seq.0));
        }
        let SeatLocation::Tmux { host, pane_id, current_command, .. } = &record.location else {
            // The managed process (OMP, Claude) or PTY belonged to the
            // process that spawned it; the durable record is the authority
            // and a live transport is never re-opened implicitly.
            return skip(report, "managed transport died with the previous process; durable record only".into());
        };
        if host != "local" {
            return skip(report, "remote tmux reattach needs an SSH binding the location does not carry".into());
        }
        let Some(lease) = (match self.store.seat_current_lease(record.seat_id) {
            Ok(lease) => lease,
            Err(error) => return skip(report, format!("lease lookup failed: {error:#}")),
        }) else {
            return skip(report, "no current writer lease".into());
        };
        if !lease.is_writer_fence() || lease.holder_task_id != record.task_id {
            return skip(report, "current lease is not a writer fence for this task".into());
        }
        let adapter: Arc<dyn SeatAdapter> = Arc::new(crate::seat::tmux::TmuxSeatAdapter::local());
        let discovered = match adapter.discover().await {
            Ok(handles) => handles,
            Err(error) => return skip(report, format!("discover failed: {error:#}")),
        };
        let Some(handle) = discovered.into_iter().find(|h| h.location_id == *pane_id) else {
            return skip(report, format!("pane {pane_id} not found on the local tmux server"));
        };
        if handle.current_command.as_deref() != Some(current_command.as_str()) {
            // The pane id was reused: the recorded occupant is gone, and the
            // remote program is provably a different one. That is the
            // in-place location change the incarnation field exists for —
            // bumping it stales every lease/fence recorded under the old
            // occupant before this seat becomes reachable again.
            match self.store.bump_seat_incarnation(record.seat_id) {
                Ok(_) => report.incarnation_bumped.push(record.seat_id),
                Err(error) => {
                    return skip(report, format!("incarnation bump failed: {error:#}"));
                }
            }
        }
        let mut live_seat = LiveSeat {
            adapter,
            attachment: SeatAttachment {
                task_id: record.task_id,
                seat_record_id: record.seat_id,
                lease_id: lease.lease_id,
            },
            handle: handle.clone(),
            durable_questions: Mutex::new(BTreeMap::new()),
        };
        self.rebind_durable_questions(&record, &handle, &mut live_seat).await;
        let mut seats = self.seats.lock().await;
        if seats.contains_key(&handle.location_id) {
            return skip(report, format!("location {} already attached", handle.location_id));
        }
        seats.insert(handle.location_id.clone(), live_seat);
        report.reattached.push(handle);
    }

    /// Rebuild `durable_questions` for a reattached seat from the durable
    /// question records. The semantic id is recomputed deterministically —
    /// the fingerprint-level identity is the stored
    /// `SeatQuestionFact.fingerprint` produced by `question_fingerprint`
    /// over kind/text/options/request ids, while the semantic id is derived
    /// by `semantic_question_id_for_fact` (the `inferred_question_id` hash
    /// for terminal-inferred questions, the adapter's request id for native
    /// ones). Records whose source never produced a recomputable semantic
    /// id are left for `observe`'s fingerprint adoption instead of being
    /// mapped under a fabricated key.
    async fn rebind_durable_questions(
        &self,
        record: &SeatRecord,
        handle: &SeatHandle,
        live_seat: &mut LiveSeat,
    ) {
        let questions = match self.store.seat_questions(record.seat_id) {
            Ok(questions) => questions,
            Err(error) => {
                tracing::warn!(
                    seat_id = %record.seat_id,
                    error = format!("{error:#}"),
                    "seat reattach: durable questions could not be read"
                );
                return;
            }
        };
        let mut durable = live_seat.durable_questions.lock().await;
        for question in questions {
            if question.state.is_closed() {
                continue;
            }
            if let Some(semantic_id) = semantic_question_id_for_fact(handle, &question.question) {
                durable.insert(semantic_id, question.question_id);
            }
        }
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
            // The semantic id is a live-adapter key, never a durable fact:
            // after a restart (or before any rebind ran) the durable map can
            // be empty even though this exact question is already recorded.
            // Fingerprint equality — the same `question_fingerprint` the
            // adapter computes — is the stored identity of the question, so
            // a still-open durable record with the same fingerprint is this
            // question and is adopted instead of duplicated.
            let mut existing = durable
                .get(&question.question_id)
                .and_then(|id| self.store.seat_question(*id).ok().flatten());
            if existing.is_none() {
                existing = self
                    .store
                    .seat_questions(attachment.seat_record_id)?
                    .into_iter()
                    .find(|record| {
                        record.question.fingerprint == fingerprint && !record.state.is_closed()
                    })
                    .inspect(|record| {
                        durable.insert(question.question_id.clone(), record.question_id);
                    });
            }
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
                question: fact_from_candidate(question)?,
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
        let candidate = candidate_from_fact(&request.semantic_question_id, &question.question)?;
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

    /// Detach a seat. PTY and managed RPC seats observe their close-time
    /// facts and stop their child process; other adapters simply stop being
    /// reachable through the runtime.
    pub async fn detach(&self, location_id: &str) -> Result<()> {
        let seat = self.seats.lock().await.remove(location_id);
        if let Some(seat) = seat {
            let any = seat.adapter.as_ref() as &dyn std::any::Any;
            if let Some(pty) = any.downcast_ref::<crate::seat::ssh_pty::SshPtySeat>() {
                pty.close().await;
            } else if let Some(omp) = any.downcast_ref::<crate::seat::omp_rpc::OmpRpcSeat>() {
                omp.close().await;
            } else if let Some(claude) =
                any.downcast_ref::<crate::seat::claude_sdk::ClaudeSdkSeat>()
            {
                claude.close().await;
            }
        }
        Ok(())
    }
}

fn fact_from_candidate(question: &SeatQuestionCandidate) -> Result<SeatQuestionFact> {
    Ok(SeatQuestionFact {
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
        expires_at_ms: question.expires_at_ms.map(u64::try_from).transpose()
            .context("seat expiry precedes Unix epoch")?,
        fingerprint: question_fingerprint(question),
    })
}

fn candidate_from_fact(
    semantic_id: &str,
    fact: &SeatQuestionFact,
) -> Result<SeatQuestionCandidate> {
    Ok(SeatQuestionCandidate {
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
        expires_at_ms: fact.expires_at_ms.map(i64::try_from).transpose()
            .context("seat expiry exceeds adapter timestamp range")?,
    })
}

/// Recompute the semantic question id one durable fact was observed under.
///
/// The reattached runtime needs the live map key, not just the durable id.
/// Terminal-inferred questions carry their identity in the content itself:
/// [`inferred_question_id`] hashes the seat identity plus the trimmed text
/// and option ids, so the same stored fact recomputes the exact id the
/// adapter assigned. Native-RPC and SDK-callback questions name their
/// remote request in `native_request_id`; each adapter keys its candidates
/// with a fixed prefix over that id. A fact that fits neither shape returns
/// `None` — it is adopted later by fingerprint match on re-observation,
/// never guessed.
fn semantic_question_id_for_fact(
    handle: &SeatHandle,
    fact: &SeatQuestionFact,
) -> Option<String> {
    match QuestionSourceKind::parse(&fact.source_kind) {
        Some(QuestionSourceKind::TerminalInference) => Some(inferred_question_id(
            handle,
            &fact.text,
            &fact
                .options
                .iter()
                .map(|option| SeatOption {
                    option_id: option.option_id.clone(),
                    label: option.label.clone(),
                    meaning: option.meaning.clone(),
                    native_value: option.native_value.clone(),
                })
                .collect::<Vec<_>>(),
        )),
        Some(QuestionSourceKind::NativeRpc) => fact
            .native_request_id
            .as_ref()
            .map(|id| format!("omp-ui:{id}")),
        Some(QuestionSourceKind::SdkCallback) => fact.native_request_id.as_ref().map(|id| {
            if fact.title.as_deref() == Some("AskUserQuestion") {
                format!("claude-ask:{id}")
            } else {
                format!("claude-perm:{id}")
            }
        }),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seat::SeatAdapterId;

    fn handle() -> SeatHandle {
        SeatHandle {
            location_id: "%3".into(),
            adapter: SeatAdapterId::Tmux,
            adapter_version: "tmux-capture-v1".into(),
            host: "local".into(),
            cwd: Some("/project".into()),
            remote_session_id: None,
            current_command: Some("omp".into()),
            process_identity: None,
        }
    }

    fn fact(source_kind: &str) -> SeatQuestionFact {
        SeatQuestionFact {
            source_kind: source_kind.into(),
            question_kind: "planning".into(),
            native_request_id: None,
            native_tool_use_id: None,
            native_payload: None,
            title: None,
            text: "Continue?".into(),
            options: vec![SeatOptionFact {
                option_id: "yes".into(),
                label: "yes".into(),
                meaning: None,
                native_value: None,
            }],
            selected_option: None,
            confidence: SeatConfidenceFact {
                level: "medium".into(),
                basis: "observed".into(),
            },
            unknown: vec![],
            observed_screen_revision: 7,
            expires_at_ms: None,
            fingerprint: String::new(),
        }
    }

    /// The reattach map key must be the same deterministic identity the
    /// adapter assigned live, so a restarted subject answers the very
    /// question that was recorded — never a fabricated duplicate.
    #[test]
    fn terminal_inference_facts_recompute_their_semantic_id() {
        let handle = handle();
        let fact = fact("terminal_inference");
        let semantic = semantic_question_id_for_fact(&handle, &fact).unwrap();
        let expected = inferred_question_id(
            &handle,
            "Continue?",
            &[SeatOption {
                option_id: "yes".into(),
                label: "yes".into(),
                meaning: None,
                native_value: None,
            }],
        );
        assert_eq!(semantic, expected);
    }

    /// Native request ids rebind under the adapter's own key prefix; a fact
    /// without a request id is adopted later by fingerprint, never guessed.
    #[test]
    fn native_facts_rebind_by_their_request_id() {
        let handle = handle();
        let mut native = fact("native_rpc");
        native.native_request_id = Some("req-9".into());
        assert_eq!(
            semantic_question_id_for_fact(&handle, &native).as_deref(),
            Some("omp-ui:req-9")
        );
        let mut callback = fact("sdk_callback");
        callback.native_request_id = Some("r2".into());
        assert_eq!(
            semantic_question_id_for_fact(&handle, &callback).as_deref(),
            Some("claude-perm:r2")
        );
        callback.title = Some("AskUserQuestion".into());
        assert_eq!(
            semantic_question_id_for_fact(&handle, &callback).as_deref(),
            Some("claude-ask:r2")
        );
        callback.native_request_id = None;
        assert_eq!(semantic_question_id_for_fact(&handle, &callback), None);
    }

    #[tokio::test]
    async fn fleet_distinguishes_durable_attached_and_exited_seats_without_a_writer() {
        let dir = std::env::temp_dir().join(format!("shenko-fleet-{}", EntityId::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let task_id = EntityId::new();
        let record = store.register_seat(
            EntityId::new(), task_id, "omp_rpc", "omp-rpc-v1",
            SeatLocation::OmpRpc {
                host: "local".into(),
                remote_session_id: "private-command --secret=must-not-be-disclosed".into(),
            },
        ).unwrap();
        let other = store.register_seat(
            EntityId::new(), EntityId::new(), "omp_rpc", "omp-rpc-v1",
            SeatLocation::OmpRpc { host: "local".into(), remote_session_id: "other".into() },
        ).unwrap();
        let runtime = SeatRuntime::new(Arc::clone(&store));
        let (all, all_truncated) = runtime.fleet_view(None).await.unwrap();
        assert!(!all_truncated);
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|view| view.seat.task_id == other.task_id));
        let (durable, truncated) = runtime.fleet_view(Some(task_id)).await.unwrap();
        assert!(!truncated);
        assert_eq!(durable.len(), 1);
        assert_eq!(durable[0].seat.seat_id, record.seat_id);
        assert_ne!(durable[0].seat.seat_id, other.seat_id);
        assert_eq!(durable[0].transport_live, Some(false));
        assert!(durable[0].durable_only);
        assert_eq!(durable[0].seat.last_observed_at, None);
        assert!(!serde_json::to_string(&durable).unwrap().contains("must-not-be-disclosed"));

        // A real owned child, not a protocol-readiness or model fixture.
        let adapter = Arc::new(crate::seat::omp_rpc::OmpRpcSeat::spawn(
            vec!["sh".into(), "-c".into(), "cat >/dev/null".into()], None,
        ).await.unwrap());
        runtime.attach(adapter.clone(), SeatAttachment {
            task_id, seat_record_id: record.seat_id, lease_id: EntityId::nil(),
        }).await.unwrap();
        let (attached, _) = runtime.fleet_view(Some(task_id)).await.unwrap();
        assert_eq!(attached[0].transport_live, Some(true));
        assert!(!attached[0].durable_only);
        assert!(attached[0].seat.last_observed_at.is_some());
        assert!(store.seat_current_lease(record.seat_id).unwrap().is_none());

        adapter.close().await;
        let (ended, _) = runtime.fleet_view(Some(task_id)).await.unwrap();
        assert_eq!(ended[0].transport_live, Some(false), "a retained handle is not a live process");
        assert!(ended[0].durable_only);
        assert_eq!(store.seat(record.seat_id).unwrap().unwrap().record_revision, record.record_revision);
        assert!(store.seat_current_lease(record.seat_id).unwrap().is_none());
        drop(runtime);
        drop(adapter);
        drop(store);
        let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
        let restarted = SeatRuntime::new(reopened);
        let (view, _) = restarted.fleet_view(Some(task_id)).await.unwrap();
        assert_eq!(view[0].seat.seat_id, record.seat_id);
        assert!(view[0].durable_only);
        assert_eq!(view[0].seat.last_observed_at, None, "no observation time is invented on reopen");
    }
}
