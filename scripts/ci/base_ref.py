#!/usr/bin/env python3
"""Resolve the independent, immutable baseline supplied by the GitHub event."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT, text=True).strip()


def resolve_base(explicit=None):
    supplied = explicit
    if supplied is None:
        event = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())
        name = os.environ['GITHUB_EVENT_NAME']
        if name == 'pull_request':
            supplied = event['pull_request']['base']['sha']
        elif name == 'merge_group':
            supplied = event['merge_group']['base_sha']
        elif name == 'push':
            supplied = event['before']
        elif name == 'workflow_dispatch':
            supplied = event['inputs']['base-sha']
        else:
            raise ValueError(f'No independent baseline for event {name}')
    if not re.fullmatch(r'[0-9a-f]{40}', supplied or '') or supplied == '0' * 40:
        raise ValueError('The baseline must be a full, nonzero immutable commit SHA')
    resolved = git('rev-parse', '--verify', supplied + '^{commit}')
    head = git('rev-parse', 'HEAD')
    if resolved != supplied or resolved == head:
        raise ValueError('The candidate cannot supply its own lint/boundary baseline')
    subprocess.run(['git', 'merge-base', '--is-ancestor', resolved, head], cwd=ROOT, check=True)
    return resolved


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', help='full independently reviewed SHA for a local reproduction')
    args = parser.parse_args()
    sha = resolve_base(args.base)
    print(sha)
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            output.write(f'sha={sha}\n')
    receipt = ROOT / '.ci/evidence/base.json'
    receipt.parent.mkdir(parents=True, exist_ok=True)
    receipt.write_text(json.dumps({'baseline': sha, 'candidate': git('rev-parse', 'HEAD')}, indent=2) + '\n')


if __name__ == '__main__':
    main()
