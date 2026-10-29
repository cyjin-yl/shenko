// Deterministic controller/transport fixtures, not native browser or provider evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { setImmediate } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: "export { createLiveController } from './src/live-controller.ts'; export { intentFor } from './src/projection.ts';",
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'controller-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'browser',
});
const { createLiveController, intentFor } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`
);

function environment(t) {
  t.mock.timers.enable({ apis: ['setTimeout', 'setInterval'] });
  const storage = new Map();
  const sockets = [];
  class TestSocket {
    static OPEN = 1;
    readyState = 0;
    sent = [];
    persistedAtSend = [];
    constructor() { sockets.push(this); }
    open() { this.readyState = 1; this.onopen?.({}); }
    receive(data) { this.onmessage?.({ data: JSON.stringify(data) }); }
    send(text) {
      const message = JSON.parse(text);
      this.sent.push(message);
      if (message.type === 'submit') {
        this.persistedAtSend.push(JSON.parse(
          storage.get(`shenko.submission.v2.${message.submission_id}`) ?? 'null'));
      }
    }
    close() { this.readyState = 3; this.onclose?.({}); }
  }
  const values = {
    window: globalThis,
    document: { documentElement: { dataset: {}, classList: { toggle() {} } } },
    location: { host: 'controller-fixture.invalid', protocol: 'http:', search: '' },
    localStorage: {
      getItem: (key) => storage.get(key) ?? null,
      setItem: (key, value) => storage.set(key, value),
      removeItem: (key) => storage.delete(key),
      key: (index) => [...storage.keys()][index] ?? null,
      get length() { return storage.size; },
    },
    WebSocket: TestSocket,
    fetch: async (path) => ({
      ok: true,
      json: async () => path === '/api/self' ? { subject_id: 'fixture-subject' } : {},
    }),
  };
  const previous = new Map();
  for (const [key, value] of Object.entries(values)) {
    previous.set(key, Object.getOwnPropertyDescriptor(globalThis, key));
    Object.defineProperty(globalThis, key, { value, configurable: true, writable: true });
  }
  t.after(() => {
    for (const [key, descriptor] of previous) {
      if (descriptor) Object.defineProperty(globalThis, key, descriptor);
      else delete globalThis[key];
    }
  });
  return { storage, sockets };
}

function open(socket) {
  socket.open();
  socket.receive({
    schema_version: 2, type: 'hello',
    capabilities: { append: true, durable_input_receipts: true, committed_replay: true },
  });
}

function intent(controller, node, action, payload = {}) {
  controller.dispatch(intentFor(controller.projection(), node, action, payload));
}

function submission(controller) {
  return controller.projection().nodes.find((node) => node.node_id === 'timeline')
    .children.find((node) => node.kind === 'user_submission').props.submission;
}

function view(socket, input, stage, extra = {}) {
  socket.receive({
    schema_version: 2, type: 'submission_view', result: 'found',
    submission_id: input.submission_id, event_id: 'durable-event',
    accepted_commit: 'ledger/1', stage, ...extra,
  });
}

test('live receipts query Store facts while preserving draft and reading state', async (t) => {
  const { storage, sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  intent(controller, 'composer', 'composer.draft', { value: '追加到持续主体' });
  intent(controller, 'composer', 'composer.append');
  const input = socket.sent.find((message) => message.type === 'submit');
  socket.receive({
    schema_version: 2, type: 'receipt', stage: 'accepted',
    submission_id: input.submission_id, event_id: 'durable-event', accepted_commit: 'ledger/1',
  });
  intent(controller, 'composer', 'composer.draft', { value: '不要覆盖的新草稿' });
  intent(controller, 'timeline', 'timeline.reading', { state: 'reading_history' });
  t.mock.timers.tick(2000);
  assert.equal(socket.sent.filter((message) => message.type === 'query_submission').at(-1).submission_id,
    input.submission_id);
  view(socket, input, 'selected', { selected_step_id: 'sealed-step' });
  assert.equal(submission(controller).receipt, 'selected');
  const queries = socket.sent.filter((message) => message.type === 'query_submission').length;
  t.mock.timers.tick(2000);
  assert.ok(socket.sent.filter((message) => message.type === 'query_submission').length > queries);
  view(socket, input, 'committed', { committed_at: 'ledger/9' });
  assert.equal(submission(controller).receipt, 'committed');
  assert.equal(storage.get(`shenko.submission.v2.${input.submission_id}`), undefined);
  const projection = controller.projection();
  assert.equal(projection.nodes.find((node) => node.node_id === 'composer').props.draft,
    '不要覆盖的新草稿');
  assert.equal(projection.nodes.find((node) => node.node_id === 'timeline').props.reading,
    'reading_history');
});

test('page reopen reconciles a lost ACK using the persisted original payload and ID', async (t) => {
  const { storage, sockets } = environment(t);
  const first = createLiveController();
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  intent(first, 'composer', 'composer.draft', { value: 'ACK 丢失也不能重复' });
  intent(first, 'composer', 'composer.append');
  const original = socket.sent.find((message) => message.type === 'submit');
  assert.deepEqual(socket.persistedAtSend[0].payload, original);
  intent(first, 'composer', 'composer.draft', { value: '重开后保留的草稿' });
  // No receipt is delivered. Dispose models page closure without any ACK.
  first.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const newSocket = sockets[1];
  open(newSocket);
  assert.equal(submission(reopened).receipt, 'unknown');
  assert.equal(newSocket.sent.find((message) => message.type === 'query_submission').submission_id,
    original.submission_id);
  assert.equal(newSocket.sent.some((message) => message.type === 'submit'), false);
  // A stale not_admitted belonging to an older send attempt must not restore
  // the draft or demote the retry; retrying keeps the same ID.
  intent(reopened, `submission.${original.submission_id}`, 'submission.retry',
    { submission_id: original.submission_id });
  const retry = newSocket.sent.filter((message) => message.type === 'submit');
  assert.equal(retry.length, 1);
  assert.equal(retry[0].submission_id, original.submission_id);
  newSocket.receive({
    schema_version: 2, type: 'submission_view', result: 'not_admitted',
    submission_id: original.submission_id, send_epoch: 0,
  });
  // The stale absence must not demote the retry or restore the draft; the
  // same ID and payload remain the only outgoing identity.
  assert.notEqual(submission(reopened).receipt, 'rejected');
  assert.equal(submission(reopened).payload.submission_id, original.submission_id);
  assert.equal(storage.get(`shenko.submission.v2.${original.submission_id}`) !== undefined, true);
  assert.equal(reopened.projection().nodes.find((node) => node.node_id === 'composer').props.draft,
    '重开后保留的草稿');
  view(newSocket, original, 'accepted');
  assert.deepEqual(submission(reopened).payload, original);
  assert.equal(submission(reopened).receipt, 'accepted');
  assert.equal(reopened.projection().nodes.find((node) => node.node_id === 'composer').props.draft,
    '重开后保留的草稿');
});
