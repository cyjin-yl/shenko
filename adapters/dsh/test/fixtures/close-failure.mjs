import { appendFile } from 'node:fs/promises';
import { appendFileSync } from 'node:fs';
import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];

export function apply(ctx, { witness }) {
  let faultInstalled = false;
  let pauseDescriptor;
  ctx.effect(() => async () => {
    if (faultInstalled) {
      if (pauseDescriptor) Object.defineProperty(process.stdin, 'pause', pauseDescriptor);
      else delete process.stdin.pause;
    }
    await appendFile(witness, 'disposed\n');
  });
  ctx.tools.register(defineTool({
    name: 'ordinary_error',
    description: 'Return an ordinary native tool failure before the close fault.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute() { throw new Error('fixture ordinary tool error'); },
  }));
  ctx.tools.register(defineTool({
    name: 'uncertain_close',
    description: 'Record an effect and interrupt actual stdio close before onclose.',
    parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute(_args, exec) {
      await appendFile(witness, 'effect-before-timeout\n');
      pauseDescriptor = Object.getOwnPropertyDescriptor(process.stdin, 'pause');
      Object.defineProperty(process.stdin, 'pause', {
        configurable: true,
        writable: true,
        value() {
          appendFileSync(witness, 'close-rejected\n');
          throw new Error('fixture transport close rejected');
        },
      });
      faultInstalled = true;
      await new Promise((resolve, reject) => {
        if (exec.signal.aborted) reject(exec.signal.reason);
        else exec.signal.addEventListener('abort', () => reject(exec.signal.reason), { once: true });
      });
      return 'unreachable';
    },
  }));
}
