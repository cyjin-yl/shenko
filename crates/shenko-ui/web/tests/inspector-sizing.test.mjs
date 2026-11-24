// Source policy consumers. Real public-owner input and layout need the native App matrix.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const webRoot = fileURLToPath(new URL('../', import.meta.url));
const compiled = await build({
  stdin: {
    contents: "export * from './src/inspector-sizing.ts'; export { storageGet, storageSet } from './src/ui-preferences.ts';",
    resolveDir: webRoot, sourcefile: 'inspector-policy-consumer.mjs',
  },
  bundle: true, write: false, format: 'esm', platform: 'browser',
});
const {
  inspectorLayout, inspectorProposal, readInspectorWidth, storageGet, storageSet,
  INSPECTOR_WIDTH_KEY,
} = await import(`data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`);
const near = (actual, expected, message) => {
  assert.ok(Math.abs(actual - expected) < 1e-8, `${message}: ${actual} !== ${expected}`);
};

test('the 1100px desktop boundary reserves the real separator and 638px stream', () => {
  const layout = inspectorLayout({ groupContentWidth: 1100, separatorWidth: 1 }, null);
  assert.ok(layout);
  near(layout.extent, 1099, 'only Panel extents are normalized');
  near(layout.width, 461, 'the old 462px preferred lens yields one pixel to the separator');
  near(layout.sizes[0] * 1099 / 100, 638, 'minimum actual stream width');
  near(layout.sizes[1] * 1099 / 100, 461, 'actual lens width');
  near(layout.constraints[1].min * 1099 / 100, 400, 'minimum actual lens width');
  near(layout.constraints[1].max * 1099 / 100, 461, 'maximum actual lens at breakpoint');
  assert.equal(inspectorLayout({ groupContentWidth: 1099, separatorWidth: 1 }, null), null);
  assert.equal(inspectorLayout({ groupContentWidth: 1100, separatorWidth: 0 }, null), null);
});

test('a wider separator and viewport clamp preserve the preference without borrowing pixels', () => {
  const narrow = inspectorLayout({ groupContentWidth: 1280, separatorWidth: 9 }, 680);
  const wide = inspectorLayout({ groupContentWidth: 1600, separatorWidth: 9 }, 680);
  assert.ok(narrow && wide);
  near(narrow.extent, 1271, 'nine physical separator pixels are excluded');
  near(narrow.width, 633, 'main reading space limits the narrow lens');
  near(narrow.sizes[0] * 1271 / 100, 638, 'the narrow main does not shrink under policy');
  near(wide.width, 680, 'the stored expanded preference restores when space returns');
  near(wide.sizes[1] * 1591 / 100, 680, 'wide physical lens maximum');
  const minimum = inspectorLayout({ groupContentWidth: 1600, separatorWidth: 9 }, 1);
  near(minimum.width, 400, 'a low proposal never becomes a subminimum lens');
  for (const measurement of [
    { groupContentWidth: NaN, separatorWidth: 1 },
    { groupContentWidth: Infinity, separatorWidth: 1 },
    { groupContentWidth: 1280, separatorWidth: -1 },
    { groupContentWidth: 1280, separatorWidth: Infinity },
    { groupContentWidth: 1100, separatorWidth: 100 },
  ]) assert.equal(inspectorLayout(measurement, null), null);
});

test('only complete bounded public proposals enter App state; collapse keeps the expanded preference', () => {
  const layout = inspectorLayout({ groupContentWidth: 1600, separatorWidth: 1 }, 500);
  assert.ok(layout);
  const expanded = inspectorProposal([100 - 500 * 100 / 1599, 500 * 100 / 1599], layout);
  assert.equal(expanded.collapsed, false);
  near(expanded.width, 500, 'a valid complete public proposal');
  assert.deepEqual(inspectorProposal([100, 0], layout), { collapsed: true, width: null });
  const hole = Array(2); hole[0] = 100;
  for (const proposal of [
    [], [100], [100, 0, 0], hole, [NaN, NaN], [Infinity, -Infinity],
    [-1, 101], [60, 30], [99, 1], [10, 90],
  ]) assert.equal(inspectorProposal(proposal, layout), null);
  near(layout.width, 500, 'validating collapse does not overwrite the remembered expanded width');
});

test('stored widths are bounded and denied browser storage falls back without breaking layout', (context) => {
  for (const value of ['', ' ', 'broken', 'NaN', 'Infinity', '399', '681'])
    assert.equal(readInspectorWidth(value), null);
  assert.equal(readInspectorWidth('400'), 400);
  assert.equal(readInspectorWidth(' 512.5 '), 512.5);
  assert.equal(readInspectorWidth('680'), 680);
  const previous = Object.getOwnPropertyDescriptor(globalThis, 'localStorage');
  Object.defineProperty(globalThis, 'localStorage', {
    configurable: true,
    get() { throw new Error('storage denied'); },
  });
  context.after(() => {
    if (previous) Object.defineProperty(globalThis, 'localStorage', previous);
    else delete globalThis.localStorage;
  });
  assert.equal(readInspectorWidth(storageGet(INSPECTOR_WIDTH_KEY)), null);
  assert.doesNotThrow(() => storageSet(INSPECTOR_WIDTH_KEY, '500'));
  near(inspectorLayout({ groupContentWidth: 1280, separatorWidth: 1 }, null).width,
    537.6, 'denied storage uses the existing 42vw preference');
});
