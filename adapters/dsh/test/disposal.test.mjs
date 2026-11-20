import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

test('check observes real Cordis disposer failure and timeout without a ready receipt', async t => {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-disposal-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  for (const mode of ['success', 'throw', 'reject', 'falsy', 'aggregate-empty', 'timeout']) {
    await t.test(mode, async () => {
      const witness = join(dir, `${mode}.txt`);
      const config = join(dir, `${mode}.json`);
      await writeFile(config, JSON.stringify({
        schemaVersion: 1,
        callTimeoutMs: 3000,
        shutdownGraceMs: 1000,
        plugins: [{
          id: 'disposal',
          entry: fileURLToPath(new URL('./fixtures/disposal.mjs', import.meta.url)),
          config: { mode, witness },
        }],
      }));
      const result = await new Promise(resolve => {
        execFile(process.execPath, [fileURLToPath(new URL('../src/cli.mjs', import.meta.url)), 'check', config],
          { timeout: 10_000 }, (error, stdout, stderr) => resolve({ error, stdout, stderr }));
      });
      if (mode === 'success') {
        assert.equal(result.error, null);
        const receipt = JSON.parse(result.stdout);
        assert.equal(receipt.status, 'ready');
        assert.ok(receipt.verified.includes('plugin-disposal'));
        assert.deepEqual(receipt.tools.map(tool => tool.name), ['disposal_probe']);
        assert.equal(await readFile(witness, 'utf8'), 'dispose-started\ndispose-finished\n');
      } else {
        assert.equal(result.error?.code, 1);
        assert.equal(result.error.killed, false);
        assert.equal(result.stdout, '');
        const receipt = JSON.parse(result.stderr.trim().split('\n').at(-1));
        assert.equal(receipt.status, 'error');
        assert.match(receipt.message, mode === 'timeout' ? /cleanup did not finish/ : /cleanup reported an error/);
        assert.equal(await readFile(witness, 'utf8'), 'dispose-started\n');
      }
    });
  }
});
