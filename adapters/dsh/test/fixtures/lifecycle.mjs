import { defineTool } from '@deepseek-ai/dsh-tools';

export const inject = ['tools'];
export const state = { aborted: 0, disposed: 0, entered: null, release: null };

export function apply(ctx, config) {
  ctx.effect(() => () => { state.disposed += 1; });
  ctx.tools.register(defineTool({
    name: 'wait', description: 'Exercise bounded lifetime.', parameters: {},
    output: { schema: { type: 'string' }, render: (_args, value) => [{ type: 'text', text: value }] },
    async execute(_args, exec) {
      await new Promise((resolve, reject) => {
        state.release = resolve;
        exec.signal.addEventListener('abort', () => {
          state.aborted += 1;
          if (config.cooperative) reject(exec.signal.reason);
        }, { once: true });
        state.entered?.();
      });
      return 'settled';
    },
  }));
}
