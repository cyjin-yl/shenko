import assert from 'node:assert/strict';
import { mkdtemp, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, isAbsolute } from 'node:path';
import { test } from 'node:test';
import { readConfig, connectionProfile } from '../src/config.mjs';

test('profile generation is inert, uses absolute paths and pins config bytes without copying credentials', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-config-'));
  try {
    const path = join(dir, 'bridge.json');
    await writeFile(path, JSON.stringify({
      schemaVersion: 1,
      plugins: [{ id: 'offline', entry: './missing-on-purpose.mjs' }],
      envRefs: { PATH: 'PATH', TOOL_TOKEN: 'SHENKO_DSH_TEST_TOKEN' },
    }));
    process.env.SHENKO_DSH_TEST_TOKEN = 'must-not-appear';
    const loaded = await readConfig(path);
    const profile = connectionProfile(loaded);
    assert.equal(loaded.config.plugins[0].entry, join(dir, 'missing-on-purpose.mjs'));
    assert.ok(isAbsolute(profile.connection.program));
    assert.ok(isAbsolute(profile.connection.args[0]));
    assert.deepEqual(profile.connection.args.slice(-2), ['--sha256', loaded.sha256]);
    assert.equal(profile.connection.env_refs.TOOL_TOKEN, 'SHENKO_DSH_TEST_TOKEN');
    assert.equal(JSON.stringify(profile).includes('must-not-appear'), false);
    await readConfig(path, loaded.sha256);
    await writeFile(path, '\n', { flag: 'a' });
    await assert.rejects(readConfig(path, loaded.sha256), /changed since profile generation/);
  } finally {
    delete process.env.SHENKO_DSH_TEST_TOKEN;
    await rm(dir, { recursive: true, force: true });
  }
});

test('config rejects ambiguous plugins and literal environment values', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-invalid-'));
  const path = join(dir, 'bridge.json');
  const base = { schemaVersion: 1, plugins: [{ id: 'test', entry: './test.mjs' }] };
  try {
    await writeFile(path, JSON.stringify({ ...base, plugins: [base.plugins[0], base.plugins[0]] }));
    await assert.rejects(readConfig(path), /Duplicate plugin/);
    await writeFile(path, JSON.stringify({ ...base, envRefs: { TOKEN: 'literal-secret-value' } }));
    await assert.rejects(readConfig(path), /environment variable names/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
