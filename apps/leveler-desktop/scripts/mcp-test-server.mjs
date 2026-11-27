// A minimal, real MCP stdio server used by the Desktop R3a acceptance.
//
// It speaks JSON-RPC 2.0 over stdin/stdout: initialize, notifications/
// initialized, tools/list, tools/call and ping. The tools are ordinary test
// operations over in-memory data; nothing here fabricates a Runtime state.
import { createInterface } from 'node:readline';
import { appendFileSync } from 'node:fs';

const state = { calls: 0, catalogGrew: false };
const TOKEN = process.env.MCP_TEST_TOKEN ?? '';
const LOG = process.env.MCP_TEST_LOG ?? '';
const instance = { pid: process.pid, started_at: new Date().toISOString() };

function record(tool, args, outcome) {
  if (!LOG) return;
  try {
    appendFileSync(LOG, `${JSON.stringify({ ...instance, tool, args, outcome, at: new Date().toISOString() })}\n`);
  } catch { /* evidence logging must not stop the server */ }
}

const tools = () => [
  {
    name: 'echo',
    description: 'Return the provided text unchanged. Use this to prove a real round trip.',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string', description: 'Text to return' } },
      required: ['text'],
      additionalProperties: false,
    },
  },
  {
    name: 'add',
    description: 'Add two numbers and return the sum.',
    inputSchema: {
      type: 'object',
      properties: { a: { type: 'number' }, b: { type: 'number' } },
      required: ['a', 'b'],
      additionalProperties: false,
    },
  },
  {
    name: 'fail',
    description: 'Always report a tool error. Used to prove failures are not painted as success.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'env_probe',
    description: 'Report whether MCP_TEST_TOKEN reached this process, without returning its value.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
  {
    name: 'big',
    description: 'Return a large deterministic text so the full-output path can be proven.',
    inputSchema: {
      type: 'object',
      properties: { lines: { type: 'integer', minimum: 1, maximum: 20000 } },
      additionalProperties: false,
    },
  },
  ...(state.catalogGrew
    ? [{
        name: 'grown',
        description: 'Present only after the catalog grew.',
        inputSchema: { type: 'object', properties: {}, additionalProperties: false },
      }]
    : []),
];

function reply(id, result, error = null) {
  const message = { jsonrpc: '2.0', id };
  if (error) message.error = { code: -32603, message: error };
  else message.result = result;
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function notify(method, params = {}) {
  process.stdout.write(`${JSON.stringify({ jsonrpc: '2.0', method, params })}\n`);
}

function handle(request) {
  const method = request.method ?? '';
  if (method === 'initialize') {
    return {
      protocolVersion: request.params?.protocolVersion ?? '2024-11-05',
      capabilities: { tools: { listChanged: true } },
      serverInfo: { name: 'leveler-r3a-fixture', version: '1' },
    };
  }
  if (method === 'tools/list') return { tools: tools() };
  if (method === 'tools/call') {
    state.calls += 1;
    const name = request.params?.name;
    const args = request.params?.arguments ?? {};
    let result;
    if (name === 'echo') {
      result = { content: [{ type: 'text', text: String(args.text ?? '') }] };
    } else if (name === 'add') {
      const sum = Number(args.a) + Number(args.b);
      result = { content: [{ type: 'text', text: String(sum) }] };
    } else if (name === 'fail') {
      result = { content: [{ type: 'text', text: 'intentional MCP tool failure' }], isError: true };
    } else if (name === 'env_probe') {
      result = {
        content: [{
          type: 'text',
          text: JSON.stringify({ token_present: TOKEN.length > 0, token_length: TOKEN.length }),
        }],
      };
    } else if (name === 'big') {
      const count = Math.max(1, Math.min(20000, Number(args.lines ?? 6000)));
      const text = Array.from({ length: count }, (_, index) => `MCP_BIG_LINE_${String(index + 1).padStart(5, '0')}`).join('\n');
      result = { content: [{ type: 'text', text }] };
    } else if (name === 'grow_catalog') {
      state.catalogGrew = true;
      notify('notifications/tools/list_changed');
      result = { content: [{ type: 'text', text: 'catalog grew' }] };
    } else {
      result = { content: [{ type: 'text', text: `unknown tool: ${name}` }], isError: true };
    }
    record(String(name), args, result.isError ? 'error' : 'ok');
    return result;
  }
  if (method === 'ping') return {};
  return {};
}

const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
lines.on('line', line => {
  const text = line.trim();
  if (!text) return;
  let request;
  try { request = JSON.parse(text); } catch { return; }
  // Notifications (no id) are acknowledged by doing nothing.
  if (request.id === undefined || request.id === null) return;
  try {
    reply(request.id, handle(request));
  } catch (error) {
    reply(request.id, null, error instanceof Error ? error.message : String(error));
  }
});
lines.on('close', () => process.exit(0));
