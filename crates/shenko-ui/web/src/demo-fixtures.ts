import type { FileEntry, FileRoot, FileSource } from './files-view';
import type { ConversationRow, DemoThreadData, DemoWidgetPreview, TrajectoryResponse } from './threads';

const demoConversations: ConversationRow[] = (() => {
  const ago = (minutes: number) => new Date(Date.now() - minutes * 60_000).toISOString();
  const group = { thread_key: 'qq:demo|group|10086', thread_label: '群 · 本地模型研究', thread_kind: 'group', thread_shape: 'group', channel: 'napcat' } as const;
  const p2p = { thread_key: 'feishu:demo|chat|oc_demo', thread_label: '单聊 ou_demo', thread_kind: 'chat', thread_shape: 'one_to_one', channel: 'feishu' } as const;
  return [
    { ...group, direction: 'inbound', message_id: 'dc1', sender_id: 'a', sender_name: '阿青', text: '有人试过 27B 在 4 路 128K 下的吞吐吗', at: ago(50) },
    { ...group, direction: 'inbound', message_id: 'dc2', sender_id: 'b', sender_name: 'Mo', text: '我这边 KV 一满就掉得厉害', at: ago(48) },
    { ...group, direction: 'outbound', message_id: 'dc3', text: '我们这边是自动分 KV，负载对半的时候会落到 50/50。可以先看看是不是启动顺序导致的重试。', at: ago(47), state: 'done' },
    { ...p2p, direction: 'inbound', message_id: 'dp1', sender_id: 'ou_demo', sender_name: '林以', text: '周五的分享你那边能给个十分钟的版本吗？', at: ago(30) },
    { ...p2p, direction: 'outbound', message_id: 'dp2', text: '可以。我先把三个结论压成一页，晚上发你看。', at: ago(28), state: 'done' },
  ];
})();

/// The simulated projection's one delegation, so `?demo=1` can show a group
/// thread; it is never used for a live subject.
export const DEMO_TEAM = { step: 'demo-step-43', team: '核对组', members: [
  { task_id: '7c1e0b22-demo-team-a', name: '甲', goal: '检查 alpha 数据集的缺失值。' },
  { task_id: '9d4f3a17-demo-team-b', name: '乙', goal: '复核甲的检查结果。' },
] } as const;

function demoTeamTrajectory(member: (typeof DEMO_TEAM.members)[number]): TrajectoryResponse {
  const ago = (seconds: number) => new Date(Date.now() - seconds * 1000).toISOString();
  const step = (index: number, seconds: number, record: Record<string, unknown>) => ({
    operation_id: `${member.task_id}-step-${index}`, operation_name: 'model.child.generate.v1', logical_key: `step/${index}`,
    state: 'resolved', outcome: 'succeeded', attempts: 1, record_status: 'sealed', observed_at: ago(seconds),
    record: { schema_version: 1, step_index: index, ...record },
  });
  const operations = member.name === '甲'
    ? [
      step(0, 300, { text: '先扫一遍每列的缺失率。', tells: [{ to: '乙', message: 'alpha 第 3 列缺失 12%，其余都在 1% 以下。' }], wait: '乙的复核' }),
      step(1, 150, { text: '乙复核一致。结论：补齐第 3 列后再进下一步。' }),
    ]
    : [
      step(0, 320, { text: '', wait: '甲的检查结果' }),
      step(1, 200, { text: '我抽样复算了第 3 列。', tells: [{ to: '甲', message: '复核一致：第 3 列 12%，建议按中位数补齐。' }] }),
    ];
  return {
    task: { task_id: member.task_id, kind: 'model.child', role: 'delegation', state: 'terminal', created_at: ago(330),
      created_by_step_id: DEMO_TEAM.step, name: member.name, team: DEMO_TEAM.team },
    input: null, brief: member.goal, children: [], operations,
  };
}

const DEMO_ARCHIVED: Record<string, { name: string; goal: string; daysAgo: number; steps: Array<{ text: string; tokens: number }> }> = {
  'demo-arch-18d': {
    name: '飞书回调排查',
    goal: '梳理飞书 Webhook 签名校验与重复投递去重策略。',
    daysAgo: 18,
    steps: [
      { text: '核对了飞书 v2 回调的 `X-Lark-Signature` 拼接顺序：timestamp + nonce + encrypt_key + raw_body。', tokens: 64 },
      { text: '已确认重复投递应以 `header.event_id` 做幂等键，保留 24 小时去重窗口，避免重放触发二次入队。', tokens: 78 },
    ],
  },
  'demo-arch-64d': {
    name: 'LayerFS 快照对照',
    goal: '对比 Linux VM 原生 LayerFS 与 WASM 家园快照恢复耗时。',
    daysAgo: 64,
    steps: [
      { text: '实测 128 MB 工作目录：WASM 家园冷恢复 42 ms，Overlay/LayerFS 挂载切换 19 ms。', tokens: 71 },
      { text: '结论：日常小文件工作区保持 WASM 家园，大体积模型权重通过只读挂载接入更省 I/O。', tokens: 66 },
    ],
  },
  'demo-arch-96d': {
    name: 'Qwen3.8 量化基线',
    goal: '测量 4 路 128K KV 并发下 Q4_K_M 与 FP8 的吞吐差异。',
    daysAgo: 96,
    steps: [
      { text: '在 4 路 128K 满载下，Q4_K_M 显存占用降低 38%，首字延迟几乎持平；FP8 在长代码生成上更稳。', tokens: 82 },
      { text: '建议单卡日常默认 Q4_K_M，高并发批处理节点保留 FP8 配置。', tokens: 59 },
    ],
  },
};

export function demoTrajectory(taskId: string): TrajectoryResponse {
  const member = DEMO_TEAM.members.find((item) => item.task_id === taskId);
  if (member) return demoTeamTrajectory(member);
  const archived = DEMO_ARCHIVED[taskId];
  if (archived) {
    const baseMs = Date.now() - archived.daysAgo * 86400_000;
    return {
      task: {
        task_id: taskId, kind: 'model.child', role: 'delegation', state: 'terminal',
        created_at: new Date(baseMs).toISOString(), name: archived.name,
      },
      input: { goal: archived.goal },
      brief: archived.goal,
      children: [],
      operations: archived.steps.map((entry, index) => ({
        operation_id: `${taskId}-step-${index}`,
        operation_name: 'model.child.generate.v1',
        logical_key: `step/${index}`,
        state: 'resolved',
        outcome: 'succeeded',
        attempts: 1,
        record_status: 'sealed',
        observed_at: new Date(baseMs + (index + 1) * 45_000).toISOString(),
        record: { schema_version: 1, step_index: index, text: entry.text, usage: { output_tokens: entry.tokens } },
      })),
    };
  }
  const at = new Date(Date.now() - 60_000).toISOString();
  const goal = '把两条关于启动时序的观察合成一条更小的记忆，保留各自来源。';
  const step = (index: number, record: Record<string, unknown>) => ({
    operation_id: `${taskId}-step-${index}`, operation_name: 'model.child.generate.v1', logical_key: `step/${index}`,
    state: 'resolved', outcome: 'succeeded', attempts: 1, record_status: 'sealed',
    record: { schema_version: 1, step_index: index, ...record },
  });
  return {
    task: { task_id: taskId, kind: 'model.child', role: 'delegation', state: 'running', created_at: at, name: taskId.startsWith('3f2a') ? '整理员' : null },
    input: { goal }, brief: goal,
    children: [{ task_id: `${taskId}-tool`, kind: 'memory.recall', role: 'tool', state: 'succeeded', created_at: at }],
    operations: [
      step(0, { text: '先找出两条观察的原文和来源。', tool_calls: [{ kind: 'memory.recall' }], usage: { output_tokens: 42 } }),
      step(1, { text: '两条都来自 11 月 12 日的部署日志。合并后保留两个来源引用；冷缓存那条仍标为暂定。', question: '要把“暂定”这条也并进去吗？', usage: { output_tokens: 88 } }),
    ],
  };
}

const secondsAgo = (seconds: number) => new Date(Date.now() - seconds * 1000).toISOString();
const DEMO_ROOTS: FileRoot[] = [
  { id: 'mount:/home', label: 'home', kind: 'home', protocol: 'wasm_home', guest_path: '/home', mode: 'read_write', writable: true, browsable: true, note: null },
  { id: 'mount:/memory', label: 'memory', kind: 'mount', protocol: 'local', guest_path: '/memory', mode: 'read_write', writable: true, browsable: true, note: null },
  { id: 'mount:/nas', label: 'nas', kind: 'mount', protocol: 'smb', guest_path: '/nas', mode: 'read_only', writable: false, browsable: true, note: null },
  { id: 'mount:/lab', label: 'lab', kind: 'mount', protocol: 'sshfs', guest_path: '/lab', mode: 'copy_on_write', writable: true, browsable: true, note: null },
  { id: 'mount:/mac', label: 'mac', kind: 'mount', protocol: 'shared_folder', guest_path: '/mac', mode: 'read_only', writable: false, browsable: true, note: null },
  { id: 'node:1cat', label: '1cat · home', kind: 'node', protocol: 'ssh', guest_path: '/srv/shenko/home', mode: null, writable: false, browsable: false, note: '示例节点：浏览其他节点的文件尚未接通' },
];
const DEMO_TREE: Record<string, Array<Omit<FileEntry, 'id'>>> = {
  'mount:/home:': [
    { name: 'notes', kind: 'dir', bytes: 0, modified: secondsAgo(3600) },
    { name: 'projects', kind: 'dir', bytes: 0, modified: secondsAgo(7200) },
    { name: 'startup-order.md', kind: 'file', bytes: 2148, modified: secondsAgo(600) },
    { name: 'merge-plan.json', kind: 'file', bytes: 734, modified: secondsAgo(320) },
  ],
  'mount:/home:6e6f746573': [
    { name: '2026-11-12-deploy.md', kind: 'file', bytes: 5120, modified: secondsAgo(86400 * 7) },
    { name: 'cold-cache.md', kind: 'file', bytes: 911, modified: secondsAgo(1800) },
  ],
  'mount:/memory:': [{ name: 'observations', kind: 'dir', bytes: 0, modified: secondsAgo(400) }],
  'mount:/nas:': [{ name: 'papers', kind: 'dir', bytes: 0, modified: secondsAgo(86400 * 30) }, { name: 'qwen3.8-27b.gguf', kind: 'file', bytes: 17_400_000_000, modified: secondsAgo(86400 * 20) }],
  'mount:/lab:': [{ name: 'runs', kind: 'dir', bytes: 0, modified: secondsAgo(5000) }],
  'mount:/mac:': [{ name: 'Desktop', kind: 'dir', bytes: 0, modified: secondsAgo(9000) }],
};
const demoFiles: FileSource = {
  async roots() { return { available: true, roots: DEMO_ROOTS, nodes_known: true }; },
  async list(root, path) {
    const entries = (DEMO_TREE[`${root}:${path}`] ?? []).map((entry) => ({
      ...entry, id: Array.from(new TextEncoder().encode(entry.name), (byte) => byte.toString(16).padStart(2, '0')).join(''),
    }));
    return { available: true, entries, truncated: false };
  },
};

function widgetHtmlDoc(theme: 'light' | 'dark', body: string, extraCss = ''): string {
  const dark = theme === 'dark';
  const bg = dark ? 'oklch(0.215 0 0)' : '#ffffff';
  const ink = dark ? 'oklch(0.945 0 0)' : 'oklch(0.22 0 0)';
  const muted = dark ? 'oklch(0.68 0 0)' : 'oklch(0.52 0 0)';
  const faint = dark ? 'oklch(0.55 0 0)' : 'oklch(0.64 0 0)';
  const line = dark ? 'oklch(0.29 0 0)' : 'oklch(0.905 0 0)';
  return `<!doctype html><html><head><meta charset="utf-8"><style>
    * { box-sizing: border-box; margin: 0; padding: 0; }
    html, body { width: 100%; height: 100%; overflow: hidden; background: ${bg}; color: ${ink};
      font: 12px/1.45 -apple-system, BlinkMacSystemFont, "PingFang SC", "Hiragino Sans GB", system-ui, sans-serif;
      -webkit-font-smoothing: antialiased; user-select: none; }
    body { padding: 10px 12px; display: flex; flex-direction: column; justify-content: space-between; }
    .muted { color: ${muted}; }
    .faint { color: ${faint}; }
    .num { font-variant-numeric: tabular-nums; }
    ${extraCss}
  </style></head><body>${body}</body></html>`;
}

function demoWidgetPreview(artifactId: string, theme: 'light' | 'dark'): DemoWidgetPreview | undefined {
  const dark = theme === 'dark';
  const ok = dark ? '#32d74b' : '#248a3d';
  const accent = dark ? '#0a84ff' : '#007aff';
  const barTrack = dark ? '#2d2d31' : '#ececef';
  const line = dark ? '#343438' : '#e5e5ea';
  const muted = dark ? '#9a9aa2' : '#6e6e73';

  if (artifactId === 'demo-work-1') {
    return {
      kind: 'html',
      html: widgetHtmlDoc(theme, `
        <div class="rows">
          <div class="item">
            <div class="label"><span>调整前 · 首请求后解析目录</span><span class="num muted">3 次重试</span></div>
            <div class="track"><div class="fill old" style="width:82%"></div></div>
          </div>
          <div class="item">
            <div class="label"><span>调整后 · 启动前解析数据目录</span><span class="num ok">0 次重试</span></div>
            <div class="track"><div class="fill now" style="width:28%"></div></div>
          </div>
        </div>
        <div class="foot muted">第二次冷启动仍多 300 ms · 暂记为缓存观察</div>
      `, `
        .rows { display: grid; gap: 9px; }
        .item { display: grid; gap: 4px; }
        .label { display: flex; justify-content: space-between; gap: 8px; font-size: 11.5px; }
        .ok { color: ${ok}; font-weight: 600; }
        .track { height: 6px; border-radius: 3px; background: ${barTrack}; overflow: hidden; }
        .fill { height: 100%; border-radius: 3px; }
        .fill.old { background: ${muted}; opacity: 0.45; }
        .fill.now { background: ${ok}; }
        .foot { font-size: 11px; padding-top: 8px; border-top: 1px solid ${line}; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
      `),
    };
  }
  if (artifactId === 'demo-work-3') {
    return {
      kind: 'html',
      html: widgetHtmlDoc(theme, `
        <p class="poem">「雨声盖过了风扇声。<br>记忆如果像底片，暂定条目就是还在暗房里没定影的片子。」</p>
        <div class="sign faint">22:14 · 随手记</div>
      `, `
        body { justify-content: space-between; padding: 12px; }
        .poem { font-size: 12.5px; line-height: 1.65; letter-spacing: 0.01em; }
        .sign { font-size: 10.5px; }
      `),
    };
  }
  if (artifactId === 'demo-work-4') {
    return {
      kind: 'html',
      html: widgetHtmlDoc(theme, `
        <div class="head">
          <span class="big num">50 / 50</span>
          <span class="muted">4 路 · 128K</span>
        </div>
        <div class="split">
          <div class="half a"></div>
          <div class="half b"></div>
        </div>
        <div class="note muted">群聊观察：自动分 KV，满载无抖动</div>
      `, `
        .head { display: flex; align-items: baseline; justify-content: space-between; gap: 6px; }
        .big { font-size: 19px; font-weight: 640; letter-spacing: -0.02em; }
        .split { display: grid; grid-template-columns: 1fr 1fr; gap: 4px; height: 8px; margin: 6px 0; }
        .half { border-radius: 4px; }
        .half.a { background: ${accent}; }
        .half.b { background: ${accent}; opacity: 0.45; }
        .note { font-size: 11px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
      `),
    };
  }
  if (artifactId === 'demo-work-2') {
    return {
      kind: 'markdown',
      text: '保留两次观察来源：\n· 11/12 部署日志：目录解析晚于首请求\n· 暂定条目（+300ms 冷缓存）待确认后再并入',
    };
  }
  if (artifactId === 'demo-work-arch-1') {
    return {
      kind: 'html',
      html: widgetHtmlDoc(theme, `
        <div class="stops">
          <div class="stop"><i class="dot"></i><span>神保町 · 矢口书店（电影脚本）</span></div>
          <div class="stop"><i class="dot"></i><span>小宫山书店 · 摄影画册二楼</span></div>
          <div class="stop"><i class="dot"></i><span>さぼうる 歇脚写见闻</span></div>
        </div>
      `, `
        .stops { display: grid; gap: 7px; }
        .stop { display: flex; align-items: center; gap: 8px; font-size: 11.5px; }
        .dot { width: 5px; height: 5px; border-radius: 50%; background: ${muted}; flex: none; }
      `),
    };
  }
  if (artifactId === 'demo-work-arch-2') {
    return {
      kind: 'markdown',
      text: '128 MB 工作区冷恢复对比：\n· WASM 家园：42 ms\n· Linux LayerFS：19 ms（大权重走只读挂载）',
    };
  }
  return undefined;
}

export const demoThreadData: DemoThreadData = {
  conversations: demoConversations,
  trajectory: demoTrajectory,
  files: demoFiles,
  widgetPreview: demoWidgetPreview,
};
