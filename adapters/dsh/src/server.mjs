import { Server } from '@modelcontextprotocol/sdk/server/index.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { CallToolRequestSchema, ListToolsRequestSchema } from '@modelcontextprotocol/sdk/types.js';
import { createDshHost, UnknownOutcomeError } from './host.mjs';

/** Serve until stdin closes or an uncertain call invalidates this process. */
export async function serve(config) {
  const host = await createDshHost(config);
  const server = new Server(
    { name: 'shenko-dsh-tools', version: '0.1.0' },
    { capabilities: { tools: { listChanged: true } } },
  );
  const transport = new StdioServerTransport();
  let fatal;
  let hasFatal = false;
  let ended;
  let stopping;
  const done = new Promise((resolve) => { ended = resolve; });

  function retainFailure(error) {
    if (hasFatal) {
      console.error('DSH shutdown failed after primary failure:', error);
      return;
    }
    hasFatal = true;
    fatal = error;
  }

  function stop(error) {
    if (arguments.length && !hasFatal) retainFailure(error);
    stopping ??= server.close().catch(retainFailure).finally(ended);
    return stopping;
  }

  server.onerror = (error) => console.error(`DSH MCP transport: ${error.message}`);
  server.onclose = ended;
  server.setRequestHandler(ListToolsRequestSchema, () => ({ tools: host.listTools() }));
  server.setRequestHandler(CallToolRequestSchema, async (request, extra) => {
    try {
      return await host.callTool(request.params.name, request.params.arguments ?? {}, {
        signal: extra.signal,
        requestId: extra.requestId,
      });
    } catch (error) {
      if (error instanceof UnknownOutcomeError) {
        try {
          await stop(error);
        } finally {
          // A rejected transport close may leave stdout writable and the SDK
          // request unaborted. Never settle this handler into an ordinary reply.
          // stop() releases serve() for bounded host cleanup; the owning CLI
          // then exits with the retained primary error and retires the process.
          await new Promise(() => {});
        }
      }
      throw error;
    }
  });
  const removeListener = host.onToolsChanged(() => {
    void server.notification({ method: 'notifications/tools/list_changed' }).catch(() => {});
  });
  const terminate = () => { void stop(new UnknownOutcomeError('DSH bridge terminated; in-flight effects require reconciliation.')); };
  const endInput = () => { void stop(); };
  process.once('SIGTERM', terminate);
  process.once('SIGINT', terminate);
  process.stdin.once('end', endInput);

  try {
    await server.connect(transport);
    await done;
  } catch (error) {
    retainFailure(error);
  } finally {
    process.removeListener('SIGTERM', terminate);
    process.removeListener('SIGINT', terminate);
    process.stdin.removeListener('end', endInput);
    removeListener();
    try {
      await server.close();
    } catch (error) {
      retainFailure(error);
    }
    try {
      const cleanup = await host.close();
      if (!cleanup.drained) retainFailure(new UnknownOutcomeError('DSH work did not quiesce within the shutdown grace; process termination does not reconcile effects.'));
    } catch (cleanupError) {
      retainFailure(cleanupError);
    }
  }
  if (hasFatal) throw fatal;
}
