import { message as m } from './messages';
import type { DegradedDocument, TimelinePart, UiState } from './projection';
import { isObject, validRevision, isTimelineChunk, type TimelineChunk } from './live-validation';

interface TimelineEffects {
  noteFreshlyCommitted(): void;
  scheduleTaskRefresh(): void;
}

/** Merges durable stream/replay facts; never parses model control syntax. */
export class TimelineReducer {
  private readonly pendingChunks = new Map<string, Map<number, TimelineChunk>>();
  constructor(private readonly state: UiState, private readonly emit: () => void,
    private readonly effects: TimelineEffects) {}

  private partKey(data: Record<string, unknown>): string {
    return [
      String(data.stream_id ?? ''),
      String(data.request_id ?? ''),
      String(data.attempt_id ?? ''),
      String(data.message_id),
      String(data.part_id),
    ].join('/');
  }

  private isCorrectedText(part: {
    message_id: string; part_id: string; commit_ref: string; step_id?: unknown;
  }): boolean {
    // This is the server's existing corrected-part identity, not a general
    // convention for arbitrary part IDs containing '@'.
    return part.step_id === part.message_id &&
      part.part_id === `text/0@${part.commit_ref}`;
  }

  private isOriginalText(part: {
    message_id: string; part_id: string; step_id?: unknown;
  }, messageId: string, stepId: string): boolean {
    return part.message_id === messageId && part.part_id === 'text/0' &&
      (typeof part.step_id === 'string' ? part.step_id : part.message_id) === stepId;
  }

  private discardOriginalTextChunks(messageId: string, stepId: string, commitRef: string): void {
    // A gap can have been buffered before any part exists. Match its facts,
    // not a string prefix of the compound key, and leave other parts alone.
    for (const [key, pending] of this.pendingChunks) {
      for (const [sequence, chunk] of pending) {
        if (this.isOriginalText(chunk, messageId, stepId) &&
          (chunk.type === 'stream_part' || chunk.commit_ref === commitRef)) pending.delete(sequence);
      }
      if (pending.size === 0) this.pendingChunks.delete(key);
    }
  }

  handleTimelinePart(data: Record<string, unknown>): void {
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
      // A new committed step may have started or settled delegations; the
      // task list behind the IM threads and Inspect branches follows it.
      this.effects.scheduleTaskRefresh();
    }
    const stepId = typeof data.step_id === 'string' ? data.step_id : data.message_id;
    const correction = data.part_id === 'text/0' ? this.state.timeline.find((part) =>
      part.committed && this.isCorrectedText(part) &&
      part.message_id === data.message_id && part.step_id === stepId &&
      (isStream || part.commit_ref === data.commit_ref)) : undefined;
    if (correction) {
      // A corrected committed body stays authoritative through late stream
      // chunks and reconnect replay. Do not attach the divergent stream's
      // identity to the corrected part or append its old text again.
      this.discardOriginalTextChunks(data.message_id, stepId, correction.commit_ref);
      this.emit();
      return;
    }
    let correctedIndex = -1;
    if (!isStream && data.chunk_seq === 0 && this.isCorrectedText(data)) {
      const superseded = (part: TimelinePart) =>
        this.isOriginalText(part, data.message_id, stepId) &&
        (!part.committed || part.commit_ref === data.commit_ref);
      correctedIndex = this.state.timeline.findIndex(superseded);
      this.state.timeline = this.state.timeline.filter((part) => !superseded(part));
      this.discardOriginalTextChunks(data.message_id, stepId, data.commit_ref);
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
          if (validRevision(data.commit_seq)) streamed.commit_seq = data.commit_seq;
          streamed.part_revision = data.part_revision;
          streamed.step = data.step;
          streamed.step_id = data.step_id;
          streamed.terminal = true;
          if (!streamed.committed) this.effects.noteFreshlyCommitted();
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
        if (validRevision(data.commit_seq)) streamed.commit_seq = data.commit_seq;
        streamed.part_revision = data.part_revision;
        streamed.step = data.step;
        streamed.step_id = data.step_id;
        streamed.terminal = true;
        if (!streamed.committed) this.effects.noteFreshlyCommitted();
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
    if (existing && !isStream && validRevision(data.commit_seq)) existing.commit_seq = data.commit_seq;
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
        ? m('stream.gap')
        : m('stream.committedGap');
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
        if (!existing.committed) this.effects.noteFreshlyCommitted();
        existing.committed = true;
        existing.step = data.step;
        if (typeof data.step_id === 'string') existing.step_id = data.step_id;
        existing.commit_ref = data.commit_ref;
      }
      this.applyPending(existing, key);
      this.state.timeline = this.state.timeline.map((part) => (part === existing ? { ...existing } : part));
    } else {
      if (!isStream) this.effects.noteFreshlyCommitted();
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
        ...(validRevision(data.commit_seq) ? { commit_seq: data.commit_seq } : {}),
        observed_at: typeof data.observed_at === 'string' ? data.observed_at : '',
        ...(typeof data.stream_id === 'string' ? { stream_id: data.stream_id } : {}),
        ...(typeof data.request_id === 'string' ? { request_id: data.request_id } : {}),
        ...(typeof data.attempt_id === 'string' ? { attempt_id: data.attempt_id } : {}),
        terminal: data.terminal === true || !isStream,
        gap: false,
      };
      this.applyPending(part, key);
      if (correctedIndex >= 0) {
        // Keep the replaced entry's reading order, with the actual corrected
        // wire identity so a browser witness can bind it to the S1 fact.
        const timeline = [...this.state.timeline];
        timeline.splice(correctedIndex, 0, part);
        this.state.timeline = timeline;
      } else {
        this.state.timeline = [...this.state.timeline, part];
      }
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
  handleDocument(data: Record<string, unknown>): void {
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
  handleStreamPart(data: Record<string, unknown>): void {
    if (!this.state.streaming || !isTimelineChunk(data)) return;
    this.applyChunk(data, true);
  }


  dispose(): void { this.pendingChunks.clear(); }
}
