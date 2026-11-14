import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  realpathSync,
  readdirSync,
  lstatSync,
  unlinkSync,
  statSync,
  symlinkSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';
import { compileShenkoPrototypes, PROTOTYPE_OUTPUT_RELATIVE } from './compiler.mjs';

const webRoot = dirname(fileURLToPath(import.meta.url));
const crateRoot = resolve(webRoot, '..');
const staticDir = join(crateRoot, 'src', 'static');
const pin = JSON.parse(readFileSync(join(webRoot, 'upstream-pin.json'), 'utf8'));
const prototypesOnly = process.argv.includes('--prototypes-only');
const upstreamRoot = process.env.PROTO_UI_SOURCE;
if (!upstreamRoot || !existsSync(upstreamRoot)) {
  throw new Error('PROTO_UI_SOURCE must name the separately checked out pinned upstream source');
}
const sourceRoot = realpathSync(upstreamRoot);
const run = (command, args, options = {}) => {
  const result = spawnSync(command, args, { encoding: 'utf8', timeout: 120000, ...options });
  if (result.status !== 0) throw new Error(result.stderr || result.stdout || `${command} failed`);
  return (result.stdout ?? "").trim();
};
const actualCommit = run('git', ['-C', sourceRoot, 'rev-parse', 'HEAD']);
if (actualCommit !== pin.commit) throw new Error(`Proto-UI is ${actualCommit}, expected ${pin.commit}`);
const sha256 = (path) => createHash('sha256').update(readFileSync(path)).digest('hex');
const actualLock = sha256(join(sourceRoot, 'pnpm-lock.yaml'));
if (actualLock !== pin.lock_sha256) throw new Error('Proto-UI pnpm-lock.yaml does not match the reviewed pin');
for (const relative of [
  'packages/adapters/react/dist/index.js',
  'packages/prototypes/shadcn/dist/button/index.js',
  'packages/prototypes/shadcn/dist/textarea/index.js',
  'packages/prototypes/shadcn/dist/scroll-area/index.js',
  'packages/prototypes/shadcn/dist/collapsible/index.js',
  'packages/prototypes/shadcn/dist/drawer/index.js',
  'packages/prototypes/shadcn/dist/toggle-group/index.js',
  'packages/prototypes/shadcn/dist/input/index.js',
  'packages/prototypes/shadcn/dist/separator/index.js',
  'packages/prototypes/shadcn/dist/toggle/index.js',
  'packages/cli/bin/proto-ui.js',
  // The compiler ships as reviewed TypeScript source only; ./compiler.mjs
  // re-audits these same inputs before bundling them for the build.
  'packages/compiler/package.json',
  'packages/compiler/src/index.ts',
]) {
  if (!existsSync(join(sourceRoot, relative))) throw new Error(`Missing audited upstream artifact: ${relative}`);
}

// Link actual package directories from the reviewed checkout. pnpm places
// workspace links in each package's own node_modules, not all at the root.
const protoScope = join(webRoot, 'node_modules', '@proto.ui');
if (existsSync(protoScope) && lstatSync(protoScope).isSymbolicLink()) {
  if (realpathSync(protoScope) !== realpathSync(join(sourceRoot, 'node_modules', '@proto.ui'))) {
    throw new Error('web/node_modules/@proto.ui is an unexpected symlink');
  }
  unlinkSync(protoScope);
}
if (existsSync(protoScope) && !lstatSync(protoScope).isDirectory()) {
  throw new Error('web/node_modules/@proto.ui is not a directory');
}
mkdirSync(protoScope, { recursive: true });
const packageDirectories = new Map();
for (const rootEntry of readdirSync(join(sourceRoot, 'packages'), { withFileTypes: true })) {
  if (!rootEntry.isDirectory()) continue;
  const direct = join(sourceRoot, 'packages', rootEntry.name);
  const candidates = [direct];
  for (const child of readdirSync(direct, { withFileTypes: true })) {
    if (child.isDirectory()) candidates.push(join(direct, child.name));
  }
  for (const candidate of candidates) {
    const manifestPath = join(candidate, 'package.json');
    if (!existsSync(manifestPath)) continue;
    const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
    if (manifest.name?.startsWith('@proto.ui/')) packageDirectories.set(manifest.name, candidate);
  }
}
for (const [name, directory] of packageDirectories) {
  const link = join(protoScope, name.slice('@proto.ui/'.length));
  if (existsSync(link)) {
    if (realpathSync(link) !== realpathSync(directory)) throw new Error(`Unexpected package link: ${name}`);
  } else {
    symlinkSync(directory, link, 'dir');
  }
}

// The UI prints `pin.packages` verbatim in its build panel, so the pin must
// name versions the checked-out commit actually carries; a stale pin would
// make the interface claim a package revision that is not installed.
for (const entry of pin.packages) {
  const split = entry.lastIndexOf('@');
  const name = entry.slice(0, split);
  const version = entry.slice(split + 1);
  const directory = packageDirectories.get(name);
  if (!directory) throw new Error(`Pinned package ${name} is absent from the Proto-UI checkout`);
  const manifest = JSON.parse(readFileSync(join(directory, 'package.json'), 'utf8'));
  if (manifest.version !== version) {
    throw new Error(`${name} is ${manifest.version}, pinned as ${version}`);
  }
}

// Compile Shenko's own Prototypes with the reviewed compiler before the app
// bundle runs: main.tsx imports the generated React DOM source, never the
// compiler. `--prototypes-only` is the documented prerequisite for `tsc`.
const compilerRecord = await compileShenkoPrototypes({ sourceRoot, webRoot, pin });
console.log(
  `Shenko prototypes compiled by Proto-UI ${pin.commit} (${compilerRecord.profile}): ` +
    compilerRecord.prototypes.map((prototype) => prototype.name).join(', '),
);
if (prototypesOnly) {
  console.log(`Prototype sources written under ${PROTOTYPE_OUTPUT_RELATIVE}/; skipped bundling`);
  process.exit(0);
}

mkdirSync(staticDir, { recursive: true });
await build({
  absWorkingDir: webRoot,
  entryPoints: [join(webRoot, 'src', 'main.tsx')],
  outfile: join(staticDir, 'app.js'),
  bundle: true,
  minify: true,
  format: 'esm',
  platform: 'browser',
  target: ['es2022'],
  jsx: 'automatic',
  legalComments: 'eof',
  sourcemap: false,
  nodePaths: [join(sourceRoot, 'node_modules')],
  alias: {
    react: join(webRoot, 'node_modules', 'react'),
    'react-dom': join(webRoot, 'node_modules', 'react-dom'),
  },
  define: { 'process.env.NODE_ENV': '"production"' },
  logLevel: 'warning',
});

// This exact CLI path was reviewed: the shadcn preset only writes local CSS;
// it does not take the init/add package-manager branches.
run(process.execPath, [join(sourceRoot, 'packages/cli/bin/proto-ui.js'), 'shadcn', '--styles-dir', staticDir], {
  cwd: webRoot,
  stdio: 'inherit',
});
copyFileSync(join(webRoot, 'src', 'app.css'), join(staticDir, 'app.css'));
copyFileSync(join(webRoot, 'src', 'theme-bootstrap.js'), join(staticDir, 'theme-bootstrap.js'));
const licenseDir = join(staticDir, 'licenses');
mkdirSync(licenseDir, { recursive: true });
const licenseCopies = [
  [join(sourceRoot, 'LICENSE'), 'PROTO-UI-LICENSE'],
  [join(sourceRoot, 'packages/prototypes/shadcn/THIRD_PARTY_NOTICES.md'), 'SHADCN-NOTICES.md'],
  [join(webRoot, 'node_modules/react/LICENSE'), 'REACT-LICENSE'],
  [join(webRoot, 'node_modules/react-dom/LICENSE'), 'REACT-DOM-LICENSE'],
];
for (const [source, name] of licenseCopies) copyFileSync(source, join(licenseDir, name));

const packageVersion = (relative) => JSON.parse(readFileSync(join(webRoot, relative), 'utf8')).version;
const receiptPath = process.env.PROTO_UI_BUILD_RECEIPT;
let upstreamBuild = null;
if (receiptPath) {
  const receipt = JSON.parse(readFileSync(receiptPath, 'utf8'));
  if (receipt.selectedCount !== 42 || receipt.results?.length !== 42) {
    throw new Error('Unexpected upstream build receipt shape');
  }
  upstreamBuild = {
    script: 'scripts/build/public-packages.mjs',
    selected_package_count: receipt.selectedCount,
    receipt_path: receiptPath,
    receipt_sha256: sha256(receiptPath),
  };
}
const artifactNames = [
  'theme-bootstrap.js',
  'app.js',
  'app.css',
  'proto-ui-style.css',
  'proto-ui-tokens.generated.css',
  'shadcn-theme.css',
  'licenses/PROTO-UI-LICENSE',
  'licenses/SHADCN-NOTICES.md',
  'licenses/REACT-LICENSE',
  'licenses/REACT-DOM-LICENSE',
];
const artifacts = Object.fromEntries(artifactNames.map((name) => {
  const path = join(staticDir, name);
  return [name, { bytes: statSync(path).size, sha256: sha256(path) }];
}));
const manifest = {
  schema_version: 1,
  upstream: {
    repository: pin.repository,
    commit: pin.commit,
    previous_reviewed_commit: pin.migrated_from,
    lock_sha256: pin.lock_sha256,
    source_checkout: sourceRoot,
    build_receipt: upstreamBuild,
  },
  packages: pin.packages,
  adopted_families: pin.adopted_families,
  adoption: {
    runtime: 'createReactAdapter({ ...React, createPortal })',
    why: 'Current main provides a real React runtime adapter and family exports; the older npm next tarball lacks current main profiles and family exports.',
    style_profile: 'rhea (explicit); no Shadow split profile',
    source_assets: [
      'packages/adapters/react/dist/index.js',
      'packages/prototypes/shadcn/dist/button/index.js',
      'packages/prototypes/shadcn/dist/textarea/index.js',
      'packages/prototypes/shadcn/dist/scroll-area/index.js',
      'packages/prototypes/shadcn/dist/collapsible/index.js',
      'packages/prototypes/shadcn/dist/drawer/index.js',
      'packages/prototypes/shadcn/dist/toggle-group/index.js',
      'packages/prototypes/shadcn/dist/input/index.js',
      'packages/prototypes/shadcn/dist/separator/index.js',
      'packages/prototypes/shadcn/dist/toggle/index.js',
      'packages/cli/bin/proto-ui.js',
      'packages/compiler/package.json',
      'packages/compiler/src/index.ts',
    ],
    shadcn_provenance: {
      rhea_luma: 'shadcn-ui/ui@25af02b1b467fcd53b63e0c6d6d324c57c16ef18',
      new_york: 'shadcn-ui/ui@02cad61166c4c20edbc8a3f71bd720eed4c1f49b',
      api_baseline: 'shadcn-ui/ui@f31ed81983653919dd4fe77aee4b4859f610f1dc',
    },
  },
  // Facts, not intentions: the compiled set, exact inputs and generated
  // digests above are what this build actually consumed.
  compiler: {
    status: compilerRecord.status,
    runtime_imported: compilerRecord.runtime_imported,
    profile: compilerRecord.profile,
    package: compilerRecord.package,
    package_sha256: compilerRecord.package_sha256,
    entry: compilerRecord.entry,
    entry_sha256: compilerRecord.entry_sha256,
    source_tree_sha256: compilerRecord.source_tree_sha256,
    typescript: compilerRecord.typescript,
    bundle: compilerRecord.bundle,
    resolutions: compilerRecord.resolutions,
    public_package_selection: {
      script: 'scripts/build/public-packages.mjs',
      selected_package_count: upstreamBuild?.selected_package_count ?? null,
      includes_compiler: pin.compiler.public_package_selection === true,
      note: 'The compiler exports TypeScript source only and is private, so it is never one of the built public packages; Shenko compiles it at build time instead.',
    },
    prototypes: compilerRecord.prototypes,
  },
  toolchain: {
    node: process.version,
    pnpm: '10.32.1',
    esbuild: packageVersion('node_modules/esbuild/package.json'),
    react: packageVersion('node_modules/react/package.json'),
    react_dom: packageVersion('node_modules/react-dom/package.json'),
  },
  web_root: webRoot,
  built_at: new Date().toISOString(),
  artifacts,
};
writeFileSync(join(staticDir, 'build-manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Shenko UI built with Proto-UI ${pin.commit}; ${artifactNames.length} hashed assets`);
