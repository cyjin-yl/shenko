import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import { dirname, extname, isAbsolute, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const CONFIG_KEYS = new Set([
  'schemaVersion', 'plugins', 'envRefs', 'callTimeoutMs', 'shutdownGraceMs',
]);
const PLUGIN_KEYS = new Set(['id', 'entry', 'config']);
const ENV_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;

function object(value, label) {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${label} must be a JSON object`);
  }
  return value;
}

function onlyKeys(value, allowed, label) {
  for (const key of Object.keys(value)) {
    if (!allowed.has(key)) throw new Error(`Unknown ${label} field: ${key}`);
  }
}

function timeout(value, fallback, label, max = 2_147_483_647) {
  const result = value ?? fallback;
  if (!Number.isSafeInteger(result) || result < 1 || result > max) {
    throw new Error(`${label} must be an integer between 1 and ${max}`);
  }
  return result;
}

/** Parse configuration only. This neither imports plugins nor resolves secrets. */
export async function readConfig(filename, expectedSha256) {
  const configPath = resolve(filename);
  const bytes = await readFile(configPath);
  if (bytes.length > 1024 * 1024) throw new Error('Bridge config exceeds 1 MiB');
  const sha256 = createHash('sha256').update(bytes).digest('hex');
  if (expectedSha256 !== undefined && expectedSha256 !== sha256) {
    throw new Error('Bridge config changed since profile generation; generate and import a new profile');
  }
  const input = object(JSON.parse(bytes.toString('utf8')), 'Bridge config');
  onlyKeys(input, CONFIG_KEYS, 'bridge config');
  if (input.schemaVersion !== 1) throw new Error('Bridge config schemaVersion must be 1');
  if (!Array.isArray(input.plugins) || input.plugins.length === 0) {
    throw new Error('Bridge config plugins must be a nonempty array');
  }
  const ids = new Set();
  const entries = new Set();
  const plugins = input.plugins.map((value, index) => {
    const plugin = object(value, `plugins[${index}]`);
    onlyKeys(plugin, PLUGIN_KEYS, `plugins[${index}]`);
    if (typeof plugin.id !== 'string' || !/^[a-zA-Z0-9][a-zA-Z0-9._-]*$/.test(plugin.id)) {
      throw new Error(`plugins[${index}].id must be a nonempty portable identifier`);
    }
    if (ids.has(plugin.id)) throw new Error(`Duplicate plugin id: ${plugin.id}`);
    ids.add(plugin.id);
    if (typeof plugin.entry !== 'string' || plugin.entry.trim() === '' || plugin.entry.includes('\0')) {
      throw new Error(`Plugin ${plugin.id} requires a local compiled JavaScript entry path`);
    }
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(plugin.entry) && !isAbsolute(plugin.entry)) {
      throw new Error(`Plugin ${plugin.id} entry must be a local path, not a URL or package specifier`);
    }
    const entry = resolve(dirname(configPath), plugin.entry);
    if (!['.js', '.mjs', '.cjs'].includes(extname(entry))) {
      throw new Error(`Plugin ${plugin.id} must point to compiled .js, .mjs or .cjs`);
    }
    if (entries.has(entry)) throw new Error(`Duplicate plugin entry: ${entry}`);
    entries.add(entry);
    return { id: plugin.id, entry, config: object(plugin.config ?? {}, `Plugin ${plugin.id} config`) };
  });
  const envRefs = object(input.envRefs ?? {}, 'envRefs');
  for (const [child, source] of Object.entries(envRefs)) {
    if (!ENV_NAME.test(child) || typeof source !== 'string' || !ENV_NAME.test(source)) {
      throw new Error('envRefs must map environment variable names to environment variable names, not values');
    }
  }
  return {
    configPath,
    sha256,
    config: {
      schemaVersion: 1,
      plugins,
      envRefs: { ...envRefs },
      callTimeoutMs: timeout(input.callTimeoutMs, 30_000, 'callTimeoutMs'),
      // Shenko's owning MCP process has its own three-second cleanup window.
      shutdownGraceMs: timeout(input.shutdownGraceMs, 1000, 'shutdownGraceMs', 2000),
    },
  };
}

/** Generate the existing Shenko MCP connection format; no new install registry. */
export function connectionProfile({ configPath, sha256, config }, nodePath = process.execPath) {
  if (!isAbsolute(nodePath)) throw new Error('Node executable path must be absolute');
  return {
    description: `DSH tool bridge: ${config.plugins.map(plugin => plugin.id).join(', ')} (standalone tools only)`,
    provenance_refs: [
      'https://github.com/deepseek-ai/deepseek-harness/tree/2e39e70496aa02986ca6a820c5d497e816b69d7e',
      `urn:sha256:${sha256}`,
    ],
    connection: {
      transport: 'stdio',
      program: nodePath,
      args: [fileURLToPath(new URL('./cli.mjs', import.meta.url)), 'serve', configPath, '--sha256', sha256],
      env_refs: config.envRefs,
    },
  };
}
