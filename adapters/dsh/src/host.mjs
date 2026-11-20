import { Context, Inject } from '@deepseek-ai/cordis';
import SystemPrompt from '@deepseek-ai/dsh-system-prompt';
import ToolRuntime from '@deepseek-ai/dsh-tools';
import * as NativeToolExecutor from '@deepseek-ai/dsh-tool-executor-native';
import { pathToFileURL } from 'node:url';
import { randomUUID } from 'node:crypto';

/** No terminal tool response is safe after an interrupted external effect. */
export class UnknownOutcomeError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = 'UnknownOutcomeError';
    this.code = 'DSH_OUTCOME_UNKNOWN';
  }
}

export class DshCompatibilityError extends Error {
  constructor(message, options) {
    super(message, options);
    this.name = 'DshCompatibilityError';
    this.code = 'DSH_UNSUPPORTED';
  }
}

function pluginExport(module) {
  const plugin = module.default ?? module;
  if (typeof plugin === 'function' || (plugin && typeof plugin.apply === 'function')) return plugin;
  throw new DshCompatibilityError('Selected module must export a Cordis function, service class, or apply(ctx, config).');
}

async function startupDeadline(work, timeoutMs, label) {
  let timer;
  try {
    return await Promise.race([
      work,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new DshCompatibilityError(`${label} did not finish within ${timeoutMs} ms.`)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function toolDescription(definition) {
  if (definition.parameters?.type !== 'object') {
    throw new DshCompatibilityError(`Tool ${JSON.stringify(definition.name)} has no MCP object input schema.`);
  }
  return {
    name: definition.name,
    description: definition.description,
    inputSchema: structuredClone(definition.parameters),
    outputSchema: {
      type: 'object',
      properties: { value: structuredClone(definition.output.schema) },
      required: ['value'],
      additionalProperties: false,
    },
  };
}

function contentProjection(blocks) {
  return blocks.map((block) => {
    if (block.type === 'text' && typeof block.text === 'string') return { type: 'text', text: block.text };
    // Inline bytes are supported without resolving DSH-owned attachment storage.
    if ((block.type === 'image' || block.type === 'audio')
      && typeof block.data === 'string' && typeof block.mimeType === 'string') {
      return { type: block.type, data: block.data, mimeType: block.mimeType };
    }
    throw new DshCompatibilityError(`Cannot project DSH content ${JSON.stringify(block.type)}; this profile has no attachment, session, or client runtime.`);
  });
}

function projectResult(batch) {
  const result = batch.results[0]?.result;
  if (!result || batch.results.length !== 1) throw new UnknownOutcomeError('DSH did not settle exactly one requested call.');
  if (['ABORTED', 'ABORTED_BEFORE_DISPATCH'].includes(result.error?.info?.code)) {
    throw new UnknownOutcomeError(`DSH reported ${result.error.info.code}; external effects require reconciliation.`);
  }
  const meta = {
    'shenko.dsh': {
      ...(result.error === undefined ? {} : { error: result.error }),
      ...(result.meta === undefined ? {} : { presentation: result.meta }),
    },
  };
  if (batch.concluded || batch.additionalContexts.length > 0) {
    return {
      isError: true,
      content: [{ type: 'text', text: 'DSH tool returned agent context or turn control. This standalone tools profile cannot apply it; the tool may already have performed effects.' }],
      ...(result.isError ? {} : { structuredContent: { value: result.value } }),
      _meta: { ...meta, 'shenko.dsh.unsupported': { concluded: batch.concluded, additionalContexts: batch.additionalContexts } },
    };
  }
  let content;
  try {
    content = contentProjection(result.content);
  } catch (error) {
    if (!(error instanceof DshCompatibilityError)) throw error;
    return {
      isError: true,
      content: [{ type: 'text', text: `${error.message} The native result was observed; any effects have not been rolled back.` }],
      ...(result.isError ? {} : { structuredContent: { value: result.value } }),
      _meta: { ...meta, 'shenko.dsh.unprojectedContent': result.content },
    };
  }
  return {
    content,
    isError: result.isError,
    ...(result.isError ? {} : { structuredContent: { value: result.value } }),
    _meta: meta,
  };
}

/** Mount genuine Cordis services, without constructing a DSH Agent or model loop. */
export async function createDshHost(config) {
  const ctx = new Context();
  const inFlight = new Set();
  const controllers = new Set();
  const callTimeoutMs = config.callTimeoutMs ?? 30_000;
  const shutdownGraceMs = config.shutdownGraceMs ?? 1_000;
  let serial = Promise.resolve();
  let closed = false;
  let poisoned;
  let closing;

  function assertAvailable() {
    if (poisoned) throw poisoned;
    if (closed) throw new DshCompatibilityError('DSH host has closed.');
  }

  function markUnknown(error) {
    poisoned ??= error;
    for (const controller of controllers) controller.abort(poisoned);
  }

  function listTools() {
    assertAvailable();
    return ctx.tools.definitions().map((definition) => {
      if (ctx.tools.executable(definition.name) !== definition) {
        throw new DshCompatibilityError(`Tool ${JSON.stringify(definition.name)} requires step-time Agent admission (register(..., { when })); this profile has no Agent.`);
      }
      return toolDescription(definition);
    });
  }

  async function close() {
    if (closing) return closing;
    closed = true;
    for (const controller of controllers) controller.abort(new UnknownOutcomeError('DSH host is closing.'));
    const cleanup = (async () => {
      await Promise.allSettled([...inFlight]);
      // Cordis 4.1 logs rejected disposers instead of rejecting Fiber.dispose().
      // Its exporter() helper is owned by the disposing root and can disappear
      // before child teardown logs arrive. Own this public exporter-map entry
      // until actual disposal settles, including after our grace window expires.
      const logger = ctx.logger;
      const exporters = logger.exporters;
      let exporterId = -1;
      while (exporters.has(exporterId)) exporterId -= 1;
      let failed = false;
      let failure;
      const observe = (reason) => {
        if (failed) return;
        failed = true;
        failure = reason;
      };
      exporters.set(exporterId, {
        levels: { default: 0 },
        export(message) {
          if (message.type === 'error') observe(message.args[0]);
        },
      });
      const errorDescriptor = Object.getOwnPropertyDescriptor(logger, 'error');
      const originalError = logger.error;
      try {
        // _unload calls this service method before Logger flattens aggregate
        // errors. An empty AggregateError emits no exporter message at all.
        Object.defineProperty(logger, 'error', {
          configurable: true,
          enumerable: errorDescriptor?.enumerable ?? false,
          writable: true,
          value: function (...args) {
            observe(args[0]);
            return Reflect.apply(originalError, this, args);
          },
        });
        await ctx.fiber.dispose();
        if (failed) {
          throw new DshCompatibilityError('DSH plugin cleanup reported an error.', { cause: failure });
        }
      } finally {
        exporters.delete(exporterId);
        if (errorDescriptor) Object.defineProperty(logger, 'error', errorDescriptor);
        else delete logger.error;
      }
    })();
    let timer;
    closing = Promise.race([
      cleanup.then(() => ({ drained: true })),
      new Promise((resolve) => { timer = setTimeout(() => resolve({ drained: false }), shutdownGraceMs); }),
    ]).finally(() => clearTimeout(timer));
    return closing;
  }

  try {
    await ctx.plugin(SystemPrompt);
    await ctx.plugin(ToolRuntime);
    await ctx.plugin(NativeToolExecutor);
    const pending = [];
    for (const entry of config.plugins) {
      let plugin;
      try {
        plugin = pluginExport(await startupDeadline(
          import(pathToFileURL(entry.entry).href), callTimeoutMs, `Plugin ${JSON.stringify(entry.id)} module import`,
        ));
      } catch (error) {
        throw new DshCompatibilityError(`Cannot load plugin ${JSON.stringify(entry.id)} from ${entry.entry}: ${error.message}`, { cause: error });
      }
      pending.push({ entry, plugin, requires: Object.keys(Inject.resolve(plugin.inject)) });
    }
    while (pending.length > 0) {
      let mounted = false;
      for (let index = 0; index < pending.length;) {
        const { entry, plugin, requires } = pending[index];
        if (requires.some((service) => ctx.get(service) === undefined)) { index += 1; continue; }
        try {
          await startupDeadline(ctx.plugin(plugin, entry.config ?? {}), callTimeoutMs, `Plugin ${JSON.stringify(entry.id)} initialization`);
        } catch (error) {
          throw new DshCompatibilityError(`Plugin ${JSON.stringify(entry.id)} failed to mount: ${error.message}`, { cause: error });
        }
        pending.splice(index, 1);
        mounted = true;
      }
      if (!mounted) {
        const blocked = pending.map(({ entry, requires }) => `${entry.id}: ${requires.filter((service) => ctx.get(service) === undefined).join(', ')}`);
        throw new DshCompatibilityError(`Missing DSH services (${blocked.join('; ')}). Select providers explicitly or use a plugin that supports standalone tools.`);
      }
    }
    const tools = listTools();
    if (tools.length === 0) throw new DshCompatibilityError('Selected modules registered no standalone tools. Client-only, skill-only, and agent-only plugins are outside this profile.');
  } catch (error) {
    try {
      await close();
    } catch (cleanupError) {
      console.error('DSH cleanup failed after plugin startup failure:', cleanupError);
    }
    throw error;
  }

  async function callTool(name, args = {}, { signal, requestId } = {}) {
    assertAvailable();
    const definition = ctx.tools.executable(name);
    if (!definition) return { isError: true, content: [{ type: 'text', text: `Unknown standalone DSH tool: ${name}` }] };
    const controller = new AbortController();
    controllers.add(controller);
    const budget = Math.min(callTimeoutMs, definition.timeoutMs ?? Infinity);
    let rejectUnknown;
    const interrupted = new Promise((_, reject) => { rejectUnknown = reject; });
    const onInterrupted = () => rejectUnknown(controller.signal.reason);
    controller.signal.addEventListener('abort', onInterrupted, { once: true });
    const interrupt = (message) => {
      const error = new UnknownOutcomeError(message);
      markUnknown(error);
    };
    const onAbort = () => interrupt('DSH call was cancelled; external effects require reconciliation.');
    signal?.addEventListener('abort', onAbort, { once: true });
    if (signal?.aborted) onAbort();
    const timer = setTimeout(() => interrupt(`DSH call exceeded ${budget} ms; external effects require reconciliation.`), budget);
    // Serial admission preserves exclusive-tool semantics across separate MCP requests.
    const owned = serial.then(async () => {
      assertAvailable();
      controller.signal.throwIfAborted();
      const input = { calls: [{ type: 'tool-call', id: String(requestId ?? randomUUID()), name, arguments: JSON.stringify(args) }], signal: controller.signal };
      let batch;
      try {
        batch = await ctx.tools.execute(input);
      } catch (error) {
        // Ordinary tool errors are normalized by DSH. A rejected batch is a
        // pipeline failure and may follow an external effect already started.
        throw new UnknownOutcomeError('DSH execution pipeline failed without a terminal result; external effects require reconciliation.', { cause: error });
      }
      return projectResult(batch);
    }).catch((error) => {
      // Poison before releasing serial admission, so already queued calls
      // cannot start after an uncertain predecessor.
      if (error instanceof UnknownOutcomeError) markUnknown(error);
      throw error;
    });
    inFlight.add(owned);
    serial = owned.catch(() => {});
    void owned.finally(() => { inFlight.delete(owned); controllers.delete(controller); }).catch(() => {});
    try {
      return await Promise.race([owned, interrupted]);
    } catch (error) {
      if (error instanceof UnknownOutcomeError) markUnknown(error);
      throw error;
    } finally {
      clearTimeout(timer);
      signal?.removeEventListener('abort', onAbort);
      controller.signal.removeEventListener('abort', onInterrupted);
    }
  }

  return { listTools, callTool, close, onToolsChanged: (listener) => ctx.on('tools/change', listener) };
}
