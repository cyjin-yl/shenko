import assert from 'node:assert/strict';
import test from 'node:test';
import { startWidgetResize } from '../src/widget-resize.ts';

class Surface extends EventTarget {
  listeners = new Map();
  captures = new Set();
  addEventListener(type, listener, options) {
    const group = this.listeners.get(type) ?? new Set();
    group.add(listener);
    this.listeners.set(type, group);
    super.addEventListener(type, listener, options);
  }
  removeEventListener(type, listener, options) {
    this.listeners.get(type)?.delete(listener);
    super.removeEventListener(type, listener, options);
  }
  setPointerCapture(id) { this.captures.add(id); }
  hasPointerCapture(id) { return this.captures.has(id); }
  releasePointerCapture(id) { this.captures.delete(id); }
  get listenerCount() { return [...this.listeners.values()].reduce((sum, group) => sum + group.size, 0); }
}

function pointer(target, type, pointerId = 7, clientX = 100, clientY = 100) {
  target.dispatchEvent(Object.assign(new Event(type), { pointerId, clientX, clientY }));
}

function session() {
  const scope = new Surface();
  const grip = new Surface();
  const changes = [];
  const endings = [];
  const cancel = startWidgetResize({
    pointer: { pointerId: 7, clientX: 100, clientY: 100 }, grip, scope,
    origin: { cols: 2, rows: 2 }, maxCols: 6, colStride: 100, rowStride: 80,
    onChange: (span) => changes.push(span), onEnd: (span) => endings.push(span),
  });
  return { scope, grip, changes, endings, cancel };
}

test('resize belongs to its captured pointer and includes the final release position', () => {
  const { scope, grip, changes, endings } = session();
  assert.equal(grip.hasPointerCapture(7), true);
  pointer(scope, 'pointermove', 8, 900, 900);
  pointer(scope, 'pointerup', 8, 900, 900);
  pointer(scope, 'pointercancel', 8);
  assert.deepEqual(changes, []);
  assert.deepEqual(endings, []);
  pointer(scope, 'pointermove', 7, 200, 180);
  assert.deepEqual(changes, [{ cols: 3, rows: 3 }]);
  pointer(scope, 'pointerup', 7, 400, 260);
  assert.deepEqual(endings, [{ cols: 5, rows: 4 }]);
  assert.equal(grip.hasPointerCapture(7), false);
  assert.equal(scope.listenerCount + grip.listenerCount, 0);
  pointer(scope, 'pointermove', 7, 900, 900);
  pointer(scope, 'pointerup', 7, 900, 900);
  assert.equal(changes.length, 2);
  assert.equal(endings.length, 1);
});

test('pointer cancellation releases every listener and ignores later events without committing', () => {
  const current = session();
  pointer(current.scope, 'pointermove', 7, 300, 260);
  pointer(current.scope, 'pointercancel');
  assert.deepEqual(current.endings, [null]);
  assert.equal(current.scope.listenerCount + current.grip.listenerCount, 0);
  assert.equal(current.grip.hasPointerCapture(7), false);
  pointer(current.scope, 'pointermove', 7, 900, 900);
  pointer(current.scope, 'pointerup', 7, 900, 900);
  current.cancel();
  assert.deepEqual(current.changes, [{ cols: 4, rows: 4 }]);
  assert.deepEqual(current.endings, [null]);
});
