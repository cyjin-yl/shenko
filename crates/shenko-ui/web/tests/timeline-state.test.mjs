// Pure display-state fixtures: reading place, selection replay and the
// tolerant local-storage reader. Not browser or native evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: `export {
      DISPLAY_STATE_KEY,
      SELECTION_KEY,
      readDisplayState,
      writeDisplayState,
      saveReadingAnchor,
      resolveAnchor,
      setCodeWrap,
      setExpanded,
      recordSelection,
      saveSelection,
      readSelection,
      selectionApplies,
      selectionHash,
    } from './src/reading-state.ts';`,
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'reading-state-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'neutral',
});
const state = await import(
  `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`
);

function memoryStorage(entries = {}) {
  const values = new Map(Object.entries(entries));
  return {
    values,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
  };
}

const anchor = {
  entry_key: 'part.text/0',
  entry_index: 3,
  prev_entry_key: 'submission.a',
  offset: -42.5,
  scroll_top: 1240,
};

test('a saved place resolves by entry identity, then by its predecessor', () => {
  const entries = [{ entry_key: 'part.a' }, { entry_key: 'part.text/0' }, { entry_key: 'part.c' }];
  assert.deepEqual(state.resolveAnchor(anchor, entries), { entry_key: 'part.text/0', index: 1 });
  // The anchored entry was withdrawn: the surviving predecessor holds the
  // place instead of an arbitrary list position.
  assert.deepEqual(
    state.resolveAnchor(anchor, [{ entry_key: 'part.x' }, { entry_key: 'submission.a' }]),
    { entry_key: 'submission.a', index: 1 },
  );
  // Neither identity survives: no invented place.
  assert.equal(state.resolveAnchor(anchor, [{ entry_key: 'part.z' }]), undefined);
  assert.equal(state.resolveAnchor(undefined, entries), undefined);
  assert.equal(state.resolveAnchor(anchor, []), undefined);
});

test('display state persists wrap, sections and place without inventing facts', () => {
  const storage = memoryStorage();
  assert.deepEqual(state.readDisplayState(storage), {
    reading: 'following_latest', wrap: false, expanded: {},
  });
  state.setExpanded(storage, 'section.artifacts', true);
  state.setCodeWrap(storage, true);
  state.saveReadingAnchor(storage, anchor);
  const restored = state.readDisplayState(storage);
  assert.equal(restored.reading, 'reading_history');
  assert.equal(restored.wrap, true);
  assert.equal(restored.expanded['section.artifacts'], true);
  assert.deepEqual(restored.anchor, anchor);

  // Following the latest tail clears the place; the preference survives.
  state.saveReadingAnchor(storage, null);
  const followed = state.readDisplayState(storage);
  assert.equal(followed.reading, 'following_latest');
  assert.equal(followed.anchor, undefined);
  assert.equal(followed.wrap, true);

  // A history reading without a resolvable entry key is not a place: the
  // anchor is dropped and the reader starts following the tail.
  const broken = memoryStorage({
    [state.DISPLAY_STATE_KEY]: JSON.stringify({
      reading: 'reading_history',
      anchor: { offset: 12, scroll_top: 40 },
      wrap: 1,
      expanded: { 'section.status': 'yes', 'section.devices': false },
    }),
  });
  assert.deepEqual(state.readDisplayState(broken), {
    reading: 'following_latest', wrap: false, expanded: { 'section.devices': false },
  });

  // Unreadable JSON is absence, not a fabricated default state.
  const garbage = memoryStorage({ [state.DISPLAY_STATE_KEY]: '{not json' });
  assert.deepEqual(state.readDisplayState(garbage), {
    reading: 'following_latest', wrap: false, expanded: {},
  });
  assert.deepEqual(state.readDisplayState(null), {
    reading: 'following_latest', wrap: false, expanded: {},
  });
});

test('a selection replays only while the same characters still start the entry', () => {
  const text = '第一段：持久提交的正文。\n\n第二段：后续说明。';
  const recorded = state.recordSelection('part.text/0', text, 6, 18);
  assert.ok(recorded);
  assert.equal(recorded.entry_key, 'part.text/0');
  assert.equal(recorded.start, 6);
  assert.equal(recorded.end, 18);
  assert.equal(state.selectionApplies(recorded, text), true);
  // Same length, different content: the recorded span no longer covers the
  // text the reader actually selected.
  const rewritten = `改${text.slice(1)}`;
  assert.equal(state.selectionApplies(recorded, rewritten), false);
  // Truncated entry cannot contain the recorded end.
  assert.equal(state.selectionApplies(recorded, text.slice(0, 10)), false);

  // Reversed or empty ranges are not a selection.
  assert.equal(state.recordSelection('part.text/0', text, 18, 6).start, 6);
  assert.equal(state.recordSelection('part.text/0', text, 5, 5), undefined);
  assert.equal(state.recordSelection('', text, 0, 4), undefined);
  assert.equal(state.selectionHash('abc'), state.selectionHash('abc'));
  assert.notEqual(state.selectionHash('abc'), state.selectionHash('abd'));
});

test('selection storage is separate from the reading place and rejects junk', () => {
  const storage = memoryStorage();
  assert.equal(state.readSelection(storage), undefined);
  state.saveReadingAnchor(storage, anchor);
  const recorded = state.recordSelection('part.text/0', 'abcdef', 1, 4);
  state.saveSelection(storage, recorded);
  assert.deepEqual(state.readSelection(storage), recorded);
  // The place is untouched by the selection record.
  assert.deepEqual(state.readDisplayState(storage).anchor, anchor);
  state.saveSelection(storage, null);
  assert.equal(state.readSelection(storage), undefined);
  assert.deepEqual(state.readDisplayState(storage).anchor, anchor);

  for (const junk of [
    'null',
    '{}',
    '{"entry_key":"","start":0,"end":3,"prefix_hash":"x"}',
    '{"entry_key":"a","start":4,"end":4,"prefix_hash":"x"}',
    '{"entry_key":"a","start":0,"end":4}',
    'not json',
  ]) {
    assert.equal(
      state.readSelection(memoryStorage({ [state.SELECTION_KEY]: junk })),
      undefined,
      junk,
    );
  }
});
