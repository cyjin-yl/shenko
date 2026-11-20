import { appendFile } from 'node:fs/promises';
import { defineTool } from '@deepseek-ai/dsh-tools';

export const name = 'protocol-witness';
export const inject = ['tools'];

export function apply(ctx, config) {
  console.log('plugin diagnostic belongs on stderr');
  ctx.effect(() => async () => { await appendFile(config.witness, 'disposed\n'); });
  ctx.tools.register(defineTool({
    name: 'add',
    description: 'Add two integers and retain an independent dispatch witness.',
    parameters: {
      a: { type: 'integer', required: true },
      b: { type: 'integer', required: true },
    },
    output: {
      schema: { type: 'integer' },
      render: (_args, value) => [{ type: 'text', text: String(value) }],
    },
    async execute({ a, b }) {
      await appendFile(config.witness, 'add\n');
      return a + b;
    },
  }));
  ctx.tools.register(defineTool({
    name: 'uncertain',
    description: 'Record an effect, then wait until the owner cancels.',
    parameters: {},
    output: {
      schema: { type: 'string' },
      render: (_args, value) => [{ type: 'text', text: value }],
    },
    async execute(_args, exec) {
      await appendFile(config.witness, 'effect-before-timeout\n');
      await new Promise((resolve, reject) => {
        if (exec.signal.aborted) reject(exec.signal.reason);
        else exec.signal.addEventListener('abort', () => reject(exec.signal.reason), { once: true });
      });
      return 'unreachable';
    },
  }));
}
