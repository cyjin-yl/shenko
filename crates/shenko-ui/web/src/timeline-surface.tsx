import { text as t } from './messages';
import * as React from 'react';
import { partState, type ReadingAnchor, type ReadingSelection, type TimelinePart, type UiNode, type UiProjection } from './projection';
import { resolveAnchor, selectionApplies } from './reading-state';
import { Icon } from './icons';
import { Button, ScrollRoot, ScrollViewport, ScrollScrollbar, ScrollThumb, type Emit } from './ui-kit';
import { EmptyTimeline, TimelineEntries } from './timeline-entries';
import { SubmissionHistory } from './submission-history';

interface AnchorSnapshot {
  key?: string;
  offset?: number;
  scrollTop: number;
  index?: number;
  previousKey?: string;
}

interface TimelineSurfaceProps {
  node: UiNode;
  projection: UiProjection;
  emit: Emit;
  /** Opens a page-bearing work as a lens beside the stream. */
  onOpenWork: (artifactId: string) => void;
}

export class TimelineSurface extends React.Component<TimelineSurfaceProps, { announcement: string }> {
  /** The bounded announcement for a screen reader (docs/specs/web-interface.md#spec-57-7).
   *
   * It says that committed content arrived and which round it was — it does
   * NOT read the answer again: a reader who is listening should not have the
   * whole answer spoken twice.
   */
  state: { announcement: string } = { announcement: '' };

  private capture = React.createRef<HTMLDivElement>();
  private scrollElement: HTMLElement | null = null;
  private lastAnchor: AnchorSnapshot | null = null;
  private resizeFrame = 0;
  private anchorFrame = 0;
  private selectionTimer: number | undefined;
  private restoredSelection: ReadingSelection | undefined;
  /** The committed entry this surface already announced. */
  private announcedEntry: string | undefined;

  componentDidMount() {
    this.scrollElement = this.findScrollElement();
    if (this.props.node.props.reading === 'following_latest') {
      this.scrollToEnd();
      // The upstream scroll area settles its viewport after first layout.
      this.resizeFrame = requestAnimationFrame(() => {
        this.resizeFrame = 0;
        if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
      });
    } else this.restoreSavedAnchor(this.props.node.props.anchor as ReadingAnchor | undefined);
    this.rememberAnchor();
    this.restoreSavedSelection();
    window.addEventListener('resize', this.onResize);
    window.visualViewport?.addEventListener('resize', this.onResize);
    document.addEventListener('selectionchange', this.onSelectionChange);
  }

  componentWillUnmount() {
    window.removeEventListener('resize', this.onResize);
    window.visualViewport?.removeEventListener('resize', this.onResize);
    document.removeEventListener('selectionchange', this.onSelectionChange);
    if (this.resizeFrame) cancelAnimationFrame(this.resizeFrame);
    if (this.anchorFrame) cancelAnimationFrame(this.anchorFrame);
    clearTimeout(this.selectionTimer);
  }

  getSnapshotBeforeUpdate(): AnchorSnapshot | null {
    return this.captureAnchor();
  }

  componentDidUpdate(_previous: TimelineSurfaceProps, _state: unknown, snapshot: AnchorSnapshot | null) {
    if (!this.scrollElement?.isConnected) this.scrollElement = this.findScrollElement();
    if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
    else if (snapshot) this.restoreAnchor(snapshot);
    this.rememberAnchor();
    this.restoreSavedSelection();
    this.announceCommitted();
  }

  /// Announce newly committed content once, in bounded words.
  private announceCommitted() {
    const entries = (this.props.node.props.entries ?? []) as UiNode[];
    let newest: { key: string; label: string } | undefined;
    for (let index = entries.length - 1; index >= 0 && !newest; index -= 1) {
      const entry = entries[index];
      if (entry.kind !== 'subject_part') continue;
      const part = entry.props.part as TimelinePart;
      if (!partState(part).committed) continue;
      newest = { key: entry.node_id, label: t('timeline.committedAnnouncement', { step: part.step }) };
    }
    if (!newest || newest.key === this.announcedEntry) return;
    this.announcedEntry = newest.key;
    this.setState((previous) => (
      previous.announcement === newest.label ? previous : { announcement: newest.label }
    ));
  }

  private onResize = () => {
    const snapshot = this.lastAnchor ?? this.captureAnchor();
    if (this.resizeFrame) cancelAnimationFrame(this.resizeFrame);
    this.resizeFrame = requestAnimationFrame(() => {
      this.resizeFrame = 0;
      if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
      else if (snapshot) this.restoreAnchor(snapshot);
    });
  };

  private findScrollElement(): HTMLElement | null {
    const viewport = this.capture.current?.querySelector<HTMLElement>('.timeline-viewport');
    if (!viewport) return null;
    const candidates = [viewport, ...Array.from(viewport.querySelectorAll<HTMLElement>('*'))];
    return candidates.find((element) => {
      const overflow = getComputedStyle(element).overflowY;
      return overflow === 'auto' || overflow === 'scroll';
    }) ?? viewport;
  }

  private captureAnchor(): AnchorSnapshot | null {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return null;
    this.scrollElement = scroller;
    const container = scroller.getBoundingClientRect();
    const entries = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'));
    const index = entries.findIndex((entry) => entry.getBoundingClientRect().bottom > container.top + 8);
    if (index < 0) return { scrollTop: scroller.scrollTop };
    const anchor = entries[index];
    return {
      key: anchor.dataset.entryKey,
      offset: anchor.getBoundingClientRect().top - container.top,
      scrollTop: scroller.scrollTop,
      index,
      previousKey: index > 0 ? entries[index - 1].dataset.entryKey : undefined,
    };
  }

  private restoreAnchor(snapshot: AnchorSnapshot) {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    if (snapshot.key) {
      const anchor = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'))
        .find((entry) => entry.dataset.entryKey === snapshot.key);
      if (anchor && typeof snapshot.offset === 'number') {
        const nextOffset = anchor.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
        scroller.scrollTop += nextOffset - snapshot.offset;
        return;
      }
    }
    scroller.scrollTop = snapshot.scrollTop;
  }

  /// Restore the saved place against the entries rendered now. An entry that
  /// no longer exists falls back to its saved predecessor; only then does the
  /// absolute offset stand in, so history above the place never silently
  /// scrolls the reader.
  private restoreSavedAnchor(anchor: ReadingAnchor | undefined) {
    if (!anchor) return;
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    this.scrollElement = scroller;
    const entries = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'))
      .map((entry) => ({ element: entry, entry_key: entry.dataset.entryKey ?? '' }))
      .filter((entry) => entry.entry_key);
    const resolved = resolveAnchor(anchor, entries);
    if (resolved) {
      const target = entries[resolved.index].element;
      const nextOffset = target.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
      scroller.scrollTop += nextOffset - anchor.offset;
      return;
    }
    scroller.scrollTop = anchor.scroll_top;
  }

  private emitAnchor() {
    if (this.anchorFrame) return;
    this.anchorFrame = requestAnimationFrame(() => {
      this.anchorFrame = 0;
      const snapshot = this.captureAnchor();
      const anchor: ReadingAnchor | undefined = snapshot?.key
        ? {
          entry_key: snapshot.key,
          entry_index: snapshot.index ?? 0,
          ...(snapshot.previousKey ? { prev_entry_key: snapshot.previousKey } : {}),
          offset: snapshot.offset ?? 0,
          scroll_top: snapshot.scrollTop,
        }
        : undefined;
      this.props.emit('timeline', 'timeline.reading', {
        state: 'reading_history',
        at_end: false,
        ...(anchor ? { anchor } : {}),
      });
    });
  }

  private rememberAnchor() {
    this.lastAnchor = this.captureAnchor();
  }

  private entryElements(): HTMLElement[] {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return [];
    return Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'));
  }

  /// A selection is recorded only inside one entry and only from text-node
  /// endpoints; an element-level boundary has no content offset the view can
  /// replay honestly, so nothing is stored for it.
  private onSelectionChange = () => {
    clearTimeout(this.selectionTimer);
    this.selectionTimer = window.setTimeout(() => {
      this.selectionTimer = undefined;
      const selection = window.getSelection();
      if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
        this.props.emit('timeline', 'timeline.selection', { clear: true });
        return;
      }
      const range = selection.getRangeAt(0);
      if (range.startContainer.nodeType !== Node.TEXT_NODE
        || range.endContainer.nodeType !== Node.TEXT_NODE) return;
      const entries = this.entryElements();
      const startEntry = entries.find((entry) => entry.contains(range.startContainer));
      const endEntry = entries.find((entry) => entry.contains(range.endContainer));
      if (!startEntry || startEntry !== endEntry) return;
      // Selecting committed text is interaction, not "follow the tail": new
      // content must not scroll the reader away from the span they are
      // holding (docs/specs/web-interface.md#spec-21-11).
      this.props.emit('timeline', 'timeline.reading', { state: 'selecting_or_interacting' });
      const entryKey = startEntry.dataset.entryKey;
      if (!entryKey) return;
      const root = textRoot(startEntry);
      const nodes = textNodes(root);
      const start = textOffset(nodes, range.startContainer, range.startOffset);
      const end = textOffset(nodes, range.endContainer, range.endOffset);
      if (start === null || end === null || end <= start) return;
      this.props.emit('timeline', 'timeline.selection', {
        entry_key: entryKey,
        text: root.textContent ?? '',
        start,
        end,
      });
    }, 160);
  };

  /// Replay a stored selection only while the same characters still start the
  /// entry, and never over a live selection the reader is making right now.
  private restoreSavedSelection() {
    // Follow mode means the tail is the place: restoring a highlight there
    // would drag the viewport away from the newest committed text.
    if (this.props.node.props.reading === 'following_latest') return;
    const saved = this.props.node.props.selection as ReadingSelection | undefined;
    if (!saved || saved === this.restoredSelection) return;
    const selection = window.getSelection();
    if (!selection || !selection.isCollapsed) return;
    const entry = this.entryElements().find((row) => row.dataset.entryKey === saved.entry_key);
    if (!entry) return;
    const root = textRoot(entry);
    const text = root.textContent ?? '';
    if (!selectionApplies(saved, text)) return;
    const nodes = textNodes(root);
    const start = pointAt(nodes, saved.start);
    const end = pointAt(nodes, saved.end);
    if (!start || !end) return;
    const range = document.createRange();
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
    selection.removeAllRanges();
    selection.addRange(range);
    this.restoredSelection = saved;
  }

  private scrollToEnd() {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    this.scrollElement = scroller;
    scroller.scrollTop = scroller.scrollHeight;
  }

  private onScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (!this.scrollElement) this.scrollElement = this.findScrollElement();
    if (target !== this.scrollElement) return;
    this.rememberAnchor();
    const atEnd = target.scrollHeight - target.scrollTop - target.clientHeight <= 24;
    if (atEnd) {
      this.props.emit('timeline', 'timeline.reading', {
        state: 'following_latest',
        at_end: true,
      });
      return;
    }
    // Reading upward records a real place: the entry at the viewport top plus
    // its offset, so backfilled history above it does not move the reader.
    this.emitAnchor();
  };

  render() {
    const { node, projection, emit } = this.props;
    const entries = node.props.entries as UiNode[];
    const name = projection.identity?.display_name || undefined;
    return (
      <section className="timeline" hidden={node.props.hidden} aria-label={t('timeline.label')}>
        {/* One bounded announcement per committed round; the answer itself is
            not read again. */}
        <span className="sr-only" aria-live="polite">{this.state.announcement}</span>
        <div
          className="timeline-capture"
          ref={this.capture}
          data-reading={node.props.reading}
          data-wrap={node.props.wrap === true ? 'on' : 'off'}
          onScrollCapture={this.onScroll}
          onWheel={(event) => {
            if (event.deltaY < 0) this.emitAnchor();
          }}
          onPointerDown={(event) => {
            if ((event.target as Element).closest('pre,button,details,input,textarea,select,[role="button"]')) {
              emit('timeline', 'timeline.reading', { state: 'selecting_or_interacting' });
            }
          }}
        >
          <ScrollRoot className="timeline-root">
            <ScrollViewport className="timeline-viewport">
              <div className="timeline-column">
                <SubmissionHistory node={node} emit={emit} />
                {entries.length === 0
                  ? <EmptyTimeline connection={projection.connection} demo={projection.source === 'demo'} name={name} />
                  : <TimelineEntries entries={entries} emit={emit} name={name} onOpenWork={this.props.onOpenWork} />}
              </div>
            </ScrollViewport>
            <ScrollScrollbar orientation="vertical" className="timeline-scrollbar">
              <ScrollThumb className="timeline-thumb" />
            </ScrollScrollbar>
          </ScrollRoot>
          {node.props.reading !== 'following_latest' && (
            <div className="jump-latest">
              <Button variant="secondary" size="sm" onClick={() => emit('timeline', 'timeline.latest')}>
                <Icon name="down" size={15} />
                {node.props.new_count > 0 ? t('timeline.newContent', { count: node.props.new_count }) : t('timeline.latest')}
              </Button>
            </div>
          )}
        </div>
      </section>
    );
  }
}

/// The rendered text of one entry, excluding its chrome (meta line, receipt
/// footer): a selection replay must address the content the reader saw.
function textRoot(entry: HTMLElement): HTMLElement {
  return entry.querySelector<HTMLElement>('.entry-body') ?? entry;
}

function textNodes(root: HTMLElement): Text[] {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (node.nodeType === Node.TEXT_NODE) nodes.push(node as Text);
  }
  return nodes;
}

function textOffset(nodes: Text[], container: Node, offset: number): number | null {
  let total = 0;
  for (const node of nodes) {
    if (node === container) return total + offset;
    total += node.data.length;
  }
  return null;
}

function pointAt(nodes: Text[], offset: number): { node: Text; offset: number } | null {
  let remaining = offset;
  for (const node of nodes) {
    const length = node.data.length;
    if (remaining <= length) return { node, offset: remaining };
    remaining -= length;
  }
  const last = nodes[nodes.length - 1];
  return last ? { node: last, offset: last.data.length } : null;
}
