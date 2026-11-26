// R2a host-adapter acceptance: provider configuration through the real
// `leveler desktop-bridge` process, against a temporary HOME and a local
// OpenAI-compatible endpoint. No real credential and no user configuration is
// touched. The bridge is the same process the Desktop uses; only the HTTP
// upstream is local so the check is deterministic.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFileSync, statSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { createInterface } from 'node:readline';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const binary = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(binary)).digest('hex');
const home = await mkdtemp('/tmp/leveler-r2a-bridge-');
const evidence = {
  scope: 'real desktop-bridge host adapter; temporary HOME; local OpenAI-compatible upstream; no real credential',
  binary: { path: binary, sha256: binarySha },
  home,
  steps: {},
};
const KEY = 'sk-r2a-bridge-fixture-000000000000';
let seenKeyLeak = false;

const server = createServer(async (request, response) => {
  const authorized = request.headers.authorization === `Bearer ${KEY}`;
  if (request.method === 'GET' && request.url === '/v1/models') {
    if (!authorized) {
      response.writeHead(401, { 'content-type': 'application/json' });
      response.end('{"error":"unauthorized"}');
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ data: [{ id: 'alpha' }, { id: 'beta' }] }));
    return;
  }
  if (request.method === 'POST' && request.url === '/v1/chat/completions') {
    let raw = '';
    for await (const chunk of request) raw += chunk;
    if (!authorized) {
      response.writeHead(401, { 'content-type': 'application/json' });
      response.end('{"error":"unauthorized"}');
      return;
    }
    const body = JSON.parse(raw || '{}');
    if (body.stream === true) {
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      response.write('data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":null}]}\n\n');
      response.write('data: {"id":"c1","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}\n\n');
      response.end('data: [DONE]\n\n');
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      id: 'c1',
      choices: [{ index: 0, message: { role: 'assistant', content: 'OK' }, finish_reason: 'stop' }],
      usage: { prompt_tokens: 3, completion_tokens: 1, total_tokens: 4 },
    }));
    return;
  }
  response.writeHead(404);
  response.end();
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const baseUrl = `http://127.0.0.1:${server.address().port}/v1`;

const env = { ...process.env, LEVELER_HOME: home };
delete env.LEVELER_CONFIG_DIR;
delete env.LEVELER_LANG;
const child = spawn(binary, ['desktop-bridge'], { stdio: ['pipe', 'pipe', 'inherit'], env });
const lines = createInterface({ input: child.stdout });
const pending = new Map();
let nextId = 0;
const responses = [];
lines.on('line', line => {
  const frame = JSON.parse(line);
  if (frame.event) return;
  const waiter = pending.get(String(frame.id));
  if (!waiter) return;
  pending.delete(String(frame.id));
  responses.push(frame);
  if (JSON.stringify(frame).includes(KEY)) seenKeyLeak = true;
  waiter.resolve(frame);
});
const request = (method, params = {}) => new Promise((resolve, reject) => {
  const id = String(++nextId);
  const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out`)); }, 60000);
  pending.set(id, { resolve: frame => { clearTimeout(timer); resolve(frame); } });
  child.stdin.write(JSON.stringify({ id, method, params }) + '\n');
});
const ok = async (method, params) => {
  const frame = await request(method, params);
  assert.equal(frame.ok, true, `${method} failed: ${JSON.stringify(frame.error)}`);
  return frame.result;
};
const fails = async (method, params, pattern) => {
  const frame = await request(method, params);
  assert.equal(frame.ok, false, `${method} must be refused`);
  if (pattern) assert.match(frame.error.message, pattern);
  return frame.error;
};
const configPath = path.join(home, 'config.toml');

try {
  const empty = await ok('providers', { workspace: null });
  assert.deepEqual(empty.providers, []);
  assert.deepEqual(empty.models, []);
  assert.equal(empty.configuration_complete, false);
  assert.equal(empty.config_exists, false);
  assert.equal(empty.presets.length, 5);
  assert.equal(empty.runtime.connected, false);
  evidence.steps.empty = { providers: 0, presets: empty.presets.length, note: 'first-run state is explicit, not a blank list' };

  // A draft probe verifies the endpoint before anything is written.
  const draftProbe = await ok('provider_models', {
    workspace: null,
    id: null,
    provider: { id: 'localmock', protocol: 'openai_chat', base_url: baseUrl, api_key_env: null, api_key: KEY },
  });
  assert.equal(draftProbe.status, 'ok', JSON.stringify(draftProbe));
  assert.deepEqual(draftProbe.models, ['alpha', 'beta']);
  assert.equal((await readFile(configPath, 'utf8').catch(() => null)), null, 'a probe must not create the config');
  evidence.steps.draft_probe = { status: draftProbe.status, models: draftProbe.models, wrote_config: false };

  const badProbe = await ok('provider_models', {
    workspace: null,
    id: null,
    provider: { id: 'localmock', protocol: 'openai_chat', base_url: baseUrl, api_key_env: null, api_key: 'sk-wrong' },
  });
  assert.equal(badProbe.status, 'auth', JSON.stringify(badProbe));
  evidence.steps.draft_probe_auth = { status: badProbe.status };

  const networkProbe = await ok('provider_models', {
    workspace: null,
    id: null,
    provider: { id: 'localmock', protocol: 'openai_chat', base_url: 'http://127.0.0.1:9/v1', api_key_env: null, api_key: KEY },
  });
  assert.equal(networkProbe.status, 'network', JSON.stringify(networkProbe));
  evidence.steps.draft_probe_network = { status: networkProbe.status };

  const added = await ok('provider_save', {
    workspace: null,
    mode: 'add',
    provider: {
      id: 'localmock', protocol: 'openai_chat', base_url: baseUrl,
      api_key_env: '', api_key: KEY,
      models: [{ id: 'alpha', context_window: 65536 }],
    },
  });
  const provider = added.providers.find(item => item.id === 'localmock');
  assert.ok(provider, JSON.stringify(added));
  assert.equal(provider.api_key_configured, true);
  assert.equal(provider.key_source, 'config');
  assert.equal(added.default_model, 'localmock/alpha');
  assert.equal(added.models.find(model => model.model === 'alpha').default, true);
  assert.equal(added.configuration_complete, true);
  const mode = statSync(configPath).mode & 0o777;
  assert.equal(mode, 0o600, `config must be owner-only: ${mode.toString(8)}`);
  const text = await readFile(configPath, 'utf8');
  assert.ok(text.includes(KEY), 'the key must be stored in the owner-only config');
  assert.ok(!JSON.stringify(responses).includes(KEY), 'no response may carry the key');
  evidence.steps.add = {
    provider: { id: provider.id, key_source: provider.key_source, models: provider.model_count },
    default_model: added.default_model,
    config_mode: mode.toString(8),
    responses_carry_key: seenKeyLeak,
  };

  // The saved provider's own key resolves through the same reader.
  const savedProbe = await ok('provider_models', { workspace: null, id: 'localmock', provider: null });
  assert.equal(savedProbe.status, 'ok');
  assert.deepEqual(savedProbe.models, ['alpha', 'beta']);
  evidence.steps.saved_probe = { status: savedProbe.status, models: savedProbe.models };

  // A real text + streaming request through the configured route.
  const connection = await ok('provider_test', { workspace: null, id: 'localmock', model: null });
  assert.equal(connection.status, 'ok', JSON.stringify(connection));
  assert.equal(connection.model, 'alpha');
  evidence.steps.connection_test = { status: connection.status, model: connection.model, text_ok: connection.text_ok, stream_ok: connection.stream_ok };

  // Editing without restating the key or endpoint preserves both.
  const edited = await ok('provider_save', {
    workspace: null,
    mode: 'edit',
    provider: {
      id: 'localmock', protocol: 'openai_chat', base_url: null,
      api_key_env: '', api_key: null,
      models: [{ id: 'beta', context_window: null }],
    },
  });
  const editedProvider = edited.providers.find(item => item.id === 'localmock');
  assert.equal(editedProvider.api_key_configured, true, 'an edit must not clear the stored key');
  assert.equal(edited.models.filter(model => model.provider === 'localmock').length, 2);
  assert.ok((await readFile(configPath, 'utf8')).includes(KEY));
  evidence.steps.edit_preserves_key = { models: editedProvider.model_count, key_configured: editedProvider.api_key_configured };

  // An empty key is refused without touching the file.
  const before = await readFile(configPath, 'utf8');
  await fails('provider_save', {
    workspace: null,
    mode: 'edit',
    provider: {
      id: 'localmock', protocol: 'openai_chat', base_url: null,
      api_key_env: '', api_key: '   ', models: [],
    },
  }, /API Key/);
  assert.equal(await readFile(configPath, 'utf8'), before);
  evidence.steps.empty_key_refused = true;

  // Default model persistence.
  const withDefault = await ok('default_model_set', { workspace: null, model: { provider: 'localmock', model: 'beta' } });
  assert.equal(withDefault.default_model, 'localmock/beta');
  assert.equal(withDefault.models.find(model => model.model === 'beta').default, true);
  evidence.steps.default_model = { default_model: withDefault.default_model };

  // Removal of the default-bound model clears the default with it.
  const removed = await ok('provider_remove', { workspace: null, id: 'localmock' });
  assert.deepEqual(removed.removed_models.sort(), ['alpha', 'beta']);
  assert.equal(removed.default_cleared, true);
  assert.deepEqual(removed.providers, []);
  assert.equal(removed.configuration_complete, false);
  evidence.steps.remove = { removed_models: removed.removed_models, default_cleared: removed.default_cleared };

  await fails('provider_remove', { workspace: null, id: 'localmock' }, /not configured|未/);
  evidence.steps.remove_unknown_refused = true;

  // Configuration path is the real global file for this HOME.
  const finalView = await ok('providers', { workspace: null });
  assert.equal(finalView.config_path, configPath);
  evidence.steps.final = { config_path: finalView.config_path, complete: finalView.configuration_complete };

  assert.equal(seenKeyLeak, false, 'the bridge response carried the key');
  assert.ok(!JSON.stringify(evidence).includes(KEY), 'evidence must not carry the key');
  evidence.result = 'PASS';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.message;
  throw error;
} finally {
  child.stdin.end();
  server.close();
  await writeFile(path.join(output, 'r2a-provider-bridge-evidence.json'), JSON.stringify(evidence, null, 2));
  await rm(home, { recursive: true, force: true });
}
