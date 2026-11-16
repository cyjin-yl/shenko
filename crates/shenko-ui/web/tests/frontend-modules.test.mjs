// Source consumers only. Native browser, served chunks and model behavior need separate evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const webRoot = fileURLToPath(new URL('../', import.meta.url));
const compiled = await build({
  stdin: { contents: "export * from './src/messages.ts'; export { buildProjection } from './src/projection.ts'; export { initialState } from './src/live-state.ts';",
    resolveDir: webRoot, sourcefile: 'localized-projection-consumer.mjs' },
  bundle: true, write: false, format: 'esm', platform: 'browser',
});
const { message, resolveMessage, resolveLocale, buildProjection, initialState } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`
);

test('catalog IDs survive projection with typed parameters, deterministic locale fallback and raw diagnostics', (context) => {
  const previousLocation = Object.getOwnPropertyDescriptor(globalThis, 'location');
  Object.defineProperty(globalThis, 'location', { value: { search: '' }, configurable: true });
  context.after(() => {
    if (previousLocation) Object.defineProperty(globalThis, 'location', previousLocation);
    else delete globalThis.location;
  });
  assert.equal(resolveLocale('en-GB'), 'en');
  assert.equal(resolveLocale(['fr-CA', 'zh-TW']), 'zh-CN');
  assert.equal(resolveLocale('fr-CA'), 'zh-CN');
  const detail = 'HTTP 409 · revision 已过期 · source[id]';
  const notice = message('error.document', { detail });
  assert.equal(resolveMessage(notice, 'en'), 'Document could not render: ' + detail);
  assert.equal(resolveMessage(notice, resolveLocale('unsupported')), '文档未能呈现：' + detail);
  assert.equal(resolveMessage(detail, 'en'), detail, 'server text is never translated or reparsed');
  const state = initialState();
  state.display.notice = notice;
  const projection = buildProjection(state, 1);
  assert.deepEqual(projection.nodes.find(node => node.kind === 'status_notice').props.text, notice);
  assert.equal(projection.nodes.find(node => node.kind === 'timeline').a11y.label.message_id, 'timeline.label');
});

test('the live static module closure excludes fixtures while the demo import retains its complete chunk', async () => {
  const bundle = await build({
    stdin: { contents: "export { createLiveController } from './src/live-controller.ts'; export { collectThreads } from './src/threads.ts'; export function openDemo() { return import('./src/demo-controller.ts'); }",
      resolveDir: webRoot, sourcefile: 'live-demo-entry.mjs' },
    bundle: true, write: false, format: 'esm', platform: 'browser', splitting: true,
    outdir: webRoot + '/consumer-output', metafile: true,
  });
  const outputs = bundle.metafile.outputs;
  const entry = Object.entries(outputs).find(([, output]) => output.entryPoint === 'live-demo-entry.mjs' || output.entryPoint === '<stdin>');
  assert.ok(entry, 'the actual live entry output is identified');
  const visited = new Set();
  function visit(path) {
    if (visited.has(path)) return;
    visited.add(path);
    const output = outputs[path];
    assert.ok(output, 'each static dependency is an emitted output');
    assert.ok(!Object.keys(output.inputs).some(input => /(?:demo-controller|demo-fixtures)\.ts$/.test(input)));
    for (const dependency of output.imports) {
      if (dependency.external || dependency.kind === 'dynamic-import') continue;
      visit(dependency.path);
    }
  }
  visit(entry[0]);
  assert.ok(Object.values(outputs).some(output => Object.keys(output.inputs).some(input => input.endsWith('src/demo-fixtures.ts'))),
    'the dynamically loaded fixture chunk is still shipped');
  assert.ok(Object.values(outputs).some(output => output.imports.some(dependency => dependency.kind === 'dynamic-import')));
});
