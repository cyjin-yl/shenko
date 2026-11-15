#!/usr/bin/env python3
"""Compare real same-tool workspace diagnostics with an immutable PR baseline.

Production and all-target diagnostics have separate per-package/per-lint counts.
No checked-in warning budget is used. The same compiler enumerates its default
Rust warnings for the workspace edition once. Force-warn keeps those warnings
and Clippy's selected lints visible through allow/expect attributes; the source-
policy comparison also rejects new suppressions, ignored tests and lint changes.
"""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib

from base_ref import ROOT, resolve_base

TOOLCHAIN = tomllib.loads((ROOT / 'rust-toolchain.toml').read_text())['toolchain']['channel']
EDITION = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['edition']
CLIPPY_LINTS = ('clippy::all', 'clippy::unwrap_used', 'clippy::expect_used')


def probe_rust_lints(rustc, environment, names, output, label):
    """The help table can include unstable lints the same stable rustc rejects."""
    flags = [argument for name in names for argument in ('--force-warn', name)]
    metadata_path = output / f'rustc-lint-probe-{label}.rmeta'
    command = [rustc, '--edition', EDITION, '--crate-name', 'shenko_ci_lint_probe',
               '--emit=metadata', '--error-format=json', '-o', str(metadata_path), *flags, '-']
    source = 'fn main() {}\n'
    result = subprocess.run(command, cwd=ROOT, env=environment, input=source,
                            text=True, capture_output=True)
    diagnostics_path = output / f'rustc-lint-probe-{label}.jsonl'
    stdout_path = output / f'rustc-lint-probe-{label}.stdout.log'
    diagnostics_path.write_text(result.stderr)
    stdout_path.write_text(result.stdout)
    unavailable = set()
    for line in result.stderr.splitlines():
        diagnostic = json.loads(line)
        if (diagnostic.get('code') or {}).get('code') != 'unknown_lints':
            continue
        reported = re.fullmatch(r'unknown lint: `([a-z][a-z0-9_-]*)`', diagnostic['message'])
        if not reported or reported[1].replace('_', '-') not in names:
            raise RuntimeError(f'Unexpected unknown lint in compiler probe; retained {diagnostics_path}')
        unavailable.add(reported[1].replace('_', '-'))
    if result.returncode:
        raise RuntimeError(f'Compiler lint probe failed ({result.returncode}); retained {diagnostics_path}')
    return sorted(unavailable), {
        'command': command, 'source': source, 'returncode': result.returncode,
        'diagnostics_path': str(diagnostics_path), 'stdout_path': str(stdout_path),
        'metadata_path': str(metadata_path),
    }


def lint_policy(output):
    # Bind discovery and both source graphs to one native compiler. Inherited
    # flags or wrappers must not replace that compiler or suppress its lints.
    environment = dict(os.environ, RUSTUP_TOOLCHAIN=TOOLCHAIN)
    for key in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CLIPPY_ARGS',
                'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER'):
        environment.pop(key, None)
    rustc = subprocess.check_output(
        ['rustup', 'which', '--toolchain', TOOLCHAIN, 'rustc'], cwd=ROOT, env=environment, text=True).strip()
    environment['RUSTC'] = rustc
    command = [rustc, '--edition', EDITION, '-W', 'help']
    listing = subprocess.check_output(command, cwd=ROOT, env=environment, text=True)
    listing_path = output / 'rustc-lints.log'
    listing_path.write_text(listing)
    _, start, table = listing.partition('Lint checks provided by rustc:\n')
    table, end, _ = table.partition('\nLint groups provided by rustc:')
    rows = [line.split(None, 2) for line in table.splitlines() if line.strip()]
    if not start or not end or rows[:2] != [['name', 'default', 'meaning'], ['----', '-------', '-------']]:
        raise RuntimeError(f'Unrecognized rustc lint table; retained {listing_path}')
    levels = {}
    for row in rows[2:]:
        if (len(row) != 3 or not re.fullmatch(r'[a-z][a-z0-9-]*', row[0])
                or row[1] not in ('allow', 'expect', 'warn', 'force-warn', 'deny', 'forbid')
                or row[0] in levels):
            raise RuntimeError(f'Unrecognized rustc lint row {row!r}; retained {listing_path}')
        levels[row[0]] = row[1]
    # The special `warnings` group cannot be force-warned. Enumerate individual
    # warn-by-default IDs instead: plain -W warnings permits source-level allow.
    # Use this edition's defaults so deny-by-default lints are not downgraded.
    warnings = sorted(name for name, level in levels.items() if level == 'warn' and name != 'warnings')
    if not warnings:
        raise RuntimeError(f'No default Rust warning lints discovered; retained {listing_path}')
    discovered = warnings
    unavailable, original_probe = probe_rust_lints(rustc, environment, discovered, output, 'original')
    warnings = [name for name in discovered if name not in unavailable]
    if not warnings:
        raise RuntimeError(f'No compiler-accepted warning lints remain; retained {listing_path}')
    rejected, filtered_probe = probe_rust_lints(rustc, environment, warnings, output, 'filtered')
    if rejected:
        raise RuntimeError(f'Filtered compiler lint probe still rejects {rejected}; retained {filtered_probe["diagnostics_path"]}')
    flags = [argument for name in (*warnings, *CLIPPY_LINTS) for argument in ('--force-warn', name)]
    policy = {
        'toolchain': TOOLCHAIN, 'edition': EDITION, 'rustc_path': rustc,
        'rustc': subprocess.check_output([rustc, '-Vv'], cwd=ROOT, env=environment, text=True),
        'clippy': subprocess.check_output(
            ['cargo', 'clippy', '--version'], cwd=ROOT, env=environment, text=True).strip(),
        'lint_discovery_command': command, 'rust_warning_lints': warnings, 'flags': flags,
        'discovered_rust_warning_lints': discovered,
        'unavailable_rust_warning_lints': unavailable,
        'rust_lint_probes': {'original': original_probe, 'filtered': filtered_probe},
    }
    # Retain the shared policy even if the first compilation fails.
    (output / 'lint-policy.json').write_text(json.dumps(policy, indent=2) + '\n')
    return environment, policy


def tracked(root):
    return subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).decode().split('\0')[:-1]


def rust_tokens(source):
    """Read attributes without mistaking strings or nested comments for code."""
    index = 0
    while index < len(source):
        if source[index].isspace():
            index += 1
        elif source.startswith('//', index):
            end = source.find('\n', index)
            index = len(source) if end < 0 else end + 1
        elif source.startswith('/*', index):
            depth = 1
            index += 2
            while depth:
                if index >= len(source):
                    raise ValueError('Unterminated Rust block comment')
                if source.startswith('/*', index):
                    depth += 1
                    index += 2
                elif source.startswith('*/', index):
                    depth -= 1
                    index += 2
                else:
                    index += 1
        else:
            raw = re.match(r'(?:br|cr|r)(#*)"', source[index:])
            if raw:
                end = source.find('"' + raw[1], index + raw.end())
                if end < 0:
                    raise ValueError('Unterminated Rust raw string')
                end += 1 + len(raw[1])
            elif source[index] == '"':
                end = index + 1
                while end < len(source) and source[end] != '"':
                    end += 2 if source[end] == '\\' else 1
                end += 1
            else:
                character = re.match(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'", source[index:])
                word = re.match(r'[A-Za-z_][A-Za-z_0-9]*', source[index:])
                end = index + (character.end() if character else word.end() if word else 1)
            yield source[index:end]
            index = end


def source_policy(root):
    controls = Counter()
    configuration = {}
    for name in tracked(root):
        path = root / name
        if name.endswith('.rs'):
            tokens = list(rust_tokens(path.read_text()))
            index = 0
            while index < len(tokens):
                if tokens[index] != '#':
                    index += 1
                    continue
                start = index
                index += 1
                if index < len(tokens) and tokens[index] == '!':
                    index += 1
                if index >= len(tokens) or tokens[index] != '[':
                    continue
                depth = 1
                index += 1
                while depth and index < len(tokens):
                    depth += (tokens[index] == '[') - (tokens[index] == ']')
                    index += 1
                attribute = tokens[start:index]
                if {'allow', 'expect', 'ignore'} & set(attribute):
                    controls[(name, ' '.join(attribute))] += 1
        elif path.name == 'Cargo.toml':
            manifest = tomllib.loads(path.read_text())
            for section, table in [('lints', manifest.get('lints', {})),
                                   ('workspace.lints', manifest.get('workspace', {}).get('lints', {}))]:
                for tool, entries in table.items():
                    if not isinstance(entries, dict):
                        continue
                    for lint, value in entries.items():
                        level = value.get('level') if isinstance(value, dict) else value
                        if level in ('allow', 'expect'):
                            controls[(name, f'{section}.{tool}.{lint}={json.dumps(value, sort_keys=True)}')] += 1
        elif path.name in ('clippy.toml', '.clippy.toml') or name.endswith(('.cargo/config', '.cargo/config.toml')):
            configuration[name] = path.read_bytes()
    return controls, configuration


def collect(root, target, output, selection, environment, lint_flags):
    environment = dict(environment, CARGO_TARGET_DIR=str(target))
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--locked', '--no-deps', '--format-version=1'], cwd=root, env=environment, text=True))
    members = set(metadata['workspace_members'])
    packages = {package['id']: package['name'] for package in metadata['packages'] if package['id'] in members}
    command = ['cargo', 'clippy', '--workspace', '--locked', '-j2', '--message-format=json', *selection, '--', *lint_flags]
    counts = Counter()
    artifacts = set()
    finished = False
    print(f'{root}: {" ".join(command)}', flush=True)
    with output.with_suffix('.jsonl').open('w') as log, output.with_suffix('.stderr.log').open('w') as errors:
        process = subprocess.Popen(command, cwd=root, env=environment, text=True, stdout=subprocess.PIPE, stderr=errors)
        for line in process.stdout:
            log.write(line)
            log.flush()
            record = json.loads(line)
            if record.get('reason') == 'build-finished':
                finished = record['success']
            if record.get('package_id') not in packages:
                continue
            if record.get('reason') == 'compiler-artifact':
                artifacts.add(packages[record['package_id']])
            if record.get('reason') == 'compiler-message':
                diagnostic = record['message']
                if diagnostic['level'] in ('warning', 'error'):
                    print(diagnostic.get('rendered') or diagnostic['message'], flush=True)
                if diagnostic['level'] == 'warning':
                    code = (diagnostic.get('code') or {}).get('code') or diagnostic['message']
                    counts[(packages[record['package_id']], code)] += 1
        status = process.wait()
    if status or not finished or not artifacts:
        raise RuntimeError(f'Clippy failed ({status}) in {root}; retained {output.with_suffix(".stderr.log")}')
    return counts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', required=True)
    parser.add_argument('--output', type=Path, default=ROOT / '.ci/evidence/clippy')
    parser.add_argument('--target-root', type=Path, default=ROOT / '.ci/target/clippy')
    args = parser.parse_args()
    base = resolve_base(args.base)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    environment, policy = lint_policy(output)
    baseline = output / 'baseline'
    subprocess.run(['git', 'worktree', 'add', '--detach', str(baseline), base], cwd=ROOT, check=True)
    base_controls, base_configuration = source_policy(baseline)
    current_controls, current_configuration = source_policy(ROOT)
    errors = [f'New suppression/ignored test: {name}: {attribute}'
              for name, attribute in sorted((current_controls - base_controls).elements())]
    if base_configuration != current_configuration:
        errors.append('Clippy/Cargo configuration differs from the immutable baseline; lint policy cannot lower its own gate')
    comparisons = {}
    for scope, selection in [('production', ['--lib', '--bins']), ('all-targets', ['--all-targets'])]:
        old = collect(baseline, args.target_root.resolve() / 'baseline' / scope, output / f'base-{scope}', selection,
                      environment, policy['flags'])
        new = collect(ROOT, args.target_root.resolve() / 'candidate' / scope, output / f'candidate-{scope}', selection,
                      environment, policy['flags'])
        comparisons[scope] = {
            'baseline': [{'package': key[0], 'lint': key[1], 'count': count} for key, count in sorted(old.items())],
            'candidate': [{'package': key[0], 'lint': key[1], 'count': count} for key, count in sorted(new.items())],
        }
        errors.extend(f'{scope}: {package}: {lint} increased by {count}'
                      for (package, lint), count in sorted((new - old).items()))
    receipt = {'baseline': base, 'candidate': subprocess.check_output(
        ['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
        **policy, 'comparisons': comparisons, 'errors': errors}
    (output / 'ratchet.json').write_text(json.dumps(receipt, indent=2) + '\n')
    if errors:
        raise SystemExit('\n'.join(errors))
    print('Production and all-target workspace diagnostics do not exceed the immutable baseline')


if __name__ == '__main__':
    main()
