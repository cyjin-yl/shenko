import { message as m } from './messages';
import type { ReadingAnchor, ReadingSelection, ReadingState, UiState } from './projection';
import { recordSelection, saveReadingAnchor, saveSelection, setCodeWrap, setExpanded, type DisplayStorage, type PersistedDisplayState } from './reading-state';
import { isObject } from './live-validation';

export const DRAFT_KEY = 'shenko.draft.v1';
const THEME_KEY = 'shenko.theme.v1';
const DEFAULT_SECTIONS: Record<string, boolean> = {
  status: true,
  activity: true,
  artifacts: false,
  devices: false,
};
const VALID_READING: Record<ReadingState, true> = {
  following_latest: true,
  reading_history: true,
  selecting_or_interacting: true,
};

export function storageGet(key: string): string {
  try {
    return localStorage.getItem(key) ?? '';
  } catch {
    return '';
  }
}

export function storageSet(key: string, value: string): void {
  try {
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
    // Browser storage is optional; the editor and transport keep working.
  }
}

/// Display state is restored through the same tolerant reader as local
/// storage; a privacy mode that denies access simply starts from defaults.
export function displayStorage(): DisplayStorage | null {
  try {
    return localStorage;
  } catch {
    return null;
  }
}

export function initialTheme(): 'dark' | 'light' {
  const query = new URLSearchParams(location.search).get('theme');
  if (query === 'light' || query === 'dark') return query;
  const stored = storageGet(THEME_KEY);
  if (stored === 'light' || stored === 'dark') return stored;
  // Nothing was chosen yet: the reader's own system preference decides, so
  // a reader who prefers the light scheme never sees the dark one first.
  if (typeof window !== 'undefined' && typeof window.matchMedia === 'function') {
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  }
  return 'dark';
}

export function initialSections(persisted: PersistedDisplayState): Record<string, boolean> {
  const sections: Record<string, boolean> = {};
  for (const [name, fallback] of Object.entries(DEFAULT_SECTIONS)) {
    const stored = persisted.expanded[`section.${name}`];
    sections[name] = typeof stored === 'boolean' ? stored : fallback;
  }
  return sections;
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

export function applyDisplayIntent(state: UiState, kind: string, payload: Record<string, unknown>, emit: () => void): boolean {
  switch (kind) {
      case 'panel.toggle':
        state.display.panel_open = !state.display.panel_open;
        state.display.notice = undefined;
        emit();
        break;
      case 'theme.toggle': {
        const theme = state.display.theme === 'dark' ? 'light' : 'dark';
        state.display.theme = theme;
        document.documentElement.dataset.theme = theme;
        document.documentElement.classList.toggle('dark', theme === 'dark');
        document.documentElement.classList.toggle('light', theme === 'light');
        storageSet(THEME_KEY, theme);
        emit();
        break;
      }
      case 'section.toggle': {
        const section = payload.section;
        if (typeof section === 'string' && Object.hasOwn(state.display.open_sections, section)) {
          state.display.open_sections[section] = !state.display.open_sections[section];
          setExpanded(
            displayStorage(),
            `section.${section}`,
            state.display.open_sections[section],
          );
          emit();
        }
        break;
      }
      case 'timeline.reading': {
        const atEnd = payload.at_end === true;
        const requested = payload.state;
        const next: ReadingState = atEnd
          ? 'following_latest'
          : typeof requested === 'string' && Object.hasOwn(VALID_READING, requested)
            ? (requested as ReadingState)
            : 'reading_history';
        const anchor = validAnchorPayload(payload.anchor);
        const previousAnchor = state.display.anchor;
        let changed = state.display.reading !== next
          || (atEnd && state.display.new_count !== 0);
        state.display.reading = next;
        if (atEnd) {
          state.display.new_count = 0;
          if (previousAnchor) {
            state.display.anchor = undefined;
            saveReadingAnchor(displayStorage(), null);
            changed = true;
          }
        } else if (anchor && !sameAnchor(anchor, previousAnchor)) {
          // A place is only restorable when the reader is actually in history;
          // a transient selection is not a place.
          state.display.anchor = anchor;
          saveReadingAnchor(displayStorage(), anchor);
          changed = true;
        }
        if (changed) {
          emit();
        }
        break;
      }
      case 'timeline.latest':
        state.display.reading = 'following_latest';
        state.display.new_count = 0;
        state.display.anchor = undefined;
        saveReadingAnchor(displayStorage(), null);
        state.display.scroll_request += 1;
        emit();
        break;
      case 'timeline.code_wrap': {
        const wrap = !state.display.wrap;
        state.display.wrap = wrap;
        setCodeWrap(displayStorage(), wrap);
        state.display.notice = wrap ? m('wrap.enabled') : m('wrap.disabled');
        emit();
        break;
      }
      case 'timeline.selection': {
        const storage = displayStorage();
        const entryKey = payload.entry_key;
        const text = payload.text;
        const start = payload.start;
        const end = payload.end;
        if (payload.clear === true || typeof entryKey !== 'string' || typeof text !== 'string'
          || typeof start !== 'number' || typeof end !== 'number'
          || !Number.isFinite(start) || !Number.isFinite(end)) {
          if (payload.clear !== true) break;
          if (!state.display.selection) break;
          state.display.selection = undefined;
          saveSelection(storage, null);
          emit();
          break;
        }
        const recorded = recordSelection(entryKey, text, start, end);
        if (!recorded) {
          if (!state.display.selection) break;
          state.display.selection = undefined;
          saveSelection(storage, null);
          emit();
          break;
        }
        if (sameSelection(recorded, state.display.selection)) break;
        state.display.selection = recorded;
        saveSelection(storage, recorded);
        emit();
        break;
      }
      case 'composer.draft':
        if (typeof payload.value === 'string') {
          state.draft = payload.value;
          storageSet(DRAFT_KEY, state.draft);
          emit();
        }
        break;
    default: return false;
  }
  return true;
}
