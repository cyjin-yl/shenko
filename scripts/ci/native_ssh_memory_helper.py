#!/usr/bin/env python3
"""Provision and run the two existing native SSH memory-helper proofs on Linux.

Inside the declared development container:
  python3 scripts/ci/native_ssh_memory_helper.py prepare
  python3 scripts/ci/native_ssh_memory_helper.py run --name <full Rust test name>
Use a fresh --output directory when retaining another attempt. Preparation builds
and installs the normal default-feature CLI at the fixture's /fixed-shenko-cli;
Cargo/build scripts stay outside the restricted execution scope.

Only the selected test executable and its descendants receive the inherited
libseccomp filter and the two fixture variables. Every socket() family, endpoint
connection/listening syscall and io_uring entrypoint is denied with EPERM.
Anonymous AF_UNIX socketpair IPC remains available for Rust/Tokio subprocess
startup; it cannot create or connect a named/network endpoint. All other
socketpair families are denied. The existing "all-sockets-denied" marker is set
only after installing and probing this policy. No privileges or namespaces are
needed to load it: PR_SET_NO_NEW_PRIVS prevents privilege gain across exec.

This is endpoint isolation, not filesystem, PID or mount isolation. It does not
exercise the real TCP opener, login shell, a remote host, or a model. Preparation
needs permission to create the fixed CLI path (the nightly development container
runs as root); execution requires Linux seccomp and libseccomp.so.2. Missing
support, build failures, zero-case selections and proof failures are fatal.
"""
import argparse
import ctypes
import errno
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = Path(__file__).resolve()
FIXED_CLI = Path('/fixed-shenko-cli')
PREFIX = 'recovery_integration_tests::native_ssh_memory_helper::'
CASES = {
    PREFIX + 'configured_identity_actual_helper_receipt_survives_lost_reply_without_replay': 'lost-reply',
    PREFIX + 'configured_identity_does_not_authorize_wrong_host_pin_or_helper': 'identity-refusal',
}
DENIED = ('socket', 'connect', 'bind', 'listen', 'accept', 'accept4',
          'io_uring_setup', 'io_uring_enter', 'io_uring_register')


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def fingerprint(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(chunk)
    return {'path': str(path), 'bytes': path.stat().st_size, 'sha256': digest.hexdigest()}


def require_file(expected):
    path = Path(expected['path'])
    if path.is_symlink() or fingerprint(path) != expected:
        raise RuntimeError(f'Prepared artifact changed: {path}')
    return path


def inputs():
    names = ['Cargo.toml', 'Cargo.lock', 'bin/shenko/Cargo.toml',
             'bin/shenko/src/main.rs', 'bin/shenko/src/lib.rs',
             'bin/shenko/src/native_ssh_memory_helper_tests.rs',
             'crates/shenko-exec-ssh/src/remote_helper.py',
             '.github/workflows/nightly.yml', str(SCRIPT.relative_to(ROOT))]
    return {name: fingerprint(ROOT / name) for name in names}


def command(argv, output, label, environment=None):
    """Keep each original output and raw exit status, including failed attempts."""
    argv = list(map(str, argv))
    receipt = {'command': argv, 'started_unix': time.time()}
    started = time.monotonic()
    print('Running ' + ' '.join(argv), flush=True)
    try:
        with (output / f'{label}.stdout.log').open('x') as stdout, \
                (output / f'{label}.stderr.log').open('x') as stderr:
            completed = subprocess.run(argv, cwd=ROOT, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=stdout,
                                       stderr=stderr, close_fds=True)
        receipt['returncode'] = completed.returncode
    except BaseException as error:
        receipt['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        receipt['elapsed_seconds'] = time.monotonic() - started
        receipt['finished_unix'] = time.time()
        write_json(output / f'{label}.json', receipt)
    if completed.returncode:
        raise RuntimeError(f'{label} exited {completed.returncode}; see {output}')
    return (output / f'{label}.stdout.log').read_text()


def cargo_artifact(arguments, output, label, kind, test):
    stdout = command(['cargo', *arguments, '--locked', '-j2', '-p', 'shenko',
                      '--message-format=json'], output, label)
    executables = set()
    for line in stdout.splitlines():
        message = json.loads(line)
        if (message.get('reason') == 'compiler-artifact'
                and message['target']['name'] == 'shenko'
                and kind in message['target']['kind']
                and message['profile']['test'] == test
                and Path(message['manifest_path']).resolve() == ROOT / 'bin/shenko/Cargo.toml'
                and message.get('executable')):
            executables.add(Path(message['executable']).resolve())
    if len(executables) != 1:
        raise RuntimeError(f'Expected one actual shenko {kind} executable, found {executables}')
    return executables.pop()


def prepare(output):
    output.mkdir(parents=True, exist_ok=False)
    receipt = {'status': 'preparing', 'inputs': inputs(),
               'candidate_from_github_sha': os.environ.get('GITHUB_SHA')}
    write_json(output / 'preparation.json', receipt)
    try:
        executable = cargo_artifact(['build', '--bin', 'shenko'], output, 'cli-build', 'bin', False)
        receipt['built_cli'] = fingerprint(executable)
        if FIXED_CLI.exists() or FIXED_CLI.is_symlink():
            if (FIXED_CLI.is_symlink() or not FIXED_CLI.is_file()
                    or fingerprint(FIXED_CLI)['sha256'] != receipt['built_cli']['sha256']):
                raise RuntimeError('Refusing to replace a different existing /fixed-shenko-cli')
        else:
            with executable.open('rb') as source, FIXED_CLI.open('xb') as destination:
                shutil.copyfileobj(source, destination)
            FIXED_CLI.chmod(0o555)
        receipt['installed_cli'] = fingerprint(FIXED_CLI)
        if (receipt['installed_cli']['sha256'] != receipt['built_cli']['sha256']
                or not os.access(FIXED_CLI, os.X_OK)):
            raise RuntimeError('Installed normal CLI is not the executable Cargo just built')
        receipt['status'] = 'prepared'
    except BaseException as error:
        receipt.update(status='failed', error=f'{type(error).__name__}: {error}')
        raise
    finally:
        write_json(output / 'preparation.json', receipt)


def preparation(output):
    receipt = json.loads((output / 'preparation.json').read_text())
    if receipt['status'] != 'prepared' or receipt['inputs'] != inputs():
        raise RuntimeError('Normal CLI preparation is missing, failed or from different source inputs')
    if receipt['installed_cli']['path'] != str(FIXED_CLI):
        raise RuntimeError('Prepared CLI is not installed at the fixture-required path')
    require_file(receipt['installed_cli'])
    return receipt


def fixture_environment(directory):
    # No inherited account, agent, model, proxy, loader or Cargo environment.
    return {'PATH': '/usr/bin:/bin', 'HOME': str(directory / 'home'),
            'TMPDIR': str(directory / 'tmp'), 'LANG': 'C.UTF-8', 'TZ': 'UTC',
            'RUST_BACKTRACE': '1'}


def run(output, name):
    directory = output / CASES[name]
    directory.mkdir(parents=True, exist_ok=False)
    receipt = {'name': name, 'status': 'preparing'}
    write_json(directory / 'result.json', receipt)
    try:
        prepared = preparation(output)
        receipt['installed_cli'] = prepared['installed_cli']
        executable = cargo_artifact(['test', '--lib', '--no-run'], directory,
                                    'test-build', 'lib', True)
        receipt['test_executable'] = fingerprint(executable)
        for child in ('home', 'tmp'):
            (directory / child).mkdir(mode=0o700)
        environment = fixture_environment(directory)
        listing = command([executable, name, '--exact', '--ignored', '--list'],
                          directory, 'listing', environment)
        selected = [line for line in listing.splitlines() if line.endswith(': test')]
        if selected != [name + ': test']:
            raise RuntimeError(f'Expected exactly {name}, found {selected}')
        receipt['status'] = 'running'
        write_json(directory / 'result.json', receipt)
        stdout = command([sys.executable, '-I', '-S', SCRIPT, '_exec',
                          '--output', output, '--name', name], directory, 'proof', environment)
        print(stdout, end='', flush=True)
        if not any(line.startswith('test result: ok. 1 passed; 0 failed; 0 ignored;')
                   for line in stdout.splitlines()):
            raise RuntimeError('Native proof did not report exactly one executed, passing test')
        receipt['status'] = 'passed'
    except BaseException as error:
        receipt.update(status='failed', error=f'{type(error).__name__}: {error}')
        raise
    finally:
        write_json(directory / 'result.json', receipt)


class ArgumentComparison(ctypes.Structure):
    _fields_ = [('arg', ctypes.c_uint), ('op', ctypes.c_int),
                ('datum_a', ctypes.c_uint64), ('datum_b', ctypes.c_uint64)]


def restrict_endpoints():
    """Install an inheritable native-ABI filter; never silently omit a rule."""
    if sys.platform != 'linux':
        raise RuntimeError('The native memory-helper fixture requires Linux seccomp')
    library = ctypes.CDLL('libseccomp.so.2', use_errno=True)
    library.seccomp_init.argtypes = [ctypes.c_uint32]
    library.seccomp_init.restype = ctypes.c_void_p
    library.seccomp_syscall_resolve_name.argtypes = [ctypes.c_char_p]
    library.seccomp_syscall_resolve_name.restype = ctypes.c_int
    library.seccomp_rule_add_array.argtypes = [ctypes.c_void_p, ctypes.c_uint32,
                                             ctypes.c_int, ctypes.c_uint,
                                             ctypes.POINTER(ArgumentComparison)]
    library.seccomp_rule_add_array.restype = ctypes.c_int
    library.seccomp_load.argtypes = [ctypes.c_void_p]
    library.seccomp_load.restype = ctypes.c_int
    library.seccomp_release.argtypes = [ctypes.c_void_p]
    library.seccomp_release.restype = None
    libc = ctypes.CDLL(None, use_errno=True)
    libc.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong,
                          ctypes.c_ulong, ctypes.c_ulong]
    libc.prctl.restype = ctypes.c_int
    libc.syscall.restype = ctypes.c_long
    if libc.prctl(38, 1, 0, 0, 0) != 0:  # PR_SET_NO_NEW_PRIVS
        raise OSError(ctypes.get_errno(), 'PR_SET_NO_NEW_PRIVS failed')
    context = library.seccomp_init(0x7fff0000)  # SCMP_ACT_ALLOW
    if not context:
        raise RuntimeError('seccomp_init failed')
    numbers = {}
    try:
        for name in (*DENIED, 'socketpair'):
            number = library.seccomp_syscall_resolve_name(name.encode('ascii'))
            if number < 0:
                raise RuntimeError(f'libseccomp cannot resolve required native syscall {name}')
            numbers[name] = number
            # SCMP_CMP_NE: keep only anonymous AF_UNIX process-local IPC.
            comparison = ArgumentComparison(0, 1, socket.AF_UNIX, 0) if name == 'socketpair' else None
            status = library.seccomp_rule_add_array(
                context, 0x00050000 | errno.EPERM, number,
                1 if comparison is not None else 0,
                ctypes.byref(comparison) if comparison is not None else None)
            if status != 0:
                raise OSError(-status, f'seccomp rule failed for {name}')
        status = library.seccomp_load(context)
        if status != 0:
            raise OSError(-status, 'seccomp_load failed')
    finally:
        library.seccomp_release(context)
    if libc.prctl(39, 0, 0, 0, 0) != 1:  # PR_GET_NO_NEW_PRIVS
        raise RuntimeError('no_new_privs was not retained')
    probes = {}
    for family in (socket.AF_INET, socket.AF_INET6, socket.AF_UNIX):
        try:
            created = socket.socket(family, socket.SOCK_STREAM)
        except OSError as error:
            if error.errno != errno.EPERM:
                raise
            probes[family.name] = 'socket denied EPERM'
        else:
            created.close()
            raise RuntimeError(f'socket creation was permitted for {family.name}')
    # Invalid arguments cannot contact an endpoint, even if a rule is broken.
    # EPERM, rather than the ordinary argument error, proves each loaded rule.
    for name in DENIED:
        ctypes.set_errno(0)
        result = libc.syscall(ctypes.c_long(numbers[name]), *[ctypes.c_long(-1)] * 6)
        if result != -1 or ctypes.get_errno() != errno.EPERM:
            raise RuntimeError(f'{name} was not denied with EPERM')
        probes[name] = 'denied EPERM'
    try:
        pair = socket.socketpair(socket.AF_INET, socket.SOCK_STREAM)
    except OSError as error:
        if error.errno != errno.EPERM:
            raise
        probes['socketpair_other_families'] = 'denied EPERM'
    else:
        for descriptor in pair:
            descriptor.close()
        raise RuntimeError('Non-AF_UNIX socketpair was permitted')
    left, right = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    with left, right:
        left.sendall(b'x')
        if right.recv(1) != b'x':
            raise RuntimeError('Anonymous AF_UNIX subprocess IPC is unavailable')
    probes['anonymous_AF_UNIX_socketpair'] = 'local IPC transferred successfully'
    return probes


def execute(output, name):
    directory = output / CASES[name]
    receipt = {'name': name, 'status': 'restricting', 'kernel': list(os.uname())}
    try:
        prepared = preparation(output)
        result = json.loads((directory / 'result.json').read_text())
        if result['name'] != name or result['status'] != 'running':
            raise RuntimeError('Missing exact native proof execution request')
        executable = require_file(result['test_executable'])
        if any(stat.S_ISSOCK(os.fstat(fd).st_mode) for fd in (0, 1, 2)):
            raise RuntimeError('Fixture standard descriptors must not inherit sockets')
        receipt['probes'] = restrict_endpoints()
        receipt.update(status='ready-to-exec', no_new_privs=True,
                       denied_syscalls=list(DENIED),
                       permitted_socketpair='AF_UNIX anonymous local IPC only',
                       installed_cli=prepared['installed_cli'],
                       test_executable=result['test_executable'])
        write_json(directory / 'isolation.json', receipt)
        environment = fixture_environment(directory)
        environment.update(SHENKO_MEMORY_HELPER_FIXTURE='all-sockets-denied',
                           SHENKO_FIXED_CLI=str(FIXED_CLI))
        # Exec preserves seccomp/NNP, and every test/CLI/Python descendant inherits
        # the filter even where the fixture deliberately uses env_clear().
        os.execve(executable, [str(executable), name, '--exact', '--ignored',
                              '--nocapture', '--test-threads=1'], environment)
    except BaseException as error:
        receipt.update(status='failed', error=f'{type(error).__name__}: {error}')
        write_json(directory / 'isolation.json', receipt)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=['prepare', 'run', '_exec'])
    parser.add_argument('--name', choices=CASES)
    parser.add_argument('--output', type=Path, default=ROOT / '.ci/evidence/native-ssh-memory-helper')
    args = parser.parse_args()
    output = args.output.resolve()
    if args.action == 'prepare':
        if args.name:
            parser.error('prepare does not take --name')
        prepare(output)
    else:
        if not args.name:
            parser.error('run/_exec requires the full --name')
        if args.action == 'run':
            run(output, args.name)
        else:
            execute(output, args.name)


if __name__ == '__main__':
    main()
