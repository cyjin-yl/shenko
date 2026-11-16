import { message as m, text as t } from './messages';
import { compareSubmissionOrder, type BuildManifest, type ConfigEntryView, type IdentityView, type OwnerState, type Submission, type SubmissionCursor, type SubmissionPayload, type UiState } from './projection';
import { isObject, validRevision } from './live-validation';
import { UiApi } from './ui-api';

interface ReadModelEffects {
  applySubmissionView(submission: Submission, data: Record<string, unknown>, persist: boolean): void;
  refreshSubmissions(): void;
  scheduleReceiptRefresh(): void;
}

/** Loads service read models; sequence, cursor and revision guards survive late responses. */
export class LiveReadModels {
  private disposed = false;
  private selfSeq = 0;
  private selfApplied = 0;
  private identityTimer: number | null = null;
  private identityRetryDelay = 1000;
  private submissionHistoryLoading = false;
  private committedPartsSeen = 0;
  private historySnapshotAt = 0;
  private newestHistoryPending = false;

  constructor(private readonly state: UiState, private readonly api: UiApi, private readonly emit: () => void,
    private readonly effects: ReadModelEffects) {}

  async loadConfig(): Promise<void> {
    try {
      const data = await this.api.get('/api/config');
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

  async loadConfigEntries(): Promise<void> {
    try {
      const data = await this.api.get('/api/config/entries');
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

  async loadSubmissionHistory(before?: SubmissionCursor): Promise<void> {
    const subject = this.state.subject_id;
    if (!subject || this.disposed) return;
    if (this.submissionHistoryLoading) {
      if (before === undefined) this.newestHistoryPending = true;
      return;
    }
    this.submissionHistoryLoading = true;
    if (before === undefined) this.newestHistoryPending = false;
    const snapshotAt = this.committedPartsSeen;
    const previous = this.state.submission_history;
    this.state.submission_history = { has_older: false, ...previous, loading: true, error: undefined };
    this.emit();
    try {
      const query = new URLSearchParams({ limit: '100' });
      if (before !== undefined) {
        query.set('before_seq', String(before.seq));
        query.set('before_id', before.submission_id);
      }
      const page = await this.api.get(`/api/submissions?${query}`);
      if (this.disposed || subject !== this.state.subject_id) return;
      if (!isObject(page) || page.available !== true || page.subject_id !== subject
        || !Array.isArray(page.items) || !validRevision(page.total) || typeof page.has_older !== 'boolean') {
        throw new Error(isObject(page) && typeof page.note === 'string' ? page.note : m('history.unavailable'));
      }
      const items: Array<{ payload: SubmissionPayload; created_at: string; seq: number; view: Record<string, unknown> }> = [];
      const ids = new Set<string>();
      let upper = before;
      for (const row of page.items) {
        if (!isObject(row) || !isObject(row.payload) || !isObject(row.view)
          || row.payload.schema_version !== 2 || row.payload.type !== 'submit'
          || row.payload.subject_id !== subject || typeof row.payload.submission_id !== 'string'
          || typeof row.payload.message_id !== 'string' || typeof row.payload.content !== 'string'
          || !['append', 'steer'].includes(String(row.payload.mode))
          || typeof row.created_at !== 'string' || !Number.isFinite(Date.parse(row.created_at))
          || !validRevision(row.accepted_seq) || !row.payload.submission_id
          || upper !== undefined && compareSubmissionOrder(row.accepted_seq, row.payload.submission_id,
            upper.seq, upper.submission_id) >= 0
          || row.view.result !== 'found' || row.view.submission_id !== row.payload.submission_id
          || typeof row.view.event_id !== 'string' || typeof row.view.accepted_commit !== 'string'
          || !['accepted', 'selected', 'committed'].includes(String(row.view.stage))
          || row.view.stage === 'selected' && typeof row.view.selected_step_id !== 'string'
          || row.view.stage === 'committed' && typeof row.view.committed_at !== 'string'
          || ids.has(row.payload.submission_id)) throw new Error(t('history.invalidOrder'));
        upper = { seq: row.accepted_seq, submission_id: row.payload.submission_id };
        ids.add(row.payload.submission_id);
        items.push({ payload: row.payload as unknown as SubmissionPayload, created_at: row.created_at,
          seq: row.accepted_seq, view: row.view });
      }
      if (page.has_older && (!items.length || !isObject(page.next_before)
        || page.next_before.seq !== items[items.length - 1].seq
        || page.next_before.submission_id !== items[items.length - 1].payload.submission_id)
        || !page.has_older && page.next_before !== null && page.next_before !== undefined
        || previous?.total !== undefined && page.total < previous.total) {
        throw new Error(t('history.invalidCursor'));
      }
      const known = new Map(this.state.submissions.map((row) => [row.payload.submission_id, row]));
      const newest = previous?.newest_cursor;
      const overlaps = before === undefined && newest !== undefined && items.some((row) =>
        row.seq === newest.seq && row.payload.submission_id === newest.submission_id);
      for (const row of items) {
        let submission = known.get(row.payload.submission_id);
        if (submission && (submission.payload.subject_id !== subject
          || submission.payload.message_id !== row.payload.message_id
          || submission.payload.content !== row.payload.content || submission.payload.mode !== row.payload.mode)) {
          submission.receipt = 'conflict';
          submission.detail = m('history.conflict');
          continue;
        }
        if (!submission) {
          submission = { payload: row.payload, created_at: row.created_at, receipt: 'accepted', send_epoch: 0 };
          this.state.submissions.push(submission);
          known.set(row.payload.submission_id, submission);
        }
        if (submission.receipt === 'conflict') continue;
        submission.created_at = row.created_at;
        submission.accepted_seq = row.seq;
        this.effects.applySubmissionView(submission, row.view, false);
      }
      if (before === undefined) this.historySnapshotAt = snapshotAt;
      this.state.submission_history = {
        loading: false, total: page.total,
        stale: this.committedPartsSeen > this.historySnapshotAt,
        has_older: overlaps ? previous!.has_older : page.has_older,
        next_before: overlaps ? previous!.next_before : page.has_older ? {
          seq: items[items.length - 1].seq, submission_id: items[items.length - 1].payload.submission_id,
        } : undefined,
        newest_cursor: before === undefined && items.length
          ? { seq: items[0].seq, submission_id: items[0].payload.submission_id } : newest,
      };
      this.effects.scheduleReceiptRefresh();
    } catch (error) {
      if (this.disposed || subject !== this.state.subject_id) return;
      this.state.submission_history = { has_older: false, ...previous, loading: false,
        stale: this.committedPartsSeen > this.historySnapshotAt,
        error: error instanceof Error ? error.message : m('history.unavailable') };
    } finally {
      this.submissionHistoryLoading = false;
    }
    // Consume a newer demand once; a failed attempt does not retry itself.
    if (!this.disposed && this.newestHistoryPending) void this.loadSubmissionHistory();
    if (!this.disposed) this.emit();
  }

  noteFreshlyCommitted(): void {
    this.committedPartsSeen += 1;
    const history = this.state.submission_history;
    if (history && !history.stale) this.state.submission_history = { ...history, stale: true };
    if (this.submissionHistoryLoading) this.newestHistoryPending = true;
    this.scheduleTaskRefresh();
  }

  private applyIdentity(identity: IdentityView | undefined): void {
    if (!identity) return;
    const incoming = validRevision(identity.revision) ? identity.revision : null;
    const shown = this.state.identity?.revision ?? null;
    if (incoming !== null && shown !== null && incoming < shown) return;
    this.state.identity = identity;
  }

  async loadSelf(): Promise<void> {
    const seq = ++this.selfSeq;
    try {
      const data = await this.api.get('/api/self');
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
      this.effects.refreshSubmissions();
      void this.loadSubmissionHistory();
      this.effects.scheduleReceiptRefresh();
      // The ownership facts travel with the self view: the reader learns
      // whether this installation is claimed without a second round trip.
      void this.loadOwnerState();
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
    this.identityTimer = window.setTimeout(() => {
      this.identityTimer = null;
      void this.loadSelf();
    }, this.identityRetryDelay);
    this.identityRetryDelay = Math.min(this.identityRetryDelay * 2, 30000);
  }

  private taskRefresh: number | undefined;

  scheduleTaskRefresh(): void {
    if (this.taskRefresh !== undefined) return;
    this.taskRefresh = window.setTimeout(() => {
      this.taskRefresh = undefined;
      if (!this.disposed) {
        void this.loadTasks();
        void this.loadPlayground();
        void this.loadSubmissionHistory();
      }
    }, 600);
  }

  async loadTasks(): Promise<void> {
    try {
      const data = await this.api.get('/api/tasks');
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

  async loadMemory(): Promise<void> {
    try {
      const data = await this.api.get('/api/memory');
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

  /// Open one work: the reader sees what the subject made, at the revision
  /// the work is at now.
  async openWork(artifactId: unknown): Promise<void> {
    if (typeof artifactId !== 'string' || artifactId === '') return;
    try {
      const [meta, content] = await Promise.all([
        this.api.get(`/playground/${artifactId}/manifest`),
        this.api.workContent(artifactId),
      ]);
      if (!isObject(meta)) throw new Error('invalid work manifest');
      const record = isObject(meta.record) ? meta.record : {};
      const candidates = Array.isArray(record.candidate_manifest_refs)
        ? record.candidate_manifest_refs
          .map((reference) => (isObject(reference) && typeof reference.digest === 'string'
            ? { digest: reference.digest }
            : null))
          .filter((entry): entry is { digest: string } => entry !== null)
        : [];
      const versions = await this.api.get(`/playground/${artifactId}/versions`)
        .then((value) => (isObject(value) && Array.isArray(value.revisions) ? (value.revisions as number[]) : []))
        .catch(() => [] as number[]);
      this.state.work = {
        artifact_id: artifactId,
        title: typeof record.title === 'string' ? record.title : undefined,
        revision: content.revision,
        lifecycle: typeof record.lifecycle === 'string' ? record.lifecycle : undefined,
        content: content.text,
        candidates,
        revisions: versions,
      };
      this.state.work_error = false;
    } catch (error) {
      if (this.disposed || (error instanceof DOMException && error.name === 'AbortError')) return;
      this.state.work = undefined;
      this.state.work_error = true;
    }
    if (!this.disposed) {
      this.state.display.panel_open = true;
      this.emit();
    }
  }

  /// The ownership facts, as the installation reports them.
  async loadOwnerState(): Promise<void> {
    try {
      const data = await this.api.get('/api/owner/state');
      if (!isObject(data)) throw new Error('invalid owner state');
      this.state.owner_state = data as OwnerState;
    } catch {
      // An installation without ownership reports that, rather than a
      // fabricated "not claimed".
      this.state.owner_state = undefined;
    }
  }

  async loadPlayground(): Promise<void> {
    try {
      const [surface, namespaces] = await Promise.all([
        this.api.get('/api/playground'),
        this.api.get('/api/config/namespaces').catch(() => undefined),
      ]);
      if (!isObject(surface) || surface.available !== true) throw new Error('invalid playground surface');
      const namespaceList = isObject(namespaces) && 'namespaces' in namespaces && Array.isArray(namespaces.namespaces)
        ? namespaces.namespaces
        : undefined;
      const roots: NonNullable<NonNullable<UiState['playground']>['roots']> = [];
      if (Array.isArray(surface.roots)) {
        for (const entry of surface.roots) {
          if (!Array.isArray(entry) || entry.length !== 3
            || typeof entry[0] !== 'string' || typeof entry[1] !== 'string' || typeof entry[2] !== 'string') continue;
          roots.push({ artifact_id: entry[0], root: entry[1], identity: entry[2] });
        }
      }
      const artifacts: NonNullable<NonNullable<UiState['playground']>['artifacts']> = [];
      if (Array.isArray(surface.artifacts)) {
        for (const entry of surface.artifacts) {
          if (!isObject(entry) || typeof entry.artifact_id !== 'string') continue;
          const record = isObject(entry.record) ? entry.record : undefined;
          const active = record && isObject(record.active_manifest_ref) ? record.active_manifest_ref : undefined;
          artifacts.push({
            ...entry,
            artifact_id: entry.artifact_id,
            active_manifest_digest: typeof active?.digest === 'string' ? active.digest : undefined,
          });
        }
      }
      this.state.playground = {
        ...(surface as Record<string, unknown>),
        roots,
        artifacts,
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

  async loadHealth(): Promise<void> {
    try {
      const data = await this.api.get('/health');
      if (isObject(data) && typeof data.version === 'string') this.state.server_version = data.version;
    } catch {
      // /api/self and the socket continue to report their own actual state.
    }
    if (!this.disposed) this.emit();
  }

  async loadManifest(): Promise<void> {
    try {
      const data = await this.api.get('/assets/build-manifest.json');
      if (isObject(data) && isObject(data.upstream) && typeof data.upstream.commit === 'string') {
        this.state.manifest = data as unknown as BuildManifest;
      }
    } catch {
      this.state.manifest = undefined;
    }
    if (!this.disposed) this.emit();
  }


  dispose(): void {
    this.disposed = true;
    const clearTimer = typeof window === 'undefined' ? clearTimeout : window.clearTimeout.bind(window);
    if (this.identityTimer !== null) clearTimer(this.identityTimer);
    if (this.taskRefresh !== undefined) clearTimer(this.taskRefresh);
    this.identityTimer = null;
    this.taskRefresh = undefined;
  }
}
