"""Bounded autonomous-initiation observation for the memory-intervention worker.

What this program does
----------------------
It starts the **actual default-feature CLI binary** in a fresh private root,
records every provider request/response through a pass-through proxy, submits
two genuine user messages through the real Web UI input path
(``UserSubmission`` over ``/ws``), and then reads the durable product state back
with a separately built read-only inspector.

What it deliberately never does
-------------------------------
* it scripts no model answer, so every model output is the served model's own;
* it admits no ``pma.evaluate`` task itself and never asks for one;
* it injects no decision and no reminder text;
* it names no native selector: the submitted user text contains no
  ``[delegate:``, ``pma.`` or ``research.`` token, which the receipt re-verifies;
* it declares no success from keyword matching. Admission is derived from
  durable provenance (a real task bound to the creating step, that step's
  sealed control, its sealed model answer and terminal provider evidence); the
  presence of the words in a model answer is evidence of *which* control the
  model emitted, not a quality judgement.

What it can conclude
--------------------
One of three honest outcomes, recorded with its reason:

* ``initiated`` — a real ``pma.evaluate`` task exists whose creating step's
  sealed model answer contains the delegate control;
* ``admitted_without_model_delegation`` — the worker ran, but no subject
  delegation control produced it (a future non-model trigger, or an external
  admission). This is never reported as model initiation;
* ``not_initiated`` — the observation completed (the subject really ran, the
  bound was reached) and no task exists at all;
* ``inconclusive`` — the observation could not establish any of these (no live
  step, no descriptor in the request, a provenance mismatch, an unreadable
  ledger).

Every other fact is mechanical: descriptor availability in the sealed system
prompt, task/epoch/decision rows, prepared-contribution delivery joins against
the recorded wire, and durable reminder use. Semantic quality, efficacy, chance
outcomes and "the model should have reminded" are explicitly out of scope.

No source is modified. This program does not run by itself inside the
development container: the parent agent executes it against a binding label
whose final default-feature build passed.

Usage::

    python3 scripts/pma_autonomous_initiation_consumer.py \
        --out <fresh output dir> --binding <run_native label dir> \
        --target <cargo target dir> [--repo <runtime worktree>] \
        [--config <cargo config>] [--model-url URL] [--model ID] \
        [--deadline 900] [--max-subject-steps 8]
"""
import argparse, base64, hashlib, http.server, json, os, pathlib, re, shutil, signal, socket, stat, subprocess, threading, time, urllib.error, urllib.request, uuid

SCRIPTS = pathlib.Path(__file__).resolve().parent
# This script lives at <project>/worktrees/<worktree>/scripts, so the project
# root (which owns the sibling runtime worktree and the build evidence tree)
# is three levels up. Both defaults are overridable.
PROJECT = SCRIPTS.parents[2]
DEFAULT_REPO = PROJECT / 'worktrees/deepseek-integration-20261105'
DEFAULT_CONFIG = PROJECT / 'build/ecosystem-compat-20261104/combined-config.toml'
INSPECTOR = SCRIPTS / 'pma_autonomy_inspector'

# Role markers are the product's own authored prompt text, used only to label a
# recorded request. They are never sent to the model and never reworded here.
PMA_WORKER_MARK = '你是记忆干预 worker：'
CHILD_MARK = '运行角色：有明确任务边界的有限模型 child'
SUBJECT_CONTRACT_MARK = '你是一个持续运行的个人 agent'
PMA_SELECTOR = '[delegate:pma.evaluate:1]'
RESEARCH_SELECTORS = ('[delegate:research.reset:1]', '[delegate:research.bank:1]', '[delegate:research.candidate:1]')
# The installed prompt sections name these definitions by kind/version, not by
# the delegate selector form, so availability is detected on the real text.
PMA_MARKER = '记忆干预 worker 已注册'
RESEARCH_MARKERS = ('research.reset/v1', 'research.bank/v1', 'research.candidate/v1')
FOCUS_TASK_KINDS = ('pma.evaluate', 'research.reset', 'research.bank', 'research.candidate')

# Two fresh user inputs. They carry a durable project requirement and then a
# situation that conflicts with it. Neither mentions a mechanism, a selector or
# an expected answer.
FIRST_INPUT = (
    '我接手了季度对账这个项目，先说明一下背景。这个项目已经确认过一条硬性要求：'
    '归档之前必须把备份真正恢复到独立目录并逐条回读，只记录旧文件的校验和不能算备份可用。'
    '今天先熟悉环境，明天再开始动手。'
)
SECOND_INPUT = (
    '现在开始做：我先直接覆盖原库，然后跑一遍新的导出脚本；'
    '跑完拿旧文件的 SHA 比较一下，就能说明备份没有问题。'
)

CREDENTIAL = re.compile(rb'\b(?:sc_|sat_)[0-9a-f]{32}\b')
ENV = os.environ.copy()
ENV.update(
    CARGO_HOME='/usr/local/cargo',
    RUSTUP_HOME='/usr/local/rustup',
    CARGO_BUILD_JOBS='1',
    CMAKE_BUILD_PARALLEL_LEVEL='1',
    MAKEFLAGS='-j1',
    PATH='/usr/local/cargo/bin:' + ENV.get('PATH', ''),
)
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))

sha = lambda path: hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()
sha_bytes = lambda data: hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path = pathlib.Path(path)
    tmp = path.with_name(path.name + '.tmp')
    tmp.write_text(json.dumps(value, ensure_ascii=False, indent=2, sort_keys=False) + '\n')
    os.replace(tmp, path)


def freeze(repo):
    """Content hash of every non-Git, non-target file of the runtime source."""
    files = {}
    def fail(error): raise error
    for parent, dirs, names in os.walk(repo, followlinks=False, onerror=fail):
        parent = pathlib.Path(parent)
        links = [name for name in dirs if (parent / name).is_symlink()]
        dirs[:] = sorted(name for name in dirs if name not in links and not (parent == repo and name in ('.git', 'target')))
        for name in sorted(names + links):
            path = parent / name
            relative = path.relative_to(repo)
            if relative.parts[0] in ('.git', 'target'):
                continue
            mode = path.lstat().st_mode
            kind = 'symlink' if stat.S_ISLNK(mode) else 'file'
            assert kind == 'symlink' or stat.S_ISREG(mode), str(path)
            data = os.fsencode(os.readlink(path)) if kind == 'symlink' else path.read_bytes()
            files[str(relative)] = {'sha256': hashlib.sha256(data).hexdigest(), 'bytes': len(data), 'mode': stat.S_IMODE(mode), 'kind': kind}
    independent = {
        str(path.relative_to(repo)) for path in repo.rglob('*')
        if path.relative_to(repo).parts[0] not in ('.git', 'target') and (path.is_file() or path.is_symlink())
    }
    assert independent == set(files), 'independent whole-source membership differs'
    return files


def admit(repo, binding, binary):
    """Bind this run to one completed default-feature CLI build and its sources."""
    rows = json.loads((binding / 'receipt.json').read_text())
    assert rows and all(row['exit_code'] == 0 and not row['timed_out'] and not row['changed_inputs'] for row in rows), \
        'binding receipt has a failed or input-changing command'
    built = rows[-1]
    assert built['argv'][0] == 'cargo' and 'build' in built['argv'] and built['argv'][-2:] == ['--bin', 'shenko'], \
        'binding receipt does not end with a CLI build'
    assert '--no-default-features' not in built['argv'], 'a default-feature CLI build is required'
    source_path = binding / f'sources-{built.get("ordinal", len(rows) - 1)}.json'
    assert source_path.is_file(), f'missing source admission record {source_path}'
    source = json.loads(source_path.read_text())
    entries = []
    for directory, dirs, names in os.walk(repo):
        dirs[:] = sorted(name for name in dirs if name not in ('.git', 'target', 'node_modules'))
        for name in sorted(names):
            if name.endswith('.rs') or name in ('Cargo.toml', 'Cargo.lock'):
                path = pathlib.Path(directory) / name
                entries.append({'path': str(path.relative_to(repo)), 'bytes': path.stat().st_size, 'sha256': sha(path)})
    assert entries == source['entries'], 'Rust sources/manifests differ from the actual CLI build'
    try:
        head = subprocess.check_output(['git', '-c', f'safe.directory={repo}', '-C', str(repo), 'rev-parse', 'HEAD'], text=True).strip()
        status = subprocess.check_output(['git', '-c', f'safe.directory={repo}', '-C', str(repo), 'status', '--porcelain'], text=True)
    except (OSError, subprocess.CalledProcessError) as error:
        head, status = None, f'git unavailable: {error}'
    workspace = freeze(repo)
    return {
        'binding_label': binding.name,
        'binding_receipt_sha256': sha(binding / 'receipt.json'),
        'source_scope': source['scope'],
        'source_manifest_sha256': sha(source_path),
        'source_entry_count': len(entries),
        'workspace_snapshot_sha256': hashlib.sha256(json.dumps(workspace, sort_keys=True).encode()).hexdigest(),
        'workspace_excluded_roles': {'.git': 'Git metadata', 'target': 'existing native seed/home runtime fixtures'},
        'git_head': head,
        'git_status_sha256': sha_bytes(status.encode()),
        'git_status_clean': status == '',
        'cli_binary': str(binary),
        'cli_binary_sha256': sha(binary),
    }


def stop(process, sig=signal.SIGTERM):
    if process.poll() is None:
        os.killpg(process.pid, sig)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=15)


def run(out, receipt, label, argv, deadline, cwd, env=None):
    row = {'label': label, 'argv': [str(item) for item in argv], 'deadline_seconds': deadline, 'start_ns': time.time_ns()}
    receipt.setdefault('runs', []).append(row)
    started = time.monotonic()
    with (out / (label + '.stdout')).open('xb') as stdout, (out / (label + '.stderr')).open('xb') as stderr:
        process = subprocess.Popen(argv, cwd=cwd, env=env or ENV, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, close_fds=True, start_new_session=True)
        parts = (pathlib.Path('/proc') / str(process.pid) / 'stat').read_text().rsplit(')', 1)[1].split()
        row['process'] = {'pid': process.pid, 'start_ticks': int(parts[19]), 'pgid': int(parts[2])}
        try:
            row['exit_code'] = process.wait(timeout=deadline)
            row['timed_out'] = False
        except subprocess.TimeoutExpired:
            row['timed_out'] = True
            row['exit_code'] = None
        finally:
            stop(process)
            row['exit_code'] = process.returncode
            row['elapsed_seconds'] = time.monotonic() - started
            write_json(out / 'receipt.json', receipt)
    return row


def build_inspector(out, receipt, repo, target, config):
    """Render the read-only inspector against the runtime source and build it."""
    work = out / 'inspector'
    work.mkdir(mode=0o700)
    (work / 'src').mkdir(mode=0o700)
    sources = {
        'main.rs': (INSPECTOR / 'src/main.rs').read_bytes(),
        'Cargo.toml.in': (INSPECTOR / 'Cargo.toml.in').read_bytes(),
    }
    (work / 'src/main.rs').write_bytes(sources['main.rs'])
    rendered = sources['Cargo.toml.in'].decode().replace('@RUNTIME_REPO@', str(repo))
    assert '@RUNTIME_REPO@' not in rendered, 'runtime repository placeholder was not substituted'
    (work / 'Cargo.toml').write_text(rendered)
    cargo = ['cargo', '--config', str(config)]
    cargo_env = dict(ENV, CARGO_TARGET_DIR=str(target))
    run(out, receipt, 'inspector-metadata', cargo + ['metadata', '--offline', '--format-version', '1'], 180, work, cargo_env)
    metadata = json.loads((out / 'inspector-metadata.stdout').read_text())
    linked = {package['name']: package['manifest_path'] for package in metadata['packages'] if package['name'] in ('shenko-store', 'shenko-types')}
    assert set(linked) == {'shenko-store', 'shenko-types'} and all(pathlib.Path(path).is_relative_to(repo) for path in linked.values()), \
        'the inspector must link the runtime source, not a copy'
    receipt['inspector_manifest_binding'] = linked
    run(out, receipt, 'inspector-build', cargo + ['build', '--offline', '-j1', '--message-format=json'], 1800, work, cargo_env)
    messages = [json.loads(line) for line in (out / 'inspector-build.stdout').read_text().splitlines() if line.startswith('{')]
    artifacts = [
        message for message in messages
        if message.get('reason') == 'compiler-artifact'
        and message.get('target', {}).get('name') == 'shenko-pma-autonomy-inspector'
        and message.get('executable')
    ]
    assert len(artifacts) == 1, 'the inspector build produced no single executable'
    executable = pathlib.Path(artifacts[0]['executable'])
    assert executable.is_relative_to(target), 'the inspector executable came from outside the pinned target dir'
    binary = out / 'inspector-bin'
    shutil.copy2(executable, binary)
    receipt['inspector'] = {
        'binary': str(binary),
        'binary_sha256': sha(binary),
        'sources': {name: sha_bytes(data) for name, data in sources.items()},
        'rendered_manifest_sha256': sha(work / 'Cargo.toml'),
        'generated_lock_sha256': sha(work / 'Cargo.lock') if (work / 'Cargo.lock').exists() else None,
        'compiler_artifact': artifacts[0],
    }
    return binary


def texts(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, list):
        for item in value:
            yield from texts(item)
    elif isinstance(value, dict):
        for key in ('content', 'text'):
            if key in value:
                yield from texts(value[key])


def request_texts(wire):
    """Split one recorded request into its system prompt and its other messages."""
    system, others = [], []
    for message in wire.get('messages') or []:
        parts = list(texts(message.get('content')))
        (system if message.get('role') == 'system' else others).extend(parts)
    return '\n'.join(system), '\n'.join(others)


def completion_text(content_type, payload):
    """The assistant text a provider answer carries, as recorded on the wire."""
    if 'event-stream' in content_type:
        deltas, complete = [], []
        for line in payload.decode('utf-8', 'replace').splitlines():
            if not line.startswith('data:'):
                continue
            data = line[5:].strip()
            if not data or data == '[DONE]':
                continue
            try:
                frame = json.loads(data)
            except ValueError:
                continue
            for choice in frame.get('choices') or []:
                delta = choice.get('delta') or {}
                for target, candidate in ((deltas, delta.get('content')), (complete, (choice.get('message') or {}).get('content'))):
                    if isinstance(candidate, str):
                        target.append(candidate)
                    elif isinstance(candidate, list):
                        target.extend(part.get('text', '') for part in candidate if isinstance(part, dict))
        # Some servers repeat the whole message next to deltas; deltas are the
        # stream, the repeated message is a fallback only.
        return ''.join(deltas) if deltas else ''.join(complete)
    try:
        body = json.loads(payload)
    except ValueError:
        return ''
    pieces = []
    for choice in body.get('choices') or []:
        content = (choice.get('message') or {}).get('content')
        if isinstance(content, str):
            pieces.append(content)
        elif isinstance(content, list):
            pieces.extend(part.get('text', '') for part in content if isinstance(part, dict))
    return ''.join(pieces)


def classify(wire):
    chunks = [part for message in wire.get('messages') or [] for part in texts(message.get('content'))]
    if any(PMA_WORKER_MARK in part for part in chunks):
        return 'pma-worker'
    if any(CHILD_MARK in part for part in chunks):
        return 'model-child'
    return 'subject'


class Recorder:
    """Pass-through provider proxy that only records. It answers nothing."""

    def __init__(self, out, url, limit=512):
        self.out = out
        self.url = url
        self.limit = limit
        self.state = {
            'stage': 'recording',
            'requests': [],
            'errors': [],
            'live_subject_completions': 0,
            'live_pma_completions': 0,
            'live_child_completions': 0,
        }
        self.lock = threading.Lock()
        self.failed = threading.Event()
        self.metadata = None

    def fetch_metadata(self):
        with HTTP.open(self.url + '/v1/models', timeout=15) as response:
            self.metadata = json.load(response)
        return self.metadata

    def save(self):
        write_json(self.out / 'provider-observation.json', self.state)

    def record_forward(self, raw, wire):
        record = None
        try:
            role = classify(wire)
            with self.lock:
                assert len(self.state['requests']) < self.limit, 'bounded observer request count exceeded'
                ordinal = len(self.state['requests']) + 1
                record = {
                    'ordinal': ordinal,
                    'role': role,
                    'origin': 'live-model',
                    'start_ns': time.time_ns(),
                    'wire': wire,
                }
                self.state['requests'].append(record)
            request_path = self.out / f'wire-{ordinal:03d}.request.json'
            request_path.write_bytes(raw)
            record['request_sha256'] = sha(request_path)
            system_prompt, messages = request_texts(wire)
            record['system_prompt_sha256'] = sha_bytes(system_prompt.encode())
            all_request_text = '\n'.join(part for message in wire.get('messages') or [] for part in texts(message.get('content')))
            record['markers_in_system_prompt'] = {
                'pma_worker_descriptor': PMA_MARKER in system_prompt,
                'pma_delegate_selector': PMA_SELECTOR in system_prompt,
                'research': {marker: marker in system_prompt for marker in RESEARCH_MARKERS},
            }
            record['markers_in_request'] = {
                'pma_worker_descriptor': PMA_MARKER in all_request_text,
                'pma_delegate_selector': PMA_SELECTOR in all_request_text,
                'research': [marker for marker in RESEARCH_MARKERS if marker in all_request_text],
            }
            record['selectors_in_messages'] = [selector for selector in (PMA_SELECTOR,) + RESEARCH_SELECTORS if selector in messages]
            request = urllib.request.Request(self.url + '/v1/chat/completions', data=raw, headers={'Content-Type': 'application/json'}, method='POST')
            try:
                response = HTTP.open(request, timeout=300)
            except urllib.error.HTTPError as error:
                response = error
            with response:
                status = response.status
                content_type = response.headers.get('Content-Type', 'application/json')
                payload = response.read()
            response_path = self.out / f'wire-{ordinal:03d}.response.raw'
            response_path.write_bytes(payload)
            answer = completion_text(content_type, payload)
            record.update(
                http_status=status,
                content_type=content_type,
                response_sha256=sha(response_path),
                answer=answer,
                answer_sha256=sha_bytes(answer.encode()),
                answer_bytes=len(answer.encode()),
                answer_has_pma_selector=PMA_SELECTOR in answer,
                answer_has_research_selector=any(selector in answer for selector in RESEARCH_SELECTORS),
                finish_ns=time.time_ns(),
            )
            with self.lock:
                if role == 'subject':
                    self.state['live_subject_completions'] += 1
                elif role == 'pma-worker':
                    self.state['live_pma_completions'] += 1
                elif role == 'model-child':
                    self.state['live_child_completions'] += 1
            return status, content_type, payload
        except BaseException as error:
            with self.lock:
                self.state['errors'].append({'ordinal': record['ordinal'] if record else None, 'type': type(error).__name__, 'message': str(error)})
            self.failed.set()
            raise

    def handler(self):
        recorder = self

        class Proxy(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def log_message(self, *args):
                pass

            def respond(self, status, payload, content_type='application/json'):
                self.send_response(status)
                self.send_header('Content-Type', content_type)
                self.send_header('Content-Length', str(len(payload)))
                self.send_header('Connection', 'close')
                self.end_headers()
                self.wfile.write(payload)
                self.wfile.flush()

            def do_GET(self):
                if self.path == '/v1/models':
                    self.respond(200, json.dumps(recorder.metadata or {}).encode())
                else:
                    self.respond(404, b'{}')

            def do_POST(self):
                try:
                    assert self.path == '/v1/chat/completions', self.path
                    raw = self.rfile.read(int(self.headers['Content-Length']))
                    assert not CREDENTIAL.search(raw), 'credential-looking model input'
                    wire = json.loads(raw)
                    status, content_type, payload = recorder.record_forward(raw, wire)
                    self.respond(status, payload, content_type)
                except BaseException:
                    try:
                        self.respond(500, b'{"error":"bounded observer failed"}')
                    except OSError:
                        pass

        return Proxy


GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'


class WebSocket:
    """Minimal RFC 6455 client: one text frame out, JSON frames in."""

    def __init__(self, host, port, path='/ws', timeout=20):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            f'GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n'
            f'Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'
        )
        self.sock.sendall(request.encode())
        buffer = b''
        while b'\r\n\r\n' not in buffer:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise RuntimeError('websocket handshake closed before headers')
            buffer += chunk
            assert len(buffer) <= 65536, 'websocket handshake too large'
        head, self.buffer = buffer.split(b'\r\n\r\n', 1)
        first = head.split(b'\r\n', 1)[0]
        assert b' 101 ' in first, 'websocket upgrade refused: ' + first.decode('utf-8', 'replace')
        accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest())
        assert accept.lower() in head.lower(), 'websocket accept mismatch'

    def _send_frame(self, opcode, payload):
        header = bytearray([0x80 | opcode])
        length = len(payload)
        if length < 126:
            header.append(0x80 | length)
        elif length < 65536:
            header += bytes([0x80 | 126]) + length.to_bytes(2, 'big')
        else:
            header += bytes([0x80 | 127]) + length.to_bytes(8, 'big')
        mask = os.urandom(4)
        header += mask
        self.sock.sendall(bytes(header) + bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload)))

    def send_text(self, text):
        self._send_frame(0x1, text.encode())

    def _read(self, count, deadline):
        while len(self.buffer) < count:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError('websocket read deadline')
            self.sock.settimeout(remaining)
            chunk = self.sock.recv(max(4096, count - len(self.buffer)))
            if not chunk:
                raise RuntimeError('websocket closed while reading')
            self.buffer += chunk
        value, self.buffer = self.buffer[:count], self.buffer[count:]
        return value

    def recv_json(self, deadline):
        fragments = bytearray()
        while True:
            first, second = self._read(2, deadline)
            final, opcode, length = first & 0x80, first & 0x0F, second & 0x7F
            assert not second & 0x80, 'server frames must not be masked'
            if length == 126:
                length = int.from_bytes(self._read(2, deadline), 'big')
            elif length == 127:
                length = int.from_bytes(self._read(8, deadline), 'big')
            data = self._read(length, deadline) if length else b''
            if opcode == 0x9:
                self._send_frame(0xA, data)
                continue
            if opcode == 0xA:
                continue
            if opcode == 0x8:
                raise RuntimeError('websocket closed by server')
            fragments += data
            if not final:
                continue
            try:
                return json.loads(bytes(fragments))
            except ValueError:
                fragments.clear()

    def close(self):
        try:
            self._send_frame(0x8, b'')
        except OSError:
            pass
        finally:
            self.sock.close()


def api_get(port, path, timeout=10):
    request = urllib.request.Request(f'http://127.0.0.1:{port}{path}', headers={'Accept': 'application/json'})
    with HTTP.open(request, timeout=timeout) as response:
        return json.load(response)


def safe_api_get(port, path, process, timeout=10):
    """One bounded status read; an unreachable server is "not yet", not a fact."""
    if process.poll() is not None:
        raise RuntimeError('CLI exited before serving its status surface')
    try:
        return api_get(port, path, timeout)
    except (urllib.error.URLError, ConnectionError, TimeoutError, json.JSONDecodeError):
        return None


def submit(port, subject_id, text, path):
    """Admit one genuine user message through the product's own input path."""
    websocket = WebSocket('127.0.0.1', port)
    submission_id, message_id = str(uuid.uuid4()), str(uuid.uuid4())
    submission = {
        'schema_version': 2,
        'type': 'submit',
        'submission_id': submission_id,
        'message_id': message_id,
        'subject_id': subject_id,
        'mode': 'append',
        'content': text,
        'attachment_refs': [],
    }
    websocket.send_text(json.dumps(submission, ensure_ascii=False))
    deadline = time.monotonic() + 30
    receipt = None
    try:
        while time.monotonic() < deadline:
            message = websocket.recv_json(deadline)
            if message.get('type') == 'receipt' and message.get('submission_id') == submission_id:
                receipt = message
                break
    finally:
        websocket.close()
    write_json(path, {'submission': submission, 'receipt': receipt})
    assert receipt is not None, 'no durable input receipt arrived'
    assert receipt.get('stage') == 'accepted', receipt
    return receipt


def wait_for(predicate, deadline, interval=0.05):
    while time.monotonic() < deadline:
        value = predicate()
        if value is not None and value is not False:
            return value
        time.sleep(interval)
    return None


def wait_until_subject_completions(state, target, deadline, process, failed):
    def ready():
        if process.poll() is not None:
            raise RuntimeError('CLI exited before the observation bound was reached')
        if failed.is_set():
            raise RuntimeError('provider observer failed; original response files retained')
        if state['live_subject_completions'] >= target:
            return True
        return None
    reached = wait_for(ready, deadline)
    return reached is True


def message_contains(state, needle):
    """Ordinals of live subject requests whose non-system text carries `needle`."""
    found = []
    for record in list(state['requests']):
        if record['role'] != 'subject':
            continue
        wire = record.get('wire') or {}
        _, messages = request_texts(wire)
        if needle and needle in messages:
            found.append(record['ordinal'])
    return found


def observe(out, receipt, args, binding, inspector):
    private = out / 'runtime'
    private.mkdir(mode=0o700)
    (private / 'home').mkdir(mode=0o700)
    receipt['scope'] = (
        'One fresh-root default-CLI observation with two genuine user inputs and real served Coder390 inference. '
        'No scripted model answer, no admitted pma.evaluate, no injected decision, no instructed selector. '
        'Model initiation is separated from any non-model admission trigger, and mechanical facts from semantic quality; '
        'this is not an efficacy, quality, chance-outcome, live-platform or crash claim.'
    )
    receipt['inputs'] = {'first': FIRST_INPUT, 'second': SECOND_INPUT}
    receipt['model'] = {'url': args.model_url, 'id': args.model, 'metadata': None}

    recorder = Recorder(out, args.model_url.rstrip('/'))
    metadata = recorder.fetch_metadata()
    assert any(item['id'] == args.model and 'Merkyor-Coder390-NVFP4-W4A16' in item.get('root', '') for item in metadata['data']), \
        'the served model metadata does not name the expected Coder390 root'
    receipt['model']['metadata'] = metadata

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), recorder.handler())
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    with socket.socket() as reservation:
        reservation.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        reservation.bind(('127.0.0.1', 0))
        port = reservation.getsockname()[1]

    native_env = {
        'PATH': ENV['PATH'],
        'HOME': str(private / 'home'),
        'XDG_CONFIG_HOME': str(private / 'home/config'),
        'TMPDIR': str(private),
        'LANG': 'C.UTF-8',
        'NO_COLOR': '1',
        'SHENKO_USE_LOCAL_MODEL': '1',
        'SHENKO_LOCAL_MODEL_URL': f'http://127.0.0.1:{server.server_port}',
        'SHENKO_LOCAL_MODEL': args.model,
    }
    started = time.monotonic()
    deadline = started + args.deadline
    process = None
    readers = []
    buffers = [bytearray(), bytearray()]
    receipt['observation'] = {'submissions': []}

    def drain(pipe, buffer):
        while chunk := pipe.read(65536):
            buffer.extend(chunk)

    try:
        process = subprocess.Popen(
            [str(binding['cli_binary']), '--data-dir', str(private / 'data'), 'start', '--port', str(port)],
            cwd=private, env=native_env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            close_fds=True, start_new_session=True,
        )
        parts = (pathlib.Path('/proc') / str(process.pid) / 'stat').read_text().rsplit(')', 1)[1].split()
        receipt['process'] = {'pid': process.pid, 'start_ticks': int(parts[19]), 'pgid': int(parts[2]), 'port': port}
        write_json(out / 'receipt.json', receipt)
        readers = [threading.Thread(target=drain, args=pair, daemon=True) for pair in zip((process.stdout, process.stderr), buffers)]
        for reader in readers:
            reader.start()

        self_view = wait_for(
            lambda: safe_api_get(port, '/api/self', process),
            min(deadline, started + 120),
        )
        assert self_view is not None, 'the CLI never served /api/self'
        assert self_view.get('subject_id'), 'no initialized subject identity was served'
        subject_id = self_view['subject_id']
        receipt['observation']['subject_id'] = subject_id
        receipt['observation']['api_self'] = self_view

        first_receipt = submit(port, subject_id, FIRST_INPUT, out / 'submission-1.json')
        receipt['observation']['submissions'].append({'input': 'first', 'receipt': first_receipt})
        write_json(out / 'receipt.json', receipt)
        selected_first = wait_for(lambda: message_contains(recorder.state, FIRST_INPUT) or None, min(deadline, time.monotonic() + args.phase_seconds))
        receipt['observation']['first_input_selected_in_request_ordinals'] = selected_first or []
        baseline = recorder.state['live_subject_completions']
        target = max(baseline + args.min_steps, 1)
        wait_until_subject_completions(recorder.state, target, min(deadline, time.monotonic() + args.phase_seconds), process, recorder.failed)
        receipt['observation']['steps_before_second_input'] = recorder.state['live_subject_completions']
        write_json(out / 'receipt.json', receipt)

        second_receipt = submit(port, subject_id, SECOND_INPUT, out / 'submission-2.json')
        receipt['observation']['submissions'].append({'input': 'second', 'receipt': second_receipt})
        receipt['observation']['second_input_selected_in_request_ordinals'] = wait_for(
            lambda: message_contains(recorder.state, SECOND_INPUT) or None,
            min(deadline, time.monotonic() + args.phase_seconds),
        ) or []
        write_json(out / 'receipt.json', receipt)
        final_target = max(args.max_subject_steps, recorder.state['live_subject_completions'] + 2)
        wait_until_subject_completions(recorder.state, final_target, deadline, process, recorder.failed)
        receipt['observation']['api_self_final'] = safe_api_get(port, '/api/self', process)
        receipt['observation']['completed'] = True
    except BaseException as error:
        receipt['observation']['error'] = {'type': type(error).__name__, 'message': str(error)}
    finally:
        if process is not None:
            stop(process, signal.SIGINT)
        for reader in readers:
            reader.join(timeout=5)
        for suffix, buffer in zip(('stdout', 'stderr'), buffers):
            (out / ('cli.' + suffix)).write_bytes(CREDENTIAL.sub(b'<redacted-owner-credential>', bytes(buffer)))
        receipt['cli'] = {
            'exit_code': process.returncode if process else None,
            'streams_complete': all(not reader.is_alive() for reader in readers),
            'credential_free': not any(CREDENTIAL.search(buffer) for buffer in buffers),
            'elapsed_seconds': time.monotonic() - started,
        }
        absent = True
        if process is not None:
            try:
                os.killpg(process.pid, 0)
                absent = False
            except ProcessLookupError:
                pass
        receipt['cli']['process_group_absent'] = absent
        recorder.state['stage'] = 'stopped'
        recorder.save()
        receipt['provider_observation_sha256'] = sha(out / 'provider-observation.json')
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
        receipt['observation'].setdefault('completed', False)
        write_json(out / 'receipt.json', receipt)

    readback = run(out, receipt, 'native-readback', [str(inspector), str(private / 'data'), str(out / 'native-observation.json')], 180, out)
    receipt['native_readback_exit_code'] = readback['exit_code']
    assert (out / 'native-observation.json').is_file(), \
        f"the read-only inspector exited {readback['exit_code']} without a document"
    native = json.loads((out / 'native-observation.json').read_text())
    receipt['native_observation_sha256'] = sha(out / 'native-observation.json')
    return recorder.state, native


def spans_text(spans, raw):
    pieces = []
    for span in spans or []:
        start, end = span.get('start'), span.get('end')
        if isinstance(start, int) and isinstance(end, int) and 0 <= start <= end <= len(raw):
            pieces.append(raw[start:end])
    return '\n'.join(pieces)


def s0_parts(step):
    body = ((step.get('s0') or {}).get('body')) or {}
    payload = body.get('payload') or {}
    system_prompt = payload.get('system_prompt') or ''
    messages = []
    for message in payload.get('messages') or []:
        for part in message.get('parts') or []:
            if part.get('type') == 'text':
                messages.append(part.get('text', ''))
    return body, system_prompt, '\n'.join(messages)


def facts(native, state, receipt):
    steps = native.get('steps') or []
    tasks = native.get('tasks') or []
    by_kind = {}
    for task in tasks:
        by_kind.setdefault(task['record']['kind'], []).append(task)
    live_subject = [record for record in state['requests'] if record['role'] == 'subject']
    answers = {record['ordinal']: record.get('answer') or '' for record in live_subject}

    initiations = []
    for task in by_kind.get('pma.evaluate', []):
        record = task['record']
        control = task['input'].get('control') or {}
        definition = control.get('definition') or {}
        memos = record.get('memos') or {}
        # The non-model trigger writes these exact additive memos. Nothing here
        # is inferred from a memo key substring or from model text.
        trigger = memos.get('trigger') if isinstance(memos.get('trigger'), dict) else None
        intent_memo = memos.get('intent_id')
        has_control = control.get('kind') == 'delegate'
        creator = record.get('created_by_step_id')
        step = next((item for item in steps if item.get('step_id') == creator), None)
        model_text = ((step or {}).get('model_result') or {}).get('text') or ''
        provider = (step or {}).get('model_outcome') or {}
        body, system_prompt, messages = s0_parts(step or {})
        span_text = spans_text(control.get('spans'), model_text)
        step_checks = (step or {}).get('checks') or {}
        operations = [
            {
                'operation_name': row['operation']['operation_name'],
                'state': row['operation']['state'],
                'outcome': row['operation'].get('outcome'),
                'attempts': len(row['attempts']),
                'evidence_receipt_ids': row['evidence_receipt_ids'],
            }
            for row in task.get('operations') or []
        ]
        # Model initiation requires ALL of: the subject's own delegate control
        # for this exact task id, its frozen native definition, the directive in
        # that step's sealed model answer, no directive in the sealed S0
        # messages, both sealed identity checks, and a complete provider outcome.
        model_delegated = bool(
            has_control
            and definition.get('kind') == 'pma.evaluate'
            and definition.get('definition_version') == record['definition_version']
            and PMA_SELECTOR in model_text
            and PMA_SELECTOR not in messages
            and step is not None
            and step.get('step_id') == creator
            and step_checks.get('s0_identity') is True
            and step_checks.get('decision_identity') is True
            and provider.get('complete') is True
        )
        # A policy-triggered admission has no subject control by invariant; it
        # carries `trigger.source == "policy"` instead.
        policy_triggered = bool(not has_control and trigger is not None and trigger.get('source') == 'policy')
        if model_delegated:
            classification = 'model-delegation'
        elif policy_triggered:
            classification = 'policy-triggered'
        elif has_control:
            classification = 'control-without-verified-provenance'
        else:
            classification = 'unattributed-admission'
        initiations.append({
            'task_id': record['task_id'],
            'kind': record['kind'],
            'definition_version': record['definition_version'],
            'task_state': record['state'],
            'intent_id_memo': intent_memo,
            'trigger_memo': trigger,
            'created_by_step_id': creator,
            'creator_step_no': (step or {}).get('step_no'),
            'creator_step_state': (step or {}).get('state'),
            'has_delegation_control': has_control,
            'control_kind': control.get('kind'),
            'control_input': definition.get('input'),
            'control_spans': control.get('spans'),
            'control_source_span_text': span_text,
            'control_span_contains_selector': PMA_SELECTOR in span_text,
            # These four are delegation checks only: null, never false, when no
            # subject control exists, so a policy admission cannot read as a
            # failed model delegation.
            'model_delegation_checks': {
                's0_identity_verified': step_checks.get('s0_identity') if has_control else None,
                'decision_identity_verified': step_checks.get('decision_identity') if has_control else None,
                'selector_in_model_answer': (PMA_SELECTOR in model_text) if has_control else None,
                'selector_in_s0_messages': (PMA_SELECTOR in messages) if has_control else None,
                'provider_outcome_complete': provider.get('complete') if has_control else None,
            },
            'creator_step_checks': step_checks,
            'selector_in_system_prompt': PMA_SELECTOR in system_prompt,
            'request_id': (body or {}).get('request_id'),
            'selector_in_recorded_subject_answers': [
                ordinal for ordinal, answer in answers.items() if PMA_SELECTOR in answer
            ],
            'provider_stop_reason': provider.get('stop_reason'),
            'provider_raw_response_field_sha256': sha_bytes((provider.get('raw_response') or '').encode()),
            'operations': operations,
            'admitted_from_model_output': model_delegated,
            'trigger_classification': classification,
        })

    decisions = []
    for lineage in native.get('research') or []:
        for epoch in lineage.get('epochs') or []:
            if epoch.get('latest_intervention') is not None:
                decisions.append({'lineage_id': lineage['lineage_id'], 'epoch_id': epoch['epoch']['epoch_id'], 'decision': epoch['latest_intervention'], 'delivery_scope': epoch.get('delivery_scope')})

    uses = []
    for use in native.get('pma_reminder_joins') or []:
        body = ((use.get('contribution') or {}).get('body')) or {}
        text = body.get('text') or ''
        uses.append({
            'step_no': use.get('step_no'),
            'step_id': use.get('step_id'),
            'key': use.get('key'),
            'contribution_body': body,
            'durable_reminder': use.get('durable_reminder'),
            'decision_join': use.get('decision_join'),
            'live_request_ordinals_carrying_body': message_contains(state, text) if text else [],
            'durable_used_step_id': ((use.get('durable_reminder') or {}).get('used_step_id')),
            'durable_used_at_ms': ((use.get('durable_reminder') or {}).get('used_at_ms')),
            'used_by_this_step': ((use.get('durable_reminder') or {}).get('used_step_id') == use.get('step_id')),
        })

    descriptors = {
        'pma_descriptor_in_any_subject_system_prompt': any(
            ((record.get('markers_in_system_prompt') or {}).get('pma_worker_descriptor')
             or (record.get('markers_in_system_prompt') or {}).get('pma_delegate_selector'))
            for record in live_subject
        ),
        'pma_descriptor_in_any_subject_request': any(
            ((record.get('markers_in_request') or {}).get('pma_worker_descriptor')
             or (record.get('markers_in_request') or {}).get('pma_delegate_selector'))
            for record in live_subject
        ),
        'research_descriptors_in_any_subject_system_prompt': any(
            any((record.get('markers_in_system_prompt') or {}).get('research', {}).values()) for record in live_subject
        ),
        'research_descriptors_in_any_subject_request': any(
            bool((record.get('markers_in_request') or {}).get('research')) for record in live_subject
        ),
        'selector_ever_in_subject_request_messages': [
            {'ordinal': record['ordinal'], 'selectors': record['selectors_in_messages']}
            for record in live_subject if record.get('selectors_in_messages')
        ],
        'subject_contract_present_in_any_subject_request': any(
            SUBJECT_CONTRACT_MARK in '\n'.join(texts((record.get('wire') or {}).get('messages', [])))
            for record in live_subject
        ),
        'subject_steps_observed': len(live_subject),
        'live_pma_worker_completions': state['live_pma_completions'],
        'live_model_child_completions': state['live_child_completions'],
    }
    descriptor_sentence = None
    for record in live_subject:
        system_prompt, messages = request_texts(record.get('wire') or {})
        for host_text, needle in ((system_prompt, PMA_MARKER), (system_prompt, PMA_SELECTOR), (messages, PMA_MARKER)):
            index = host_text.find(needle)
            if index >= 0:
                descriptor_sentence = host_text[max(0, index - 120):index + 260]
                break
        if descriptor_sentence:
            break
    descriptors['pma_descriptor_sentence'] = descriptor_sentence
    descriptors['pma_descriptor_requires_lineage_and_epoch'] = bool(
        descriptor_sentence
        and 'lineage_id' in descriptor_sentence
        and 'epoch_id' in descriptor_sentence
    )

    research = []
    for lineage in native.get('research') or []:
        research.append({
            'lineage_id': lineage['lineage_id'],
            'epochs': [
                {
                    'epoch_id': epoch['epoch']['epoch_id'],
                    'status': epoch['epoch']['status'],
                    'bank_entries': len(epoch.get('bank') or []),
                    'latest_intervention': epoch.get('latest_intervention'),
                }
                for epoch in lineage.get('epochs') or []
            ],
        })

    admitted = [item for item in initiations if item['admitted_from_model_output']]
    policy = [item for item in initiations if item['trigger_classification'] == 'policy-triggered']
    unclassified = [
        item for item in initiations
        if item['trigger_classification'] in ('control-without-verified-provenance', 'unattributed-admission')
    ]
    if admitted:
        outcome = 'initiated'
        reason = 'a pma.evaluate task is bound to its creating step control whose sealed model answer contains the delegate directive'
    elif unclassified:
        outcome = 'inconclusive'
        reason = 'a pma.evaluate task exists whose admission is neither a verified model delegation nor a marked policy trigger'
    elif policy:
        outcome = 'admitted_without_model_delegation'
        reason = 'every pma.evaluate task carries a policy trigger memo and no subject delegation control, so no model initiation was observed'
    elif descriptors['subject_steps_observed'] == 0:
        outcome = 'inconclusive'
        reason = 'no live subject completion was observed inside the bound'
    elif not descriptors['pma_descriptor_in_any_subject_request']:
        outcome = 'inconclusive'
        reason = 'the runtime never offered the pma.evaluate descriptor to a subject request, so this model could not have chosen the worker from that request'
    elif not receipt['observation'].get('completed'):
        outcome = 'inconclusive'
        reason = 'the observation did not reach its bound: ' + json.dumps(receipt['observation'].get('error'))
    else:
        outcome = 'not_initiated'
        reason = 'the observation completed within its bound and no pma.evaluate task was admitted by the runtime'

    ambient = [
        {
            'step_no': use.get('step_no'),
            'step_id': use.get('step_id'),
            'key': use.get('key'),
            'contribution_body': ((use.get('contribution') or {}).get('body')) or {},
        }
        for use in native.get('pma_ambient_contributions') or []
    ]
    mechanical = {
        'outcome': outcome,
        'outcome_reason': reason,
        'model_initiated': bool(admitted),
        'admitted_without_model_delegation': bool(policy),
        'initiation_sources': {
            'model_delegation': len(admitted),
            'policy_triggered': len(policy),
            'control_without_verified_provenance': len([item for item in unclassified if item['trigger_classification'] == 'control-without-verified-provenance']),
            'unattributed_admission': len([item for item in unclassified if item['trigger_classification'] == 'unattributed-admission']),
        },
        'ambient_pma_contributions': ambient,
        'descriptors': descriptors,
        'initiations': initiations,
        'decisions': decisions,
        'delivery': {
            'decisions': len(decisions),
            'remind_decisions': [item for item in decisions if (item['decision'] or {}).get('decision') == 'remind'],
            'silent_decisions': [item for item in decisions if (item['decision'] or {}).get('decision') == 'silent'],
            'contributions_sealed_into_s0': len(uses),
            'durably_used_by_the_same_step': [item for item in uses if item['used_by_this_step']],
            'live_requests_carrying_a_reminder_body': sorted({
                ordinal for item in uses for ordinal in item['live_request_ordinals_carrying_body']
            }),
        },
        'delivery_uses': uses,
        'research': research,
        'task_kinds_observed': {kind: len(rows) for kind, rows in sorted(by_kind.items())},
        'research_epochs_observed': sum(len(lineage['epochs']) for lineage in research),
        'active_research_epoch_observed': any(
            epoch['status'] == 'active' for lineage in research for epoch in lineage['epochs']
        ),
        'research_bank_entries_observed': sum(
            epoch['bank_entries'] for lineage in research for epoch in lineage['epochs']
        ),
        'delivery_journal': native.get('pma_delivery_journal') or [],
    }
    return mechanical


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--out', required=True, type=pathlib.Path, help='fresh output directory for this observation')
    parser.add_argument('--binding', required=True, type=pathlib.Path, help='run_native label dir whose final build row is a default-feature CLI build')
    parser.add_argument('--target', required=True, type=pathlib.Path, help='cargo target dir of that binding')
    parser.add_argument('--repo', type=pathlib.Path, default=DEFAULT_REPO, help='runtime source worktree the binary was built from')
    parser.add_argument('--config', type=pathlib.Path, default=DEFAULT_CONFIG, help='cargo config with the offline vendored sources')
    parser.add_argument('--model-url', default=os.environ.get('SHENKO_LOCAL_MODEL_URL', 'http://10.10.10.11:8000'))
    parser.add_argument('--model', default=os.environ.get('SHENKO_LOCAL_MODEL', 'swift-qwen3.8-27b-nvfp4'))
    parser.add_argument('--deadline', type=int, default=900)
    parser.add_argument('--phase-seconds', type=int, default=240)
    parser.add_argument('--max-subject-steps', type=int, default=10)
    parser.add_argument('--min-steps', type=int, default=3)
    args = parser.parse_args()

    out = args.out.resolve()
    out.mkdir(mode=0o700)
    repo = args.repo.resolve()
    binding_path = args.binding.resolve()
    binary = args.target.resolve() / 'debug/shenko'
    assert binary.is_file(), f'missing CLI binary {binary}'
    assert args.config.is_file(), f'missing cargo config {args.config}'

    receipt = {
        'program': str(pathlib.Path(__file__).resolve()),
        'program_sha256': sha(pathlib.Path(__file__).resolve()),
        'inspector_source_root': str(INSPECTOR),
        'started_ns': time.time_ns(),
        'fixture_texts_selector_free': not any(
            token in text for text in (FIRST_INPUT, SECOND_INPUT) for token in ('[delegate:', 'pma.', 'research.')
        ),
    }
    write_json(out / 'receipt.json', receipt)
    binding = admit(repo, binding_path, binary)
    receipt['binding'] = binding
    write_json(out / 'receipt.json', receipt)
    (out / 'workspace-source.json').write_text(json.dumps(freeze(repo), indent=2) + '\n')
    shutil.copy2(binary, out / 'shenko')
    assert sha(out / 'shenko') == binding['cli_binary_sha256']
    write_json(out / 'receipt.json', receipt)

    try:
        inspector = build_inspector(out, receipt, repo, args.target.resolve(), args.config.resolve())
        write_json(out / 'receipt.json', receipt)
        state, native = observe(out, receipt, args, binding, inspector)
        receipt['mechanical_facts'] = facts(native, state, receipt)
        checks = {
            'binding_unchanged': admit(repo, binding_path, binary) == binding,
            'inspector_binary_hash_matches': sha(out / 'inspector-bin') == receipt['inspector']['binary_sha256'],
            'native_readback_complete': (native.get('errors') or []) == [],
            'all_live_requests_succeeded': all(
                record.get('http_status') == 200 for record in state['requests'] if record['origin'] == 'live-model'
            ),
            'no_observer_error': not state['errors'],
            'clean_cli_exit': receipt['cli']['exit_code'] == 0 and receipt['cli']['streams_complete'] and receipt['cli']['process_group_absent'],
            'credential_free': receipt['cli']['credential_free'],
            'fixture_texts_selector_free': receipt['fixture_texts_selector_free'],
            'provider_archive_complete': all(
                record.get('request_sha256') and record.get('response_sha256') for record in state['requests']
            ),
        }
        receipt['checks'] = checks
        receipt['passed'] = all(checks.values()) and receipt['mechanical_facts']['outcome'] != 'inconclusive'
        receipt['not_a_claim'] = (
            'Not an efficacy, quality, chance-outcome, comparative-ablation, live-platform, physical-crash or full-runtime acceptance claim. '
            'A not_initiated outcome reports that this bounded observation did not see the model initiate the worker; it does not prove the model never would. '
            'An admitted_without_model_delegation outcome reports a worker admission this observation could not attribute to the model. '
            'A silent decision is a recorded miss, not a wrong answer; a remind decision is a recorded reminder, not a correct one.'
        )
        receipt['passed_means'] = (
            'Strongest integrity facts only: binding/source/binary unchanged, all live provider requests succeeded, every recorded raw '
            'request/response is archived, the read-only readback verified every sealed object, the CLI stopped cleanly, no credential '
            'appeared, and the outcome is not inconclusive. It says nothing about whether the model chose well.'
        )
    except BaseException as error:
        receipt['error'] = {'type': type(error).__name__, 'message': str(error)}
        receipt['passed'] = False
        raise
    finally:
        receipt['finished_ns'] = time.time_ns()
        write_json(out / 'receipt.json', receipt)
        print(json.dumps({
            'receipt': str(out / 'receipt.json'),
            'passed': receipt.get('passed'),
            'outcome': (receipt.get('mechanical_facts') or {}).get('outcome'),
            'checks': receipt.get('checks'),
            'error': receipt.get('error'),
            'observation_error': (receipt.get('observation') or {}).get('error'),
        }, ensure_ascii=False), flush=True)


if __name__ == '__main__':
    main()
