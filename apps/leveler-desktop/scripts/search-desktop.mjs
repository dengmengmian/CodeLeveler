// Real Desktop + Runtime search settings: save through the settings surface,
// then prove the Agent's web_search tool actually reaches the configured
// endpoint. The model and search upstreams are local test servers; Electron,
// the bridge, the runtime, config locking and the tool surface are real.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-search-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'a.txt'), 'alpha\n');

const searchRequests = [];
const searchServer = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  searchRequests.push({ url: request.url, headers: request.headers, body: raw });
  response.writeHead(200, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ results: [{ title: 'R4 search result', url: 'https://example.test/r4', snippet: 'R4_SEARCH_SNIPPET', date: null }] }));
});
await new Promise(resolve => searchServer.listen(0, '127.0.0.1', resolve));
const searchPort = searchServer.address().port;

const modelRequests = [];
const modelServer = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  let body = {};
  try { body = JSON.parse(raw); } catch { /* probe calls may be empty */ }
  const tools = (body.tools ?? []).map(tool => tool.function?.name);
  modelRequests.push({ tools, messages: body.messages?.length ?? 0 });
  const lastUser = [...(body.messages ?? [])].reverse().find(message => message.role === 'user');
  const userText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
  const toolTurns = (body.messages ?? []).filter(message => message.role === 'tool').length;
  const wantsSearch = userText.includes('R4_SEARCH');
  const hasSearchTool = tools.includes('web_search');
  const toolText = JSON.stringify((body.messages ?? []).filter(message => message.role === 'tool'));
  const sawSearchResult = toolText.includes('R4_SEARCH_SNIPPET');
  const emitToolCall = (name, args, id) => {
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.write(`data: ${JSON.stringify({ id: 'search', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { role: 'assistant', tool_calls: [{ index: 0, id, type: 'function', function: { name, arguments: JSON.stringify(args) } }] }, finish_reason: null }] })}\n\n`);
    response.end(`data: ${JSON.stringify({ id: 'search', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] })}\n\ndata: [DONE]\n\n`);
  };
  if (wantsSearch && !hasSearchTool && tools.includes('capability')) {
    emitToolCall('capability', { action: 'enable', id: 'web' }, 'call-enable-web');
    return;
  }
  if (wantsSearch && hasSearchTool && !sawSearchResult) {
    emitToolCall('web_search', { query: 'R4_SEARCH_QUERY', count: 3 }, 'call-search');
    return;
  }
  const text = sawSearchResult ? 'SEARCH_DONE R4_SEARCH_SNIPPET' : 'OK';
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.write(`data: ${JSON.stringify({ id: 'search', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { content: text }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ id: 'search', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => modelServer.listen(0, '127.0.0.1', resolve));

await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${modelServer.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });
const configPath = path.join(home, 'config.toml');

const evidence = { scope: 'real Electron + real bridge/runtime/config lock; local model and search servers' };
const errors = [];
let app;
let page;
async function launch() {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'full_access';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } });
}
async function openSearchSettings() {
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
  await page.locator('#settings-nav-list button').filter({ hasText: '搜索' }).click();
  await page.waitForSelector('input[aria-label="搜索 API Key"]');
}
try {
  await launch();
  await openSearchSettings();
  const before = await page.evaluate(() => window.desktop.searchSettings());
  assert.equal(before.configured, false);
  assert.equal(before.api_key_configured, false);
  evidence.initial_unconfigured = true;

  // Enable through the actual controls: switch, provider default (deepseek),
  // a key, and an advanced endpoint pointing at the local search server.
  const enableRow = page.locator('.setting-row').filter({ hasText: '启用网页搜索' }).first();
  await enableRow.locator('.switch').click();
  await page.waitForFunction(() => { const input = document.querySelector('input[aria-label="搜索 API Key"]'); return input && !input.disabled; });
  await page.locator('input[aria-label="搜索 API Key"]').fill('sk-r4-search-test');
  await page.locator('details.search-advanced summary').click();
  await page.locator('input[aria-label="API 地址"]').fill(`http://127.0.0.1:${searchPort}`);
  await page.locator('input[aria-label="结果数量"]').fill('3');
  await page.locator('input[aria-label="结果数量"]').press('Tab');
  await page.getByRole('button', { name: '保存搜索设置' }).click();
  await page.waitForFunction(() => document.querySelector('.search-save-note')?.textContent.includes('已保存'), null, { timeout: 30000 });
  const savedNote = await page.locator('.search-save-note').innerText();
  evidence.save_note = savedNote;

  const configText = await readFile(configPath, 'utf8');
  assert.match(configText, /\[web\.search\]/);
  assert.match(configText, /provider = "deepseek"/);
  assert.match(configText, /api_key = "sk-r4-search-test"/);
  assert.match(configText, new RegExp(`base_url = "http://127\\.0\\.0\\.1:${searchPort}"`));
  assert.match(configText, /max_results = 3/);
  evidence.config_written = true;

  const after = await page.evaluate(() => window.desktop.searchSettings());
  assert.equal(after.configured, true);
  assert.equal(after.provider, 'deepseek');
  assert.equal(after.api_key_configured, true);
  assert.equal(after.key_source, 'stored');
  assert.equal(Object.hasOwn(after, 'api_key'), false, 'the key must never come back');
  evidence.masked_projection = { configured: after.configured, provider: after.provider, api_key_configured: after.api_key_configured, key_source: after.key_source, max_results: after.max_results, base_url: after.base_url };
  await page.screenshot({ path: path.join(output, 'search-settings.png') });

  // The saved endpoint must be what the Agent's real web_search tool uses.
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('请用 web_search 搜索 R4_SEARCH 验证搜索配置');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SEARCH_DONE'), null, { timeout: 120000 });
  assert.ok(searchRequests.length >= 1, 'the search endpoint was never called');
  const searchBody = JSON.parse(searchRequests[0].body);
  assert.equal(searchBody.query, 'R4_SEARCH_QUERY');
  assert.equal(searchRequests[0].url, '/search');
  evidence.agent_called_search = { requests: searchRequests.length, query: searchBody.query, url: searchRequests[0].url };
  const assistantText = await page.locator('.message.assistant .markdown').last().innerText();
  assert.match(assistantText, /SEARCH_DONE/);
  await page.screenshot({ path: path.join(output, 'search-agent-result.png') });

  // A restart must keep both the settings and the tool's availability.
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await app.evaluate(({ app }) => app.exit(0));
  await new Promise(resolve => setTimeout(resolve, 1500));
  await launch();
  await page.waitForSelector(`.session-row[data-session-id="${sessionId}"]`);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await openSearchSettings();
  const restarted = await page.evaluate(() => window.desktop.searchSettings());
  assert.equal(restarted.configured, true);
  assert.equal(restarted.api_key_configured, true);
  assert.equal(restarted.max_results, 3);
  evidence.restart_kept_settings = true;
  // Disable and confirm the section is removed, not merely hidden from the UI.
  const enableAgain = page.locator('.setting-row').filter({ hasText: '启用网页搜索' }).first();
  await enableAgain.locator('.switch').click();
  await page.getByRole('button', { name: '保存搜索设置' }).click();
  await page.waitForFunction(() => document.querySelector('.search-save-note')?.textContent.includes('已保存'));
  const disabledText = await readFile(configPath, 'utf8');
  assert.doesNotMatch(disabledText, /\[web\.search\]/);
  assert.match(disabledText, /default_model/);
  evidence.disable_removes_section = true;
  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'search-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, model_requests: modelRequests.slice(0, 8), search_calls: searchRequests.length, last_status: page && !page.isClosed() ? await page.locator('#conversation-status').innerText().catch(() => null) : null }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
  await new Promise(resolve => searchServer.close(resolve));
  await new Promise(resolve => modelServer.close(resolve));
}
