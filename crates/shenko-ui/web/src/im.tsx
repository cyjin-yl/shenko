import * as React from 'react';
import { CompiledMessageBubble } from '../generated/prototypes/message-bubble/Component';
import { Icon } from './icons';
import { Markdown } from './markdown';
import type { Thread, ThreadMessage, Participant } from './threads';
import { ToggleGroupItem, ToggleGroupRoot, dayLabel, formatTime, type Emit } from './ui-kit';

/**
 * The IM surface (docs/specs/web-interface.md#5-2): the owner's primary way
 * of talking with the subject, in an iMessage-like idiom. Conversations are
 * a list on the left — the direct chat first, then delegation groups and
 * channel groups by recency — and the open thread on the right.
 *
 * The list is the upstream toggle-group in vertical orientation (one
 * selection, roving focus, arrow keys); bubbles are Shenko's own compiled
 * `message-bubble` Prototype with the message content in its slot.
 */
export function ImView({
  threads, selected, onSelect, composer, renderAttachment, emit, onOpenInspect, historyControl, historyLoading, demo,
}: {
  threads: Thread[];
  selected: string;
  onSelect: (id: string) => void;
  composer: React.ReactNode;
  renderAttachment: (message: ThreadMessage) => React.ReactNode;
  emit: Emit;
  onOpenInspect: () => void;
  historyControl?: (beforeLoad: () => void) => React.ReactNode;
  historyLoading?: boolean;
  demo: boolean;
}) {
  const thread = threads.find((item) => item.id === selected) ?? threads[0];
  return (
    <div className="im" data-thread-kind={thread?.kind}>
      <nav className="im-list" aria-label="会话">
        <ToggleGroupRoot
          className="im-list-group"
          a11yLabel="会话"
          orientation="vertical"
          value={thread ? [thread.id] : []}
          onValueChange={(detail: { value: readonly string[] }) => {
            const next = detail.value.find((id) => id !== thread?.id);
            if (next) onSelect(next);
          }}
        >
          {threads.map((item) => (
            <ToggleGroupItem key={item.id} value={item.id} className="im-row" data-kind={item.kind}>
              <Avatar participants={item.participants} kind={item.kind} />
              <span className="im-row-text">
                <span className="im-row-top">
                  <span className="im-row-title">{item.title}</span>
                  <time className="im-row-time">{shortWhen(item.updated_at)}</time>
                </span>
                <span className="im-row-preview">{item.preview}</span>
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
          historyLoading={historyLoading}
          demo={demo}
        />
        : <div className="im-empty">还没有会话。</div>}
    </div>
  );
}

function initials(name: string): string {
  return Array.from(name.trim())[0] ?? '·';
}

/// A monogram, as Messages draws a contact without a photo; a group shows
/// up to two members overlapped.
function Avatar({ participants, kind, size = 40 }: { participants: Participant[]; kind: string; size?: number }) {
  const shown = kind === 'direct' ? participants.slice(1, 2) : participants.slice(0, 2);
  return (
    <span className={`avatar avatar-${shown.length > 1 ? 'group' : 'single'}`} style={{ width: size, height: size }} aria-hidden="true">
      {shown.map((participant) => (
        <span key={participant.id} className={`monogram role-${participant.role}`}>{initials(participant.name)}</span>
      ))}
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

function ThreadView({ thread, composer, renderAttachment, emit, onOpenInspect, historyControl, historyLoading, demo }: {
  thread: Thread;
  composer: React.ReactNode;
  renderAttachment: (message: ThreadMessage) => React.ReactNode;
  emit: Emit;
  onOpenInspect: () => void;
  historyControl?: (beforeLoad: () => void) => React.ReactNode;
  historyLoading?: boolean;
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
  }, [thread.id, thread.state, demo]);
  const messages = loaded ?? thread.messages;
  const viewport = React.useRef<MessageViewport>(null);
  const group = thread.participants.length > 2 || thread.kind !== 'direct';
  return (
    <section className="im-thread" aria-label={thread.title}>
      <header className="im-head">
        <Avatar participants={thread.participants} kind={thread.kind} size={28} />
        <span className="im-head-text">
          <span className="im-head-title">{thread.title}</span>
          <span className="im-head-sub">
            {thread.kind === 'direct' ? '直接对话' : `${thread.participants.length} 位参与者`}
            {thread.state ? ` · ${thread.state}` : ''}
          </span>
        </span>
      </header>
      <MessageViewport ref={viewport} historyLoading={historyLoading === true}>
        <div className="im-messages">
          {thread.kind === 'direct' && historyControl?.(() => viewport.current?.beginBackfill())}
          {thread.note && <p className="im-note">{thread.note}</p>}
          {error && <p className="im-system is-error">这段对话没能读出来：{error}</p>}
          {thread.load && !loaded && !error && !demo && <p className="im-system">正在读取…</p>}
          {messages.length === 0 && !thread.load && <p className="im-system">还没有消息。</p>}
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

// Capture the old DOM before mutation, as the Inspect timeline does. A page
// load keeps its original anchor through the intermediate loading control.
class MessageViewport extends React.Component<{ historyLoading: boolean; children: React.ReactNode }> {
  private element = React.createRef<HTMLDivElement>();
  private following = true;
  private backfillAnchor: MessageAnchor | null = null;
  private lastScrollTop = 0;

  componentDidMount() {
    this.restore(null);
  }

  beginBackfill = () => {
    this.following = false;
    this.backfillAnchor = this.captureAnchor();
  };

  getSnapshotBeforeUpdate(): MessageAnchor | null {
    return this.following ? null : this.backfillAnchor ?? this.captureAnchor();
  }

  componentDidUpdate(_previous: unknown, _state: unknown, snapshot: MessageAnchor | null) {
    this.restore(snapshot);
    if (!this.props.historyLoading) this.backfillAnchor = null;
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
    this.backfillAnchor = null;
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
    out.push(
      <div
        className={`im-msg ${self ? 'is-self' : 'is-other'}${first ? ' is-first' : ''}${tail ? ' is-tail' : ''}`}
        key={message.key}
        data-message-key={message.key}
        data-entry-key={message.node?.node_id}
      >
        {!self && group && first && <span className="im-sender">{message.from.name}</span>}
        <div className="im-line">
          {!self && group && <span className="im-avatar-slot">{tail && <span className={`monogram role-${message.from.role}`}>{initials(message.from.name)}</span>}</span>}
          {attachment ?? (
            <CompiledMessageBubble side={self ? 'self' : 'other'} tail={tail} tone={message.tone ?? 'plain'}>
              {message.tag && <span className="bubble-tag">{message.tag === '引导' && <Icon name="steer" size={12} />}{message.tag}</span>}
              {message.tone === 'pending' && !message.text
                ? <span className="typing" aria-label="正在输出"><i /><i /><i /></span>
                : message.from.role === 'owner' || message.from.role === 'person'
                  ? <span className="bubble-text">{message.text}</span>
                  : <Markdown text={message.text} />}
            </CompiledMessageBubble>
          )}
        </div>
        {(message.meta || messageActions(message).length > 0) && tail && (
          <div className="im-meta">
            {message.meta && <span>{message.meta}</span>}
            {messageActions(message).map((action) => (
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

function messageActions(message: ThreadMessage): Array<{ id: string; label: string }> {
  const node = message.node;
  if (!node || node.kind !== 'user_submission') return [];
  const actions: Array<{ id: string; label: string }> = [];
  if (typeof node.props.retry_action === 'string') actions.push({ id: node.props.retry_action, label: '重试' });
  if (typeof node.props.withdraw_action === 'string' && !node.props.submission?.withdrawal) {
    actions.push({ id: node.props.withdraw_action, label: '撤回' });
  }
  return actions;
}
