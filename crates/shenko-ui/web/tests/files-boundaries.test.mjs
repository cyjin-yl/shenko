// Deterministic source consumers; the real served UI needs the separate smoke.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const webRoot = fileURLToPath(new URL('../', import.meta.url));
const compiled = await build({
  stdin: { contents: "export { liveFiles } from './src/files-view.tsx'; export { demoThreadData } from './src/demo-fixtures.ts';",
    resolveDir: webRoot, sourcefile: 'files-boundary-consumer.mjs' },
  bundle: true, write: false, format: 'esm', platform: 'browser',
});
const { liveFiles, demoThreadData } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`
);

test('live Files preserves unavailable and partial answers and refuses incomplete or failed responses', async (context) => {
  const previousFetch = globalThis.fetch;
  context.after(() => { globalThis.fetch = previousFetch; });
  let body = { available: false, roots: [], nodes_known: false, error: 'not admitted' };
  let status = 200;
  const calls = [];
  globalThis.fetch = async (url, options) => {
    calls.push({ url, options });
    return new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
  };
  assert.deepEqual(await liveFiles.roots(), body);
  body = { available: false, entries: [], truncated: false, error: 'not admitted' };
  assert.deepEqual(await liveFiles.list('mount:/home', ''), body);
  body = { available: true, entries: [{ id: 'ff', name: '\ufffd', kind: 'dir', bytes: 0, modified: null }], truncated: true };
  assert.deepEqual(await liveFiles.list('mount:/home', 'ff/6e6f746573'), body);
  const request = new URL(calls.at(-1).url, 'http://localhost');
  assert.equal(request.searchParams.get('path'), 'ff/6e6f746573');
  assert.equal(request.searchParams.get('root'), 'mount:/home');
  assert.equal(calls.at(-1).options.cache, 'no-store');
  body = { entries: [] };
  await assert.rejects(liveFiles.list('mount:/home', ''));
  body = { available: true, nodes_known: false };
  await assert.rejects(liveFiles.roots());
  body = { available: true, entries: [], truncated: false, error: 'listing failed' };
  status = 503;
  await assert.rejects(liveFiles.list('mount:/home', ''), /listing failed/);
});

test('the demo source navigates by native component identity and labels remain display only', async () => {
  const source = demoThreadData.files;
  const roots = await source.roots();
  assert.equal(roots.available, true);
  const home = roots.roots.find(root => root.kind === 'home');
  const listing = await source.list(home.id, '');
  assert.equal(listing.truncated, false);
  const notes = listing.entries.find(entry => entry.name === 'notes');
  assert.equal(notes.id, '6e6f746573');
  const contents = await source.list(home.id, notes.id);
  assert.deepEqual(contents.entries.map(entry => entry.name), ['2026-11-12-deploy.md', 'cold-cache.md']);
  assert.ok(contents.entries.every(entry => entry.id && entry.id !== entry.name));
  assert.equal(contents.available, true);
  assert.equal(contents.truncated, false);
});
