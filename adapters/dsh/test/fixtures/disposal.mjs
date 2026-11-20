import { appendFile } from 'node:fs/promises';
import { appendFileSync } from 'node:fs';
import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];

export function apply(ctx, { mode, witness }) {
  ctx.effect(() => () => {
    appendFileSync(witness, 'dispose-started\n');
    if (mode === 'throw') throw new Error('fixture disposer threw');
    if (mode === 'falsy') throw null;
    if (mode === 'aggregate-empty') throw new AggregateError([], 'fixture empty aggregate');
    return (async () => {
      if (mode === 'reject') throw new Error('fixture disposer rejected');
      if (mode === 'timeout') await new Promise(() => {});
      await appendFile(witness, 'dispose-finished\n');
    })();
  });
  ctx.tools.register(defineTool({
    name: 'disposal_probe',
    description: 'Expose a catalog while a real Cordis effect owns cleanup.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute() { return 'ready'; },
  }));
}
