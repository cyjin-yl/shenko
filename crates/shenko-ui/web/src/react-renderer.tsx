import * as React from 'react';
import { createPortal } from 'react-dom';
import { RichDocument, type DocumentNodeView } from './document-view';
import { createRoot, type Root } from 'react-dom/client';
import { createReactAdapter } from '@proto.ui/adapter-react';
import { shadcnButton } from '@proto.ui/prototypes-shadcn/button';
import { shadcnTextareaRoot } from '@proto.ui/prototypes-shadcn/textarea';
import {
  shadcnScrollAreaRoot,
  shadcnScrollAreaViewport,
  shadcnScrollAreaScrollbar,
  shadcnScrollAreaThumb,
} from '@proto.ui/prototypes-shadcn/scroll-area';
import {
  shadcnCollapsibleRoot,
  shadcnCollapsibleTrigger,
  shadcnCollapsibleContent,
} from '@proto.ui/prototypes-shadcn/collapsible';
import type { OpenedWork, OwnerState, WorkView } from './projection';
import {
  intentFor,
  partState,
  type ActionReceiptView,
  type BuildManifest,
  type ConnectionState,
  type DegradedDocument,
  type DocumentErrorView,
  type Dispatch,
  type PlaygroundView,
  type ReadingAnchor,
  type ReadingSelection,
  type Submission,
  type TimelinePart,
  type UiNode,
  type UiProjection,
  type ViewName,
} from './projection';
import { ConfigEntries } from './config-panel';
import { resolveAnchor, selectionApplies } from './reading-state';
import {
  readContentRevisionDisclosure,
  readRevisionDisclosure,
  type ContentRevisionDisclosure,
  type RevisionDisclosure,
} from './revision-disclosure';
import { CompiledTransportBadge } from '../generated/prototypes/transport-badge/Component';

// These are the actual upstream prototypes and official runtime adapter, not
// local lookalikes. The Proto-UI compiler is not part of this graph: it runs at
// build time (web/compiler.mjs) and this bundle only imports the React DOM
// source it emitted for Shenko's own Prototypes.
const adapt = createReactAdapter({ ...React, createPortal });
const ProtoButton = adapt(shadcnButton);
const ProtoTextarea = adapt(shadcnTextareaRoot);
const ScrollRoot = adapt(shadcnScrollAreaRoot);
const ScrollViewport = adapt(shadcnScrollAreaViewport);
const ScrollScrollbar = adapt(shadcnScrollAreaScrollbar);
const ScrollThumb = adapt(shadcnScrollAreaThumb);
const CollapsibleRoot = adapt(shadcnCollapsibleRoot);
const CollapsibleTrigger = adapt(shadcnCollapsibleTrigger);
const CollapsibleContent = adapt(shadcnCollapsibleContent);

export interface ProjectionRenderer {
  render(projection: UiProjection, dispatch: Dispatch): void;
  patch(projection: UiProjection): void;
  dispose(): void;
}

export function createReactProjectionRenderer(host: HTMLElement): ProjectionRenderer {
  const root: Root = createRoot(host);
  let dispatch: Dispatch = () => undefined;
  const patch = (projection: UiProjection) => {
    root.render(<ProjectionView projection={projection} dispatch={dispatch} />);
  };
  return {
    render(projection, nextDispatch) {
      dispatch = nextDispatch;
      patch(projection);
    },
    patch,
    dispose() {
      root.unmount();
    },
  };
}

type Emit = (nodeId: string, actionId: string, payload?: Record<string, unknown>) => void;

function ProjectionView({ projection, dispatch }: { projection: UiProjection; dispatch: Dispatch }) {
  const nodes = Object.fromEntries(projection.nodes.map((item) => [item.kind, item]));
  const navigation = nodes.navigation as UiNode;
  const timeline = nodes.timeline as UiNode;
  const composer = nodes.composer as UiNode;
  const panel = nodes.context_panel as UiNode;
  const notice = nodes.status_notice as UiNode;
  const emit: Emit = React.useCallback((nodeId, actionId, payload = {}) => {
    dispatch(intentFor(projection, nodeId, actionId, payload));
  }, [projection, dispatch]);

  React.useEffect(() => {
    const theme = navigation.props.theme === 'light' ? 'light' : 'dark';
    document.documentElement.dataset.theme = theme;
    document.documentElement.classList.toggle('dark', theme === 'dark');
    document.documentElement.classList.toggle('light', theme === 'light');
  }, [navigation.props.theme]);

  const identityName = projection.identity?.display_name || '未命名主体';
  const mark = projection.identity?.display_name
    ? Array.from(projection.identity.display_name)[0]
    : '·';

  return (
    <div className="app-shell" data-source={projection.source}>
      <a className="skip-link" href="#composer-editor" onClick={(event) => {
        event.preventDefault();
        document.querySelector<HTMLTextAreaElement>('#composer-editor textarea')?.focus();
      }}>跳到输入区</a>
      <aside className="side-rail">
        <div className="brand" aria-label="Shenko">Shenko<span className="brand-mark" aria-hidden="true" /></div>
        <Navigation node={navigation} emit={emit} />
        <div className="rail-foot">
          <span className="rail-rule" />
          <ProtoButton
            styleProfile="rhea"
            variant="ghost"
            size="sm"
            className="theme-button"
            onClick={() => emit('navigation', 'theme.toggle')}
          >
            <Icon name={navigation.props.theme === 'light' ? 'moon' : 'sun'} />
            切换明暗
          </ProtoButton>
          <span className="rail-version">Proto-UI · {projection.assets.proto_ui.revision.slice(0, 8)}</span>
        </div>
      </aside>

      <main className="main-surface">
        <header className="topbar">
          <div className="mobile-brand" aria-label="Shenko">Shenko<span className="brand-mark" /></div>
          <div className="subject-heading">
            <span className="subject-mark" aria-hidden="true">{mark}</span>
            <div className="subject-heading-text">
              <div className="subject-name-row">
                <h1>{identityName}</h1>
                {projection.source === 'demo' && <span className="demo-badge">模拟投影</span>}
              </div>
              <span className="subject-subline">
                {projection.identity?.revision ? `identity r${projection.identity.revision}` : 'identity revision 未知'}
              </span>
            </div>
          </div>
          <div className="topbar-actions">
            <TransportBadge connection={projection.connection} demo={projection.source === 'demo'} />
            <ProtoButton
              styleProfile="rhea"
              variant="outline"
              size="sm"
              className="panel-toggle"
              onClick={() => emit('context', 'panel.toggle')}
            >
              <Icon name="panel" />详情
            </ProtoButton>
          </div>
        </header>

        <div className="status-strip">
          <span><span className="status-tick" />{timeline.props.step ? `已见第 ${timeline.props.step} 次提交` : '尚无已提交步骤'}</span>
          <span className="status-divider" />
          <span>{timeline.props.subject_status === 'unknown' || !timeline.props.subject_status ? '主体状态未知' : String(timeline.props.subject_status)}</span>
          <span className="status-spacer" />
          <span className="reading-label">{readingLabel(timeline.props.reading)}</span>
        </div>

        <div className="surface-stack">
          <TimelineSurface node={timeline} projection={projection} emit={emit} />
          <TasksSurface node={nodes.tasks_surface} />
          <MemorySurface node={nodes.memory_surface} />
          <ConfigSurface node={nodes.config_surface} projection={projection} emit={emit} />
          <PlaygroundSurface node={nodes.playground_surface} />
        </div>
        <Composer node={composer} emit={emit} />
      </main>

      <nav className="mobile-nav" aria-label="主要导航">
        <NavigationItems node={navigation} emit={emit} mobile />
      </nav>
      <ContextPanel node={panel} projection={projection} emit={emit} />
      {notice.props.text && <div className="notice" role="status">{notice.props.text}</div>}
      <span className="sr-only" aria-live="polite">{connectionAnnouncement(projection.connection)}</span>
    </div>
  );
}

function Navigation({ node, emit }: { node: UiNode; emit: Emit }) {
  return <nav className="rail-nav" aria-label="主要导航"><NavigationItems node={node} emit={emit} /></nav>;
}

const VIEW_LABELS: Record<ViewName, string> = {
  timeline: '时间线',
  tasks: '任务',
  memory: '记忆',
  config: '配置',
  playground: 'Playground',
};

function NavigationItems({ node, emit, mobile = false }: { node: UiNode; emit: Emit; mobile?: boolean }) {
  return (node.props.items as UiNode[]).map((item) => {
    const view = item.props.view as ViewName;
    return (
      <button
        type="button"
        key={`${mobile ? 'mobile' : 'rail'}-${view}`}
        className={`nav-item ${item.props.current ? 'is-current' : ''}`}
        aria-current={item.props.current ? 'page' : undefined}
        onClick={() => emit(item.node_id, item.action_refs[0], { view })}
      >
        <Icon name={view} />
        <span>{VIEW_LABELS[view]}</span>
      </button>
    );
  });
}

// The chip is Shenko's own Prototype, compiled at build time into React DOM
// source by the pinned Proto-UI compiler. This caller keeps the transport facts
// and decides only the label and tone; the Prototype renders markup and tokens.
function TransportBadge({ connection, demo }: { connection: ConnectionState; demo: boolean }) {
  if (demo) return <CompiledTransportBadge label="本地模拟" tone="local" />;
  const labels: Record<ConnectionState, string> = {
    connecting: '连接中',
    online: '传输已连接',
    reconnecting: '正在重连',
    offline: '传输离线',
  };
  return <CompiledTransportBadge label={labels[connection]} tone={connection} />;
}

/** Compiler facts exactly as the build recorded them; nothing is inferred. */
function compilerLabel(compiler: BuildManifest['compiler'] | undefined): string {
  if (!compiler) return 'Compiler：构建 manifest 未记录。';
  const runtime = compiler.runtime_imported === true ? '浏览器运行期导入编译器' : '浏览器未导入编译器';
  if (compiler.status !== 'build_time_compiled') {
    return `Compiler：${compiler.status}；${runtime}。`;
  }
  const names = (compiler.prototypes ?? []).map((prototype) => prototype.name);
  const profile = compiler.profile ?? 'profile 未记录';
  if (names.length === 0) return `Compiler：构建期编译，未记录已编译 prototype（${profile}）；${runtime}。`;
  return `Compiler：构建期编译 ${names.length} 个 Shenko prototype（${names.join('、')}，${profile}）；${runtime}。`;
}

function connectionAnnouncement(connection: ConnectionState): string {
  return connection === 'online' ? '传输已连接' : connection === 'reconnecting' ? '连接中断，正在重连' : '传输未连接';
}

function readingLabel(reading: string): string {
  if (reading === 'following_latest') return '跟随最新';
  if (reading === 'selecting_or_interacting') return '保持选择与交互';
  return '阅读历史';
}

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
}

class TimelineSurface extends React.Component<TimelineSurfaceProps, { announcement: string }> {
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
    if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
    else this.restoreSavedAnchor(this.props.node.props.anchor as ReadingAnchor | undefined);
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
      newest = { key: entry.node_id, label: `第 ${part.step} 轮已提交内容` };
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
    return (
      <section className="timeline-surface" hidden={node.props.hidden} aria-label="主体时间线">
        {/* One bounded announcement per committed round; the answer itself is
            not read again. */}
        <span className="sr-only" aria-live="polite">{this.state.announcement}</span>
        <div
          className="timeline-scroll-capture"
          ref={this.capture}
          data-reading={node.props.reading}
          data-wrap={node.props.wrap === true ? 'on' : 'off'}
          onScrollCapture={this.onScroll}
          onWheel={(event) => {
            if (event.deltaY < 0) this.emitAnchor();
          }}
          onPointerDown={(event) => {
            if ((event.target as Element).closest('pre,button,details,input,textarea')) {
              emit('timeline', 'timeline.reading', { state: 'selecting_or_interacting' });
            }
          }}
        >
          <ScrollRoot className="timeline-root">
            <ScrollViewport className="timeline-viewport">
              <div className="timeline-measure">
                {entries.length === 0
                  ? <EmptyTimeline connection={projection.connection} demo={projection.source === 'demo'} />
                  : <TimelineEntries entries={entries} emit={emit} />}
              </div>
            </ScrollViewport>
            <ScrollScrollbar orientation="vertical" className="timeline-scrollbar">
              <ScrollThumb className="timeline-thumb" />
            </ScrollScrollbar>
          </ScrollRoot>
          <div className="timeline-tools">
            <ProtoButton
              styleProfile="rhea"
              variant="ghost"
              size="sm"
              onClick={() => emit('timeline', 'timeline.code_wrap')}
            >
              {node.props.wrap === true ? '取消软换行' : '软换行长代码'}
            </ProtoButton>
          </div>
          {node.props.reading !== 'following_latest' && (
            <div className="jump-latest">
              <ProtoButton
                styleProfile="rhea"
                variant="secondary"
                size="sm"
                onClick={() => emit('timeline', 'timeline.latest')}
              >
                {node.props.new_count > 0 ? `${node.props.new_count} 条新内容` : '回到最新'}<Icon name="down" />
              </ProtoButton>
            </div>
          )}
        </div>
      </section>
    );
  }
}

function EmptyTimeline({ connection, demo }: { connection: ConnectionState; demo: boolean }) {
  return (
    <div className="empty-timeline" data-testid="empty-timeline">
      <div className="empty-orbit" aria-hidden="true"><span /><span /><i /></div>
      <h2>{demo ? '模拟时间线为空' : '等待第一条已提交内容'}</h2>
      <p>{demo ? '这里只展示本地模拟 projection。' : connection === 'online' ? '传输已连接；主体的下一步会在持久提交后出现。' : '草稿会保留，连接恢复后可继续追加。'}</p>
      <span className="empty-note">不显示未提交 token，不推断主体正在思考。</span>
    </div>
  );
}

function TimelineEntries({ entries, emit }: { entries: UiNode[]; emit: Emit }) {
  let lastDay = '';
  return <div className="timeline-entries">{entries.map((entry) => {
    // A document (or its failure) rides the day of the committed step that
    // carried it; it never opens a "时间未知" group of its own.
    if (entry.kind === 'semantic_document' || entry.kind === 'document_error') {
      return <EntryView node={entry} emit={emit} key={entry.node_id} />;
    }
    const value = entry.kind === 'subject_part'
      ? entry.props.part.observed_at
      : entry.kind === 'user_submission'
        ? entry.props.submission.created_at
        : null;
    const day = dayLabel(value);
    const separator = day !== lastDay ? <div className="day-separator" key={`day-${entry.node_id}`}><span>{day}</span></div> : null;
    lastDay = day;
    return <React.Fragment key={entry.node_id}>{separator}<EntryView node={entry} emit={emit} /></React.Fragment>;
  })}</div>;
}

function EntryView({ node, emit }: { node: UiNode; emit: Emit }) {
  return node.kind === 'subject_part'
    ? <SubjectPart node={node} emit={emit} />
    : node.kind === 'semantic_document'
      ? <SemanticDocumentView node={node} emit={emit} />
      : node.kind === 'document_error'
        ? <DocumentErrorEntry node={node} />
      : node.kind === 'work'
        ? <WorkEntry node={node} emit={emit} />
      : <UserSubmission node={node} emit={emit} />;
}

/// One work of the subject, in the timeline (docs/specs/configuration-playground.md#spec-57-6).
///
/// It names the work's own revision: what the reader does next is bound to
/// the revision they saw. Opening the work is the reader's own action.
function WorkEntry({ node, emit }: { node: UiNode; emit: Emit }) {
  const work = node.props.work as WorkView;
  return (
    <article className="timeline-entry work-entry" data-entry-key={node.node_id} data-artifact-id={work.artifact_id}>
      <div className="entry-gutter"><span className="step-label">作品</span></div>
      <div className="entry-content">
        <div className="work-summary">
          <strong>{String(work.title ?? work.artifact_id)}</strong>
          <span>r{String(work.revision ?? '—')}</span>
          <span>{String(work.lifecycle ?? 'draft')}</span>
          <span>{String(work.mime ?? '—')}</span>
        </div>
        <ProtoButton
          styleProfile="rhea"
          variant="outline"
          size="sm"
          onClick={() => emit(node.node_id, node.action_refs[0])}
        >打开作品</ProtoButton>
      </div>
    </article>
  );
}

// The rich layer is gone; the committed text beside it stays the record and
// the reason is readable instead of only flashing as a notice (docs/research/product-positioning.md#spec-3).
function DocumentErrorEntry({ node }: { node: UiNode }) {
  const error = node.props.error as DocumentErrorView;
  return (
    <article
      className="timeline-entry document-error-entry"
      data-entry-key={node.node_id}
      data-source-step={error.source_step_id}
      role="note"
    >
      <div className="entry-gutter"><span className="step-label">界面降级</span></div>
      <div className="entry-content">
        <p className="document-error-reason">这段已提交正文里的界面块未能呈现，富层已丢弃；正文仍按原文显示。</p>
        <p className="document-error-detail">{error.reason}</p>
        {error.source_commit && (
          <span className="document-error-commit">commit {error.source_commit}</span>
        )}
      </div>
    </article>
  );
}

// A degraded semantic document: the mode decides how much structure renders,
// but the numbered options are always the same action identities (docs/specs/web-interface.md#spec-21-8).
// Execution authority comes from the subject's disposition of the admitted
// input; this view only submits that input.
const ACTION_STAGE_LABELS: Record<ActionReceiptView['stage'], string> = {
  sending: '动作提交中',
  accepted: '已持久接纳 · 处置待主体',
  selected: '已选入封口输入计划',
  committed: '主体已提交处置',
  rejected: '未接纳',
  unavailable: '回执暂不可用',
  conflict: '动作键冲突',
};

function SemanticDocumentView({ node, emit }: { node: UiNode; emit: Emit }) {
  const document = node.props.document as DegradedDocument;
  const receipts = (node.props.receipts ?? []) as ActionReceiptView[];
  const latest = (actionId: string): ActionReceiptView | undefined => receipts
    .filter((row) => row.action_id === actionId)
    .at(-1);
  // An admitted decision awaiting disposition must not be admitted twice by a
  // second click; the option stays visible with its durable stage shown.
  const admitted = (row: ActionReceiptView | undefined): boolean =>
    row !== undefined && ['sending', 'accepted', 'selected'].includes(row.stage);
  return (
    <article className="timeline-entry document-entry" data-entry-key={node.node_id} data-document-id={document.document_id}>
      <div className="entry-gutter">
        <span className="step-label">界面</span>
        <span className="document-mode" data-mode={document.mode}>
          {document.mode === 'rich' ? '完整' : document.mode === 'card' ? '卡片' : '文本'}
        </span>
      </div>
      <div className="entry-content">
        <div className={`semantic-document is-${document.mode}`}>
          <p className="document-summary">{document.summary}</p>
          {document.mode === 'rich' && Array.isArray(document.nodes) && document.nodes.length > 0 && (
            <RichDocument
              nodes={document.nodes as DocumentNodeView[]}
              options={document.options}
              actionRefs={node.action_refs ?? []}
              onAction={(actionRef) => emit(node.node_id, actionRef)}
              renderMarkdown={(markdown) => <Markdown text={markdown} />}
            />
          )}
          {document.options.length > 0 && (
            <ol className="document-options">
              {document.options.map((option, index) => {
                const receipt = latest(option.action_id);
                const pending = admitted(receipt);
                return (
                  <li key={`${option.action_id}:${option.nonce}`} data-action-id={option.action_id}>
                    <ProtoButton
                      styleProfile="rhea"
                      variant="outline"
                      size="sm"
                      disabled={pending}
                      onClick={() => emit(node.node_id, node.action_refs[index])}
                    >{index + 1}. {option.label}</ProtoButton>
                    {receipt && (
                      <span className={`action-receipt is-${receipt.stage}`} data-stage={receipt.stage}>
                        {ACTION_STAGE_LABELS[receipt.stage]}
                        {receipt.detail ? ` · ${receipt.detail}` : ''}
                      </span>
                    )}
                  </li>
                );
              })}
            </ol>
          )}
        </div>
      </div>
    </article>
  );
}

function SubjectPart({ node, emit }: { node: UiNode; emit: Emit }) {
  const part = node.props.part as TimelinePart;
  const state = partState(part);
  // Only committed text carries a step number. A durable streamed prefix is
  // real ledger content, but labelling it "第 N 轮" would claim an S1 step the
  // part does not have yet (docs/specs/web-interface.md#spec-21-11).
  const gutterLabel = state.committed
    ? `第 ${part.step} 轮`
    : part.terminal
      ? '流式 · 已完成'
      : '流式接收中';
  const footerLabel = state.committed ? '已提交' : '未提交 · 持久流式';
  const announcement = state.committed
    ? `第 ${part.step} 轮已提交内容`
    : '主体正在输出、尚未提交的持久流式内容';
  return (
    <article
      className="timeline-entry subject-entry"
      data-entry-key={node.node_id}
      data-message-id={part.message_id}
      data-part-id={part.part_id}
      data-part-revision={part.part_revision}
      data-next-chunk-seq={part.next_chunk_seq}
      data-commit-ref={part.commit_ref}
      data-committed={state.committed ? 'true' : 'false'}
      data-streaming={state.streaming ? 'true' : 'false'}
      aria-label={announcement}
      aria-busy={state.streaming && !part.terminal ? true : undefined}
    >
      <div className="entry-gutter">
        <span className="step-label">{gutterLabel}</span>
        <time dateTime={part.observed_at} title="UI 接收时间，不是持久 commit 墙钟时间">{formatTime(part.observed_at)}</time>
      </div>
      <div className="entry-content">
        <PartErrorBoundary text={part.content}><Markdown text={part.content} /></PartErrorBoundary>
        <footer className="entry-footer">
          <span className="commit-label"><Icon name="commit" />{footerLabel}</span>
          {part.gap && <span className="gap-label">序号空洞 · 等待补读</span>}
          <details className="identity-details">
            <summary>part identity</summary>
            <dl>
              <div><dt>message</dt><dd>{part.message_id}</dd></div>
              <div><dt>part</dt><dd>{part.part_id}</dd></div>
              <div><dt>revision</dt><dd>{part.part_revision}</dd></div>
              <div><dt>commit</dt><dd>{part.commit_ref}</dd></div>
              <div><dt>request / attempt</dt><dd>{part.request_id ?? '未知'} / {part.attempt_id ?? '未知'}</dd></div>
            </dl>
          </details>
          <span className="entry-action">
            <ProtoButton
              styleProfile="rhea"
              variant="ghost"
              size="sm"
              onClick={() => emit(node.node_id, node.action_refs[0])}
            ><Icon name="copy" />复制</ProtoButton>
          </span>
        </footer>
      </div>
    </article>
  );
}

class PartErrorBoundary extends React.Component<{ text: string; children: React.ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) {
      return <div className="part-fallback" role="note">
        <span>此 part 已降级为原始文本；其余界面仍可用。</span>
        <pre>{this.props.text}</pre>
        <button type="button" onClick={() => this.setState({ failed: false })}>重新呈现</button>
      </div>;
    }
    return this.props.children;
  }
}

interface MarkdownBlock { kind: string; key: string; text: string; language?: string; items?: string[] }

function parseMarkdown(text: string): MarkdownBlock[] {
  const lines = text.replaceAll('\r\n', '\n').split('\n');
  const blocks: MarkdownBlock[] = [];
  let index = 0;
  let sequence = 0;
  const nextKey = (kind: string) => `${kind}-${sequence++}`;
  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) { index += 1; continue; }
    if (line.trimStart().startsWith('```')) {
      const language = line.trimStart().slice(3).trim();
      const key = nextKey('code');
      index += 1;
      const code: string[] = [];
      while (index < lines.length && !lines[index].trimStart().startsWith('```')) {
        code.push(lines[index]);
        index += 1;
      }
      if (index < lines.length) index += 1;
      blocks.push({ kind: 'code', key, text: code.join('\n'), language });
      continue;
    }
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      blocks.push({ kind: `h${heading[1].length}`, key: nextKey('heading'), text: heading[2] });
      index += 1;
      continue;
    }
    if (/^[-*]\s+/.test(line)) {
      const items: string[] = [];
      const key = nextKey('list');
      while (index < lines.length && /^[-*]\s+/.test(lines[index])) {
        items.push(lines[index].replace(/^[-*]\s+/, ''));
        index += 1;
      }
      blocks.push({ kind: 'list', key, text: '', items });
      continue;
    }
    if (line.startsWith('>')) {
      const key = nextKey('quote');
      const quote: string[] = [];
      while (index < lines.length && lines[index].startsWith('>')) {
        quote.push(lines[index].replace(/^>\s?/, ''));
        index += 1;
      }
      blocks.push({ kind: 'quote', key, text: quote.join('\n') });
      continue;
    }
    const key = nextKey('paragraph');
    const paragraph: string[] = [];
    while (index < lines.length && lines[index].trim() && !lines[index].trimStart().startsWith('```') && !/^(#{1,4})\s+/.test(lines[index]) && !/^[-*]\s+/.test(lines[index]) && !lines[index].startsWith('>')) {
      paragraph.push(lines[index]);
      index += 1;
    }
    blocks.push({ kind: 'paragraph', key, text: paragraph.join('\n') });
  }
  return blocks;
}

function inlineText(text: string): React.ReactNode[] {
  return text.split(/(`[^`]*`)/g).filter(Boolean).map((part, index) =>
    part.startsWith('`') && part.endsWith('`') && part.length > 1
      ? <code className="inline-code" key={index}>{part.slice(1, -1)}</code>
      : <React.Fragment key={index}>{part}</React.Fragment>,
  );
}

function Markdown({ text }: { text: string }) {
  const blocks = React.useMemo(() => parseMarkdown(text), [text]);
  return <div className="markdown-body">{blocks.map((block) => {
    if (block.kind === 'code') {
      return <figure className="code-block" key={block.key}>
        <figcaption>{block.language || 'text'}<span>已提交代码</span></figcaption>
        <pre tabIndex={0}><code>{block.text}</code></pre>
      </figure>;
    }
    if (block.kind === 'list') return <ul key={block.key}>{block.items?.map((item, index) => <li key={index}>{inlineText(item)}</li>)}</ul>;
    if (block.kind === 'quote') return <blockquote key={block.key}>{inlineText(block.text)}</blockquote>;
    if (block.kind.startsWith('h')) {
      const level = Math.min(4, Number(block.kind.slice(1))) as 1 | 2 | 3 | 4;
      return React.createElement(`h${level + 1}`, { key: block.key }, inlineText(block.text));
    }
    return <p key={block.key}>{inlineText(block.text)}</p>;
  })}</div>;
}

function UserSubmission({ node, emit }: { node: UiNode; emit: Emit }) {
  const submission = node.props.submission as Submission;
  return (
    <article className="timeline-entry user-entry" data-entry-key={node.node_id} data-submission-id={submission.payload.submission_id}>
      <div className="entry-gutter"><span className="step-label">你</span><time dateTime={submission.created_at}>{formatTime(submission.created_at)}</time></div>
      <div className="entry-content">
        <div className="user-text">{submission.payload.content}</div>
        <ReceiptView submission={submission} emit={emit} node={node} />
      </div>
    </article>
  );
}

function ReceiptView({ submission, emit, node }: { submission: Submission; emit: Emit; node: UiNode }) {
  const placement = submission.placement;
  const placementText = placement
    ? placement.actual === 'next_boundary'
      ? '处置位置：下一边界'
      : placement.actual === 'live_steer'
        ? '处置位置：进行中请求内'
        : placement.actual === 'cancel_reseal'
          ? '处置位置：取消重封'
          : '处置位置：待定'
    : null;
  const labels: Record<string, string> = {
    sending: '发送中',
    sent: '已写入传输 · 等待服务端回执',
    accepted: '已持久接纳 · 等待选入/处置回执',
    selected: '已选入封口输入计划 · 等待处置回执',
    committed: '主体已提交处置',
    conflict: '提交 ID 冲突 · 原输入未改写',
    rejected: '未接收',
    unknown: '传输回执未知 · 可用同一 ID 查询或重试',
  };
  const withdrawal = submission.withdrawal;
  const withdrawAction = node.props.withdraw_action as string | undefined;
  return <div className={`receipt is-${submission.receipt}`} data-receipt={submission.receipt}>
    <span className="receipt-mark" aria-hidden="true" />
    <span>
      {labels[submission.receipt]}{submission.detail ? ` · ${submission.detail}` : ''}
      {placementText ? ` · ${placementText}` : ''}
    </span>
    {withdrawal && (
      <span className="withdrawal-mark">
        {withdrawal.result === 'withdrawal_queued' ? '撤回已排队 · 等待 S-折入' : '呈现前已撤回'}
      </span>
    )}
    {(submission.receipt === 'rejected' || submission.receipt === 'unknown')
      && typeof node.props.retry_action === 'string' && (
      <ProtoButton styleProfile="rhea" variant="outline" size="sm" onClick={() => emit(node.node_id, node.props.retry_action)}>以同一 ID 重试</ProtoButton>
    )}
    {!withdrawal && typeof withdrawAction === 'string' && (
      <ProtoButton styleProfile="rhea" variant="outline" size="sm" onClick={() => emit(node.node_id, withdrawAction)}>撤回</ProtoButton>
    )}
  </div>;
}

function Composer({ node, emit }: { node: UiNode; emit: Emit }) {
  const draftAction = node.action_refs[0];
  const appendAction = node.action_refs[1];
  const steerAction = node.props.steer_action as string | undefined;
  const append = node.props.append_action as string;
  const canSend = Boolean(node.props.append_enabled) && appendAction === append;
  const canSteer = Boolean(node.props.steer_enabled) && steerAction === node.action_refs[2];
  return (
    <footer className="composer-dock" hidden={node.props.hidden}>
      <div className="composer-shell">
        <div className="composer-topline">
          <span className="composer-mode">Append<span className="mode-dot" /></span>
          <span className="composer-capability">
            {node.props.streaming === true ? '流式传输已接入' : '仅已提交内容'}
            {canSteer ? ' · Steer 可用（下一边界处置）' : ' · Steer 未提供'}
          </span>
        </div>
        <div
          className="composer-editor"
          id="composer-editor"
          onKeyDownCapture={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              if (canSend) emit('composer', appendAction);
            } else if (event.key === 'Escape') {
              event.stopPropagation();
              if (node.props.draft) emit('composer', draftAction, { value: '' });
              else (event.target as HTMLElement).blur();
            }
          }}
        >
          <ProtoTextarea
            ariaLabel={node.props.demo ? '模拟投影输入区，不发送' : '向同一主体追加输入'}
            value={node.props.draft}
            rows={3}
            readOnly={node.props.demo}
            placeholder={node.props.demo ? '模拟投影不发送输入' : '给同一主体追加一条消息…'}
            className="composer-textarea"
            onValueChange={(detail) => emit('composer', draftAction, { value: detail.value })}
          />
        </div>
        <div className="composer-actions">
          <div className="composer-truth">
            <span>{node.props.demo ? '本地模拟 · live 写入未启动' : node.props.connection !== 'online' ? '离线时保留草稿' : !node.props.subject_bound ? '等待真实 subject 绑定' : '草稿仅保存在此浏览器'}</span>
            <span className="composer-hint">Enter 追加 · Shift+Enter 换行{canSteer ? ' · Steer 在下一边界处置' : ''}</span>
          </div>
          {canSteer && (
            <ProtoButton
              styleProfile="rhea"
              variant="outline"
              size="default"
              disabled={!canSteer}
              onClick={() => emit('composer', steerAction)}
            >Steer<Icon name="arrow" /></ProtoButton>
          )}
          <ProtoButton
            styleProfile="rhea"
            variant="default"
            size="default"
            className="send-button"
            disabled={!canSend}
            onClick={() => emit('composer', appendAction)}
          >追加<Icon name="arrow" /></ProtoButton>
        </div>
      </div>
    </footer>
  );
}

function SurfaceHeader({ eyebrow, title, detail }: { eyebrow: string; title: string; detail: string }) {
  return <header className="surface-header"><span className="eyebrow">{eyebrow}</span><h2>{title}</h2><p>{detail}</p></header>;
}

function TasksSurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const data = node.props.data;
  const items = Array.isArray(data?.items) ? data.items : [];
  return <section className="detail-surface" hidden={node.props.hidden}>
    <SurfaceHeader eyebrow="按需视图" title="任务" detail="来自当前节点只读任务接口；空结果不代替未知。" />
    {node.props.error ? <SurfaceNote text="任务接口不可用" tone="error" /> : data?.available === false ? <SurfaceNote text={data?.note || '任务源尚未接入'} /> : items.length === 0 ? <SurfaceNote text="暂无任务" /> : (
      <div className="data-list">{items.map((task: Record<string, unknown>, index: number) => <div className="data-row task-row" key={String(task.task_id ?? index)}>
        <code>{String(task.task_id ?? '—')}</code><span>{String(task.kind ?? '—')}</span><strong>{String(task.state ?? '—')}</strong><time>{formatStamp(task.created_at)}</time>
      </div>)}</div>
    )}
  </section>;
}

function MemorySurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const items = Array.isArray(node.props.data?.items) ? node.props.data.items : [];
  return <section className="detail-surface" hidden={node.props.hidden}>
    <SurfaceHeader eyebrow="按需视图" title="记忆" detail="只呈现接口实际返回的条目，不从未持久 profile 生成回忆。" />
    {node.props.error ? <SurfaceNote text="记忆接口不可用" tone="error" /> : items.length === 0 ? <SurfaceNote text={node.props.data?.note || '暂无记忆条目'} /> : (
      <div className="data-list">{items.map((memory: Record<string, unknown>, index: number) => <div className="data-row memory-row" key={String(memory.id ?? index)}>
        {/* `kind` is a learnable hint, never the entry's content. */}
        <span>{String(memory.content ?? memory.text ?? memory.summary ?? memory.title ?? '—')}{memory.kind ? <small className="memory-kind">{String(memory.kind)}</small> : null}</span><time>{formatStamp(memory.created_at)}</time>
        <RevisionDisclosureView disclosure={readRevisionDisclosure(memory.disclosure)} />
        <ContentRevisionDisclosureView revision={readContentRevisionDisclosure(memory.revision_disclosure)} />
      </div>)}</div>
    )}
  </section>;
}

/**
 * Renders the server's taste disclosure verbatim: retained-version count and
 * latest modification by default, full text only when the request asked for a
 * specific revision. A missing projection stays missing (docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-4).
 */
function RevisionDisclosureView({ disclosure }: { disclosure: RevisionDisclosure | null }) {
  if (!disclosure) return null;
  const facts: string[] = [];
  if (disclosure.older_versions !== null) facts.push(`旧版本 ${disclosure.older_versions} 版`);
  if (disclosure.latest_modified_at_ms !== null) facts.push(`最新修改 ${formatStampMs(disclosure.latest_modified_at_ms)}`);
  if (disclosure.selected !== null) facts.push(`当前读第 ${disclosure.selected} 版`);
  if (disclosure.written_at_ms !== null) facts.push(`写于 ${formatStampMs(disclosure.written_at_ms)}`);
  if (facts.length === 0 && !disclosure.full_text) return null;
  return <div className="revision-disclosure" data-revision-family={disclosure.family}>
    {facts.length ? <span className="revision-summary">{facts.join(' · ')}</span> : null}
    {disclosure.content_digest ? <code className="revision-digest">{disclosure.content_digest.slice(0, 12)}</code> : null}
    {disclosure.full_text ? <div className="revision-full">
      <h4>第 {disclosure.full_text.revision} 版全文{disclosure.full_text.written_at_ms !== null ? ` · 写于 ${formatStampMs(disclosure.full_text.written_at_ms)}` : ''}</h4>
      {disclosure.full_text.text !== null
        ? <p>{disclosure.full_text.text}</p>
        : <small className="revision-note">请求的版本全文未解析</small>}
    </div> : null}
    {disclosure.full_text_unreadable ? <small className="revision-note">请求的版本全文不可读</small> : null}
  </div>;
}

/** Store-level revision summary exactly as projected; no derived chain. */
function ContentRevisionDisclosureView({ revision }: { revision: ContentRevisionDisclosure | null }) {
  if (!revision) return null;
  return <div className="revision-disclosure">
    <span className="revision-summary">旧版本 {revision.old_version_count} 版 · 最新修改 {formatStampMs(revision.latest_modified_at_ms)}</span>
    <code className="revision-digest">{revision.latest_digest.slice(0, 12)}</code>
  </div>;
}

function ConfigSurface({ node, projection, emit }: { node?: UiNode; projection: UiProjection; emit: Emit }) {
  if (!node) return null;
  const config = node.props.config;
  const self = node.props.self;
  const manifest = node.props.manifest;
  const unknown = (value: unknown) => value === undefined || value === null || value === '' || value === 'unknown' ? '未知' : String(value);
  return <section className="detail-surface config-surface" hidden={node.props.hidden}>
    <SurfaceHeader eyebrow="运行事实" title="配置与能力" detail="值来自当前服务接口或本地构建 manifest；unknown 不被改写成在线或已启用。" />
    <div className="config-grid">
      <ConfigItem label="模型绑定" value={node.props.config_error ? '接口不可用' : unknown(config?.model)} />
      <ConfigItem label="主体状态" value={unknown(self?.status)} />
      <ConfigItem label="传输" value={connectionAnnouncement(node.props.connection)} />
      <ConfigItem label="最近已见提交" value={node.props.step ? `第 ${node.props.step} 次` : '尚无'} />
      <ConfigItem label="Runtime generation" value={unknown(config?.generation)} />
      <ConfigItem label="API key" value={typeof config?.api_key_configured === 'boolean' ? config.api_key_configured ? '已配置（未读取值）' : '未配置' : '未知'} />
      <ConfigItem label="数据目录" value={unknown(config?.data_dir)} wide />
      <ConfigItem label="WebSocket" value={wsUrl()} wide />
      <ConfigItem label="服务版本" value={unknown(node.props.server_version)} />
      <ConfigItem label="Subject ID" value={projection.subject_id ?? '未知'} wide />
    </div>
    <ConfigEntries node={node} emit={emit} />
    <div className="capability-block">
      <h3>输入与时间线能力</h3>
      <div className="capability-row"><span>Append</span><strong>{projection.capabilities.append ? projection.capabilities.durable_input_receipts ? '可用 · 持久回执' : '可用 · 传输回执' : '不可用'}</strong></div>
      <div className="capability-row"><span>Steer</span><strong>{projection.capabilities.steer ? '可用 · 下一边界处置' : '未提供 · 不显示伪动作'}</strong></div>
      <div className="capability-row"><span>Durable accepted / selected</span><strong>{projection.capabilities.durable_input_receipts ? '已接入 · 由 Store 回执' : '不可用'}</strong></div>
      <div className="capability-row"><span>Streaming</span><strong>{projection.capabilities.streaming ? '生产者已接线 · 仅已提交 chunk' : '无生产者 · 仅已提交内容'}</strong></div>
      <div className="capability-row"><span>Committed replay</span><strong>{projection.capabilities.committed_replay ? '已接入' : '未知'}</strong></div>
    </div>
    <div className="build-block">
      <div><span className="eyebrow">真实前端构建</span><h3>Proto-UI + React adapter</h3></div>
      {manifest ? <>
        <code className="commit-sha">{manifest.upstream.commit}</code>
        <p>{manifest.packages.join(' · ')}</p>
        <p>采用 family：{manifest.adopted_families.join('、')}。{compilerLabel(manifest.compiler)}</p>
      </> : <p>构建 manifest 未读取。</p>}
      <div className="license-links"><a href="/assets/licenses/PROTO-UI-LICENSE" target="_blank" rel="noreferrer">Proto-UI 许可</a><a href="/assets/licenses/SHADCN-NOTICES.md" target="_blank" rel="noreferrer">shadcn 来源与许可</a><a href="/assets/licenses/REACT-LICENSE" target="_blank" rel="noreferrer">React 许可</a></div>
    </div>
  </section>;
}

function ConfigItem({ label, value, wide = false }: { label: string; value: string; wide?: boolean }) {
  return <div className={`config-item ${wide ? 'is-wide' : ''}`}><span>{label}</span><strong title={value}>{value}</strong></div>;
}

function PlaygroundSurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const playground: PlaygroundView | undefined = node.props.playground;
  const roots = Array.isArray(playground?.roots) ? playground.roots : [];
  const artifacts = Array.isArray(playground?.artifacts) ? playground.artifacts : [];
  const namespaces = Array.isArray(playground?.namespaces) ? playground.namespaces : [];
  return <section className="detail-surface playground-surface" hidden={node.props.hidden}>
    <SurfaceHeader eyebrow="全栈工件" title="Playground" detail="工件、可选根与配置命名空间全部来自持久记录；不可用即显示不可用，不用占位数据。" />
    {node.props.error ? <SurfaceNote text="Playground 接口不可用" tone="error" /> : playground?.available === false ? <SurfaceNote text="Playground 需要持久 Store 与主体" /> : <>
      <div className="capability-block">
        <h3>可选根（enumerable roots）</h3>
        {roots.length === 0 ? <SurfaceNote text="尚无已选根" /> : (
          <div className="data-list">{roots.map((root) => <div className="data-row playground-row" key={String(root.artifact_id)}>
            <code>{String(root.artifact_id)}</code><span>{String(root.root)}</span><code>{String(root.identity).slice(0, 16)}…</code>
          </div>)}</div>
        )}
      </div>
      <div className="capability-block">
        <h3>工件</h3>
        {artifacts.length === 0 ? <SurfaceNote text="尚无工件" /> : (
          <div className="data-list">{artifacts.map((artifact) => <div className="data-row playground-row" key={String(artifact.artifact_id)}>
            <strong>{String(artifact.title ?? artifact.artifact_id)}</strong>
            <span>r{String(artifact.revision ?? '—')}</span>
            <span>{String(artifact.mime ?? '—')}</span>
            <span>{artifact.backend ? 'backend' : 'content'}</span>
            <span>{String(artifact.lifecycle ?? 'draft')}</span>
            <time>{formatStamp(artifact.updated_at)}</time>
          </div>)}</div>
        )}
      </div>
      <div className="capability-block">
        <h3>配置命名空间</h3>
        {namespaces.length === 0 ? <SurfaceNote text="尚未注册配置命名空间" /> : (
          <div className="data-list">{namespaces.map((entry) => <div className="data-row playground-row" key={String(entry.namespace)}>
            <code>{String(entry.namespace)}</code><span>{String(entry.description ?? '—')}</span>
          </div>)}</div>
        )}
      </div>
    </>}
  </section>;
}

function SurfaceNote({ text, tone = 'neutral' }: { text: string; tone?: 'neutral' | 'error' }) {
  return <div className={`surface-note is-${tone}`}><span className="note-mark" /><p>{text}</p></div>;
}

function ContextPanel({ node, projection, emit }: { node: UiNode; projection: UiProjection; emit: Emit }) {
  const open = Boolean(node.props.open);
  const aside = React.createRef<HTMLElement>();
  const emitRef = React.useRef(emit);
  emitRef.current = emit;
  React.useEffect(() => {
    if (!open) return;
    const previous = document.activeElement as HTMLElement | null;
    const panel = aside.current;
    panel?.querySelector<HTMLElement>('.panel-close-button button')?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        emitRef.current('context', 'panel.toggle');
        return;
      }
      if (event.key !== 'Tab' || !panel || !window.matchMedia('(max-width: 719px)').matches) return;
      const focusable = Array.from(panel.querySelectorAll<HTMLElement>('button,[href],input,textarea,select,[tabindex]:not([tabindex="-1"])')).filter((element) => element.offsetParent !== null);
      if (!focusable.length) return;
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      previous?.focus();
    };
  }, [open]);
  const identityName = projection.identity?.display_name || '未命名主体';
  return <>
    <div className="panel-scrim" hidden={!open} onPointerDown={() => emit('context', 'panel.toggle')} />
    <aside className="context-panel" ref={aside} hidden={!open} aria-label="主体详情" role="dialog" aria-modal={false}>
      <header className="panel-header">
        <div><span className="eyebrow">主体详情</span><h2>{identityName}</h2><p>{projection.identity?.revision ? `identity revision ${projection.identity.revision}` : 'identity revision 未知'}</p></div>
        <span className="panel-close-button"><ProtoButton styleProfile="rhea" variant="ghost" size="icon" onClick={() => emit('context', 'panel.toggle')}><Icon name="close" /><span className="sr-only">关闭详情</span></ProtoButton></span>
      </header>
      <div className="panel-sections">
        {(node.props.sections as UiNode[]).map((section) => <PanelSection key={section.node_id} section={section} node={node} emit={emit} />)}
      </div>
      <OpenedWorkPanel node={node} emit={emit} />
      <IdentityPanel node={node} emit={emit} />
      <footer className="panel-footer">框架 Shenko · 实例身份独立</footer>
    </aside>
  </>;
}

const SECTION_LABELS: Record<string, string> = { status: '当前状态', activity: '近期提交', artifacts: '产物', devices: '设备' };

/// The work the reader opened, with the actions bound to its revision
/// (docs/specs/configuration-playground.md#spec-57-6). Every action names the revision the reader saw: a work
/// that moved on refuses the stale action instead of quietly applying to
/// whatever is there now.
/// The instance's own name and who owns it (docs/specs/web-interface.md#spec-21-1, docs/specs/plugins.md#spec-44-4).
///
/// The name is the instance's, not the brand's, and the reader may give it one
/// — or let the subject name itself. Ownership is reported as the fact it is:
/// claimed, not claimed, or a claim still pending.
function IdentityPanel({ node, emit }: { node: UiNode; emit: Emit }) {
  const owner = node.props.owner_state as OwnerState | undefined;
  const nameDraft = (node.props.name_draft ?? '') as string;
  const claimDraft = (node.props.claim_draft ?? '') as string;
  const nameRef = node.props.identity_name_ref as string | undefined;
  const claimRef = node.props.owner_claim_ref as string | undefined;
  return <section className="identity-panel">
    <h3 className="identity-panel-title">这个实例</h3>
    <div className="identity-panel-row">
      <label className="identity-name-field">
        <span>给它取个名字</span>
        <input
          value={nameDraft}
          onChange={(event) => emit(node.node_id, node.props.identity_name_draft_ref ?? '', { value: event.target.value })}
          placeholder="留空则让主体自己取名"
        />
      </label>
      <ProtoButton
        styleProfile="rhea"
        variant="outline"
        size="sm"
        onClick={() => nameRef && emit(node.node_id, nameRef)}
      >取名</ProtoButton>
    </div>
    {owner?.available === false ? (
      <SurfaceNote text="这台安装还没有装上归属机制" />
    ) : (
      <div className="identity-panel-row">
        <span className="identity-owner-state">
          {owner?.claimed === true ? '这台安装已经归你' : owner?.claim_pending === true ? '认领在等你完成' : '这台安装还没有被认领'}
        </span>
        {!owner?.claimed && (
          <>
            <label className="identity-claim-field">
              <span>认领凭据</span>
              <input
                value={claimDraft}
                onChange={(event) => emit(node.node_id, node.props.owner_claim_draft_ref ?? '', { value: event.target.value })}
                placeholder="这台安装给出的认领凭据"
              />
            </label>
            <ProtoButton
              styleProfile="rhea"
              variant="ghost"
              size="sm"
              onClick={() => claimRef && emit(node.node_id, claimRef)}
            >认领</ProtoButton>
          </>
        )}
      </div>
    )}
  </section>;
}

function OpenedWorkPanel({ node, emit }: { node: UiNode; emit: Emit }) {
  const work = node.props.work as OpenedWork | undefined;
  const refs = (node.props.work_action_refs ?? []) as string[];
  if (node.props.work_error === true) {
    return <section className="opened-work"><SurfaceNote text="这件作品现在读不到" tone="error" /></section>;
  }
  if (!work) return null;
  return <section className="opened-work" data-artifact-id={work.artifact_id}>
    <h3 className="opened-work-title">{String(work.title ?? work.artifact_id)}</h3>
    <p className="opened-work-facts">
      <span>r{String(work.revision ?? '—')}</span>
      <span>{String(work.lifecycle ?? 'draft')}</span>
      <span>{String(work.mime ?? '—')}</span>
    </p>
    {work.content !== undefined && (
      <pre className="opened-work-content" tabIndex={0}><code>{work.content}</code></pre>
    )}
    <div className="opened-work-actions">
      <ProtoButton
        styleProfile="rhea"
        variant="outline"
        size="sm"
        onClick={() => refs[0] && emit(node.node_id, refs[0])}
      >暂存为候选</ProtoButton>
      <ProtoButton
        styleProfile="rhea"
        variant="outline"
        size="sm"
        onClick={() => refs[1] && emit(node.node_id, refs[1])}
      >采纳候选</ProtoButton>
      <ProtoButton
        styleProfile="rhea"
        variant="ghost"
        size="sm"
        onClick={() => refs[2] && emit(node.node_id, refs[2])}
      >回到旧修订</ProtoButton>
    </div>
  </section>;
}

function PanelSection({ section, node, emit }: { section: UiNode; node: UiNode; emit: Emit }) {
  const name = section.props.section as string;
  const open = Boolean(section.props.open);
  return <CollapsibleRoot
    open={open}
    className="panel-collapsible"
    onOpenChange={(detail) => {
      if (detail.open !== open) emit(section.node_id, section.action_refs[0], { section: name });
    }}
  >
    <CollapsibleTrigger className="collapsible-trigger"><span>{SECTION_LABELS[name]}</span><Icon name={open ? 'minus' : 'plus'} /></CollapsibleTrigger>
    <CollapsibleContent className="collapsible-content"><PanelSectionBody name={name} node={node} /></CollapsibleContent>
  </CollapsibleRoot>;
}

function PanelSectionBody({ name, node }: { name: string; node: UiNode }) {
  const self = node.props.self;
  const config = node.props.config;
  if (name === 'status') return <dl className="panel-facts">
    <div><dt>主体状态</dt><dd>{self?.status && self.status !== 'unknown' ? self.status : '未知'}</dd></div>
    <div><dt>传输</dt><dd>{connectionAnnouncement(node.props.connection)}</dd></div>
    <div><dt>最近已见提交</dt><dd>{node.props.step ? `第 ${node.props.step} 次` : '尚无'}</dd></div>
    <div><dt>已连接客户端</dt><dd>{typeof self?.connected_clients === 'number' ? self.connected_clients : '未知'}</dd></div>
    <div><dt>模型绑定</dt><dd>{config?.model && config.model !== 'unknown' ? config.model : '未知'}</dd></div>
  </dl>;
  if (name === 'activity') {
    const parts = (node.props.timeline as TimelinePart[]).filter((part) => part.content).slice(-3).reverse();
    return parts.length ? <div className="panel-activity">{parts.map((part) => <div className="activity-row" key={part.part_id}><time>{formatTime(part.observed_at)}</time><span><strong>第 {part.step} 轮</strong>{firstLine(part.content)}</span></div>)}</div> : <p className="panel-note">尚无已提交内容</p>;
  }
  if (name === 'artifacts') {
    const artifacts = Array.isArray(self?.artifacts) ? self.artifacts : [];
    return artifacts.length ? <div className="panel-list">{artifacts.map((artifact: { id: string; title?: string; kind?: string }) => <div className="panel-list-row" key={artifact.id}><span>{artifact.title || artifact.id}</span><small>{artifact.kind || '未知类型'}</small></div>)}</div> : <p className="panel-note">暂无产物 · 未读取时不虚构</p>;
  }
  const devices = Array.isArray(self?.devices) ? self.devices : [];
  return devices.length ? <div className="panel-list">{devices.map((device: { id: string; name?: string; label?: string; kind?: string; online?: boolean }) => <div className="panel-list-row" key={device.id}><span>{device.name || device.label || device.id}</span><small>{device.online === true ? '在线' : device.online === false ? '离线' : '未知'}{device.kind ? ` · ${device.kind}` : ''}</small></div>)}</div> : <p className="panel-note">设备发现尚未接入；不显示本机绿色在线标记。</p>;
}

function formatTime(value: unknown): string {
  if (typeof value !== 'string' || !value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '—';
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
}

function dayLabel(value: string | null): string {
  if (value === null) return '时间未知';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '时间未知';
  const weekdays = '日一二三四五六';
  return `${date.getMonth() + 1}月${date.getDate()}日 · 周${weekdays[date.getDay()]}`;
}

function formatStamp(value: unknown): string {
  if (typeof value !== 'string' || !value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return `${date.getMonth() + 1}-${date.getDate()} ${formatTime(value)}`;
}

/** Disclosure timestamps are epoch milliseconds; only presentation changes here. */
function formatStampMs(value: number): string {
  return Number.isFinite(value) ? formatStamp(new Date(value).toISOString()) : '—';
}

function firstLine(text: string): string {
  return text.split('\n').find((line) => line.trim())?.trim() ?? '（空）';
}

function wsUrl(): string {
  if (!location.host) return '不可用';
  return `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws`;
}

/// The rendered text of one entry, excluding its chrome (step label, receipt
/// footer): a selection replay must address the content the reader saw.
function textRoot(entry: HTMLElement): HTMLElement {
  return entry.querySelector<HTMLElement>('.entry-content') ?? entry;
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

function Icon({ name }: { name: string }) {
  const common = { width: 16, height: 16, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor', strokeWidth: 1.35, strokeLinecap: 'round' as const, strokeLinejoin: 'round' as const, 'aria-hidden': true };
  switch (name) {
    case 'timeline': return <svg {...common}><path d="M4 2.5v11M4 4h8M4 8h6M4 12h8" /><circle cx="3.9" cy="2.5" r=".7" fill="currentColor" stroke="none" /></svg>;
    case 'tasks': return <svg {...common}><rect x="3" y="2.5" width="10" height="11" rx="2" /><path d="m5.5 6 1 1 2-2M9.5 6h1M5.5 10l1 1 2-2M9.5 10h1" /></svg>;
    case 'memory': return <svg {...common}><path d="M5 3.5h6a2 2 0 0 1 2 2v7H6a3 3 0 0 1-3-3v-6Z" /><path d="M6 3.5v9M8.5 6h2M8.5 8.5h2" /></svg>;
    case 'config': return <svg {...common}><circle cx="8" cy="8" r="2" /><path d="M8 1.8v1.6M8 12.6v1.6M1.8 8h1.6M12.6 8h1.6M3.6 3.6l1.1 1.1M11.3 11.3l1.1 1.1M12.4 3.6l-1.1 1.1M4.7 11.3l-1.1 1.1" /></svg>;
    case 'playground': return <svg {...common}><path d="M3 3.5h10v9H3Z" /><path d="m6.5 5.8 3.5 2.2-3.5 2.2Z" /></svg>;
    case 'panel': return <svg {...common}><rect x="2" y="2.5" width="12" height="11" rx="2" /><path d="M9.5 2.5v11M12 6h.01M12 9h.01" /></svg>;
    case 'sun': return <svg {...common}><circle cx="8" cy="8" r="2.5" /><path d="M8 1.5v1M8 13.5v1M1.5 8h1M13.5 8h1M3.4 3.4l.7.7M11.9 11.9l.7.7M12.6 3.4l-.7.7M4.1 11.9l-.7.7" /></svg>;
    case 'moon': return <svg {...common}><path d="M12.8 10.2A5.4 5.4 0 0 1 5.8 3.3a5.4 5.4 0 1 0 7 6.9Z" /></svg>;
    case 'copy': return <svg {...common}><rect x="5" y="5" width="8" height="8" rx="1.5" /><path d="M10.5 5V3.5a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1H5" /></svg>;
    case 'commit': return <svg {...common}><circle cx="8" cy="8" r="5.5" /><path d="m5.3 8.2 1.7 1.7 3.7-3.8" /></svg>;
    case 'arrow': return <svg {...common}><path d="M3 8h9M8.5 4.5 12 8l-3.5 3.5" /></svg>;
    case 'down': return <svg {...common}><path d="M8 2.5v10M4.5 9 8 12.5 11.5 9" /></svg>;
    case 'close': return <svg {...common}><path d="m4 4 8 8M12 4l-8 8" /></svg>;
    case 'plus': return <svg {...common}><path d="M8 3v10M3 8h10" /></svg>;
    case 'minus': return <svg {...common}><path d="M3 8h10" /></svg>;
    default: return <svg {...common}><circle cx="8" cy="8" r="5" /></svg>;
  }
}
