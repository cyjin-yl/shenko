import { message as m } from './messages';
import type { ActionReceiptView, Submission } from './projection';
import { isObject, validRevision } from './live-validation';

const SUBMISSION_KEY_PREFIX = 'shenko.submission.v2.';
const ACTION_KEY_PREFIX = 'shenko.action.v2.';
function submissionKey(submissionId: string): string {
  return `${SUBMISSION_KEY_PREFIX}${submissionId}`;
}

// Each submission persists under its own key so concurrent tabs never
// overwrite one another's unconfirmed IDs — the UI outbox is a per-submission
// recovery record, never an authoritative whole-array ledger.
export function saveSubmission(submission: Submission): boolean {
  try {
    localStorage.setItem(submissionKey(submission.payload.submission_id), JSON.stringify({
      payload: submission.payload,
      created_at: submission.created_at,
      receipt: submission.receipt,
      send_epoch: submission.send_epoch,
    }));
    return true;
  } catch {
    return false;
  }
}

export function retireSubmission(submission: Submission): void {
  // Only a committed disposition retires the persisted pending payload: it is
  // the sole fact whose truth is already durable in the Store ledger. Every
  // non-committed row keeps ID+payload until an explicit retry (same-key
  // overwrite) or a real durable admission — an unrelated newer draft must
  // never strand the only copy of a rejected/conflicting/absent input.
  try {
    localStorage.removeItem(submissionKey(submission.payload.submission_id));
  } catch {
    // Storage is optional; the in-memory row retires regardless.
  }
}

export function storedSubmissions(): Submission[] {
  const restored: Submission[] = [];
  try {
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index);
      if (!key || !key.startsWith(SUBMISSION_KEY_PREFIX)) continue;
      const saved: unknown = JSON.parse(localStorage.getItem(key) ?? 'null');
      if (!isObject(saved) || !isObject(saved.payload)) continue;
      const payload = saved.payload;
      if (
        payload.schema_version !== 2 || payload.type !== 'submit' || payload.mode !== 'append' ||
        typeof payload.submission_id !== 'string' || !payload.submission_id ||
        payload.submission_id !== key.slice(SUBMISSION_KEY_PREFIX.length) ||
        typeof payload.message_id !== 'string' || !payload.message_id ||
        typeof payload.subject_id !== 'string' || !payload.subject_id ||
        typeof payload.content !== 'string' || !payload.content.trim() ||
        !Array.isArray(payload.attachment_refs) || payload.attachment_refs.length !== 0 ||
        typeof saved.created_at !== 'string' || !Number.isFinite(Date.parse(saved.created_at))
      ) continue;
      restored.push({
        payload: {
          schema_version: 2, type: 'submit', mode: 'append',
          submission_id: payload.submission_id, message_id: payload.message_id,
          subject_id: payload.subject_id, content: payload.content, attachment_refs: [],
          ...(typeof payload.observed_step_id === 'string'
            ? { observed_step_id: payload.observed_step_id } : {}),
        },
        created_at: saved.created_at,
        receipt: saved.receipt === 'conflict' ? 'conflict'
          : saved.receipt === 'rejected' ? 'rejected' : 'unknown',
        detail: saved.receipt === 'conflict'
          ? m('restore.conflict')
          : saved.receipt === 'rejected'
          ? m('restore.rejected')
          : m('restore.submission'),
        send_epoch: validRevision(saved.send_epoch) ? saved.send_epoch : 0,
      });
    }
  } catch {
    // Browser storage is optional; an unreadable entry recovers as absent,
    // never as an admitted fact.
  }
  return restored.sort((left, right) => Date.parse(left.created_at) - Date.parse(right.created_at));
}

/// One admitted action click, persisted before the transport can send it. It
/// shares the server's web-submission ledger, so a reopen can reconcile the
/// same `submission_id` instead of admitting the same decision twice.
export interface PendingAction {
  submission_id: string;
  subject_id: string;
  document_id: string;
  action_id: string;
  revision: string;
  nonce: string;
  payload: Record<string, unknown>;
  created_at: string;
  send_epoch: number;
}

function actionKey(submissionId: string): string {
  return `${ACTION_KEY_PREFIX}${submissionId}`;
}

export function saveAction(action: PendingAction): boolean {
  try {
    localStorage.setItem(actionKey(action.submission_id), JSON.stringify(action));
    return true;
  } catch {
    return false;
  }
}

export function retireAction(action: PendingAction): void {
  try {
    localStorage.removeItem(actionKey(action.submission_id));
  } catch {
    // Storage is optional; the in-memory receipt retires regardless.
  }
}

export function storedActions(): Map<string, PendingAction> {
  const restored = new Map<string, PendingAction>();
  try {
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index);
      if (!key || !key.startsWith(ACTION_KEY_PREFIX)) continue;
      const saved: unknown = JSON.parse(localStorage.getItem(key) ?? 'null');
      if (!isObject(saved) || !isObject(saved.payload)) continue;
      if (
        typeof saved.submission_id !== 'string' || !saved.submission_id ||
        saved.submission_id !== key.slice(ACTION_KEY_PREFIX.length) ||
        typeof saved.subject_id !== 'string' || !saved.subject_id ||
        typeof saved.document_id !== 'string' || !saved.document_id ||
        typeof saved.action_id !== 'string' || !saved.action_id ||
        typeof saved.revision !== 'string' || !saved.revision ||
        typeof saved.nonce !== 'string' || !saved.nonce ||
        typeof saved.created_at !== 'string' || !Number.isFinite(Date.parse(saved.created_at))
      ) continue;
      restored.set(saved.submission_id, {
        submission_id: saved.submission_id,
        subject_id: saved.subject_id,
        document_id: saved.document_id,
        action_id: saved.action_id,
        revision: saved.revision,
        nonce: saved.nonce,
        payload: saved.payload,
        created_at: saved.created_at,
        send_epoch: validRevision(saved.send_epoch) ? saved.send_epoch : 0,
      });
    }
  } catch {
    // An unreadable record recovers as absent, never as an admitted action.
  }
  return restored;
}

export function initialActionReceipts(actions: Map<string, PendingAction>): ActionReceiptView[] {
  return [...actions.values()]
    .sort((left, right) => Date.parse(left.created_at) - Date.parse(right.created_at))
    .map((action) => ({
      submission_id: action.submission_id,
      document_id: action.document_id,
      action_id: action.action_id,
      revision: action.revision,
      stage: 'sending' as const,
      detail: m('restore.action'),
    }));
}
