import { message as m, text as t } from './messages';
import {
  UI_INTENT_SCHEMA,
  UI_PROJECTION_SCHEMA,
  buildProjection,
  semanticHash,
  type ProjectionController,
  type DocumentErrorView,
  type ServerCapabilities,
  type Submission,
  type SubmissionPayload,
  type PendingAttachment,
  type WebAttachmentRef,
  type UiIntent,
  type UiProjection,
  type ViewName,
} from './projection';
import { applyDisplayIntent, DRAFT_KEY, storageSet } from './ui-preferences';
import { saveSubmission, saveAction, storedActions, type PendingAction } from './pending-inputs';
import { initialState } from './live-state';
import { isObject, validRevision } from './live-validation';
import { SocketTransport } from './socket-transport';
import { LiveReadModels } from './live-read-models';
import { TimelineReducer } from './timeline-reducer';
import { InputReducer } from './input-reducer';
import { UiApi } from './ui-api';

const RECEIPT_REFRESH_MS = 2000;
const VALID_VIEWS: Record<ViewName, true> = { timeline: true, tasks: true, memory: true, config: true, playground: true };

function uid(): string {
  if (globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function') {
    return globalThis.crypto.randomUUID();
  }
  return `id-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

export function createLiveController(): ProjectionController {
  return new LiveController();
}

class LiveController implements ProjectionController {
  private state = initialState();
  // Server history is readable without becoming this browser's pending outbox.
  private localOutbox = new Set(this.state.submissions.map((row) => row.payload.submission_id));
  private listeners = new Set<() => void>();
  private revision = 0;
  private cached: UiProjection;
  private readonly api = new UiApi();
  private readonly readModels: LiveReadModels;
  private readonly timeline: TimelineReducer;
  private readonly inputs: InputReducer;
  private readonly transport: SocketTransport;
  private disposed = false;
  private receiptTimer: number | null = null;
  /// One durable origin identity per (key, exact desired bytes): pressing save
  /// again for the same value resolves the original change instead of
  /// submitting a second one.
  private configOrigins = new Map<string, string>();
  /** One stable request id per (work, action, revision). */
  private workRequests = new Map<string, string>();
  /// Actions whose admission is still open, keyed by the durable submission
  /// ID the Store knows. Restored before the socket connects so a reopen
  /// reconciles the original decision instead of admitting a second one.
  private pendingActions = storedActions();

  constructor() {
    this.inputs = new InputReducer(this.state, this.localOutbox, this.pendingActions,
      () => this.emit(), () => this.scheduleReceiptRefresh());
    this.readModels = new LiveReadModels(this.state, this.api, () => this.emit(), {
      applySubmissionView: (submission, data, persist) => this.inputs.applySubmissionView(submission, data, persist),
      refreshSubmissions: () => this.refreshSubmissions(),
      scheduleReceiptRefresh: () => this.scheduleReceiptRefresh(),
    });
    this.timeline = new TimelineReducer(this.state, () => this.emit(), this.readModels);
    this.transport = new SocketTransport({
      connection: (connection, seconds) => {
        this.state.connection = connection;
        this.state.reconnect_seconds = seconds;
        this.emit();
      },
      countdown: (seconds) => { this.state.reconnect_seconds = seconds; this.emit(); },
      opened: () => this.onOpen(),
      closed: () => this.onClose(),
      message: (event) => this.onMessage(event),
    });
    document.documentElement.dataset.theme = this.state.display.theme;
    document.documentElement.classList.toggle('dark', this.state.display.theme === 'dark');
    document.documentElement.classList.toggle('light', this.state.display.theme === 'light');
    this.cached = buildProjection(this.state, ++this.revision);
    this.transport.connect();
    void this.readModels.loadConfig();
    void this.readModels.loadSelf();
    void this.readModels.loadTasks();
    void this.readModels.loadMemory();
    // Works can be lenses beside the stream, so they are known from the start.
    void this.readModels.loadPlayground();
    void this.readModels.loadHealth();
    void this.readModels.loadManifest();
  }

  projection(): UiProjection {
    return this.cached;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(): void {
    this.cached = buildProjection(this.state, ++this.revision);
    this.listeners.forEach((listener) => listener());
  }

  dispatch(intent: UiIntent): void {
    if (this.disposed) return;
    const current = this.cached;
    const action = current.actions[intent.action_id];
    if (
      intent.schema_version !== UI_INTENT_SCHEMA ||
      intent.surface_id !== current.surface_id ||
      current.schema_version !== UI_PROJECTION_SCHEMA ||
      !action
    ) {
      return;
    }
    if (action.revision !== intent.action_revision) {
      this.state.display.notice = m('error.staleAction');
      this.emit();
      return;
    }
    if (!action.enabled) return;
    if (applyDisplayIntent(this.state, action.kind, intent.payload, () => this.emit())) return;

    switch (action.kind) {
      case 'view.select': {
        const view = intent.payload.view;
        if (typeof view === 'string' && Object.hasOwn(VALID_VIEWS, view)) {
          this.state.display.view = view as ViewName;
          this.state.display.panel_open = false;
          this.emit();
          if (view === 'tasks') void this.readModels.loadTasks();
          if (view === 'memory') void this.readModels.loadMemory();
          if (view === 'playground') void this.readModels.loadPlayground();
          if (view === 'config') {
            void this.readModels.loadConfig();
            void this.readModels.loadConfigEntries();
            void this.readModels.loadSelf();
            void this.readModels.loadHealth();
            void this.readModels.loadManifest();
          }
        }
        break;
      }
      case 'composer.attach': {
        const files = Array.isArray(intent.payload.files) ? intent.payload.files.filter((file): file is File => file instanceof File) : [];
        for (const file of files) void this.uploadAttachment(file);
        break;
      }
      case 'composer.detach': {
        const id = intent.payload.local_id;
        const removed = this.state.attachments.find((item) => item.local_id === id);
        if (removed?.preview) URL.revokeObjectURL(removed.preview);
        this.state.attachments = this.state.attachments.filter((item) => item.local_id !== id);
        this.emit();
        break;
      }
      case 'part.copy':
        void this.copyText(String(action.payload?.content ?? ''));
        break;
      case 'config.draft': {
        const entry = intent.payload.entry;
        const value = intent.payload.value;
        if (typeof entry === 'string' && typeof value === 'string') {
          this.state.config_drafts = { ...(this.state.config_drafts ?? {}), [entry]: value };
          this.emit();
        }
        break;
      }
      case 'config.apply':
        void this.applyConfigEntry(action.payload?.namespace, action.payload?.logical_key);
        break;
      case 'identity.name.draft':
        this.state.name_draft = typeof intent.payload?.value === 'string' ? intent.payload.value : '';
        this.emit();
        break;
      case 'identity.name':
        void this.nameInstance();
        break;
      case 'owner.claim.draft':
        this.state.claim_draft = typeof intent.payload?.value === 'string' ? intent.payload.value : '';
        this.emit();
        break;
      case 'owner.claim':
        void this.claimOwnership();
        break;
      case 'work.open':
        void this.readModels.openWork(action.payload?.artifact_id);
        break;
      case 'work.stage':
        void this.actOnWork('stage', action.payload?.artifact_id);
        break;
      case 'work.publish':
        void this.actOnWork('publish', action.payload?.artifact_id);
        break;
      case 'work.rollback':
        void this.actOnWork('rollback', action.payload?.artifact_id);
        break;
      case 'composer.append':
        // One send, as in Claude Code: with steering available the server
        // binds the message to the request in flight, if there is one, and
        // otherwise places it at the next boundary like any message.
        this.sendWithMode(this.state.capabilities.steer ? 'steer' : 'append');
        break;
      case 'submission.edit': {
        const id = action.payload?.submission_id;
        const submission = this.state.submissions.find((item) => item.payload.submission_id === id);
        if (submission && !submission.withdrawal) {
          this.sendWithdraw(submission);
          const draft = this.state.draft.trim();
          this.state.draft = draft ? `${submission.payload.content}\n${draft}` : submission.payload.content;
          storageSet(DRAFT_KEY, this.state.draft);
          this.emit();
        }
        break;
      }
      case 'composer.steer':
        this.sendSteer();
        break;
      case 'submission.withdraw': {
        const id = action.payload?.submission_id;
        const submission = this.state.submissions.find((row) => row.payload.submission_id === id);
        if (submission && !submission.withdrawal) this.sendWithdraw(submission);
        break;
      }
      case 'document.action': {
        const documentId = action.payload?.document_id;
        const actionId = action.payload?.action_id;
        const document = this.state.documents.find((row) => row.document_id === documentId);
        const option = document?.options.find((row) => row.action_id === actionId);
        if (document && option && this.state.subject_id) {
          this.sendAction(document.document_id, option);
        }
        break;
      }
      case 'submissions.load_older':
        void this.readModels.loadSubmissionHistory(this.state.submission_history?.next_before);
        break;
      case 'submission.retry': {
        const id = action.payload?.submission_id;
        const submission = this.state.submissions.find((row) => row.payload.submission_id === id);
        // A rejected or unknown-receipt row resends the SAME submission ID:
        // the server replays the original receipt instead of double-input.
        if (submission && (submission.receipt === 'rejected' || submission.receipt === 'unknown')) {
          this.sendSubmission(submission, true);
        }
        break;
      }
    }
  }

  /// Seal one file on the server before any message names it. The composer
  /// shows it as uploading, then ready with its sealed reference, or failed
  /// with the server's reason; nothing is sent until every file is ready.
  private async uploadAttachment(file: File): Promise<void> {
    const local_id = uid();
    const media_type = file.type || 'application/octet-stream';
    this.state.attachments = [...this.state.attachments, {
      local_id, name: file.name || 'file', media_type, bytes: file.size, status: 'uploading',
      preview: media_type.startsWith('image/') ? URL.createObjectURL(file) : undefined,
    }];
    this.emit();
    const settle = (patch: Partial<PendingAttachment>) => {
      this.state.attachments = this.state.attachments.map((item) => item.local_id === local_id ? { ...item, ...patch } : item);
      this.emit();
    };
    try {
      const response = await fetch(`/api/attachments?name=${encodeURIComponent(file.name || 'file')}`, {
        method: 'POST', headers: { 'content-type': media_type }, body: file,
      });
      const body = await response.json().catch(() => null) as { accepted?: boolean; attachment?: WebAttachmentRef; error?: string } | null;
      if (response.ok && body?.accepted && body.attachment) settle({ status: 'ready', ref: body.attachment });
      else settle({ status: 'failed', error: body?.error ?? `HTTP ${response.status}` });
    } catch (error) {
      settle({ status: 'failed', error: String((error as Error)?.message ?? error) });
    }
  }

  private async copyText(content: string): Promise<void> {
    let copied = false;
    try {
      if (navigator.clipboard && typeof navigator.clipboard.writeText === 'function') {
        await navigator.clipboard.writeText(content);
        copied = true;
      } else {
        copied = this.legacyCopy(content);
      }
    } catch {
      copied = this.legacyCopy(content);
    }
    this.state.display.notice = copied ? m('copy.visible') : m('copy.unavailable');
    this.emit();
  }

  private legacyCopy(content: string): boolean {
    try {
      const textarea = document.createElement('textarea');
      textarea.value = content;
      textarea.readOnly = true;
      textarea.style.position = 'fixed';
      textarea.style.opacity = '0';
      document.body.append(textarea);
      textarea.select();
      const copied = document.execCommand('copy');
      textarea.remove();
      return copied;
    } catch {
      return false;
    }
  }

  // Steer is admitted exactly like append; without a verified in-request
  // control capability the server's placement is next_boundary (UI02). The
  // client does not promise live injection.
  private sendSteer(): void {
    this.sendWithMode('steer');
  }

  private sendWithMode(mode: 'append' | 'steer'): void {
    const content = this.state.draft.trim();
    const subjectId = this.state.subject_id;
    const attachments = this.state.attachments;
    if (attachments.some((item) => item.status !== 'ready')) return;
    const refs = attachments.map((item) => item.ref).filter((ref): ref is WebAttachmentRef => Boolean(ref));
    if ((!content && refs.length === 0) || !subjectId || this.state.connection !== 'online') return;
    if (mode === 'steer' && !this.state.capabilities.steer) {
      this.state.display.notice = m('error.steerUnavailable');
      this.emit();
      return;
    }
    const latestStep = [...this.state.timeline].reverse().find((part) => part.step_id);
    const payload: SubmissionPayload = {
      schema_version: 2,
      type: 'submit',
      submission_id: uid(),
      message_id: uid(),
      subject_id: subjectId,
      mode,
      content,
      attachment_refs: refs,
      ...(latestStep ? { observed_step_id: latestStep.step_id } : {}),
    };
    const submission: Submission = {
      payload,
      created_at: new Date().toISOString(),
      receipt: 'sending',
      send_epoch: 0,
    };
    // Save the immutable ID and exact payload BEFORE the transport can send.
    // A page reopen after a lost ACK must recover this ID, not mint another.
    if (!saveSubmission(submission)) {
      this.state.display.notice = m('error.saveSubmission');
      this.emit();
      return;
    }
    this.localOutbox.add(payload.submission_id);
    // The files now belong to this saved submission; a retry resends them.
    for (const item of attachments) if (item.preview) URL.revokeObjectURL(item.preview);
    this.state.attachments = [];
    this.state.submissions = [...this.state.submissions, submission];
    this.state.display.reading = 'following_latest';
    this.state.display.new_count = 0;
    this.state.display.scroll_request += 1;
    this.sendSubmission(submission, false);
  }

  private sendSubmission(submission: Submission, retry: boolean): void {
    const socket = this.transport;
    if (!socket.isOpen) return;
    submission.send_epoch += 1;
    submission.receipt = 'sending';
    submission.detail = undefined;
    if (!saveSubmission(submission)) {
      submission.receipt = 'unknown';
      submission.detail = m('error.saveOriginal');
      this.emit();
      return;
    }
    this.localOutbox.add(submission.payload.submission_id);
    this.emit();
    try {
      socket.send(JSON.stringify(submission.payload));
      submission.receipt = 'sent';
      if (this.state.draft.trim() === submission.payload.content) {
        this.state.draft = '';
        storageSet(DRAFT_KEY, '');
      }
      this.state.display.notice = undefined;
      this.emit();
    } catch {
      submission.receipt = 'unknown';
      submission.detail = retry ? m('error.retryTransport') : m('error.submissionTransport');
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
      this.emit();
    }
    saveSubmission(submission);
    this.scheduleReceiptRefresh();
  }

  // Withdrawal is a durable control record, not a delete: the admitted input
  // stays in the ledger and the server reports withdrawal_queued vs
  // withdrawn_before_presentation through the next submission_view.
  private sendWithdraw(submission: Submission): void {
    const socket = this.transport;
    if (!socket.isOpen) return;
    if (!this.state.capabilities.withdraw) {
      this.state.display.notice = m('error.withdrawUnavailable');
      this.emit();
      return;
    }
    try {
      socket.send(
        JSON.stringify({
          schema_version: 2,
          type: 'withdraw',
          submission_id: submission.payload.submission_id,
          subject_id: submission.payload.subject_id,
          cancel_id: uid(),
        }),
      );
    } catch {
      // onclose schedules the reconnect; the pending record stays durable.
    }
  }

  // An action click admits a durable input; execution authority comes from
  // the subject's disposition of that input, never from the click.
  private sendAction(documentId: string, option: {
    action_id: string;
    nonce: string;
    revision: string;
    payload: Record<string, unknown>;
  }): void {
    const socket = this.transport;
    const subjectId = this.state.subject_id;
    if (!subjectId) {
      this.state.display.notice = m('error.subjectUnbound');
      this.emit();
      return;
    }
    if (!this.state.capabilities.actions) {
      this.state.display.notice = m('error.actionUnavailable');
      this.emit();
      return;
    }
    // The same (document, action, revision) may already be durably admitted
    // and merely awaiting disposition. A second click would admit a second
    // decision, so the pending receipt is shown instead of resending it.
    const open = this.state.action_receipts.find(
      (row) => row.document_id === documentId && row.action_id === option.action_id
        && ['sending', 'accepted', 'selected'].includes(row.stage),
    );
    if (open) {
      this.state.display.notice = open.stage === 'sending'
        ? m('action.awaiting')
        : m('action.alreadyAdmitted');
      this.emit();
      return;
    }
    const action: PendingAction = {
      submission_id: uid(),
      subject_id: subjectId,
      document_id: documentId,
      action_id: option.action_id,
      revision: option.revision,
      nonce: option.nonce,
      payload: option.payload,
      created_at: new Date().toISOString(),
      send_epoch: 0,
    };
    if (!saveAction(action)) {
      this.state.display.notice = m('error.saveAction');
      this.emit();
      return;
    }
    this.pendingActions.set(action.submission_id, action);
    this.inputs.pushActionReceipt({
      submission_id: action.submission_id,
      document_id: documentId,
      action_id: option.action_id,
      revision: option.revision,
      stage: 'sending',
    });
    if (!socket.isOpen) {
      // Not admitted: the transport never took it. Keep the durable ID so a
      // reconnect can resend this exact decision rather than mint a new one.
      this.inputs.setActionReceipt(action.submission_id, {
        stage: 'unavailable',
        detail: m('error.actionOffline'),
      });
      return;
    }
    try {
      socket.send(
        JSON.stringify({
          schema_version: 2,
          type: 'action',
          submission_id: action.submission_id,
          document_id: documentId,
          action_id: option.action_id,
          revision: option.revision,
          nonce: option.nonce,
          subject_id: subjectId,
          payload: option.payload,
        }),
      );
      action.send_epoch += 1;
      saveAction(action);
      this.scheduleReceiptRefresh();
    } catch {
      this.inputs.setActionReceipt(action.submission_id, {
        stage: 'unavailable',
        detail: m('error.actionTransport'),
      });
    }
  }

  private onOpen(): void {
      // Attest this connection's display facts so the server degrades
      // documents to what THIS surface can actually render.
      try {
        this.transport.send(
          JSON.stringify({
            schema_version: 2,
            type: 'client_hello',
            viewport: [window.innerWidth, window.innerHeight],
            language: navigator.language,
            theme: this.state.display.theme,
            reduced_motion: window.matchMedia?.('(prefers-reduced-motion: reduce)').matches === true,
          }),
        );
      } catch {
        // A failed attestation only means baseline capabilities; the socket
        // itself is still usable for committed replay and input receipts.
      }
      this.refreshSubmissions();
      void this.readModels.loadSelf();
  }

  private onClose(): void {
      if (this.receiptTimer !== null) window.clearTimeout(this.receiptTimer);
      this.receiptTimer = null;
      for (const submission of this.state.submissions) {
        if (submission.receipt === 'sending' || submission.receipt === 'sent') {
          submission.receipt = 'unknown';
          submission.detail = m('error.submissionDisconnected');
          saveSubmission(submission);
        }
      }
      // The action's admission result is unknown too, but its ID and payload
      // stay durable so the reconnect query settles the same decision.
      for (const row of this.state.action_receipts) {
        if (row.stage !== 'sending') continue;
        row.stage = 'unavailable';
        row.detail = m('error.actionDisconnected');
      }
  }

  private onMessage(event: MessageEvent): void {
    if (this.disposed || typeof event.data !== 'string') return;
    let data: unknown;
    try {
      data = JSON.parse(event.data);
    } catch {
      return;
    }
    if (!isObject(data)) return;
    try {
      if (data.type === 'hello') {
        if (typeof data.server_version === 'string') this.state.server_version = data.server_version;
        const capabilities = isObject(data.capabilities) ? data.capabilities : {};
        const caps: ServerCapabilities = {
          append: capabilities.append === true,
          steer: capabilities.steer === true,
          withdraw: capabilities.withdraw === true,
          actions: capabilities.actions === true,
          documents: capabilities.documents === true,
          attachments: capabilities.attachments === true,
          durable_input_receipts: capabilities.durable_input_receipts === true,
          committed_replay: capabilities.committed_replay === true,
          streaming: capabilities.streaming === true,
        };
        this.state.capabilities = caps;
        if (isObject(data.surface)) this.state.surface = data.surface;
        this.state.append_available = capabilities.append === true;
        this.state.durable_receipts = capabilities.durable_input_receipts === true;
        this.state.committed_replay = capabilities.committed_replay === true;
        // Streaming is real only when the server advertises a durable sink;
        // the client never fabricates a typewriter from this flag alone.
        this.state.streaming = caps.streaming;
        this.refreshSubmissions();
        void this.readModels.loadSubmissionHistory();
        this.scheduleReceiptRefresh();
        this.emit();
        return;
      }
      if (data.type === 'capabilities') {
        // Renegotiated surface facts (client_hello answer or server change).
        if (isObject(data.surface)) {
          this.state.surface = data.surface;
          const surface = data.surface as { streaming?: unknown };
          if (typeof surface.streaming === 'boolean') this.state.streaming = surface.streaming;
        }
        this.emit();
        return;
      }
      if (data.type === 'document') {
        this.timeline.handleDocument(data);
        return;
      }
      if (data.type === 'document_error') {
        if (typeof data.reason === 'string') {
          const view: DocumentErrorView = {
            source_step_id: String(data.source_step_id ?? ''),
            source_commit: String(data.source_commit ?? ''),
            reason: data.reason,
          };
          // Replay repeats every committed error; the reason is a fact about
          // that step, so an identical observation is not a second failure.
          const known = this.state.document_errors.some(
            (row) => row.source_step_id === view.source_step_id && row.reason === view.reason,
          );
          if (!known) {
            this.state.document_errors = [...this.state.document_errors, view].slice(-16);
          }
          this.state.display.notice = m('error.document', { detail: data.reason });
          this.emit();
        }
        return;
      }
      if (data.type === 'identity') {
        if (validRevision(data.revision)) {
          const shown = this.state.identity?.revision ?? null;
          this.state.wanted_identity_revision = Math.max(
            this.state.wanted_identity_revision ?? 0,
            data.revision,
          );
          if (shown === null || shown < this.state.wanted_identity_revision) void this.readModels.loadSelf();
          else this.emit();
        }
        return;
      }
      if (data.type === 'receipt') {
        this.inputs.handleReceipt(data);
        return;
      }
      if (data.type === 'submission_view') {
        this.inputs.handleSubmissionView(data);
        return;
      }
      if (data.type === 'timeline_part') {
        this.timeline.handleTimelinePart(data);
        return;
      }
      if (data.type === 'stream_part') {
        this.timeline.handleStreamPart(data);
      }
    } catch {
      // One malformed DTO or projection is contained; transport stays alive.
      this.state.display.notice = m('error.messageRender');
      this.emit();
    }
  }

  // Poll only while unresolved receipts exist and the real socket advertises
  // durable queries. Timeline traffic is not evidence of this input's S0/S1.
  private scheduleReceiptRefresh(): void {
    if (this.receiptTimer !== null || this.disposed || this.state.connection !== 'online'
      || !this.state.durable_receipts || !this.state.subject_id
      || !this.state.submissions.some((submission) =>
        submission.payload.subject_id === this.state.subject_id
        && ['sending', 'sent', 'unknown', 'accepted', 'selected'].includes(submission.receipt))
      && this.pendingActions.size === 0) return;
    this.receiptTimer = window.setTimeout(() => {
      this.receiptTimer = null;
      this.refreshSubmissions();
      this.scheduleReceiptRefresh();
    }, RECEIPT_REFRESH_MS);
  }

  /// Query the original durable key after reconnect and during live progress.
  private refreshSubmissions(): void {
    const socket = this.transport;
    if (!socket.isOpen || !this.state.durable_receipts) return;
    const subjectId = this.state.subject_id;
    if (!subjectId) return;
    for (const submission of this.state.submissions) {
      if (submission.payload.subject_id !== subjectId
        || !['sending', 'sent', 'unknown', 'accepted', 'selected'].includes(submission.receipt)) {
        continue;
      }
      try {
        socket.send(
          JSON.stringify({
            schema_version: 2,
            type: 'query_submission',
            submission_id: submission.payload.submission_id,
            subject_id: submission.payload.subject_id,
            send_epoch: submission.send_epoch,
          }),
        );
      } catch {
        // A dead socket triggers onclose and schedules reconnect on its own.
      }
    }
    // Actions share the same ledger and the same query message; the original
    // decision key is what the Store matches, so no second admission happens.
    for (const action of this.pendingActions.values()) {
      if (action.subject_id !== subjectId) continue;
      try {
        socket.send(
          JSON.stringify({
            schema_version: 2,
            type: 'query_submission',
            submission_id: action.submission_id,
            subject_id: action.subject_id,
            send_epoch: action.send_epoch,
          }),
        );
      } catch {
        // onclose schedules the reconnect; the record stays durable.
      }
    }
  }

  /// Submit one panel edit through the same durable change contract the
  /// subject's `config.manage` tool uses. The saved revision, the apply
  /// receipt and each consumer's adoption stay separate facts: a rejected or
  /// unadopted change is reported as such, never as success.
  private async applyConfigEntry(namespace: unknown, logicalKey: unknown): Promise<void> {
    if (typeof namespace !== 'string' || typeof logicalKey !== 'string') return;
    const key = `${namespace}/${logicalKey}`;
    const entry = this.state.config_entries?.find(
      (row) => row.namespace === namespace && row.logical_key === logicalKey,
    );
    const text = this.state.config_drafts?.[key]
      ?? (entry?.desired_value === undefined ? '' : JSON.stringify(entry.desired_value));
    let desired: unknown;
    try {
      desired = JSON.parse(text);
    } catch {
      this.state.display.notice = m('error.configJson', { key });
      this.emit();
      return;
    }
    const originKey = `${key}:${semanticHash(JSON.stringify(desired))}`;
    if (!this.configOrigins.has(originKey)) {
      this.configOrigins.set(originKey, `panel-${originKey}-${Date.now().toString(36)}`);
    }
    try {
      const data = await this.api.post('/api/config/apply', {
        namespace,
        logical_key: logicalKey,
        desired,
        origin_identity: this.configOrigins.get(originKey),
        expected_base_revision: entry?.entry?.desired_revision ?? 0,
      });
      if (!isObject(data) || data.accepted !== true) {
        const code = isObject(data) && typeof data.code === 'string' ? data.code : t('config.rejected');
        this.state.display.notice = m('config.outcome', { key, outcome: code });
      } else {
        const change = isObject(data.change) ? data.change : undefined;
        const state = change && typeof change.state === 'string' ? change.state : 'unknown';
        const receipt = change && isObject(change.receipt) && typeof change.receipt.outcome === 'string'
          ? change.receipt.outcome
          : undefined;
        this.state.display.notice = m('config.applied', { key, submission: t(data.replayed === true ? 'config.replayed' : 'timeline.committed'), state, receipt: receipt ? ` · ${receipt}` : '' });
        // The saved revision now exists; the next edit is a new intent.
        this.configOrigins.delete(originKey);
        const drafts = { ...(this.state.config_drafts ?? {}) };
        delete drafts[key];
        this.state.config_drafts = drafts;
      }
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.display.notice = m('error.configUnconfirmed', { key, detail: error instanceof Error ? error.message : String(error) });
    }
    if (!this.disposed) this.emit();
    await this.readModels.loadConfigEntries();
  }

  /// One action on the opened work.
  ///
  /// The action names the revision the reader saw and a request id that is
  /// stable for that (work, action, revision): a repeated submit is the same
  /// request, and the server's revision binding is what keeps it from
  /// producing a second effect.
  private async actOnWork(op: string, artifactId: unknown): Promise<void> {
    if (typeof artifactId !== 'string' || artifactId === '') return;
    const work = this.state.work;
    if (!work || work.artifact_id !== artifactId) {
      this.state.display.notice = m('work.openFirst');
      this.emit();
      return;
    }
    const revision = work.revision ?? 0;
    const key = `${artifactId}:${op}:${revision}`;
    if (!this.workRequests.has(key)) {
      this.workRequests.set(key, `work-${key}-${Date.now().toString(36)}`);
    }
    const requestId = this.workRequests.get(key) as string;
    try {
      if (op === 'publish') {
        const candidate = work.candidates?.at(-1);
        if (!candidate) {
          this.state.display.notice = m('work.noCandidate');
          this.emit();
          return;
        }
        await this.api.post(`/playground/${artifactId}/publish`, {
          manifest_digest: candidate.digest,
          revision,
          request_id: requestId,
        });
      } else if (op === 'rollback') {
        const source = work.revisions?.filter((entry) => entry !== revision).at(-1);
        if (source === undefined) {
          this.state.display.notice = m('work.noOlderRevision');
          this.emit();
          return;
        }
        await this.api.post(`/playground/${artifactId}/rollback`, {
          revision: source,
          request_id: requestId,
        });
      } else {
        await this.api.post(`/playground/${artifactId}/stage`, { revision, request_id: requestId });
      }
      this.state.display.notice = m('work.actionRecorded');
      await this.readModels.openWork(artifactId);
      void this.readModels.loadPlayground();
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.display.notice = m('error.workAction', { detail: error instanceof Error ? error.message : String(error) });
      this.emit();
    }
  }

  /// Name the instance (可由用户取名，或让 Agent 自主取名).
  ///
  /// The name goes through the same identity policy the subject's own naming
  /// uses; the server answers with the new revision or says why not.
  private async nameInstance(): Promise<void> {
    const name = this.state.name_draft?.trim() ?? '';
    if (name === '') {
      this.state.display.notice = m('identity.empty');
      this.emit();
      return;
    }
    try {
      const data = await this.api.post('/api/identity/name', { name });
      if (!isObject(data) || data.accepted !== true) {
        const code = isObject(data) && typeof data.code === 'string' ? data.code : t('identity.rejected');
        this.state.display.notice = m('identity.outcome', { outcome: code });
        this.emit();
        return;
      }
      this.state.display.notice = data.changed === true ? m('identity.saved') : m('identity.unchanged');
      this.state.name_draft = '';
      await this.readModels.loadSelf();
      this.emit();
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.display.notice = m('identity.error', { detail: error instanceof Error ? error.message : String(error) });
      this.emit();
    }
  }

  /// Claim this installation.
  ///
  /// The capability is what the installation issued; the server answers with
  /// the outcome it really reached.
  private async claimOwnership(): Promise<void> {
    const capability = this.state.claim_draft?.trim() ?? '';
    if (capability === '') {
      this.state.display.notice = m('owner.empty');
      this.emit();
      return;
    }
    try {
      const data = await this.api.post('/api/owner/claim', { capability });
      const outcome = isObject(data) && typeof data.outcome === 'string' ? data.outcome : t('common.unknown');
      this.state.display.notice = outcome === 'granted' ? m('owner.granted') : m('owner.outcome', { outcome });
      this.state.claim_draft = '';
      await this.readModels.loadOwnerState();
      this.emit();
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.display.notice = m('owner.error', { detail: error instanceof Error ? error.message : String(error) });
      this.emit();
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.transport.dispose();
    this.readModels.dispose();
    this.timeline.dispose();
    // The timer belongs to the browser's window; a controller disposed after
    // its environment is gone must still be able to stop its own timer.
    if (this.receiptTimer !== null) {
      const clear = typeof window !== 'undefined' ? window.clearTimeout.bind(window) : clearTimeout;
      clear(this.receiptTimer);
    }
    this.receiptTimer = null;
    this.api.dispose();
    this.listeners.clear();
  }
}
