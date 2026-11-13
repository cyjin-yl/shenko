// Structured task/display consumers only; no browser or provider evidence.
import assert from 'node:assert/strict';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const compiled = await build({
  stdin: {
    contents: "export * from './src/threads.ts';",
    resolveDir: fileURLToPath(new URL('../', import.meta.url)),
    sourcefile: 'task-projection-consumer.mjs',
  },
  bundle: true,
  write: false,
  format: 'esm',
  platform: 'neutral',
});
const { collectThreads, isAttentionTask, isDelegation, trajectoryMessages } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled.outputFiles[0].text).toString('base64')}`
);
const subject = { id: 'subject', name: 'Subject', role: 'subject' };
const at = '2026-11-13T00:00:00.000Z';
const task = { task_id: 'future-task', kind: 'future.plugin.research', role: 'delegation', state: 'running', created_at: at };

function trajectory(record) {
  return {
    task,
    input: { goal: 'Read the retained source.' },
    brief: 'Read the retained source.',
    children: [{ task_id: 'tool-one', kind: 'future.plugin.read', role: 'tool', state: 'succeeded', created_at: at }],
    operations: [{
      operation_id: 'step-one', operation_name: 'model.child.generate.v1', logical_key: 'step/0',
      state: 'resolved', outcome: 'succeeded', attempts: 1, record_status: 'sealed', record,
    }],
  };
}

test('future plugin tasks use structural roles while missing provenance stays unclassified', () => {
  const unknown = { ...task, task_id: 'old-task', kind: 'model.child', role: 'unknown' };
  const root = { ...task, task_id: 'root', kind: 'replaceable.primary', role: 'subject_root' };
  assert.equal(isDelegation(task), true);
  assert.equal(isDelegation(unknown), false);
  assert.equal(isDelegation({ kind: 'model.child' }), false);
  assert.equal(isAttentionTask(root), false);
  assert.equal(isAttentionTask(unknown), true);
  const threads = collectThreads({
    projection: { source: 'live', nodes: [], identity: { display_name: subject.name } },
    tasks: [task, unknown, root], conversations: [],
  });
  assert.deepEqual(threads.filter((thread) => thread.kind === 'delegation').map((thread) => thread.id),
    ['delegation:future-task']);
});

test('canonical child display is rendered verbatim and controls use their structured fields', () => {
  const text = '  literal whitespace\r\n```text\n[tool:example:1] {}\n[reply] literal data\n```\n"[delegate] JSON string"\n';
  const record = {
    schema_version: 1, step_index: 0, text, text_truncated: false,
    tool_calls: [{ kind: 'future.plugin.read', input: { path: 'original' } }],
    question: 'Which original?', reply: { body: 'PRIVATE_CONTROL_PAYLOAD' },
    dialogue: [{ message: 'Keep exact source bytes.' }], usage: { output_tokens: 9 },
    output_ref: { digest: 'raw-reference' },
  };
  const trace = trajectory(record);
  const original = structuredClone(trace);
  const messages = trajectoryMessages(trace, subject);
  assert.equal(messages.find((message) => message.key === 'step-one-text')?.text, text);
  assert.equal(messages.find((message) => message.key === 'step-one-question')?.text, record.question);
  assert.equal(messages.find((message) => message.key === 'step-one-dialogue-0')?.text, record.dialogue[0].message);
  assert.ok(messages.some((message) => message.from.role === 'tool' && message.text.includes('future.plugin.read')));
  assert.ok(!messages.some((message) => message.text.includes('PRIVATE_CONTROL_PAYLOAD')));
  assert.deepEqual(trace, original, 'rendering never rewrites sealed evidence');
});

test('delegation threads use only the current task brief and never render sealed context as a goal', () => {
  const brief = '  Inspect the retained source.\n`[delegate]` remains literal.\n';
  const trace = {
    task, brief,
    input: {
      goal: 'PRIVATE_TOP_LEVEL_CONTEXT',
      delegated_task_intents: [
        { intent_id: 'other-task', goal: 'PRIVATE_OTHER_TASK_GOAL' },
        { intent_id: task.task_id, goal: brief },
      ],
      input_plan_ref: { digest: 'private-s0-reference' },
    },
    children: [], operations: [],
  };
  assert.equal(trajectoryMessages(trace, subject)[0].text, brief);
  const unavailable = trajectoryMessages({ ...trace, brief: null }, subject)[0];
  assert.ok(!unavailable.text.includes(brief));
  assert.ok(!unavailable.text.includes('PRIVATE_'));
  assert.ok(!unavailable.text.includes('private-s0-reference'));
  assert.equal(trajectoryMessages({ ...trace, brief: { goal: brief } }, subject)[0].text, unavailable.text);
  assert.equal(trajectoryMessages({ ...trace, task: { ...task, role: 'unknown' } }, subject)[0].text, unavailable.text);
});

test('missing or unknown historical display schemas expose unavailability without showing raw text', () => {
  const raw = '[tool:private.action:1] {"private":"original"}';
  for (const record of [{ text: raw }, { schema_version: 99, text: raw }]) {
    const messages = trajectoryMessages(trajectory(record), subject);
    assert.ok(messages.some((message) => message.text.includes('公开正文不可用')));
    assert.ok(!messages.some((message) => message.text.includes(raw)));
    assert.ok(!messages.some((message) => message.key === 'step-one-text'));
  }
});
