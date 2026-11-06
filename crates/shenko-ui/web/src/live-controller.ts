import {
  UI_INTENT_SCHEMA,
  UI_PROJECTION_SCHEMA,
  buildProjection,
  semanticHash,
  type ActionReceiptView,
  type BuildManifest,
  type ConfigEntryView,
  type ConnectionState,
  type DisplayState,
  type IdentityView,
  type ProjectionController,
  type ReadingAnchor,
  type ReadingSelection,
  type ReadingState,
  type DegradedDocument,
  type DocumentErrorView,
  type ServerCapabilities,
  type Submission,
  type SubmissionPayload,
  type TimelinePart,
  type UiIntent,
  type UiProjection,
  type UiState,
  type ViewName,
} from './projection';
import {
  readDisplayState,
  readSelection,
  recordSelection,
  saveReadingAnchor,
  saveSelection,
  setCodeWrap,
  setExpanded,
  type DisplayStorage,
  type PersistedDisplayState,
} from './reading-state';

const DRAFT_KEY = 'shenko.draft.v1';
const SUBMISSION_KEY_PREFIX = 'shenko.submission.v2.';
const ACTION_KEY_PREFIX = 'shenko.action.v2.';
const THEME_KEY = 'shenko.theme.v1';
const RECEIPT_REFRESH_MS = 2000;
const RECONNECT_BASE = [1, 2, 4, 8, 15];
const DEFAULT_SECTIONS: Record<string, boolean> = {
  status: true,
  activity: true,
  artifacts: false,
  devices: false,
};
const MAX_ACTION_RECEIPTS = 16;
const VALID_VIEWS = new Set<ViewName>(['timeline', 'tasks', 'memory', 'config', 'playground']);
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

/// Display state is restored through the same tolerant reader as local
/// storage; a privacy mode that denies access simply starts from defaults.
function displayStorage(): DisplayStorage | null {
  try {
    return localStorage;
  } catch {
    return null;
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

function initialSections(persisted: PersistedDisplayState): Record<string, boolean> {
  const sections: Record<string, boolean> = {};
  for (const [name, fallback] of Object.entries(DEFAULT_SECTIONS)) {
    const stored = persisted.expanded[`section.${name}`];
    sections[name] = typeof stored === 'boolean' ? stored : fallback;
  }
  return sections;
}

/// One admitted action click, persisted before the transport can send it. It
/// shares the server's web-submission ledger, so a reopen can reconcile the
/// same `submission_id` instead of admitting the same decision twice.
interface PendingAction {
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

function saveAction(action: PendingAction): boolean {
  try {
    localStorage.setItem(actionKey(action.submission_id), JSON.stringify(action));
    return true;
  } catch {
    return false;
  }
}

function retireAction(action: PendingAction): void {
  try {
    localStorage.removeItem(actionKey(action.submission_id));
  } catch {
    // Storage is optional; the in-memory receipt retires regardless.
  }
}

function storedActions(): Map<string, PendingAction> {
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

function initialActionReceipts(actions: Map<string, PendingAction>): ActionReceiptView[] {
  return [...actions.values()]
    .sort((left, right) => Date.parse(left.created_at) - Date.parse(right.created_at))
    .map((action) => ({
      submission_id: action.submission_id,
      document_id: action.document_id,
      action_id: action.action_id,
      revision: action.revision,
      stage: 'sending' as const,
      detail: '已恢复原动作 ID；等待持久对账，不会重复接纳同一决定。',
    }));
}

function initialState(): UiState {
  const storage = displayStorage();
  const persisted = readDisplayState(storage);
  const actions = storedActions();
  const selection = readSelection(storage);
  const display: DisplayState = {
    view: 'timeline',
    panel_open: false,
    open_sections: initialSections(persisted),
    theme: initialTheme(),
    reading: persisted.reading,
    new_count: 0,
    scroll_request: 0,
    wrap: persisted.wrap,
    ...(persisted.anchor ? { anchor: persisted.anchor } : {}),
    ...(selection ? { selection } : {}),
  };
  return {
    source: 'live',
    connection: 'connecting',
    reconnect_seconds: 0,
    capabilities: {
      append: false,
      steer: false,
      withdraw: false,
      actions: false,
      documents: false,
      attachments: false,
      durable_input_receipts: false,
      committed_replay: false,
      streaming: false,
    },
    append_available: false,
    durable_receipts: false,
    committed_replay: false,
    streaming: false,
    documents: [],
    document_errors: [],
    action_receipts: initialActionReceipts(actions),
    subject_id: null,
    wanted_identity_revision: null,
    step: null,
    timeline: [],
    submissions: storedSubmissions(),
    draft: storageGet(DRAFT_KEY),
    config_error: false,
    config_entries: undefined,
    config_entries_error: false,
    config_drafts: {},
    self_error: false,
    tasks_error: false,
    memory_error: false,
    playground_error: false,
    display,
  };
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function validRevision(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function finite(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

/// A place is only accepted when it names a rendered entry; an anonymous
/// scroll offset alone is never promoted into a reading anchor.
function validAnchorPayload(value: unknown): ReadingAnchor | undefined {
  if (!isObject(value)) return undefined;
  if (typeof value.entry_key !== 'string' || !value.entry_key) return undefined;
  if (!finite(value.offset) || !finite(value.scroll_top)) return undefined;
  return {
    entry_key: value.entry_key,
    entry_index: finite(value.entry_index) ? Math.max(0, Math.trunc(value.entry_index)) : 0,
    ...(typeof value.prev_entry_key === 'string' && value.prev_entry_key
      ? { prev_entry_key: value.prev_entry_key }
      : {}),
    offset: value.offset,
    scroll_top: Math.max(0, value.scroll_top),
  };
}

function sameAnchor(left: ReadingAnchor, right: ReadingAnchor | undefined): boolean {
  return right !== undefined
    && left.entry_key === right.entry_key
    && left.prev_entry_key === right.prev_entry_key
    && Math.abs(left.offset - right.offset) < 1
    && Math.abs(left.scroll_top - right.scroll_top) < 1;
}

function sameSelection(left: ReadingSelection, right: ReadingSelection | undefined): boolean {
  return right !== undefined
    && left.entry_key === right.entry_key
    && left.start === right.start
    && left.end === right.end
    && left.prefix_hash === right.prefix_hash;
}

interface TimelineChunk extends Record<string, unknown> {
  message_id: string;
  part_id: string;
  part_revision: string;
  commit_ref: string;
  content: string;
  step: number;
  chunk_seq: number;
}

function isTimelineChunk(data: Record<string, unknown>): data is TimelineChunk {
  return data.schema_version === 2 &&
    data.committed_only === true &&
    data.role === 'subject' &&
    typeof data.message_id === 'string' &&
    typeof data.part_id === 'string' &&
    typeof data.part_revision === 'string' &&
    typeof data.commit_ref === 'string' &&
    typeof data.content === 'string' &&
    validRevision(data.step) && validRevision(data.chunk_seq);
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
  private pendingChunks = new Map<string, Map<number, TimelineChunk>>();
  /// One durable origin identity per (key, exact desired bytes): pressing save
  /// again for the same value resolves the original change instead of
  /// submitting a second one.
  private configOrigins = new Map<string, string>();
  /// Actions whose admission is still open, keyed by the durable submission
  /// ID the Store knows. Restored before the socket connects so a reopen
  /// reconciles the original decision instead of admitting a second one.
  private pendingActions = storedActions();

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
          if (view === 'playground') void this.loadPlayground();
          if (view === 'config') {
            void this.loadConfig();
            void this.loadConfigEntries();
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
          setExpanded(
            displayStorage(),
            `section.${section}`,
            this.state.display.open_sections[section],
          );
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
        const anchor = validAnchorPayload(intent.payload.anchor);
        const previousAnchor = this.state.display.anchor;
        let changed = this.state.display.reading !== next
          || (atEnd && this.state.display.new_count !== 0);
        this.state.display.reading = next;
        if (atEnd) {
          this.state.display.new_count = 0;
          if (previousAnchor) {
            this.state.display.anchor = undefined;
            saveReadingAnchor(displayStorage(), null);
            changed = true;
          }
        } else if (anchor && !sameAnchor(anchor, previousAnchor)) {
          // A place is only restorable when the reader is actually in history;
          // a transient selection is not a place (§21.11).
          this.state.display.anchor = anchor;
          saveReadingAnchor(displayStorage(), anchor);
          changed = true;
        }
        if (changed) {
          this.emit();
        }
        break;
      }
      case 'timeline.latest':
        this.state.display.reading = 'following_latest';
        this.state.display.new_count = 0;
        this.state.display.anchor = undefined;
        saveReadingAnchor(displayStorage(), null);
        this.state.display.scroll_request += 1;
        this.emit();
        break;
      case 'timeline.code_wrap': {
        const wrap = !this.state.display.wrap;
        this.state.display.wrap = wrap;
        setCodeWrap(displayStorage(), wrap);
        this.state.display.notice = wrap ? '长代码行已软换行' : '长代码行保持原样，可横向滚动';
        this.emit();
        break;
      }
      case 'timeline.selection': {
        const storage = displayStorage();
        const entryKey = intent.payload.entry_key;
        const text = intent.payload.text;
        const start = intent.payload.start;
        const end = intent.payload.end;
        if (intent.payload.clear === true || typeof entryKey !== 'string' || typeof text !== 'string'
          || typeof start !== 'number' || typeof end !== 'number'
          || !Number.isFinite(start) || !Number.isFinite(end)) {
          if (intent.payload.clear !== true) break;
          if (!this.state.display.selection) break;
          this.state.display.selection = undefined;
          saveSelection(storage, null);
          this.emit();
          break;
        }
        const recorded = recordSelection(entryKey, text, start, end);
        if (!recorded) {
          if (!this.state.display.selection) break;
          this.state.display.selection = undefined;
          saveSelection(storage, null);
          this.emit();
          break;
        }
        if (sameSelection(recorded, this.state.display.selection)) break;
        this.state.display.selection = recorded;
        saveSelection(storage, recorded);
        this.emit();
        break;
      }
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
      case 'composer.append':
        this.sendWithMode('append');
        break;
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

  // Steer is admitted exactly like append; without a verified in-request
  // control capability the server's placement is next_boundary (UI02). The
  // client does not promise live injection.
  private sendSteer(): void {
    this.sendWithMode('steer');
  }

  private sendWithMode(mode: 'append' | 'steer'): void {
    const content = this.state.draft.trim();
    const subjectId = this.state.subject_id;
    if (!content || !subjectId || this.state.connection !== 'online') return;
    if (mode === 'steer' && !this.state.capabilities.steer) {
      this.state.display.notice = '当前服务器未提供 steer 能力；输入未发送。';
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

  // Withdrawal is a durable control record, not a delete: the admitted input
  // stays in the ledger and the server reports withdrawal_queued vs
  // withdrawn_before_presentation through the next submission_view.
  private sendWithdraw(submission: Submission): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    if (!this.state.capabilities.withdraw) {
      this.state.display.notice = '当前服务器未提供撤回能力。';
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
  // the subject's disposition of that input, never from the click (§21.8).
  private sendAction(documentId: string, option: {
    action_id: string;
    nonce: string;
    revision: string;
    payload: Record<string, unknown>;
  }): void {
    const socket = this.socket;
    const subjectId = this.state.subject_id;
    if (!subjectId) {
      this.state.display.notice = '尚无真实 subject 绑定；动作未发送。';
      this.emit();
      return;
    }
    if (!this.state.capabilities.actions) {
      this.state.display.notice = '当前服务器未提供动作能力。';
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
        ? '此动作正在提交；等待持久回执。'
        : '此动作已持久接纳；处置结果由主体决定，不重复接纳。';
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
      this.state.display.notice = '浏览器无法保存动作 ID；未发送。';
      this.emit();
      return;
    }
    this.pendingActions.set(action.submission_id, action);
    this.pushActionReceipt({
      submission_id: action.submission_id,
      document_id: documentId,
      action_id: option.action_id,
      revision: option.revision,
      stage: 'sending',
    });
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      // Not admitted: the transport never took it. Keep the durable ID so a
      // reconnect can resend this exact decision rather than mint a new one.
      this.setActionReceipt(action.submission_id, {
        stage: 'unavailable',
        detail: '连接未就绪；动作未发送，原动作 ID 已保留。',
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
      this.setActionReceipt(action.submission_id, {
        stage: 'unavailable',
        detail: '未能写入传输；动作未接纳，原动作 ID 已保留。',
      });
    }
  }

  private pushActionReceipt(receipt: ActionReceiptView): void {
    const rest = this.state.action_receipts.filter(
      (row) => row.submission_id !== receipt.submission_id,
    );
    this.state.action_receipts = [...rest, receipt].slice(-MAX_ACTION_RECEIPTS);
    this.emit();
  }

  private setActionReceipt(
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
      // Attest this connection's display facts so the server degrades
      // documents to what THIS surface can actually render (§21.5).
      try {
        socket.send(
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
      // The action's admission result is unknown too, but its ID and payload
      // stay durable so the reconnect query settles the same decision.
      for (const row of this.state.action_receipts) {
        if (row.stage !== 'sending') continue;
        row.stage = 'unavailable';
        row.detail = '连接中断，动作回执未知；保留原动作 ID 等待对账。';
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
        this.handleDocument(data);
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
          this.state.display.notice = `文档未能呈现：${data.reason}`;
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
        return;
      }
      if (data.type === 'stream_part') {
        this.handleStreamPart(data);
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
          detail: detail ?? '动作已持久接纳；处置另查回执。',
          ...(eventId ? { event_id: eventId } : {}),
          ...(acceptedCommit ? { accepted_commit: acceptedCommit } : {}),
        });
      }
    } else if (data.stage === 'conflict') {
      this.setActionReceipt(submissionId, {
        stage: 'conflict',
        detail: detail ?? '同一动作键已关联不同内容；原动作未改写。',
      });
      this.retirePendingAction(submissionId);
    } else if (data.stage === 'rejected') {
      this.setActionReceipt(submissionId, {
        stage: 'rejected',
        detail: detail ?? '服务器拒绝此动作；未接纳。',
      });
      this.state.display.notice = detail ?? '动作未接纳。';
      // Nothing durable exists for this ID: the option may be pressed again
      // as a new decision, so the stored record is not kept.
      this.retirePendingAction(submissionId);
      this.emit();
    } else if (data.stage === 'unavailable') {
      this.setActionReceipt(submissionId, {
        stage: 'unavailable',
        detail: detail ?? '持久结果暂不可用；原动作 ID 已保留等待对账。',
      });
    }
    this.scheduleReceiptRefresh();
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

  private handleSubmissionView(data: Record<string, unknown>): void {
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
    if (data.result === 'found' && typeof data.event_id === 'string'
      && typeof data.accepted_commit === 'string') {
      if (isObject(data.withdrawal)) {
        const withdrawal = data.withdrawal;
        submission.withdrawal = {
          cancel_id: String(withdrawal.cancel_id ?? ''),
          commit_ref: String(withdrawal.commit_ref ?? ''),
          result: withdrawal.result === 'withdrawal_queued'
            ? 'withdrawal_queued'
            : 'withdrawn_before_presentation',
        };
      }
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
          detail: '主体已提交对这次动作的处置。',
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
        // The disposition is durable; the pending copy is no longer needed.
        this.retirePendingAction(submissionId);
      } else if (data.stage === 'selected') {
        this.setActionReceipt(submissionId, {
          stage: 'selected',
          detail: '已选入封口输入计划；等待处置回执。',
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
      } else {
        this.setActionReceipt(submissionId, {
          stage: 'accepted',
          detail: existing.detail ?? '动作已持久接纳；处置另查回执。',
          event_id: eventId,
          accepted_commit: acceptedCommit,
        });
      }
    } else if (data.result === 'not_admitted') {
      this.setActionReceipt(submissionId, {
        stage: 'rejected',
        detail: '服务端无此动作记录；未接纳。可重新点击当前选项。',
      });
      this.retirePendingAction(submissionId);
    } else if (data.result === 'temporarily_unavailable') {
      this.setActionReceipt(submissionId, {
        stage: 'unavailable',
        detail: '持久查询暂不可用；原动作 ID 保留等待对账。',
      });
    }
    this.scheduleReceiptRefresh();
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
      !isTimelineChunk(data) ||
      typeof data.step_id !== 'string' ||
      data.step < 1
    ) {
      return;
    }
    this.applyChunk(data, false);
  }

  // Shared part/chunk contract for committed and streamed chunks. Identity
  // is (message_id, part_id) plus, when present, (stream_id, request_id,
  // attempt_id). A committed chunk carries no stream facts: it either binds
  // S1 identity onto an already-streamed part (absorption, idempotent in
  // both delivery orders) or starts the part with the full committed text.
  private applyChunk(data: TimelineChunk, isStream: boolean): void {
    if (!isStream && (this.state.step === null || data.step > this.state.step)) {
      this.state.step = data.step;
    }
    const key = this.partKey(data);
    const streamFacts = {
      stream: String(data.stream_id ?? ''),
      request: String(data.request_id ?? ''),
      attempt: String(data.attempt_id ?? ''),
    };
    let existing = this.state.timeline.find(
      (part) =>
        part.message_id === data.message_id &&
        part.part_id === data.part_id &&
        (part.stream_id ?? '') === streamFacts.stream &&
        (part.request_id ?? '') === streamFacts.request &&
        (part.attempt_id ?? '') === streamFacts.attempt,
    );
    if (!existing && !isStream && typeof data.step_id === 'string') {
      // Committed binding for a part this client watched stream in: absorb
      // the S1 identity instead of re-appending the full text at seq 0.
      const streamed = this.state.timeline.find(
        (part) =>
          part.message_id === data.message_id &&
          part.part_id === data.part_id &&
          part.stream_id !== undefined,
      );
      if (streamed) {
        if (data.chunk_seq < streamed.next_chunk_seq) {
          // The committed message carries the authoritative full text, so a
          // streamed prefix is replaced by it: keeping only the prefix would
          // render a committed step with its tail missing.
          streamed.content = data.content;
          streamed.next_chunk_seq = Math.max(streamed.next_chunk_seq, 1);
          streamed.commit_ref = data.commit_ref;
          streamed.part_revision = data.part_revision;
          streamed.step = data.step;
          streamed.step_id = data.step_id;
          streamed.terminal = true;
          streamed.committed = true;
          streamed.gap = false;
          this.pendingChunks.delete(key);
          this.emit();
          return;
        }
        // Committed text at or beyond the streamed cursor: the full text still
        // supersedes it, never appends to it.
        streamed.content = data.content;
        streamed.next_chunk_seq = Math.max(streamed.next_chunk_seq, data.chunk_seq + 1);
        streamed.commit_ref = data.commit_ref;
        streamed.part_revision = data.part_revision;
        streamed.step = data.step;
        streamed.step_id = data.step_id;
        streamed.terminal = true;
        streamed.committed = true;
        streamed.gap = false;
        this.pendingChunks.delete(key);
        this.emit();
        return;
      }
    }
    if (!existing && isStream) {
      // Stream chunk arriving after the committed base: the text is already
      // complete; adopt the stream facts so later chunks dedup exactly.
      const base = this.state.timeline.find(
        (part) =>
          part.message_id === data.message_id &&
          part.part_id === data.part_id &&
          part.stream_id === undefined,
      );
      if (base) {
        base.stream_id = String(data.stream_id ?? base.stream_id);
        base.request_id = String(data.request_id ?? base.request_id);
        base.attempt_id = String(data.attempt_id ?? base.attempt_id);
        this.emit();
        return;
      }
    }
    if (isStream && existing?.committed) {
      // The committed text is complete; a late streamed chunk for the same
      // part would duplicate it. Identity is already adopted above.
      this.emit();
      return;
    }
    const chunkSeq = data.chunk_seq;
    if (existing && chunkSeq < existing.next_chunk_seq) {
      // Committed replay and reconnect duplicates are idempotent. No
      // browser-visible step counter substitutes for message/part identity.
      this.emit();
      return;
    }
    if (chunkSeq > (existing?.next_chunk_seq ?? 0)) {
      const pending = this.pendingChunks.get(key) ?? new Map<number, TimelineChunk>();
      pending.set(chunkSeq, data);
      this.pendingChunks.set(key, pending);
      if (existing) existing.gap = true;
      this.state.display.notice = isStream
        ? '流 part 存在序号空洞；等待补读，未拼接后到内容。'
        : '已提交 part 存在序号空洞；等待补读，未拼接后到内容。';
      this.emit();
      return;
    }

    const appended = data.content;
    if (existing) {
      existing.content += appended;
      existing.next_chunk_seq += 1;
      existing.part_revision = data.part_revision;
      existing.observed_at = typeof data.observed_at === 'string' ? data.observed_at : existing.observed_at;
      if (data.terminal === true || !isStream) existing.terminal = true;
      if (!isStream) {
        // This chunk came from the committed projection: the text now has an
        // S1 step, so the view may label it committed from here on.
        existing.committed = true;
        existing.step = data.step;
        if (typeof data.step_id === 'string') existing.step_id = data.step_id;
        existing.commit_ref = data.commit_ref;
      }
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
        committed: !isStream,
        step: data.step,
        step_id: typeof data.step_id === 'string' ? data.step_id : data.message_id,
        commit_ref: data.commit_ref,
        observed_at: typeof data.observed_at === 'string' ? data.observed_at : '',
        ...(typeof data.stream_id === 'string' ? { stream_id: data.stream_id } : {}),
        ...(typeof data.request_id === 'string' ? { request_id: data.request_id } : {}),
        ...(typeof data.attempt_id === 'string' ? { attempt_id: data.attempt_id } : {}),
        terminal: data.terminal === true || !isStream,
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
      part.content += next.content;
      part.next_chunk_seq += 1;
      part.part_revision = next.part_revision;
      pending.delete(part.next_chunk_seq - 1);
    }
    part.gap = pending.size > 0;
    if (pending.size === 0) this.pendingChunks.delete(key);
  }

  // A validated semantic document already degraded for this connection's
  // attested capabilities. Replay arrives in commit order, so the last
  // projection of a document_id is the newest committed one; the source
  // step it came from is projected so the surface renders beside that step.
  private handleDocument(data: Record<string, unknown>): void {
    if (data.schema_version !== 1 || !isObject(data.degraded)) return;
    const degraded = data.degraded as unknown as DegradedDocument;
    if (typeof degraded.document_id !== 'string' || typeof degraded.revision !== 'string') return;
    if (!Array.isArray(degraded.options)) return;
    const sourceStepId = typeof data.source_step_id === 'string' && data.source_step_id
      ? data.source_step_id
      : undefined;
    const sourceCommit = typeof data.source_commit === 'string' && data.source_commit
      ? data.source_commit
      : undefined;
    const next: DegradedDocument = {
      ...degraded,
      ...(sourceStepId ? { source_step_id: sourceStepId } : {}),
      ...(sourceCommit ? { source_commit: sourceCommit } : {}),
    };
    // Replacing in place keeps commit arrival order, so an unplaced surface
    // still reads in the order the subject produced it.
    const index = this.state.documents.findIndex(
      (row) => row.document_id === degraded.document_id,
    );
    this.state.documents = index >= 0
      ? this.state.documents.map((row, at) => (at === index ? next : row))
      : [...this.state.documents, next];
    if (this.state.display.reading === 'following_latest') {
      this.state.display.scroll_request += 1;
    }
    this.emit();
  }

  // Stream parts obey the same part/chunk contract as committed parts, with
  // two honest relaxations: step may be 0 (no committed step exists yet —
  // the client never invents one) and chunks may merge into a committed base
  // part when the server projected exactly the streamed prefix. A chunk that
  // does not fit is buffered as a gap, never patched over.
  private handleStreamPart(data: Record<string, unknown>): void {
    if (!this.state.streaming || !isTimelineChunk(data)) return;
    this.applyChunk(data, true);
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

  private async loadConfigEntries(): Promise<void> {
    try {
      const data = await this.fetchJson('/api/config/entries');
      if (!isObject(data) || data.available !== true) throw new Error('config entries unavailable');
      this.state.config_entries = Array.isArray(data.entries)
        ? (data.entries as ConfigEntryView[])
        : [];
      this.state.config_entries_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.config_entries = undefined;
      this.state.config_entries_error = true;
    }
    if (!this.disposed) this.emit();
  }

  private async postJson(path: string, body: unknown): Promise<unknown> {
    const response = await fetch(path, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json', Accept: 'application/json' },
      cache: 'no-store',
      body: JSON.stringify(body),
      signal: this.abort.signal,
    });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
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
      this.state.display.notice = `${key}: 值必须是合法 JSON`;
      this.emit();
      return;
    }
    const originKey = `${key}:${semanticHash(JSON.stringify(desired))}`;
    if (!this.configOrigins.has(originKey)) {
      this.configOrigins.set(originKey, `panel-${originKey}-${Date.now().toString(36)}`);
    }
    try {
      const data = await this.postJson('/api/config/apply', {
        namespace,
        logical_key: logicalKey,
        desired,
        origin_identity: this.configOrigins.get(originKey),
        expected_base_revision: entry?.entry?.desired_revision ?? 0,
      });
      if (!isObject(data) || data.accepted !== true) {
        const code = isObject(data) && typeof data.code === 'string' ? data.code : '修改被拒绝';
        this.state.display.notice = `${key}: ${code}`;
      } else {
        const change = isObject(data.change) ? data.change : undefined;
        const state = change && typeof change.state === 'string' ? change.state : 'unknown';
        const receipt = change && isObject(change.receipt) && typeof change.receipt.outcome === 'string'
          ? change.receipt.outcome
          : undefined;
        this.state.display.notice = `${key}: ${data.replayed === true ? '原修改（replayed）' : '已提交'} · ${state}${receipt ? ` · ${receipt}` : ''}`;
        // The saved revision now exists; the next edit is a new intent.
        this.configOrigins.delete(originKey);
        const drafts = { ...(this.state.config_drafts ?? {}) };
        delete drafts[key];
        this.state.config_drafts = drafts;
      }
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.display.notice = `${key}: 提交未确认（${
        error instanceof Error ? error.message : '网络错误'
      }）；保存仍是 durable 事实时才可重试同一值`;
    }
    if (!this.disposed) this.emit();
    await this.loadConfigEntries();
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

  private async loadPlayground(): Promise<void> {
    try {
      const [surface, namespaces] = await Promise.all([
        this.fetchJson('/api/playground'),
        this.fetchJson('/api/config/namespaces').catch(() => undefined),
      ]);
      if (!isObject(surface)) throw new Error('invalid playground surface');
      const namespaceList = isObject(namespaces) && 'namespaces' in namespaces && Array.isArray(namespaces.namespaces)
        ? namespaces.namespaces
        : undefined;
      this.state.playground = {
        ...(surface as Record<string, unknown>),
        namespaces: namespaceList,
      } as UiState['playground'];
      this.state.playground_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.playground = undefined;
      this.state.playground_error = true;
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
