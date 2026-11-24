import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const webRoot = fileURLToPath(new URL('../', import.meta.url));
const srcDir = join(webRoot, 'src');

const compiledMessages = await build({
  stdin: {
    contents: "export { message, resolveMessage } from './src/messages.ts';",
    resolveDir: webRoot,
    sourcefile: 'messages-audit.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'neutral',
});
const { message, resolveMessage } = await import(
  `data:text/javascript;base64,${Buffer.from(compiledMessages.outputFiles[0].text).toString('base64')}`
);

const CJK_OR_FULLWIDTH = /[\u4e00-\u9fff（）【】：；，。]/;

test('non-catalog UI sources contain no hardcoded CJK strings outside demo data', () => {
  const exempt = new Set(['messages.ts', 'demo-controller.ts', 'demo-fixtures.ts']);
  const hits = [];
  for (const name of readdirSync(srcDir).sort()) {
    if (!/\.tsx?$/.test(name) || exempt.has(name)) continue;
    const lines = readFileSync(join(srcDir, name), 'utf8').split('\n');
    lines.forEach((line, index) => {
      const stripped = line.replace(/\/\/.*$/, '').replace(/\/\*.*?\*\//g, '');
      if (CJK_OR_FULLWIDTH.test(stripped)) {
        hits.push(`${name}:${index + 1}: ${line.trim()}`);
      }
    });
  }
  assert.deepEqual(hits, [], `Unexpected hardcoded CJK in UI source:\n${hits.join('\n')}`);
});

test('English locale resolves all shell, inspector, timeline, and files labels without CJK leakage', () => {
  const checks = [
    message('common.open'),
    message('common.withdraw'),
    message('common.colon'),
    message('common.listSep'),
    message('work.details'),
    message('receipt.retrySame'),
    message('shell.shortcut', { key: 1 }),
    message('inspector.attentionTitle'),
    message('inspector.attentionTasks', { count: 2 }),
    message('inspector.attentionOpen'),
    message('inspector.summaryTasks', { active: 2, total: 5 }),
    message('inspector.openThread'),
    message('inspector.emptyAuxiliary', { roots: 0, namespaces: 0 }),
    message('inspector.runtimeBuild'),
    message('inspector.capabilities'),
    message('files.searchPlaceholder'),
    message('files.recent'),
    message('files.itemsCount', { count: 4 }),
    message('files.filteredCount', { shown: 2, total: 4 }),
    message('files.badge.ro'),
    message('files.badge.rw'),
    message('files.badge.cow'),
    message('files.badge.offline'),
    message('files.statusReadonly'),
    message('files.noMatches'),
  ];
  for (const item of checks) {
    const rendered = resolveMessage(item, 'en');
    assert.ok(rendered.length > 0, `Empty en translation for ${item.message_id}`);
    assert.equal(CJK_OR_FULLWIDTH.test(rendered), false, `CJK leaked in en for ${item.message_id}: ${rendered}`);
  }
});

test('composed scroll projection hides WebKit scrollbars and ReadingViewport opts into composed projection', () => {
  const uiKit = readFileSync(join(srcDir, 'ui-kit.tsx'), 'utf8');
  const css = readFileSync(join(srcDir, 'app.css'), 'utf8');
  assert.match(uiKit, /asScrollSurface\(\)\.configure\(\{\s*anchor:\s*\{\s*mode:\s*'off'\s*\},\s*projection:\s*'composed'\s*\}\)/);
  assert.match(css, /\[data-pui-scroll-projection="composed"\]::-webkit-scrollbar\s*\{[^}]*display:\s*none/);
  // Apple HIG / native-feel invariants: overlay scrollbars hidden at rest,
  // chrome keeps default arrow cursor, no pulsing status dot halo, no gradient avatars.
  assert.match(css, /\.sk-scrollbar,\s*\.timeline-scrollbar\s*\{[^}]*opacity:\s*0/);
  assert.match(css, /\[data-pui-style~="cursor-pointer"\][^{]*\{[^}]*cursor:\s*default/);
  assert.doesNotMatch(css, /@keyframes\s+avatar-live/);
  assert.doesNotMatch(css, /\.monogram[^{]*linear-gradient/);
  assert.doesNotMatch(css, /text-transform:\s*uppercase/);
});
