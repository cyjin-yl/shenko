#!/usr/bin/env node
import { Console } from 'node:console';
import { readConfig, connectionProfile } from './config.mjs';

const writeJson = value => process.stdout.write(`${JSON.stringify(value, null, 2)}\n`);
const usage = 'Usage: node adapters/dsh/src/cli.mjs <check|profile|serve> CONFIG.json [--sha256 DIGEST]';

async function main() {
  const [command, filename, flag, digest, ...extra] = process.argv.slice(2);
  if (command === '--help' || command === '-h') {
    process.stdout.write(`${usage}\n\nprofile only reads config; check and serve execute the selected local plugins.\n`);
    return;
  }
  if (!['check', 'profile', 'serve'].includes(command) || !filename || extra.length
    || (flag !== undefined && (flag !== '--sha256' || !/^[a-f0-9]{64}$/.test(digest ?? '')))) {
    throw new Error(usage);
  }
  const loaded = await readConfig(filename, digest);
  if (command === 'profile') {
    writeJson(connectionProfile(loaded));
    return;
  }
  // Third-party console output must not corrupt stdio MCP or the check receipt.
  // Direct process.stdout writes remain a plugin compatibility constraint.
  globalThis.console = new Console({ stdout: process.stderr, stderr: process.stderr });
  if (command === 'serve') {
    const { serve } = await import('./server.mjs');
    await serve(loaded.config);
    return;
  }
  const { createDshHost } = await import('./host.mjs');
  const host = await createDshHost(loaded.config);
  let tools;
  try {
    tools = await host.listTools();
  } finally {
    const cleanup = await host.close();
    if (!cleanup.drained) throw new Error('DSH plugin cleanup did not finish within its grace window');
  }
  writeJson({
    status: 'ready',
    scope: 'standalone-tools',
    configSha256: loaded.sha256,
    plugins: loaded.config.plugins.map(({ id, entry }) => ({ id, entry })),
    tools,
    // Mounting and listing do not prove a tool's business effect or model use.
    verified: ['plugin-mount', 'tool-catalog', 'plugin-disposal'],
  });
}

main().catch(error => {
  // The host has already attempted bounded cleanup. A plugin can own an
  // uncooperative timer or task, so exitCode alone cannot finish this process.
  process.stderr.write(`${JSON.stringify({ status: 'error', message: error.message })}\n`, () => process.exit(1));
});
