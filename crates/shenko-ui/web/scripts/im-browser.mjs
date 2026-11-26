/** Actual App regression journeys for pending-decision navigation and IM
 * filters. Uses the existing demo only; no replacement renderer or hooks.
 * Required: PROTO_UI_SOURCE, PUI_IM_BUILD_RECEIPT, PUI_CHROME_EXECUTABLE.
 * Optional: PUI_PLAYWRIGHT_MODULE, PUI_EVIDENCE_DIR. */
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { readFile, readdir, mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const web = fileURLToPath(new URL('../', import.meta.url));
const application = path.resolve(web, '../../..');
const assets = path.join(application, 'crates/shenko-ui/src/static');
const html = path.join(application, 'crates/shenko-ui/src/index.html');
const evidence = path.resolve(process.env.PUI_EVIDENCE_DIR ?? path.join(web, 'generated/im-native'));
const report = {
  kind: 'shenko-im-navigation-review-v1', runId: process.env.PUI_OBSERVER_RUN_ID ?? randomUUID(),
  startedAt: new Date().toISOString(), passed: false,
  scope: 'Two journeys against the actual App and existing demo: repeated pending-decision reveal across phone/desktop; retained query, scope and archive controls across desktop/tablet. No live backend, model, physical touch or assistive-technology acceptance.',
  cases: [], pageErrors: [], cleanupErrors: [], executionErrors: [],
};
await mkdir(evidence, { recursive: true });
// Invalidate an older success before preflight, imports or browser startup can fail.
await writeFile(path.join(evidence, 'report.json'), JSON.stringify(report, null, 2) + '\n');
function required(name) { assert.ok(process.env[name], name + ' is required'); return process.env[name]; }
const upstream = path.resolve(required('PROTO_UI_SOURCE'));
const receiptPath = path.resolve(required('PUI_IM_BUILD_RECEIPT'));
const executablePath = required('PUI_CHROME_EXECUTABLE');
const hash = (data) => createHash('sha256').update(data).digest('hex');
const json = async (file) => JSON.parse(await readFile(file, 'utf8'));
async function snapshot() {
  const files = [];
  async function visit(directory) {
    for (const entry of await readdir(directory, { withFileTypes: true })) {
      const file = path.join(directory, entry.name);
      if (entry.isDirectory()) await visit(file);
      else if (entry.isFile()) files.push(file);
    }
  }
  await visit(path.join(web, 'src'));
  await visit(path.join(web, 'tests'));
  for (const name of ['build.mjs', 'compiler.mjs', 'upstream-pin.json', 'package.json', 'pnpm-lock.yaml', 'tsconfig.json'])
    files.push(path.join(web, name));
  const result = {};
  for (const file of files.sort()) result[path.relative(application, file).split(path.sep).join('/')] = hash(await readFile(file));
  return result;
}
await mkdir(evidence, { recursive: true });
const receipt = await json(receiptPath);
const manifest = await json(path.join(assets, 'build-manifest.json'));
const pin = await json(path.join(web, 'upstream-pin.json'));
const sourceBefore = await snapshot();
assert.equal(receipt.passed, true);
assert.equal(receipt.sourceStable, true);
assert.deepEqual(sourceBefore, receipt.sourceAfter, 'observe the exact built App source');
assert.equal(pin.commit, receipt.upstreamCandidate);
assert.equal(pin.commit, manifest.upstream.commit);
assert.equal(pin.lock_sha256, hash(await readFile(path.join(upstream, 'pnpm-lock.yaml'))));
for (const [name, artifact] of Object.entries(manifest.artifacts))
  assert.equal(hash(await readFile(path.join(assets, name))), artifact.sha256, name);
Object.assign(report, {
  appBase: receipt.appBase, upstreamCandidate: pin.commit, sourceBefore,
  buildReceiptSha256: hash(await readFile(receiptPath)),
  buildManifestSha256: hash(await readFile(path.join(assets, 'build-manifest.json'))),
  observerSha256: hash(await readFile(fileURLToPath(import.meta.url))),
});
const playwright = process.env.PUI_PLAYWRIGHT_MODULE
  ? await import(pathToFileURL(process.env.PUI_PLAYWRIGHT_MODULE).href)
  : createRequire(path.join(upstream, 'apps/www/package.json'))('playwright-core');
const server = createServer(async (request, response) => {
  try {
    const pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    let file;
    if (pathname === '/') file = html;
    else if (pathname.startsWith('/assets/')) {
      file = path.resolve(assets, pathname.slice(8));
      const relative = path.relative(assets, file);
      assert.ok(!relative.startsWith('..') && !path.isAbsolute(relative));
    } else { response.writeHead(404).end(); return; }
    const mime = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8',
      '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml' };
    response.writeHead(200, { 'Content-Type': mime[path.extname(file)] ?? 'application/octet-stream' });
    response.end(await readFile(file));
  } catch { if (!response.headersSent) response.writeHead(404); response.end(); }
});
let browser;
let baseURL;
async function runCase(name, viewport, query, action) {
  const item = { name, status: 'running', startedAt: new Date().toISOString(), samples: [] };
  report.cases.push(item);
  const context = await browser.newContext({ viewport, locale: 'en-US', reducedMotion: 'reduce' });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  page.on('pageerror', (error) => report.pageErrors.push({ name, message: error.message }));
  try {
    await page.addInitScript(() => {
      window.__imInputs = [];
      for (const type of ['pointerdown', 'keydown', 'wheel']) document.addEventListener(type, (event) => {
        window.__imInputs.push({ type, trusted: event.isTrusted, key: event.key ?? null });
      }, { capture: true, passive: true });
    });
    await page.goto(baseURL + '/?demo=1&theme=light' + query, { waitUntil: 'networkidle' });
    await page.locator('.shell[data-source="demo"]').waitFor();
    await action(page, item);
    item.inputs = await page.evaluate(() => window.__imInputs);
    assert.ok(item.inputs.length > 0 && item.inputs.every((input) => input.trusted));
    await page.screenshot({ path: path.join(evidence, name + '.png') });
    item.status = 'passed';
  } catch (error) {
    item.status = 'failed'; item.error = String(error.stack ?? error);
    await page.screenshot({ path: path.join(evidence, name + '-failure.png') }).catch(() => {});
  } finally {
    try { await context.close(); }
    catch (error) {
      item.status = 'failed';
      report.cleanupErrors.push({ resource: name + '/context', error: String(error.stack ?? error) });
    }
    item.finishedAt = new Date().toISOString();
    await writeFile(path.join(evidence, 'progress.json'), JSON.stringify(report, null, 2) + '\n');
  }
}
async function revealedDecision(page, item) {
  await page.waitForFunction(() => {
    const decision = document.querySelector('.im-thread [data-document-id="demo-document-1"]');
    const target = document.activeElement;
    const frame = document.querySelector('.im-scroll-frame');
    if (!decision?.contains(target) || !frame || !target?.getClientRects().length) return false;
    const visible = frame.getBoundingClientRect();
    const bounds = target.getBoundingClientRect();
    return document.querySelector('.im')?.dataset.pane === 'thread'
      && bounds.top >= visible.top - 1 && bounds.bottom <= visible.bottom + 1;
  });
  item.samples.push(await page.evaluate(() => ({
    viewport: innerWidth, pane: document.querySelector('.im').dataset.pane,
    focusedDocument: document.activeElement.closest('[data-document-id]')?.dataset.documentId,
    focusedLabel: document.activeElement.textContent,
    lens: document.querySelector('.shell').dataset.lens,
  })));
}
try {
  await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
  baseURL = 'http://127.0.0.1:' + server.address().port;
  browser = await playwright.chromium.launch({ executablePath, headless: true, chromiumSandbox: true });
  report.browserVersion = browser.version();
  await runCase('phone-review-repeat-and-desktop-reveal', { width: 390, height: 760 }, '&tab=inspect&view=tasks', async (page, item) => {
    await page.locator('.stream[data-tab="inspect"]').waitFor();
    await page.locator('.inspector-attention-cta').click();
    await revealedDecision(page, item);
    await page.locator('.im-back').click();
    await page.locator('.tabbar-item').last().click();
    await page.locator('.more-item').first().click();
    await page.locator('.inspector-attention-cta').click();
    await revealedDecision(page, item);
    await page.setViewportSize({ width: 1280, height: 600 });
    await page.locator('.im-scroll-frame').hover();
    await page.mouse.wheel(0, -4000);
    await page.locator('.lensbar .lens-item').first().click();
    await page.locator('.inspector-attention-cta').click();
    await revealedDecision(page, item);
  });
  await runCase('tablet-filter-and-archive-controls', { width: 1280, height: 800 }, '&tab=im', async (page, item) => {
    const search = page.locator('.im-search');
    await search.fill('unmatched-pr411-query');
    await page.locator('.im-no-matches').waitFor();
    await page.setViewportSize({ width: 900, height: 800 });
    assert.equal(await search.isVisible(), true, 'retained query must have a reachable clear control');
    await search.press('Escape');
    await page.locator('.im-scope-bar .scope-id-agents').click();
    const archive = page.locator('.im-archive-toggle');
    assert.equal(await archive.isVisible(), true, 'tablet layout must expose the archive control');
    assert.equal(await archive.getAttribute('aria-expanded'), 'false');
    await archive.click();
    await page.locator('.im-row-title').filter({ hasText: '飞书回调排查' }).waitFor();
    assert.equal(await archive.getAttribute('aria-expanded'), 'true');
    await archive.click();
    await page.waitForFunction(() => document.querySelector('.im-archive-toggle')?.getAttribute('aria-expanded') === 'false');
    assert.equal(await page.locator('.im-row-title').filter({ hasText: '飞书回调排查' }).count(), 0);
    item.samples.push(await page.evaluate(() => ({
      viewport: innerWidth, query: document.querySelector('.im-search').value,
      scope: document.querySelector('.im-scope-bar [aria-selected="true"]')?.textContent,
      archiveExpanded: document.querySelector('.im-archive-toggle').getAttribute('aria-expanded'),
      overflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    })));
    assert.ok(item.samples.at(-1).overflow <= 1, 'tablet sidebar must not overflow the page');
  });
} catch (error) {
  report.executionErrors.push(String(error.stack ?? error));
} finally {
  if (browser) await browser.close().catch((error) => report.cleanupErrors.push(String(error)));
  await new Promise((resolve) => server.close((error) => {
    if (error && error.code !== 'ERR_SERVER_NOT_RUNNING') report.cleanupErrors.push(String(error));
    resolve();
  }));
  try { report.sourceStable = JSON.stringify(sourceBefore) === JSON.stringify(await snapshot()); }
  catch (error) { report.sourceStable = false; report.executionErrors.push(String(error.stack ?? error)); }
  report.finishedAt = new Date().toISOString();
  report.passed = report.cases.length === 2 && report.cases.every((item) => item.status === 'passed')
    && report.pageErrors.length === 0 && report.cleanupErrors.length === 0
    && report.executionErrors.length === 0 && report.sourceStable;
  await writeFile(path.join(evidence, 'report.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify({ passed: report.passed, cases: report.cases.map(({ name, status, error }) => ({ name, status, error })),
    pageErrors: report.pageErrors, cleanupErrors: report.cleanupErrors, sourceStable: report.sourceStable, evidence }, null, 2));
}
assert.equal(report.passed, true, 'actual App IM journeys must pass');
