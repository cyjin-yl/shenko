// Deterministic controller/transport fixtures, not native browser or provider evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { setImmediate } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: "export { createLiveController } from './src/live-controller.ts'; export { intentFor, partState } from './src/projection.ts';",
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'controller-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'browser',
});
const { createLiveController, intentFor, partState } = await import(
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
  // The action the reader clicks is the submission's own action ref (the
  // projection keeps one action per submission, so its id carries the
  // submission id); the intent it produces is still `submission.retry`.
  intent(reopened, `submission.${original.submission_id}`,
    `submission.retry.${original.submission_id}`,
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

function timelineNode(controller) {
  return controller.projection().nodes.find((node) => node.node_id === 'timeline');
}

function documentFrom(stepId, documentId = 'doc-1', revision = 'doc-rev-1') {
  return {
    schema_version: 1,
    type: 'document',
    source_step_id: stepId,
    source_commit: `ledger/${stepId}`,
    degraded: {
      mode: 'card',
      document_id: documentId,
      revision,
      summary: '确认继续部署？',
      options: [{
        index: 1,
        action_id: 'confirm',
        nonce: 'nonce-1',
        revision,
        label: '确认',
        required_capabilities: [],
        payload: { decision: 'confirm' },
      }],
    },
  };
}

function committedPart(stepId, step, content) {
  return {
    schema_version: 2,
    type: 'timeline_part',
    role: 'subject',
    content,
    message_id: stepId,
    part_id: 'text/0',
    part_revision: `rev-${step}`,
    chunk_seq: 0,
    committed_only: true,
    observed_at: '2026-11-05T02:00:00Z',
    step,
    step_id: stepId,
    commit_ref: `ledger/${step}`,
    terminal: true,
  };
}

test('a degraded document renders beside the committed step that carried it', async (t) => {
  const { sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  socket.receive(committedPart('step-3', 3, '第一轮已提交正文'));
  socket.receive(documentFrom('step-3'));
  assert.deepEqual(
    timelineNode(controller).children.map((child) => child.node_id),
    ['part.text/0', 'document.doc-1'],
  );
  const documentNode = timelineNode(controller).children[1];
  assert.equal(documentNode.props.document.source_step_id, 'step-3');
  assert.equal(documentNode.props.document.source_commit, 'ledger/step-3');

  // A surface whose step is not rendered is kept readable at the end rather
  // than dropped or attached to an unrelated entry.
  socket.receive(documentFrom('step-9', 'doc-2', 'doc-rev-2'));
  const children = timelineNode(controller).children.map((child) => child.node_id);
  assert.deepEqual(children, ['part.text/0', 'document.doc-1', 'document.doc-2']);
});

test('an admitted action is persisted before send and never admitted twice', async (t) => {
  const { storage, sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  socket.open();
  socket.receive({
    schema_version: 2, type: 'hello',
    capabilities: {
      append: true, actions: true, documents: true,
      durable_input_receipts: true, committed_replay: true,
    },
  });
  socket.receive(documentFrom('step-3'));
  const documentNode = timelineNode(controller).children.find((child) => child.node_id === 'document.doc-1');
  const actionRef = documentNode.action_refs[0];
  controller.dispatch(intentFor(controller.projection(), documentNode.node_id, actionRef));
  const sent = socket.sent.filter((message) => message.type === 'action');
  assert.equal(sent.length, 1);
  assert.equal(sent[0].document_id, 'doc-1');
  assert.equal(sent[0].action_id, 'confirm');
  assert.deepEqual(sent[0].payload, { decision: 'confirm' });
  // The durable decision key is stored before the transport result is known.
  const persisted = JSON.parse(storage.get(`shenko.action.v2.${sent[0].submission_id}`));
  assert.equal(persisted.subject_id, 'fixture-subject');
  assert.equal(persisted.revision, 'doc-rev-1');

  const receipts = () => timelineNode(controller).children
    .find((child) => child.node_id === 'document.doc-1').props.receipts;
  assert.equal(receipts()[0].stage, 'sending');
  socket.receive({
    schema_version: 2, type: 'receipt', stage: 'accepted',
    submission_id: sent[0].submission_id, event_id: 'action-event',
    accepted_commit: 'ledger/70',
  });
  assert.equal(receipts()[0].stage, 'accepted');
  assert.equal(receipts()[0].event_id, 'action-event');
  assert.equal(receipts()[0].accepted_commit, 'ledger/70');

  // A second click on the same admitted option must not admit another
  // decision: the durable receipt is shown instead.
  controller.dispatch(intentFor(controller.projection(), documentNode.node_id, actionRef));
  assert.equal(socket.sent.filter((message) => message.type === 'action').length, 1);

  // The same query message reconciles the action after a reconnect.
  t.mock.timers.tick(2000);
  const query = socket.sent.filter((message) => message.type === 'query_submission').at(-1);
  assert.equal(query.submission_id, sent[0].submission_id);

  view(socket, { submission_id: sent[0].submission_id }, 'committed', { committed_at: 'ledger/71' });
  assert.equal(receipts()[0].stage, 'committed');
  assert.equal(storage.get(`shenko.action.v2.${sent[0].submission_id}`), undefined);
});

test('a rejected action can be decided again with a new durable key', async (t) => {
  const { storage, sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  socket.open();
  socket.receive({
    schema_version: 2, type: 'hello',
    capabilities: { append: true, actions: true, documents: true, durable_input_receipts: true },
  });
  socket.receive(documentFrom('step-3'));
  const documentNode = timelineNode(controller).children.find((child) => child.node_id === 'document.doc-1');
  const actionRef = documentNode.action_refs[0];
  controller.dispatch(intentFor(controller.projection(), documentNode.node_id, actionRef));
  const first = socket.sent.find((message) => message.type === 'action');
  socket.receive({
    schema_version: 2, type: 'receipt', stage: 'rejected',
    submission_id: first.submission_id, detail: '动作 revision 已过期；当前为 doc-rev-2。',
  });
  const receipts = () => timelineNode(controller).children
    .find((child) => child.node_id === 'document.doc-1').props.receipts;
  assert.equal(receipts()[0].stage, 'rejected');
  assert.match(receipts()[0].detail, /revision 已过期/);
  // Nothing durable exists for the rejected key, so it is not kept.
  assert.equal(storage.get(`shenko.action.v2.${first.submission_id}`), undefined);
  // The option stays user-actionable; pressing it again is a new decision
  // with its own identity, not a replay of the rejected one.
  controller.dispatch(intentFor(controller.projection(), documentNode.node_id, actionRef));
  const actions = socket.sent.filter((message) => message.type === 'action');
  assert.equal(actions.length, 2);
  assert.notEqual(actions[1].submission_id, first.submission_id);
});

test('reading place, code wrap and selection survive a page reopen', async (t) => {
  const { storage, sockets } = environment(t);
  const first = createLiveController();
  await setImmediate();
  open(sockets[0]);
  const anchor = {
    entry_key: 'part.text/0',
    entry_index: 2,
    prev_entry_key: 'submission.a',
    offset: -30,
    scroll_top: 900,
  };
  intent(first, 'timeline', 'timeline.reading', {
    state: 'reading_history', at_end: false, anchor,
  });
  assert.deepEqual(timelineNode(first).props.anchor, anchor);
  intent(first, 'timeline', 'timeline.code_wrap');
  assert.equal(timelineNode(first).props.wrap, true);
  intent(first, 'timeline', 'timeline.selection', {
    entry_key: 'part.text/0', text: '第一段正文继续说明', start: 2, end: 6,
  });
  assert.equal(timelineNode(first).props.selection.start, 2);
  assert.equal(timelineNode(first).props.selection.entry_key, 'part.text/0');
  // Nothing about a reading place is written as a business fact: the only
  // records are browser display state.
  assert.equal([...storage.keys()].some((key) => key.startsWith('shenko.submission.')), false);
  first.dispose();

  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  open(sockets[1]);
  const restored = timelineNode(reopened);
  assert.equal(restored.props.reading, 'reading_history');
  assert.deepEqual(restored.props.anchor, anchor);
  assert.equal(restored.props.wrap, true);
  assert.deepEqual(restored.props.selection, {
    entry_key: 'part.text/0', start: 2, end: 6,
    prefix_hash: timelineNode(reopened).props.selection.prefix_hash,
  });
  // Jumping to the newest clears the place but keeps the reading preference.
  intent(reopened, 'timeline', 'timeline.latest');
  assert.equal(timelineNode(reopened).props.reading, 'following_latest');
  assert.equal(timelineNode(reopened).props.anchor, undefined);
  assert.equal(timelineNode(reopened).props.wrap, true);
  const persisted = [...storage.values()].filter((value) => value.includes('"reading"'));
  assert.ok(persisted.every((value) => !value.includes('"anchor"')), persisted.join('|'));
  assert.ok(persisted.some((value) => value.includes('"wrap":true')), persisted.join('|'));
});

test('a streamed prefix is not labelled committed and is replaced by the committed text', async (t) => {
  const { sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  socket.open();
  socket.receive({
    schema_version: 2, type: 'hello',
    capabilities: { append: true, committed_replay: true, durable_input_receipts: true, streaming: true },
  });
  socket.receive({
    schema_version: 2, type: 'stream_part', role: 'subject',
    content: '正在生成', message_id: 'step-5', part_id: 'text/0',
    part_revision: 'rev-s0', chunk_seq: 0, terminal: false, committed_only: true,
    observed_at: '2026-11-05T02:00:00Z', step: 4,
    stream_id: 'stream-1', request_id: 'req-1', attempt_id: 'att-1',
    commit_ref: 'ledger/50',
  });
  const entry = () => timelineNode(controller).children
    .find((child) => child.node_id === 'part.text/0');
  assert.equal(entry().props.part.committed, false);
  assert.equal(entry().props.part.terminal, false);
  assert.deepEqual(partState(entry().props.part), {
    committed: false, streaming: true, gap: false,
  });
  // The step number on a streamed part is the head step at publish time, not
  // this part's own step: the view must not present it as a committed step.
  assert.equal(entry().props.part.step, 4);

  socket.receive(committedPart('step-5', 5, '正在生成并提交的完整正文'));
  // The committed full text supersedes the streamed prefix in place: one
  // entry, no duplicate, and no truncated committed text.
  assert.deepEqual(
    timelineNode(controller).children.map((child) => child.node_id),
    ['part.text/0'],
  );
  assert.equal(entry().props.part.content, '正在生成并提交的完整正文');
  assert.equal(entry().props.part.committed, true);
  assert.equal(entry().props.part.step, 5);
  assert.deepEqual(partState(entry().props.part), {
    committed: true, streaming: false, gap: false,
  });
  // A late chunk for the now-committed part cannot duplicate the text.
  socket.receive({
    schema_version: 2, type: 'stream_part', role: 'subject',
    content: '迟到的分片', message_id: 'step-5', part_id: 'text/0',
    part_revision: 'rev-s1', chunk_seq: 1, terminal: true, committed_only: true,
    observed_at: '2026-11-05T02:00:01Z', step: 5,
    stream_id: 'stream-1', request_id: 'req-1', attempt_id: 'att-1',
    commit_ref: 'ledger/51',
  });
  assert.equal(entry().props.part.content, '正在生成并提交的完整正文');
  // The committed step now owns the surface that referenced it.
  socket.receive(documentFrom('step-5', 'doc-5', 'doc-rev-5'));
  assert.deepEqual(
    timelineNode(controller).children.map((child) => child.node_id),
    ['part.text/0', 'document.doc-5'],
  );
});

test('a block that cannot become a surface keeps its reason beside the step', async (t) => {
  const { sockets } = environment(t);
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  socket.receive(committedPart('step-4', 4, '```shenko-ui\n{不是合法文档}\n```'));
  const failure = {
    schema_version: 1,
    type: 'document_error',
    source_step_id: 'step-4',
    source_commit: 'ledger/4',
    reason: 'SEMANTIC_DOCUMENT_INVALID: 未知节点 kind',
  };
  socket.receive(failure);
  // Replay repeats the same failure fact; it is not a second failure and must
  // not produce a duplicate entry.
  socket.receive(failure);
  const children = timelineNode(controller).children;
  assert.deepEqual(children.map((child) => child.kind), ['subject_part', 'document_error']);
  assert.equal(children[1].props.error.reason, failure.reason);
  assert.equal(children[1].props.error.source_step_id, 'step-4');
  assert.equal(children[1].props.error.source_commit, 'ledger/4');
  assert.deepEqual(
    timelineNode(controller).props.entries.map((entry) => entry.kind),
    ['subject_part', 'document_error'],
  );
});

test('opening an HTML work reads its raw content and reveals its revision-bound details', async (t) => {
  const { sockets } = environment(t);
  const artifactId = 'html-work';
  const html = '<!doctype html><h1>作品与 emoji 🌲</h1>';
  const work = { artifact_id: artifactId, title: 'A page', mime: 'text/html', revision: 4 };
  const json = (value) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
  globalThis.fetch = async (path) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (path === '/api/playground') return json({ available: true, artifacts: [work] });
    if (path === `/playground/${artifactId}/manifest`) return json({ record: { title: work.title, record_revision: 9 } });
    if (path === `/playground/${artifactId}/versions`) return json({ ok: true, revisions: [1, 4] });
    if (path === `/playground/${artifactId}`) return new Response(html, { headers: { 'Content-Type': 'text/html', 'X-Artifact-Revision': '4' } });
    return new Response('not found', { status: 404 });
  };
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  open(sockets[0]);
  intent(controller, `work.${artifactId}`, `work.open.${artifactId}`);
  await setImmediate();
  const panel = controller.projection().nodes.find((node) => node.node_id === 'context');
  assert.equal(panel.props.work?.content, html);
  assert.equal(panel.props.work?.revision, 4);
  assert.deepEqual(panel.props.work?.revisions, [1, 4]);
  assert.equal(panel.props.work_error, false);
  assert.equal(panel.props.open, true);
});
