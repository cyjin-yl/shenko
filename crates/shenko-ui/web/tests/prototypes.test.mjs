/**
 * Consumer of the build-time Proto-UI compiler output.
 *
 * This test does not re-run the compiler and does not render: it reads the
 * generation the build actually published and checks the chain the compiler
 * itself recorded — authored bytes → parsed source → generated React DOM source
 * → on-disk artifacts. It fails loudly when the generation is absent instead of
 * skipping, because a missing generation means the UI's import cannot resolve.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const webRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const generatedRoot = join(webRoot, 'generated', 'prototypes');
const PREREQUISITE =
  'generated prototypes are absent: run `node build.mjs --prototypes-only` in crates/shenko-ui/web with PROTO_UI_SOURCE naming the reviewed Proto-UI checkout';

const sha256 = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');

function generation() {
  const manifestPath = join(generatedRoot, 'manifest.json');
  assert.ok(existsSync(manifestPath), PREREQUISITE);
  return JSON.parse(readFileSync(manifestPath, 'utf8'));
}

test('every declared Shenko prototype was compiled from the authored source bytes', () => {
  const manifest = generation();
  assert.equal(manifest.status, 'build_time_compiled');
  assert.equal(manifest.runtime_imported, false);
  assert.equal(manifest.profile, 'react-dom-source-v1');
  assert.deepEqual(manifest.prototypes.map((prototype) => prototype.name), ['transport-badge']);
  for (const prototype of manifest.prototypes) {
    assert.equal(sha256(join(webRoot, prototype.entry)), prototype.entry_sha256);
    assert.equal(sha256(join(webRoot, prototype.generated_source)), prototype.generated_sha256);
  }
});

test('generated code is native React DOM source, not a Proto-UI runtime or adapter import', () => {
  for (const prototype of generation().prototypes) {
    const generated = readFileSync(join(webRoot, prototype.generated_source), 'utf8');
    // The generated file's OWN import statements are what the browser bundle
    // sees. The manifest's list is advisory: its scanner reads import-like
    // text, so a string literal can show up there as a "specifier" that the
    // bundle never resolves.
    const specifiers = [...generated.matchAll(/from ['"]([^'"]+)['"]/g)].map((match) => match[1]);
    assert.ok(specifiers.length > 0, 'the generated file imports something');
    for (const specifier of specifiers) {
      assert.ok(!specifier.startsWith('@proto.ui/'), `no Proto-UI package import: ${specifier}`);
      assert.ok(!specifier.includes('adapter-react'), `no adapter import: ${specifier}`);
    }
    assert.ok(specifiers.includes('react'), 'the generated code is React DOM source');
    assert.ok(
      !prototype.generated_imports.some((specifier) => specifier.startsWith('@proto.ui/')),
      'the manifest names no Proto-UI runtime either',
    );
    assert.ok(generated.includes(`export const ${prototype.component_name} =`));
    assert.ok(generated.includes('Profile: react-dom-source-v1'));
  }
});

test('compiler provenance binds the generation to the parsed source graph and artifacts', () => {
  for (const prototype of generation().prototypes) {
    const provenance = JSON.parse(readFileSync(join(webRoot, prototype.provenance), 'utf8'));
    assert.equal(provenance.backend, prototype.profile);
    assert.equal(provenance.source.exportName, 'default');
    assert.deepEqual(provenance.sourceFiles, [
      { file: prototype.entry.slice('prototypes/'.length), sha256: prototype.entry_sha256 },
    ]);
    assert.equal(provenance.generatedSha256, prototype.generated_sha256);
    const directory = join(webRoot, prototype.output_directory);
    for (const artifact of provenance.artifacts) {
      assert.equal(sha256(join(directory, artifact.path)), artifact.sha256, artifact.path);
    }
  }
});

test('the recorded compiler identity is the audited source package, bundled for the build only', () => {
  const manifest = generation();
  assert.match(manifest.bundle.sha256, /^[0-9a-f]{64}$/);
  assert.deepEqual(manifest.bundle.external, ['typescript']);
  assert.equal(manifest.package, 'packages/compiler');
  assert.equal(manifest.entry, 'packages/compiler/src/index.ts');
  assert.match(manifest.source_tree_sha256, /^[0-9a-f]{64}$/);
  assert.ok(manifest.typescript.length > 0);
  assert.ok(manifest.resolutions.some((resolution) => resolution.specifier === '@floating-ui/dom'));
});
