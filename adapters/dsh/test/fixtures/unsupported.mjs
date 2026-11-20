import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];

export function apply(ctx, config) {
  if (config.agentDependency) {
    ctx.plugin({ inject: ['agents'], apply() { throw new Error('Missing service must not become available.'); } });
    return;
  }
  ctx.tools.register(defineTool({
    name: 'conditional', description: 'Requires a genuine agent step.', parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute() { throw new Error('A conditional tool cannot run without a step.'); },
  }), { when: () => true });
}
