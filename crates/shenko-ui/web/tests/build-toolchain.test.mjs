import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

test('the real producer rejects a different Node pin before dependencies or generated writes', (context) => {
  const root = mkdtempSync(join(tmpdir(), 'shenko-build-toolchain-'));
  context.after(() => rmSync(root, { recursive: true, force: true }));
  const source = join(root, 'source');
  const web = join(source, 'web');
  const generated = join(web, 'generated');
  const staticDir = join(source, 'src', 'static');
  mkdirSync(generated, { recursive: true });
  mkdirSync(staticDir, { recursive: true });
  copyFileSync(new URL('../build.mjs', import.meta.url), join(web, 'build.mjs'));
  writeFileSync(join(source, '.node-version'), '0.0.0\n');
  const oldOutputs = [
    [join(generated, 'previous.tsx'), 'export const retained = true;\n'],
    [join(staticDir, 'build-manifest.json'), '{"retained":true}\n'],
  ];
  for (const [path, bytes] of oldOutputs) writeFileSync(path, bytes);
  const before = readdirSync(source, { recursive: true }).sort();

  // The snapshot has no compiler or installed third-party modules. A useful
  // refusal must identify the runtime first, before those imports are tried.
  const result = spawnSync(process.execPath, [join(web, 'build.mjs')], {
    cwd: web,
    encoding: 'utf8',
    timeout: 5000,
  });
  assert.equal(result.error, undefined);
  assert.equal(result.status, 1);
  assert.match(result.stderr, /Node v0\.0\.0 required by .*\.node-version; running v\d+\.\d+\.\d+/);
  assert.doesNotMatch(result.stderr, /ERR_MODULE_NOT_FOUND|ENOENT/);
  assert.equal(result.stdout, '');
  assert.deepEqual(readdirSync(source, { recursive: true }).sort(), before);
  for (const [path, bytes] of oldOutputs) assert.equal(readFileSync(path, 'utf8'), bytes);
});
