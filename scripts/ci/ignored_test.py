#!/usr/bin/env python3
"""Run one existing explicitly provisioned native test; a zero-case match fails."""
import argparse
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', required=True)
    parser.add_argument('--name', required=True)
    args = parser.parse_args()
    command = ['cargo', 'test', '--locked', '-j2', '-p', args.package, '--lib', args.name,
               '--', '--exact', '--ignored']
    listing = subprocess.check_output([*command, '--list'], text=True)
    selected = [line for line in listing.splitlines() if line.endswith(': test')]
    if selected != [args.name + ': test']:
        raise SystemExit(f'Expected exactly {args.name}, found {selected}')
    subprocess.run([*command, '--nocapture'], check=True)


if __name__ == '__main__':
    main()
