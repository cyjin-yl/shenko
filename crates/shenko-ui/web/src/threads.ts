import type { Submission, TimelinePart, UiNode, UiProjection } from './projection';

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

export interface Thread {
  id: string;
  kind: ThreadKind;
  title: string;
  /** Who is in the thread, the viewer's side first. */
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

export interface ThreadInputs {
  projection: UiProjection;
  tasks: Array<Record<string, unknown>>;
  conversations: ConversationRow[];
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
  const all = [...sources.values()].sort((a, b) => a.order - b.order).flatMap((source) => source.threads(inputs));
  const direct = all.filter((thread) => thread.kind === 'direct');
  const rest = all.filter((thread) => thread.kind !== 'direct')
    .sort((a, b) => Date.parse(b.updated_at ?? '') - Date.parse(a.updated_at ?? '') || 0);
  return [...direct, ...rest];
}

export const OWNER: Participant = { id: 'owner', name: '你', role: 'owner' };
export function subjectParticipant(projection: UiProjection): Participant {
  return { id: 'subject', name: projection.identity?.display_name || '主体', role: 'subject' };
}

// Durable admission stages, in IM words; each is a distinct fact
// (docs/specs/web-interface.md#spec-41-18), never a progress bar.
const RECEIPT_WORDS: Record<string, string> = {
  sending: '发送中…',
  sent: '已发送',
  accepted: '已送达',
  selected: '已选入',
  committed: '已处理',
  conflict: 'ID 冲突',
  rejected: '未送达',
  unknown: '送达未知',
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
      && (!history || history.loading || history.error || history.has_older);
    const messages: ThreadMessage[] = [];
    let quiet = 0;
    let quietKey = '';
    const flushQuiet = () => {
      if (quiet === 0) return;
      messages.push({
        key: `quiet-${quietKey}`, from: subject, kind: 'system',
        text: historyIncomplete ? `其他 ${quiet} 轮（输入历史尚未全部读取）` : `${subject.name} 独自思考了 ${quiet} 轮`,
        link: { label: '在 Inspect 查看', target: 'inspect' },
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
          tag: submission.payload.mode === 'steer' ? '引导' : undefined,
          meta: submission.withdrawal
            ? (submission.withdrawal.result === 'withdrawal_queued' ? '撤回已排队' : '已撤回')
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
      id: 'direct', kind: 'direct', title: subject.name,
      participants: [OWNER, subject], viewer: OWNER.id,
      preview: last ? last.text : '还没有消息',
      updated_at: updatedAt, messages, composer: true,
    }];
  },
});

export interface TrajectoryResponse {
  task: { task_id: string; kind: string; state: string; created_at: string; created_by_step_id?: string | null };
  input?: Record<string, unknown> | null;
  children: Array<{ task_id: string; kind: string; state: string; created_at: string }>;
  operations: Array<{
    operation_id: string; operation_name: string; logical_key: string; state: unknown;
    outcome: unknown; attempts: number; record: Record<string, unknown> | null; record_status: string;
  }>;
}

const DELEGATION_KINDS = new Set(['model.child']);

export function isDelegation(task: Record<string, unknown>): boolean {
  return DELEGATION_KINDS.has(String(task.kind));
}

export function shortId(id: string): string {
  return id.slice(0, 4);
}

export function childParticipant(taskId: string): Participant {
  return { id: `child:${taskId}`, name: `子代理 ${shortId(taskId)}`, role: 'child' };
}

function briefOf(input: Record<string, unknown> | null | undefined): string {
  if (!input) return '（委派内容未读取）';
  for (const key of ['goal', 'prompt', 'task', 'instruction']) {
    const value = input[key];
    if (typeof value === 'string' && value.trim()) return value;
  }
  const spec = input.spec as Record<string, unknown> | undefined;
  if (spec && typeof spec.goal === 'string') return spec.goal;
  return JSON.stringify(input).slice(0, 400);
}

/// Turn one sealed trajectory into a delegation thread's messages.
export function trajectoryMessages(trajectory: TrajectoryResponse, subject: Participant): ThreadMessage[] {
  const child = childParticipant(trajectory.task.task_id);
  const messages: ThreadMessage[] = [{
    key: `brief-${trajectory.task.task_id}`, from: subject, kind: 'message', text: briefOf(trajectory.input),
    at: trajectory.task.created_at, tag: '委派',
  }];
  const tools = [...trajectory.children];
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

const CONTROL_LINE = /^\s*\[(tool|reply|question|delegate)[^\]]*\].*$/gm;

registerRecordView('model.child.generate.v1', ({ operation, subject, child, tools }) => {
  const record = operation.record;
  if (!record) {
    return [{ key: operation.operation_id, from: child, kind: 'message', tone: 'pending', text: '', meta: operation.record_status === 'unsettled' ? '这一步还在进行' : `记录不可读：${operation.record_status}` }];
  }
  const out: ThreadMessage[] = [];
  for (const [index, turn] of ((record.dialogue as Array<Record<string, unknown>> | undefined) ?? []).entries()) {
    out.push({ key: `${operation.operation_id}-dialogue-${index}`, from: subject, kind: 'message', text: String(turn.message ?? '') });
  }
  const text = String(record.text ?? '').replace(CONTROL_LINE, '').trim();
  const usage = record.usage as Record<string, unknown> | undefined;
  const meta = [`第 ${Number(record.step_index ?? 0) + 1} 步`, usage?.output_tokens != null ? `${usage.output_tokens} tokens` : null, record.text_truncated ? '正文截断' : null]
    .filter(Boolean).join(' · ');
  if (text) out.push({ key: `${operation.operation_id}-text`, from: child, kind: 'message', text, meta });
  if (typeof record.question === 'string' && record.question) {
    out.push({ key: `${operation.operation_id}-question`, from: child, kind: 'message', text: record.question, tag: '提问' });
  }
  const calls = (record.tool_calls as Array<Record<string, unknown>> | undefined) ?? [];
  for (const [index, call] of calls.entries()) {
    const tool = tools.shift();
    const name = String(call.kind ?? call.tool ?? tool?.kind ?? '工具');
    out.push({
      key: `${operation.operation_id}-tool-${index}`, from: toolParticipant(name), kind: 'system',
      text: `${child.name} 调用 ${name}${tool ? ` · ${tool.state}` : ''}`,
    });
  }
  if (!text && calls.length === 0 && !record.question) {
    out.push({ key: `${operation.operation_id}-empty`, from: child, kind: 'system', text: `${meta} · 没有文字输出` });
  }
  return out;
});

registerRecordView('model.child.result.v1', ({ operation, child }) => {
  const record = operation.record;
  const termination = record ? String(record.termination ?? record.outcome ?? '') : '';
  return [{
    key: operation.operation_id, from: child, kind: 'system',
    text: record ? `${child.name} 结束${termination ? ` · ${termination}` : ''}` : `${child.name} 的结果尚未封存`,
  }];
});

/// Delegations: one group per model child the subject started.
registerThreadSource({
  id: 'delegations',
  order: 10,
  threads({ projection, tasks }) {
    const subject = subjectParticipant(projection);
    return tasks.filter(isDelegation).map((task) => {
      const id = String(task.task_id);
      const child = childParticipant(id);
      return {
        id: `delegation:${id}`, kind: 'delegation', title: `${subject.name} 与 ${child.name}`,
        participants: [subject, child], viewer: subject.id,
        preview: `委派 · ${String(task.state)}`, updated_at: String(task.created_at ?? ''),
        messages: projection.source === 'demo' ? trajectoryMessages(demoTrajectory(id), subject) : [],
        composer: false, state: String(task.state),
        note: '旁观中 · 这是主体和它委派的子代理之间的对话',
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
  channel: string;
  direction: 'inbound' | 'outbound';
  message_id: string;
  sender_id?: string | null;
  sender_name?: string | null;
  text: string;
  at?: string | null;
  state?: string | null;
}

const PUBLICATION_WORDS: Record<string, string> = {
  ready: '待发送', claimed: '发送中', acknowledged: '已确认', settled: '已结束',
  blocked: '未发送', unknown: '发送结果未知',
};

/// Channel conversations — groups and direct chats on other platforms — from
/// the subject's inbox envelopes and its committed publications.
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
        if (row.direction === 'outbound') {
          return {
            key: row.message_id, from: subject, kind: 'message', text: row.text, at: row.at ?? undefined,
            meta: row.state ? PUBLICATION_WORDS[row.state] ?? row.state : undefined,
          } satisfies ThreadMessage;
        }
        const id = `person:${row.sender_id ?? 'unknown'}`;
        const person = people.get(id) ?? { id, name: row.sender_name || row.sender_id || '未知成员', role: 'person' as const };
        people.set(id, person);
        return { key: row.message_id, from: person, kind: 'message', text: row.text, at: row.at ?? undefined } satisfies ThreadMessage;
      });
      const label = rows.find((row) => row.thread_label)?.thread_label;
      const last = messages[messages.length - 1];
      return {
        id: `channel:${key}`, kind: 'channel',
        title: label || `${rows[0].channel} ${rows[0].thread_kind} ${key.split('|').pop()}`,
        participants: [subject, ...people.values()], viewer: subject.id,
        preview: last ? `${last.from.role === 'subject' ? '' : `${last.from.name}：`}${last.text}` : '',
        updated_at: [...messages].reverse().find((message) => message.at)?.at,
        messages, composer: false,
        note: `旁观中 · ${subject.name} 在 ${rows[0].channel} 的会话；网页暂不能代它发言`,
      } satisfies Thread;
    });
  },
});

/// The simulated projection's one delegation, so `?demo=1` can show a group
/// thread; it is never used for a live subject.
export function demoTrajectory(taskId: string): TrajectoryResponse {
  const at = new Date(Date.now() - 60_000).toISOString();
  const step = (index: number, record: Record<string, unknown>) => ({
    operation_id: `${taskId}-step-${index}`, operation_name: 'model.child.generate.v1', logical_key: `step/${index}`,
    state: 'resolved', outcome: 'succeeded', attempts: 1, record_status: 'sealed',
    record: { step_index: index, ...record },
  });
  return {
    task: { task_id: taskId, kind: 'model.child', state: 'running', created_at: at },
    input: { goal: '把两条关于启动时序的观察合成一条更小的记忆，保留各自来源。' },
    children: [{ task_id: `${taskId}-tool`, kind: 'memory.recall', state: 'succeeded', created_at: at }],
    operations: [
      step(0, { text: '先找出两条观察的原文和来源。', tool_calls: [{ kind: 'memory.recall' }], usage: { output_tokens: 42 } }),
      step(1, { text: '两条都来自 11 月 12 日的部署日志。合并后保留两个来源引用；冷缓存那条仍标为暂定。', question: '要把“暂定”这条也并进去吗？', usage: { output_tokens: 88 } }),
    ],
  };
}
