import { text as t } from './messages';
import * as React from 'react';
import { CompiledMessageBubble } from '../generated/prototypes/message-bubble/Component';
import { Icon } from './icons';
import { Markdown } from './markdown';
import type { Thread, ThreadMessage, Participant } from './threads';
import type { WebAttachmentRef } from './projection';
import { formatBytes } from './composer';
import {
  ToggleGroupItem, ToggleGroupRoot, MessageRoot, ReadingViewport, ScopeTabs,
  ScrollBox, ScrollScrollbar, ScrollThumb, Button, dayLabel, formatTime,
  type Emit, type ReadingViewportHandle,
} from './ui-kit';

type ImScope = 'all' | 'chats' | 'agents';
const IM_DORMANT_MS = 7 * 86_400_000;

export interface ImRevealRequest {
  requestId: number;
  threadId: string;
  messageKey: string;
}

function threadTimestamp(thread: Thread): number {
  return thread.updated_at ? Date.parse(thread.updated_at) : NaN;
}

function matchesThread(thread: Thread, needle: string): boolean {
  const hay = [
    thread.title,
    thread.subtitle,
    thread.preview,
    ...thread.participants.map((p) => p.name),
  ].filter(Boolean).join(' ').toLowerCase();
  return hay.includes(needle);
}

/**
 * The IM surface: the owner's primary way
 * of talking with the subject, in an iMessage-like idiom. Conversations are
 * organized on the left with Spotlight search, scope filtering, and lifecycle
 * bucketing — direct chat pinned first, live subagents surfaced, human channel
 * conversations protected from ended-subagent clutter, and months-old dormant
 * subagents folded away until expanded or searched.
 *
 * The list is the upstream toggle-group in vertical orientation (one
 * selection, roving focus, arrow keys); bubbles are Shenko's own compiled
 * `message-bubble` Prototype with the message content in its slot.
 */
export function ImView({
  threads, selected, onSelect, revealRequest, onRevealComplete, composer, renderAttachment, emit, onOpenInspect, historyControl, demo,
}: {
  threads: Thread[];
  selected: string;
  onSelect: (id: string) => void;
  revealRequest?: ImRevealRequest;
  onRevealComplete?: (requestId: number) => void;
  composer: React.ReactNode;
  renderAttachment: (message: ThreadMessage) => React.ReactNode;
  emit: Emit;
  onOpenInspect: () => void;
  historyControl?: (beforeLoad: () => void) => React.ReactNode;
  demo: boolean;
}) {
  const thread = threads.find((item) => item.id === selected) ?? threads[0];
  // On a phone the list and the open thread are two screens, as in Messages;
  // wider layouts show both and ignore this.
  const [pane, setPane] = React.useState<'list' | 'thread'>(
    () => revealRequest?.threadId === selected || new URLSearchParams(location.search).has('thread') ? 'thread' : 'list',
  );
  React.useEffect(() => {
    if (revealRequest?.threadId === selected) setPane('thread');
  }, [revealRequest, selected]);
  const [scope, setScope] = React.useState<ImScope>('all');
  const [query, setQuery] = React.useState('');
  const [archiveOpen, setArchiveOpen] = React.useState(false);

  const sections = React.useMemo(() => {
    let newestMs = 0;
    for (const item of threads) {
      if (item.kind === 'direct') continue;
      const ts = threadTimestamp(item);
      if (Number.isFinite(ts) && ts > newestMs) newestMs = ts;
    }
    if (!newestMs) newestMs = Date.now();
    const direct: Thread[] = [];
    const activeAgents: Thread[] = [];
    const channels: Thread[] = [];
    const recentAgents: Thread[] = [];
    const archivedAgents: Thread[] = [];
    for (const item of threads) {
      if (item.kind === 'direct') {
        direct.push(item);
      } else if (item.kind === 'channel') {
        channels.push(item);
      } else if (item.active) {
        activeAgents.push(item);
      } else {
        const ts = threadTimestamp(item);
        if (Number.isFinite(ts) && (newestMs - ts) > IM_DORMANT_MS) {
          archivedAgents.push(item);
        } else {
          recentAgents.push(item);
        }
      }
    }
    return { direct, activeAgents, channels, recentAgents, archivedAgents };
  }, [threads]);

  const selectedInArchive = Boolean(thread && sections.archivedAgents.some((item) => item.id === thread.id));
  React.useEffect(() => {
    if (selectedInArchive) setArchiveOpen(true);
  }, [selectedInArchive]);

  const needle = query.trim().toLowerCase();
  const searchResults = React.useMemo(
    () => (needle ? threads.filter((item) => matchesThread(item, needle)) : []),
    [threads, needle],
  );
  const chatCount = sections.direct.length + sections.channels.length;
  const agentCount = sections.activeAgents.length + sections.recentAgents.length + sections.archivedAgents.length;
  const showControls = threads.length > 1;
  const archiveExpanded = archiveOpen;

  const renderRow = (item: Thread) => (
    <ToggleGroupItem key={item.id} value={item.id} className={`im-row kind-${item.kind}`}>
      <Avatar participants={item.participants} audience={item.audience} active={item.active} />
      <span className="im-row-text">
        <span className="im-row-top">
          <span className="im-row-title">{item.title}</span>
          <time className="im-row-time">{shortWhen(item.updated_at)}</time>
        </span>
        <span className="im-row-preview">{item.preview}</span>
        {item.active && <span className="sr-only">{item.state ?? t('taskState.running')}</span>}
      </span>
    </ToggleGroupItem>
  );

  const renderArchiveSection = () => {
    if (sections.archivedAgents.length === 0) return null;
    return (
      <>
        <button
          type="button"
          className="im-archive-toggle"
          aria-expanded={archiveExpanded}
          onClick={(event) => {
            event.stopPropagation();
            setArchiveOpen((open) => !open);
          }}
        >
          <Icon name="chevron" size={12} />
          <span className="im-archive-title">{t('im.section.archivedAgents')}</span>
          <span className="im-section-count">{sections.archivedAgents.length}</span>
        </button>
        {archiveExpanded && sections.archivedAgents.map(renderRow)}
      </>
    );
  };

  return (
    <div className="im" data-thread-kind={thread?.kind} data-pane={pane}>
      <nav
        className="im-list"
        aria-label={t('conversation.label')}
        onClick={(event) => { if ((event.target as Element).closest('.im-row')) setPane('thread'); }}
      >
        {showControls && (
          <div className="im-controls">
            <div className="inspector-search-wrap im-search-wrap">
              <Icon name="search" size={13} />
              <input
                type="search"
                className="inspector-search im-search"
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === 'Escape') {
                    if (query) setQuery('');
                    else (event.currentTarget as HTMLInputElement).blur();
                  }
                }}
                placeholder={t('im.searchPlaceholder')}
                aria-label={t('im.searchPlaceholder')}
                spellCheck={false}
              />
              {query && (
                <button
                  type="button"
                  className="inspector-search-clear"
                  aria-label={t('files.searchClear')}
                  onClick={() => setQuery('')}
                >
                  <Icon name="close" size={12} />
                </button>
              )}
            </div>
            {!needle && (
              <ScopeTabs
                value={scope}
                onChange={setScope}
                label={t('conversation.label')}
                className="im-scope-bar"
                items={[
                  { id: 'all', label: t('im.scope.all'), count: threads.length },
                  { id: 'chats', label: t('im.scope.chats'), count: chatCount },
                  { id: 'agents', label: t('im.scope.agents'), count: agentCount },
                ]}
              />
            )}
          </div>
        )}
        <ScrollBox className="im-list-scroll" viewportClassName="im-list-viewport">
          {needle && searchResults.length === 0 ? (
            <p className="im-no-matches">{t('im.noMatches')}</p>
          ) : (
            <ToggleGroupRoot
              className="im-list-group"
              a11yLabel={t('conversation.label')}
              orientation="vertical"
              value={thread ? [thread.id] : []}
              onValueChange={(detail: { value: readonly string[] }) => {
                const next = detail.value.find((id) => id !== thread?.id);
                if (next) onSelect(next);
              }}
            >
              {needle ? (
                searchResults.map(renderRow)
              ) : scope === 'chats' ? (
                <>
                  {sections.direct.map(renderRow)}
                  {sections.channels.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.channels')}</span>
                      <span className="im-section-count">{sections.channels.length}</span>
                    </div>
                  )}
                  {sections.channels.map(renderRow)}
                </>
              ) : scope === 'agents' ? (
                <>
                  {sections.activeAgents.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.activeAgents')}</span>
                      <span className="im-section-count">{sections.activeAgents.length}</span>
                    </div>
                  )}
                  {sections.activeAgents.map(renderRow)}
                  {sections.recentAgents.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.recentAgents')}</span>
                      <span className="im-section-count">{sections.recentAgents.length}</span>
                    </div>
                  )}
                  {sections.recentAgents.map(renderRow)}
                  {renderArchiveSection()}
                  {agentCount === 0 && <p className="im-no-matches">{t('im.noMatches')}</p>}
                </>
              ) : (
                <>
                  {sections.direct.map(renderRow)}
                  {sections.activeAgents.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.activeAgents')}</span>
                      <span className="im-section-count">{sections.activeAgents.length}</span>
                    </div>
                  )}
                  {sections.activeAgents.map(renderRow)}
                  {sections.channels.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.channels')}</span>
                      <span className="im-section-count">{sections.channels.length}</span>
                    </div>
                  )}
                  {sections.channels.map(renderRow)}
                  {sections.recentAgents.length > 0 && (
                    <div className="im-section-head" aria-hidden="true">
                      <span>{t('im.section.recentAgents')}</span>
                      <span className="im-section-count">{sections.recentAgents.length}</span>
                    </div>
                  )}
                  {sections.recentAgents.map(renderRow)}
                  {renderArchiveSection()}
                </>
              )}
            </ToggleGroupRoot>
          )}
        </ScrollBox>
      </nav>
      {thread
        ? <ThreadView
          key={thread.id}
          thread={thread}
          revealRequest={pane === 'thread' && revealRequest?.threadId === thread.id ? revealRequest : undefined}
          onRevealComplete={onRevealComplete}
          composer={composer}
          renderAttachment={renderAttachment}
          emit={emit}
          onOpenInspect={onOpenInspect}
          historyControl={historyControl}
          demo={demo}
          onBack={() => setPane('list')}
        />
        : <div className="im-empty">{t('conversation.empty')}</div>}
    </div>
  );
}

function initials(name: string): string {
  return Array.from(name.trim())[0] ?? '·';
}

/// A monogram, as Messages draws a contact without a photo: a one-to-one
/// thread shows the counterpart, a group up to two members overlapped.
/// A group shows its latest-joined member in front, so a delegation reads as
/// the child and a team as its members rather than the subject repeated on
/// every row. `active` marks someone in it still working.
function Avatar({ participants, audience, size = 40, active }: {
  participants: Participant[]; audience: Thread['audience']; size?: number; active?: boolean;
}) {
  const shown = audience === 'one_to_one' ? participants.slice(1, 2) : participants.slice(-2);
  return (
    <span className={`avatar avatar-${shown.length > 1 ? 'group' : 'single'}`} style={{ width: size, height: size }} aria-hidden="true">
      {shown.map((participant) => (
        <span key={participant.id} className={`monogram role-${participant.role}`}>{initials(participant.name)}</span>
      ))}
      {active && <span className="avatar-live" />}
    </span>
  );
}

function shortWhen(value: string | undefined): string {
  if (!value) return '';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '';
  const now = new Date();
  if (date.toDateString() === now.toDateString()) return formatTime(value);
  if (date.getFullYear() !== now.getFullYear()) {
    return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()}`;
  }
  return `${date.getMonth() + 1}/${date.getDate()}`;
}

function ThreadView({ thread, revealRequest, onRevealComplete, composer, renderAttachment, emit, onOpenInspect, historyControl, demo, onBack }: {
  onBack: () => void;
  thread: Thread;
  revealRequest?: ImRevealRequest;
  onRevealComplete?: (requestId: number) => void;
  composer: React.ReactNode;
  renderAttachment: (message: ThreadMessage) => React.ReactNode;
  emit: Emit;
  onOpenInspect: () => void;
  historyControl?: (beforeLoad: () => void) => React.ReactNode;
  demo: boolean;
}) {
  const [loaded, setLoaded] = React.useState<ThreadMessage[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!thread.load || demo) return;
    let live = true;
    thread.load().then((messages) => { if (live) { setLoaded(messages); setError(null); } })
      .catch((reason) => { if (live) setError(String(reason?.message ?? reason)); });
    return () => { live = false; };
  }, [thread.id, thread.state, thread.loadRevision, demo]);
  const messages = loaded ?? thread.messages;
  const viewport = React.useRef<MessageViewport>(null);
  React.useEffect(() => {
    if (!revealRequest) return;
    return viewport.current?.revealMessage(revealRequest.messageKey,
      () => onRevealComplete?.(revealRequest.requestId));
  }, [revealRequest, onRevealComplete]);
  const group = thread.audience === 'group';
  return (
    <section className="im-thread" aria-label={thread.title}>
      <header className="im-head">
        <button type="button" className="im-back" onClick={onBack} aria-label={t('conversation.back')}>
          <Icon name="chevron" size={22} />
        </button>
        <Avatar participants={thread.participants} audience={thread.audience} size={28} />
        <span className="im-head-text">
          <span className="im-head-title">{thread.title}</span>
          <span className="im-head-sub">
            {thread.subtitle}
            {thread.state ? ` · ${thread.state}` : ''}
          </span>
        </span>
      </header>
      <MessageViewport key={thread.id} ref={viewport}>
        <div className="im-messages">
          {thread.kind === 'direct' && historyControl?.(() => viewport.current?.beginBackfill())}
          {thread.note && <p className="im-note">{thread.note}</p>}
          {error && <p className="im-system is-error">{t('conversation.readError', { detail: error })}</p>}
          {thread.load && !loaded && !error && !demo && <p className="im-system">{t('common.loading')}</p>}
          {messages.length === 0 && !thread.load && <p className="im-system">{t('common.messagesEmpty')}</p>}
          <Messages
            messages={messages}
            viewer={thread.viewer}
            group={group}
            renderAttachment={renderAttachment}
            emit={emit}
            onOpenInspect={onOpenInspect}
          />
        </div>
      </MessageViewport>
      {thread.composer ? composer : null}
    </section>
  );
}

interface MessageAnchor {
  key?: string;
  offset?: number;
  scrollTop: number;
}

// Compiled bubbles publish their hosts after the parent React commit. Keep
// the reading anchor through those content resizes, until the reader moves.
class MessageViewport extends React.Component<{ children: React.ReactNode }, { paused: boolean; jumpFocused: boolean }> {
  state = { paused: false, jumpFocused: false };
  private element = React.createRef<HTMLDivElement>();
  private viewport: ReadingViewportHandle | null = null;
  private offFollowing: (() => void) | undefined;
  private active = false;
  private epoch = 0;
  private readingAnchor: MessageAnchor | null = null;
  private lastScrollTop = 0;
  private resizeObserver: ResizeObserver | null = null;

  componentDidMount() {
    this.active = true;
    document.addEventListener('selectionchange', this.onSelectionChange);
  }

  private bindContent = (content: HTMLDivElement | null) => {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    if (!content) return;
    const observer = new ResizeObserver(() => {
      if (this.active && this.resizeObserver === observer) this.restore(this.readingAnchor);
    });
    this.resizeObserver = observer;
    observer.observe(content);
  };

  componentWillUnmount() {
    this.active = false;
    document.removeEventListener('selectionchange', this.onSelectionChange);
    this.epoch += 1;
    this.offFollowing?.();
    this.offFollowing = undefined;
    this.viewport = null;
    this.resizeObserver?.disconnect();
  }

  beginBackfill = () => {
    this.viewport?.getExposes().pauseFollowing();
    this.readingAnchor = this.captureAnchor();
  };

  /** Proto-UI slots can mount after the React parent. Observe their real
   * materialization instead of racing a one-shot query against the adapter. */
  revealMessage = (key: string, complete: () => void): (() => void) => {
    const host = this.element.current;
    if (!host) return () => undefined;
    let frame = 0;
    let cancelled = false;
    const cleanup = () => {
      cancelled = true;
      cancelAnimationFrame(frame);
      mutation.disconnect();
      resize.disconnect();
    };
    const schedule = () => {
      if (cancelled || frame) return;
      frame = requestAnimationFrame(() => {
        frame = 0;
        if (cancelled || !this.active) return;
        const surface = this.surface();
        const row = [...host.querySelectorAll<HTMLElement>('[data-message-key]')]
          .find((element) => element.dataset.messageKey === key);
        if (!this.viewport || !surface?.clientHeight || !row?.getClientRects().length) return;
        this.viewport.getExposes().pauseFollowing();
        const target = row.querySelector<HTMLElement>('.doc-option:not(:disabled)') ?? row;
        if (target === row) row.tabIndex = -1;
        const bounds = target.getBoundingClientRect();
        const viewportBounds = surface.getBoundingClientRect();
        surface.scrollTop += bounds.top - viewportBounds.top - Math.max(0, (surface.clientHeight - bounds.height) / 2);
        target.focus({ preventScroll: true });
        this.readingAnchor = this.captureAnchor();
        this.lastScrollTop = surface.scrollTop;
        cleanup();
        complete();
      });
    };
    const mutation = new MutationObserver(schedule);
    mutation.observe(host, { childList: true, subtree: true });
    const resize = new ResizeObserver(schedule);
    resize.observe(host);
    schedule();
    return cleanup;
  };

  private onSelectionChange = () => {
    if (!this.active) return;
    const selection = window.getSelection();
    if (selection && !selection.isCollapsed
      && this.element.current?.contains(selection.anchorNode)
      && this.element.current.contains(selection.focusNode)) this.beginBackfill();
  };

  private isFollowing() {
    return this.viewport?.getExposes().following.get() !== 'paused';
  }

  private bindViewport = (viewport: ReadingViewportHandle | null) => {
    this.offFollowing?.();
    this.offFollowing = undefined;
    this.viewport = viewport;
    const epoch = ++this.epoch;
    if (!viewport) return;
    const exposed = viewport.getExposes();
    this.offFollowing = exposed.following.subscribe(() => {
      queueMicrotask(() => {
        if (!this.active || epoch !== this.epoch || this.viewport !== viewport) return;
        const paused = exposed.following.get() === 'paused';
        this.readingAnchor = paused ? this.readingAnchor ?? this.captureAnchor() : null;
        if (paused !== this.state.paused) this.setState({ paused });
      });
    });
  };

  private surface(): HTMLElement | null {
    const viewport = this.element.current?.querySelector<HTMLElement>('.im-scroll');
    if (!viewport) return null;
    return [viewport, ...Array.from(viewport.querySelectorAll<HTMLElement>('*'))].find((element) => {
      const overflow = getComputedStyle(element).overflowY;
      return overflow === 'auto' || overflow === 'scroll';
    }) ?? viewport;
  }

  getSnapshotBeforeUpdate(): MessageAnchor | null {
    return this.isFollowing() ? null : this.readingAnchor ?? this.captureAnchor();
  }

  componentDidUpdate(_previous: unknown, _state: unknown, snapshot: MessageAnchor | null) {
    this.readingAnchor = snapshot;
    this.restore(snapshot);
  }

  private captureAnchor(): MessageAnchor | null {
    const element = this.surface();
    if (!element) return null;
    const viewport = element.getBoundingClientRect();
    let summary: HTMLElement | undefined;
    for (const row of element.querySelectorAll<HTMLElement>('[data-message-key]')) {
      const bounds = row.getBoundingClientRect();
      if (bounds.bottom <= viewport.top) continue;
      if (bounds.top >= viewport.bottom) break;
      // An unpaired-round summary becomes real messages when history loads.
      // Anchor the visible message that survives that classification change.
      if (row.classList.contains('im-msg')) return {
        key: row.dataset.messageKey, offset: bounds.top - viewport.top, scrollTop: element.scrollTop,
      };
      summary ??= row;
    }
    if (summary) return {
      key: summary.dataset.messageKey,
      offset: summary.getBoundingClientRect().top - viewport.top,
      scrollTop: element.scrollTop,
    };
    return { scrollTop: element.scrollTop };
  }

  private restore(snapshot: MessageAnchor | null) {
    const element = this.surface();
    if (!element) return;
    if (!this.isFollowing() && snapshot) {
      let target: HTMLElement | undefined;
      if (snapshot.key) for (const row of element.querySelectorAll<HTMLElement>('[data-message-key]')) {
        if (row.dataset.messageKey === snapshot.key) { target = row; break; }
      }
      if (target && snapshot.offset !== undefined) {
        element.scrollTop += target.getBoundingClientRect().top
          - element.getBoundingClientRect().top - snapshot.offset;
      } else element.scrollTop = snapshot.scrollTop;
    }
    this.lastScrollTop = element.scrollTop;
  }

  private onScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const element = this.surface();
    if (!element || event.target !== element) return;
    if (element.scrollTop === this.lastScrollTop) return;
    this.readingAnchor = this.isFollowing() ? null : this.captureAnchor();
    this.lastScrollTop = element.scrollTop;
  };

  render() {
    return <div className="im-scroll-frame" ref={this.element} onScrollCapture={this.onScroll}
      onPointerDown={(event) => {
        if ((event.target as Element).closest('pre,button,details,input,textarea,select,[role="button"]')) this.beginBackfill();
      }}>
      <MessageRoot className="im-message-root" a11yLabel={t('conversation.label')}>
        <ReadingViewport className="im-scroll" ref={this.bindViewport}>
          {/* Focused message actions are reader interaction, including Tab's
              native scroll. Composer and resume controls own separate focus. */}
          <div ref={this.bindContent} onFocusCapture={this.beginBackfill}>{this.props.children}</div>
        </ReadingViewport>
        <ScrollScrollbar orientation="vertical" className="sk-scrollbar sk-scrollbar-y">
          <ScrollThumb className="sk-scroll-thumb" />
        </ScrollScrollbar>
      </MessageRoot>
      {(this.state.paused || this.state.jumpFocused) && <div className="jump-latest"
        onFocusCapture={() => this.setState({ jumpFocused: true })}
        onBlurCapture={() => this.setState({ jumpFocused: false })}>
        <Button variant="secondary" onClick={() => this.viewport?.getExposes().jumpToEnd()}>
          <Icon name="down" size={15} />{t('timeline.latest')}
        </Button>
      </div>}
    </div>;
  }
}

export function Messages({ messages, viewer, group, renderAttachment, emit, onOpenInspect }: {
  messages: ThreadMessage[];
  viewer: string;
  group: boolean;
  renderAttachment: (message: ThreadMessage) => React.ReactNode;
  emit: Emit;
  onOpenInspect: () => void;
}) {
  const out: React.ReactNode[] = [];
  let lastAt = 0;
  let lastDay = '';
  messages.forEach((message, index) => {
    const at = message.at ? Date.parse(message.at) : NaN;
    // Messages prints the time above a message after a quiet gap.
    if (Number.isFinite(at) && (at - lastAt > 15 * 60_000)) {
      const day = dayLabel(message.at ?? null);
      out.push(<p className="im-time" key={`time-${message.key}`}>{day === lastDay ? formatTime(message.at) : `${day} ${formatTime(message.at)}`}</p>);
      lastDay = day;
    }
    if (Number.isFinite(at)) lastAt = at;
    if (message.kind === 'system') {
      out.push(
        <p className="im-system" key={message.key} data-message-key={message.key}>
          {message.text}
          {message.link && <button type="button" className="im-link" onClick={onOpenInspect}>{message.link.label}</button>}
        </p>,
      );
      return;
    }
    const previous = messages[index - 1];
    const next = messages[index + 1];
    const same = (other?: ThreadMessage) => other && other.kind === 'message' && other.from.id === message.from.id;
    const self = message.from.id === viewer;
    const first = !same(previous);
    const tail = !same(next);
    const attachment = message.text === '' && message.node ? renderAttachment(message) : null;
    const actions = messageActions(message);
    out.push(
      <div
        className={`im-msg ${self ? 'is-self' : 'is-other'}${first ? ' is-first' : ''}${tail ? ' is-tail' : ''}`}
        key={message.key}
        data-message-key={message.key}
        data-entry-key={message.node?.node_id}
      >
        {!self && group && first && <span className="im-sender">{message.from.name}</span>}
        {message.attachments && message.attachments.length > 0 && (
          <div className="im-attachments">
            {message.attachments.map((file) => <AttachmentView key={file.digest} file={file} />)}
          </div>
        )}
        {(message.text || attachment || message.tone === 'pending' || !message.attachments?.length) && <div className="im-line">
          {!self && group && <span className="im-avatar-slot">{tail && <span className={`monogram role-${message.from.role}`}>{initials(message.from.name)}</span>}</span>}
          {attachment ?? (
            <CompiledMessageBubble side={self ? 'self' : 'other'} tail={tail} tone={message.tone ?? 'plain'}>
              {message.tag && <span className="bubble-tag">{message.tone === 'steer' && <Icon name="steer" size={12} />}{message.tag}</span>}
              {message.tone === 'pending' && !message.text
                ? <span className="typing" aria-label={t('timeline.streaming')}><i /><i /><i /></span>
                : message.from.role === 'owner' || message.from.role === 'person'
                  ? <span className="bubble-text">{message.text}</span>
                  : <Markdown text={message.text} />}
            </CompiledMessageBubble>
          )}
        </div>}
        {(message.meta || actions.length > 0) && (
          <div className="im-meta">
            {message.meta && <span>{message.meta}</span>}
            {actions.map((action) => (
              <button key={action.id} type="button" className="im-link" data-action-id={action.id} onClick={() => message.node && emit(message.node.node_id, action.id)}>
                {action.label}
              </button>
            ))}
          </div>
        )}
      </div>,
    );
  });
  return <>{out}</>;
}

/// An attachment as Messages shows one: a photo stands alone with rounded
/// corners; any other file is a card with its name and size that downloads
/// the exact stored bytes.
function AttachmentView({ file }: { file: WebAttachmentRef }) {
  const href = `/api/attachments/${encodeURIComponent(file.digest)}?type=${encodeURIComponent(file.media_type)}`;
  if (/^image\/(png|jpeg|gif|webp|avif)$/.test(file.media_type)) {
    return <a className="im-photo" href={href} target="_blank" rel="noreferrer"><img src={href} alt={file.name} loading="lazy" /></a>;
  }
  return (
    <a className="im-file" href={href} download={file.name}>
      <span className="im-file-icon"><Icon name="file" size={22} /></span>
      <span className="im-file-text">
        <span className="im-file-name">{file.name}</span>
        <span className="im-file-meta">{formatBytes(file.bytes)}</span>
      </span>
    </a>
  );
}

function messageActions(message: ThreadMessage): Array<{ id: string; label: string }> {
  const node = message.node;
  if (!node || node.kind !== 'user_submission') return [];
  const actions: Array<{ id: string; label: string }> = [];
  if (typeof node.props.retry_action === 'string') actions.push({ id: node.props.retry_action, label: t('common.retry') });
  if (typeof node.props.edit_action === 'string') actions.push({ id: node.props.edit_action, label: t('common.edit') });
  if (typeof node.props.withdraw_action === 'string' && !node.props.submission?.withdrawal) {
    actions.push({ id: node.props.withdraw_action, label: t('common.withdraw') });
  }
  return actions;
}
