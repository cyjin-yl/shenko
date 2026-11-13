import type { ConversationRow, DemoThreadData, TrajectoryResponse } from './threads';

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
    task: { task_id: member.task_id, kind: 'model.child', role: 'delegation', state: 'succeeded', created_at: ago(330),
      created_by_step_id: DEMO_TEAM.step, name: member.name, team: DEMO_TEAM.team },
    input: null, brief: member.goal, children: [], operations,
  };
}

export function demoTrajectory(taskId: string): TrajectoryResponse {
  const member = DEMO_TEAM.members.find((item) => item.task_id === taskId);
  if (member) return demoTeamTrajectory(member);
  const at = new Date(Date.now() - 60_000).toISOString();
  const goal = '把两条关于启动时序的观察合成一条更小的记忆，保留各自来源。';
  const step = (index: number, record: Record<string, unknown>) => ({
    operation_id: `${taskId}-step-${index}`, operation_name: 'model.child.generate.v1', logical_key: `step/${index}`,
    state: 'resolved', outcome: 'succeeded', attempts: 1, record_status: 'sealed',
    record: { schema_version: 1, step_index: index, ...record },
  });
  return {
    task: { task_id: taskId, kind: 'model.child', role: 'delegation', state: 'running', created_at: at },
    input: { goal }, brief: goal,
    children: [{ task_id: `${taskId}-tool`, kind: 'memory.recall', role: 'tool', state: 'succeeded', created_at: at }],
    operations: [
      step(0, { text: '先找出两条观察的原文和来源。', tool_calls: [{ kind: 'memory.recall' }], usage: { output_tokens: 42 } }),
      step(1, { text: '两条都来自 11 月 12 日的部署日志。合并后保留两个来源引用；冷缓存那条仍标为暂定。', question: '要把“暂定”这条也并进去吗？', usage: { output_tokens: 88 } }),
    ],
  };
}

export const demoThreadData: DemoThreadData = { conversations: demoConversations, trajectory: demoTrajectory };
