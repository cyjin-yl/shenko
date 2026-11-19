import { message as m } from './messages';
import type { ActionReceiptView, Submission, UiState } from './projection';
import { DRAFT_KEY, storageSet } from './ui-preferences';
import { saveSubmission, retireSubmission, retireAction, type PendingAction } from './pending-inputs';
import { admissionSequence, isObject, validRevision } from './live-validation';

const MAX_ACTION_RECEIPTS = 16;

/** Reduces Store admission/selection/disposition facts without dispatching an effect. */
export class InputReducer {
  constructor(private readonly state: UiState, private readonly localOutbox: Set<string>,
    private readonly pendingActions: Map<string, PendingAction>, private readonly emit: () => void,
    private readonly scheduleRefresh: () => void,
    private readonly editReady: (submission: Submission) => void) {}

  private editSource(submission: Submission): Submission | undefined {
    return this.state.submissions.find(
      (source) => source.edit?.submission_id === submission.payload.submission_id);
  }

  pushActionReceipt(receipt: ActionReceiptView): void {
    const rest = this.state.action_receipts.filter(
      (row) => row.submission_id !== receipt.submission_id,
    );
    this.state.action_receipts = [...rest, receipt].slice(-MAX_ACTION_RECEIPTS);
    this.emit();
  }


  setActionReceipt(
    submissionId: string,
    patch: Partial<Omit<ActionReceiptView, 'submission_id'>>,
  ): void {
    let found = false;
    this.state.action_receipts = this.state.action_receipts.map((row) => {
      if (row.submission_id !== submissionId) return row;
      found = true;
      return { ...row, ...patch };
    });
    if (!found) return;
    this.emit();
  }


  private retirePendingAction(submissionId: string): void {
    const action = this.pendingActions.get(submissionId);
    if (!action) return;
    this.pendingActions.delete(submissionId);
    retireAction(action);
  }


  handleReceipt(data: Record<string, unknown>): void {
    if (data.schema_version !== 2 || typeof data.submission_id !== 'string') return;
    const submission = this.state.submissions.find(
      (row) => row.payload.submission_id === data.submission_id,
    );
    if (!submission) {
      this.handleActionReceipt(data);
      return;
    }
    const detail = typeof data.detail === 'string' ? data.detail : undefined;
    if (isObject(data.placement)) {
      const placement = data.placement;
      submission.placement = {
        requested: placement.requested === 'steer' ? 'steer' : 'append',
        actual: placement.actual === 'next_boundary'
          || placement.actual === 'live_steer'
          || placement.actual === 'cancel_reseal'
          ? placement.actual
          : 'pending',
        ...(typeof placement.target_request_id === 'string'
          ? { target_request_id: placement.target_request_id }
          : {}),
      };
    }
    if (data.stage === 'accepted' && typeof data.event_id === 'string'
      && typeof data.accepted_commit === 'string') {
      // Durable acceptance, not selection or disposition. The same ID replayed
      // returns this same receipt; the durable fact is not a progress bar.
      // Replaying an acceptance must not erase a later, observed disposition.
      if (submission.receipt !== 'selected' && submission.receipt !== 'committed') {
        submission.receipt = 'accepted';
      }
      submission.event_id = typeof data.event_id === 'string' ? data.event_id : submission.event_id;
      submission.accepted_commit =
        typeof data.accepted_commit === 'string' ? data.accepted_commit : submission.accepted_commit;
      submission.accepted_seq = admissionSequence(data.accepted_commit) ?? submission.accepted_seq;
      if (submission.receipt === 'accepted') submission.detail = detail;
    } else if (data.stage === 'unavailable') {
      if (!submission.accepted_commit) submission.receipt = 'unknown';
      submission.detail = detail ?? m('error.receiptUnavailable');
    } else if (data.stage === 'conflict') {
      submission.receipt = 'conflict';
      submission.detail = detail ?? m('error.submissionConflict');
      if (!submission.edit && !this.editSource(submission) && !this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    } else if (data.stage === 'rejected') {
      submission.receipt = 'rejected';
      submission.detail = detail ?? m('error.submissionRejected');
      if (!submission.edit && !this.editSource(submission) && !this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    }
    // Non-committed outcomes stay persisted: they are the only copy of this
    // input's identity and payload until the user retries or the Store holds
    // a committed disposition for the original event.
    if (this.localOutbox.has(submission.payload.submission_id)) saveSubmission(submission);
    this.scheduleRefresh();
    this.emit();
  }


  // An admitted action answers with the same receipt shape as a text input,
  // minus the placement facts. Its stage is admission evidence only: the click
  // never claims the action ran.
  private handleActionReceipt(data: Record<string, unknown>): void {
    const submissionId = String(data.submission_id);
    const action = this.pendingActions.get(submissionId);
    const existing = this.state.action_receipts.find(
      (row) => row.submission_id === submissionId,
    );
    if (!action || !existing) return;
    const detail = typeof data.detail === 'string' ? data.detail : undefined;
    const eventId = typeof data.event_id === 'string' ? data.event_id : undefined;
    const acceptedCommit = typeof data.accepted_commit === 'string'
      ? data.accepted_commit
      : undefined;
    if (data.stage === 'accepted') {
      // A replayed acceptance must not erase an observed disposition.
      if (!['selected', 'committed'].includes(existing.stage)) {
        this.setActionReceipt(submissionId, {
          stage: 'accepted',
          detail: detail ?? m('action.durable'),
          ...(eventId ? { event_id: eventId } : {}),
          ...(acceptedCommit ? { accepted_commit: acceptedCommit } : {}),
        });
      }
    } else if (data.stage === 'conflict') {
      this.setActionReceipt(submissionId, {
        stage: 'conflict',
        detail: detail ?? m('error.actionConflict'),
      });
      this.retirePendingAction(submissionId);
    } else if (data.stage === 'rejected') {
      this.setActionReceipt(submissionId, {
        stage: 'rejected',
        detail: detail ?? m('error.actionRejected'),
      });
      this.state.display.notice = detail ?? m('error.actionNotAdmitted');
      // Nothing durable exists for this ID: the option may be pressed again
      // as a new decision, so the stored record is not kept.
      this.retirePendingAction(submissionId);
      this.emit();
    } else if (data.stage === 'unavailable') {
      this.setActionReceipt(submissionId, {
        stage: 'unavailable',
        detail: detail ?? m('error.actionReceiptUnavailable'),
      });
    }
    this.scheduleRefresh();
  }


  handleSubmissionView(data: Record<string, unknown>): void {
    if (data.schema_version !== 2 || typeof data.submission_id !== 'string') return;
    const submission = this.state.submissions.find(
      (row) => row.payload.submission_id === data.submission_id,
    );
    if (!submission) {
      this.handleActionView(data);
      return;
    }
    // Correlate the view with the send attempt that produced it. A stale
    // not_admitted racing a newer retry must not restore the draft or demote
    // a fact already returned by that retry.
    if (validRevision(data.send_epoch) && data.send_epoch < submission.send_epoch) {
      return;
    }
    if (submission.receipt === 'conflict') return;
    this.applySubmissionView(submission, data, this.localOutbox.has(submission.payload.submission_id));
    this.scheduleRefresh();
    this.emit();
  }


  applySubmissionView(submission: Submission, data: Record<string, unknown>, persist: boolean): void {
    if (data.schema_version !== 2 || data.submission_id !== submission.payload.submission_id) return;
    if (data.result === 'found' && typeof data.event_id === 'string'
      && data.event_id.length > 0 && typeof data.accepted_commit === 'string' && data.accepted_commit.length > 0) {
      const withdrawal = data.withdrawal;
      if (isObject(withdrawal)
        && typeof withdrawal.cancel_id === 'string' && withdrawal.cancel_id.length > 0
        && typeof withdrawal.commit_ref === 'string' && withdrawal.commit_ref.length > 0
        && (withdrawal.result === 'withdrawal_queued'
          || withdrawal.result === 'withdrawn_before_presentation')) {
        submission.withdrawal = {
          cancel_id: withdrawal.cancel_id,
          commit_ref: withdrawal.commit_ref,
          result: withdrawal.result,
        };
        if (submission.payload.subject_id === this.state.subject_id
          && submission.edit?.state === 'pending' && withdrawal.cancel_id === submission.edit.cancel_id
          && withdrawal.result === 'withdrawn_before_presentation') {
          submission.edit.state = 'ready';
          if (saveSubmission(submission)) this.editReady(submission);
          else {
            submission.edit.state = 'pending';
            this.state.display.notice = m('error.saveOriginal');
          }
        }
        if (submission.edit?.state === 'pending' && withdrawal.cancel_id === submission.edit.cancel_id
          && withdrawal.result === 'withdrawal_queued') {
          submission.edit.state = 'closed';
          if (!saveSubmission(submission)) {
            submission.edit.state = 'pending';
            this.state.display.notice = m('error.saveOriginal');
          } else {
            this.state.display.notice = 'Withdrawal was queued after selection. The original message and attachments remain saved; no edit was put in the composer.';
          }
        }
      }
      submission.event_id = typeof data.event_id === 'string' ? data.event_id : submission.event_id;
      submission.accepted_commit =
        typeof data.accepted_commit === 'string' ? data.accepted_commit : submission.accepted_commit;
      submission.accepted_seq = admissionSequence(data.accepted_commit) ?? submission.accepted_seq;
      if (typeof data.selected_step_id === 'string') submission.selected_step_id = data.selected_step_id;
      if (data.stage === 'committed' && typeof data.committed_at === 'string') {
        submission.receipt = 'committed';
        submission.committed_at =
          typeof data.committed_at === 'string' ? data.committed_at : submission.committed_at;
        submission.detail = undefined;
        const source = this.editSource(submission);
        retireSubmission(submission, source);
        if (!submission.edit || ['closed', 'retired'].includes(submission.edit.state)) {
          this.localOutbox.delete(submission.payload.submission_id);
        }
        if (source?.edit?.state === 'retired') this.localOutbox.delete(source.payload.submission_id);
        return;
      } else if (data.stage === 'selected' && typeof data.selected_step_id === 'string') {
        if (submission.receipt !== 'committed') submission.receipt = 'selected';
        submission.selected_step_id =
          typeof data.selected_step_id === 'string' ? data.selected_step_id : submission.selected_step_id;
        if (submission.receipt === 'selected') submission.detail = m('receipt.sealed');
      } else if (data.stage === 'accepted') {
        if (submission.receipt !== 'selected' && submission.receipt !== 'committed') {
          submission.receipt = 'accepted';
          submission.detail = undefined;
        }
      }
    } else if (data.result === 'not_admitted') {
      // A durable query proves the input was never admitted; the stable ID is
      // free to retry with identical content. The pending record is kept —
      // it is the only copy of this attempt until the user acts.
      submission.receipt = 'rejected';
      submission.detail = m('error.submissionAbsent');
      if (persist && !submission.edit && !this.editSource(submission) && !this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    } else if (data.result === 'temporarily_unavailable') {
      if (!submission.accepted_commit) submission.receipt = 'unknown';
      submission.detail = m('error.queryUnavailable');
    }
    // Query failure is not not_admitted and never restores an accepted draft.
    if (persist && this.localOutbox.has(submission.payload.submission_id)) saveSubmission(submission);
  }


  // The same durable query answers for an admitted action. Its view proves
  // admission/selection/disposition and never re-runs the action.
  private handleActionView(data: Record<string, unknown>): void {
    const submissionId = String(data.submission_id);
    const action = this.pendingActions.get(submissionId);
    const existing = this.state.action_receipts.find(
      (row) => row.submission_id === submissionId,
    );
    if (!action || !existing) return;
    if (validRevision(data.send_epoch) && data.send_epoch < action.send_epoch) return;
    const eventId = typeof data.event_id === 'string' ? data.event_id : undefined;
    const acceptedCommit = typeof data.accepted_commit === 'string'
      ? data.accepted_commit
      : undefined;
    if (data.result === 'found' && eventId && acceptedCommit) {
      if (data.stage === 'committed') {
        this.setActionReceipt(submissionId, {
          stage: 'committed',
          detail: m('action.disposition'),
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
        // The disposition is durable; the pending copy is no longer needed.
        this.retirePendingAction(submissionId);
      } else if (data.stage === 'selected') {
        this.setActionReceipt(submissionId, {
          stage: 'selected',
          detail: m('action.sealed'),
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
      } else {
        this.setActionReceipt(submissionId, {
          stage: 'accepted',
          detail: existing.detail ?? m('action.durable'),
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
      }
    } else if (data.result === 'not_admitted') {
      this.setActionReceipt(submissionId, {
        stage: 'rejected',
        detail: m('error.actionAbsent'),
      });
      this.retirePendingAction(submissionId);
    } else if (data.result === 'temporarily_unavailable') {
      this.setActionReceipt(submissionId, {
        stage: 'unavailable',
        detail: m('error.actionQueryUnavailable'),
      });
    }
    this.scheduleRefresh();
  }


}
