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
  let ended;
  let stopping;
  const done = new Promise((resolve) => { ended = resolve; });

  function stop(error) {
    fatal ??= error;
    stopping ??= server.close().finally(ended);
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
      // Close before the handler settles: no ordinary result/error may turn an
      // unknown external effect into a failed or completed Shenko receipt.
      if (error instanceof UnknownOutcomeError) await stop(error);
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
  } finally {
    process.removeListener('SIGTERM', terminate);
    process.removeListener('SIGINT', terminate);
    process.stdin.removeListener('end', endInput);
    removeListener();
    await server.close();
    const cleanup = await host.close();
    if (!cleanup.drained) fatal ??= new UnknownOutcomeError('DSH work did not quiesce within the shutdown grace; process termination does not reconcile effects.');
  }
  if (fatal) throw fatal;
}
