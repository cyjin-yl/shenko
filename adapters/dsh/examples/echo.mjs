import { defineTool } from '@deepseek-ai/dsh-tools';

export const name = 'shenko-dsh-echo-example';
export const inject = ['tools'];

export function apply(ctx) {
  ctx.tools.register(defineTool({
    name: 'echo',
    description: 'Return the supplied text without external effects.',
    parameters: { text: { type: 'string', required: true } },
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value }],
    },
    async execute({ text }) { return text; },
  }));
}
