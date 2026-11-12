// Registry lifecycle and browser-storage failure boundaries; no browser or
// provider evidence. The same TypeScript module is bundled for the consumer.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: "export * from './src/slots.ts';",
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'lens-registry-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'neutral',
});
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`;
let sequence = 0;

async function registry(storage, context) {
  const original = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', { configurable: true, value: storage });
  context.after(() => {
    if (original) Object.defineProperty(globalThis, 'localStorage', original);
    else delete globalThis.localStorage;
  });
  return import(`${moduleUrl}#consumer-${sequence++}`);
}

function memoryStorage() {
  const values = new Map();
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
  };
}

const page = { artifact_id: 'page-one', title: 'A page', mime: 'text/html' };

function workLens(lenses) {
  return lenses.lensSnapshot().find((lens) => lens.artifact_id === page.artifact_id);
}

test('a storage refusal keeps a work pinned for this page and still allows unpinning', async (context) => {
  const lenses = await registry({
    getItem: () => null,
    setItem: () => { throw new DOMException('Storage is unavailable', 'QuotaExceededError'); },
  }, context);
  lenses.syncWorkLenses([page]);
  lenses.setWorkPinned(page.artifact_id, true);
  assert.ok(lenses.pinnedWorks().includes(page.artifact_id));
  assert.equal(workLens(lenses)?.title, page.title);
  lenses.setWorkPinned(page.artifact_id, false);
  assert.ok(!lenses.pinnedWorks().includes(page.artifact_id));
  assert.equal(workLens(lenses), undefined);
});

test('an unavailable work listing preserves lenses while a confirmed empty listing removes them', async (context) => {
  const lenses = await registry(memoryStorage(), context);
  lenses.syncWorkLenses([page]);
  lenses.setWorkPinned(page.artifact_id, true);
  lenses.syncWorkLenses(undefined);
  assert.equal(workLens(lenses)?.title, page.title);
  lenses.syncWorkLenses([]);
  assert.equal(workLens(lenses), undefined);
  assert.ok(lenses.pinnedWorks().includes(page.artifact_id), 'the reading preference survives an absent work');
  lenses.syncWorkLenses([{ ...page, title: 'A revised page' }]);
  assert.equal(workLens(lenses)?.title, 'A revised page');
});
