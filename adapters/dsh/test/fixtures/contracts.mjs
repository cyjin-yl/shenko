import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];
export const state = { calls: 0, disposed: 0, observations: [] };

export function apply(ctx) {
  ctx.effect(() => () => { state.disposed += 1; });
  ctx.tools.guard((exec) => exec.arguments.number === 13 ? { reason: 'fixture denial' } : undefined);
  ctx.on('tools/result', (_exec, result) => { state.observations.push(result.isError); });
  ctx.tools.register(defineTool({
    name: 'double',
    description: 'Double a number using the real DSH contract.',
    parameters: { number: { type: 'number', required: true } },
    output: { schema: { type: 'number' }, render: (_args, value) => [{ type: 'text', text: String(value) }] },
    finalizeContent: (_exec, result) => result.isError ? undefined : [{ type: 'text', text: `answer ${result.value}` }],
    async execute({ number }) { state.calls += 1; return number * 2; },
  }));
  ctx.tools.register(defineTool({
    name: 'bad_output',
    description: 'Deliberately violate the return schema.',
    parameters: {},
    output: { schema: { type: 'number' }, render: (_args, value) => [{ type: 'text', text: String(value) }] },
    async execute() { return 'not a number'; },
  }));
}
