#!/usr/bin/env python3
"""Read the original CI ZIP; never infer an unobserved compiler input closure."""
import argparse
from collections import Counter
import hashlib
import io
import json
from pathlib import Path
import zipfile

EXPECTED_ZIP_SHA256 = 'f6de891ffe2529c1d8de780939ed221bd15d5c5a24dbac33c11f38a04d75f986'
EXPECTED_ZIP_BYTES = 2_735_684
SCOPES = ('base-production', 'candidate-production', 'base-all-targets', 'candidate-all-targets')


def digest(path):
    result = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            result.update(chunk)
    return result.hexdigest()


def inspect(archive):
    if archive.stat().st_size != EXPECTED_ZIP_BYTES or digest(archive) != EXPECTED_ZIP_SHA256:
        raise ValueError('Input is not the retained original CI 43968540714 Clippy ZIP')
    output = {
        'scope': 'read-only analysis of original records, not a compiler-input manifest',
        'run_id': 43968540714,
        'zip_bytes': EXPECTED_ZIP_BYTES,
        'zip_sha256': EXPECTED_ZIP_SHA256,
        'compiler_inputs_complete': False,
        'absence_from_target_roots_proves_unused': False,
        'new_build_or_product_execution': False,
        'streams': {},
    }
    with zipfile.ZipFile(archive) as source:
        members = source.infolist()
        if len({item.filename for item in members}) != len(members):
            raise ValueError('Duplicate original ZIP member')
        if sum(item.file_size for item in members) > 128 * 1024 * 1024:
            raise ValueError('Original ZIP exceeds the bounded read budget')
        output['original_members'] = []
        for item in members:
            hashed = hashlib.sha256()
            with source.open(item) as member:
                for chunk in iter(lambda: member.read(1024 * 1024), b''):
                    hashed.update(chunk)
            output['original_members'].append({
                'path': item.filename, 'bytes': item.file_size,
                'crc32': f'{item.CRC:08x}', 'sha256': hashed.hexdigest(),
            })
        output['original_members'].sort(key=lambda item: item['path'])

        def member_named(name):
            matches = [item.filename for item in members if Path(item.filename).name == name]
            if len(matches) != 1:
                raise ValueError(f'Expected exactly one original {name}')
            return matches[0]

        ratchet = json.loads(source.read(member_named('ratchet.json')))
        output['baseline'] = ratchet['baseline']
        output['candidate_checkout'] = ratchet['candidate']
        output['toolchain'] = ratchet['toolchain']
        output['original_gate_errors'] = ratchet['errors']
        output['original_comparison_scope_names'] = sorted(ratchet['comparisons'])
        output['original_compiler_flags'] = ratchet['flags']
        for scope in SCOPES:
            reasons = Counter()
            fresh = Counter()
            kinds = Counter()
            macros = set()
            artifact_keys = set()
            build_keys = set()
            generated_directories = set()
            finished = []
            record_count = 0
            with source.open(member_named(scope + '.jsonl')) as raw:
                for line in io.TextIOWrapper(raw, encoding='utf-8'):
                    record = json.loads(line)
                    record_count += 1
                    reason = record['reason']
                    reasons[reason] += 1
                    if reason == 'compiler-artifact':
                        artifact_keys.update(record)
                        fresh[str(record['fresh']).lower()] += 1
                        kinds.update(record['target']['kind'])
                        if 'proc-macro' in record['target']['kind']:
                            macros.add(record['package_id'])
                    elif reason == 'build-script-executed':
                        build_keys.update(record)
                        generated_directories.add(record['out_dir'])
                    elif reason == 'build-finished':
                        finished.append(record['success'])
            output['streams'][scope] = {
                'records': record_count, 'reasons': dict(sorted(reasons.items())),
                'artifact_fresh': dict(sorted(fresh.items())),
                'artifact_target_kinds': dict(sorted(kinds.items())),
                'artifact_field_names': sorted(artifact_keys),
                'build_script_field_names': sorted(build_keys),
                'proc_macro_artifact_packages': sorted(macros),
                'out_dir_values': sorted(generated_directories),
                'build_finished': finished,
            }
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('original_zip', type=Path)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    result = inspect(args.original_zip)
    with args.output.open('x', encoding='utf-8') as output:
        json.dump(result, output, indent=2, sort_keys=True)
        output.write('\n')


if __name__ == '__main__':
    main()
