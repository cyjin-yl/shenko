import { text as t } from './messages';
import type { Submission, TaskSummary, TimelinePart, UiNode, UiProjection } from './projection';

/**
 * Conversations as one model (docs/specs/web-interface.md#5-2).
 *
 * Every conversation the subject takes part in is a thread with participants:
 * the owner's direct chat with the subject, a delegation where the subject
 * and the children it started talk, and a channel group such as a QQ group.
 * They differ only in who takes part and whose side the viewer sits on, so
 * the IM view renders all of them with the same parts.
 *
 * Sources are registered like lenses: the shell's own sources and a plugin's
 * use the same `registerThreadSource()`. Every message is built from a
 * durable fact — a submission and its receipt, the committed step that took
 * it, a sealed step record, a channel envelope or a publication intent.
 */
export type ParticipantRole = 'owner' | 'subject' | 'child' | 'tool' | 'person';

export interface Participant {
  id: string;
  name: string;
  role: ParticipantRole;
}

export interface ThreadMessage {
  key: string;
  from: Participant;
  text: string;
  at?: string;
  /** `system` rows are facts about the thread (a tool ran, rounds passed). */
  kind: 'message' | 'system';
  tone?: 'plain' | 'steer' | 'pending';
  /** A short factual label under the bubble (receipt, publication state). */
  meta?: string;
  /** The UI node behind the message, for actions such as withdraw/retry. */
  node?: UiNode;
  /** A system row that leads somewhere else in the shell. */
  link?: { label: string; target: 'inspect' };
  /** Small tag shown above the text, e.g. 提问 / 引导. */
  tag?: string;
}

export type ThreadKind = 'direct' | 'delegation' | 'channel';

/** The layout used for the source's audience shape. */
export type ThreadAudience = 'one_to_one' | 'group';

export interface Thread {
  id: string;
  kind: ThreadKind;
  /**
   * A one-to-one thread reads like a private chat: no sender names, one
   * avatar. It comes from facts the thread source holds (a channel plugin's
   * declared audience shape). An unknown shape uses the group layout while
   * its subtitle keeps the missing audience information explicit.
   */
  audience: ThreadAudience;
  title: string;
  /** One line under the title: where and with whom, in the source's words. */
  subtitle: string;
  /** Known message senders, the viewer's side first; not a membership roster. */
  participants: Participant[];
  /** The participant whose messages sit on the viewer's (right) side. */
  viewer: string;
  preview: string;
  updated_at?: string;
  /** Messages known now; a thread may also load more on open. */
  messages: ThreadMessage[];
  /** Loads the full thread when it is opened (e.g. a delegation trajectory). */
  load?: () => Promise<ThreadMessage[]>;
  /** Whether the shell's composer posts into this thread. */
  composer: boolean;
  /** Why the viewer only reads this thread, when it is so. */
  note?: string;
  state?: string;
}

export interface DemoThreadData {
  conversations: ConversationRow[];
  trajectory(taskId: string): TrajectoryResponse;
}

export interface ThreadInputs {
  projection: UiProjection;
  tasks: TaskSummary[];
  conversations: ConversationRow[];
  demo?: DemoThreadData;
}

export interface ThreadSource {
  id: string;
  order: number;
  threads(inputs: ThreadInputs): Thread[];
}

const sources = new Map<string, ThreadSource>();

export function registerThreadSource(source: ThreadSource): () => void {
  sources.set(source.id, source);
  return () => { if (sources.get(source.id) === source) sources.delete(source.id); };
}

export function collectThreads(inputs: ThreadInputs): Thread[] {
  if (inputs.projection.source === 'demo' && !inputs.demo) throw new Error('Demo thread data is missing');
  const all = [...sources.values()].sort((a, b) => a.order - b.order).flatMap((source) => source.threads(inputs));
  const direct = all.filter((thread) => thread.kind === 'direct');
  const rest = all.filter((thread) => thread.kind !== 'direct')
    .sort((a, b) => Date.parse(b.updated_at ?? '') - Date.parse(a.updated_at ?? '') || 0);
  return [...direct, ...rest];
}

export const OWNER: Participant = { id: 'owner', name: t('im.owner'), role: 'owner' };
export function subjectParticipant(projection: UiProjection): Participant {
  return { id: 'subject', name: projection.identity?.display_name || t('subject.name'), role: 'subject' };
}

// Durable admission stages, in IM words; each is a distinct fact
// (docs/specs/web-interface.md#spec-41-18), never a progress bar.
const RECEIPT_WORDS: Record<string, string> = {
  sending: t('im.sending'),
  sent: t('im.sent'),
  accepted: t('im.accepted'),
  selected: t('receipt.selected'),
  committed: t('im.committed'),
  conflict: t('receipt.conflict'),
  rejected: t('im.rejected'),
  unknown: t('im.unknown'),
};

/// The owner's direct chat with the subject. A subject message here is the
/// committed step that took one of the owner's messages (the receipt names
/// it); rounds the subject spent on its own are a system row that leads to
/// Inspect, not chat bubbles.
registerThreadSource({
  id: 'direct',
  order: 0,
  threads({ projection }) {
    const subject = subjectParticipant(projection);
    const timeline = projection.nodes.find((node) => node.kind === 'timeline');
    const entries = (timeline?.props.entries ?? []) as UiNode[];
    const submissions = entries.filter((entry) => entry.kind === 'user_submission');
    const replySteps = new Set(submissions
      .map((entry) => (entry.props.submission as Submission).selected_step_id)
      .filter((id): id is string => typeof id === 'string'));
    const history = timeline?.props.submission_history;
    const historyIncomplete = projection.source === 'live'
      && (!history || history.loading || history.error || history.has_older || history.stale === true);
    const messages: ThreadMessage[] = [];
    let quiet = 0;
    let quietKey = '';
    const flushQuiet = () => {
      if (quiet === 0) return;
      messages.push({
        key: `quiet-${quietKey}`, from: subject, kind: 'system',
        text: historyIncomplete ? t('delegation.quietIncomplete', { count: quiet }) : t('delegation.quiet', { name: subject.name, count: quiet }),
        link: { label: t('delegation.inspect'), target: 'inspect' },
      });
      quiet = 0;
    };
    for (const entry of entries) {
      if (entry.kind === 'user_submission') {
        const submission = entry.props.submission as Submission;
        flushQuiet();
        messages.push({
          key: entry.node_id, from: OWNER, kind: 'message', at: submission.created_at,
          text: submission.payload.content,
          tone: submission.payload.mode === 'steer' ? 'steer' : 'plain',
          tag: submission.payload.mode === 'steer' ? t('composer.steer') : undefined,
          meta: submission.withdrawal
            ? (submission.withdrawal.result === 'withdrawal_queued' ? t('receipt.withdrawalQueued') : t('receipt.withdrawn'))
            : RECEIPT_WORDS[submission.receipt] ?? submission.receipt,
          node: entry,
        });
      } else if (entry.kind === 'subject_part') {
        const part = entry.props.part as TimelinePart;
        if (part.committed && replySteps.has(part.step_id)) {
          flushQuiet();
          // observed_at is transport/replay arrival, not when the subject
          // wrote this message. No historical wall clock is supplied here.
          messages.push({ key: entry.node_id, from: subject, kind: 'message', text: part.content, node: entry });
        } else if (!part.committed && part.stream_id && !part.terminal) {
          // Real durable streamed text: the only "typing" the thread shows.
          messages.push({ key: entry.node_id, from: subject, kind: 'message', text: part.content, tone: 'pending', node: entry });
        } else {
          quiet += 1;
          quietKey = entry.node_id;
        }
      } else if (entry.kind === 'semantic_document') {
        messages.push({ key: entry.node_id, from: subject, kind: 'message', text: '', node: entry });
      }
    }
    flushQuiet();
    let last: ThreadMessage | undefined;
    let updatedAt: string | undefined;
    for (let index = messages.length - 1; index >= 0 && (!last || !updatedAt); index -= 1) {
      const message = messages[index];
      if (!last && message.kind === 'message' && message.text) last = message;
      if (!updatedAt && message.at) updatedAt = message.at;
    }
    return [{
      id: 'direct', kind: 'direct', audience: 'one_to_one', title: subject.name, subtitle: t('delegation.direct'),
      participants: [OWNER, subject], viewer: OWNER.id,
      preview: last ? last.text : t('im.previewEmpty'),
      updated_at: updatedAt, messages, composer: true,
    }];
  },
});

export interface TrajectoryResponse {
  task: TaskSummary;
  input?: unknown;
  /** The Store's text projection for this task, separate from its sealed input. */
  brief: string | null;
  children: TaskSummary[];
  operations: Array<{
    operation_id: string; operation_name: string; logical_key: string; state: unknown;
    outcome: unknown; attempts: number; record: Record<string, unknown> | null; record_status: string;
  }>;
}

export function isDelegation(task: { role?: unknown }): boolean {
  return task.role === 'delegation';
}

export function isAttentionTask(task: { role?: unknown; state?: unknown }): boolean {
  return task.role !== 'subject_root'
    && ['running', 'pending', 'waiting', 'queued'].includes(String(task.state));
}

export function shortId(id: string): string {
  return id.slice(0, 4);
}

export function childParticipant(taskId: string): Participant {
  return { id: `child:${taskId}`, name: t('delegation.child', { id: shortId(taskId) }), role: 'child' };
}

function briefOf(trajectory: TrajectoryResponse): string {
  return isDelegation(trajectory.task) && typeof trajectory.brief === 'string' && trajectory.brief.trim()
    ? trajectory.brief
    : t('delegation.briefUnavailable');
}

/// Turn one sealed trajectory into a delegation thread's messages.
export function trajectoryMessages(trajectory: TrajectoryResponse, subject: Participant): ThreadMessage[] {
  const child = childParticipant(trajectory.task.task_id);
  const messages: ThreadMessage[] = [{
    key: `brief-${trajectory.task.task_id}`, from: subject, kind: 'message', text: briefOf(trajectory),
    at: trajectory.task.created_at, tag: t('delegation.label'),
  }];
  const tools = trajectory.children.filter((task) => task.role === 'tool');
  for (const operation of trajectory.operations) {
    const view = recordViews.get(operation.operation_name);
    if (view) {
      messages.push(...view({ operation, subject, child, tools }));
    } else {
      messages.push({
        key: operation.operation_id, from: child, kind: 'system',
        text: `${operation.operation_name} · ${stateWord(operation.state)}`,
      });
    }
  }
  for (const tool of tools) {
    messages.push({ key: `tool-${tool.task_id}`, from: toolParticipant(tool.kind), kind: 'system', text: `${tool.kind} · ${tool.state}` });
  }
  for (const task of trajectory.children.filter((task) => task.role !== 'tool')) {
    messages.push({
      key: `task-${task.task_id}`, from: task.role === 'delegation' ? childParticipant(task.task_id) : subject,
      kind: 'system', text: task.role === 'unknown' ? t('delegation.unknownRole', { kind: task.kind, state: task.state }) : `${task.kind} · ${task.state}`,
    });
  }
  return messages;
}

function toolParticipant(kind: string): Participant {
  return { id: `tool:${kind}`, name: kind, role: 'tool' };
}

function stateWord(state: unknown): string {
  return typeof state === 'string' ? state : JSON.stringify(state);
}

type RecordView = (input: {
  operation: TrajectoryResponse['operations'][number];
  subject: Participant;
  child: Participant;
  /** Tool tasks not yet placed; a view consumes the ones its step started. */
  tools: TrajectoryResponse['children'];
}) => ThreadMessage[];

/// Record views are a keyed slot: one renderer per operation name. The shell
/// registers its own; a plugin registers its record's view the same way.
const recordViews = new Map<string, RecordView>();
export function registerRecordView(operationName: string, view: RecordView): () => void {
  recordViews.set(operationName, view);
  return () => { if (recordViews.get(operationName) === view) recordViews.delete(operationName); };
}

registerRecordView('model.child.generate.v1', ({ operation, subject, child, tools }) => {
  const record = operation.record;
  if (!record) {
    return [{ key: operation.operation_id, from: child, kind: 'message', tone: 'pending', text: '', meta: operation.record_status === 'unsettled' ? t('delegation.stepPending') : t('delegation.recordError', { status: operation.record_status }) }];
  }
  // Version 1 records already store the canonical parser's public text
  // separately from tool/reply/question controls. Other historical formats
  // stay inspectable through the API; raw output is never a display fallback.
  if (record.schema_version !== 1 || typeof record.text !== 'string') {
    return [{ key: `${operation.operation_id}-unavailable`, from: child, kind: 'system', text: t('delegation.textUnavailable') }];
  }
  const out: ThreadMessage[] = [];
  for (const [index, turn] of ((record.dialogue as Array<Record<string, unknown>> | undefined) ?? []).entries()) {
    out.push({ key: `${operation.operation_id}-dialogue-${index}`, from: subject, kind: 'message', text: String(turn.message ?? '') });
  }
  const text = record.text;
  const usage = record.usage as Record<string, unknown> | undefined;
  const meta = [t('delegation.step', { step: Number(record.step_index ?? 0) + 1 }), usage?.output_tokens != null ? `${usage.output_tokens} tokens` : null, record.text_truncated ? t('delegation.truncated') : null]
    .filter(Boolean).join(' · ');
  if (text) out.push({ key: `${operation.operation_id}-text`, from: child, kind: 'message', text, meta });
  if (typeof record.question === 'string' && record.question) {
    out.push({ key: `${operation.operation_id}-question`, from: child, kind: 'message', text: record.question, tag: t('delegation.question') });
  }
  const calls = (record.tool_calls as Array<Record<string, unknown>> | undefined) ?? [];
  for (const [index, call] of calls.entries()) {
    const tool = tools.shift();
    const name = String(call.kind ?? call.tool ?? tool?.kind ?? t('delegation.tool'));
    out.push({
      key: `${operation.operation_id}-tool-${index}`, from: toolParticipant(name), kind: 'system',
      text: t('delegation.call', { child: child.name, tool: name, state: tool ? ` · ${tool.state}` : '' }),
    });
  }
  if (!text && calls.length === 0 && !record.question) {
    out.push({ key: `${operation.operation_id}-empty`, from: child, kind: 'system', text: t('delegation.noOutput', { meta }) });
  }
  return out;
});

registerRecordView('model.child.result.v1', ({ operation, child }) => {
  const record = operation.record;
  const termination = record ? String(record.termination ?? record.outcome ?? '') : '';
  return [{
    key: operation.operation_id, from: child, kind: 'system',
    text: record ? t('delegation.ended', { child: child.name, termination: termination ? ` · ${termination}` : '' }) : t('delegation.unsealedResult', { child: child.name }),
  }];
});

/// Delegations: one group per model child the subject started.
registerThreadSource({
  id: 'delegations',
  order: 10,
  threads({ projection, tasks, demo }) {
    const subject = subjectParticipant(projection);
    return tasks.filter(isDelegation).map((task) => {
      const id = String(task.task_id);
      const child = childParticipant(id);
      return {
        id: `delegation:${id}`, kind: 'delegation', audience: 'group', title: t('delegation.groupTitle', { subject: subject.name, child: child.name }),
        subtitle: t('delegation.label'), participants: [subject, child], viewer: subject.id,
        preview: t('delegation.preview', { state: String(task.state) }), updated_at: String(task.created_at ?? ''),
        messages: demo ? trajectoryMessages(demo.trajectory(id), subject) : [],
        composer: false, state: String(task.state),
        note: t('delegation.readonly'),
        load: projection.source === 'demo' ? undefined : async () => {
          const response = await fetch(`/api/tasks/${encodeURIComponent(id)}/trajectory`, { cache: 'no-store' });
          if (!response.ok) throw new Error(`trajectory ${response.status}`);
          return trajectoryMessages(await response.json() as TrajectoryResponse, subject);
        },
      } satisfies Thread;
    });
  },
});

export interface ConversationRow {
  thread_key: string;
  thread_label?: string | null;
  thread_kind: string;
  /** The channel plugin's declared audience shape, when it stated one. */
  thread_shape?: ThreadAudience | null;
  channel: string;
  /** `own` proves account origin; `unknown` leaves that attribution unresolved. */
  direction: 'inbound' | 'outbound' | 'own' | 'unknown';
  message_id: string;
  sender_id?: string | null;
  sender_name?: string | null;
  /** This message's envelope recorded an owner-set grant, not current authority. */
  sender_admin?: boolean;
  text: string;
  at?: string | null;
  state?: string | null;
}

const PUBLICATION_WORDS: Record<string, string> = {
  ready: t('publication.ready'), claimed: t('receipt.sending'), acknowledged: t('publication.acknowledged'), settled: t('publication.settled'),
  blocked: t('publication.blocked'), unknown: t('publication.unknown'),
};

/// Every conversation the subject has with the outside, over any channel
/// plugin — IM platforms, a relay to another Shenko on the local network, an
/// embodied robot's channel — from its inbox envelopes and its committed
/// publications. One reply audience is one thread; a private chat is a
/// one-to-one thread when the plugin declared the audience shape.
registerThreadSource({
  id: 'channels',
  order: 20,
  threads({ projection, conversations }) {
    const subject = subjectParticipant(projection);
    const groups = new Map<string, ConversationRow[]>();
    for (const row of conversations) {
      const list = groups.get(row.thread_key) ?? [];
      list.push(row);
      groups.set(row.thread_key, list);
    }
    return [...groups.entries()].map(([key, rows]) => {
      const people = new Map<string, Participant>();
      const messages: ThreadMessage[] = rows.map((row) => {
        const grant = row.sender_admin === true
          ? t('channel.grant', { time: row.at || t('channel.timeUnknown') })
          : undefined;
        if (row.direction === 'outbound') {
          return {
            key: row.message_id, from: subject, kind: 'message', text: row.text, at: row.at ?? undefined,
            meta: row.state ? PUBLICATION_WORDS[row.state] ?? row.state : undefined,
          } satisfies ThreadMessage;
        }
        if (row.direction === 'own') {
          // The platform identified this account, not which client sent it.
          return {
            key: row.message_id, from: subject, kind: 'message', text: row.text, at: row.at ?? undefined,
            meta: [t('channel.own'), grant].filter(Boolean).join(' · '),
          } satisfies ThreadMessage;
        }
        const unknown = row.direction === 'unknown';
        const id = `${unknown ? 'source' : 'person'}:${row.sender_id ?? 'unknown'}`;
        const person: Participant = {
          id, name: row.sender_name || row.sender_id || t('channel.senderUnknown'), role: 'person',
        };
        if (!unknown) people.set(id, person);
        return {
          key: row.message_id, from: person, kind: 'message', text: row.text, at: row.at ?? undefined,
          meta: [unknown ? t('channel.originUnknown') : undefined, grant].filter(Boolean).join(' · ') || undefined,
        } satisfies ThreadMessage;
      });
      const label = rows.find((row) => row.thread_label)?.thread_label;
      const shape = rows.find((row) => row.thread_shape)?.thread_shape;
      const audience: ThreadAudience = shape === 'one_to_one' ? 'one_to_one' : 'group';
      const inbound = rows.find((row) => row.direction === 'inbound');
      const counterpart = inbound ? people.get(`person:${inbound.sender_id ?? 'unknown'}`) : undefined;
      const channel = rows[0].channel;
      const last = messages[messages.length - 1];
      return {
        id: `channel:${key}`, kind: 'channel', audience,
        title: (audience === 'one_to_one' && counterpart?.name) || label || `${channel} ${rows[0].thread_kind} ${key.split('|').pop()}`,
        subtitle: audience === 'one_to_one'
          ? t('channel.private', { channel })
          : t('channel.groupSubtitle', { channel, shape: t(shape === 'group' ? 'channel.group' : 'channel.shapeUnknown') }),
        participants: [subject, ...people.values()], viewer: subject.id,
        preview: last ? `${last.from.role === 'subject' || audience === 'one_to_one' ? '' : `${last.from.name}：`}${last.text}` : '',
        updated_at: [...messages].reverse().find((message) => message.at)?.at,
        messages, composer: false,
        note: t('channel.observe', { subject: subject.name, channel }),
      } satisfies Thread;
    });
  },
});
