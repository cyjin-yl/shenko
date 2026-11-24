export interface WidgetSpan {
  cols: number;
  rows: number;
}

export function clampWidgetSpan(span: WidgetSpan, maxCols = 6): WidgetSpan {
  return {
    cols: Math.max(1, Math.min(maxCols, Math.round(span.cols))),
    rows: Math.max(1, Math.min(5, Math.round(span.rows))),
  };
}

/** One pointer owns a resize until release, cancellation or its widget retires. */
export function startWidgetResize({
  pointer, grip, scope, origin, maxCols, colStride, rowStride, onChange, onEnd,
}: {
  pointer: Pick<PointerEvent, 'pointerId' | 'clientX' | 'clientY'>;
  grip: HTMLElement;
  scope: Window;
  origin: WidgetSpan;
  maxCols: number;
  colStride: number;
  rowStride: number;
  onChange: (span: WidgetSpan) => void;
  onEnd: (committed: WidgetSpan | null) => void;
}): () => void {
  let active = true;
  let latest = origin;
  const move = (event: PointerEvent) => {
    if (!active || event.pointerId !== pointer.pointerId) return;
    const next = clampWidgetSpan({
      cols: origin.cols + Math.round((event.clientX - pointer.clientX) / Math.max(40, colStride)),
      rows: origin.rows + Math.round((event.clientY - pointer.clientY) / rowStride),
    }, maxCols);
    if (next.cols !== latest.cols || next.rows !== latest.rows) {
      latest = next;
      onChange(next);
    }
  };
  const finish = (commit: boolean) => {
    if (!active) return;
    active = false;
    scope.removeEventListener('pointermove', move);
    scope.removeEventListener('pointerup', release);
    scope.removeEventListener('pointercancel', cancelPointer);
    scope.removeEventListener('blur', cancel);
    grip.removeEventListener('lostpointercapture', cancelPointer);
    if (grip.hasPointerCapture(pointer.pointerId)) grip.releasePointerCapture(pointer.pointerId);
    onEnd(commit ? latest : null);
  };
  const release = (event: PointerEvent) => {
    if (event.pointerId !== pointer.pointerId) return;
    move(event);
    finish(true);
  };
  const cancelPointer = (event: PointerEvent) => {
    if (event.pointerId === pointer.pointerId) finish(false);
  };
  const cancel = () => finish(false);
  scope.addEventListener('pointermove', move);
  scope.addEventListener('pointerup', release);
  scope.addEventListener('pointercancel', cancelPointer);
  scope.addEventListener('blur', cancel);
  grip.addEventListener('lostpointercapture', cancelPointer);
  try {
    grip.setPointerCapture(pointer.pointerId);
  } catch {
    // A target retired before capture cannot own a later window event.
    finish(false);
  }
  return cancel;
}
