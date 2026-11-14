#!/usr/bin/env python3
"""Rebuild the pinned real frontend in a fresh output tree and compare its bytes.

The checked-in static directory is only read. The existing build.mjs and
compiler.mjs execute from a clean snapshot with no node_modules, generated
compiler, generated prototypes or static output. Keep both generations and
their manifests after failure; this command never updates expected digests.
"""
import argparse
import copy
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
WEB = ROOT / 'crates/shenko-ui/web'
STATIC = ROOT / 'crates/shenko-ui/src/static'
EXCLUDED = {'node_modules', 'generated', '.pnpm-store', '.git'}


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def record(path):
    return {'bytes': path.stat().st_size, 'sha256': sha256(path)}


def read_json(path):
    return json.loads(path.read_text())


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def inside(root, name):
    relative = PurePosixPath(name)
    if relative.is_absolute() or not relative.parts or any(part in ('.', '..') for part in relative.parts):
        raise ValueError(f'Invalid artifact path: {name}')
    result = root.joinpath(*relative.parts)
    if not result.resolve().is_relative_to(root.resolve()):
        raise ValueError(f'Artifact escapes its owning root: {name}')
    return result


def files(root, exclude=()):
    result = {}
    for path in sorted(root.iterdir()):
        if path.name in exclude:
            continue
        if path.is_symlink():
            raise ValueError(f'Build input/output is an unexpected symlink: {path}')
        if path.is_dir():
            result.update({f'{path.name}/{name}': value for name, value in files(path, exclude).items()})
        elif path.is_file():
            result[path.name] = record(path)
        else:
            raise ValueError(f'Build input/output is not a regular file: {path}')
    return result


def run(command, cwd, output, label, environment=None, stdout=None):
    print(f'{cwd}: {" ".join(map(str, command))}', flush=True)
    with (output / f'{label}.log').open('w') as log:
        if stdout is None:
            process = subprocess.Popen(command, cwd=cwd, env=environment, text=True,
                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            for line in process.stdout:
                log.write(line)
                log.flush()
                print(line, end='', flush=True)
            status = process.wait()
        else:
            with stdout.open('wb') as result:
                status = subprocess.run(command, cwd=cwd, env=environment, stdout=result, stderr=log).returncode
    if status:
        raise RuntimeError(f'{label} failed ({status}); retained {output / (label + ".log")}')


def validate_static(directory, manifest):
    if manifest.get('schema_version') != 1 or not manifest.get('artifacts'):
        raise ValueError('Expected a populated schema-1 static manifest')
    inventory = files(directory)
    inventory.pop('build-manifest.json')
    if inventory != manifest['artifacts']:
        raise ValueError(f'{directory}: static bytes/membership do not match every recorded hash and byte count')
    for name in manifest['artifacts']:
        inside(directory, name)


def prepare(output):
    output.mkdir(parents=True, exist_ok=False)
    manifest = read_json(STATIC / 'build-manifest.json')
    pin = read_json(WEB / 'upstream-pin.json')
    validate_static(STATIC, manifest)
    for field, value in [('repository', pin['repository']), ('commit', pin['commit']),
                         ('lock_sha256', pin['lock_sha256']), ('previous_reviewed_commit', pin['migrated_from'])]:
        if manifest['upstream'][field] != value:
            raise ValueError(f'Committed upstream provenance is stale: {field}')
    if not re.fullmatch(r'[0-9a-f]{40}', pin['commit']):
        raise ValueError('Proto-UI must be pinned by its full commit SHA')
    if not manifest['upstream'].get('build_receipt'):
        raise ValueError('The committed manifest must retain its real public-closure build receipt')
    source_inputs = files(WEB, EXCLUDED)
    shutil.copytree(STATIC, output / 'committed')
    fresh_web = output / 'source/web'
    shutil.copytree(WEB, fresh_web, ignore=shutil.ignore_patterns(*EXCLUDED))
    if files(fresh_web) != source_inputs:
        raise ValueError('The clean frontend source snapshot differs from the admitted input bytes')
    upstream = output / 'proto-ui'
    upstream.mkdir()
    run(['git', 'init', str(upstream)], ROOT, output, 'upstream-init')
    run(['git', 'remote', 'add', 'origin', pin['repository']], upstream, output, 'upstream-remote')
    run(['git', 'fetch', '--depth=1', 'origin', pin['commit']], upstream, output, 'upstream-fetch')
    run(['git', 'checkout', '--detach', 'FETCH_HEAD'], upstream, output, 'upstream-checkout')
    actual = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=upstream, text=True).strip()
    if actual != pin['commit'] or sha256(upstream / 'pnpm-lock.yaml') != pin['lock_sha256']:
        raise ValueError('The independent Proto-UI checkout/lock does not match the pin')
    manager = read_json(WEB / 'package.json')['packageManager']
    if read_json(upstream / 'package.json')['packageManager'] != manager:
        raise ValueError('The consumer and pinned upstream declare different package managers')
    entries = [entry.rsplit('@', 1)[0] for entry in pin['packages']]
    if len(entries) != 3 or set(entries) != {'@proto.ui/adapter-react', '@proto.ui/prototypes-shadcn', '@proto.ui/cli'}:
        raise ValueError('Revisit the public-closure contract: expected the actual three reviewed public entries')
    state = {'pin': pin, 'package_manager': manager, 'public_entries': entries,
             'web_inputs': source_inputs, 'committed_static': files(STATIC),
             'node': (ROOT / '.node-version').read_text().strip()}
    write_json(output / 'inputs.json', state)
    if os.environ.get('GITHUB_ENV'):
        with open(os.environ['GITHUB_ENV'], 'a') as environment:
            environment.write(f'PROTO_UI_SOURCE={upstream}\n')
            environment.write(f'PROTO_UI_BUILD_RECEIPT={output / "upstream-build.json"}\n')


def stable_manifest(manifest):
    result = copy.deepcopy(manifest)
    # These exact fields describe an individual run. Their original values
    # remain in committed/rebuilt manifests and comparison.json. No payload,
    # compiler, generated source or Prototype provenance digest is excluded.
    del result['built_at']
    del result['web_root']
    del result['upstream']['source_checkout']
    del result['upstream']['build_receipt']['receipt_path']
    del result['upstream']['build_receipt']['receipt_sha256']
    return result


def differences(expected, actual, path='manifest'):
    if isinstance(expected, dict) and isinstance(actual, dict):
        result = []
        for key in sorted(expected.keys() | actual.keys()):
            if key not in expected or key not in actual:
                result.append(f'{path}.{key}: field membership differs')
            else:
                result.extend(differences(expected[key], actual[key], f'{path}.{key}'))
        return result
    return [] if expected == actual else [f'{path}: {expected!r} != {actual!r}']


def compiler_files(web, upstream, manifest):
    compiler = manifest['compiler']
    verified = {}
    required = [
        (upstream, compiler['package'] + '/package.json', compiler['package_sha256'], None),
        (upstream, compiler['entry'], compiler['entry_sha256'], None),
        (web, compiler['bundle']['path'], compiler['bundle']['sha256'], compiler['bundle']['bytes']),
    ]
    source_root = inside(upstream, compiler['package'] + '/src')
    source_digest = hashlib.sha256()
    source_files = files(source_root)
    for name in sorted(source_files):
        value = source_files[name]
        source_digest.update(f'{name}\0{value["sha256"]}\n'.encode())
    if source_digest.hexdigest() != compiler['source_tree_sha256']:
        raise ValueError('Rebuilt compiler source-tree digest does not describe its real source bytes')
    for resolution in compiler['resolutions']:
        name = f'{resolution["owner"]}/node_modules/{resolution["specifier"]}/package.json'
        required.append((upstream, name, resolution['package_sha256'], None))
        if read_json(inside(upstream, name))['version'] != resolution['version']:
            raise ValueError(f'Compiler dependency version differs: {name}')
    for prototype in compiler['prototypes']:
        required.extend([
            (web, prototype['entry'], prototype['entry_sha256'], None),
            (web, prototype['generated_source'], prototype['generated_sha256'], None),
            (web, prototype['provenance'], prototype['provenance_sha256'], None),
        ])
        provenance = read_json(inside(web, prototype['provenance']))
        if (provenance['source']['sha256'] != prototype['compiler_source_sha256'] or
                provenance['backend'] != prototype['compiler_backend'] or
                provenance['irVersion'] != prototype['compiler_ir_version'] or
                provenance['generatedSha256'] != prototype['generated_sha256']):
            raise ValueError(f'Prototype provenance does not match the real generation: {prototype["name"]}')
        directory = inside(web, prototype['output_directory'])
        declared = set()
        for artifact in provenance['artifacts']:
            declared.add(artifact['path'])
            required.append((directory, artifact['path'], artifact['sha256'], artifact.get('bytes')))
        actual = set(files(directory))
        actual.remove(inside(web, prototype['provenance']).relative_to(directory).as_posix())
        if actual != declared:
            raise ValueError(f'Generated Prototype artifact membership differs: {prototype["name"]}')
    for root, name, digest, size in required:
        value = record(inside(root, name))
        if value['sha256'] != digest or (size is not None and value['bytes'] != size):
            raise ValueError(f'Rebuilt compiler/Prototype bytes differ from their manifest: {name}')
        verified[str(root / name)] = value
    return verified


def check(output):
    state = read_json(output / 'inputs.json')
    upstream = output / 'proto-ui'
    web = output / 'source/web'
    rebuilt = output / 'source/src/static'
    if (web / 'node_modules').exists() or (web / 'generated').exists() or rebuilt.exists():
        raise ValueError('A fresh check cannot reuse installed dependencies, generated sources or static outputs')
    node = subprocess.check_output(['node', '--version'], text=True).strip()
    pnpm = subprocess.check_output(['pnpm', '--version'], text=True).strip()
    if node != 'v' + state['node'] or state['package_manager'] != 'pnpm@' + pnpm:
        raise ValueError(f'Unexpected build toolchain: {node}, pnpm@{pnpm}')
    if files(web) != state['web_inputs']:
        raise ValueError('The admitted clean frontend source changed before dependency installation')
    environment = dict(os.environ, PROTO_UI_SOURCE=str(upstream),
                       PROTO_UI_BUILD_RECEIPT=str(output / 'upstream-build.json'), HUSKY='0')
    run(['pnpm', 'install', '--frozen-lockfile'], upstream, output, 'upstream-install', environment)
    run(['pnpm', 'install', '--frozen-lockfile'], web, output, 'web-install', environment)
    if sha256(upstream / 'pnpm-lock.yaml') != state['pin']['lock_sha256']:
        raise ValueError('Dependency installation changed the pinned upstream lock')
    arguments = ['node', 'scripts/build/public-packages.mjs']
    for name in state['public_entries']:
        arguments.extend(['--package', name])
    run([*arguments, '--plan', '--json'], upstream, output, 'upstream-plan', environment, output / 'upstream-plan.json')
    plan = read_json(output / 'upstream-plan.json')
    if not set(state['public_entries']).issubset(plan['packages']) or '@proto.ui/compiler' in plan['packages']:
        raise ValueError('The public build must contain the three public entries and exclude the private compiler')
    if len(plan['packages']) != plan['selectedCount'] or len(set(plan['packages'])) != plan['selectedCount']:
        raise ValueError('The upstream public build plan is incomplete or duplicated')
    run([*arguments, '--json'], upstream, output, 'upstream-build', environment, output / 'upstream-build.json')
    receipt = read_json(output / 'upstream-build.json')
    if (receipt['selectedCount'] != plan['selectedCount'] or
            [entry['name'] for entry in receipt['results']] != plan['packages']):
        raise ValueError('The actual public build did not complete its planned closure')
    run(['pnpm', 'run', 'check'], web, output, 'web-types', environment)
    run(['pnpm', 'run', 'test'], web, output, 'web-tests', environment)
    run(['pnpm', 'run', 'build'], web, output, 'web-build', environment)
    actual = read_json(rebuilt / 'build-manifest.json')
    expected = read_json(output / 'committed/build-manifest.json')
    validate_static(rebuilt, actual)
    verified = compiler_files(web, upstream, actual)
    if actual['upstream']['build_receipt']['receipt_sha256'] != sha256(output / 'upstream-build.json'):
        raise ValueError('The rebuilt manifest does not bind the actual upstream receipt')
    errors = differences(stable_manifest(expected), stable_manifest(actual))
    for name in expected['artifacts'].keys() & actual['artifacts'].keys():
        if inside(output / 'committed', name).read_bytes() != inside(rebuilt, name).read_bytes():
            errors.append(f'Payload bytes differ: {name}')
    if files(STATIC) != state['committed_static'] or files(WEB, EXCLUDED) != state['web_inputs']:
        errors.append('The original checkout changed during independent verification')
    write_json(output / 'comparison.json', {
        'pin': state['pin'], 'node': node, 'package_manager': state['package_manager'],
        'committed_manifest_sha256': sha256(output / 'committed/build-manifest.json'),
        'rebuilt_manifest_sha256': sha256(rebuilt / 'build-manifest.json'),
        'committed_run': {key: expected[key] for key in ('built_at', 'web_root', 'upstream')},
        'rebuilt_run': {key: actual[key] for key in ('built_at', 'web_root', 'upstream')},
        'public_plan': plan, 'rebuilt_payloads': actual['artifacts'],
        'verified_compiler_and_prototype_files': verified, 'errors': errors,
    })
    if errors:
        raise SystemExit('\n'.join(errors))
    print('Independent frontend payload bytes and stable compiler/Prototype provenance match the committed generation')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('prepare', 'check'))
    parser.add_argument('--output', type=Path, default=ROOT / '.ci/web')
    args = parser.parse_args()
    output = args.output.resolve()
    if args.command == 'prepare':
        prepare(output)
    else:
        check(output)


if __name__ == '__main__':
    main()
