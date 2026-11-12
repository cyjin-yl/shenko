#!/usr/bin/env python3
"""Check the dependency boundary between the kernel and its plugins.

This is a structure-only check for docs/specs/kernel.md#kernel-7. It reads the
workspace manifests and nothing else: it does not build, run or verify the
Shenko product, and it says nothing about whether a crate's code is correct.

Two directions are checked:

* ``plugin_reaches_core``: a plugin crate depends on a core-internal crate.
  A plugin may depend on the interface crates only.
* ``core_depends_on_plugin``: a core crate depends on a plugin crate.
* ``interface_depends_outward``: an interface crate depends on anything that
  is not an interface crate.

The current violations are recorded in docs/state/kernel-boundary.json. That
baseline is a ratchet: a new violation fails the check, and a violation that
disappeared must be removed from the baseline with ``--update`` so it cannot
come back unnoticed. ``--update`` refuses to add violations.

Which crate is interface, core or assembly is data in the baseline file, not
code here. Every other workspace member is a plugin.
"""
from pathlib import Path
import json
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / 'docs/state/kernel-boundary.json'
RULES = ('plugin_reaches_core', 'core_depends_on_plugin', 'interface_depends_outward')


def member_manifests():
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']
    excluded = set(workspace.get('exclude', []))
    for member in workspace['members']:
        if member not in excluded:
            yield ROOT / member / 'Cargo.toml'


def dependency_tables(manifest):
    """Normal and build dependencies, including target-specific ones.

    Dev-dependencies are not part of the shipped dependency graph: a test may
    assemble the whole product without making the crate depend on it.
    """
    for key in ('dependencies', 'build-dependencies'):
        yield manifest.get(key, {})
    for target in manifest.get('target', {}).values():
        for key in ('dependencies', 'build-dependencies'):
            yield target.get(key, {})


def workspace_graph():
    packages = {}
    for path in member_manifests():
        manifest = tomllib.loads(path.read_text())
        packages[manifest['package']['name']] = manifest
    graph = {}
    for name, manifest in packages.items():
        edges = set()
        for table in dependency_tables(manifest):
            for key, spec in table.items():
                target = spec.get('package', key) if isinstance(spec, dict) else key
                if target in packages and target != name:
                    edges.add(target)
        graph[name] = sorted(edges)
    return graph


def violations(graph, roles):
    interface = set(roles['interface'])
    core = set(roles['core'])
    assembly = set(roles['assembly'])
    found = []
    for source, targets in sorted(graph.items()):
        if source in assembly:
            continue
        for target in targets:
            if source in interface:
                if target not in interface:
                    found.append({'rule': 'interface_depends_outward', 'from': source, 'to': target})
            elif source in core:
                if target not in core and target not in interface:
                    found.append({'rule': 'core_depends_on_plugin', 'from': source, 'to': target})
            elif target in core:
                found.append({'rule': 'plugin_reaches_core', 'from': source, 'to': target})
    return found


def key(item):
    return (item['rule'], item['from'], item['to'])


def main():
    update = '--update' in sys.argv[1:]
    errors = []
    baseline = json.loads(BASELINE.read_text())
    if baseline.get('schema_version') != 1:
        errors.append('Unsupported kernel boundary baseline schema')
    roles = baseline['roles']
    graph = workspace_graph()
    for role in ('interface', 'core', 'assembly'):
        for name in roles[role]:
            if name not in graph:
                errors.append(f'Unknown workspace member in roles.{role}: {name}')
    claimed = [name for role in ('interface', 'core', 'assembly') for name in roles[role]]
    if len(claimed) != len(set(claimed)):
        errors.append('A crate has more than one role')

    current = violations(graph, roles)
    recorded = baseline['violations']
    for item in recorded:
        if item.get('rule') not in RULES:
            errors.append(f'Unknown rule in baseline: {item}')
    current_keys = {key(item) for item in current}
    recorded_keys = {key(item) for item in recorded}
    new = sorted(current_keys - recorded_keys)
    resolved = sorted(recorded_keys - current_keys)

    for rule, source, target in new:
        errors.append(f'New boundary violation ({rule}): {source} -> {target}')
    if update and not errors:
        baseline['violations'] = sorted(current, key=key)
        baseline['counts'] = {rule: sum(1 for item in current if item['rule'] == rule) for rule in RULES}
        BASELINE.write_text(json.dumps(baseline, ensure_ascii=False, indent=2) + '\n')
        resolved_pending = []
    else:
        resolved_pending = resolved
        for rule, source, target in resolved:
            errors.append(
                f'Baseline is stale ({rule}): {source} -> {target} no longer exists; '
                'run with --update to lock the improvement in')
        expected = {rule: sum(1 for item in recorded if item['rule'] == rule) for rule in RULES}
        if baseline.get('counts') != expected:
            errors.append('Baseline counts do not match its recorded violations')

    result = {
        'kind': 'kernel_boundary_structure_only',
        'product_tested': False,
        'workspace_members': len(graph),
        'plugins': len(graph) - len(set(claimed)),
        'violations': {rule: sum(1 for item in current if item['rule'] == rule) for rule in RULES},
        'new_violations': [' -> '.join(item[1:]) for item in new],
        'resolved_violations': [' -> '.join(item[1:]) for item in resolved_pending],
        'baseline_updated': bool(update and not errors),
        'errors': errors,
    }
    print(json.dumps(result, ensure_ascii=False, indent=2))
    sys.exit(1 if errors else 0)


if __name__ == '__main__':
    main()
