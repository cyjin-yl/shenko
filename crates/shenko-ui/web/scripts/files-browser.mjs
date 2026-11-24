/**
 * Keyboard regression checks against the actual built Shenko App and its
 * existing demo FileSource. Build normally first; no alternate renderer/data.
 * Required: PUI_CHROME_EXECUTABLE. Set PUI_PLAYWRIGHT_MODULE to a module path,
 * or PROTO_UI_SOURCE to resolve the upstream Playwright installation.
 * Optional: PUI_EVIDENCE_DIR (defaults to generated/files-native).
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const web = fileURLToPath(new URL('../', import.meta.url));
const assets = path.resolve(web, '../src/static');
const evidence = path.resolve(process.env.PUI_EVIDENCE_DIR ?? path.join(web, 'generated/files-native'));
const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const manifestBytes = await readFile(path.join(assets, 'build-manifest.json'));
const manifest = JSON.parse(manifestBytes);
for (const [name, artifact] of Object.entries(manifest.artifacts)) {
  assert.equal(sha256(await readFile(path.join(assets, name))), artifact.sha256, name);
}
assert.ok(process.env.PUI_CHROME_EXECUTABLE, 'PUI_CHROME_EXECUTABLE is required');
const playwright = process.env.PUI_PLAYWRIGHT_MODULE
  ? await import(pathToFileURL(path.resolve(process.env.PUI_PLAYWRIGHT_MODULE)).href)
  : createRequire(path.resolve(process.env.PROTO_UI_SOURCE ?? '.', 'apps/www/package.json'))('playwright-core');
const report = {
  kind: 'shenko-actual-files-keyboard-v1', startedAt: new Date().toISOString(),
  buildManifestSha256: sha256(manifestBytes), upstreamCommit: manifest.upstream.commit,
  scope: 'Built App with its existing demo FileSource; no live backend, hardware or screen-reader claim.',
  cases: [], pageErrors: [], passed: false,
};
const server = createServer(async (request, response) => {
  try {
    const pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    let file;
    if (pathname === '/') file = path.resolve(web, '../src/index.html');
    else if (pathname.startsWith('/assets/')) {
      file = path.resolve(assets, pathname.slice(8));
      const relative = path.relative(assets, file);
      assert.ok(!relative.startsWith('..') && !path.isAbsolute(relative));
    } else { response.writeHead(404).end(); return; }
    const types = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8',
      '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml' };
    response.writeHead(200, { 'Content-Type': types[path.extname(file)] ?? 'application/octet-stream' });
    response.end(await readFile(file));
  } catch { if (!response.headersSent) response.writeHead(404); response.end(); }
});
let browser;
try {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  browser = await playwright.chromium.launch({
    executablePath: process.env.PUI_CHROME_EXECUTABLE, headless: true, chromiumSandbox: true,
  });
  const page = await browser.newPage({ viewport: { width: 1280, height: 900 }, locale: 'en-US' });
  page.on('pageerror', (error) => report.pageErrors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}/?demo=1&tab=files`);
  const grid = page.locator('.files-table');
  const rows = page.locator('.files-row[data-row-id]');
  const activeGrid = () => page.waitForFunction(() =>
    document.activeElement === document.querySelector('.files-table'));
  const rootListing = () => page.waitForFunction(() =>
    document.querySelectorAll('.files-crumb').length === 1
    && document.querySelectorAll('.files-row[data-row-id]').length === 4);
  await rootListing();

  // A loading/empty list must retain a keyboard owner; do not repair focus from
  // the observer after entering a folder or returning to its parent.
  await rows.first().focus();
  await page.keyboard.press('n');
  await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelectorAll('.files-crumb').length === 2
    && document.querySelectorAll('.files-row[data-row-id]').length === 2);
  await activeGrid();
  await page.keyboard.press('ArrowDown');
  assert.equal(await rows.first().evaluate((row) => document.activeElement === row), true);
  assert.equal(await rows.first().getAttribute('aria-selected'), 'true');
  await page.keyboard.press('Backspace');
  await rootListing();
  await activeGrid();
  await page.keyboard.press('p');
  await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelectorAll('.files-crumb').length === 2
    && document.querySelectorAll('.files-row[data-row-id]').length === 0);
  await activeGrid();
  assert.equal(await grid.getAttribute('tabindex'), '0');
  await page.keyboard.press('Alt+ArrowUp');
  await rootListing();
  await activeGrid();
  assert.equal(await page.locator('.files-row[data-row-id][tabindex="0"]').count(), 1);
  report.cases.push({ name: 'folder entry and empty-directory return preserve keyboard navigation', passed: true });

  // Observe cancellation directly: browser Find must remain available when the
  // responsive layout hides the application's search field.
  const findShortcut = () => page.evaluate(() => {
    const event = new KeyboardEvent('keydown', { key: 'f', ctrlKey: true, bubbles: true, cancelable: true });
    document.activeElement.dispatchEvent(event);
    return { prevented: event.defaultPrevented, searchFocused: document.activeElement === document.querySelector('.files-search') };
  });
  assert.deepEqual(await findShortcut(), { prevented: true, searchFocused: true });
  await page.setViewportSize({ width: 430, height: 932 });
  assert.equal(await page.locator('.files-search').isVisible(), false);
  await grid.focus();
  assert.deepEqual(await findShortcut(), { prevented: false, searchFocused: false });
  report.cases.push({ name: 'Find shortcut respects the visible search field and mobile browser fallback', passed: true });
  assert.deepEqual(report.pageErrors, []);
  assert.equal(sha256(await readFile(path.join(assets, 'build-manifest.json'))), report.buildManifestSha256);
  report.passed = true;
} catch (error) {
  report.failure = String(error?.stack ?? error);
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
  await new Promise((resolve) => server.close(resolve));
  report.finishedAt = new Date().toISOString();
  await mkdir(evidence, { recursive: true });
  await writeFile(path.join(evidence, 'report.json'), `${JSON.stringify(report, null, 2)}\n`);
  console.log(JSON.stringify(report, null, 2));
}
