#!/usr/bin/env python3
"""Build and observe the same pinned App, with complete input and run bindings.

Run `build` in the development container, then `observe` as the non-root browser
user against the same persistent worktree/output. Existing dependency installs
and a real PROTO_UI_BUILD_RECEIPT may be reused. Each output directory is new.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import uuid

import web_assets as assets

ROOT, WEB, STATIC = assets.ROOT, assets.WEB, assets.STATIC
SUITES = {
    'files': ('report.json', 2), 'im': ('report.json', 2),
    'inspector': ('receipt.json', 4), 'tree': ('report.json', 4),
}


def require(value, message):
    if not value:
        raise ValueError(message)


def head(directory):
    return subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=directory, text=True).strip()


def snapshot(upstream):
    result = assets.checkout_inputs(upstream)
    # A dirty checkout under the right HEAD is not the pinned compiler/source.
    for name, entry in result.items():
        file = upstream / name
        data = os.fsencode(os.readlink(file)) if file.is_symlink() else file.read_bytes()
        blob = hashlib.sha1(f'blob {len(data)}\0'.encode() + data).hexdigest()
        require(blob == entry['git_blob'], f'Pinned upstream bytes changed: {name}')
    return {
        'web': assets.files(WEB, assets.EXCLUDED),
        'html': assets.record(ROOT / 'crates/shenko-ui/src/index.html'),
        'node_pin': assets.record(ROOT / '.node-version'),
        'producer': assets.record(Path(__file__)),
        'asset_verifier': assets.record(ROOT / 'scripts/ci/web_assets.py'),
        'upstream_head': head(upstream), 'upstream': result,
    }


def observer_source():
    # Existing observers retain this explicitly bounded App snapshot. The full
    # binding above additionally covers prototypes, scripts, HTML and upstream.
    paths = [file for folder in ('src', 'tests') for file in (WEB / folder).rglob('*') if file.is_file()]
    paths.extend(WEB / name for name in ('build.mjs', 'compiler.mjs', 'upstream-pin.json',
                                       'package.json', 'pnpm-lock.yaml', 'tsconfig.json'))
    return {file.relative_to(ROOT).as_posix(): assets.sha256(file) for file in sorted(paths)}


def run(command, cwd, output, label, environment, timeout):
    with (output / f'{label}.log').open('w') as log:
        child = subprocess.Popen(command, cwd=cwd, env=environment, stdout=log,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        try:
            code = child.wait(timeout)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
            code = 124
    (output / f'{label}.exit').write_text(str(code) + '\n')
    return code


def build(output, upstream, environment):
    output.mkdir(parents=True, exist_ok=False)
    report = {'passed': False, 'appBase': head(ROOT), 'upstreamCandidate': head(upstream)}
    try:
        before = snapshot(upstream)
        assets.write_json(output / 'inputs-before.json', before)
        report['sourceBefore'] = observer_source()
        for label in ('check', 'test', 'build'):
            code = run(['pnpm', 'run', label], WEB, output, label, environment, 600)
            require(code == 0, f'Normal App {label} failed ({code})')
        report['sourceAfter'] = observer_source()
        after = snapshot(upstream)
        assets.write_json(output / 'inputs-after.json', after)
        report['sourceStable'] = report['sourceBefore'] == report['sourceAfter']
        report['fullSourceStable'] = before == after
        require(report['sourceStable'] and report['fullSourceStable'], 'Build inputs changed')
        manifest = assets.read_json(STATIC / 'build-manifest.json')
        assets.validate_static(STATIC, manifest)
        expected = json.loads(subprocess.check_output([
            'git', 'show', 'HEAD:crates/shenko-ui/src/static/build-manifest.json'], cwd=ROOT))
        require(assets.stable_manifest(manifest) == assets.stable_manifest(expected),
                'Normal build differs from committed payload/provenance')
        upstream_receipt = Path(environment['PROTO_UI_BUILD_RECEIPT'])
        require(manifest['upstream']['build_receipt']['receipt_sha256'] == assets.sha256(upstream_receipt),
                'Normal build is not bound to the real public-package receipt')
        report.update(compilerFilesVerified=len(assets.compiler_files(WEB, upstream, manifest)),
                      staticManifestSha256=assets.sha256(STATIC / 'build-manifest.json'),
                      fullInputsSha256=assets.sha256(output / 'inputs-after.json'),
                      staticArtifacts=assets.files(STATIC), toolchain=manifest['toolchain'], passed=True)
    except Exception as error:
        report['failure'] = str(error)
        raise
    finally:
        assets.write_json(output / 'normal-build.json', report)


def observe(output, upstream, environment):
    require(os.geteuid() != 0, 'Use the non-root browser user with Chromium sandbox enabled')
    built = assets.read_json(output / 'normal-build.json')
    require(built['passed'] is True and built['fullSourceStable'] is True, 'Normal build did not pass')
    require(built['appBase'] == head(ROOT) and built['upstreamCandidate'] == head(upstream), 'Candidate changed')
    require(built['fullInputsSha256'] == assets.sha256(output / 'inputs-after.json'), 'Input receipt changed')
    bound = assets.read_json(output / 'inputs-after.json')
    observed = output / 'observers'
    observed.mkdir(exist_ok=False)
    report = {'passed': False, 'appBase': head(ROOT), 'upstreamCandidate': head(upstream),
              'normalBuildSha256': assets.sha256(output / 'normal-build.json'), 'suites': []}
    try:
        require(snapshot(upstream) == bound, 'Complete source inputs differ before observation')
        require(assets.files(STATIC) == built['staticArtifacts'], 'Built assets differ before observation')
        for suite, (filename, count) in SUITES.items():
            directory = observed / suite
            directory.mkdir()
            run_id = str(uuid.uuid4())
            env = {**environment, 'PUI_EVIDENCE_DIR': str(directory), 'PUI_OBSERVER_RUN_ID': run_id}
            for prefix in ('IM', 'TREE', 'INSPECTOR'):
                env[f'PUI_{prefix}_BUILD_RECEIPT'] = str(output / 'normal-build.json')
            code = run(['node', str(WEB / 'scripts' / f'{suite}-browser.mjs')],
                       ROOT, observed, suite, env, 180)
            item = {'suite': suite, 'runId': run_id, 'exitCode': code, 'passed': False}
            report['suites'].append(item)
            try:
                receipt = assets.read_json(directory / filename)
                cases = receipt.get('cases', [])
                require(code == 0 and receipt.get('passed') is True, 'Observer process or receipt failed')
                require(receipt.get('runId') == run_id and receipt.get('finishedAt'), 'Receipt is not from this invocation')
                require(receipt['buildManifestSha256'] == built['staticManifestSha256'], 'Observer consumed another build')
                require(receipt.get('pageErrors') == [] and receipt.get('cleanupErrors') == [], 'Observer errors remain')
                require(len(cases) == count and all(case.get('passed') is True or case.get('status') == 'passed'
                                                  for case in cases), 'Incomplete observer journeys')
                item.update(passed=True, receiptSha256=assets.sha256(directory / filename), cases=len(cases))
            except Exception as error:
                item['failure'] = str(error)
        report['fullSourceStable'] = snapshot(upstream) == bound
        report['artifactsStable'] = assets.files(STATIC) == built['staticArtifacts']
        require(report['fullSourceStable'] and report['artifactsStable'], 'Source, HTML or served assets changed during observation')
        require(all(item['passed'] for item in report['suites']), 'One or more actual App observers failed')
        report['passed'] = True
    except Exception as error:
        report['failure'] = str(error)
        raise
    finally:
        assets.write_json(observed / 'summary.json', report)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('build', 'observe'))
    parser.add_argument('--upstream', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    upstream, output = args.upstream.resolve(), args.output.resolve()
    pin = assets.read_json(WEB / 'upstream-pin.json')
    require(head(upstream) == pin['commit'] and assets.sha256(upstream / 'pnpm-lock.yaml') == pin['lock_sha256'],
            'Upstream commit or lock differs from the consumer pin')
    node = subprocess.check_output(['node', '--version'], text=True).strip()
    require(node == 'v' + (ROOT / '.node-version').read_text().strip(), 'Canonical Node version required')
    environment = {**os.environ, 'PROTO_UI_SOURCE': str(upstream)}
    if args.command == 'build':
        build(output, upstream, environment)
    else:
        observe(output, upstream, environment)


if __name__ == '__main__':
    main()
