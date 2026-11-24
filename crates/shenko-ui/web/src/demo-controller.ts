import { message as m } from './messages';
import { DEMO_TEAM, demoThreadData } from './demo-fixtures';
export { demoThreadData };
import {
  UI_INTENT_SCHEMA,
  buildProjection,
  type DegradedDocument,
  type DisplayState,
  type Submission,
  type ProjectionController,
  type TimelinePart,
  type UiIntent,
  type UiProjection,
  type UiState,
  type ViewName,
} from './projection';

function demoPart(step: number, minutesAgo: number, content: string): TimelinePart {
  const stepId = `demo-step-${step}`;
  return {
    role: 'subject',
    content,
    message_id: `demo-message-${step}`,
    part_id: `demo-part-${step}-0`,
    part_revision: `demo-revision-${step}`,
    next_chunk_seq: 1,
    committed: true,
    step,
    step_id: stepId,
    commit_ref: `demo/${step}`,
    observed_at: new Date(Date.now() - minutesAgo * 60_000).toISOString(),
    stream_id: 'demo-stream',
    request_id: `demo-request-${step}`,
    attempt_id: 'demo-attempt-1',
    gap: false,
  };
}

function demoSubmission(id: string, minutesAgo: number, content: string, receipt: Submission['receipt'], mode: 'append' | 'steer' = 'append', selectedStep?: string): Submission {
  return {
    payload: {
      schema_version: 2,
      type: 'submit',
      submission_id: id,
      message_id: `${id}-message`,
      subject_id: 'demo-subject',
      mode,
      content,
      attachment_refs: [],
    },
    created_at: new Date(Date.now() - minutesAgo * 60_000).toISOString(),
    receipt,
    placement: receipt === 'committed' ? { requested: mode, actual: 'next_boundary' } : undefined,
    selected_step_id: selectedStep,
    send_epoch: 1,
  };
}

const demoDocument: DegradedDocument = {
  mode: 'card',
  document_id: 'demo-document-1',
  revision: 'demo-r1',
  summary: '记忆合并有两种做法，选一个我就继续：',
  source_step_id: 'demo-step-44',
  options: [
    { index: 1, action_id: 'merge-keep-sources', nonce: 'n1', revision: 'demo-r1', label: '合成一条，保留两次观察的来源', required_capabilities: [], payload: {} },
    { index: 2, action_id: 'merge-later', nonce: 'n2', revision: 'demo-r1', label: '先不合并，等下一次部署再看', required_capabilities: [], payload: {} },
  ],
};

function demoState(): UiState {
  const query = new URLSearchParams(location.search);
  const view = query.get("view");
  const display: DisplayState = {
    view: (['tasks', 'memory', 'config', 'playground'].includes(view ?? '') ? view : 'timeline') as ViewName,
    panel_open: query.get('panel') === '1',
    open_sections: { status: true, activity: true, artifacts: false, devices: false },
    theme: query.get('theme') === 'light' ? 'light' : 'dark',
    reading: 'following_latest',
    new_count: 0,
    scroll_request: 0,
    wrap: false,
  };
  return {
    source: 'demo',
    connection: 'offline',
    reconnect_seconds: 0,
    server_version: 'demo-only',
    capabilities: {
      append: false,
      steer: false,
      withdraw: false,
      actions: false,
      documents: false,
      attachments: false,
      durable_input_receipts: false,
      committed_replay: false,
      streaming: false,
    },
    append_available: false,
    durable_receipts: false,
    committed_replay: false,
    streaming: false,
    documents: [demoDocument],
    document_errors: [],
    action_receipts: [],
    subject_id: 'demo-subject',
    identity: {
      agent_id: 'demo-agent',
      revision: 1,
      display_name: '示例主体',
      naming: 'demo',
    },
    wanted_identity_revision: null,
    step: 44,
    timeline: [
      demoPart(
        42,
        18,
        '昨天留下的那条线索值得继续：记忆里的部署顺序和今天早上的日志对不上。日志显示三次重试都发生在配置加载之前，而不是模型调用阶段。\n\n如果这个判断成立，问题不在推理容量，而在启动顺序。先查配置同步的时序，再决定是否改动 admission 窗口。',
      ),
      demoPart(
        43,
        9,
        '配置时序已经确认：数据目录在首次请求之后才解析，重试因此读到了默认值。修复不需要新的抽象，只要把目录解析提前到服务启动的同一段代码里。\n\n```rust\nlet store = Store::open(path)?;\nlet identity = adopt_identity(&store)?;\n```\n\n我把这个结论记成一条临时判断，等下一次部署验证后，再决定是否写进长期记忆。',
      ),
      demoPart(
        44,
        2,
        '验证结果符合预期，重试次数归零。仍然有一个未解释的细节：第二次启动比第一次慢 300ms。可能只是冷缓存，不值得现在就追。\n\n接下来回到昨天未完成的记忆合并：保留两次观察的来源，合成一条更小的条目。',
      ),
    ],
    submissions: [
      demoSubmission('demo-sub-1', 12, '部署那边的重试，是不是和昨天说的配置顺序有关？', 'committed', 'append', 'demo-step-43'),
      demoSubmission('demo-sub-2', 4, '冷缓存先别追了，先把记忆合并做完吧', 'committed', 'steer', 'demo-step-44'),
    ],
    draft: '',
    attachments: [],
    config_error: false,
    self_error: false,
    tasks: {
      available: true,
      items: [
        { task_id: 'demo-subject-root', kind: 'subject.main', role: 'subject_root', state: 'running', name: 'Shenko', goal: '持续处理当前主体的任务。', created_at: new Date(Date.now() - 30 * 60_000).toISOString() },
        { task_id: 'demo-task-7f3a', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'running', goal: '合并两条启动时序观察，保留来源。', created_at: new Date(Date.now() - 3 * 60_000).toISOString() },
        { task_id: '3f2a91c0-demo-child', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'running', name: '整理员', goal: '把两条关于启动时序的观察合成一条更小的记忆，保留各自来源。', created_by_step_id: 'demo-step-44', created_at: new Date(Date.now() - 1 * 60_000).toISOString() },
        { task_id: 'demo-tool-read', parent_task_id: '3f2a91c0-demo-child', kind: 'fs.read', role: 'tool', state: 'terminal', name: '读取观察记录', goal: '读取带有来源的两条观察。', created_at: new Date(Date.now() - 45_000).toISOString() },
        ...DEMO_TEAM.members.map((member) => ({
          task_id: member.task_id, parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation' as const, state: 'terminal', created_by_step_id: DEMO_TEAM.step, goal: member.goal,
          created_at: new Date(Date.now() - 5.5 * 60_000).toISOString(), name: member.name, team: DEMO_TEAM.team,
        })),
        { task_id: 'demo-task-6c12', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'terminal', goal: '验证配置读取发生在目录解析之后。', created_at: new Date(Date.now() - 11 * 60_000).toISOString() },
        // Dormant historical subagents from weeks & months ago (demonstrates time-horizon archival).
        { task_id: 'demo-arch-18d', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'terminal', name: '飞书回调排查', goal: '梳理飞书 Webhook 签名校验与重复投递去重策略。', created_at: new Date(Date.now() - 18 * 86400_000).toISOString() },
        { task_id: 'demo-arch-64d', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'terminal', name: 'LayerFS 快照对照', goal: '对比 Linux VM 原生 LayerFS 与 WASM 家园快照恢复耗时。', created_at: new Date(Date.now() - 64 * 86400_000).toISOString() },
        { task_id: 'demo-arch-96d', parent_task_id: 'demo-subject-root', kind: 'model.child', role: 'delegation', state: 'terminal', name: 'Qwen3.8 量化基线', goal: '测量 4 路 128K KV 并发下 Q4_K_M 与 FP8 的吞吐差异。', created_at: new Date(Date.now() - 96 * 86400_000).toISOString() },
      ],
    },
    tasks_error: false,
    memory: {
      items: [
        { id: 'demo-mem-1', kind: 'observation', content: '服务启动时数据目录在首次请求之后才解析，导致重试读到默认值。', created_at: new Date(Date.now() - 9 * 60_000).toISOString() },
        { id: 'demo-mem-2', kind: 'tentative', content: '第二次启动比第一次慢约 300ms，可能是冷缓存；暂不追查。', created_at: new Date(Date.now() - 2 * 60_000).toISOString() },
      ],
    },
    memory_error: false,
    playground: {
      available: true,
      artifacts: [
        { artifact_id: 'demo-work-1', title: '启动时序对照表', mime: 'text/html', revision: 3, active_manifest_digest: 'demo-digest-1', backend: true, lifecycle: 'running', updated_at: new Date(Date.now() - 7 * 60_000).toISOString() },
        { artifact_id: 'demo-work-4', title: '27B KV 并发水位', mime: 'text/html', revision: 5, active_manifest_digest: 'demo-digest-4', backend: true, lifecycle: 'running', updated_at: new Date(Date.now() - 125 * 60_000).toISOString() },
        { artifact_id: 'demo-work-3', title: '雨夜与显影便签', mime: 'text/html', revision: 2, active_manifest_digest: 'demo-digest-3', lifecycle: 'running', updated_at: new Date(Date.now() - 22 * 60_000).toISOString() },
        { artifact_id: 'demo-work-5', title: '桌角待读 · 3 篇', mime: 'text/html', revision: 1, active_manifest_digest: 'demo-digest-5', lifecycle: 'running', updated_at: new Date(Date.now() - 41 * 60_000).toISOString() },
        { artifact_id: 'demo-work-2', title: '记忆合并草稿', mime: 'text/markdown', revision: 1, lifecycle: 'draft', updated_at: new Date(Date.now() - 2 * 60_000).toISOString() },
        { artifact_id: 'demo-work-6', title: '周五给林以压一页分享', mime: 'text/html', revision: 1, active_manifest_digest: 'demo-digest-6', lifecycle: 'running', updated_at: new Date(Date.now() - 14 * 60_000).toISOString() },
        // Historical archived works (folded under Earlier archive by default).
        { artifact_id: 'demo-work-arch-1', title: '东京旧书店路线摘记', mime: 'text/html', revision: 4, active_manifest_digest: 'demo-digest-arch-1', lifecycle: 'archived', updated_at: new Date(Date.now() - 52 * 86400_000).toISOString() },
        { artifact_id: 'demo-work-arch-2', title: 'LayerFS 压测小表', mime: 'text/markdown', revision: 2, lifecycle: 'archived', updated_at: new Date(Date.now() - 88 * 86400_000).toISOString() },
      ],
    },
    playground_error: false,
    display,
  };
}

export function createDemoController(): ProjectionController {
  return new DemoController();
}

class DemoController implements ProjectionController {
  private state = demoState();
  private revision = 0;
  private cached: UiProjection;
  private listeners = new Set<() => void>();
  private disposed = false;

  constructor() {
    document.documentElement.dataset.theme = this.state.display.theme;
    document.documentElement.classList.toggle('dark', this.state.display.theme === 'dark');
    document.documentElement.classList.toggle('light', this.state.display.theme === 'light');
    this.cached = buildProjection(this.state, ++this.revision);
  }

  projection(): UiProjection {
    return this.cached;
  }

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  private emit(): void {
    this.cached = buildProjection(this.state, ++this.revision);
    this.listeners.forEach((listener) => listener());
  }

  dispatch(intent: UiIntent): void {
    if (this.disposed) return;
    const action = this.cached.actions[intent.action_id];
    if (
      intent.schema_version !== UI_INTENT_SCHEMA ||
      intent.surface_id !== this.cached.surface_id ||
      !action
    ) return;
    if (action.revision !== intent.action_revision) {
      this.state.display.notice = m('demo.staleAction');
      this.emit();
      return;
    }
    if (!action.enabled) return;
    switch (action.kind) {
      case 'view.select':
        if (['timeline', 'tasks', 'memory', 'config', 'playground'].includes(String(intent.payload.view))) {
          this.state.display.view = intent.payload.view as ViewName;
          this.state.display.panel_open = false;
          this.emit();
        }
        break;
      case 'panel.toggle':
        this.state.display.panel_open = !this.state.display.panel_open;
        this.emit();
        break;
      case 'theme.toggle':
        this.state.display.theme = this.state.display.theme === 'dark' ? 'light' : 'dark';
        document.documentElement.dataset.theme = this.state.display.theme;
    document.documentElement.classList.toggle('dark', this.state.display.theme === 'dark');
    document.documentElement.classList.toggle('light', this.state.display.theme === 'light');
        this.emit();
        break;
      case 'section.toggle': {
        const section = String(intent.payload.section ?? '');
        if (Object.hasOwn(this.state.display.open_sections, section)) {
          this.state.display.open_sections[section] = !this.state.display.open_sections[section];
          this.emit();
        }
        break;
      }
      case 'timeline.reading':
        this.state.display.reading = intent.payload.at_end === true
          ? 'following_latest'
          : intent.payload.state === 'selecting_or_interacting'
            ? 'selecting_or_interacting'
            : 'reading_history';
        if (this.state.display.reading === 'following_latest') this.state.display.new_count = 0;
        this.emit();
        break;
      case 'timeline.latest':
        this.state.display.reading = 'following_latest';
        this.state.display.new_count = 0;
        this.state.display.scroll_request += 1;
        this.emit();
        break;
      case 'timeline.code_wrap':
        this.state.display.wrap = !this.state.display.wrap;
        this.state.display.notice = this.state.display.wrap
          ? m('demo.wrapEnabled')
          : m('demo.wrapDisabled');
        this.emit();
        break;
      case 'part.copy': {
        const content = String(action.payload?.content ?? '');
        void navigator.clipboard?.writeText(content).then(
          () => {
            this.state.display.notice = m('demo.copied');
            this.emit();
          },
          () => {
            this.state.display.notice = m('copy.unavailable');
            this.emit();
          },
        );
        break;
      }
    }
  }

  dispose(): void {
    this.disposed = true;
    this.listeners.clear();
  }
}
