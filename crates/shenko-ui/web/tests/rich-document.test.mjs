/**
 * The rich layer of a semantic document renders as components, not as a JSON
 * dump (docs/specs/configuration-playground.md#spec-57-6, docs/specs/web-interface.md#spec-21-6, acceptance UI-S11).
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
    contents: "export { RichDocument } from '../src/document-view.tsx';",
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
const { RichDocument } = await import(pathToFileURL(bundlePath).href);

const markdown = (text) => createElement('div', { className: 'markdown-body' }, text);

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
