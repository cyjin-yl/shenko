#!/usr/bin/env python3
"""Bind one manual reading run to Shenko 03b80d7 and its unchanged producer.

This is not a command runner. It admits one application commit, upstream pin,
workflow ref and four-route receipt. It never launches or changes the browser.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess


APP_SHA = '03b80d770357fb8e7694c0fdac7a2c79911e970c'
BASE_SHA = '2c778c1832a71f68b6323c86af6c5fa9b1314b85'
UPSTREAM_SHA = 'ad39435aeb3589d3bc52de3cbd1f4cdd375cec68'
SCRIPT_SHA256 = '5107d3a43538e610a347de2509018b5a782a8d97f6933adaf783944c9701fb6b'
MANIFEST_SHA256 = '42ea5abe58a8636a6b04a3402b10deb3ec2a583d7d3c99e3abeebcdc0b21f277'
REF = 'refs/heads/evidence/shenko-reading-03b80-20261122'
REPOSITORY = 'cyjin-yl/shenko'
CHROME = '/usr/bin/google-chrome'
SCRIPT = 'scripts/web_reading_journey.mjs'
MANIFEST = 'crates/shenko-ui/src/static/build-manifest.json'
PIN = 'crates/shenko-ui/web/upstream-pin.json'
ROUTES = [('inspect', 1280), ('im', 1280), ('inspect', 390), ('im', 390)]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def read_json(path):
    return json.loads(path.read_text())


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + '\n')


def command(arguments, root):
    return subprocess.check_output(arguments, cwd=root, text=True).strip()


def tracked_blob(path, mode, expected_blob):
    actual_mode = path.lstat().st_mode
    if mode == '120000':
        require(stat.S_ISLNK(actual_mode), f'Expected tracked symlink: {path}')
        content = os.fsencode(os.readlink(path))
    else:
        require(mode in ('100644', '100755') and stat.S_ISREG(actual_mode),
                f'Expected tracked regular file: {path}')
        require(bool(actual_mode & 0o111) == (mode == '100755'),
                f'Tracked executable mode changed: {path}')
        content = path.read_bytes()
    blob = hashlib.sha1(b'blob ' + str(len(content)).encode() + b'\0' + content).hexdigest()
    require(blob == expected_blob, f'Tracked source bytes changed: {path}')
    return hashlib.sha256(content).hexdigest()


def checkout(root, expected_head):
    require(command(['git', 'rev-parse', 'HEAD'], root) == expected_head,
            f'Wrong checkout head: {root}')
    entries = subprocess.check_output(
        ['git', 'ls-tree', '-r', '-z', '--full-tree', 'HEAD'], cwd=root).split(b'\0')
    digest = hashlib.sha256()
    count = 0
    for entry in entries:
        if not entry:
            continue
        metadata, raw_name = entry.split(b'\t', 1)
        mode, kind, blob = metadata.decode().split()
        name = os.fsdecode(raw_name)
        require(kind == 'blob' and '..' not in Path(name).parts and not Path(name).is_absolute(),
                f'Unexpected tracked membership: {name}')
        source_digest = tracked_blob(root / name, mode, blob)
        digest.update(f'{mode}\0{name}\0{source_digest}\n'.encode())
        count += 1
    require(count > 0, 'Empty source inventory')
    return {'head': expected_head, 'tracked_files': count, 'tracked_sha256': digest.hexdigest()}


def fixed_bindings(values):
    expected = {'app_head': APP_SHA, 'upstream_pin': UPSTREAM_SHA,
                'script_sha256': SCRIPT_SHA256, 'manifest_sha256': MANIFEST_SHA256}
    require(values == expected, 'The application/source/producer binding is not this candidate')


def context(values):
    require(values['event'] == 'workflow_dispatch' and values['ref'] == REF
            and values['repository'] == REPOSITORY and values['base_sha'] == BASE_SHA,
            'This evidence job accepts only its fixed manual ref and independent baseline')
    require(values['node'] == 'v24.22.1' and values['pnpm'] == '10.32.1',
            'Wrong Node or pnpm runtime')
    require(isinstance(values['uid'], int) and values['uid'] > 0,
            'The reading producer requires the ordinary non-root runner user')
    require(values['os_id'] == 'ubuntu' and values['os_version'] == '24.04',
            'The reading producer requires the standard Ubuntu 24.04 runner')
    require(re.fullmatch(r'[0-9a-f]{40}', values['workflow_sha']) is not None,
            'Missing immutable workflow source identity')
    require(all(str(values[key]).isdigit() and int(values[key]) > 0
                for key in ('run_id', 'run_attempt')), 'Missing actual workflow run identity')
    require(values['chrome_path'] == CHROME and re.search(r'\d+\.\d+\.\d+\.\d+', values['chrome_version']),
            'Missing system Chrome version')
    require('Noto Sans CJK' in values['cjk_font'], 'The CJK font prerequisite is unavailable')


def environment(app, tools):
    event = read_json(Path(os.environ['GITHUB_EVENT_PATH']))
    release = dict(line.split('=', 1) for line in Path('/etc/os-release').read_text().splitlines() if '=' in line)
    values = {
        'event': os.environ.get('GITHUB_EVENT_NAME'), 'ref': os.environ.get('GITHUB_REF'),
        'repository': os.environ.get('GITHUB_REPOSITORY'),
        'base_sha': event.get('inputs', {}).get('base-sha'),
        'workflow_sha': os.environ.get('GITHUB_SHA', ''),
        'run_id': os.environ.get('GITHUB_RUN_ID', ''),
        'run_attempt': os.environ.get('GITHUB_RUN_ATTEMPT', ''),
        'node': command(['node', '--version'], app), 'pnpm': command(['pnpm', '--version'], app),
        'uid': os.getuid(), 'os_id': release.get('ID', '').strip('"'),
        'os_version': release.get('VERSION_ID', '').strip('"'),
        'chrome_path': CHROME, 'chrome_version': command([CHROME, '--version'], app),
        'cjk_font': command(['fc-match', '-f', '%{family}', 'Noto Sans CJK SC'], app),
        'runner_image': os.environ.get('ImageOS'), 'runner_image_version': os.environ.get('ImageVersion'),
    }
    context(values)
    require(command(['git', 'rev-parse', 'HEAD'], tools) == values['workflow_sha'],
            'The verifier checkout does not match this workflow run')
    return values


def static_assets(directory, manifest):
    expected = manifest['artifacts']
    actual = {p.relative_to(directory).as_posix() for p in directory.rglob('*') if p.is_file()}
    require(actual == {*expected, 'build-manifest.json'}, 'Static artifact membership differs')
    for name, value in expected.items():
        require(not Path(name).is_absolute() and '..' not in Path(name).parts,
                'Unsafe static artifact name')
        path = directory / name
        require(not path.is_symlink() and path.stat().st_size == value['bytes']
                and sha256(path) == value['sha256'], f'Static payload differs: {name}')


def rebuilt_binding(comparison, manifest):
    require(comparison.get('errors') == [], 'Independent rebuild did not pass')
    require(comparison.get('committed_manifest_sha256') == MANIFEST_SHA256,
            'Independent rebuild used a different committed generation')
    require(comparison.get('pin', {}).get('commit') == UPSTREAM_SHA
            and comparison.get('node') == 'v24.22.1'
            and comparison.get('package_manager') == 'pnpm@10.32.1', 'Wrong independent rebuild inputs')
    require(comparison.get('public_plan', {}).get('selectedCount') == 42,
            'Independent rebuild omitted the fixed public closure')
    require(comparison.get('rebuilt_payloads') == manifest['artifacts'],
            'Independent rebuild payloads are not the served candidate bytes')


def snapshot(app, tools, prepared):
    pin = read_json(app / PIN)
    manifest = read_json(app / MANIFEST)
    bindings = {'app_head': command(['git', 'rev-parse', 'HEAD'], app),
                'upstream_pin': pin.get('commit'), 'script_sha256': sha256(app / SCRIPT),
                'manifest_sha256': sha256(app / MANIFEST)}
    fixed_bindings(bindings)
    require(manifest['upstream']['commit'] == UPSTREAM_SHA, 'Manifest has another upstream')
    static_assets(app / 'crates/shenko-ui/src/static', manifest)
    untracked = subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=app).split(b'\0')
    require(all(not name or os.fsdecode(name).startswith('.ci/web/') for name in untracked),
            'Unexpected untracked source outside the declared rebuild output')
    result = {'schema_version': 1, 'bindings': bindings, 'context': environment(app, tools),
              'application': checkout(app, APP_SHA), 'verifier_sha256': sha256(Path(__file__)),
              'producer_launch': {'headless': True, 'executablePath': CHROME,
                                  'extra_arguments': [], 'chromiumSandbox_option': 'not set by the fixed producer',
                                  'sandbox_status': 'not observed; no sandbox-enabled claim'}}
    if prepared:
        build = app / '.ci/web'
        comparison = read_json(build / 'comparison.json')
        rebuilt_binding(comparison, manifest)
        rebuilt = read_json(build / 'source/src/static/build-manifest.json')
        static_assets(build / 'source/src/static', rebuilt)
        require(rebuilt['artifacts'] == manifest['artifacts'], 'Rebuilt bytes changed after verification')
        result['upstream'] = checkout(build / 'proto-ui', UPSTREAM_SHA)
        result['comparison_sha256'] = sha256(build / 'comparison.json')
        module = build / 'proto-ui/apps/www/node_modules/playwright-core/index.mjs'
        require(module.is_file(), 'The pinned installation has no Playwright entry')
        result['playwright'] = {'version': read_json(module.parent / 'package.json')['version'],
                                'entry_sha256': sha256(module)}
    return result


def expected_checks(tab):
    checks = [
        'committed reply text reaches the actual conversation renderer',
        'append follows the actual tail and retains composer focus',
        'trusted Shift+Tab reaches a historical message action away from the tail',
        'trusted Shift+Tab retains focus within a historical message',
        'append preserves Shift+Tab focus and its reader anchor',
        'trusted Tab retains focus within a historical message',
        'append preserves Tab focus and its reader anchor',
        'explicit resume after content focus follows appends and keeps the resume control focused',
        'trusted wheel departs from the end',
        'append preserves the same reader key and offset',
        'replayed committed identity adds no duplicate row',
    ]
    if tab == 'inspect':
        checks.append('reopening restores the saved reader key across replay materialization')
    return checks + [
        'keyboard resume keeps focus on its surviving control',
        'resumed lease follows later appends',
        'selection and its anchor survive append',
        'natural reader end arrival resumes later growth',
        'subject replacement retires the old lease and ignores its late callbacks',
        'dispose disconnects observers and ignores late observer callbacks',
        'render/replay/dispose issue no business writes',
        'real App has no page errors',
    ]


def receipt_binding(receipt, manifest, before, after):
    require(before == after, 'Source, toolchain or run binding changed across the browser journey')
    fixed_bindings(before['bindings'])
    context(before['context'])
    require(receipt.get('schema_version') == 1 and receipt.get('status') == 'passed',
            'The actual producer did not pass')
    require(receipt.get('scope') == 'real Shenko live frontend; fixture HTTP/WS reads'
            and receipt.get('physical_input') is False and receipt.get('model_or_backend_acceptance') is False,
            'Receipt changed the bounded frontend evidence scope')
    require(receipt.get('bundle_manifest_sha256') == MANIFEST_SHA256
            and receipt.get('upstream') == manifest['upstream'], 'Receipt belongs to a different source generation')
    browser = receipt.get('browser', {})
    version = re.search(r'\d+\.\d+\.\d+\.\d+', before['context']['chrome_version']).group()
    require(browser.get('engine') == 'Chromium' and browser.get('headless') is True
            and browser.get('version') == version, 'Receipt used another browser')
    runs = receipt.get('runs', [])
    require([(r.get('tab'), r.get('viewport', {}).get('width')) for r in runs] == ROUTES,
            'Missing, duplicated or substituted reading route')
    for run in runs:
        require(run.get('status') == 'passed' and run.get('errors') == []
                and not any(run.get(key) for key in ('failure', 'skip', 'skipped', 'pending')),
                'A reading route failed, skipped, or emitted page errors')
        require(run['viewport'].get('height') == 900 and run.get('theme') == 'light'
                and run.get('reduced_motion') == 'reduce', 'Reading route configuration changed')
        checks = run.get('checks', [])
        require([c.get('name') for c in checks] == expected_checks(run['tab'])
                and all(c.get('passed') is True for c in checks),
                'Missing, duplicated, weakened or failed reading check')
    require(sum(len(r['checks']) for r in runs) == 78, 'Expected Inspect 20 + IM 19, at each width')
    requests = receipt.get('requests', [])
    require(requests and all(r.get('method') in ('GET', 'HEAD') for r in requests),
            'The frontend fixture observed a non-read HTTP request')


def screenshots(directory):
    result = {}
    for tab, width in ROUTES:
        name = f'{tab}-{width}.png'
        path = directory / name
        require(path.is_file() and not path.is_symlink(), f'Missing real screenshot: {name}')
        require(path.read_bytes().startswith(b'\x89PNG\r\n\x1a\n') and path.stat().st_size > 8,
                f'Invalid screenshot: {name}')
        result[name] = {'bytes': path.stat().st_size, 'sha256': sha256(path)}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('stage', choices=('before', 'ready', 'verify'))
    stage = parser.parse_args().stage
    workspace = Path(os.environ['GITHUB_WORKSPACE']).resolve()
    app, tools, output = workspace / 'reading-app', workspace / 'reading-tools', workspace / 'reading-evidence'
    try:
        current = snapshot(app, tools, stage != 'before')
        write_json(output / f'source-{stage}.json', current)
        if stage == 'verify':
            before = read_json(output / 'source-ready.json')
            receipt = read_json(output / 'journey/receipt.json')
            receipt_binding(receipt, read_json(app / MANIFEST), before, current)
            result = {'status': 'passed', 'application': APP_SHA, 'upstream': UPSTREAM_SHA,
                      'context': current['context'], 'producer_sha256': SCRIPT_SHA256,
                      'producer_manifest_sha256': MANIFEST_SHA256,
                      'producer_receipt_sha256': sha256(output / 'journey/receipt.json'),
                      'source_before_sha256': sha256(output / 'source-ready.json'),
                      'source_after_sha256': sha256(output / 'source-verify.json'),
                      'screenshots': screenshots(output / 'journey'), 'routes': 4, 'checks': 78,
                      'boundary': 'Actual production frontend with deterministic reads; no physical input, backend/model, deployment or sandbox-status acceptance.'}
            write_json(output / 'verified.json', result)
        print(json.dumps({'stage': stage, 'status': 'passed', 'application': APP_SHA}))
    except Exception as error:
        write_json(output / f'failure-{stage}.json', {'stage': stage, 'status': 'failed', 'error': str(error)})
        raise


if __name__ == '__main__':
    main()
