import {
  UI_INTENT_SCHEMA,
  buildProjection,
  type DisplayState,
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

function demoState(): UiState {
  const query = new URLSearchParams(location.search);
  const display: DisplayState = {
    view: 'timeline',
    panel_open: query.get('panel') === '1',
    open_sections: { status: true, activity: true, artifacts: false, devices: false },
    theme: query.get('theme') === 'light' ? 'light' : 'dark',
    reading: 'following_latest',
    new_count: 0,
    scroll_request: 0,
  };
  return {
    source: 'demo',
    connection: 'offline',
    reconnect_seconds: 0,
    server_version: 'demo-only',
    append_available: false,
    durable_receipts: false,
    committed_replay: false,
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
    submissions: [],
    draft: '',
    config_error: false,
    self_error: false,
    tasks: { available: true, items: [], note: '模拟投影不包含真实任务。' },
    tasks_error: false,
    memory: { items: [], note: '模拟投影不读取真实记忆。' },
    memory_error: false,
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
      this.state.display.notice = '模拟视图已变化，旧动作已拒绝。';
      this.emit();
      return;
    }
    if (!action.enabled) return;
    switch (action.kind) {
      case 'view.select':
        if (['timeline', 'tasks', 'memory', 'config'].includes(String(intent.payload.view))) {
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
      case 'part.copy': {
        const content = String(action.payload?.content ?? '');
        void navigator.clipboard?.writeText(content).then(
          () => {
            this.state.display.notice = '已从模拟投影复制';
            this.emit();
          },
          () => {
            this.state.display.notice = '复制不可用；请手动选择文本。';
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
