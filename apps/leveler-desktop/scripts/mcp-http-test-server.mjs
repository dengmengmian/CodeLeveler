// A minimal Streamable HTTP MCP server for the R3a acceptance.
//
// It implements the subset the runtime's rmcp client uses: POST JSON-RPC for
// initialize / tools/list / tools/call, an optional SSE GET stream, and a
// session header. The tool set mirrors the stdio fixture.
import { createServer } from 'node:http';

const SESSION = 'leveler-r3a-http-session';
const PORT = Number(process.env.MCP_HTTP_PORT ?? 0);
const TOKEN = process.env.MCP_HTTP_TOKEN ?? '';

const tools = [
  {
    name: 'http_echo',
    description: 'Echo text over Streamable HTTP.',
    inputSchema: {
      type: 'object',
      properties: { text: { type: 'string' } },
      required: ['text'],
      additionalProperties: false,
    },
  },
  {
    name: 'http_add',
    description: 'Add two numbers over Streamable HTTP.',
    inputSchema: {
      type: 'object',
      properties: { a: { type: 'number' }, b: { type: 'number' } },
      required: ['a', 'b'],
      additionalProperties: false,
    },
  },
  {
    name: 'http_env',
    description: 'Report whether the configured bearer token reached this request.',
    inputSchema: { type: 'object', properties: {}, additionalProperties: false },
  },
];

function answer(request, authorization) {
  const method = request.method ?? '';
  if (method === 'initialize') {
    return {
      protocolVersion: request.params?.protocolVersion ?? '2024-11-05',
      capabilities: { tools: { listChanged: false } },
      serverInfo: { name: 'leveler-r3a-http', version: '1' },
    };
  }
  if (method === 'tools/list') return { tools };
  if (method === 'tools/call') {
    const name = request.params?.name;
    const args = request.params?.arguments ?? {};
    if (name === 'http_echo') return { content: [{ type: 'text', text: String(args.text ?? '') }] };
    if (name === 'http_add') {
      return { content: [{ type: 'text', text: String(Number(args.a) + Number(args.b)) }] };
    }
    if (name === 'http_env') {
      return {
        content: [{
          type: 'text',
          text: JSON.stringify({ authorization_present: Boolean(authorization), token_configured: TOKEN.length > 0 }),
        }],
      };
    }
    return { content: [{ type: 'text', text: `unknown tool: ${name}` }], isError: true };
  }
  if (method === 'ping') return {};
  return {};
}

function headers(extra = {}) {
  return {
    'mcp-session-id': SESSION,
    'mcp-protocol-version': '2024-11-05',
    ...extra,
  };
}

function drain(request) {
  return new Promise(resolve => {
    let body = '';
    request.on('data', chunk => { body += chunk; });
    request.on('end', () => resolve(body));
  });
}

const server = createServer(async (request, response) => {
  const url = new URL(request.url ?? '/', 'http://127.0.0.1');
  if (url.pathname !== '/mcp') {
    response.writeHead(404).end();
    return;
  }
  if (request.method === 'GET') {
    // The optional server-to-client stream. It stays open and sends nothing;
    // the client may use POST answers only.
    response.writeHead(200, headers({ 'content-type': 'text/event-stream', 'cache-control': 'no-cache' }));
    response.write(': connected\n\n');
    request.on('close', () => response.end());
    return;
  }
  if (request.method === 'DELETE') {
    response.writeHead(200, headers()).end();
    return;
  }
  if (request.method !== 'POST') {
    response.writeHead(405).end();
    return;
  }
  const body = await drain(request);
  process.stderr.write(`[mcp-http] ${request.method} ${url.pathname} ${body.slice(0, 800)}\n`);
  let payload;
  try { payload = JSON.parse(body); } catch { response.writeHead(400).end(); return; }
  const messages = Array.isArray(payload) ? payload : [payload];
  const answers = [];
  for (const message of messages) {
    if (message.id === undefined || message.id === null) continue;
    // rmcp probes the 2026-07-28 `server/discover` handshake first. A
    // method-not-found error is the protocol's signal that this server is a
    // legacy `initialize` peer, which is exactly what this fixture is.
    if (message.method === 'server/discover') {
      answers.push({ jsonrpc: '2.0', id: message.id, error: { code: -32601, message: 'method not found' } });
      continue;
    }
    answers.push({ jsonrpc: '2.0', id: message.id, result: answer(message, request.headers.authorization) });
  }
  if (answers.length === 0) {
    response.writeHead(202, headers()).end();
    return;
  }
  const text = JSON.stringify(answers.length === 1 ? answers[0] : answers);
  process.stderr.write(`[mcp-http] answer ${text.slice(0, 800)}\n`);
  response.writeHead(200, headers({ 'content-type': 'application/json', 'content-length': Buffer.byteLength(text) }));
  response.end(text);
});

server.listen(PORT, '127.0.0.1', () => {
  process.stdout.write(`LISTENING ${server.address().port}\n`);
});
