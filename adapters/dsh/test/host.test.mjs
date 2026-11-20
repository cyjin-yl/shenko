import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { createDshHost, DshCompatibilityError, UnknownOutcomeError } from '../src/host.mjs';
import { state as contracts } from './fixtures/contracts.mjs';
import { state as lifecycle } from './fixtures/lifecycle.mjs';

function profile(file, config = {}, settings = {}) {
  return {
    schemaVersion: 1, callTimeoutMs: 1000, shutdownGraceMs: 100,
    plugins: [{ id: 'fixture', entry: fileURLToPath(new URL(`./fixtures/${file}`, import.meta.url)), config }],
    ...settings,
  };
}

test('native schemas, guards, normalization and finalizers remain authoritative', async () => {
  const host = await createDshHost(profile('contracts.mjs'));
  try {
    const schema = host.listTools().find(({ name }) => name === 'double');
    assert.deepEqual(schema.outputSchema.properties.value, { type: 'number' });
    assert.deepEqual(schema.inputSchema.required, ['number']);
    const result = await host.callTool('double', { number: 4 });
    assert.deepEqual(result.structuredContent, { value: 8 });
    assert.deepEqual(result.content, [{ type: 'text', text: 'answer 8' }]);
    assert.equal((await host.callTool('double', { number: 'invalid' })).isError, true);
    assert.equal((await host.callTool('double', { number: 13 })).isError, true);
    assert.equal((await host.callTool('bad_output')).isError, true);
    assert.equal(contracts.calls, 1);
    assert.deepEqual(contracts.observations, [false, true, true, true]);
  } finally {
    assert.deepEqual(await host.close(), { drained: true });
  }
  assert.equal(contracts.disposed, 1);
  assert.throws(() => host.listTools(), /closed/);
});

test('missing services and conditional tools fail with explicit compatibility diagnostics', async () => {
  await assert.rejects(createDshHost(profile('missing-service.mjs')), (error) => {
    assert.ok(error instanceof DshCompatibilityError);
    assert.match(error.message, /agents, jobs/);
    return true;
  });
  await assert.rejects(createDshHost(profile('unsupported.mjs')), /step-time Agent admission/);
});

test('cancellation reaches native work and never becomes an ordinary failed tool result', async () => {
  const host = await createDshHost(profile('lifecycle.mjs', { cooperative: true }));
  let dispatches = 0;
  const started = new Promise((resolve) => { lifecycle.entered = () => { dispatches += 1; resolve(); }; });
  const cancel = new AbortController();
  const call = host.callTool('wait', {}, { signal: cancel.signal });
  await started;
  const queued = host.callTool('wait');
  const queuedRejected = assert.rejects(queued, UnknownOutcomeError);
  cancel.abort();
  await assert.rejects(call, UnknownOutcomeError);
  await queuedRejected;
  await assert.rejects(host.callTool('wait'), UnknownOutcomeError);
  assert.deepEqual(await host.close(), { drained: true });
  assert.equal(lifecycle.aborted, 1);
  assert.equal(lifecycle.disposed, 1);
  assert.equal(dispatches, 1);
});

test('timeout poisons an uncooperative host and reports bounded cleanup honestly', async () => {
  const host = await createDshHost(profile('lifecycle.mjs', { cooperative: false }, { callTimeoutMs: 25, shutdownGraceMs: 25 }));
  try {
    await assert.rejects(host.callTool('wait'), UnknownOutcomeError);
    assert.deepEqual(await host.close(), { drained: false });
    assert.throws(() => host.listTools(), UnknownOutcomeError);
  } finally {
    // Release the deliberately uncooperative fixture so the test itself owns
    // no abandoned work. The earlier cleanup observation remains false.
    lifecycle.release();
  }
});
