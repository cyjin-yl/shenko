// Deterministic controller/transport fixtures, not native browser or provider evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { setImmediate } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: "export { createLiveController } from './src/live-controller.ts'; export { intentFor, partState } from './src/projection.ts'; export { collectThreads } from './src/threads.ts';",
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'controller-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'browser',
});
const { createLiveController, intentFor, partState, collectThreads } = await import(
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
    commit_seq: step,
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
    timelineNode(controller).children.map((child) => child.kind),
    ['subject_part', 'semantic_document'],
  );
  const documentNode = timelineNode(controller).children[1];
  assert.equal(documentNode.props.document.source_step_id, 'step-3');
  assert.equal(documentNode.props.document.source_commit, 'ledger/step-3');

  // A surface whose step is not rendered is kept readable at the end rather
  // than dropped or attached to an unrelated entry.
  socket.receive(documentFrom('step-9', 'doc-2', 'doc-rev-2'));
  const children = timelineNode(controller).children;
  assert.deepEqual(children.map((child) => child.kind), ['subject_part', 'semantic_document', 'semantic_document']);
  assert.equal(children[2].props.document.source_step_id, 'step-9');
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
  sockets[0].receive(committedPart('reading-step', 4, '第一段正文继续说明'));
  const entryKey = timelineNode(first).children[0].node_id;
  const anchor = {
    entry_key: entryKey,
    entry_index: 0,
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
    entry_key: entryKey, text: '第一段正文继续说明', start: 2, end: 6,
  });
  assert.equal(timelineNode(first).props.selection.start, 2);
  assert.equal(timelineNode(first).props.selection.entry_key, entryKey);
  // Nothing about a reading place is written as a business fact: the only
  // records are browser display state.
  assert.equal([...storage.keys()].some((key) => key.startsWith('shenko.submission.')), false);
  first.dispose();

  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  open(sockets[1]);
  sockets[1].receive(committedPart('reading-step', 4, '第一段正文继续说明'));
  const restored = timelineNode(reopened);
  assert.equal(restored.props.reading, 'reading_history');
  assert.deepEqual(restored.props.anchor, anchor);
  assert.equal(restored.props.wrap, true);
  assert.deepEqual(restored.props.selection, {
    entry_key: entryKey, start: 2, end: 6,
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
    .find((child) => child.kind === 'subject_part' && child.props.part.message_id === 'step-5');
  const streamedIdentity = entry().node_id;
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
    [streamedIdentity],
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
    [streamedIdentity, 'document.doc-5'],
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
  const olderCandidate = `blake3:${'a'.repeat(64)}`;
  const latestCandidate = `blake3:${'b'.repeat(64)}`;
  const published = [];
  const json = (value) => new Response(JSON.stringify(value), { headers: { 'Content-Type': 'application/json' } });
  globalThis.fetch = async (path, options) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (path === '/api/playground') return json({ available: true, artifacts: [work], roots: [[artifactId, '/owned/work/root', 'blake3:root-identity']] });
    if (path === `/playground/${artifactId}/manifest`) return json({ record: { title: work.title, record_revision: 9, candidate_manifest_refs: [{ digest: olderCandidate }, { digest: latestCandidate }] } });
    if (path === `/playground/${artifactId}/versions`) return json({ ok: true, revisions: [1, 4] });
    if (path === `/playground/${artifactId}`) return new Response(html, { headers: { 'Content-Type': 'text/html', 'X-Artifact-Revision': '4' } });
    if (path === `/playground/${artifactId}/publish`) {
      published.push(JSON.parse(options.body));
      return json({ ok: true });
    }
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
  const playground = controller.projection().nodes.find((node) => node.props.playground)?.props.playground;
  assert.deepEqual(playground?.roots, [{ artifact_id: artifactId, root: '/owned/work/root', identity: 'blake3:root-identity' }]);
  intent(controller, 'context', `work.publish.${artifactId}`);
  await setImmediate();
  assert.deepEqual(published.map(({ manifest_digest, revision }) => ({ manifest_digest, revision })), [{ manifest_digest: latestCandidate, revision: 4 }]);
});

function historicalInput(seq, stepId) {
  const id = `history-${seq}`;
  return {
    payload: { schema_version: 2, type: 'submit', submission_id: id, message_id: `message-${seq}`,
      subject_id: 'fixture-subject', mode: 'append', content: `原始输入 ${seq}`, attachment_refs: [] },
    created_at: '2026-01-01T00:00:00.000000000Z', accepted_seq: seq,
    view: { schema_version: 2, type: 'submission_view', submission_id: id, result: 'found',
      stage: 'committed', event_id: `event-${seq}`, accepted_commit: `ledger/${seq}`,
      selected_step_id: stepId, committed_at: `ledger/${seq + 1}` },
  };
}

test('replayed replies keep local ledger order ahead of newer unselected owner inputs', async (t) => {
  const { sockets } = environment(t);
  const older = historicalInput(3, 'old-step');
  const pending = historicalInput(12);
  pending.view.stage = 'accepted';
  delete pending.view.committed_at;
  // Admission order remains authoritative even when browser clocks differ.
  pending.created_at = '2025-01-01T00:00:00Z';
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => json(path === '/api/self' ? { subject_id: 'fixture-subject' }
    : String(path).startsWith('/api/submissions?') ? {
      available: true, subject_id: 'fixture-subject', total: 2,
      items: [pending, older], has_older: false, next_before: null,
    } : {});
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  socket.receive({ ...committedPart('old-step', 10, '原先已提交的回复'),
    commit_ref: 'foreign-source/700', observed_at: '2099-01-01T00:00:00Z' });
  const direct = () => collectThreads({ projection: controller.projection(), tasks: [], conversations: [] })[0];
  assert.deepEqual(direct().messages.map((message) => message.text),
    ['原始输入 3', '原先已提交的回复', '原始输入 12']);
  assert.equal(direct().messages.find((message) => message.text === '原先已提交的回复').at, undefined,
    'a replay arrival in 2099 is not a historical message timestamp');
  assert.equal(direct().updated_at, pending.created_at);

  intent(controller, 'composer', 'composer.draft', { value: '本页新发送的输入' });
  intent(controller, 'composer', 'composer.append');
  const input = socket.sent.find((message) => message.type === 'submit');
  view(socket, input, 'accepted', { accepted_commit: 'ledger/14' });
  assert.deepEqual(direct().messages.map((message) => message.text),
    ['原始输入 3', '原先已提交的回复', '原始输入 12', '本页新发送的输入']);
});

test('queried historical inputs stay outside the browser outbox until an explicit retry', async (t) => {
  const { storage, sockets } = environment(t);
  const historical = historicalInput(30);
  historical.view.stage = 'accepted';
  delete historical.view.committed_at;
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => json(path === '/api/self' ? { subject_id: 'fixture-subject' }
    : String(path).startsWith('/api/submissions?') ? {
      available: true, subject_id: 'fixture-subject', total: 1,
      items: [historical], has_older: false, next_before: null,
    } : {});
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  intent(controller, 'composer', 'composer.draft', { value: '本机尚待处理的输入' });
  intent(controller, 'composer', 'composer.append');
  const local = socket.sent.find((message) => message.type === 'submit');
  t.mock.timers.tick(2000);
  assert.ok(socket.sent.some((message) => message.type === 'query_submission'
    && message.submission_id === historical.payload.submission_id));
  socket.receive({ ...historical.view, send_epoch: 0 });
  view(socket, local, 'accepted');
  assert.equal(storage.has('shenko.submission.v2.history-30'), false);
  assert.ok(storage.has(`shenko.submission.v2.${local.submission_id}`));
  assert.equal(socket.sent.filter((message) => message.type === 'submit').length, 1);
  socket.receive({ schema_version: 2, type: 'submission_view', result: 'not_admitted',
    submission_id: historical.payload.submission_id, send_epoch: 0 });
  assert.equal(controller.projection().nodes.find((node) => node.node_id === 'composer').props.draft, '');
  assert.equal(storage.has('shenko.submission.v2.history-30'), false);
  const row = timelineNode(controller).children.find((entry) => entry.node_id === 'submission.history-30');
  intent(controller, row.node_id, row.props.retry_action);
  assert.equal(socket.sent.filter((message) => message.type === 'submit').length, 2);
  assert.deepEqual(socket.sent.filter((message) => message.type === 'submit').at(-1), historical.payload);
  assert.deepEqual(socket.persistedAtSend.at(-1).payload, historical.payload);
});

test('reopened owner history pairs each reply, loads earlier pages and keeps copy actions independent', async (t) => {
  const { storage, sockets } = environment(t);
  const copied = [];
  const originalNavigator = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
  Object.defineProperty(globalThis, 'navigator', { configurable: true,
    value: { clipboard: { writeText: async (text) => copied.push(text) } } });
  t.after(() => {
    if (originalNavigator) Object.defineProperty(globalThis, 'navigator', originalNavigator);
    else delete globalThis.navigator;
  });
  const requests = [];
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (String(path).startsWith('/api/submissions?')) {
      requests.push(String(path));
      const before = new URL(String(path), 'http://fixture.invalid').searchParams.get('before_seq');
      return json({ available: true, subject_id: 'fixture-subject', total: 2,
        items: [historicalInput(before ? 4 : 20, before ? 'step-4' : 'step-20')],
        has_older: !before, next_before: before ? null : { seq: 20, submission_id: 'history-20' } });
    }
    return json({});
  };
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  socket.receive(committedPart('step-4', 4, '较早的真实回复'));
  socket.receive(committedPart('step-20', 20, '较后的真实回复'));
  let direct = collectThreads({ projection: controller.projection(), tasks: [], conversations: [] })[0];
  assert.equal(direct.messages.find((message) => message.from.role === 'owner').text, '原始输入 20');
  assert.deepEqual(direct.messages.filter((message) => message.kind === 'message').map((message) => message.text),
    ['原始输入 20', '较后的真实回复']);
  assert.equal(timelineNode(controller).props.submission_history.has_older, true);
  intent(controller, 'timeline', 'submissions.load_older');
  await setImmediate();
  direct = collectThreads({ projection: controller.projection(), tasks: [], conversations: [] })[0];
  assert.deepEqual(direct.messages.map((message) => message.text),
    ['原始输入 4', '较早的真实回复', '原始输入 20', '较后的真实回复']);
  assert.equal(timelineNode(controller).props.submission_history.has_older, false);
  const entries = timelineNode(controller).children;
  assert.equal(new Set(entries.map((entry) => entry.node_id)).size, 4);
  const replies = entries.filter((entry) => entry.kind === 'subject_part');
  for (const reply of replies) intent(controller, reply.node_id, reply.action_refs[0]);
  await setImmediate();
  assert.deepEqual(copied, ['较早的真实回复', '较后的真实回复']);
  assert.equal([...storage.keys()].some((key) => key.startsWith('shenko.submission.v2.')), false);
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
  const count = requests.length;
  intent(controller, 'timeline', 'submissions.load_older');
  await setImmediate();
  assert.equal(requests.length, count, 'a complete history issues no next-page request');
  controller.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const restored = timelineNode(reopened).children.find((entry) => entry.kind === 'user_submission').props.submission;
  assert.equal(restored.payload.submission_id, 'history-20');
  assert.equal(restored.selected_step_id, 'step-20');
  assert.equal(restored.receipt, 'committed');
});

test('a committed reply cannot use an earlier history snapshot to claim independent thought', async (t) => {
  const { sockets } = environment(t);
  const pages = [];
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (String(path).startsWith('/api/submissions?')) {
      return new Promise((resolve) => pages.push(resolve));
    }
    return json({});
  };
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  const reply = committedPart('step-99', 99, '新输入的真实回复');
  socket.receive(reply);
  t.mock.timers.tick(600);
  const olderView = historicalInput(30);
  olderView.view.stage = 'accepted';
  delete olderView.view.selected_step_id;
  delete olderView.view.committed_at;
  const page = (row) => json({ available: true, subject_id: 'fixture-subject', total: 1,
    items: [row], has_older: false, next_before: null });
  pages[0](page(olderView));
  await setImmediate();
  assert.equal(pages.length, 2, 'the latest refresh remains owed after the in-flight page');
  assert.equal(timelineNode(controller).props.submission_history.stale, true);
  const direct = () => collectThreads({ projection: controller.projection(), tasks: [], conversations: [] })[0];
  assert.equal(direct().messages.some((message) => message.text.includes('独自思考')), false);
  pages[1](page(historicalInput(30, 'step-99')));
  await setImmediate();
  assert.equal(timelineNode(controller).props.submission_history.stale, false);
  assert.deepEqual(direct().messages.filter((message) => message.kind === 'message').map((message) => message.text),
    ['原始输入 30', '新输入的真实回复']);
  socket.receive(reply);
  t.mock.timers.tick(600);
  await setImmediate();
  assert.equal(pages.length, 2, 'replay of an already committed identity does not invalidate history');
  assert.equal(timelineNode(controller).props.submission_history.stale, false);
  socket.receive(committedPart('step-100', 100, '下一轮回复'));
  t.mock.timers.tick(600);
  await setImmediate();
  pages[2](new Response('history unavailable', { status: 503 }));
  await setImmediate();
  assert.match(timelineNode(controller).props.submission_history.error, /503/);
  assert.equal(timelineNode(controller).props.submission_history.loading, false);
  t.mock.timers.tick(600);
  await setImmediate();
  assert.equal(pages.length, 3, 'a failed history request keeps its error instead of retrying itself');
});

test('unavailable and invalid history pages preserve both admitted and pending inputs', async (t) => {
  const { sockets } = environment(t);
  let phase = 'first';
  const requests = [];
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (String(path).startsWith('/api/submissions?')) {
      requests.push(String(path));
      if (phase === 'unavailable') return json({ available: false, items: [], note: 'storage unavailable' });
      return json({ available: true, subject_id: 'fixture-subject', total: 2,
        items: [historicalInput(phase === 'invalid' ? 40 : 30, 'step-30')],
        has_older: true, next_before: { seq: phase === 'invalid' ? 40 : 30,
          submission_id: phase === 'invalid' ? 'history-40' : 'history-30' } });
    }
    return json({});
  };
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  intent(controller, 'composer', 'composer.draft', { value: '尚未收到回执的原输入' });
  intent(controller, 'composer', 'composer.append');
  const pendingId = socket.sent.find((message) => message.type === 'submit').submission_id;
  const original = () => timelineNode(controller).children.filter((entry) => entry.kind === 'user_submission');
  const saved = original().find((entry) => entry.props.submission.payload.submission_id === pendingId).props.submission.receipt;
  for (phase of ['unavailable', 'invalid']) {
    intent(controller, 'timeline', 'submissions.load_older');
    await setImmediate();
    assert.deepEqual(new Set(original().map((entry) => entry.props.submission.payload.submission_id)),
      new Set(['history-30', pendingId]));
    assert.equal(original().find((entry) => entry.props.submission.payload.submission_id === pendingId).props.submission.receipt, saved);
    assert.deepEqual(timelineNode(controller).props.submission_history.next_before,
      { seq: 30, submission_id: 'history-30' });
    assert.ok(timelineNode(controller).props.submission_history.error);
  }
  assert.ok(requests.slice(-2).every((path) => path.includes('before_seq=30') && path.includes('before_id=history-30')));
  assert.equal(socket.sent.filter((message) => message.type === 'submit').length, 1);
});

test('history pages sharing one import commit retain every ID in SQLite binary order', async (t) => {
  const { storage, sockets } = environment(t);
  const ids = ['same-\u{10000}', 'same-\ue000', 'same-a'];
  const rows = ids.map((id) => {
    const row = historicalInput(50);
    row.payload.submission_id = id;
    row.payload.message_id = `message-${id}`;
    row.payload.content = `原始输入 ${id}`;
    row.view = { ...row.view, submission_id: id, stage: 'accepted' };
    delete row.view.committed_at;
    return row;
  });
  const requests = [];
  const json = (value) => new Response(JSON.stringify(value));
  globalThis.fetch = async (path) => {
    if (path === '/api/self') return json({ subject_id: 'fixture-subject' });
    if (!String(path).startsWith('/api/submissions?')) return json({});
    const query = new URL(String(path), 'http://fixture.invalid').searchParams;
    requests.push(query);
    const earlier = query.has('before_id');
    return json({ available: true, subject_id: 'fixture-subject', total: 3,
      items: earlier ? rows.slice(2) : rows.slice(0, 2), has_older: !earlier,
      next_before: earlier ? null : { seq: 50, submission_id: ids[1] } });
  };
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = sockets[0];
  open(socket);
  await setImmediate();
  assert.equal(timelineNode(controller).props.submission_history.error, undefined);
  intent(controller, 'timeline', 'submissions.load_older');
  await setImmediate();
  assert.deepEqual(timelineNode(controller).children.filter((entry) => entry.kind === 'user_submission')
    .map((entry) => entry.props.submission.payload.submission_id), [...ids].reverse());
  assert.equal(requests.at(-1).get('before_seq'), '50');
  assert.equal(requests.at(-1).get('before_id'), ids[1]);
  assert.equal(timelineNode(controller).props.submission_history.has_older, false);
  assert.equal([...storage.keys()].some((key) => key.startsWith('shenko.submission.v2.')), false);
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
});

const editAttachment = {
  digest: `blake3:${'a'.repeat(64)}`, name: 'admitted.txt', media_type: 'text/plain', bytes: 7,
};

async function editableFixture(t, history) {
  const env = environment(t);
  if (history) {
    const fetch = globalThis.fetch;
    globalThis.fetch = async (path, options) => String(path).startsWith('/api/submissions?')
      ? new Response(JSON.stringify({ available: true, subject_id: 'fixture-subject',
        total: history.length, items: history, has_older: false, next_before: null }))
      : fetch(path, options);
  }
  const original = { schema_version: 2, type: 'submit', mode: 'append',
    submission_id: 'edit-original', message_id: 'edit-message', subject_id: 'fixture-subject',
    content: 'Original admitted input', attachment_refs: [editAttachment] };
  env.storage.set(`shenko.submission.v2.${original.submission_id}`, JSON.stringify({
    payload: original, created_at: '2026-11-19T00:00:00Z', receipt: 'accepted', send_epoch: 1,
  }));
  const controller = createLiveController();
  t.after(() => controller.dispose());
  await setImmediate();
  const socket = env.sockets[0];
  openEditable(socket);
  view(socket, original, 'accepted');
  return { ...env, controller, socket, original };
}

function openEditable(socket) {
  socket.open();
  socket.receive({ schema_version: 2, type: 'hello', capabilities: {
    append: true, withdraw: true, attachments: true, durable_input_receipts: true, committed_replay: true,
  } });
}

function editRow(controller) {
  return timelineNode(controller).children.find((row) => row.node_id === 'submission.edit-original');
}

function chooseEdit(controller) {
  const row = editRow(controller);
  intent(controller, row.node_id, row.props.edit_action);
}

function composer(controller) {
  return controller.projection().nodes.find((node) => node.node_id === 'composer').props;
}

test('queued edit leaves the prior state intact when the socket is unavailable', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  intent(controller, 'composer', 'composer.draft', { value: 'Another draft' });
  // The action was projected online, but the real socket closed before dispatch.
  socket.readyState = 3;
  chooseEdit(controller);
  assert.equal(composer(controller).draft, 'Another draft');
  assert.equal(editRow(controller).props.submission.edit, undefined);
  assert.deepEqual(editRow(controller).props.submission.payload, original);
  assert.equal(socket.sent.some((message) => message.type === 'withdraw' || message.type === 'submit'), false);
  assert.deepEqual(JSON.parse(storage.get('shenko.submission.v2.edit-original')).payload, original);
  socket.readyState = 1;
  const setItem = localStorage.setItem;
  localStorage.setItem = () => { throw new Error('storage refused'); };
  chooseEdit(controller);
  localStorage.setItem = setItem;
  assert.equal(editRow(controller).props.submission.edit, undefined);
  assert.equal(composer(controller).draft, 'Another draft');
  assert.equal(socket.sent.some((message) => message.type === 'withdraw' || message.type === 'submit'), false);
});

test('queued edit retains one cancel through transport failure refusal and queued selection', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  const send = socket.send.bind(socket);
  socket.send = (text) => {
    if (JSON.parse(text).type === 'withdraw') throw new Error('lost withdrawal transport');
    send(text);
  };
  chooseEdit(controller);
  const cancelId = JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.cancel_id;
  assert.equal(composer(controller).draft, '');
  for (const result of ['temporarily_unavailable', 'invalid', 'not_admitted']) {
    socket.receive({ schema_version: 2, type: 'submission_view', result,
      submission_id: original.submission_id });
    assert.equal(composer(controller).draft, '', result);
    assert.deepEqual(editRow(controller).props.submission.payload, original);
    assert.ok(editRow(controller).props.edit_action, `${result} retains same-cancel Edit`);
  }
  socket.send = send;
  chooseEdit(controller);
  assert.equal(socket.sent.filter((message) => message.type === 'withdraw').at(-1).cancel_id, cancelId);
  const withdrawal = { cancel_id: cancelId, commit_ref: 'ledger/4', result: 'withdrawal_queued' };
  view(socket, original, 'selected', { selected_step_id: 'already-selected', withdrawal });
  view(socket, original, 'selected', { selected_step_id: 'already-selected', withdrawal });
  assert.equal(composer(controller).draft, '');
  assert.deepEqual(composer(controller).attachments, []);
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
  assert.equal(editRow(controller).props.edit_action, undefined, 'queued cancellation cannot safely edit');
  assert.deepEqual(JSON.parse(storage.get('shenko.submission.v2.edit-original')).payload, original);
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'closed');
  view(socket, original, 'committed', { committed_at: 'ledger/6', withdrawal });
  assert.equal(storage.has('shenko.submission.v2.edit-original'), false);
  const queries = socket.sent.filter((message) => message.type === 'query_submission').length;
  t.mock.timers.tick(6000);
  assert.equal(socket.sent.filter((message) => message.type === 'query_submission').length, queries,
    'a queued edit stops querying after original disposition');
});

test('queued edit restores exact attachments only after its durable cancel and ignores replay', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  for (const withdrawal of [
    { cancel_id: cancel.cancel_id, result: 'withdrawn_before_presentation' },
    { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'unknown' },
    { cancel_id: 'different-cancel', commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' },
  ]) {
    view(socket, original, 'accepted', { withdrawal });
    assert.equal(composer(controller).draft, '');
    assert.deepEqual(composer(controller).attachments, []);
  }
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  assert.equal(composer(controller).draft, original.content);
  assert.deepEqual(composer(controller).attachments.map((item) => item.ref), original.attachment_refs);
  assert.deepEqual(editRow(controller).props.submission.payload, original);
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
  intent(controller, 'composer', 'composer.draft', { value: 'Edited content' });
  view(socket, original, 'accepted', { withdrawal });
  assert.equal(composer(controller).draft, 'Edited content');
  const saved = JSON.parse(storage.get('shenko.submission.v2.edit-original'));
  assert.equal(saved.edit.draft.content, 'Edited content');
  intent(controller, 'composer', 'composer.append');
  intent(controller, 'composer', 'composer.append');
  const replacements = socket.sent.filter((message) => message.type === 'submit');
  assert.equal(replacements.length, 1);
  assert.equal(replacements[0].submission_id, saved.edit.submission_id);
  assert.notEqual(replacements[0].submission_id, original.submission_id);
  assert.equal(replacements[0].content, 'Edited content');
  assert.deepEqual(replacements[0].attachment_refs, original.attachment_refs);
  view(socket, original, 'accepted', { withdrawal });
  assert.equal(composer(controller).draft, '');
});

test('queued edit reopens by original identity and preserves an occupied composer until recovery', async (t) => {
  const { controller: first, sockets, socket, original, storage } = await editableFixture(t);
  chooseEdit(first);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  first.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const next = sockets[1];
  openEditable(next);
  assert.ok(next.sent.some((message) => message.type === 'query_submission'
    && message.submission_id === original.submission_id));
  assert.equal(next.sent.some((message) => message.type === 'submit' || message.type === 'withdraw'), false);
  intent(reopened, 'composer', 'composer.draft', { value: 'Keep this draft' });
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(next, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  assert.equal(composer(reopened).draft, 'Keep this draft');
  assert.deepEqual(composer(reopened).attachments, []);
  assert.ok(editRow(reopened).props.edit_action);
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'ready');
  intent(reopened, 'composer', 'composer.draft', { value: '' });
  chooseEdit(reopened);
  assert.equal(composer(reopened).draft, original.content);
  assert.deepEqual(composer(reopened).attachments.map((item) => item.ref), original.attachment_refs);
  intent(reopened, 'composer', 'composer.draft', { value: 'Survives another reopen' });
  reopened.dispose();
  const again = createLiveController();
  t.after(() => again.dispose());
  await setImmediate();
  const last = sockets[2];
  openEditable(last);
  assert.equal(composer(again).draft, '');
  view(last, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  assert.equal(composer(again).draft, 'Survives another reopen');
  assert.deepEqual(composer(again).attachments.map((item) => item.ref), original.attachment_refs);
  assert.equal(last.sent.some((message) => message.type === 'submit'), false);
});

test('queued edit replacement transport failure retains one ID and exact attachment retry', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  view(socket, original, 'accepted', { withdrawal: { cancel_id: cancel.cancel_id,
    commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' } });
  const reservedId = JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.submission_id;
  const send = socket.send.bind(socket);
  socket.send = (text) => {
    if (JSON.parse(text).type === 'submit') throw new Error('lost replacement transport');
    send(text);
  };
  intent(controller, 'composer', 'composer.append');
  assert.equal(composer(controller).draft, '');
  const saved = JSON.parse(storage.get(`shenko.submission.v2.${reservedId}`));
  assert.equal(saved.payload.submission_id, reservedId);
  assert.deepEqual(saved.payload.attachment_refs, original.attachment_refs);
  socket.send = send;
  const row = timelineNode(controller).children.find((entry) => entry.node_id === `submission.${reservedId}`);
  intent(controller, row.node_id, row.props.retry_action);
  assert.deepEqual(socket.sent.filter((message) => message.type === 'submit'), [saved.payload]);
});

test('queued edit freezes a saved replacement across failed finish save retry and reopen', async (t) => {
  const { controller, socket, sockets, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  const id = JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.submission_id;
  const key = `shenko.submission.v2.${id}`;
  const browserStorage = localStorage;
  const setItem = browserStorage.setItem;
  browserStorage.setItem = (recordKey, value) => {
    if (recordKey === 'shenko.submission.v2.edit-original' && JSON.parse(value).edit?.state === 'finished') {
      throw new Error('original finish save refused');
    }
    setItem(recordKey, value);
  };
  t.after(() => { browserStorage.setItem = setItem; });
  intent(controller, 'composer', 'composer.append');
  const frozen = JSON.parse(storage.get(key)).payload;
  assert.deepEqual(frozen.attachment_refs, original.attachment_refs);
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'editing');
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
  assert.equal(composer(controller).draft, '');
  assert.equal(editRow(controller).props.edit_action, undefined);
  assert.equal(editRow(controller).props.retry_action, undefined);
  intent(controller, 'composer', 'composer.draft', { value: 'Independent later draft' });
  const retry = (target) => {
    const row = timelineNode(target).children.find((entry) => entry.node_id === `submission.${id}`);
    intent(target, row.node_id, row.props.retry_action);
  };
  retry(controller);
  assert.deepEqual(JSON.parse(storage.get(key)).payload, frozen);
  assert.equal(socket.sent.some((message) => message.type === 'submit'), false);
  controller.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const next = sockets[1];
  openEditable(next);
  assert.equal(editRow(reopened).props.edit_action, undefined);
  view(next, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  next.receive({ schema_version: 2, type: 'submission_view', result: 'not_admitted', submission_id: id });
  assert.equal(composer(reopened).draft, 'Independent later draft');
  retry(reopened);
  assert.equal(next.sent.some((message) => message.type === 'submit'), false);
  assert.deepEqual(JSON.parse(storage.get(key)).payload, frozen);
  localStorage.setItem = setItem;
  retry(reopened);
  assert.deepEqual(next.sent.filter((message) => message.type === 'submit'), [frozen]);
  assert.equal(composer(reopened).draft, 'Independent later draft');
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'finished');
  view(next, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  assert.ok(storage.has('shenko.submission.v2.edit-original'), 'source link survives original disposition');
  view(next, { submission_id: id }, 'committed', { committed_at: 'ledger/9' });
  assert.equal(storage.has(key), false);
  assert.equal(storage.has('shenko.submission.v2.edit-original'), false);
  view(next, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  assert.equal(storage.has('shenko.submission.v2.edit-original'), false, 'source replay does not resurrect recovery');
});

test('queued edit replacement absence rejected and conflict never restore a second draft', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  intent(controller, 'composer', 'composer.append');
  const replacement = socket.sent.find((message) => message.type === 'submit');
  view(socket, original, 'committed', { withdrawal, committed_at: 'ledger/5' });
  assert.ok(storage.has('shenko.submission.v2.edit-original'));
  socket.receive({ schema_version: 2, type: 'submission_view', result: 'not_admitted',
    submission_id: replacement.submission_id });
  assert.equal(composer(controller).draft, '');
  for (const stage of ['rejected', 'conflict']) {
    socket.receive({ schema_version: 2, type: 'receipt', stage, submission_id: replacement.submission_id });
    assert.equal(composer(controller).draft, '', stage);
    assert.deepEqual(composer(controller).attachments, []);
    assert.deepEqual(JSON.parse(storage.get(`shenko.submission.v2.${replacement.submission_id}`)).payload, replacement);
  }
});

test('queued edit accepted source stays withdrawn after replacement disposition and late receipts', async (t) => {
  const { controller, socket, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  intent(controller, 'composer', 'composer.append');
  const replacement = socket.sent.find((message) => message.type === 'submit');
  view(socket, replacement, 'committed', { committed_at: 'ledger/9' });
  assert.equal(storage.has('shenko.submission.v2.edit-original'), false);
  assert.equal(storage.has(`shenko.submission.v2.${replacement.submission_id}`), false);
  view(socket, original, 'accepted', { withdrawal });
  socket.receive({ schema_version: 2, type: 'receipt', stage: 'accepted',
    submission_id: original.submission_id, event_id: 'durable-event', accepted_commit: 'ledger/1' });
  const row = editRow(controller).props.submission;
  assert.equal(row.receipt, 'accepted', 'withdrawal never invents original disposition');
  assert.deepEqual(row.withdrawal, withdrawal);
  assert.deepEqual(row.payload, original);
  assert.equal(storage.has('shenko.submission.v2.edit-original'), false);
  const queries = socket.sent.filter((message) => message.type === 'query_submission').length;
  t.mock.timers.tick(6000);
  assert.equal(socket.sent.filter((message) => message.type === 'query_submission').length, queries);
  assert.equal(editRow(controller).props.edit_action, undefined);
});

test('queued edit clearing releases its durable owner for another saved edit and reopen', async (t) => {
  const second = historicalInput(80);
  second.view.stage = 'accepted';
  delete second.view.selected_step_id;
  delete second.view.committed_at;
  const { controller, socket, sockets, original, storage } = await editableFixture(t, [second]);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  await setImmediate();
  const secondRow = () => timelineNode(controller).children.find((row) => row.node_id === `submission.${second.payload.submission_id}`);
  assert.equal(secondRow().props.edit_action, `submission.edit.${second.payload.submission_id}`);
  intent(controller, secondRow().node_id, secondRow().props.edit_action);
  const secondCancel = socket.sent.filter((message) => message.type === 'withdraw').at(-1);
  assert.equal(secondCancel.submission_id, second.payload.submission_id,
    'the second row Edit uses its live socket and exact withdrawal identity');
  const secondWithdrawal = { cancel_id: secondCancel.cancel_id, commit_ref: 'ledger/81', result: 'withdrawn_before_presentation' };
  view(socket, second.payload, 'accepted', { withdrawal: secondWithdrawal });
  assert.equal(composer(controller).draft, original.content, 'another saved edit waits for the composer');
  intent(controller, 'composer', 'composer.draft', { value: '' });
  const attachment = composer(controller).attachments[0];
  const setItem = localStorage.setItem;
  localStorage.setItem = () => { throw new Error('clear save refused'); };
  intent(controller, 'composer', 'composer.detach', { local_id: attachment.local_id });
  assert.equal(composer(controller).attachments.length, 1, 'failed clear retains visible ownership and attachment');
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'editing');
  localStorage.setItem = setItem;
  intent(controller, 'composer', 'composer.detach', { local_id: attachment.local_id });
  assert.equal(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.state, 'closed');
  assert.equal(composer(controller).attachments.length, 0);
  assert.equal(controller.projection().actions[secondRow().props.edit_action].enabled, true);
  intent(controller, secondRow().node_id, secondRow().props.edit_action);
  assert.equal(composer(controller).draft, second.payload.content, 'clearing released the first owner');
  controller.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const next = sockets[1];
  openEditable(next);
  view(next, original, 'accepted', { withdrawal });
  assert.equal(composer(reopened).draft, '', 'cleared empty edit cannot resurrect');
  view(next, second.payload, 'accepted', { withdrawal: secondWithdrawal });
  assert.equal(composer(reopened).draft, second.payload.content);
  assert.equal(next.sent.some((message) => message.type === 'submit'), false);
});

test('queued edit attachment snapshot refusal preserves visible refs and later complete text saves', async (t) => {
  const { controller, socket, sockets, original, storage } = await editableFixture(t);
  chooseEdit(controller);
  const cancel = socket.sent.find((message) => message.type === 'withdraw');
  const withdrawal = { cancel_id: cancel.cancel_id, commit_ref: 'ledger/4', result: 'withdrawn_before_presentation' };
  view(socket, original, 'accepted', { withdrawal });
  const setItem = localStorage.setItem;
  localStorage.setItem = () => { throw new Error('attachment save refused'); };
  const attached = composer(controller).attachments[0];
  intent(controller, 'composer', 'composer.detach', { local_id: attached.local_id });
  assert.deepEqual(composer(controller).attachments.map((item) => item.ref), original.attachment_refs);
  localStorage.setItem = setItem;
  intent(controller, 'composer', 'composer.detach', { local_id: attached.local_id });
  const uploaded = { ...editAttachment, digest: `blake3:${'b'.repeat(64)}`, name: 'uploaded.txt' };
  const fetch = globalThis.fetch;
  globalThis.fetch = async (path, options) => String(path).startsWith('/api/attachments?')
    ? new Response(JSON.stringify({ accepted: true, attachment: uploaded }))
    : fetch(path, options);
  // A successful seal followed by local storage refusal must not claim Ready.
  localStorage.setItem = () => { throw new Error('sealed snapshot refused'); };
  intent(controller, 'composer', 'composer.attach', { files: [new File(['payload'], 'uploaded.txt', { type: 'text/plain' })] });
  await setImmediate();
  assert.equal(composer(controller).attachments[0].status, 'failed');
  assert.equal(composer(controller).attachments[0].ref, undefined);
  assert.match(composer(controller).attachments[0].error, /could not be saved/);
  localStorage.setItem = setItem;
  intent(controller, 'composer', 'composer.draft', { value: 'Text after failed upload' });
  assert.deepEqual(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.draft,
    { content: 'Text after failed upload', attachment_refs: [] });
  intent(controller, 'composer', 'composer.detach', { local_id: composer(controller).attachments[0].local_id });
  intent(controller, 'composer', 'composer.attach', { files: [new File(['payload'], 'uploaded.txt', { type: 'text/plain' })] });
  await setImmediate();
  assert.equal(composer(controller).attachments[0].status, 'ready');
  localStorage.setItem = () => { throw new Error('complete text snapshot refused'); };
  intent(controller, 'composer', 'composer.draft', { value: 'Unsaved text must not appear saved' });
  assert.equal(composer(controller).draft, 'Text after failed upload');
  assert.deepEqual(composer(controller).attachments.map((item) => item.ref), [uploaded]);
  assert.deepEqual(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.draft,
    { content: 'Text after failed upload', attachment_refs: [uploaded] });
  localStorage.setItem = setItem;
  intent(controller, 'composer', 'composer.draft', { value: 'Text with the sealed upload' });
  assert.deepEqual(JSON.parse(storage.get('shenko.submission.v2.edit-original')).edit.draft,
    { content: 'Text with the sealed upload', attachment_refs: [uploaded] });
  controller.dispose();
  const reopened = createLiveController();
  t.after(() => reopened.dispose());
  await setImmediate();
  const next = sockets[1];
  openEditable(next);
  assert.equal(composer(reopened).subject_bound, true, 'reopen receives the real fixture self identity');
  view(next, original, 'accepted', { withdrawal });
  assert.equal(composer(reopened).draft, 'Text with the sealed upload');
  assert.deepEqual(composer(reopened).attachments.map((item) => item.ref), [uploaded]);
  assert.equal(next.sent.some((message) => message.type === 'submit'), false);
});
