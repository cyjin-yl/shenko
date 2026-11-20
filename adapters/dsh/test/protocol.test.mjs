import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { connectionProfile, readConfig } from '../src/config.mjs';

async function fixture(t, callTimeoutMs = 3000) {
  const dir = await mkdtemp(join(tmpdir(), 'shenko-dsh-mcp-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const witness = join(dir, 'witness.txt');
  await writeFile(witness, '');
  const path = join(dir, 'bridge.json');
  await writeFile(path, JSON.stringify({
    schemaVersion: 1,
    plugins: [{
      id: 'protocol',
      entry: fileURLToPath(new URL('./fixtures/protocol-plugin.mjs', import.meta.url)),
      config: { witness },
    }],
    callTimeoutMs,
  }));
  const profile = connectionProfile(await readConfig(path));
  async function connect() {
    const transport = new StdioClientTransport({
      command: profile.connection.program,
      args: profile.connection.args,
      env: {},
      stderr: 'pipe',
    });
    let diagnostics = '';
    transport.stderr.on('data', chunk => { diagnostics += chunk; });
    const client = new Client({ name: 'shenko-dsh-protocol-test', version: '1.0.0' });
    t.after(() => client.close());
    await client.connect(transport);
    return { client, transport, diagnostics: () => diagnostics };
  }
  return { connect, witness };
}

test('generated profile completes real stdio discovery, canonical result, invalid args and Cordis cleanup', async t => {
  const { connect, witness } = await fixture(t);
  const { client, diagnostics } = await connect();
  const { tools } = await client.listTools();
  assert.deepEqual(tools.map(tool => tool.name).sort(), ['add', 'uncertain']);
  assert.equal(tools.find(tool => tool.name === 'add').inputSchema.properties.a.type, 'integer');
  const result = await client.callTool({ name: 'add', arguments: { a: 17, b: 25 } });
  assert.notEqual(result.isError, true);
  assert.deepEqual(result.content, [{ type: 'text', text: '42' }]);
  assert.equal(result.structuredContent.value, 42);
  const invalid = await client.callTool({ name: 'add', arguments: { a: '17', b: 25 } });
  assert.equal(invalid.isError, true);
  await client.close();
  assert.equal(await readFile(witness, 'utf8'), 'add\ndisposed\n');
  assert.match(diagnostics(), /plugin diagnostic belongs on stderr/);
});

test('post-effect timeout closes the session without a false terminal result or replay on restart', async t => {
  const { connect, witness } = await fixture(t, 150);
  const { client } = await connect();
  await assert.rejects(
    client.callTool({ name: 'uncertain', arguments: {} }, undefined, { timeout: 5000 }),
    /closed|unknown/i,
  );
  await client.close();
  const replacement = await connect();
  await replacement.client.listTools();
  await replacement.client.close();
  const facts = await readFile(witness, 'utf8');
  assert.equal(facts.split('\n').filter(line => line === 'effect-before-timeout').length, 1);
  assert.equal(facts.split('\n').filter(line => line === 'disposed').length, 2);
});
