import { appendFileSync } from 'node:fs';
import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];

export function apply(ctx, { witness }) {
  ctx.effect(() => async () => {
    appendFileSync(witness, 'dispose-started\n');
    throw new AggregateError([], 'fixture empty aggregate');
  });
  ctx.tools.register(defineTool({
    name: 'disposal_probe',
    description: 'Expose a catalog while a real Cordis effect owns cleanup.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute() { return 'ready'; },
  }));
}
