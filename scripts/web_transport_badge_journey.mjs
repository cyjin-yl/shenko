/** Observe the actual live Shenko App and its compiled transport badge.
 * HTTP/WS fixtures terminate at the network boundary. No React props, controller
 * methods, generated component internals or update policy are replaced.
 * This is frontend acceptance, not Rust/backend, deployment, AT or physical input.
 */
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdir, readFile, readdir, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { dirname, join, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const output = resolve(process.env.TRANSPORT_EVIDENCE_DIR ?? '/tmp/shenko-transport-evidence');
const productBase = process.env.SHENKO_PRODUCT_BASE ?? '0e202f7aaecafc6ec761504dfb23cac437ee3e5e';
const staticRoot = join(root, 'crates/shenko-ui/src/static');
const chipSelector = '[data-pui-style~="shenko-status-chip"]';
const dotSelector = '[data-pui-style~="shenko-status-dot"]';
const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');
const git = (...args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();
const receipt = {
  schema_version: 1, status: 'preparing', started_at: new Date().toISOString(),
  scope: 'Real Shenko live frontend, HTTP/WS fixture boundary, Chromium DOM and paint',
  source: { candidate: git('rev-parse', 'HEAD'), product_base: productBase },
  producer_run: process.env.SHENKO_BUILD_RUN ?? null,
  no_product_source_change: false, checks: [], observations: [], requests: [], sockets: [], errors: [],
  limits: {
    backend_or_deployment: false, physical_input_or_at: false,
    public_handle: 'The actual App does not expose the badge handle; no hidden handle identity is inferred.',
    external_slots: 'The authored badge has no external slot; its existing authored dot child is observed.',
    pixels: 'Automatic checks observe visibility, geometry and computed styles. Saved screenshots require separate independent visual review.',
    retirement: 'Synthetic pagehide on the still-live page exercises the real installed application cleanup; it is not a trusted OS/page lifecycle claim.',
  },
};
await mkdir(output, { recursive: true });
const persist = () => writeFile(join(output, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
const check = (name, passed, details) => {
  receipt.checks.push({ name, passed: !!passed, details });
  assert.ok(passed, `${name}: ${JSON.stringify(details)}`);
};
const sourceFiles = [
  'crates/shenko-ui/web/prototypes/transport-badge.proto.ts',
  'crates/shenko-ui/web/src/ui-kit.tsx', 'crates/shenko-ui/web/src/react-renderer.tsx',
  'crates/shenko-ui/web/src/socket-transport.ts', 'crates/shenko-ui/web/src/live-controller.ts',
  'crates/shenko-ui/web/src/main.tsx', 'crates/shenko-ui/web/src/app.css',
  'crates/shenko-ui/src/index.html', 'crates/shenko-ui/web/upstream-pin.json',
  'scripts/web_transport_badge_journey.mjs',
];
const sourceHashes = async () => Object.fromEntries(await Promise.all(sourceFiles.map(async (file) => [file, sha(await readFile(join(root, file)))])));
async function assetInventory(directory, prefix = '') {
  const result = {};
  for (const entry of await readdir(directory, { withFileTypes: true })) {
    const name = prefix + entry.name;
    if (entry.isDirectory()) Object.assign(result, await assetInventory(join(directory, entry.name), name + '/'));
    else {
      assert.ok(entry.isFile(), `non-file asset ${name}`);
      if (name === 'build-manifest.json') continue;
      const data = await readFile(join(directory, entry.name));
      result[name] = { bytes: data.length, sha256: sha(data) };
    }
  }
  return result;
}

let browser, server, page;
try {
  assert.match(productBase, /^[0-9a-f]{40}$/);
  // The evidence-only branch must leave the real application exactly at its
  // independently rebuilt main version. A changed product requires a new build.
  git('diff', '--exit-code', productBase, 'HEAD', '--', 'crates/shenko-ui');
  git('diff', '--exit-code', 'HEAD', '--', 'crates/shenko-ui');
  receipt.no_product_source_change = true;
  receipt.source.before = await sourceHashes();
  const manifestBytes = await readFile(join(staticRoot, 'build-manifest.json'));
  const manifest = JSON.parse(manifestBytes);
  const pin = JSON.parse(await readFile(join(root, 'crates/shenko-ui/web/upstream-pin.json')));
  const assets = await assetInventory(staticRoot);
  assert.deepEqual(assets, manifest.artifacts, 'served assets must exactly equal the normal build manifest');
  assert.equal(manifest.upstream.commit, pin.commit);
  assert.equal(manifest.upstream.lock_sha256, pin.lock_sha256);
  const badge = manifest.compiler.prototypes.find((entry) => entry.name === 'transport-badge');
  assert.ok(badge, 'normal build must contain the real authored badge');
  assert.equal(badge.entry_sha256, receipt.source.before['crates/shenko-ui/web/prototypes/transport-badge.proto.ts']);
  receipt.artifacts = { manifest_sha256: sha(manifestBytes), assets, compiler: manifest.compiler, upstream: manifest.upstream };
  await writeFile(join(output, 'served-build-manifest.json'), manifestBytes);
  await persist();

  server = createServer(async (request, response) => {
    const url = new URL(request.url, 'http://localhost');
    receipt.requests.push({ method: request.method, path: url.pathname });
    try {
      let body, mime = 'application/json';
      if (url.pathname === '/') { body = await readFile(join(root, 'crates/shenko-ui/src/index.html')); mime = 'text/html'; }
      else if (url.pathname.startsWith('/assets/')) {
        const file = resolve(staticRoot, decodeURIComponent(url.pathname.slice('/assets/'.length)));
        if (!file.startsWith(staticRoot + sep)) { response.writeHead(400); response.end(); return; }
        body = await readFile(file);
        mime = file.endsWith('.css') ? 'text/css' : file.endsWith('.js') ? 'text/javascript' : 'application/json';
      } else if (url.pathname === '/api/self') body = JSON.stringify({ subject_id: 'transport-observer',
        identity: { agent_id: 'transport-observer', revision: 1, display_name: 'Transport observer', naming: 'explicit' }, step: 0 });
      else if (url.pathname === '/api/submissions') body = JSON.stringify({ available: true, subject_id: 'transport-observer', items: [], total: 0, has_older: false });
      else if (url.pathname === '/api/conversations') body = JSON.stringify({ available: true, messages: [] });
      else if (url.pathname === '/api/tasks') body = JSON.stringify({ available: true, tasks: [] });
      else if (url.pathname === '/api/playground') body = JSON.stringify({ available: true, artifacts: [] });
      else if (url.pathname === '/api/owner/state') body = JSON.stringify({ claimed: true });
      else if (url.pathname.startsWith('/api/')) body = JSON.stringify({ available: true, entries: [] });
      else if (url.pathname === '/favicon.ico') { response.writeHead(204); response.end(); return; }
      else { response.writeHead(404); response.end(); return; }
      response.writeHead(200, { 'content-type': mime, 'cache-control': 'no-store' });
      response.end(body);
    } catch (error) { receipt.errors.push({ surface: 'fixture-server', message: String(error) }); response.writeHead(500); response.end('Fixture unavailable'); }
  });
  await new Promise((done, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', done); });
  const origin = `http://127.0.0.1:${server.address().port}`;
  assert.notEqual(process.getuid?.(), 0, 'run native Chromium as a non-root user');
  assert.ok(process.env.PLAYWRIGHT_MODULE, 'select the exact installed browser client');
  assert.ok(process.env.CHROMIUM_EXECUTABLE, 'select the actual native browser executable');
  const { chromium } = await import(pathToFileURL(resolve(process.env.PLAYWRIGHT_MODULE)).href);
  browser = await chromium.launch({ headless: true, chromiumSandbox: true, executablePath: process.env.CHROMIUM_EXECUTABLE });
  receipt.browser = { node: process.version, uid: process.getuid?.(), version: browser.version(), executable: process.env.CHROMIUM_EXECUTABLE, chromium_sandbox: true, viewport: { width: 1280, height: 900 } };
  const context = await browser.newContext({ viewport: receipt.browser.viewport, locale: 'en-US', colorScheme: 'light', reducedMotion: 'reduce' });
  page = await context.newPage();
  page.setDefaultTimeout(8_000);
  page.on('pageerror', (error) => receipt.errors.push({ surface: 'pageerror', message: String(error) }));
  page.on('console', (message) => { if (message.type() === 'error') receipt.errors.push({ surface: 'console', message: message.text() }); });
  const routes = [];
  await page.routeWebSocket('**/ws', (socket) => {
    const row = { number: routes.length + 1, at: new Date().toISOString(), received: [], closed: null };
    routes.push(socket); receipt.sockets.push(row);
    socket.onClose((code, reason) => { row.closed = { code, reason, at: new Date().toISOString() }; });
    socket.onMessage((raw) => {
      const message = JSON.parse(String(raw)); row.received.push(message);
      if (message.type === 'client_hello') socket.send(JSON.stringify({ schema_version: 2, type: 'hello', server_version: 'transport-network-fixture',
        capabilities: { append: true, durable_input_receipts: true, committed_replay: true, streaming: true } }));
    });
  });
  await page.goto(`${origin}/?tab=im&thread=direct&theme=light&lang=en`, { waitUntil: 'networkidle' });
  const chip = page.locator(chipSelector);
  await chip.waitFor({ state: 'visible' });
  const initialRoot = await chip.elementHandle();
  const initialDot = await chip.locator(dotSelector).elementHandle();
  assert.ok(initialRoot && initialDot);
  async function observe(name, state, label) {
    await page.waitForFunction(({ selector, state, label }) => {
      const node = document.querySelector(selector);
      return node?.textContent === label && node.getAttribute('data-pui-style')?.split(/\s+/).includes(`shenko-status-${state}`);
    }, { selector: chipSelector, state, label });
    const [rootVisible, dotVisible] = await Promise.all([chip.isVisible(), chip.locator(dotSelector).isVisible()]);
    const observation = await chip.evaluate((node, { initialRoot, initialDot, dotSelector }) => {
      const dot = node.querySelector(dotSelector), bounds = node.getBoundingClientRect();
      return { text: node.textContent, tokens: node.getAttribute('data-pui-style'), sameRoot: node === initialRoot,
        sameDot: dot === initialDot, rootConnected: node.isConnected, dotConnected: dot?.isConnected,
        color: dot ? getComputedStyle(dot).backgroundColor : null, foreground: getComputedStyle(node).color,
        theme: document.documentElement.dataset.theme, bounds: { width: bounds.width, height: bounds.height },
        dotBounds: dot ? { width: dot.getBoundingClientRect().width, height: dot.getBoundingClientRect().height } : null };
    }, { initialRoot, initialDot, dotSelector });
    receipt.observations.push({ name, expected: { state, label }, at: new Date().toISOString(), rootVisible, dotVisible, ...observation });
    check(`${name}: label, tone and the original authored nodes remain visible`, rootVisible && dotVisible && observation.text === label && observation.sameRoot && observation.sameDot && observation.rootConnected && observation.dotConnected && observation.bounds.width > 0 && observation.bounds.height > 0 && observation.dotBounds?.width > 0 && observation.dotBounds?.height > 0, { rootVisible, dotVisible, ...observation });
    await persist();
    return observation;
  }
  const connected = await observe('initial-connected', 'online', 'Connected');
  await page.screenshot({ path: join(output, 'connected-light.png') });
  for (let cycle = 1; cycle <= 2; cycle++) {
    const before = routes.length;
    await routes.at(-1).close({ code: 1012, reason: `observer reconnect ${cycle}` });
    const reconnecting = await observe(`cycle-${cycle}-reconnecting`, 'reconnecting', 'Reconnecting');
    check(`cycle-${cycle}: the computed dot color changes with the network tone`, reconnecting.color !== connected.color, { connected: connected.color, reconnecting: reconnecting.color });
    if (cycle === 1) await page.screenshot({ path: join(output, 'reconnecting-light.png') });
    await page.waitForFunction(() => document.querySelector('[data-pui-style~="shenko-status-online"]')?.textContent === 'Connected');
    check(`cycle-${cycle}: the actual transport opened one replacement socket`, routes.length === before + 1, { before, after: routes.length });
    const restored = await observe(`cycle-${cycle}-connected`, 'online', 'Connected');
    check(`cycle-${cycle}: original online computed color returns`, restored.color === connected.color, { original: connected.color, current: restored.color });
  }
  await page.getByRole('button', { name: 'Switch to dark theme', exact: true }).click();
  const dark = await observe('theme-dark-connected', 'online', 'Connected');
  check('real application theme input keeps the same badge owner nodes', dark.theme === 'dark', dark);
  await page.screenshot({ path: join(output, 'connected-dark.png') });

  // The document stays alive, so the browser cannot hide a surviving reconnect
  // timer by destroying the realm. This dispatch reaches the real main.tsx hook.
  await routes.at(-1).close({ code: 1012, reason: 'observer retire while retry is pending' });
  await observe('before-retirement', 'reconnecting', 'Reconnecting');
  const socketsBeforeRetirement = routes.length;
  receipt.retirement = await page.evaluate(() => {
    const event = new PageTransitionEvent('pagehide', { persisted: false });
    window.dispatchEvent(event);
    return { stimulus: 'synthetic pagehide', trusted: event.isTrusted, persisted: event.persisted };
  });
  await chip.waitFor({ state: 'detached' });
  await page.waitForTimeout(1_500); // The successful socket reset the actual first reconnect delay to one second.
  check('actual application retirement clears the pending reconnect and removes the old nodes', routes.length === socketsBeforeRetirement && await chip.count() === 0 && await initialRoot.evaluate((node) => !node.isConnected) && await initialDot.evaluate((node) => !node.isConnected), { before: socketsBeforeRetirement, after: routes.length });
  check('the actual live entry was used', receipt.requests.some(({ path }) => path === '/api/self') && !receipt.requests.some(({ path }) => path.includes('demo-controller')), receipt.requests);
  check('no application or fixture error was hidden', receipt.errors.length === 0, receipt.errors);
  receipt.status = 'passed';
} catch (error) {
  receipt.status = 'failed'; receipt.failure = error instanceof Error ? error.stack : String(error);
  if (page) {
    await page.screenshot({ path: join(output, 'failure.png') }).catch(() => {});
    await writeFile(join(output, 'failure.html'), await page.content().catch(() => '')).catch(() => {});
  }
  process.exitCode = 1;
} finally {
  await browser?.close().catch((error) => { receipt.status = 'failed'; receipt.errors.push({ surface: 'browser-close', message: String(error) }); process.exitCode = 1; });
  if (server) await new Promise((done) => server.close(done));
  receipt.source.after = await sourceHashes();
  if (JSON.stringify(receipt.source.before) !== JSON.stringify(receipt.source.after)) { receipt.status = 'failed'; receipt.source.changed_during_run = true; process.exitCode = 1; }
  receipt.finished_at = new Date().toISOString();
  await persist();
  console.log(JSON.stringify({ status: receipt.status, checks: receipt.checks.length, source: receipt.source.candidate, product_base: productBase, output, failure: receipt.failure ?? null }, null, 2));
}
