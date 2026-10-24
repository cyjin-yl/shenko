// 浏览器交互回归：模拟网络/输出只验证呈现，不证明持久协议或真实主体运行。
// 使用外部安装的 playwright-core，不修改产品依赖锁。
// SHENKO_PLAYWRIGHT_MODULE=/绝对路径/playwright-core SHENKO_CHROMIUM=/绝对路径/chrome node --test 本文件
const { test } = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { createServer } = require('node:http');
const { join } = require('node:path');
const { chromium } = require(process.env.SHENKO_PLAYWRIGHT_MODULE || 'playwright-core');

const html = readFileSync(join(__dirname, '../src/index.html'));

test('demo 隔离 live 网络与状态，正常模式只显示可观测事实', async () => {
  const server = createServer((req, res) => {
    res.setHeader('Content-Type', 'text/html; charset=utf-8');
    res.end(html);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try {
    browser = await chromium.launch({
      headless: true,
      executablePath: process.env.SHENKO_CHROMIUM || '/opt/google/chrome/chrome',
    });
    const context = await browser.newContext({ viewport: { width: 1440, height: 1000 } });
    const page = await context.newPage();
    const errors = [];
    page.on('pageerror', err => errors.push(err.message));
    await page.addInitScript(() => {
      window.probe = { fetches: [], sockets: [], sent: [] };
      window.fetch = async path => {
        probe.fetches.push(path);
        const data = path === '/api/self'
          ? { step: 811, execution_status: null, devices: [{ id: 'local', label: '服务端节点', online: null }], artifacts: [] }
          : path === '/api/memory'
            ? { available: false, error: 'STORE_READ_FAILED', items: [] }
            : path === '/api/tasks'
              ? { available: true, items: [] }
              : {};
        return { ok: true, json: async () => data };
      };
      window.WebSocket = class {
        static OPEN = 1;
        constructor() {
          this.readyState = 1;
          probe.sockets.push(this);
          queueMicrotask(() => { if (this.onopen) this.onopen(); });
        }
        send(text) { probe.sent.push(JSON.parse(text)); }
        close() { this.readyState = 3; if (this.onclose) this.onclose(); }
      };
    });
    const base = `http://127.0.0.1:${server.address().port}`;
    await page.goto(base + '/?demo=1');
    assert.equal(await page.locator('[data-demo="1"]').count(), 3);
    await page.locator('#avatar').click();
    assert.match(await page.locator('#p-activity').textContent(), /模拟模式不记录真实活动/);
    assert.match(await page.locator('#status-step').textContent(), /模拟样本/);
    assert.equal(await page.locator('#compose').isDisabled(), true);
    assert.equal(await page.locator('#send').isDisabled(), true);
    await page.locator('#panel-close').click();
    await page.locator('[data-view="tasks"]').click();
    await page.locator('[data-view="memory"]').click();
    await page.locator('[data-view="config"]').click();
    assert.deepEqual(await page.evaluate(() => [probe.fetches.length, probe.sockets.length, probe.sent.length]), [0, 0, 0]);
    if (process.env.SHENKO_SCREENSHOT_DIR) {
      await page.locator('[data-view="timeline"]').click();
      await page.screenshot({ path: join(process.env.SHENKO_SCREENSHOT_DIR, 'demo-isolated.png') });
    }

    await page.goto(base + '/');
    await page.waitForFunction(() => document.getElementById('status-step').textContent === '最后观测 #811');
    assert.equal(await page.locator('[data-demo="1"]').count(), 0);
    assert.equal(await page.evaluate(() => probe.sockets.length), 1);
    await page.evaluate(() => {
      const emit = data => probe.sockets[0].onmessage({ data: JSON.stringify(data) });
      const out = { role: 'subject', content: '真实来源编号的模拟传输', message_id: 'outbox:fixture-812', step: 812, committed_at: null, observed_at: '2026-10-24T00:00:00Z' };
      emit(out);
      emit(out);
      emit({ role: 'subject', content: '没有编号的旧输出', message_id: 'legacy-no-step' });
    });
    assert.equal(await page.locator('[data-kind="thought"]').count(), 2);
    assert.equal(await page.locator('#status-step').textContent(), '最后观测 #812');
    assert.equal(await page.locator('[data-kind="thought"] .step').last().textContent(), '轮次未知');
    assert.equal(await page.locator('[data-kind="thought"] time').count(), 0);
    await page.locator('#mode').click();
    assert.equal(await page.locator('[data-mode="steer"]').isDisabled(), true);
    await page.locator('#mode').click();
    await page.locator('#compose').fill('追加输入');
    await page.locator('#send').click();
    const payload = await page.evaluate(() => probe.sent[0]);
    assert.equal(payload.mode, 'append');
    assert.ok(payload.submission_id);
    await page.evaluate(id => probe.sockets[0].onmessage({ data: JSON.stringify({
      type: 'receipt', role: 'system', stage: 'rejected', error: 'INBOX_FULL', submission_id: id,
    }) }), payload.submission_id);
    assert.match(await page.locator('.receipt').textContent(), /未入队.*内存队列已满/);
    assert.equal(await page.locator('[data-kind="thought"]').count(), 2);
    await page.locator('#avatar').click();
    assert.match(await page.locator('#panel-status').textContent(), /当前执行未知/);
    assert.match(await page.locator('#p-devices').textContent(), /服务端节点.*存活未知/);
    assert.equal(await page.locator('#panel [data-online="true"]').count(), 0);
    await page.locator('#panel-close').click();
    await page.locator('[data-view="memory"]').click();
    await page.waitForFunction(() => document.getElementById('memory-list').textContent.includes('读取失败'));
    assert.doesNotMatch(await page.locator('#memory-list').textContent(), /暂无记忆/);
    assert.deepEqual(errors, []);
    await context.close();
  } finally {
    if (browser) await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
});
