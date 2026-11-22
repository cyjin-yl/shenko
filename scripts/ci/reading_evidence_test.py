#!/usr/bin/env python3
"""Focused admission/receipt negative controls, never browser acceptance."""
from collections import Counter
import copy
import hashlib
import importlib.util
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


spec = importlib.util.spec_from_file_location('reading_evidence', Path(__file__).with_name('reading_evidence.py'))
evidence = importlib.util.module_from_spec(spec)
spec.loader.exec_module(evidence)


def runtime():
    return {'event': 'workflow_dispatch', 'ref': evidence.REF, 'repository': evidence.REPOSITORY,
            'base_sha': evidence.BASE_SHA, 'workflow_sha': 'a' * 40, 'run_id': '123', 'run_attempt': '1',
            'node': 'v24.22.1', 'pnpm': '10.32.1', 'uid': 1001, 'os_id': 'ubuntu', 'os_version': '24.04',
            'chrome_path': evidence.CHROME, 'chrome_version': 'Google Chrome 145.0.7632.6',
            'cjk_font': 'Noto Sans CJK SC'}


def fixture():
    # Synthetic parser input only. It is never written as a producer receipt.
    manifest = {'upstream': {'commit': evidence.UPSTREAM_SHA}, 'artifacts': {'app.js': {'sha256': 'x', 'bytes': 1}}}
    snapshot = {'bindings': {'app_head': evidence.APP_SHA, 'upstream_pin': evidence.UPSTREAM_SHA,
                            'script_sha256': evidence.SCRIPT_SHA256, 'manifest_sha256': evidence.MANIFEST_SHA256},
                'context': runtime(), 'application': {'tracked_sha256': 'candidate-source'},
                'comparison_sha256': 'actual-rebuild-comparison'}
    receipt = {'schema_version': 1, 'status': 'passed',
               'scope': 'real Shenko live frontend; fixture HTTP/WS reads',
               'physical_input': False, 'model_or_backend_acceptance': False,
               'bundle_manifest_sha256': evidence.MANIFEST_SHA256, 'upstream': manifest['upstream'],
               'browser': {'engine': 'Chromium', 'headless': True, 'version': '145.0.7632.6'},
               'requests': [{'method': 'GET', 'path': '/api/self'}], 'runs': []}
    for tab, width in evidence.ROUTES:
        receipt['runs'].append({'tab': tab, 'viewport': {'width': width, 'height': 900},
                                'theme': 'light', 'reduced_motion': 'reduce', 'status': 'passed', 'errors': [],
                                'checks': [{'name': name, 'passed': True} for name in evidence.expected_checks(tab)]})
    return manifest, snapshot, receipt


class ReadingEvidenceControls(unittest.TestCase):
    def test_real_file_bytes_and_mode_are_checked_against_git_blob(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'producer.mjs'
            content = b'export const source = "reviewed";\n'
            path.write_bytes(content)
            path.chmod(0o644)
            blob = hashlib.sha1(b'blob ' + str(len(content)).encode() + b'\0' + content).hexdigest()
            self.assertEqual(evidence.tracked_blob(path, '100644', blob), hashlib.sha256(content).hexdigest())
            path.write_bytes(content.replace(b'reviewed', b'modified'))
            with self.assertRaisesRegex(ValueError, 'source bytes changed'):
                evidence.tracked_blob(path, '100644', blob)
            path.write_bytes(content)
            path.chmod(0o755)
            with self.assertRaisesRegex(ValueError, 'executable mode changed'):
                evidence.tracked_blob(path, '100644', blob)

    def test_fixed_candidate_and_execution_context_reject_substitution(self):
        _, snapshot, _ = fixture()
        evidence.fixed_bindings(snapshot['bindings'])
        evidence.context(runtime())
        bad = dict(snapshot['bindings'], app_head='b' * 40)
        with self.assertRaisesRegex(ValueError, 'not this candidate'):
            evidence.fixed_bindings(bad)
        for patch in ({'ref': 'refs/heads/main'}, {'event': 'push'}, {'uid': 0}, {'node': 'v24.22.3'}):
            with self.subTest(patch=patch), self.assertRaises(ValueError):
                evidence.context(dict(runtime(), **patch))

    def test_complete_receipt_matches_the_fixed_four_routes_and_78_checks(self):
        manifest, snapshot, receipt = fixture()
        evidence.receipt_binding(receipt, manifest, snapshot, copy.deepcopy(snapshot))
        self.assertEqual([len(run['checks']) for run in receipt['runs']], [20, 19, 20, 19])
        source_root = (Path(os.environ['GITHUB_WORKSPACE']) / 'reading-app'
                       if os.environ.get('GITHUB_WORKSPACE') else Path(__file__).resolve().parents[2])
        source = subprocess.check_output(['git', 'show', f'{evidence.APP_SHA}:{evidence.SCRIPT}'], cwd=source_root).decode()
        self.assertEqual(hashlib.sha256(source.encode()).hexdigest(), evidence.SCRIPT_SHA256)
        names = [match.group(2) for match in re.finditer(r'\bcheck\(([\'"`])(.+?)\1', source)]
        expanded = []
        for name in names:
            expanded.extend([name.replace('${direction}', direction) for direction in ('Shift+Tab', 'Tab')]
                            if '${direction}' in name else [name])
        self.assertEqual(Counter(expanded), Counter(evidence.expected_checks('inspect')))
        self.assertEqual(len(expanded), 20)

    def test_historical_receipt_and_another_workflow_run_cannot_be_relabelled(self):
        manifest, snapshot, receipt = fixture()
        receipt['bundle_manifest_sha256'] = 'old-v6-manifest'
        with self.assertRaisesRegex(ValueError, 'different source generation'):
            evidence.receipt_binding(receipt, manifest, snapshot, snapshot)
        manifest, snapshot, receipt = fixture()
        after = copy.deepcopy(snapshot)
        after['context']['run_id'] = '124'
        with self.assertRaisesRegex(ValueError, 'binding changed'):
            evidence.receipt_binding(receipt, manifest, snapshot, after)

    def test_route_or_check_omission_duplicate_skip_and_page_error_fail(self):
        manifest, snapshot, original = fixture()
        mutations = [lambda r: r['runs'].pop(),
                     lambda r: r['runs'][0]['checks'].pop(),
                     lambda r: r['runs'][0]['checks'].append(r['runs'][0]['checks'][0]),
                     lambda r: r['runs'][0].update(status='skipped'),
                     lambda r: r['runs'][0]['errors'].append('late callback error')]
        for change in mutations:
            receipt = copy.deepcopy(original)
            change(receipt)
            with self.subTest(change=change), self.assertRaises(ValueError):
                evidence.receipt_binding(receipt, manifest, snapshot, snapshot)

    def test_source_mutation_and_wrong_rebuild_are_rejected(self):
        manifest, snapshot, receipt = fixture()
        after = copy.deepcopy(snapshot)
        after['application']['tracked_sha256'] = 'changed-source'
        with self.assertRaisesRegex(ValueError, 'binding changed'):
            evidence.receipt_binding(receipt, manifest, snapshot, after)
        comparison = {'errors': [], 'committed_manifest_sha256': evidence.MANIFEST_SHA256,
                      'pin': {'commit': evidence.UPSTREAM_SHA}, 'node': 'v24.22.1',
                      'package_manager': 'pnpm@10.32.1', 'public_plan': {'selectedCount': 42},
                      'rebuilt_payloads': manifest['artifacts']}
        evidence.rebuilt_binding(comparison, manifest)
        comparison['errors'] = ['Payload bytes differ: app.js']
        with self.assertRaisesRegex(ValueError, 'rebuild did not pass'):
            evidence.rebuilt_binding(comparison, manifest)

    def test_payload_hash_and_screenshot_presence_are_real_file_checks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'app.js').write_bytes(b'x')
            (root / 'build-manifest.json').write_text('{}')
            manifest = {'artifacts': {'app.js': {'bytes': 1, 'sha256': hashlib.sha256(b'x').hexdigest()}}}
            evidence.static_assets(root, manifest)
            (root / 'app.js').write_bytes(b'y')
            with self.assertRaisesRegex(ValueError, 'payload differs'):
                evidence.static_assets(root, manifest)
            for tab, width in evidence.ROUTES:
                (root / f'{tab}-{width}.png').write_bytes(b'\x89PNG\r\n\x1a\nunit-only-placeholder')
            self.assertEqual(len(evidence.screenshots(root)), 4)
            (root / 'im-390.png').unlink()
            with self.assertRaisesRegex(ValueError, 'Missing real screenshot'):
                evidence.screenshots(root)

    def test_business_write_or_different_browser_cannot_pass(self):
        manifest, snapshot, receipt = fixture()
        receipt['requests'].append({'method': 'POST', 'path': '/api/submit'})
        with self.assertRaisesRegex(ValueError, 'non-read HTTP'):
            evidence.receipt_binding(receipt, manifest, snapshot, snapshot)
        manifest, snapshot, receipt = fixture()
        receipt['browser']['version'] = 'old-browser'
        with self.assertRaisesRegex(ValueError, 'another browser'):
            evidence.receipt_binding(receipt, manifest, snapshot, snapshot)


if __name__ == '__main__':
    unittest.main()
