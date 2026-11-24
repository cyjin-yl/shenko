/**
 * The rich layer of a semantic document renders as components, not as a JSON
 * dump (acceptance UI-S11).
 *
 * The test renders the real component tree to static markup and reads the
 * result the way a browser would: a table is a table, a section is a section,
 * an unknown kind is named instead of dumped, and a node's action keeps the
 * document's own action identity.
 */
import assert from 'node:assert/strict';
import test from 'node:test';
import { mkdirSync, mkdtempSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { build } from 'esbuild';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';

// Keep each generated bundle in the ignored dependency cache. It resolves
// the SAME react instance as the test without modifying authored sources or
// sharing an output file with another test process.
const webRoot = fileURLToPath(new URL('../', import.meta.url));
const cacheRoot = join(webRoot, 'node_modules', '.cache');
mkdirSync(cacheRoot, { recursive: true });
const bundlePath = join(mkdtempSync(join(cacheRoot, 'shenko-document-')), 'consumer.mjs');
await build({
  stdin: {
    contents: "export { RichDocument } from '../src/document-view.tsx'; export { SemanticDocumentView } from '../src/timeline-entries.tsx'; export { Messages } from '../src/im.tsx'; export { buildProjection } from '../src/projection.ts'; export { initialState } from '../src/live-state.ts'; export { collectThreads } from '../src/threads.ts';",
    resolveDir: join(webRoot, 'tests'),
    sourcefile: 'document-view-consumer.mjs',
  },
  bundle: true,
  outfile: bundlePath,
  format: 'esm',
  platform: 'node',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react/jsx-runtime'],
});
const { RichDocument, SemanticDocumentView, Messages, buildProjection, initialState, collectThreads } = await import(pathToFileURL(bundlePath).href);

test('a revised decision remains actionable while earlier admitted receipts retain their own revision', () => {
  const receipt = { submission_id: 'choice-1', document_id: 'decision', action_id: 'review', revision: 'r1', stage: 'accepted' };
  const node = {
    node_id: 'document.decision', action_refs: ['document.decision.review'],
    props: {
      document: { document_id: 'decision', revision: 'r1', mode: 'card', summary: 'Choose a revision',
        options: [{ action_id: 'review', nonce: 'choice-r1', revision: 'r1', label: 'Review' }] },
      receipts: [receipt],
    },
  };
  const markup = () => renderToStaticMarkup(createElement(SemanticDocumentView, { node, emit: () => {} }));
  assert.match(markup(), /disabled=""/, 'the admitted original revision cannot be submitted twice');
  node.props.document = { ...node.props.document, revision: 'r2',
    options: [{ action_id: 'review', nonce: 'choice-r2', revision: 'r2', label: 'Review' }] };
  node.props.receipts.push({ ...receipt, document_id: 'another-decision', revision: 'r2' });
  const revised = markup();
  assert.doesNotMatch(revised, /disabled=""/, 'neither old nor foreign receipts disable the new decision');
  assert.doesNotMatch(revised, /data-stage="accepted"/, 'old admission is not displayed as this revision\'s result');
  assert.equal(receipt.revision, 'r1', 'the historical receipt is preserved');
});

const markdown = (text) => createElement('div', { className: 'markdown-body' }, text);

test('grouped IM owner rows each retain their saved edit recovery action', (context) => {
  const previousLocation = Object.getOwnPropertyDescriptor(globalThis, 'location');
  Object.defineProperty(globalThis, 'location', { value: { search: '' }, configurable: true });
  context.after(() => {
    if (previousLocation) Object.defineProperty(globalThis, 'location', previousLocation);
    else delete globalThis.location;
  });
  const state = initialState();
  state.source = 'live';
  state.connection = 'online';
  state.subject_id = 'fixture-subject';
  state.submissions = [1, 2, 3].map((id) => ({
    payload: { schema_version: 2, type: 'submit', mode: 'append', subject_id: 'fixture-subject',
      submission_id: `saved-${id}`, message_id: `message-${id}`, content: `Original ${id}`, attachment_refs: [] },
    created_at: `2026-11-19T00:00:0${id}Z`, receipt: 'accepted', send_epoch: 1,
    withdrawal: { cancel_id: `cancel-${id}`, commit_ref: `ledger/${id}`, result: 'withdrawn_before_presentation' },
    edit: { cancel_id: `cancel-${id}`, state: id === 3 ? 'closed' : 'ready',
      submission_id: `replacement-${id}`, message_id: `replacement-message-${id}`,
      draft: { content: `Saved edit ${id}`, attachment_refs: [] } },
  }));
  const projection = buildProjection(state, 1);
  const threads = collectThreads({ projection, tasks: [], conversations: [] });
  const direct = threads[0];
  // Test the actual row renderer's action ownership. MessageScroller now
  // materializes the outer viewport after mount; its live App journey is
  // scripts/web_reading_journey.mjs, not server static markup.
  const markup = renderToStaticMarkup(createElement(Messages, {
    messages: direct.messages, viewer: direct.viewer, group: false,
    renderAttachment: () => null, emit: () => {}, onOpenInspect: () => {},
  }));
  for (const id of [1, 2]) {
    const start = markup.indexOf(`data-entry-key="submission.saved-${id}"`);
    const end = markup.indexOf(`data-entry-key="submission.saved-${id + 1}"`, start + 1);
    assert.ok(start >= 0 && end > start, 'the earlier grouped owner row exists');
    assert.ok(markup.slice(start, end).includes(`data-action-id="submission.edit.saved-${id}"`),
      'each non-tail row exposes its own legitimate recovery action');
    assert.equal(projection.actions[`submission.edit.saved-${id}`].enabled, true);
  }
  assert.equal(markup.includes('data-action-id="submission.edit.saved-3"'), false,
    'a closed edit does not acquire a recovery action from grouping');
});

function render(nodes, options = [], actionRefs = []) {
  return renderToStaticMarkup(createElement(RichDocument, {
    nodes,
    options,
    actionRefs,
    onAction: () => {},
    renderMarkdown: markdown,
  }));
}

test('a rich document renders components, not a JSON dump', () => {
  const markup = render([
    {
      node_id: 'section-1',
      kind: 'section',
      props: { title: '观测结果' },
      children: [
        { node_id: 'text-1', kind: 'text', props: { text: '今天完成。' } },
      ],
    },
    {
      node_id: 'table-1',
      kind: 'table',
      props: { columns: ['指标', '值'], rows: [['延迟', '42ms']] },
    },
  ]);
  assert.match(markup, /<section[^>]*data-node-kind="section"/, markup);
  assert.match(markup, /观测结果/, markup);
  assert.match(markup, /<table[^>]*data-node-kind="table"|<table class="document-table"/, markup);
  assert.match(markup, /<th[^>]*>指标<\/th>/, markup);
  assert.match(markup, /42ms/, markup);
  assert.doesNotMatch(markup, /&quot;node_id&quot;/, 'no JSON dump reaches the reader');
  assert.doesNotMatch(markup, /document-tree/, 'the debug view is gone');
});

test('a code node keeps its content and loses nothing else', () => {
  const markup = render([
    { node_id: 'code-1', kind: 'code', props: { code: 'fn main() {}', language: 'rust' } },
  ]);
  assert.match(markup, /fn main\(\) \{\}/, markup);
  assert.match(markup, /rust/, markup);
});

test('a markdown node reuses the shell renderer', () => {
  const markup = render([
    { node_id: 'md-1', kind: 'markdown', props: { text: '**重点**' } },
  ]);
  assert.match(markup, /markdown-body/, markup);
  assert.match(markup, /重点/, markup);
});

test('an unknown kind is named and readable, never dumped', () => {
  const markup = render([
    { node_id: 'odd-1', kind: 'sunburst', props: { text: '一段正文' } },
  ]);
  assert.match(markup, /sunburst/, 'the kind is named');
  assert.match(markup, /一段正文/, 'the node keeps its own text');
  assert.doesNotMatch(markup, /&quot;kind&quot;/, 'no JSON dump');
});

test("a node's action submits the document's own action identity", () => {
  const nodes = [
    {
      node_id: 'select-1',
      kind: 'select',
      props: { label: '选一个', options: [{ value: 'a', label: '方案 A' }] },
      action: { action_id: 'act-1' },
    },
  ];
  const markup = render(nodes, [{ action_id: 'act-1', nonce: 'n', revision: 'r', label: '方案 A' }], ['document.d1.act-1']);
  assert.match(markup, /方案 A/, markup);
  assert.doesNotMatch(markup, /act-1/, 'the client does not invent an action identity');
});
