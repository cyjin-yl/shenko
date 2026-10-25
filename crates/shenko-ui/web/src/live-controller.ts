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
const THEME_KEY = 'shenko.theme.v1';
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
    subject_id: null,
    wanted_identity_revision: null,
    step: null,
    timeline: [],
    submissions: [],
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
        if (submission && submission.receipt === 'rejected') this.sendSubmission(submission, true);
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
    };
    this.state.submissions = [...this.state.submissions, submission];
    this.state.display.reading = 'following_latest';
    this.state.display.new_count = 0;
    this.state.display.scroll_request += 1;
    this.sendSubmission(submission, false);
  }

  private sendSubmission(submission: Submission, retry: boolean): void {
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) return;
    submission.receipt = 'sending';
    submission.detail = undefined;
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
      submission.receipt = 'rejected';
      submission.detail = retry ? '重试未能写入传输。' : '未能写入传输；草稿与同一 submission ID 已保留。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
      this.emit();
    }
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
      void this.loadSelf();
    };
    socket.onmessage = (event) => this.onMessage(event);
    socket.onerror = () => socket.close();
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      if (this.disposed) return;
      for (const submission of this.state.submissions) {
        if (submission.receipt === 'sending' || submission.receipt === 'sent') {
          submission.receipt = 'unknown';
          submission.detail = '连接中断，服务端回执未知；未生成新的 submission ID。';
        } else if (submission.receipt === 'queued_volatile') {
          submission.receipt = 'unknown';
          submission.last_transport_receipt = 'queued_volatile';
          submission.detail = '最后回执仅为内存队列；断连后的持久结果未知。';
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
    if (data.stage === 'queued') {
      submission.receipt = 'queued_volatile';
      submission.last_transport_receipt = 'queued_volatile';
      submission.detail = detail;
    } else if (data.stage === 'rejected') {
      submission.receipt = 'rejected';
      submission.detail = detail ?? '服务器拒绝接收。';
      if (!this.state.draft.trim()) {
        this.state.draft = submission.payload.content;
        storageSet(DRAFT_KEY, this.state.draft);
      }
    }
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
