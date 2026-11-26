import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { test } from 'node:test';

// Run each complete production observer, without extracting or rewriting its
// control flow. Only the browser is a double; these are failure-receipt tests,
// not App/browser acceptance. The real journeys run separately after building.
const fakeBrowser = `
let contextCount = 0, pageError;
function page() {
  let width = 1280, expanded = false;
  const locator = (selector) => ({
    first() { return this; }, last() { return this; }, filter() { return this; },
    async waitFor() {}, async focus() {}, async fill() {}, async press() {}, async hover() {},
    async click() { if (selector === '.im-archive-toggle') expanded = !expanded; },
    async count() { return selector.includes('.im-row-title') ? 0 : 1; },
    async evaluate() { return true; },
    async isVisible() { return selector !== '.files-search' || width >= 600; },
    async getAttribute(name) {
      return name === 'aria-expanded' ? String(expanded) : name === 'tabindex' ? '0' : 'true';
    },
  });
  return {
    locator, keyboard: { async press() {} }, mouse: { async wheel() {} },
    setDefaultTimeout() {}, on(name, callback) { if (name === 'pageerror') pageError = callback; }, async addInitScript() {}, async goto() {},
    async waitForFunction() {}, async screenshot() {},
    async setViewportSize(viewport) { width = viewport.width; },
    async evaluate(fn) {
      const text = String(fn);
      if (text.includes('new KeyboardEvent')) return { prevented: width >= 600, searchFocused: width >= 600 };
      if (text.includes('window.__imInputs')) return [{ type: 'keydown', trusted: true }];
      return { viewport: width, overflow: 0 };
    },
  };
}
export const chromium = { async launch() { return {
  version() { return 'receipt-test-double'; },
  async newPage() { return page(); },
  async newContext() {
    const index = ++contextCount;
    if (process.env.OBSERVER_FAULT === 'outer' && index === 2) throw Error('injected outer failure');
    return { async newPage() { return page(); }, async close() {
      if (process.env.OBSERVER_FAULT === 'context' && index === 2) throw Error('injected context close');
    } };
  },
  async close() {
    if (process.env.OBSERVER_FAULT === 'late-pageerror') pageError(Error('injected late page error'));
    if (process.env.OBSERVER_FAULT === 'browser') throw Error('injected browser close');
  },
}; } };
`;
const hash = (value) => createHash('sha256').update(value).digest('hex');

async function observe(suite, fault = '') {
  const root = await mkdtemp(path.join(os.tmpdir(), 'shenko-observer-receipt-'));
  try {
    const web = path.join(root, 'crates/shenko-ui/web');
    const assets = path.join(root, 'crates/shenko-ui/src/static');
    const upstream = path.join(root, 'upstream');
    const evidence = path.join(root, 'evidence');
    for (const directory of [web + '/scripts', web + '/src', web + '/tests', assets, upstream, evidence])
      await mkdir(directory, { recursive: true });
    const script = path.join(web, 'scripts', suite + '-browser.mjs');
    await writeFile(script, await readFile(new URL('../scripts/' + suite + '-browser.mjs', import.meta.url)));
    const pin = { commit: 'fixture-pin', lock_sha256: hash('fixture-lock') };
    const sources = {};
    for (const name of ['build.mjs', 'compiler.mjs', 'upstream-pin.json', 'package.json', 'pnpm-lock.yaml', 'tsconfig.json']) {
      const value = name === 'upstream-pin.json' ? JSON.stringify(pin) : '{}';
      await writeFile(path.join(web, name), value);
      sources['crates/shenko-ui/web/' + name] = hash(value);
    }
    await writeFile(path.join(upstream, 'pnpm-lock.yaml'), 'fixture-lock');
    await writeFile(path.join(assets, 'build-manifest.json'), JSON.stringify({ upstream: pin, artifacts: {} }));
    const buildReceipt = path.join(root, 'normal-build.json');
    await writeFile(buildReceipt, JSON.stringify({ passed: true, sourceStable: true,
      sourceAfter: sources, upstreamCandidate: pin.commit, appBase: 'fixture-app' }));
    const playwright = path.join(root, 'playwright.mjs');
    await writeFile(playwright, fakeBrowser);
    const reportPath = path.join(evidence, 'report.json');
    await writeFile(reportPath, JSON.stringify({ runId: 'previous-run', passed: true }));
    if (fault === 'preflight') await writeFile(path.join(assets, 'build-manifest.json'), 'not-json');
    const child = spawn(process.execPath, [script], { env: { ...process.env,
      PROTO_UI_SOURCE: upstream, PUI_IM_BUILD_RECEIPT: buildReceipt,
      PUI_CHROME_EXECUTABLE: '/unused-test-double', PUI_PLAYWRIGHT_MODULE: playwright,
      PUI_EVIDENCE_DIR: evidence, PUI_OBSERVER_RUN_ID: 'current-' + suite, OBSERVER_FAULT: fault,
    }, stdio: ['ignore', 'pipe', 'pipe'] });
    let output = '';
    child.stdout.on('data', (chunk) => { output += chunk; });
    child.stderr.on('data', (chunk) => { output += chunk; });
    const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
    let code;
    try { code = await new Promise((resolve, reject) => { child.once('error', reject); child.once('exit', resolve); }); }
    finally { clearTimeout(timer); }
    return { code, report: JSON.parse(await readFile(reportPath, 'utf8')), output };
  } finally { await rm(root, { recursive: true, force: true }); }
}

test('successful IM and Files observers replace a prior report with the current run', async () => {
  for (const suite of ['im', 'files']) {
    const { code, report, output } = await observe(suite);
    assert.equal(code, 0, output);
    assert.equal(report.passed, true);
    assert.equal(report.runId, 'current-' + suite);
    assert.ok(report.finishedAt);
  }
});

test('IM context cleanup failure makes the final receipt and process fail', async () => {
  const { code, report, output } = await observe('im', 'context');
  assert.equal(code, 1, output);
  assert.equal(report.passed, false);
  assert.equal(report.cases[1].status, 'failed');
  assert.equal(report.cleanupErrors.length, 1);
  assert.ok(report.finishedAt);
});

test('IM outer failure is retained even when earlier case and cleanup succeeded', async () => {
  const { code, report, output } = await observe('im', 'outer');
  assert.equal(code, 1, output);
  assert.equal(report.passed, false);
  assert.match(report.executionErrors[0], /injected outer failure/);
});

test('Files browser close failure still closes the server and replaces the stale success', async () => {
  const { code, report, output } = await observe('files', 'browser');
  assert.equal(code, 1, output);
  assert.equal(report.runId, 'current-files');
  assert.equal(report.passed, false);
  assert.match(report.cleanupErrors[0], /injected browser close/);
  assert.ok(report.finishedAt);
});

test('Files page errors arriving during cleanup cannot retain success', async () => {
  const { code, report, output } = await observe('files', 'late-pageerror');
  assert.equal(code, 1, output);
  assert.equal(report.passed, false);
  assert.match(report.pageErrors[0], /injected late page error/);
});

test('Files preflight failure cannot leave the previous successful report', async () => {
  const { code, report, output } = await observe('files', 'preflight');
  assert.equal(code, 1, output);
  assert.equal(report.runId, 'current-files');
  assert.equal(report.passed, false);
});
