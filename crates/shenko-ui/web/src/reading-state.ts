/**
 * Browser-local display state for the single subject timeline (docs/specs/web-interface.md#spec-21-11).
 *
 * Only display facts live here — reading position, code wrapping and which
 * disclosures the reader opened. Durable business facts never do: those come
 * from the Store through the WS projection. A saved anchor is a *hint* that is
 * re-resolved against the committed entries actually rendered now, so a
 * deleted/withdrawn entry falls back to an explainable nearby position instead
 * of jumping to an arbitrary list end.
 */
import type { ReadingAnchor, ReadingSelection } from './projection';

export const DISPLAY_STATE_KEY = 'shenko.display.v1';
/** A text selection lives under its own key: an unreadable selection must
 *  never take the reading place down with it, and vice versa. */
export const SELECTION_KEY = 'shenko.selection.v1';

export interface PersistedDisplayState {
  /** Only these two readings are a place; a transient selection is not. */
  reading: 'following_latest' | 'reading_history';
  /** `null` means the reader follows the tail again (an explicit choice). */
  anchor?: ReadingAnchor | null;
  wrap: boolean;
  expanded: Record<string, boolean>;
}

export interface DisplayStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

export interface EntryAnchorSource {
  entry_key: string;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function finite(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value);
}

export function emptyDisplayState(): PersistedDisplayState {
  return { reading: 'following_latest', wrap: false, expanded: {} };
}

/** Tolerant read: an unreadable entry restores as absent, never as a fact. */
export function readDisplayState(storage: DisplayStorage | null): PersistedDisplayState {
  if (!storage) return emptyDisplayState();
  let raw: unknown;
  try {
    raw = JSON.parse(storage.getItem(DISPLAY_STATE_KEY) ?? 'null');
  } catch {
    return emptyDisplayState();
  }
  if (!isObject(raw)) return emptyDisplayState();
  const state = emptyDisplayState();
  if (raw.reading === 'reading_history' && validAnchor(raw.anchor)) {
    state.reading = 'reading_history';
    state.anchor = normaliseAnchor(raw.anchor);
  }
  if (raw.wrap === true) state.wrap = true;
  if (isObject(raw.expanded)) {
    for (const [key, value] of Object.entries(raw.expanded)) {
      if (typeof value === 'boolean') state.expanded[key] = value;
    }
  }
  return state;
}

function validAnchor(value: unknown): value is Record<string, unknown> {
  return isObject(value)
    && typeof value.entry_key === 'string'
    && value.entry_key.length > 0
    && finite(value.offset);
}

function normaliseAnchor(value: Record<string, unknown>): ReadingAnchor {
  return {
    entry_key: String(value.entry_key),
    entry_index: finite(value.entry_index) ? Math.max(0, Math.trunc(value.entry_index)) : 0,
    ...(typeof value.prev_entry_key === 'string' && value.prev_entry_key
      ? { prev_entry_key: value.prev_entry_key }
      : {}),
    offset: Number(value.offset),
    scroll_top: finite(value.scroll_top) ? Math.max(0, value.scroll_top) : 0,
  };
}

export function writeDisplayState(
  storage: DisplayStorage | null,
  patch: Partial<PersistedDisplayState> & { anchor?: ReadingAnchor | null },
): PersistedDisplayState {
  const next = { ...readDisplayState(storage), ...patch } as PersistedDisplayState;
  if (patch.anchor === null) delete next.anchor;
  if (next.reading !== 'reading_history') delete next.anchor;
  if (!storage) return next;
  try {
    storage.setItem(DISPLAY_STATE_KEY, JSON.stringify(next));
  } catch {
    // Storage may be full or disabled; the timeline still works this session.
  }
  return next;
}

/** Persist the reader's place; `null` clears it (following the latest tail). */
export function saveReadingAnchor(
  storage: DisplayStorage | null,
  anchor: ReadingAnchor | null,
): PersistedDisplayState {
  const patch: Partial<PersistedDisplayState> = anchor
    ? { reading: 'reading_history', anchor }
    : { reading: 'following_latest', anchor: null };
  return writeDisplayState(storage, patch);
}

export function setCodeWrap(storage: DisplayStorage | null, wrap: boolean): PersistedDisplayState {
  return writeDisplayState(storage, { wrap });
}

export function setExpanded(
  storage: DisplayStorage | null,
  key: string,
  open: boolean,
): PersistedDisplayState {
  const current = readDisplayState(storage);
  return writeDisplayState(storage, {
    expanded: { ...current.expanded, [key]: open },
  });
}

/**
 * Resolve a saved anchor against the entries rendered now. Exact identity
 * wins; otherwise the nearest surviving predecessor holds the place; the
 * absolute scroll offset is the last resort and is never invented.
 */
export function resolveAnchor(
  anchor: ReadingAnchor | undefined,
  entries: EntryAnchorSource[],
): { entry_key: string; index: number } | undefined {
  if (!anchor || entries.length === 0) return undefined;
  const exact = entries.findIndex((entry) => entry.entry_key === anchor.entry_key);
  if (exact >= 0) return { entry_key: anchor.entry_key, index: exact };
  if (anchor.prev_entry_key) {
    const previous = entries.findIndex((entry) => entry.entry_key === anchor.prev_entry_key);
    if (previous >= 0) return { entry_key: anchor.prev_entry_key, index: previous };
  }
  return undefined;
}

export type RecordedSelection = ReadingSelection;

/** Non-secret content fingerprint (FNV-1a); not a cryptographic digest. */
export function selectionHash(text: string): string {
  let hash = 2166136261;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
}

export function recordSelection(
  entryKey: string,
  text: string,
  start: number,
  end: number,
): RecordedSelection | undefined {
  if (!entryKey) return undefined;
  const lower = Math.max(0, Math.min(start, end));
  const upper = Math.min(text.length, Math.max(start, end));
  if (upper <= lower) return undefined;
  return {
    entry_key: entryKey,
    start: lower,
    end: upper,
    prefix_hash: selectionHash(text.slice(0, upper)),
  };
}

/** True only when the recorded span still covers the same rendered text. */
export function selectionApplies(recorded: RecordedSelection, currentText: string): boolean {
  if (currentText.length < recorded.end) return false;
  return selectionHash(currentText.slice(0, recorded.end)) === recorded.prefix_hash;
}

/** Tolerant read: a malformed entry restores as no selection, not as a fact. */
export function readSelection(storage: DisplayStorage | null): ReadingSelection | undefined {
  if (!storage) return undefined;
  let raw: unknown;
  try {
    raw = JSON.parse(storage.getItem(SELECTION_KEY) ?? 'null');
  } catch {
    return undefined;
  }
  if (!isObject(raw)) return undefined;
  if (
    typeof raw.entry_key !== 'string' || raw.entry_key.length === 0 ||
    !finite(raw.start) || !finite(raw.end) || typeof raw.prefix_hash !== 'string' ||
    raw.prefix_hash.length === 0
  ) return undefined;
  const start = Math.max(0, Math.trunc(raw.start));
  const end = Math.trunc(raw.end);
  if (end <= start) return undefined;
  return { entry_key: raw.entry_key, start, end, prefix_hash: raw.prefix_hash };
}

/** Persist the reader's selection; `null` clears it. */
export function saveSelection(
  storage: DisplayStorage | null,
  selection: ReadingSelection | null,
): void {
  if (!storage) return;
  try {
    if (selection) storage.setItem(SELECTION_KEY, JSON.stringify(selection));
    else storage.removeItem(SELECTION_KEY);
  } catch {
    // Storage may be full or disabled; the selection still works this session.
  }
}

