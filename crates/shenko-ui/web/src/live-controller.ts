import {
  UI_INTENT_SCHEMA,
  UI_PROJECTION_SCHEMA,
  buildProjection,
  type BuildManifest,
  type ConnectionState,
  type DisplayState,
  type IdentityView,
  type ProjectionController,
  type ReadingState,
  type Submission,
  type SubmissionPayload,
  type TimelinePart,
  type UiIntent,
  type UiProjection,
  type UiState,
  type ViewName,
} from './projection';

const DRAFT_KEY = 'shenko.draft.v1';
const SUBMISSION_KEY_PREFIX = 'shenko.submission.v2.';
const THEME_KEY = 'shenko.theme.v1';
const RECEIPT_REFRESH_MS = 2000;
const RECONNECT_BASE = [1, 2, 4, 8, 15];
const VALID_VIEWS = new Set<ViewName>(['timeline', 'tasks', 'memory', 'config']);
const VALID_READING = new Set<ReadingState>([
  'following_latest',
  'reading_history',
  'selecting_or_interacting',
]);

function storageGet(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

function storageSet(key: string, value: string): void {
  try {
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
    // Browser storage is optional; the editor and transport keep working.
  }
}

function uid(): string {
  if (globalThis.crypto && typeof globalThis.crypto.randomUUID === 'function') {
    return globalThis.crypto.randomUUID();
  }
  return `id-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}

function submissionKey(submissionId: string): string {
  return `${SUBMISSION_KEY_PREFIX}${submissionId}`;
}

// Each submission persists under its own key so concurrent tabs never
// overwrite one another's unconfirmed IDs — the UI outbox is a per-submission
// recovery record, never an authoritative whole-array ledger.
function saveSubmission(submission: Submission): boolean {
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

function retireSubmission(submission: Submission): void {
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

function storedSubmissions(): Submission[] {
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
          ? '原提交 ID 已关联不同内容；此冲突内容未改写原输入。'
          : saved.receipt === 'rejected'
          ? '服务端已确认未接纳此提交；可用同一 ID 重发。'
          : '已恢复原提交 ID 与内容；等待持久查询，不会生成新 ID 重发。',
        send_epoch: validRevision(saved.send_epoch) ? saved.send_epoch : 0,
      });
    }
  } catch {
    // Browser storage is optional; an unreadable entry recovers as absent,
    // never as an admitted fact.
  }
  return restored.sort((left, right) => Date.parse(left.created_at) - Date.parse(right.created_at));
}

function initialTheme(): 'dark' | 'light' {
  const query = new URLSearchParams(location.search).get('theme');
  if (query === 'light' || query === 'dark') return query;
  return storageGet(THEME_KEY) === 'light' ? 'light' : 'dark';
}

function initialState(): UiState {
  const display: DisplayState = {
    view: 'timeline',
    panel_open: false,
    open_sections: { status: true, activity: true, artifacts: false, devices: false },
    theme: initialTheme(),
    reading: 'following_latest',
    new_count: 0,
    scroll_request: 0,
  };
  return {
    source: 'live',
    connection: 'connecting',
    reconnect_seconds: 0,
    append_available: false,
    durable_receipts: false,
    committed_replay: false,
    subject_id: null,
    wanted_identity_revision: null,
    step: null,
    timeline: [],
    submissions: storedSubmissions(),
    draft: storageGet(DRAFT_KEY),
    config_error: false,
    self_error: false,
    tasks_error: false,
    memory_error: false,
    display,
  };
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function validRevision(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function wsUrl(): string {
  if (!location.host) return '';
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${location.host}/ws`;
}

export function createLiveController(): ProjectionController {
  return new LiveController();
}

class LiveController implements ProjectionController {
  private state = initialState();
  private listeners = new Set<() => void>();
  private revision = 0;
  private cached: UiProjection;
  private socket: WebSocket | null = null;
  private disposed = false;
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private countdownTimer: ReturnType<typeof setInterval> | null = null;
  private identityTimer: ReturnType<typeof setTimeout> | null = null;
  private receiptTimer: number | null = null;
  private identityRetryDelay = 1000;
  private selfSeq = 0;
  private selfApplied = 0;
  private abort = new AbortController();
  private pendingChunks = new Map<string, Map<number, Record<string, unknown>>>();

  constructor() {
    document.documentElement.dataset.theme = this.state.display.theme;
    document.documentElement.classList.toggle('dark', this.state.display.theme === 'dark');
    document.documentElement.classList.toggle('light', this.state.display.theme === 'light');
    this.cached = buildProjection(this.state, ++this.revision);
    this.connect();
    void this.loadConfig();
    void this.loadSelf();
    void this.loadTasks();
    void this.loadMemory();
    void this.loadHealth();
    void this.loadManifest();
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
      this.state.display.notice = '内容已变化，旧动作已拒绝；请基于当前 revision 重试。';
      this.emit();
      return;
    }
    if (!action.enabled) return;

    switch (action.kind) {
      case 'view.select': {
        const view = intent.payload.view;
        if (typeof view === 'string' && VALID_VIEWS.has(view as ViewName)) {
          this.state.display.view = view as ViewName;
          this.state.display.panel_open = false;
          this.emit();
          if (view === 'tasks') void this.loadTasks();
          if (view === 'memory') void this.loadMemory();
          if (view === 'config') {
            void this.loadConfig();
            void this.loadSelf();
            void this.loadHealth();
            void this.loadManifest();
          }
        }
        break;
      }
      case 'panel.toggle':
        this.state.display.panel_open = !this.state.display.panel_open;
        this.state.display.notice = undefined;
        this.emit();
        break;
      case 'theme.toggle': {
        const theme = this.state.display.theme === 'dark' ? 'light' : 'dark';
        this.state.display.theme = theme;
        document.documentElement.dataset.theme = theme;
        document.documentElement.classList.toggle('dark', theme === 'dark');
        document.documentElement.classList.toggle('light', theme === 'light');
        storageSet(THEME_KEY, theme);
        this.emit();
        break;
      }
      case 'section.toggle': {
        const section = intent.payload.section;
        if (typeof section === 'string' && Object.hasOwn(this.state.display.open_sections, section)) {
          this.state.display.open_sections[section] = !this.state.display.open_sections[section];
          this.emit();
        }
        break;
      }
      case 'timeline.reading': {
        const atEnd = intent.payload.at_end === true;
        const requested = intent.payload.state;
        const next: ReadingState = atEnd
          ? 'following_latest'
          : typeof requested === 'string' && VALID_READING.has(requested as ReadingState)
            ? (requested as ReadingState)
            : 'reading_history';
        if (this.state.display.reading !== next || (atEnd && this.state.display.new_count !== 0)) {
          this.state.display.reading = next;
          if (atEnd) this.state.display.new_count = 0;
          this.emit();
        }
        break;
      }
      case 'timeline.latest':
        this.state.display.reading = 'following_latest';
        this.state.display.new_count = 0;
        this.state.display.scroll_request += 1;
        this.emit();
        break;
      case 'part.copy':
        void this.copyText(String(action.payload?.content ?? ''));
        break;
      case 'composer.draft':
        if (typeof intent.payload.value === 'string') {
          this.state.draft = intent.payload.value;
          storageSet(DRAFT_KEY, this.state.draft);
          this.emit();
        }
        break;
      case 'composer.append':
        this.sendAppend();
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
    this.state.display.notice = copied ? '已复制当前可见 revision' : '复制不可用；请手动选择文本。';
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

  private sendAppend(): void {
    const content = this.state.draft.trim();
    const subjectId = this.state.subject_id;
    if (!content || !subjectId || this.state.connection !== 'online') return;
    const latestStep = [...this.state.timeline].reverse().find((part) => part.step_id);
    const payload: SubmissionPayload = {
      schema_version: 2,
      type: 'submit',
      submission_id: uid(),
      message_id: uid(),
      subject_id: subjectId,
      mode: 'append',
      content,
      attachment_refs: [],
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
      this.state.display.notice = '浏览器无法保存提交 ID；未发送，草稿已保留。';
      this.emit();
      return;
    }
    this.state.submissions = [...this.state.submissions, submission];
    this.state.display.reading = 'following_latest';
    this.state.display.new_count = 0;
    this.state.display.scroll_request += 1;
    this.sendSubmission(submission, false);
  }

  private sendSubmission(submission: Submission, retry: boolean): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    submission.send_epoch += 1;
    submission.receipt = 'sending';
    submission.detail = undefined;
    if (!saveSubmission(submission)) {
      submission.receipt = 'unknown';
      submission.detail = '浏览器无法保存原提交；未发送，原 ID 已保留。';
      this.emit();
      return;
    }
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
      submission.detail = retry ? '重试未能写入传输。' : '未能写入传输；草稿与同一 submission ID 已保留。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
      this.emit();
    }
    saveSubmission(submission);
    this.scheduleReceiptRefresh();
  }

  private clearReconnectTimers(): void {
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    if (this.countdownTimer) clearInterval(this.countdownTimer);
    this.reconnectTimer = null;
    this.countdownTimer = null;
  }

  private setConnection(connection: ConnectionState, reconnectSeconds = 0): void {
    this.state.connection = connection;
    this.state.reconnect_seconds = reconnectSeconds;
    this.emit();
  }

  private scheduleReconnect(): void {
    if (this.disposed) return;
    this.clearReconnectTimers();
    const base = RECONNECT_BASE[Math.min(this.reconnectAttempt, RECONNECT_BASE.length - 1)];
    this.reconnectAttempt += 1;
    const delay = Math.max(1, Math.round(base * (0.85 + Math.random() * 0.3)));
    this.state.reconnect_seconds = delay;
    this.setConnection('reconnecting', delay);
    this.countdownTimer = setInterval(() => {
      const next = Math.max(0, this.state.reconnect_seconds - 1);
      this.state.reconnect_seconds = next;
      this.emit();
    }, 1000);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay * 1000);
  }

  private connect(): void {
    if (this.disposed || this.socket) return;
    const url = wsUrl();
    if (!url) {
      this.setConnection('offline');
      return;
    }
    let socket: WebSocket;
    try {
      socket = new WebSocket(url);
    } catch {
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;
    this.setConnection('connecting');
    socket.onopen = () => {
      if (this.socket !== socket || this.disposed) return;
      this.reconnectAttempt = 0;
      this.clearReconnectTimers();
      this.setConnection('online');
      this.refreshSubmissions();
      void this.loadSelf();
    };
    socket.onmessage = (event) => this.onMessage(event);
    socket.onerror = () => socket.close();
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      if (this.disposed) return;
      if (this.receiptTimer !== null) window.clearTimeout(this.receiptTimer);
      this.receiptTimer = null;
      for (const submission of this.state.submissions) {
        if (submission.receipt === 'sending' || submission.receipt === 'sent') {
          submission.receipt = 'unknown';
          submission.detail = '连接中断，服务端回执未知；未生成新的 submission ID。';
          saveSubmission(submission);
        }
      }
      this.scheduleReconnect();
    };
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
        this.state.append_available = capabilities.append === true;
        this.state.durable_receipts = capabilities.durable_input_receipts === true;
        this.state.committed_replay = capabilities.committed_replay === true;
        this.refreshSubmissions();
        this.scheduleReceiptRefresh();
        this.emit();
        return;
      }
      if (data.type === 'identity') {
        if (validRevision(data.revision)) {
          const shown = this.state.identity?.revision ?? null;
          this.state.wanted_identity_revision = Math.max(
            this.state.wanted_identity_revision ?? 0,
            data.revision,
          );
          if (shown === null || shown < this.state.wanted_identity_revision) void this.loadSelf();
          else this.emit();
        }
        return;
      }
      if (data.type === 'receipt') {
        this.handleReceipt(data);
        return;
      }
      if (data.type === 'submission_view') {
        this.handleSubmissionView(data);
        return;
      }
      if (data.type === 'timeline_part') {
        this.handleTimelinePart(data);
      }
    } catch {
      // One malformed DTO or projection is contained; transport stays alive.
      this.state.display.notice = '一条消息未能呈现；连接与其余历史仍可用。';
      this.emit();
    }
  }

  private handleReceipt(data: Record<string, unknown>): void {
    if (data.schema_version !== 2 || typeof data.submission_id !== 'string') return;
    const submission = this.state.submissions.find(
      (row) => row.payload.submission_id === data.submission_id,
    );
    if (!submission) return;
    const detail = typeof data.detail === 'string' ? data.detail : undefined;
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
      if (submission.receipt === 'accepted') submission.detail = detail;
    } else if (data.stage === 'unavailable') {
      if (!submission.accepted_commit) submission.receipt = 'unknown';
      submission.detail = detail ?? '持久结果暂不可用；保留原 ID 等待对账。';
    } else if (data.stage === 'conflict') {
      submission.receipt = 'conflict';
      submission.detail = detail ?? '同一 submission_id 已关联不同内容；原输入未改写。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    } else if (data.stage === 'rejected') {
      submission.receipt = 'rejected';
      submission.detail = detail ?? '服务器拒绝接收。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    }
    // Non-committed outcomes stay persisted: they are the only copy of this
    // input's identity and payload until the user retries or the Store holds
    // a committed disposition for the original event.
    saveSubmission(submission);
    this.scheduleReceiptRefresh();
    this.emit();
  }

  // Poll only while unresolved receipts exist and the real socket advertises
  // durable queries. Timeline traffic is not evidence of this input's S0/S1.
  private scheduleReceiptRefresh(): void {
    if (this.receiptTimer !== null || this.disposed || this.state.connection !== 'online'
      || !this.state.durable_receipts || !this.state.subject_id
      || !this.state.submissions.some((submission) =>
        submission.payload.subject_id === this.state.subject_id
        && ['sending', 'sent', 'unknown', 'accepted', 'selected'].includes(submission.receipt))) return;
    this.receiptTimer = window.setTimeout(() => {
      this.receiptTimer = null;
      this.refreshSubmissions();
      this.scheduleReceiptRefresh();
    }, RECEIPT_REFRESH_MS);
  }

  /// Query the original durable key after reconnect and during live progress.
  private refreshSubmissions(): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN || !this.state.durable_receipts) return;
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
  }

  private handleSubmissionView(data: Record<string, unknown>): void {
    if (data.schema_version !== 2 || typeof data.submission_id !== 'string') return;
    const submission = this.state.submissions.find(
      (row) => row.payload.submission_id === data.submission_id,
    );
    if (!submission) return;
    // Correlate the view with the send attempt that produced it. A stale
    // not_admitted racing a newer retry must not restore the draft or demote
    // a fact already returned by that retry.
    if (validRevision(data.send_epoch) && data.send_epoch < submission.send_epoch) {
      return;
    }
    if (data.result === 'found' && typeof data.event_id === 'string'
      && typeof data.accepted_commit === 'string') {
      submission.event_id = typeof data.event_id === 'string' ? data.event_id : submission.event_id;
      submission.accepted_commit =
        typeof data.accepted_commit === 'string' ? data.accepted_commit : submission.accepted_commit;
      if (data.stage === 'committed' && typeof data.committed_at === 'string') {
        submission.receipt = 'committed';
        submission.committed_at =
          typeof data.committed_at === 'string' ? data.committed_at : submission.committed_at;
        submission.detail = undefined;
        retireSubmission(submission);
        this.scheduleReceiptRefresh();
        this.emit();
        return;
      } else if (data.stage === 'selected' && typeof data.selected_step_id === 'string') {
        if (submission.receipt !== 'committed') submission.receipt = 'selected';
        submission.selected_step_id =
          typeof data.selected_step_id === 'string' ? data.selected_step_id : submission.selected_step_id;
        if (submission.receipt === 'selected') submission.detail = '已观察封口输入计划；等待处置回执。';
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
      submission.detail = '服务端无此提交记录；可以同一 ID 重发。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    } else if (data.result === 'temporarily_unavailable') {
      if (!submission.accepted_commit) submission.receipt = 'unknown';
      submission.detail = '持久查询暂不可用；原 ID 与此前回执仍保留。';
    }
    // Query failure is not not_admitted and never restores an accepted draft.
    saveSubmission(submission);
    this.scheduleReceiptRefresh();
    this.emit();
  }

  private partKey(data: Record<string, unknown>): string {
    return [
      String(data.stream_id ?? ''),
      String(data.request_id ?? ''),
      String(data.attempt_id ?? ''),
      String(data.message_id),
      String(data.part_id),
    ].join('/');
  }

  private handleTimelinePart(data: Record<string, unknown>): void {
    if (
      data.schema_version !== 2 ||
      data.committed_only !== true ||
      data.role !== 'subject' ||
      typeof data.message_id !== 'string' ||
      typeof data.part_id !== 'string' ||
      typeof data.part_revision !== 'string' ||
      typeof data.commit_ref !== 'string' ||
      typeof data.content !== 'string' ||
      typeof data.step_id !== 'string' ||
      !validRevision(data.step) ||
      data.step < 1 ||
      !validRevision(data.chunk_seq)
    ) {
      return;
    }
    if (this.state.step === null || data.step > this.state.step) this.state.step = data.step;
    const key = this.partKey(data);
    const existing = this.state.timeline.find(
      (part) =>
        part.message_id === data.message_id &&
        part.part_id === data.part_id &&
        (part.stream_id ?? '') === String(data.stream_id ?? '') &&
        (part.request_id ?? '') === String(data.request_id ?? '') &&
        (part.attempt_id ?? '') === String(data.attempt_id ?? ''),
    );
    const chunkSeq = data.chunk_seq;
    if (existing && chunkSeq < existing.next_chunk_seq) {
      // Committed replay and reconnect duplicates are idempotent. No browser-
      // visible step counter is used as a substitute for message/part identity.
      this.emit();
      return;
    }
    if (chunkSeq > (existing?.next_chunk_seq ?? 0)) {
      const pending = this.pendingChunks.get(key) ?? new Map<number, Record<string, unknown>>();
      pending.set(chunkSeq, data);
      this.pendingChunks.set(key, pending);
      if (existing) existing.gap = true;
      this.state.display.notice = '已提交 part 存在序号空洞；等待补读，未拼接后到内容。';
      this.emit();
      return;
    }

    const appended = data.content;
    if (existing) {
      existing.content += appended;
      existing.next_chunk_seq += 1;
      existing.part_revision = data.part_revision;
      existing.observed_at = typeof data.observed_at === 'string' ? data.observed_at : existing.observed_at;
      this.applyPending(existing, key);
      this.state.timeline = this.state.timeline.map((part) => (part === existing ? { ...existing } : part));
    } else {
      const part: TimelinePart = {
        role: 'subject',
        content: appended,
        message_id: data.message_id,
        part_id: data.part_id,
        part_revision: data.part_revision,
        next_chunk_seq: 1,
        step: data.step,
        step_id: data.step_id,
        commit_ref: data.commit_ref,
        observed_at: typeof data.observed_at === 'string' ? data.observed_at : '',
        ...(typeof data.stream_id === 'string' ? { stream_id: data.stream_id } : {}),
        ...(typeof data.request_id === 'string' ? { request_id: data.request_id } : {}),
        ...(typeof data.attempt_id === 'string' ? { attempt_id: data.attempt_id } : {}),
        gap: false,
      };
      this.applyPending(part, key);
      this.state.timeline = [...this.state.timeline, part];
    }
    if (this.state.display.reading === 'following_latest') {
      this.state.display.scroll_request += 1;
    } else if (appended) {
      this.state.display.new_count += 1;
    }
    this.emit();
  }

  private applyPending(part: TimelinePart, key: string): void {
    const pending = this.pendingChunks.get(key);
    if (!pending) return;
    while (pending.has(part.next_chunk_seq)) {
      const next = pending.get(part.next_chunk_seq)!;
      part.content += String(next.content ?? '');
      part.next_chunk_seq += 1;
      part.part_revision = String(next.part_revision ?? part.part_revision);
      pending.delete(part.next_chunk_seq - 1);
    }
    part.gap = pending.size > 0;
    if (pending.size === 0) this.pendingChunks.delete(key);
  }

  private async fetchJson(path: string): Promise<unknown> {
    const response = await fetch(path, {
      headers: { Accept: 'application/json' },
      cache: 'no-store',
      signal: this.abort.signal,
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  }

  private async loadConfig(): Promise<void> {
    try {
      const data = await this.fetchJson('/api/config');
      if (!isObject(data)) throw new Error('invalid config');
      this.state.config = data as UiState['config'];
      this.state.config_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.config = undefined;
      this.state.config_error = true;
    }
    if (!this.disposed) this.emit();
  }

  private applyIdentity(identity: IdentityView | undefined): void {
    if (!identity) return;
    const incoming = validRevision(identity.revision) ? identity.revision : null;
    const shown = this.state.identity?.revision ?? null;
    if (incoming !== null && shown !== null && incoming < shown) return;
    this.state.identity = identity;
  }

  private async loadSelf(): Promise<void> {
    const seq = ++this.selfSeq;
    try {
      const data = await this.fetchJson('/api/self');
      if (!isObject(data)) throw new Error('invalid self');
      if (seq < this.selfApplied || this.disposed) return;
      this.selfApplied = seq;
      const self = data as UiState['self'];
      this.state.self = self;
      this.state.self_error = false;
      this.applyIdentity(self?.identity);
      if (typeof self?.subject_id === 'string' && self.subject_id) this.state.subject_id = self.subject_id;
      if (validRevision(self?.step)) {
        this.state.step = this.state.step === null ? self.step : Math.max(this.state.step, self.step);
      }
      this.ensureIdentityLoaded();
      this.refreshSubmissions();
      this.scheduleReceiptRefresh();
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      if (seq < this.selfApplied) return;
      this.state.self_error = !this.state.self;
      this.ensureIdentityLoaded();
    }
    if (!this.disposed) this.emit();
  }

  private ensureIdentityLoaded(): void {
    const wanted = this.state.wanted_identity_revision;
    const shown = this.state.identity?.revision ?? null;
    const behind = wanted !== null && (shown === null || shown < wanted);
    if (!behind) {
      if (this.identityTimer) clearTimeout(this.identityTimer);
      this.identityTimer = null;
      this.identityRetryDelay = 1000;
      return;
    }
    if (this.identityTimer || this.disposed) return;
    this.identityTimer = setTimeout(() => {
      this.identityTimer = null;
      void this.loadSelf();
    }, this.identityRetryDelay);
    this.identityRetryDelay = Math.min(this.identityRetryDelay * 2, 30000);
  }

  private async loadTasks(): Promise<void> {
    try {
      const data = await this.fetchJson('/api/tasks');
      if (!isObject(data)) throw new Error('invalid tasks');
      this.state.tasks = data as UiState['tasks'];
      this.state.tasks_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.tasks = undefined;
      this.state.tasks_error = true;
    }
    if (!this.disposed) this.emit();
  }

  private async loadMemory(): Promise<void> {
    try {
      const data = await this.fetchJson('/api/memory');
      if (!isObject(data)) throw new Error('invalid memory');
      this.state.memory = data as UiState['memory'];
      this.state.memory_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.memory = undefined;
      this.state.memory_error = true;
    }
    if (!this.disposed) this.emit();
  }

  private async loadHealth(): Promise<void> {
    try {
      const data = await this.fetchJson('/health');
      if (isObject(data) && typeof data.version === 'string') this.state.server_version = data.version;
    } catch {
      // /api/self and the socket continue to report their own actual state.
    }
    if (!this.disposed) this.emit();
  }

  private async loadManifest(): Promise<void> {
    try {
      const data = await this.fetchJson('/assets/build-manifest.json');
      if (isObject(data) && isObject(data.upstream) && typeof data.upstream.commit === 'string') {
        this.state.manifest = data as unknown as BuildManifest;
      }
    } catch {
      this.state.manifest = undefined;
    }
    if (!this.disposed) this.emit();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.clearReconnectTimers();
    if (this.receiptTimer !== null) window.clearTimeout(this.receiptTimer);
    this.receiptTimer = null;
    if (this.identityTimer) clearTimeout(this.identityTimer);
    this.identityTimer = null;
    this.abort.abort();
    if (this.socket) {
      this.socket.onclose = null;
      this.socket.onmessage = null;
      this.socket.close();
      this.socket = null;
    }
    this.listeners.clear();
    this.pendingChunks.clear();
  }
}
