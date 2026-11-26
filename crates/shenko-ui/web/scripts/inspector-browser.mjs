/**
 * Actual Shenko Inspector consumer; run the normal public-package build first.
 * Required: PROTO_UI_SOURCE, PUI_INSPECTOR_BUILD_RECEIPT, PUI_CHROME_EXECUTABLE.
 * Optional: PUI_PLAYWRIGHT_MODULE, PUI_EVIDENCE_DIR.
 * Serves the existing App with its explicit demo data, without replacing a
 * renderer/lens or claiming a live backend/model, touch hardware or AT.
 */
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
const evidence = path.resolve(process.env.PUI_EVIDENCE_DIR ?? path.join(web, 'generated/inspector-native'));
const upstream = path.resolve(required('PROTO_UI_SOURCE'));
const buildReceiptPath = path.resolve(required('PUI_INSPECTOR_BUILD_RECEIPT'));
const executablePath = required('PUI_CHROME_EXECUTABLE');
const hash = (data) => createHash('sha256').update(data).digest('hex');
const json = async (file) => JSON.parse(await readFile(file, 'utf8'));
function required(name) { assert.ok(process.env[name], name + ' is required'); return process.env[name]; }
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
  for (const file of files.sort())
    result[path.relative(application, file).split(path.sep).join('/')] = hash(await readFile(file));
  return result;
}
await mkdir(evidence, { recursive: true });
const buildReceipt = await json(buildReceiptPath);
const manifest = await json(path.join(assets, 'build-manifest.json'));
const generation = await json(path.join(web, 'generated/prototypes/manifest.json'));
const pin = await json(path.join(web, 'upstream-pin.json'));
const sourceBefore = await snapshot();
assert.equal(buildReceipt.passed, true, 'normal build must actually have completed');
assert.equal(buildReceipt.sourceStable, true);
assert.deepEqual(sourceBefore, buildReceipt.sourceAfter, 'observe only the exact built App source');
assert.equal(pin.commit, buildReceipt.upstreamCandidate);
assert.equal(pin.commit, manifest.upstream.commit);
assert.equal(pin.lock_sha256, hash(await readFile(path.join(upstream, 'pnpm-lock.yaml'))));
assert.equal(manifest.realisation.adapter.some((item) => item.family === 'resizable'), true);
assert.deepEqual(manifest.realisation.compiled, ['transport-badge', 'message-bubble']);
assert.equal(generation.runtime_imported, false);
assert.equal(generation.source_tree_sha256, manifest.compiler.source_tree_sha256);
for (const [name, artifact] of Object.entries(manifest.artifacts))
  assert.equal(hash(await readFile(path.join(assets, name))), artifact.sha256, name);
const manifestHash = hash(await readFile(path.join(assets, 'build-manifest.json')));
const html = path.join(application, 'crates/shenko-ui/src/index.html');
const htmlHash = hash(await readFile(html));
const report = {
  kind: 'shenko-actual-inspector-consumer-v1', runId: process.env.PUI_OBSERVER_RUN_ID ?? randomUUID(),
  startedAt: new Date().toISOString(),
  appBase: buildReceipt.appBase, upstreamCandidate: pin.commit,
  sourceHash: hash(JSON.stringify(sourceBefore)), sourceBefore,
  buildReceiptSha256: hash(await readFile(buildReceiptPath)),
  buildManifestSha256: manifestHash, htmlSha256: htmlHash,
  observerSha256: hash(await readFile(fileURLToPath(import.meta.url))),
  scope: 'Actual App, existing Playground lens and demo data; no replacement lens, live model, adopted work backend, physical touch hardware, AT or full Resizable transition-table acceptance.',
  cancellation: 'C-RESIZABLE-GROUP-0001: already accepted controlled sizes remain authoritative; cancellation stops the contact and emits no commit. No extra rollback policy.',
  cases: [], pageErrors: [], webSockets: [], cleanupErrors: [],
};
const playwright = process.env.PUI_PLAYWRIGHT_MODULE
  ? await import(pathToFileURL(process.env.PUI_PLAYWRIGHT_MODULE).href)
  : createRequire(path.join(upstream, 'apps/www/package.json'))('playwright-core');
const launchOptions = { executablePath, headless: true, chromiumSandbox: true };
report.launchOptions = launchOptions;
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
const near = (actual, expected, label, tolerance = 0.12) =>
  assert.ok(Number.isFinite(actual) && Math.abs(actual - expected) <= tolerance,
    label + ': ' + actual + ' != ' + expected);
const selector = '.inspector-resize-handle';
const settle = (page) => page.evaluate(() =>
  new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
async function geometry(page) {
  return page.evaluate(() => {
    const rect = (selector) => {
      const element = document.querySelector(selector);
      const box = element.getBoundingClientRect();
      return { x: box.x, y: box.y, width: box.width, height: box.height,
        display: getComputedStyle(element).display };
    };
    const handle = document.querySelector('.inspector-resize-handle');
    const style = getComputedStyle(handle);
    return {
      viewport: innerWidth, layout: document.querySelector('.body').dataset.inspectorLayout,
      state: document.querySelector('.body').dataset.inspectorState,
      group: rect('.inspector-resize-group'), stream: rect('.inspector-stream-pane'),
      main: rect('.stream'), lens: rect('.inspector-lens-pane'), handle: rect('.inspector-resize-handle'),
      role: handle.getAttribute('role'), orientation: handle.getAttribute('aria-orientation'),
      value: Number(handle.getAttribute('aria-valuenow')), min: Number(handle.getAttribute('aria-valuemin')),
      max: Number(handle.getAttribute('aria-valuemax')), active: document.activeElement?.className,
      stored: localStorage.getItem('shenko.inspector.width.v1'),
      focusVisible: handle.hasAttribute('data-focus-visible'),
      outlineWidth: style.outlineWidth, outlineStyle: style.outlineStyle, boxShadow: style.boxShadow,
      horizontalOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
    };
  });
}
async function expectWidth(page, width, label) {
  await page.waitForFunction((expected) =>
    Math.abs(document.querySelector('.inspector-lens-pane').getBoundingClientRect().width - expected) <= .12,
  width, { timeout: 1000 });
  const state = await geometry(page);
  near(state.lens.width, width, label);
  assert.ok(state.stream.width >= 638 - .12, 'actual stream remains at least 638px');
  near(state.group.width, state.viewport, 'group fills the actual shell');
  near(state.handle.width, 1, 'public rhea separator is the original one physical CSS pixel');
  near(state.stream.width + state.lens.width + state.handle.width, state.group.width,
    'normalization excludes exactly the fixed separator');
  return state;
}
async function active(page, selector) {
  await page.waitForFunction((selector) => document.activeElement === document.querySelector(selector),
    selector, { timeout: 1000 });
}
async function open(page, label = 'Playground') {
  await page.locator('.lensbar .lens-item').filter({ hasText: label }).click();
  await page.locator('.body[data-inspector-state="open"]').waitFor();
  await settle(page);
}
async function drag(page, delta, release = true) {
  const box = await page.locator(selector).boundingBox();
  assert.ok(box && box.height > 0);
  const point = { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  await page.mouse.move(point.x, point.y);
  await page.mouse.down();
  await page.mouse.move(point.x + delta, point.y, { steps: 5 });
  if (release) await page.mouse.up();
  await settle(page);
  return point;
}
let browser;
let url;
async function runCase(name, action, options = {}) {
  const item = { name, status: 'running', startedAt: new Date().toISOString(), samples: [], inputDocuments: [] };
  report.cases.push(item);
  const context = await browser.newContext({
    viewport: { width: 1600, height: 900 }, locale: 'zh-CN', reducedMotion: 'no-preference', ...options,
  });
  const page = await context.newPage();
  page.setDefaultTimeout(15000);
  page.on('pageerror', (error) => report.pageErrors.push({ name, error: error.message }));
  page.on('websocket', (socket) => report.webSockets.push(socket.url()));
  try {
    await page.addInitScript(() => {
      window.__inspectorInputs = [];
      for (const type of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel', 'keydown'])
        document.addEventListener(type, (event) => {
          if (type === 'pointermove' && !event.buttons) return;
          window.__inspectorInputs.push({ type, trusted: event.isTrusted, key: event.key ?? null,
            pointerType: event.pointerType ?? null, target: event.target?.className ?? null });
        }, { capture: true, passive: true });
    });
    await page.goto(url, { waitUntil: 'networkidle' });
    await page.locator('.shell[data-source="demo"]').waitFor();
    await page.locator('[data-pui-style~="shenko-status-chip"]').waitFor();
    const started = performance.now();
    await action(page, item);
    item.durationMs = performance.now() - started;
    item.inputs = [...item.inputDocuments.flat(), ...await page.evaluate(() => window.__inspectorInputs ?? [])];
    assert.ok(item.inputs.some((event) => event.trusted), 'journey used actual browser input');
    assert.ok(item.inputs.every((event) => event.trusted), 'no synthetic input event was dispatched');
    await page.screenshot({ path: path.join(evidence, name + '.png') });
    item.status = 'passed';
  } catch (error) {
    item.status = 'failed'; item.error = String(error.stack ?? error);
    await page.screenshot({ path: path.join(evidence, name + '-failure.png') }).catch(() => {});
    item.failureGeometry = await geometry(page).catch((error) => ({ error: String(error) }));
    item.inputs = [...item.inputDocuments.flat(), ...await page.evaluate(() => window.__inspectorInputs ?? []).catch(() => [])];
  } finally {
    item.finishedAt = new Date().toISOString();
    await context.close();
    await writeFile(path.join(evidence, 'progress.json'), JSON.stringify(report, null, 2) + '\n');
  }
}
try {
  await new Promise((resolve, reject) => {
    server.once('error', reject); server.listen(0, '127.0.0.1', resolve);
  });
  url = 'http://127.0.0.1:' + server.address().port + '/?demo=1&theme=light';
  browser = await playwright.chromium.launch(launchOptions);
  report.browserVersion = browser.version();
  const browserCDP = await browser.newBrowserCDPSession();
  report.actualBrowserCommandLine = (await browserCDP.send('Browser.getBrowserCommandLine')).arguments;
  await browserCDP.detach();

  await runCase('desktop-public-pointer-keyboard-and-pixel-bounds', async (page, item) => {
    await open(page);
    await page.locator('.lens-body').getByText('启动时序对照表', { exact: true }).waitFor();
    const initial = await expectWidth(page, 672, 'the existing 42vw initial preference');
    item.samples.push({ step: 'initial', ...initial });
    assert.equal(initial.role, 'separator');
    assert.equal(initial.orientation, 'vertical');
    near(initial.value, 927 * 100 / 1599, 'public ARIA primary-panel value');
    const main = await page.locator('.stream').elementHandle();
    const editor = await page.locator('.composer-input').first().elementHandle();
    assert.ok(editor);
    await drag(page, 60);
    item.samples.push({ step: 'pointer', ...await expectWidth(page, 612, '60px native pointer movement') });
    near(Number(await page.evaluate(() => localStorage.getItem('shenko.inspector.width.v1'))), 612,
      'only terminal pointer action stores expanded width');
    await page.locator(selector).focus();
    await page.keyboard.press('ArrowLeft');
    item.samples.push({ step: 'arrow', ...await expectWidth(page, 627.99, 'one percent public keyboard step') });
    await page.keyboard.press('Shift+ArrowRight');
    item.samples.push({ step: 'large-arrow', ...await expectWidth(page, 468.09, 'ten percent public acceleration') });
    await page.keyboard.press('Home');
    item.samples.push({ step: 'maximum', ...await expectWidth(page, 680, 'public maximum collision') });
    await page.keyboard.press('ArrowLeft');
    await expectWidth(page, 680, 'arrow beyond maximum stays bounded');
    await drag(page, 280);
    item.samples.push({ step: 'minimum', ...await expectWidth(page, 400, 'expanded minimum collision') });
    assert.equal(await main.evaluate((element) => element === document.querySelector('.stream')), true);
    assert.equal(await editor.evaluate((element) => element === document.querySelector('.composer-input')), true);
    item.editorReadOnly = await editor.evaluate((element) => element.readOnly);
  });

  await runCase('collapse-restore-focus-and-width-preference', async (page, item) => {
    const opener = await page.locator('.lensbar .lens-item').filter({ hasText: 'Playground' }).elementHandle();
    await open(page);
    await drag(page, 72);
    await expectWidth(page, 600, 'chosen expanded width');
    await page.locator('.inspector-collapse').click();
    await page.locator('.body[data-inspector-state="collapsed"]').waitFor();
    await active(page, selector);
    const collapsed = await geometry(page);
    item.samples.push({ step: 'button-collapse', ...collapsed });
    near(collapsed.lens.width, 0, 'collapsed panel leaves no phantom width');
    near(Number(collapsed.stored), 600, 'collapse does not overwrite expanded preference');
    await page.keyboard.press('Tab');
    await active(page, '.inspector-restore');
    await page.keyboard.press('Enter');
    await active(page, selector);
    await expectWidth(page, 600, 'native keyboard activation restores expanded width');
    await page.keyboard.press('End');
    await page.locator('.body[data-inspector-state="collapsed"]').waitFor();
    await active(page, selector);
    await page.keyboard.press('Home');
    await expectWidth(page, 680, 'public Home expands to the feasible boundary');
    await page.locator('.inspector-close').click();
    await page.locator('.body[data-inspector-state="closed"]').waitFor();
    assert.equal(await opener.evaluate((element) => document.activeElement === element), true,
      'closing returns focus to the actual visible opener');
    await open(page);
    await expectWidth(page, 680, 'reopening preserves the expanded preference');
    item.inputDocuments.push(await page.evaluate(() => window.__inspectorInputs));
    await page.reload({ waitUntil: 'networkidle' });
    await open(page);
    item.samples.push({ step: 'reload', ...await expectWidth(page, 680, 'browser preference survives reload') });
  });

  await runCase('responsive-reflow-cancels-contact-and-keeps-real-main', async (page, item) => {
    await open(page);
    await drag(page, 72);
    await expectWidth(page, 600, 'committed starting width');
    const main = await page.locator('.stream').elementHandle();
    const editor = await page.locator('.composer-input').first().elementHandle();
    const start = await geometry(page);
    item.samples.push({ step: 'before-contact', ...start });
    const contact = await drag(page, 40, false);
    const accepted = await expectWidth(page, 560, 'controlled owner accepted the current sample');
    item.samples.push({ step: 'accepted', ...accepted });
    await page.setViewportSize({ width: 1100, height: 900 });
    const resized = await expectWidth(page, 461, 'breakpoint pixel bound includes the separator');
    item.samples.push({ step: 'resized', ...resized });
    await page.mouse.move(contact.x - 200, contact.y, { steps: 4 });
    await page.mouse.up(); await settle(page);
    const retired = await expectWidth(page, 461, 'old contact cannot resize the new extent');
    item.samples.push({ step: 'late-input', ...retired });
    assert.equal(retired.stored, start.stored, 'geometry cancellation emitted no stored commit');
    await page.locator(selector).focus();
    await page.setViewportSize({ width: 1099, height: 900 });
    await active(page, '.inspector-close');
    await page.waitForFunction(() => document.querySelector('.body').dataset.inspectorLayout === 'overlay',
      undefined, { timeout: 1000 });
    await settle(page);
    const overlay = await geometry(page);
    item.samples.push({ step: 'overlay', ...overlay });
    near(overlay.lens.width, 1099, 'absolute lens remains visible with public sizes [100,0]');
    near(overlay.main.width, 1099, 'the original main remains underneath the overlay');
    near(overlay.handle.width, 0, 'narrow layout has no resize input target');
    assert.equal(await main.evaluate((element) => element === document.querySelector('.stream')), true);
    assert.equal(await editor.evaluate((element) => element === document.querySelector('.composer-input')), true);
    await page.setViewportSize({ width: 390, height: 844 });
    await settle(page);
    const phone = await geometry(page);
    item.samples.push({ step: 'phone', ...phone });
    near(phone.lens.width, 390, 'existing phone lens overlay fills the shell');
    assert.ok(phone.horizontalOverflow <= 1, 'phone overlay does not create horizontal overflow');
    await page.locator('.inspector-close').click();
    await page.locator('.body[data-inspector-state="closed"]').waitFor();
    assert.equal(await page.evaluate(() => {
      const active = document.activeElement;
      return !!active && active !== document.body && active.getBoundingClientRect().width > 0
        && active.getBoundingClientRect().height > 0;
    }), true, 'phone close returns focus to a visible shell destination');
  });

  await runCase('replaced-handle-rejects-late-contact-and-visible-focus', async (page, item) => {
    await open(page);
    await drag(page, 72);
    await expectWidth(page, 600, 'committed starting width');
    const before = await geometry(page);
    const touchBox = await page.locator(selector).boundingBox();
    assert.ok(touchBox);
    const touch = { x: touchBox.x + touchBox.width / 2, y: touchBox.y + touchBox.height / 2,
      id: 7, radiusX: 1, radiusY: 1, force: 1 };
    const cdp = await page.context().newCDPSession(page);
    try {
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchStart', touchPoints: [touch] });
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchMove', touchPoints: [{ ...touch, x: touch.x + 40 }] });
      await expectWidth(page, 560, 'native emulated touch sample enters the same public owner');
      await cdp.send('Input.dispatchTouchEvent', { type: 'touchCancel', touchPoints: [] });
    } finally { await cdp.detach(); }
    await settle(page);
    const cancelled = await expectWidth(page, 560, 'pointercancel retains the accepted canonical value');
    item.samples.push({ step: 'before-contact', ...before }, { step: 'pointercancel', ...cancelled });
    assert.equal(cancelled.stored, before.stored, 'pointercancel emits no stored commit');
    assert.equal(await page.evaluate(() => window.__inspectorInputs.some((event) =>
      event.type === 'pointercancel' && event.trusted)), true, 'browser delivered a real pointercancel');
    await drag(page, -40);
    const freshCommit = await expectWidth(page, 600, 'fresh contact works after pointercancel');
    assert.notEqual(freshCommit.stored, null);
    assert.ok(Math.abs(Number(freshCommit.stored) - 600) < 1e-7,
      'the new legitimate commit independently stores the expected 600px width');
    item.samples.push({ step: 'fresh-commit-after-cancel', ...freshCommit });
    const handle = await page.locator(selector).elementHandle();
    const main = await page.locator('.stream').elementHandle();
    const point = await drag(page, 40, false);
    const accepted = await expectWidth(page, 560, 'accepted sample before target retirement');
    await page.keyboard.press('Control+0');
    await page.locator('.body[data-inspector-state="closed"]').waitFor();
    await page.keyboard.press('Control+3');
    await page.locator('.body[data-inspector-state="open"]').waitFor();
    await expectWidth(page, 560, 'same-extent reopening restores the current authoritative width');
    assert.equal(await handle.evaluate((element) => element.isConnected), false, 'old Handle really retired');
    await page.mouse.move(point.x - 180, point.y, { steps: 4 });
    await page.mouse.up(); await settle(page);
    const after = await expectWidth(page, 560, 'late old mouse samples never enter the new owner lease');
    assert.equal(after.stored, freshCommit.stored,
      'retired contact preserves the latest legitimate stored commit byte-for-byte');
    assert.equal(await main.evaluate((element) => element === document.querySelector('.stream')), true);
    await page.locator(selector).focus();
    await page.keyboard.press('ArrowRight');
    const focused = await expectWidth(page, 544.01, 'fresh keyboard interaction still works');
    assert.equal(focused.focusVisible, true, 'public focus-visible state is present');
    assert.ok((parseFloat(focused.outlineWidth) > 0 && focused.outlineStyle !== 'none')
      || focused.boxShadow !== 'none', 'native computed focus treatment remains visible');
    item.samples.push({ step: 'accepted', ...accepted },
      { step: 'retired', ...after }, { step: 'forced-colors-focus', ...focused });
    // The initial observer incorrectly required Escape to cancel. Neither the
    // current Resizable nor Move contract defines that key. Keep native01's
    // failed receipt and observe this behavior without inventing a contract.
    const escapeBefore = await geometry(page);
    const escapePoint = await drag(page, 40, false);
    const escapeAccepted = await geometry(page);
    await page.keyboard.press('Escape');
    await page.mouse.move(escapePoint.x - 160, escapePoint.y, { steps: 4 });
    await page.mouse.up(); await settle(page);
    item.escapeDiagnostic = {
      scope: 'Unspecified by C-RESIZABLE-GROUP-0001 and C-MOVE-GESTURE-0001; no cancellation acceptance claim.',
      before: escapeBefore, accepted: escapeAccepted, after: await geometry(page),
    };
  }, { forcedColors: 'active', reducedMotion: 'reduce', hasTouch: true });

  assert.deepEqual(report.pageErrors, []);
  assert.deepEqual(report.webSockets, [], 'explicit demo data must not open the live controller');
  assert.deepEqual(await snapshot(), sourceBefore, 'App source changed during native observation');
  assert.equal(hash(await readFile(path.join(assets, 'build-manifest.json'))), manifestHash);
  assert.equal(hash(await readFile(html)), htmlHash);
  for (const [name, artifact] of Object.entries(manifest.artifacts))
    assert.equal(hash(await readFile(path.join(assets, name))), artifact.sha256, name);
  report.sourceStable = true;
} catch (error) {
  report.failure = String(error.stack ?? error);
} finally {
  try { await browser?.close(); } catch (error) { report.cleanupErrors.push(String(error)); }
  try {
    if (server.listening) await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  } catch (error) { report.cleanupErrors.push(String(error)); }
  report.finishedAt = new Date().toISOString();
  report.passed = !report.failure && report.sourceStable === true && report.pageErrors.length === 0
    && report.webSockets.length === 0 && report.cleanupErrors.length === 0
    && report.cases.length === 4 && report.cases.every((item) => item.status === 'passed');
  await writeFile(path.join(evidence, 'receipt.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify({ passed: report.passed, cases: report.cases.map(({ name, status, error }) =>
    ({ name, status, error })), failure: report.failure }, null, 2));
  if (!report.passed) process.exitCode = 1;
}
