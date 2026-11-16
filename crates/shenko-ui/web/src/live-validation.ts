export function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function validRevision(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

// Input receipts reconstruct this reference in the receiving ledger. Part
// commit_ref is an immutable source identity and must never use this parser.
export function admissionSequence(reference: string): number | undefined {
  const slash = reference.lastIndexOf('/');
  if (slash <= 0) return undefined;
  const suffix = reference.slice(slash + 1);
  const sequence = Number(suffix);
  return /^\d+$/.test(suffix) && validRevision(sequence) ? sequence : undefined;
}

export interface TimelineChunk extends Record<string, unknown> {
  message_id: string;
  part_id: string;
  part_revision: string;
  commit_ref: string;
  content: string;
  step: number;
  chunk_seq: number;
}

export function isTimelineChunk(data: Record<string, unknown>): data is TimelineChunk {
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
