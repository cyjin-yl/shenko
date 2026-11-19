import { text as t } from './messages';
import * as React from 'react';
import { CompiledMessageBubble } from '../generated/prototypes/message-bubble/Component';
import { Icon } from './icons';
import { Markdown } from './markdown';
import type { Thread, ThreadMessage, Participant } from './threads';
import type { WebAttachmentRef } from './projection';
import { formatBytes } from './composer';
import { ToggleGroupItem, ToggleGroupRoot, dayLabel, formatTime, type Emit } from './ui-kit';

/**
 * The IM surface: the owner's primary way
 * of talking with the subject, in an iMessage-like idiom. Conversations are
 * a list on the left — the direct chat first, then delegation groups and
 * channel groups by recency — and the open thread on the right.
 *
 * The list is the upstream toggle-group in vertical orientation (one
 * selection, roving focus, arrow keys); bubbles are Shenko's own compiled
 * `message-bubble` Prototype with the message content in its slot.
 */
export function ImView({
  threads, selected, onSelect, composer, renderAttachment, emit, onOpenInspect, historyControl, demo,
}: {
  threads: Thread[];
  selected: string;
  onSelect: (id: string) => void;
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
    () => new URLSearchParams(location.search).has('thread') ? 'thread' : 'list',
  );
  return (
    <div className="im" data-thread-kind={thread?.kind} data-pane={pane}>
      <nav
        className="im-list"
        aria-label={t('conversation.label')}
        onClick={(event) => { if ((event.target as Element).closest('.im-row')) setPane('thread'); }}
      >
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
          {threads.map((item) => (
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
          ))}
        </ToggleGroupRoot>
      </nav>
      {thread
        ? <ThreadView
          key={thread.id}
          thread={thread}
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
  return `${date.getMonth() + 1}/${date.getDate()}`;
}

function ThreadView({ thread, composer, renderAttachment, emit, onOpenInspect, historyControl, demo, onBack }: {
  onBack: () => void;
  thread: Thread;
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
      <MessageViewport ref={viewport}>
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
class MessageViewport extends React.Component<{ children: React.ReactNode }> {
  private element = React.createRef<HTMLDivElement>();
  private following = true;
  private readingAnchor: MessageAnchor | null = null;
  private lastScrollTop = 0;
  private resizeObserver: ResizeObserver | null = null;

  componentDidMount() {
    const content = this.element.current?.firstElementChild;
    if (content) {
      this.resizeObserver = new ResizeObserver(() => this.restore(this.readingAnchor));
      this.resizeObserver.observe(content);
    }
    this.restore(null);
  }

  componentWillUnmount() {
    this.resizeObserver?.disconnect();
  }

  beginBackfill = () => {
    this.following = false;
    this.readingAnchor = this.captureAnchor();
  };

  getSnapshotBeforeUpdate(): MessageAnchor | null {
    return this.following ? null : this.readingAnchor ?? this.captureAnchor();
  }

  componentDidUpdate(_previous: unknown, _state: unknown, snapshot: MessageAnchor | null) {
    this.readingAnchor = snapshot;
    this.restore(snapshot);
  }

  private captureAnchor(): MessageAnchor | null {
    const element = this.element.current;
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
    const element = this.element.current;
    if (!element) return;
    if (this.following) element.scrollTop = element.scrollHeight;
    else if (snapshot) {
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
    const element = event.currentTarget;
    if (element.scrollTop === this.lastScrollTop) return;
    this.following = element.scrollHeight - element.scrollTop - element.clientHeight < 32;
    this.readingAnchor = this.following ? null : this.captureAnchor();
    this.lastScrollTop = element.scrollTop;
  };

  render() {
    return <div className="im-scroll" ref={this.element} onScroll={this.onScroll}>{this.props.children}</div>;
  }
}

function Messages({ messages, viewer, group, renderAttachment, emit, onOpenInspect }: {
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
        {(message.meta || tail && actions.length > 0) && (
          <div className="im-meta">
            {message.meta && <span>{message.meta}</span>}
            {tail && actions.map((action) => (
              <button key={action.id} type="button" className="im-link" onClick={() => message.node && emit(message.node.node_id, action.id)}>
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
  if (typeof node.props.withdraw_action === 'string' && !node.props.submission?.withdrawal) {
    actions.push({ id: node.props.withdraw_action, label: t('common.withdraw') });
  }
  return actions;
}
