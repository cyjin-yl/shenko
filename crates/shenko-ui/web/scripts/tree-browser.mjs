/**
 * Actual Shenko App: two public Tree domains.
 * Required: PROTO_UI_SOURCE, PUI_TREE_BUILD_RECEIPT, PUI_CHROME_EXECUTABLE.
 * Optional: PUI_PLAYWRIGHT_MODULE, PUI_EVIDENCE_DIR.
 * Read fixtures enter the existing HTTP/WebSocket controller; no replacement
 * renderer, direct state injection or test-only product hooks are installed.
 */
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { readFile, readdir, mkdir, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const web = fileURLToPath(new URL('../', import.meta.url));
const application = path.resolve(web, '../../..');
const assets = path.join(application, 'crates/shenko-ui/src/static');
const evidence = path.resolve(process.env.PUI_EVIDENCE_DIR ?? path.join(web, 'generated/tree-native'));
const upstream = path.resolve(required('PROTO_UI_SOURCE'));
const buildReceiptPath = path.resolve(required('PUI_TREE_BUILD_RECEIPT'));
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
assert.equal(manifest.realisation.adapter.some((item) => item.family === 'tree'), true);
assert.deepEqual(manifest.realisation.compiled, ['transport-badge', 'message-bubble']);
assert.equal(generation.runtime_imported, false);
assert.equal(generation.source_tree_sha256, manifest.compiler.source_tree_sha256);
for (const [name, artifact] of Object.entries(manifest.artifacts))
  assert.equal(hash(await readFile(path.join(assets, name))), artifact.sha256, name);
const manifestHash = hash(await readFile(path.join(assets, 'build-manifest.json')));
const html = path.join(application, 'crates/shenko-ui/src/index.html');
const htmlHash = hash(await readFile(html));
const report = {
  kind: 'shenko-two-tree-domains-v1', startedAt: new Date().toISOString(),
  appBase: buildReceipt.appBase, upstreamCandidate: pin.commit,
  sourceHash: hash(JSON.stringify(sourceBefore)), sourceBefore,
  buildReceiptSha256: hash(await readFile(buildReceiptPath)),
  buildManifestSha256: manifestHash, htmlSha256: htmlHash,
  observerSha256: hash(await readFile(fileURLToPath(import.meta.url))),
  scope: 'Actual live App/controller with deterministic HTTP and WebSocket read fixtures. Two independent public Tree domains, explicit task parent IDs and MIME categories. No live backend/model, physical-input hardware, AT, lazy loading, virtualization or full portable Tree admission.',
  selection: 'Controlled-empty; public value proposals activate App records or disclose branches. This run does not approve a new portable single-selection contract.',
  cases: [], pageErrors: [], webSockets: [], cleanupErrors: [], fixtureReads: [], writes: [],
};
const playwright = process.env.PUI_PLAYWRIGHT_MODULE
  ? await import(pathToFileURL(process.env.PUI_PLAYWRIGHT_MODULE).href)
  : createRequire(path.join(upstream, 'apps/www/package.json'))('playwright-core');
const launchOptions = { executablePath, headless: true, chromiumSandbox: true };
report.launchOptions = launchOptions;

let fixture = null;
const sockets = new Set();
const knownSockets = new Set();
const tasks = () => [
  { task_id: 'shared', role: 'subject_root', kind: 'subject.main', name: '主体 Root', state: 'running', created_at: '2026-11-23T10:00:00Z' },
  { task_id: 'alpha', parent_task_id: 'shared', role: 'delegation', kind: 'model.child', name: 'Alpha 整理员', goal: '保留观察来源', state: 'running', created_at: '2026-11-23T10:01:00Z' },
  { task_id: 'tool', parent_task_id: 'alpha', role: 'tool', kind: 'fs.read', name: 'Read 读取', state: 'terminal', created_at: '2026-11-23T10:02:00Z' },
  { task_id: 'beta', parent_task_id: 'shared', role: 'delegation', kind: 'model.child', name: 'Beta 审阅员', state: 'pending', created_at: '2026-11-23T10:03:00Z' },
  { task_id: 'peer', role: 'unknown', kind: 'plugin.operation', name: 'Other 独立任务', state: 'pending', created_at: '2026-11-23T10:04:00Z' },
];
const artifacts = () => [
  { artifact_id: 'shared', title: 'Alpha 页面', mime: 'text/html', revision: 4, parent_revision: 3, lifecycle: 'draft', updated_at: '2026-11-23T10:01:00Z' },
  { artifact_id: 'notes', title: 'Beta 笔记', mime: 'text/markdown', revision: 2, lifecycle: 'draft', updated_at: '2026-11-23T10:02:00Z' },
  { artifact_id: 'other', title: 'Other 页面', mime: 'text/html', revision: 1, lifecycle: 'draft', updated_at: '2026-11-23T10:03:00Z' },
];
const resetFixture = (name) => {
  fixture = { name, subject: 'tree-subject-a', identity: 1, step: 1, tasks: tasks(), artifacts: artifacts() };
};
const send = (socket, value) => {
  if (socket.destroyed) return;
  const data = Buffer.from(JSON.stringify(value));
  const header = data.length < 126 ? Buffer.from([0x81, data.length]) : Buffer.from([0x81, 126, data.length >> 8, data.length & 255]);
  socket.write(Buffer.concat([header, data]));
};
const broadcast = (value) => { for (const socket of sockets) send(socket, value); };
function committedReadRefresh() {
  const step = ++fixture.step;
  broadcast({ schema_version: 2, type: 'timeline_part', committed_only: true,
    role: 'subject', message_id: 'tree-step-' + step, step_id: 'tree-step-' + step,
    part_id: 'text/0', part_revision: '1', commit_ref: 'fixture/tree/' + step,
    content: '层级读取更新 ' + step, terminal: true, step, chunk_seq: 0 });
}
const server = createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://127.0.0.1');
    const pathname = decodeURIComponent(url.pathname);
    if (request.method !== 'GET' && request.method !== 'HEAD') {
      report.writes.push({ case: fixture?.name, method: request.method, path: pathname });
      response.writeHead(405).end(); return;
    }
    let dto;
    if (pathname === '/api/config') dto = {};
    else if (pathname === '/api/self') dto = { subject_id: fixture.subject, step: fixture.step, status: 'running',
      identity: { agent_id: fixture.subject, revision: fixture.identity, display_name: 'Tree fixture ' + fixture.subject } };
    else if (pathname === '/api/conversations') dto = { available: true, messages: [] };
    else if (pathname === '/api/tasks') dto = { available: true, items: fixture.tasks };
    else if (pathname === '/api/playground') dto = { available: true, artifacts: fixture.artifacts, roots: [] };
    else if (pathname === '/api/memory') dto = { items: [] };
    else if (pathname === '/api/config/namespaces') dto = { namespaces: [] };
    else if (pathname === '/api/config/entries') dto = { available: true, entries: [] };
    else if (pathname === '/api/owner/state') dto = { claimed: true, available: true };
    else if (pathname === '/api/submissions') dto = { schema_version: 1, available: true,
      subject_id: fixture.subject, items: [], total: 0, has_older: false };
    else if (pathname === '/health') dto = { version: 'tree-read-fixture' };
    if (dto !== undefined) {
      report.fixtureReads.push({ case: fixture.name, path: pathname, value: structuredClone(dto) });
      response.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
      response.end(JSON.stringify(dto)); return;
    }
    if (pathname.startsWith('/playground/')) {
      report.fixtureReads.push({ case: fixture.name, path: pathname, method: request.method, result: 'no adopted page fixture' });
      response.writeHead(404).end(); return;
    }
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
  } catch (error) {
    report.cleanupErrors.push({ phase: 'http-handler', message: String(error) });
    if (!response.headersSent) response.writeHead(500);
    response.end();
  }
});
server.on('connection', (socket) => { knownSockets.add(socket); socket.on('close', () => knownSockets.delete(socket)); });
server.on('upgrade', (request, socket) => {
  if (request.url !== '/ws' || !request.headers['sec-websocket-key']) { socket.destroy(); return; }
  const accept = createHash('sha1').update(request.headers['sec-websocket-key'] + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
  socket.write('HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ' + accept + '\r\n\r\n');
  sockets.add(socket);
  let buffered = Buffer.alloc(0);
  socket.on('data', (chunk) => {
    buffered = Buffer.concat([buffered, chunk]);
    while (buffered.length >= 2) {
      const op = buffered[0] & 15;
      const masked = !!(buffered[1] & 128);
      let length = buffered[1] & 127, offset = 2;
      if (length === 126) { if (buffered.length < 4) return; length = buffered.readUInt16BE(2); offset = 4; }
      if (length === 127) { socket.destroy(); return; } // fixture accepts no large business payload
      if (buffered.length < offset + (masked ? 4 : 0) + length) return;
      const mask = masked ? buffered.subarray(offset, offset + 4) : null;
      offset += masked ? 4 : 0;
      const payload = Buffer.from(buffered.subarray(offset, offset + length));
      buffered = buffered.subarray(offset + length);
      if (mask) for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
      if (op === 8) { socket.end(Buffer.from([0x88, 0])); return; }
      if (op === 9) { socket.write(Buffer.concat([Buffer.from([0x8a, payload.length]), payload])); continue; }
      if (op !== 1) continue;
      let value;
      try { value = JSON.parse(payload.toString()); } catch { value = payload.toString(); }
      report.webSockets.push({ case: fixture?.name, direction: 'client', value });
      if (value?.type !== 'client_hello') report.writes.push({ case: fixture?.name, transport: 'websocket', value });
    }
  });
  socket.on('error', () => {});
  socket.on('close', () => sockets.delete(socket));
  send(socket, { schema_version: 2, type: 'hello', server_version: 'tree-read-fixture',
    capabilities: { append: false, steer: false, withdraw: false, actions: false, documents: false,
      attachments: false, durable_input_receipts: false, committed_replay: true, streaming: false } });
});
const treeSelector = (domain) => '[data-tree-domain="' + domain + '"] [role="tree"]';
const itemSelector = (domain, key) => '[data-tree-domain="' + domain + '"] [role="treeitem"]:has(> .inspector-tree-label[data-entry-key="' + key + '"])';
const task = (page, id) => page.locator(itemSelector('tasks', 'task:' + id));
const artifact = (page, id) => page.locator(itemSelector('artifacts', id));
const settle = (page) => page.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
async function expectedFocus(page, key) {
  await page.waitForFunction((key) => document.activeElement?.querySelector(':scope > .inspector-tree-label')?.getAttribute('data-entry-key') === key, key, { timeout: 1000 });
}
async function expectedExpanded(locator, value) {
  await locator.page().waitForFunction(({ selector, value }) =>
    document.querySelector(selector)?.getAttribute('aria-expanded') === String(value),
    { selector: await locator.evaluate((element) => '[data-tree-domain="' + element.closest('[data-tree-domain]').dataset.treeDomain + '"] [role="treeitem"]:has(> .inspector-tree-label[data-entry-key="' + element.querySelector(':scope > .inspector-tree-label')?.getAttribute('data-entry-key') + '"])'), value },
    { timeout: 1000 });
}
async function treeState(page, domain) {
  return page.locator('[data-tree-domain="' + domain + '"]').evaluate((section) => {
    const root = section.querySelector('[role="tree"]');
    return {
      id: root?.id, invalid: root?.getAttribute('aria-invalid'),
      items: [...section.querySelectorAll('[role="treeitem"]')].map((item) => ({
        key: item.querySelector(':scope > .inspector-tree-label')?.getAttribute('data-entry-key'), id: item.id,
        level: item.getAttribute('aria-level'), position: item.getAttribute('aria-posinset'), size: item.getAttribute('aria-setsize'),
        expanded: item.getAttribute('aria-expanded'), selected: item.getAttribute('aria-selected'),
        controls: item.getAttribute('aria-controls'), tabIndex: item.tabIndex,
        hidden: item.getAttribute('aria-hidden'), display: getComputedStyle(item).display,
      })),
      groups: [...section.querySelectorAll('[role="group"]')].map((group) => ({
        id: group.id, labelledBy: group.getAttribute('aria-labelledby'), hidden: group.getAttribute('aria-hidden'),
        display: getComputedStyle(group).display,
      })),
    };
  });
}
function proveRelations(state, check) {
  for (const item of state.items) if (item.controls) {
    const group = state.groups.find((group) => group.id === item.controls);
    check(Boolean(group), 'controls resolves in the same Tree domain: ' + item.key);
    check(group.labelledBy === item.id, 'Group labels from its actual Item: ' + item.key);
  }
}
async function openLens(page, domain) {
  await page.keyboard.press(domain === 'tasks' ? 'Control+1' : 'Control+3');
  await page.locator(treeSelector(domain)).waitFor({ state: 'visible', timeout: 3000 });
  await settle(page);
}
async function refreshReadModels(page) {
  const response = page.waitForResponse((response) => response.url().endsWith('/api/tasks') && response.status() === 200, { timeout: 5000 });
  committedReadRefresh();
  await response;
  await settle(page);
}
let browser;
let failed = false;
let origin;
async function journey(name, contextOptions, action) {
  resetFixture(name);
  const entry = { name, startedAt: new Date().toISOString(), checks: [], passed: false };
  report.cases.push(entry);
  const context = await browser.newContext({ viewport: { width: 1600, height: 900 }, locale: 'zh-CN',
    colorScheme: 'light', reducedMotion: 'reduce', ...contextOptions });
  const page = await context.newPage();
  page.setDefaultTimeout(1000);
  page.on('pageerror', (error) => report.pageErrors.push({ case: name, message: error.message, stack: error.stack }));
  await page.addInitScript(() => {
    window.__treeInputs = [];
    for (const type of ['keydown', 'pointerdown', 'pointerup']) document.addEventListener(type, (event) =>
      window.__treeInputs.push({ type, trusted: event.isTrusted, key: event.key, pointerType: event.pointerType,
        target: event.target?.closest?.('[role="treeitem"]')?.querySelector(':scope > .inspector-tree-label')?.getAttribute('data-entry-key') ?? event.target?.tagName }), true);
  });
  const check = (condition, label, facts) => {
    entry.checks.push({ label, passed: Boolean(condition), ...(facts === undefined ? {} : { facts }) });
    assert.ok(condition, label);
  };
  try {
    await page.goto(origin, { waitUntil: 'networkidle', timeout: 15000 });
    await page.locator('.stream').waitFor({ state: 'visible', timeout: 5000 });
    await page.waitForFunction(() => document.querySelectorAll('[data-tree-domain] [role="treeitem"]').length === 10, undefined, { timeout: 5000 });
    await action(page, check, entry);
    check(report.fixtureReads.some((read) => read.case === name && read.path === '/api/conversations' &&
      read.value.available === true && Array.isArray(read.value.messages) && read.value.messages.length === 0) &&
      await page.getByText(/^会话读取失败[:：]/).count() === 0,
      'background conversation read uses the actual empty DTO without a read error');
    entry.inputs = await page.evaluate(() => window.__treeInputs);
    check(entry.inputs.filter((event) => event.type === 'keydown').length > 0, 'native keyboard input was observed');
    check(entry.inputs.every((event) => event.trusted), 'all recorded key/pointer inputs are trusted');
    await page.screenshot({ path: path.join(evidence, name + '.png'), fullPage: true });
    entry.passed = true;
  } catch (error) {
    failed = true; entry.failure = { message: String(error), stack: error.stack };
    try {
      entry.inputs = await page.evaluate(() => window.__treeInputs);
      entry.dom = await page.locator('.inspector-lens-pane').innerHTML();
      await page.screenshot({ path: path.join(evidence, name + '-failed.png'), fullPage: true });
    } catch (captureError) { entry.captureError = String(captureError); }
  } finally {
    try { await context.close(); } catch (error) { report.cleanupErrors.push({ case: name, message: String(error) }); }
    entry.finishedAt = new Date().toISOString();
    await writeFile(path.join(evidence, 'progress.json'), JSON.stringify(report, null, 2) + '\n');
  }
}
try {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  origin = 'http://127.0.0.1:' + server.address().port;
  browser = await playwright.chromium.launch(launchOptions);
  report.browserVersion = browser.version();
  await journey('tasks-desktop', {}, async (page, check, entry) => {
    await openLens(page, 'tasks');
    const initial = await treeState(page, 'tasks'); entry.initial = initial;
    check(initial.invalid === 'false', 'actual task hierarchy is valid', initial);
    check(JSON.stringify(initial.items.map((item) => [item.key, item.level, item.position, item.size])) ===
      JSON.stringify([['task:shared', '1', '1', '2'], ['task:alpha', '2', '1', '2'], ['task:tool', '3', '1', '1'],
        ['task:beta', '2', '2', '2'], ['task:peer', '1', '2', '2']]), 'explicit server parent facts produce exact hierarchy and sibling ordinals');
    proveRelations(initial, check);
    const other = await treeState(page, 'artifacts');
    check(other.invalid === 'false', 'independent artifact Tree is valid while hidden');
    const ids = [...initial.items, ...initial.groups, ...other.items, ...other.groups].map((item) => item.id).filter(Boolean);
    check(new Set(ids).size === ids.length, 'AX relationship IDs remain unique across the two real domains');
    await task(page, 'shared').click(); await expectedExpanded(task(page, 'shared'), false);
    await page.keyboard.press('ArrowRight'); await expectedExpanded(task(page, 'shared'), true);
    await page.keyboard.press('ArrowDown'); await expectedFocus(page, 'task:alpha');
    await page.keyboard.press('ArrowRight'); await expectedFocus(page, 'task:tool');
    await page.keyboard.press('ArrowLeft'); await expectedFocus(page, 'task:alpha');
    await page.keyboard.press('ArrowLeft'); await expectedExpanded(task(page, 'alpha'), false);
    check(await task(page, 'tool').isVisible() === false, 'collapsed child stays authored and becomes hidden');
    check(await task(page, 'tool').count() === 1, 'collapse does not drop the logical record');
    await page.keyboard.press('ArrowRight'); await expectedExpanded(task(page, 'alpha'), true);
    await page.keyboard.press('Home'); await expectedFocus(page, 'task:shared');
    await page.keyboard.press('End'); await expectedFocus(page, 'task:peer');
    await page.keyboard.press('b'); await expectedFocus(page, 'task:beta');
    await page.keyboard.press('Enter');
    await page.locator('.inspector-tree-details').waitFor({ state: 'visible' });
    const details = await page.locator('.inspector-tree-details').textContent();
    check(details.includes('beta') && details.includes('shared') && details.includes('delegation') && details.includes('model.child'),
      'native activation inspects the actual task record without executing it', details);
    const final = await treeState(page, 'tasks');
    check(final.items.every((item) => item.selected === 'false'), 'controlled-empty App policy does not commit a selected value');
    check(JSON.stringify(await treeState(page, 'artifacts')) === JSON.stringify(other), 'task navigation and expansion leave the artifact domain unchanged');
  });
  await journey('artifacts-desktop', {}, async (page, check, entry) => {
    await openLens(page, 'artifacts');
    const initial = await treeState(page, 'artifacts'); entry.initial = initial;
    check(initial.invalid === 'false', 'actual artifact MIME hierarchy is valid');
    check(JSON.stringify(initial.items.map((item) => [item.key, item.level, item.position, item.size])) ===
      JSON.stringify([['mime:text/html', '1', '1', '2'], ['artifact:shared', '2', '1', '2'],
        ['artifact:other', '2', '2', '2'], ['mime:text/markdown', '1', '2', '2'], ['artifact:notes', '2', '1', '1']]),
      'MIME categories and actual artifact order form an independent hierarchy');
    proveRelations(initial, check);
    check(initial.items.every((item) => !['3', 'artifact:3'].includes(item.key)), 'parent_revision 3 does not become a parent artifact');
    const tasksBefore = await treeState(page, 'tasks');
    await page.evaluate(() => { window.__mainBeforeTree = document.querySelector('.stream'); window.__editorBeforeTree = document.querySelector('.composer-input'); });
    await artifact(page, 'mime:text/html').click(); await expectedExpanded(artifact(page, 'mime:text/html'), false);
    check(await artifact(page, 'artifact:shared').isVisible() === false, 'pointer activation collapses only this MIME branch');
    await page.keyboard.press('ArrowRight'); await expectedExpanded(artifact(page, 'mime:text/html'), true);
    await page.keyboard.press('ArrowRight'); await expectedFocus(page, 'artifact:shared');
    check(JSON.stringify(await treeState(page, 'tasks')) === JSON.stringify(tasksBefore), 'artifact disclosure leaves task expansion and AX facts unchanged');
    await page.keyboard.press('Enter');
    await page.locator('.work-frame').waitFor({ state: 'visible' });
    await page.locator('.work-frame-empty').waitFor({ state: 'visible' });
    check(report.fixtureReads.some((read) => read.case === fixture.name && read.method === 'HEAD' && read.path === '/playground/shared/adopted'),
      'artifact activation enters the actual adopted-work read route');
    check(await page.locator('.work-iframe').count() === 0, 'missing adoption stays missing instead of inventing a runnable page');
    check(await page.evaluate(() => Boolean(window.__mainBeforeTree) && Boolean(window.__editorBeforeTree)
      && window.__mainBeforeTree === document.querySelector('.stream')
      && window.__editorBeforeTree === document.querySelector('.composer-input')), 'work lens keeps main stream and editor identities');
    await page.keyboard.press('Control+0');
    await page.keyboard.press('Control+3');
    await page.locator(treeSelector('artifacts')).waitFor({ state: 'visible' });
    entry.reopened = await treeState(page, 'artifacts');
    proveRelations(entry.reopened, check);
  });
  await journey('phone-invalid-and-recovery', { viewport: { width: 390, height: 844 }, colorScheme: 'dark', forcedColors: 'active' }, async (page, check, entry) => {
    await openLens(page, 'tasks');
    await task(page, 'beta').click();
    await page.keyboard.press('Home'); await expectedFocus(page, 'task:shared');
    const focus = await task(page, 'shared').evaluate((element) => {
      const style = getComputedStyle(element);
      return { outline: style.outlineStyle, width: parseFloat(style.outlineWidth), boxShadow: style.boxShadow };
    });
    check((focus.width > 0 && focus.outline !== 'none') || focus.boxShadow !== 'none', 'keyboard focus remains painted under forced colors', focus);
    const initialArtifacts = await treeState(page, 'artifacts');
    fixture.tasks = [
      { ...tasks()[0], task_id: 'cycle-a', parent_task_id: 'cycle-b' },
      { ...tasks()[1], task_id: 'cycle-b', parent_task_id: 'cycle-a' },
      { ...tasks()[2], task_id: 'cycle-a', parent_task_id: undefined },
      { ...tasks()[3], task_id: 'orphan', parent_task_id: 'missing' },
      { ...tasks()[4], task_id: '' },
    ];
    entry.invalidInput = structuredClone(fixture.tasks);
    await refreshReadModels(page);
    await page.waitForFunction(() => document.querySelector('[data-tree-domain="tasks"] [role="tree"]')?.getAttribute('aria-invalid') === 'true', undefined, { timeout: 1000 });
    const invalid = await treeState(page, 'tasks'); entry.invalid = invalid;
    check(invalid.items.length === 5, 'every invalid server record remains authored');
    check(invalid.items.filter((item) => item.key === 'task:cycle-a').length === 2 && invalid.items.some((item) => item.key === ''),
      'duplicate and empty public keys are preserved');
    check(invalid.items.every((item) => item.tabIndex === -1), 'public invalid state removes all item navigation');
    check(await page.locator('[data-tree-domain="tasks"] .inspector-tree-invalid').isVisible(), 'invalid graph is disclosed in the actual App');
    check((await treeState(page, 'artifacts')).invalid === 'false', 'invalid tasks do not invalidate artifacts');
    fixture.tasks = tasks();
    fixture.tasks[2] = { ...fixture.tasks[2], parent_task_id: 'shared' };
    fixture.tasks.splice(3, 0, { ...tasks()[4], task_id: 'new', parent_task_id: 'alpha', name: 'New 新任务' });
    await refreshReadModels(page);
    await page.waitForFunction(() => document.querySelector('[data-tree-domain="tasks"] [role="tree"]')?.getAttribute('aria-invalid') === 'false', undefined, { timeout: 1000 });
    const recovered = await treeState(page, 'tasks'); entry.recovered = recovered;
    check(recovered.items.length === 6 && recovered.items.find((item) => item.key === 'task:tool')?.level === '2'
      && recovered.items.find((item) => item.key === 'task:new')?.level === '3', 'actual read refresh admits insertion and explicit reparenting');
    proveRelations(recovered, check);
    check(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1), 'phone Inspector has no horizontal page overflow');
    check(initialArtifacts.items.map((item) => item.key).join() === (await treeState(page, 'artifacts')).items.map((item) => item.key).join(),
      'invalid/recovery cycle preserves the other domain records');
    await page.keyboard.press('Control+3');
    await artifact(page, 'mime:text/markdown').click(); await expectedExpanded(artifact(page, 'mime:text/markdown'), false);
    await page.keyboard.press('ArrowRight'); await expectedExpanded(artifact(page, 'mime:text/markdown'), true);
    await page.keyboard.press('ArrowRight'); await expectedFocus(page, 'artifact:notes');
  });
  await journey('subject-retirement', {}, async (page, check, entry) => {
    await openLens(page, 'tasks');
    await task(page, 'beta').click();
    await page.keyboard.press('Home'); await expectedFocus(page, 'task:shared');
    await page.evaluate(() => {
      window.__retiredTaskRoot = document.querySelector('[data-tree-domain="tasks"] [role="tree"]');
      window.__retiredArtifactRoot = document.querySelector('[data-tree-domain="artifacts"] [role="tree"]');
      window.__mainBeforeTree = document.querySelector('.stream');
    });
    const box = await task(page, 'alpha').boundingBox();
    await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
    await page.mouse.down();
    fixture.subject = 'tree-subject-b'; fixture.identity = 2;
    const self = page.waitForResponse((response) => response.url().endsWith('/api/self') && response.status() === 200, { timeout: 5000 });
    broadcast({ schema_version: 2, type: 'identity', revision: 2 });
    await self;
    await page.waitForFunction(() => !window.__retiredTaskRoot.isConnected && !window.__retiredArtifactRoot.isConnected, undefined, { timeout: 1000 });
    await page.mouse.up(); await settle(page);
    check(await page.locator('.inspector-tree-details').count() === 0, 'late old pointer release cannot inspect a record in the new owner');
    check(await page.evaluate(() => window.__mainBeforeTree === document.querySelector('.stream')), 'owner replacement preserves the main stream host');
    const newTask = await treeState(page, 'tasks');
    const newArtifact = await treeState(page, 'artifacts');
    check(newTask.invalid === 'false' && newArtifact.invalid === 'false', 'both replacement domains reconstruct current valid data');
    proveRelations(newTask, check); proveRelations(newArtifact, check);
    await task(page, 'beta').click();
    await page.locator('.inspector-tree-details').waitFor({ state: 'visible' });
    check((await page.locator('.inspector-tree-details').textContent()).includes('beta'), 'fresh native input activates the replacement owner');
    await page.keyboard.press('Control+0');
    check(await page.locator('.inspector-lens-pane').isVisible() === false, 'Inspector remains optional after both domains retire/rebuild');
    entry.retired = await page.evaluate(() => ({
      tasksConnected: window.__retiredTaskRoot.isConnected, artifactsConnected: window.__retiredArtifactRoot.isConnected,
      active: document.activeElement?.outerHTML,
    }));
  });
  assert.equal(report.cases.length, 4);
  assert.equal(report.writes.length, 0, 'tree read/navigation does not emit business writes');
  assert.equal(report.pageErrors.length, 0, 'no page errors');
} catch (error) {
  failed = true; report.failure = { message: String(error), stack: error.stack };
} finally {
  if (browser) try { await browser.close(); } catch (error) { report.cleanupErrors.push({ phase: 'browser-close', message: String(error) }); }
  for (const socket of knownSockets) socket.destroy();
  await new Promise((resolve) => server.close(resolve));
  report.sourceAfter = await snapshot();
  report.sourceStable = JSON.stringify(report.sourceAfter) === JSON.stringify(sourceBefore);
  report.assetStable = hash(await readFile(path.join(assets, 'build-manifest.json'))) === manifestHash;
  for (const [name, artifact] of Object.entries(manifest.artifacts))
    report.assetStable &&= hash(await readFile(path.join(assets, name))) === artifact.sha256;
  report.finishedAt = new Date().toISOString();
  report.passed = !failed && report.sourceStable && report.assetStable && report.cleanupErrors.length === 0;
  await writeFile(path.join(evidence, 'report.json'), JSON.stringify(report, null, 2) + '\n');
  console.log(JSON.stringify({ passed: report.passed, cases: report.cases.map(({ name, passed }) => ({ name, passed })),
    pageErrors: report.pageErrors.length, cleanupErrors: report.cleanupErrors.length, sourceStable: report.sourceStable }));
  if (!report.passed) process.exitCode = 1;
}
