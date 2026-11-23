/** Real Shenko production bundle with deterministic HTTP/WS read fixtures.
 * No model, ledger, deployment or physical-device acceptance is claimed.
 * Run after the documented pinned frontend build. Browser input is trusted
 * Playwright wheel/keyboard input; geometry is only the independent oracle.
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = resolve(process.env.READING_EVIDENCE_DIR ?? '/tmp/shenko-reading-evidence');
const playwright = process.env.PLAYWRIGHT_MODULE;
const { chromium } = await import(playwright ? pathToFileURL(playwright).href : 'playwright');
const staticRoot = join(root, 'crates/shenko-ui/src/static');
const manifestBytes = await readFile(join(staticRoot, 'build-manifest.json'));
const manifest = JSON.parse(manifestBytes);
const receipt = { schema_version: 1, scope: 'real Shenko live frontend; fixture HTTP/WS reads',
  upstream: manifest.upstream, bundle_manifest_sha256: createHash('sha256').update(manifestBytes).digest('hex'),
  fixture: 'committed mixed Chinese/English Markdown with ordinary links, stable owner/reply IDs, replay duplicates',
  physical_input: false, model_or_backend_acceptance: false, runs: [] };
await mkdir(output, { recursive: true });

let parts = [];
let currentSubject = 'reading-fixture-a';
let identityRevision = 1;
let newest = 30;
const content = (n) => `阅读记录 ${n} · Reading record ${n}\n\n` +
  // The real Markdown renderer owns these links. They provide native focus
  // stops in IM as well as Inspect without adding controls to the application.
  `[Reading source ${n}](https://example.test/reading/${n})\n\n` +
  Array.from({ length: 5 }, (_, i) => `第 ${i + 1} 段：保留同一阅读对象与偏移。\nEnglish evidence ${n}-${i}: append, depart, resume, and retain the selected text. Emoji 🌿 é.`).join('\n\n');
const part = (n) => ({ schema_version: 2, type: 'timeline_part', role: 'subject', content: content(n),
  message_id: `reading-message-${n}`, part_id: 'text/0', part_revision: `revision-${n}`, chunk_seq: 0,
  committed_only: true, observed_at: '2026-11-22T00:00:00Z', step: n, step_id: `reading-step-${n}`,
  commit_ref: `fixture/${n}`, commit_seq: n * 2, terminal: true });
const submission = (n) => ({ payload: { schema_version: 2, type: 'submit', mode: 'append',
  subject_id: currentSubject, submission_id: `reading-input-${n}`, message_id: `owner-message-${n}`,
  content: `Owner question ${n} · 用户记录`, attachment_refs: [] },
  created_at: '2026-11-22T00:00:00Z', accepted_seq: n * 2 - 1,
  view: { schema_version: 2, result: 'found', submission_id: `reading-input-${n}`, event_id: `event-${n}`,
    accepted_commit: `fixture/input/${n}`, stage: 'committed', selected_step_id: `reading-step-${n}`,
    committed_at: '2026-11-22T00:00:00Z' } });
const requests = [];
const server = createServer(async (request, response) => {
  const url = new URL(request.url, 'http://localhost');
  requests.push({ method: request.method, path: url.pathname });
  try {
    let body, mime = 'application/json';
    if (url.pathname === '/') { body = await readFile(join(root, 'crates/shenko-ui/src/index.html')); mime = 'text/html'; }
    else if (url.pathname.startsWith('/assets/') && !url.pathname.includes('..')) {
      body = await readFile(join(staticRoot, url.pathname.slice('/assets/'.length)));
      mime = url.pathname.endsWith('.css') ? 'text/css' : url.pathname.endsWith('.js') ? 'text/javascript' : 'application/json';
    } else if (url.pathname === '/api/self') body = JSON.stringify({ subject_id: currentSubject,
      identity: { agent_id: currentSubject, revision: identityRevision, display_name: 'Reading fixture', naming: 'explicit' }, step: newest });
    else if (url.pathname === '/api/submissions') body = JSON.stringify({ available: true, subject_id: currentSubject,
      items: Array.from({ length: newest }, (_, i) => submission(newest - i)), total: newest, has_older: false });
    else if (url.pathname === '/api/conversations') body = JSON.stringify({ available: true, messages: [] });
    else if (url.pathname === '/api/tasks') body = JSON.stringify({ available: true, tasks: [] });
    else if (url.pathname === '/api/playground') body = JSON.stringify({ available: true, artifacts: [] });
    else if (url.pathname === '/api/owner/state') body = JSON.stringify({ claimed: true });
    else if (url.pathname.startsWith('/api/')) body = JSON.stringify({ available: true, entries: [] });
    else { response.writeHead(404); response.end(); return; }
    response.writeHead(200, { 'content-type': mime, 'cache-control': 'no-store' }); response.end(body);
  } catch (error) { response.writeHead(500); response.end(String(error)); }
});
await new Promise((done) => server.listen(0, '127.0.0.1', done));
const origin = `http://127.0.0.1:${server.address().port}`;
const browser = await chromium.launch({ headless: true,
  ...(process.env.CHROMIUM_EXECUTABLE ? { executablePath: process.env.CHROMIUM_EXECUTABLE } : {}) });
receipt.browser = { engine: 'Chromium', version: browser.version(), headless: true };

async function measure(page, selector, rowSelector) {
  return page.locator(selector).evaluate((outer, rowSelector) => {
    const element = [outer, ...outer.querySelectorAll('*')].find((node) =>
      node instanceof HTMLElement && /auto|scroll/.test(getComputedStyle(node).overflowY)) ?? outer;
    const bounds = element.getBoundingClientRect();
    const rows = [...element.querySelectorAll(rowSelector)];
    const row = rows.find((node) => node.getBoundingClientRect().bottom > bounds.top + 8);
    return { top: element.scrollTop, extent: element.scrollHeight, viewport: element.clientHeight,
      endGap: element.scrollHeight - element.clientHeight - element.scrollTop, count: rows.length,
      key: row?.getAttribute('data-entry-key') ?? row?.getAttribute('data-message-key'),
      offset: row ? row.getBoundingClientRect().top - bounds.top : null,
      focused: document.activeElement?.className, reading: document.querySelector('.timeline-capture')?.getAttribute('data-reading') };
  }, rowSelector);
}
async function settledEnd(page, selector, rowSelector) {
  for (let attempts = 0; attempts < 50; attempts++) {
    const facts = await measure(page, selector, rowSelector);
    if (facts.extent > facts.viewport * 3 && Math.abs(facts.endGap) <= 2) return facts;
    await page.waitForTimeout(100);
  }
  throw new Error(`did not follow end: ${JSON.stringify(await measure(page, selector, rowSelector))}`);
}
async function capture(page, run, name, selector, rowSelector) {
  const facts = await measure(page, selector, rowSelector);
  run.observations.push({ name, ...facts });
  return facts;
}
async function journey(tab, width) {
  currentSubject = 'reading-fixture-a'; identityRevision = 1;
  newest = 30; parts = Array.from({ length: newest }, (_, i) => part(i + 1));
  const run = { tab, viewport: { width, height: 900 }, theme: 'light', reduced_motion: 'reduce', observations: [], checks: [], errors: [] };
  receipt.runs.push(run);
  const context = await browser.newContext({ viewport: run.viewport, locale: 'en-US', colorScheme: 'light', reducedMotion: 'reduce' });
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  const sockets = [];
  const outgoing = [];
  page.on('pageerror', (error) => run.errors.push(String(error)));
  await page.addInitScript(() => {
    const Base = window.ResizeObserver;
    window.__readingObservers = [];
    window.ResizeObserver = class extends Base {
      constructor(callback) {
        const entry = { callback, disconnected: false };
        super(callback); entry.observer = this;
        window.__readingObservers.push(entry);
        this.entry = entry;
      }
      disconnect() { this.entry.disconnected = true; super.disconnect(); }
    };
  });
  await page.routeWebSocket('**/ws', (socket) => {
    sockets.push(socket);
    socket.onMessage((raw) => {
      const message = JSON.parse(String(raw)); outgoing.push(message);
      if (message.type === 'client_hello') {
        socket.send(JSON.stringify({ schema_version: 2, type: 'hello', server_version: 'reading-fixture',
          capabilities: { append: true, durable_input_receipts: true, committed_replay: true, streaming: true } }));
        for (const entry of parts) socket.send(JSON.stringify(entry));
      }
    });
  });
  const selector = tab === 'inspect' ? '.timeline-viewport' : '.im-scroll';
  const rowSelector = tab === 'inspect' ? '[data-entry-key]' : '[data-message-key]';
  const check = (name, condition, details) => { run.checks.push({ name, passed: !!condition, details }); assert.ok(condition, `${name}: ${JSON.stringify(details)}`); };
  const append = async () => {
    const entry = part(++newest); parts.push(entry); sockets.at(-1).send(JSON.stringify(entry));
    await page.waitForTimeout(850); // Includes the App's committed-history refresh (600ms).
  };
  try {
    await page.goto(`${origin}/?tab=${tab}&thread=direct&theme=light&lang=en`, { waitUntil: 'networkidle' });
    await page.locator(selector).waitFor({ state: 'visible' });
    await settledEnd(page, selector, rowSelector);
    check('committed reply text reaches the actual conversation renderer',
      (await page.locator(selector).innerText()).includes('English evidence 30-4'), { tab });
    await capture(page, run, 'initial', selector, rowSelector);
    const editor = page.locator('.composer textarea').first();
    await editor.focus();
    await append();
    const tail = await settledEnd(page, selector, rowSelector);
    check('append follows the actual tail and retains composer focus', await editor.evaluate((node) => node === document.activeElement), tail);

    // Require real rendered message content that native sequential navigation
    // can reach. Plain paragraphs alone cannot exercise content-focus retention.
    const focusTargets = await page.locator(selector).evaluate((outer, rowSelector) => {
      return [...outer.querySelectorAll('button,a[href],summary,input,textarea,select,[tabindex]')]
        .filter((node) => node instanceof HTMLElement && node.tabIndex >= 0
          && !node.matches(':disabled') && !node.closest('[inert]')
          && node.getClientRects().length > 0 && getComputedStyle(node).visibility === 'visible'
          && node.closest(rowSelector))
        .map((node) => ({ tag: node.tagName, href: node.getAttribute('href'),
          key: node.closest(rowSelector).getAttribute('data-entry-key')
            ?? node.closest(rowSelector).getAttribute('data-message-key') }));
    }, rowSelector);
    run.fixture_focus_targets = focusTargets;
    check('fixture supplies sequential focus targets in at least three messages',
      new Set(focusTargets.map((target) => target.key)).size >= 3, { focusTargets });

    // Move from the composer with native sequential focus navigation. Never
    // assign a scroll offset or directly focus the historical message action.
    // Geometry observes the browser's movement; it does not produce it.
    let keyboardDeparture;
    let backwardTabs = 0;
    let previousHistoricalAction = false;
    for (; backwardTabs < 96; backwardTabs++) {
      await page.keyboard.press('Shift+Tab');
      await page.waitForTimeout(40);
      const inMessage = await page.locator(selector).evaluate((outer, rowSelector) => {
        const focused = document.activeElement;
        return focused instanceof HTMLElement && outer.contains(focused)
          && !!focused.closest(rowSelector) && focused.matches('button,a[href],summary,input,textarea,select,[tabindex]');
      }, rowSelector);
      const facts = await measure(page, selector, rowSelector);
      const historicalAction = inMessage && facts.endGap > 100;
      // Two consecutive historical focus stops make the forward Tab below
      // return to a known historical action rather than the newest row.
      if (historicalAction && previousHistoricalAction) { keyboardDeparture = facts; break; }
      previousHistoricalAction = historicalAction;
    }
    check('trusted Shift+Tab reaches a historical message action away from the tail', !!keyboardDeparture,
      { backwardTabs: backwardTabs + 1, before: tail, after: keyboardDeparture });
    for (const direction of ['Shift+Tab', 'Tab']) {
      if (direction === 'Tab') {
        await page.keyboard.press('Tab');
        await page.waitForTimeout(100);
      }
      const focusedBefore = await capture(page, run, `keyboard-${direction}-departed`, selector, rowSelector);
      const focusedAction = await page.locator(selector).evaluate((outer, rowSelector) => {
        const focused = document.activeElement;
        window.__readingKeyboardTarget = focused;
        return focused instanceof HTMLElement && outer.contains(focused)
          && !!focused.closest(rowSelector) && focused.matches('button,a[href],summary,input,textarea,select,[tabindex]');
      }, rowSelector);
      check(`trusted ${direction} retains focus within a historical message`, focusedAction && focusedBefore.endGap > 100,
        focusedBefore);
      await append();
      const focusedAfter = await capture(page, run, `keyboard-${direction}-append`, selector, rowSelector);
      check(`append preserves ${direction} focus and its reader anchor`,
        await page.evaluate(() => document.activeElement === window.__readingKeyboardTarget)
          && focusedBefore.key === focusedAfter.key && Math.abs(focusedBefore.offset - focusedAfter.offset) <= 2,
        { before: focusedBefore, after: focusedAfter });
    }
    const keyboardJump = page.locator('.jump-latest').getByRole('button').first();
    await keyboardJump.focus();
    await keyboardJump.press('Enter');
    await settledEnd(page, selector, rowSelector);
    await append();
    await settledEnd(page, selector, rowSelector);
    check('explicit resume after content focus follows appends and keeps the resume control focused',
      await keyboardJump.evaluate((node) => node === document.activeElement), await measure(page, selector, rowSelector));

    await page.locator(selector).hover();
    await page.mouse.wheel(0, -520);
    await page.waitForTimeout(200);
    const before = await capture(page, run, 'departed', selector, rowSelector);
    check('trusted wheel departs from the end', before.endGap > 100, before);
    await append();
    const paused = await capture(page, run, 'append-paused', selector, rowSelector);
    check('append preserves the same reader key and offset', paused.key === before.key && Math.abs(paused.offset - before.offset) <= 2, { before, paused });
    const count = paused.count;
    sockets.at(-1).send(JSON.stringify(parts.at(-1)));
    await page.waitForTimeout(100);
    check('replayed committed identity adds no duplicate row', (await measure(page, selector, rowSelector)).count === count, { count });
    if (tab === 'inspect') {
      await page.reload({ waitUntil: 'networkidle' });
      await page.locator(selector).waitFor({ state: 'visible' });
      await page.waitForTimeout(850);
      const restored = await capture(page, run, 'history-reopened', selector, rowSelector);
      check('reopening restores the saved reader key across replay materialization', restored.key === paused.key
        && Math.abs(restored.offset - paused.offset) <= 2 && restored.reading === 'reading_history', { paused, restored });
    }

    const jump = page.locator('.jump-latest').getByRole('button').first();
    await jump.focus();
    await jump.press('Enter');
    await settledEnd(page, selector, rowSelector);
    check('keyboard resume keeps focus on its surviving control', await jump.evaluate((node) => node === document.activeElement), await measure(page, selector, rowSelector));
    await append();
    await settledEnd(page, selector, rowSelector);
    check('resumed lease follows later appends', true);

    await page.locator(selector).hover();
    await page.mouse.wheel(0, -620);
    await page.waitForTimeout(150);
    await page.locator(selector).evaluate((element) => {
      const candidates = [...element.querySelectorAll('.entry-body p, .im-bubble p, .im-msg p')];
      const bounds = element.getBoundingClientRect();
      const paragraph = candidates.find((node) => { const rect = node.getBoundingClientRect(); return rect.top >= bounds.top && rect.bottom <= bounds.bottom; });
      const walker = document.createTreeWalker(paragraph ?? element, NodeFilter.SHOW_TEXT);
      const text = walker.nextNode();
      if (!text || text.textContent.length < 4) throw new Error('no visible text selection candidate');
      const range = document.createRange(); range.setStart(text, 0); range.setEnd(text, Math.min(12, text.textContent.length));
      getSelection().removeAllRanges(); getSelection().addRange(range);
    });
    await page.waitForTimeout(200);
    const selection = await page.evaluate(() => getSelection().toString());
    const selected = await capture(page, run, 'selected', selector, rowSelector);
    await append();
    const selectionAppend = await capture(page, run, 'selection-append', selector, rowSelector);
    check('selection and its anchor survive append', selection.length > 0 && await page.evaluate(() => getSelection().toString()) === selection
      && selected.key === selectionAppend.key && Math.abs(selected.offset - selectionAppend.offset) <= 2, { selected, selectionAppend });
    await page.evaluate(() => getSelection().removeAllRanges());
    await page.locator(selector).hover();
    await page.mouse.wheel(0, 100000);
    await settledEnd(page, selector, rowSelector);
    await append();
    await settledEnd(page, selector, rowSelector);
    check('natural reader end arrival resumes later growth', true);
    await page.screenshot({ path: join(output, `${tab}-${width}.png`), fullPage: true });

    await page.locator(selector).evaluate((node) => {
      window.__previousReadingNode = node;
      window.__previousReadingObservers = window.__readingObservers.slice();
    });
    currentSubject = 'reading-fixture-b'; identityRevision += 1;
    sockets.at(-1).send(JSON.stringify({ schema_version: 2, type: 'identity', revision: identityRevision }));
    await page.waitForFunction((selector) => document.querySelector(selector) !== window.__previousReadingNode, selector);
    await settledEnd(page, selector, rowSelector);
    const replacement = await capture(page, run, 'new-subject-owner', selector, rowSelector);
    const late = await page.evaluate(() => {
      const retired = window.__previousReadingObservers.filter((entry) => entry.disconnected);
      for (const entry of retired) entry.callback([], entry.observer);
      return retired.length;
    });
    await page.waitForTimeout(100);
    const afterLate = await measure(page, selector, rowSelector);
    check('subject replacement retires the old lease and ignores its late callbacks', late > 0
      && replacement.key === afterLate.key && Math.abs(replacement.offset - afterLate.offset) <= 2, { late, replacement, afterLate });

    // Trigger actual page ownership disposal, then deliver callbacks retained
    // before disconnect. They must not recreate UI or mutate a future owner.
    const retired = await page.evaluate(() => {
      const old = window.__readingObservers.slice();
      window.dispatchEvent(new PageTransitionEvent('pagehide'));
      for (const entry of old) entry.callback([], entry.observer);
      return { observers: old.length, disconnected: old.filter((entry) => entry.disconnected).length,
        children: document.getElementById('app').childElementCount };
    });
    await page.waitForTimeout(200);
    check('dispose disconnects observers and ignores late observer callbacks', retired.observers > 0 && retired.observers === retired.disconnected
      && retired.children === 0 && await page.locator('#app').evaluate((node) => node.childElementCount) === 0, retired);
    check('render/replay/dispose issue no business writes', outgoing.every((message) => ['client_hello', 'query_submission'].includes(message.type)), outgoing.map((message) => message.type));
    check('real App has no page errors', run.errors.length === 0, run.errors);
    run.status = 'passed';
  } catch (error) {
    run.status = 'failed'; run.failure = String(error);
    await page.screenshot({ path: join(output, `${tab}-${width}-failed.png`), fullPage: true }).catch(() => {});
    throw error;
  } finally {
    await context.close();
    await writeFile(join(output, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
    console.log(JSON.stringify({ tab, width, status: run.status, failure: run.failure }));
  }
}

try {
  for (const [tab, width] of [['inspect', 1280], ['im', 1280], ['inspect', 390], ['im', 390]]) {
    try { await journey(tab, width); }
    catch (error) { console.error(error); }
  }
  receipt.status = receipt.runs.length === 4 && receipt.runs.every((run) => run.status === 'passed') ? 'passed' : 'failed';
  if (receipt.status === 'failed') process.exitCode = 1;
} catch (error) { receipt.status = 'failed'; process.exitCode = 1; console.error(error); }
finally {
  receipt.requests = requests;
  await writeFile(join(output, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
  await browser.close(); await new Promise((done) => server.close(done));
  console.log(JSON.stringify({ status: receipt.status, runs: receipt.runs.map(({ tab, viewport, status, failure }) => ({ tab, viewport, status, failure })), output }));
}
