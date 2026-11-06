/**
 * Build-time Proto-UI compiler seam.
 *
 * The reviewed Proto-UI pin (web/upstream-pin.json) carries a *private*
 * `@proto.ui/compiler` package that exports only `./src/index.ts`: it has no
 * built `dist`, it is not part of the pinned `scripts/build/public-packages.mjs`
 * selection, and no published compiler profile exists. Shenko therefore cannot
 * import the compiler through a package artifact and must not pretend a
 * runtime compiler exists.
 *
 * What this module does instead:
 *
 * 1. audits the pinned compiler sources it is about to consume (existence,
 *    package identity, exported entry, source-tree digest);
 * 2. bundles those sources once with the workspace's pinned esbuild into one
 *    Node ESM file, so the compiler's TypeScript module graph is executable
 *    without adding a `dist` to the reviewed upstream checkout;
 * 3. runs `compileFile` on Shenko-authored Prototypes and publishes each
 *    generation create-only into `generated/prototypes/<name>/` together with
 *    the compiler's own `provenance.json`;
 * 4. returns the exact facts to record in the build manifest.
 *
 * The browser bundle imports only the generated React DOM source, so the
 * compiler is build-time only (`runtime_imported: false`).
 */
import { createHash } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { pathToFileURL } from 'node:url';
import { build as esbuild } from 'esbuild';

export const COMPILER_PACKAGE = 'packages/compiler';
export const COMPILER_ENTRY = 'packages/compiler/src/index.ts';
export const COMPILER_PROFILE = 'react-dom-source-v1';
export const COMPILER_BUNDLE_RELATIVE = 'generated/compiler/proto-ui-compiler.mjs';
export const PROTOTYPE_SOURCE_ROOT = 'prototypes';
export const PROTOTYPE_OUTPUT_RELATIVE = 'generated/prototypes';
/**
 * `typescript` stays external: the compiler declares `typescript: ^5.0.0` and
 * the workspace's reviewed devDependency satisfies it, so the bundle does not
 * embed a second TypeScript copy. Node resolves it from web/node_modules.
 */
export const COMPILER_EXTERNALS = ['typescript'];
/**
 * `packages/compiler/src/native-adapter-modules.ts` imports `@floating-ui/dom`
 * but the compiler manifest does not declare it; the workspace installs it for
 * `packages/modules/positioning`. The resolution is explicit and recorded
 * instead of hoisting an unowned copy into the reviewed checkout.
 */
export const COMPILER_RESOLUTIONS = [
  { specifier: '@floating-ui/dom', owner: 'packages/modules/positioning' },
];

/** Shenko-authored Prototypes actually compiled and consumed by this UI. */
export const SHENKO_PROTOTYPES = [
  { name: 'transport-badge', entry: 'transport-badge.proto.ts', componentName: 'CompiledTransportBadge' },
];

export function sha256File(path) {
  return createHash('sha256').update(readFileSync(path)).digest('hex');
}

function listFiles(directory) {
  const files = [];
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) files.push(...listFiles(path));
    else if (entry.isFile()) files.push(path);
  }
  return files;
}

/** Deterministic digest over sorted relative names and their bytes. */
export function sha256Tree(directory) {
  const digest = createHash('sha256');
  for (const file of listFiles(directory).sort()) {
    digest.update(`${relative(directory, file).split('\\').join('/')}\0${sha256File(file)}\n`);
  }
  return digest.digest('hex');
}

function readJson(path) {
  return JSON.parse(readFileSync(path, 'utf8'));
}

/**
 * Audit the exact reviewed inputs. A drift here is a hard build failure: the
 * seam is only honest while these files are the ones that were reviewed.
 */
export function auditCompilerInputs(sourceRoot) {
  for (const file of [COMPILER_ENTRY, `${COMPILER_PACKAGE}/package.json`]) {
    if (!existsSync(join(sourceRoot, file))) {
      throw new Error(`Missing audited compiler input: ${file}`);
    }
  }
  const manifest = readJson(join(sourceRoot, COMPILER_PACKAGE, 'package.json'));
  if (manifest.name !== '@proto.ui/compiler') {
    throw new Error(`Pinned compiler package is ${manifest.name}, expected @proto.ui/compiler`);
  }
  const exported = manifest.exports?.['.'];
  const defaultTarget = typeof exported === 'string' ? exported : exported?.default;
  if (defaultTarget !== './src/index.ts') {
    throw new Error(
      `Pinned compiler no longer exports only ./src/index.ts (found ${defaultTarget}); revisit the build-time seam`,
    );
  }
  if (manifest.private !== true) {
    throw new Error('Pinned compiler is no longer private; a published artifact must replace this source seam');
  }
  const resolutions = COMPILER_RESOLUTIONS.map((resolution) => {
    const directory = join(sourceRoot, resolution.owner, 'node_modules', resolution.specifier);
    const path = join(directory, 'package.json');
    if (!existsSync(path)) {
      throw new Error(
        `Cannot resolve ${resolution.specifier} for the compiler; ${resolution.owner} has no installed copy`,
      );
    }
    return {
      specifier: resolution.specifier,
      owner: resolution.owner,
      version: readJson(path).version,
      package_sha256: sha256File(path),
    };
  });
  return {
    package: COMPILER_PACKAGE,
    package_sha256: sha256File(join(sourceRoot, `${COMPILER_PACKAGE}/package.json`)),
    entry: COMPILER_ENTRY,
    entry_sha256: sha256File(join(sourceRoot, COMPILER_ENTRY)),
    source_tree_sha256: sha256Tree(join(sourceRoot, COMPILER_PACKAGE, 'src')),
    resolutions,
  };
}

/**
 * Bundle the audited compiler sources for Node. This is a build step, not a
 * substitute implementation: the emitted file contains the pinned TypeScript
 * modules unchanged apart from bundling.
 */
export async function writeCompilerBundle({ sourceRoot, webRoot }) {
  const outfile = join(webRoot, COMPILER_BUNDLE_RELATIVE);
  mkdirSync(dirname(outfile), { recursive: true });
  await esbuild({
    absWorkingDir: sourceRoot,
    entryPoints: [join(sourceRoot, COMPILER_ENTRY)],
    outfile,
    bundle: true,
    format: 'esm',
    platform: 'node',
    target: ['node22'],
    external: COMPILER_EXTERNALS,
    nodePaths: COMPILER_RESOLUTIONS.map((resolution) =>
      join(sourceRoot, resolution.owner, 'node_modules'),
    ),
    legalComments: 'eof',
    sourcemap: false,
    logLevel: 'warning',
  });
  return {
    path: COMPILER_BUNDLE_RELATIVE,
    sha256: sha256File(outfile),
    bytes: statSync(outfile).size,
    format: 'esm',
    entry: COMPILER_ENTRY,
    external: [...COMPILER_EXTERNALS],
  };
}

export async function loadCompiler({ sourceRoot, webRoot }) {
  const inputs = auditCompilerInputs(sourceRoot);
  const bundle = await writeCompilerBundle({ sourceRoot, webRoot });
  const loaded = await import(pathToFileURL(join(webRoot, COMPILER_BUNDLE_RELATIVE)).href);
  for (const name of ['compileFile', 'writeCompilation', 'formatCompilerDiagnostics']) {
    if (typeof loaded[name] !== 'function') {
      throw new Error(`Bundled compiler does not expose ${name}()`);
    }
  }
  const typescriptPath = join(webRoot, 'node_modules', 'typescript', 'package.json');
  if (!existsSync(typescriptPath)) {
    throw new Error('web/node_modules/typescript is required by the externalized compiler dependency');
  }
  return {
    compiler: loaded,
    facts: {
      ...inputs,
      typescript: readJson(typescriptPath).version,
      bundle,
    },
  };
}

function bareSpecifiersIn(source) {
  const specifiers = new Set();
  for (const match of source.matchAll(/\bfrom\s+'([^']+)'/g)) {
    if (!match[1].startsWith('.')) specifiers.add(match[1]);
  }
  for (const match of source.matchAll(/\bfrom\s+"([^"]+)"/g)) {
    if (!match[1].startsWith('.')) specifiers.add(match[1]);
  }
  return [...specifiers].sort();
}

/**
 * Compile every Shenko-authored Prototype from a closed in-memory source
 * snapshot (the bytes that were hashed are the bytes the compiler parsed) and
 * publish each generation create-only.
 */
export async function compileShenkoPrototypes({ sourceRoot, webRoot, pin }) {
  const declared = pin.compiler?.prototypes;
  const names = SHENKO_PROTOTYPES.map((prototype) => prototype.name);
  if (JSON.stringify(declared) !== JSON.stringify(names)) {
    throw new Error(
      `upstream-pin.json compiler.prototypes ${JSON.stringify(declared)} does not match the compiled set ${JSON.stringify(names)}`,
    );
  }
  if (pin.compiler?.status !== 'build_time_compiled' || pin.compiler?.runtime_imported !== false) {
    throw new Error('upstream-pin.json must declare compiler.status build_time_compiled and runtime_imported false');
  }
  if (pin.compiler?.profile !== COMPILER_PROFILE) {
    throw new Error(`upstream-pin.json compiler.profile must be ${COMPILER_PROFILE}`);
  }
  const { compiler, facts } = await loadCompiler({ sourceRoot, webRoot });
  const sourceDirectory = join(webRoot, PROTOTYPE_SOURCE_ROOT);
  const outputRoot = join(webRoot, PROTOTYPE_OUTPUT_RELATIVE);
  rmSync(outputRoot, { recursive: true, force: true });
  mkdirSync(outputRoot, { recursive: true });

  const prototypes = [];
  for (const prototype of SHENKO_PROTOTYPES) {
    const entryPath = join(sourceDirectory, prototype.entry);
    if (!existsSync(entryPath)) {
      throw new Error(`Missing Shenko prototype source: ${relative(webRoot, entryPath)}`);
    }
    const source = readFileSync(entryPath, 'utf8');
    const entrySha256 = createHash('sha256').update(source).digest('hex');
    const compiled = await compiler.compileFile(prototype.entry, {
      sourceSnapshot: { entry: prototype.entry, files: { [prototype.entry]: source } },
      exportName: 'default',
      profile: COMPILER_PROFILE,
      componentName: prototype.componentName,
    });
    if (!compiled.ok) {
      throw new Error(
        `Prototype ${prototype.name} was rejected by the pinned compiler:\n${compiler.formatCompilerDiagnostics(compiled.diagnostics)}`,
      );
    }
    const target = join(outputRoot, prototype.name);
    const written = await compiler.writeCompilation(compiled.value, target);
    if (!written.ok) {
      throw new Error(
        `Prototype ${prototype.name} output was not published:\n${compiler.formatCompilerDiagnostics(written.diagnostics)}`,
      );
    }
    const generatedSource = join(target, 'Component.tsx');
    const generated = readFileSync(generatedSource, 'utf8');
    if (!generated.includes(`export const ${prototype.componentName} =`)) {
      throw new Error(
        `Prototype ${prototype.name} did not emit ${prototype.componentName}; consumed source would be stale`,
      );
    }
    const generatedImports = bareSpecifiersIn(generated);
    const forbidden = generatedImports.filter((specifier) => specifier.startsWith('@proto.ui/'));
    if (forbidden.length > 0) {
      throw new Error(
        `Prototype ${prototype.name} generated code still imports ${forbidden.join(', ')}; the seam is not native-source lowering`,
      );
    }
    const provenancePath = join(target, 'provenance.json');
    const provenance = readJson(provenancePath);
    prototypes.push({
      name: prototype.name,
      entry: `${PROTOTYPE_SOURCE_ROOT}/${prototype.entry}`,
      entry_sha256: entrySha256,
      profile: COMPILER_PROFILE,
      component_name: prototype.componentName,
      output_directory: `${PROTOTYPE_OUTPUT_RELATIVE}/${prototype.name}`,
      generated_source: `${PROTOTYPE_OUTPUT_RELATIVE}/${prototype.name}/Component.tsx`,
      generated_sha256: createHash('sha256').update(generated).digest('hex'),
      generated_imports: generatedImports,
      provenance: `${PROTOTYPE_OUTPUT_RELATIVE}/${prototype.name}/provenance.json`,
      provenance_sha256: sha256File(provenancePath),
      compiler_ir_version: provenance.irVersion,
      compiler_backend: provenance.backend,
      compiler_source_sha256: provenance.source?.sha256,
    });
  }
  const record = {
    schema_version: 1,
    status: 'build_time_compiled',
    runtime_imported: false,
    profile: COMPILER_PROFILE,
    ...facts,
    prototype_output: PROTOTYPE_OUTPUT_RELATIVE,
    prototypes,
  };
  writeFileSync(join(outputRoot, 'manifest.json'), `${JSON.stringify(record, null, 2)}\n`);
  return record;
}
