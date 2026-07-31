import { createServer } from 'node:http';

import { localhostHostValidation, localhostOriginValidation, toNodeHandler } from '@modelcontextprotocol/node';
import { createMcpHandler, McpServer } from '@modelcontextprotocol/server';
import * as z from 'zod/v4';

const port = Number.parseInt(process.argv[2] ?? '', 10);
if (!Number.isInteger(port) || port < 1 || port > 65535) {
  throw new Error('usage: node server.mjs <port>');
}

function buildServer() {
  const server = new McpServer({ name: 'typescript-sdk-interop', version: '2.0.0' });
  server.registerTool(
    'interop_add',
    {
      description: 'Add two integers for SDK interoperability testing',
      inputSchema: z.object({ a: z.number().int(), b: z.number().int() })
    },
    async ({ a, b }) => ({ content: [{ type: 'text', text: String(a + b) }] })
  );
  server.registerResource(
    'interop_fixture',
    'interop://fixture',
    { description: 'Static SDK interoperability content', mimeType: 'text/plain' },
    async uri => ({ contents: [{ uri: uri.toString(), mimeType: 'text/plain', text: 'sdk-interop resource' }] })
  );
  server.registerPrompt(
    'interop_greet',
    {
      description: 'Render a greeting for SDK interoperability testing',
      argsSchema: z.object({ name: z.string() })
    },
    async ({ name }) => ({
      messages: [{ role: 'user', content: { type: 'text', text: `Hello, ${name}!` } }]
    })
  );
  return server;
}

const handler = createMcpHandler(buildServer);
const nodeHandler = toNodeHandler(handler);
const validateHost = localhostHostValidation();
const validateOrigin = localhostOriginValidation();
const server = createServer((request, response) => {
  if (request.url !== '/mcp') {
    response.writeHead(404).end();
    return;
  }
  if (!validateHost(request, response) || !validateOrigin(request, response)) return;
  void nodeHandler(request, response);
});

async function close() {
  await handler.close();
  server.close(() => process.exit(0));
}

process.on('SIGINT', () => void close());
process.on('SIGTERM', () => void close());
server.listen(port, '127.0.0.1', () => {
  console.log(`READY http://127.0.0.1:${port}/mcp`);
});
