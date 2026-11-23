import assert from 'node:assert/strict';
import test from 'node:test';
import path from 'node:path';
import { build } from 'esbuild';

const compiled = await build({
  entryPoints: [path.resolve(import.meta.dirname, '../src/inspector-tree-model.ts')],
  bundle: true, write: false, format: 'esm', platform: 'node', target: 'es2022',
});
const { artifactEntries, taskEntries, presentHierarchy } = await import(
  'data:text/javascript;base64,' + Buffer.from(compiled.outputFiles[0].text).toString('base64')
);
const flattened = (nodes) => nodes.flatMap((node) => [node.entry, ...flattened(node.children)]);
const task = (task_id, extra = {}) => ({
  task_id, kind: 'plugin.operation', role: 'unknown', state: 'pending',
  created_at: '2026-11-23T00:00:00Z', ...extra,
});

test('artifact categories use received MIME and keep revision facts out of hierarchy identity', () => {
  const first = Object.freeze({ artifact_id: 'same', title: '报告', mime: 'text/html', revision: 4, parent_revision: 3 });
  const second = Object.freeze({ artifact_id: 'notes', title: 'Notes', mime: 'text/markdown', revision: 2 });
  const third = Object.freeze({ artifact_id: 'other', title: 'Other page', mime: 'text/html' });
  const source = Object.freeze([first, second, third]);
  const entries = artifactEntries(source);
  assert.deepEqual(entries.map((entry) => [entry.key, entry.parentKey]), [
    ['mime:text/html', undefined], ['artifact:same', 'mime:text/html'],
    ['artifact:other', 'mime:text/html'], ['mime:text/markdown', undefined],
    ['artifact:notes', 'mime:text/markdown'],
  ]);
  assert.equal(entries[1].artifact, first);
  assert.equal(entries[1].artifact.parent_revision, 3);
  assert.equal(entries.some((entry) => entry.key === '3' || entry.parentKey === '3'), false);
  assert.equal(entries[0].count, 2);
  assert.equal(entries[3].count, 1);
});

test('task parents come only from explicit parent_task_id and remain separate from artifact IDs', () => {
  const records = Object.freeze([
    Object.freeze(task('same', { role: 'subject_root', name: '主体' })),
    Object.freeze(task('child', { parent_task_id: 'same', role: 'delegation', name: '整理员', team: 'same' })),
    Object.freeze(task('tool', { parent_task_id: 'child', role: 'tool', kind: 'fs.read', created_by_step_id: 'same' })),
    Object.freeze(task('orphan', { parent_task_id: 'missing', team: 'same' })),
    Object.freeze(task('unclassified', { kind: 'model.child', created_by_step_id: 'same' })),
  ]);
  const entries = taskEntries(records);
  assert.deepEqual(entries.map((entry) => [entry.key, entry.parentKey]), [
    ['task:same', undefined], ['task:child', 'task:same'], ['task:tool', 'task:child'],
    ['task:orphan', 'task:missing'], ['task:unclassified', undefined],
  ]);
  assert.equal(entries[4].task.role, 'unknown');
  assert.equal(entries[1].task, records[1]);
  assert.equal(entries[0].key === artifactEntries([{ artifact_id: 'same' }])[1].key, false);
  assert.equal(entries[1].label, '整理员');
});

test('presentation preserves stable record keys through authored insert, reorder, removal and reparent', () => {
  const a = Object.freeze({ key: 'a', label: 'A' });
  const b = Object.freeze({ key: 'b', label: 'B' });
  const child = Object.freeze({ key: 'child', parentKey: 'a', label: 'Child' });
  const before = presentHierarchy(Object.freeze([a, child, b]));
  assert.deepEqual(flattened(before).map((entry) => entry.key), ['a', 'child', 'b']);
  const moved = Object.freeze({ ...child, parentKey: 'b' });
  const inserted = Object.freeze({ key: 'new', parentKey: 'b', label: 'New' });
  const after = presentHierarchy(Object.freeze([b, moved, inserted, a]));
  assert.deepEqual(flattened(after).map((entry) => [entry.key, entry.parentKey]), [
    ['b', undefined], ['child', 'b'], ['new', 'b'], ['a', undefined],
  ]);
  assert.equal(before[0].children[0].reactKey, after[0].children[0].reactKey);
  assert.equal(after[0].children[0].entry, moved);
  const removed = presentHierarchy(Object.freeze([b, inserted, a]));
  assert.deepEqual(flattened(removed).map((entry) => entry.key), ['b', 'new', 'a']);
});

test('duplicate, missing-parent, cycle and empty identity stay invalid inputs instead of being repaired', () => {
  const source = Object.freeze([
    Object.freeze({ key: 'a', parentKey: 'b', label: 'Cycle A' }),
    Object.freeze({ key: 'b', parentKey: 'a', label: 'Cycle B' }),
    Object.freeze({ key: 'a', label: 'Duplicate A' }),
    Object.freeze({ key: 'orphan', parentKey: 'absent', label: 'Orphan' }),
    Object.freeze({ key: '', label: 'No identity' }),
  ]);
  const output = flattened(presentHierarchy(source));
  assert.equal(output.length, source.length);
  assert.equal(new Set(output).size, source.length);
  for (const entry of source) assert.equal(output.includes(entry), true);
  assert.equal(output.filter((entry) => entry.key === 'a').length, 2);
  assert.equal(output.find((entry) => entry.key === 'orphan').parentKey, 'absent');
  assert.equal(output.includes(source[0]) && source[0].parentKey === 'b', true);
  assert.equal(output.includes(source[1]) && source[1].parentKey === 'a', true);
  assert.equal(output.some((entry) => entry.key === ''), true);
});
