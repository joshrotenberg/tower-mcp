import assert from 'node:assert/strict';

import { Client, StreamableHTTPClientTransport } from '@modelcontextprotocol/client';

const url = process.argv[2];
const protocol = process.argv[3];
if (!url || !['2025-11-25', '2026-07-28'].includes(protocol)) {
  throw new Error('usage: node client.mjs <url> <2025-11-25|2026-07-28>');
}

const mode = protocol === '2026-07-28' ? 'auto' : 'legacy';
const expectedEra = protocol === '2026-07-28' ? 'modern' : 'legacy';
const client = new Client(
  { name: 'typescript-sdk-interop-client', version: '2.0.0' },
  { versionNegotiation: { mode } }
);

try {
  await client.connect(new StreamableHTTPClientTransport(new URL(url)));
  assert.equal(client.getProtocolEra(), expectedEra, `expected ${expectedEra} protocol era`);

  const { tools } = await client.listTools();
  assert.ok(tools.some(tool => tool.name === 'interop_add'), 'tools/list omitted interop_add');
  const called = await client.callTool({ name: 'interop_add', arguments: { a: 19, b: 23 } });
  assert.equal(called.content?.[0]?.type === 'text' ? called.content[0].text : undefined, '42');

  const { resources } = await client.listResources();
  assert.ok(resources.some(resource => resource.uri === 'interop://fixture'), 'resources/list omitted fixture');
  const read = await client.readResource({ uri: 'interop://fixture' });
  assert.equal(read.contents[0]?.text, 'sdk-interop resource');

  const { prompts } = await client.listPrompts();
  assert.ok(prompts.some(prompt => prompt.name === 'interop_greet'), 'prompts/list omitted interop_greet');
  const prompt = await client.getPrompt({ name: 'interop_greet', arguments: { name: 'Tower' } });
  assert.equal(prompt.messages[0]?.content.type === 'text' ? prompt.messages[0].content.text : undefined, 'Hello, Tower!');

  console.log(`PASS TypeScript SDK client -> ${url} (${protocol})`);
} finally {
  await client.close();
}
