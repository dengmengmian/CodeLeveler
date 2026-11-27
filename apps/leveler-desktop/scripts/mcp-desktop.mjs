// R3a isolated Desktop acceptance: real Electron, real leveler runtime, real
// local MCP stdio and Streamable HTTP servers, temporary HOME/workspace.
//
// Nothing here calls a model. It proves the management/connection/discovery
// surface and CLI↔Desktop configuration consistency. The real-provider Agent
// call is covered by scripts/mcp-dogfood.mjs on the actual HOME.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { spawn, execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { createServer } from 'node:http';
import { createHash, randomUUID } from 'node:crypto';
import { appendFileSync, existsSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, realpath, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const binary = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.resolve(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-r3a-mcp-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
const canonicalWorkspace = await realpath(workspace);
const callsPath = path.join(workspace, 'mcp-calls.jsonl');
const providerLogPath = path.join(workspace, 'provider-requests.jsonl');
const configPath = path.join(home, 'config.toml');
const nodeBin = process.env.LEVELER_TEST_NODE ?? '/opt/homebrew/bin/node';
const stdioServer = path.join(root, 'scripts/mcp-test-server.mjs');
const httpServerScript = path.join(root, 'scripts/mcp-http-test-server.mjs');
const SECRET = 'r3a-secret-must-never-appear';
const HTTP_SECRET = 'r3a-http-secret-must-never-appear';

let activationPhase = 'idle';
let planPhase = 'idle';
let toolCallSeq = 0;
const sseText = text => [
  `data: ${JSON.stringify({ id: 'r3a', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { role: 'assistant', content: text }, finish_reason: null }] })}`,
  `data: ${JSON.stringify({ id: 'r3a', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';
const sseTools = calls => [
  `data: ${JSON.stringify({ id: 'r3a', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { role: 'assistant', tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } })) }, finish_reason: null }] })}`,
  `data: ${JSON.stringify({ id: 'r3a', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }] })}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';

const provider = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  let body = {};
  try { body = JSON.parse(raw); } catch { /* a malformed prediction call gets the setup answer */ }
  const tools = (body.tools ?? []).map(tool => tool.function?.name ?? tool.name).filter(Boolean);
  const hasMcp = tools.some(name => name.startsWith('mcp__'));
  const hasCapability = tools.includes('capability');
  try { appendFileSync(providerLogPath, `${JSON.stringify({ tools, activationPhase, planPhase })}\n`); } catch { /* test log only */ }
  let payload;
  if (activationPhase === 'request' && hasCapability && !hasMcp) {
    activationPhase = 'done';
    toolCallSeq += 1;
    payload = sseTools([{ id: `call_activate_${toolCallSeq}`, name: 'capability', arguments: { action: 'enable', id: 'external_tools' } }]);
  } else if (activationPhase === 'done' && hasMcp) {
    activationPhase = 'idle';
    payload = sseText('EXTERNAL_TOOLS_ENABLED');
  } else if (planPhase === 'call' && hasMcp) {
    planPhase = 'denied';
    toolCallSeq += 1;
    payload = sseTools([{ id: `call_plan_${toolCallSeq}`, name: 'mcp__r3a-renamed__echo', arguments: { text: 'PLAN_MUST_NOT_RUN' } }]);
  } else if (planPhase === 'denied') {
    planPhase = 'idle';
    payload = sseText('PLAN_DENIED_OK');
  } else {
    payload = sseText('R3A_SETUP_OK');
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.end(payload);
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
const configText = `# R3a isolated profile; comments and unrelated keys must survive\nlang="zh"\ndefault_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`;
await writeFile(configPath, configText, { mode: 0o600 });
const configHash = async () => createHash('sha256').update(await readFile(configPath)).digest('hex');

const serverCalls = async () => {
  if (!existsSync(callsPath)) return [];
  return (await readFile(callsPath, 'utf8')).split('\n').filter(Boolean).map(line => JSON.parse(line)).filter(entry => entry.tool && entry.tool !== 'undefined');
};
const callCount = async () => (await serverCalls()).length;
const providerRequests = async () => {
  if (!existsSync(providerLogPath)) return [];
  return (await readFile(providerLogPath, 'utf8')).split('\n').filter(Boolean).map(line => JSON.parse(line));
};

const configBefore = await configHash();

const evidence = {
  scope: 'isolated HOME + real Electron + real runtime + real local MCP servers; no model call',
  binary,
  home,
  workspace: canonicalWorkspace,
  config_sha256_before: configBefore,
  steps: {},
  failures: [],
};
let app;
let page;
let daemonPid = null;
const daemonPids = new Set();
let httpServer = null;
let httpPort = null;
const errors = [];
const createdSessions = [];
const testServers = [];

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: {
      ...process.env,
      ELECTRON_RUN_AS_NODE: undefined,
      LEVELER_HOME: home,
      LEVELER_CONFIG_DIR: undefined,
      LEVELER_BINARY: binary,
      MCP_TEST_TOKEN: SECRET,
      MCP_TEST_LOG: callsPath,
      MCP_HTTP_TOKEN: HTTP_SECRET,
      LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '30',
    },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
  daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo().catch(() => null)))?.pid ?? null;
  if (daemonPid) daemonPids.add(daemonPid);
};

const capture = async name => {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
};

const openSettings = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
};

const openMcp = async () => {
  await openSettings();
  await page.locator('#settings-nav-list button', { hasText: 'MCP 服务' }).click();
  await page.waitForSelector('#settings-content[data-section="mcp"]');
};

const card = name => page.locator(`.mcp-card[data-server="${name}"]`);

const waitState = (name, label) => page.waitForFunction(
  ([server, expected]) => document.querySelector(`.mcp-card[data-server="${server}"] .mcp-state-label`)?.textContent === expected,
  [name, label],
  { timeout: 60000 },
);

const waitCard = name => page.waitForSelector(`.mcp-card[data-server="${name}"]`, { timeout: 30000 });
const selectedId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');

async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => {
    dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] });
  }, canonicalWorkspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(canonicalWorkspace));
}


const receipts = () => page.locator('.turn-receipt').count();
async function waitTurn(before, { approval = 'deny', timeout = 180000 } = {}) {
  const started = Date.now();
  let approved = 0;
  let denied = 0;
  while (Date.now() - started < timeout) {
    const cardNode = page.locator('.approval-card').first();
    if (await cardNode.count()) {
      const button = cardNode.getByRole('button', { name: approval === 'deny' ? '拒绝' : '允许一次' });
      if (await button.count()) {
        await button.click();
        if (approval === 'deny') denied += 1; else approved += 1;
        await page.waitForTimeout(150);
        continue;
      }
    }
    if ((await receipts()) >= before + 1) {
      const status = await page.locator('#conversation-status').innerText();
      if (!['运行中', '等待你处理'].includes(status)) return { approved, denied };
    }
    await page.waitForTimeout(300);
  }
  throw new Error(`turn did not settle (approved=${approved}, denied=${denied})`);
}
async function sendTurn(text, options) {
  const before = await receipts();
  await page.locator('#message').fill(text);
  // A post-settings refresh can briefly freeze the composer; wait for the
  // real enabled state instead of racing that window.
  await page.waitForFunction(() => !document.querySelector('#send')?.disabled, null, { timeout: 30000 });
  await page.locator('#send').click();
  return waitTurn(before, options);
}
const triggerMode = () => page.locator('#mode-picker .trigger-label').innerText();
async function switchMode(label) {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForFunction(() => document.querySelector('#mode-picker') && !document.querySelector('#mode-picker').disabled, null, { timeout: 30000 });
    await page.locator('#mode-picker').click();
    try { await page.waitForSelector('.menu .menu-item', { timeout: 1000 }); break; } catch { await page.waitForTimeout(100); }
  }
  await page.locator('.menu .menu-item').filter({ hasText: new RegExp(`^${label}$`) }).first().click();
  await page.waitForFunction(expected => document.querySelector('#mode-picker .trigger-label')?.textContent === expected, label, { timeout: 30000 });
}


const addStdio = async (name, { command = nodeBin, argsText = stdioServer, envText = 'MCP_TEST_TOKEN=MCP_TEST_TOKEN' } = {}) => {
  await page.getByRole('button', { name: '添加服务' }).click();
  const dialog = page.locator('dialog.provider-editor');
  await dialog.waitFor();
  await dialog.getByPlaceholder('例如 filesystem').fill(name);
  await dialog.getByPlaceholder('例如 npx 或 /usr/local/bin/my-mcp-server').fill(command);
  await dialog.getByPlaceholder('每行一个参数').fill(argsText);
  await dialog.getByPlaceholder('每行一个 KEY=ENV_VAR；只写名称，不写密钥').fill(envText);
  await dialog.getByRole('button', { name: '添加', exact: true }).click();
  await dialog.waitFor({ state: 'detached', timeout: 30000 });
  testServers.push(name);
  await waitCard(name);
  return card(name);
};

const addHttp = async (name, url, bearer) => {
  await page.getByRole('button', { name: '添加服务' }).click();
  const dialog = page.locator('dialog.provider-editor');
  await dialog.waitFor();
  await dialog.getByPlaceholder('例如 filesystem').fill(name);
  await dialog.locator('select').selectOption('streamable_http');
  await dialog.getByPlaceholder('https://example.com/mcp').fill(url);
  await dialog.getByPlaceholder('例如 SERVICE_TOKEN').fill(bearer);
  await dialog.getByRole('button', { name: '添加', exact: true }).click();
  await dialog.waitFor({ state: 'detached', timeout: 30000 });
  testServers.push(name);
  await waitCard(name);
  return card(name);
};

const connect = async name => {
  await card(name).getByRole('button', { name: /连接测试|重新连接/ }).click();
  await waitState(name, '已连接');
};

const toolNames = async name => {
  const toolsButton = card(name).locator('button', { hasText: /^工具/ });
  if (await toolsButton.count()) await toolsButton.first().click();
  await page.waitForSelector(`.mcp-card[data-server="${name}"] .mcp-tool-name`, { timeout: 30000 });
  return page.locator(`.mcp-card[data-server="${name}"] .mcp-tool-name`).allInnerTexts();
};

const step = async (name, f) => {
  try {
    evidence.steps[name] = { result: await f() };
  } catch (error) {
    evidence.failures.push({ step: name, detail: error.stack ?? String(error) });
    throw error;
  }
};

const cli = (args, env = {}) => execFile(binary, args, {
  env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: '', ...env },
  maxBuffer: 8 * 1024 * 1024,
});

try {
  await launch();
  await step('D01_startup', async () => {
    await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, canonicalWorkspace);
    await page.locator('#add-workspace').click();
    await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(canonicalWorkspace));
    // The home composer creates the session in the selected workspace. A
    // message is required by the UI; the fixture provider answers it without
    // tools, so this turn never touches MCP.
    await page.locator('#message').fill('R3A_SETUP_ONLY');
    await page.locator('#send').click();
    await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
    await page.waitForFunction(() => {
      const status = document.querySelector('#conversation-status')?.textContent ?? '';
      return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
    }, null, { timeout: 120000 });
    const id = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
    createdSessions.push(id);
    const snapshot = await page.evaluate(sessionId => window.desktop.snapshot(sessionId), id);
    assert.equal(snapshot.repository, canonicalWorkspace, 'the session owns the real workspace runtime');
    daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo()))?.pid ?? daemonPid;
    if (daemonPid) daemonPids.add(daemonPid);
    return { session: id.slice(0, 8), repository: snapshot.repository };
  });

  await step('D02_empty_page', async () => {
    await openMcp();
    await page.waitForFunction(() => document.querySelector('#settings-content')?.innerText.includes('还没有 MCP 服务'));
    return { empty: true };
  });

  await step('D03_add_stdio', async () => {
    await addStdio('r3a-stdio');
    const text = await readFile(configPath, 'utf8');
    assert.match(text, /name = "r3a-stdio"/);
    assert.match(text, /MCP_TEST_TOKEN = "MCP_TEST_TOKEN"/);
    assert.ok(!text.includes(SECRET), 'the resolved token must never be written');
    assert.match(text, /# R3a isolated profile/, 'unrelated comments survive');
    const state = await card('r3a-stdio').locator('.mcp-state-label').innerText();
    assert.equal(state, '已配置，未连接', 'config is not a connection');
    return { state, config_has_env_ref: true };
  });

  await step('D04_connect_and_tools', async () => {
    await connect('r3a-stdio');
    const names = await toolNames('r3a-stdio');
    assert.deepEqual(names.sort(), ['mcp__r3a-stdio__add', 'mcp__r3a-stdio__big', 'mcp__r3a-stdio__echo', 'mcp__r3a-stdio__env_probe', 'mcp__r3a-stdio__fail']);
    const body = await card('r3a-stdio').innerText();
    assert.match(body, /必填 text/, body);
    assert.match(body, /协议：2024-11-05/, body);
    assert.ok(!body.includes(SECRET), 'the page never carries the token value');
    await capture('r3a-01-connected-tools.png');
    return { tools: names, protocol: '2024-11-05' };
  });

  await step('D05_broken_server_is_not_success', async () => {
    await addStdio('r3a-broken', { command: '/usr/bin/false', argsText: '', envText: '' });
    await card('r3a-broken').getByRole('button', { name: /连接测试|重新连接/ }).click();
    await waitState('r3a-broken', '连接失败');
    const text = await card('r3a-broken').innerText();
    assert.match(text, /连接失败/);
    assert.match(text, /最近一次连接尝试失败/);
    return { state: '连接失败' };
  });

  await step('D06_disable_and_enable', async () => {
    await card('r3a-stdio').locator('.switch').click();
    await waitState('r3a-stdio', '已停用');
    const connectButton = card('r3a-stdio').getByRole('button', { name: /连接测试|重新连接/ });
    assert.equal(await connectButton.isDisabled(), true);
    await card('r3a-stdio').locator('.switch').click();
    await waitState('r3a-stdio', '已配置，未连接');
    await connect('r3a-stdio');
    return { disabled: true, reenabled: true };
  });

  await step('D07_cli_cross_process_adds', async () => {
    await Promise.all(Array.from({ length: 6 }, (_, index) => cli([
      'mcp', 'add', `cli-${index}`, '--', nodeBin, stdioServer,
    ])));
    const text = await readFile(configPath, 'utf8');
    for (let index = 0; index < 6; index += 1) {
      assert.match(text, new RegExp(`name = "cli-${index}"`), `concurrent CLI add ${index} survived`);
      testServers.push(`cli-${index}`);
    }
    await page.getByRole('button', { name: '刷新' }).click();
    for (let index = 0; index < 6; index += 1) await waitCard(`cli-${index}`);
    // Reload makes the runtime consume the CLI-written configuration, then a
    // GUI connect proves it is the same declaration.
    await page.getByRole('button', { name: '重新加载配置' }).click();
    await page.waitForFunction(() => document.querySelector('#settings-content')?.innerText.includes('已重新加载配置'));
    await connect('cli-0');
    return { concurrent: 6, reloaded: true, connected: 'cli-0' };
  });

  await step('D08_edit_rename', async () => {
    await card('r3a-stdio').getByRole('button', { name: '编辑' }).click();
    const dialog = page.locator('dialog.provider-editor');
    await dialog.waitFor();
    await dialog.locator('.provider-editor-row', { hasText: '服务名' }).locator('input').fill('r3a-renamed');
    await dialog.getByRole('button', { name: '保存', exact: true }).click();
    await dialog.waitFor({ state: 'detached', timeout: 30000 });
    testServers.push('r3a-renamed');
    await waitCard('r3a-renamed');
    await waitState('r3a-renamed', '已配置，未连接');
    const text = await readFile(configPath, 'utf8');
    assert.ok(!text.includes('name = "r3a-stdio"'), 'the old declaration is gone');
    await connect('r3a-renamed');
    return { renamed: true, reconnected: true };
  });

  await step('D09_http_transport', async () => {
    httpServer = spawn(nodeBin, [httpServerScript], {
      env: { ...process.env, MCP_HTTP_TOKEN: HTTP_SECRET },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    httpServer.stderr.on('data', chunk => process.stderr.write(chunk));
    httpPort = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error('HTTP MCP fixture did not listen')), 10000);
      let buffer = '';
      httpServer.stdout.on('data', chunk => {
        buffer += chunk;
        const match = /LISTENING (\d+)/.exec(buffer);
        if (match) { clearTimeout(timer); resolve(Number(match[1])); }
      });
      httpServer.once('exit', code => reject(new Error(`HTTP MCP fixture exited (${code})`)));
    });
    await addHttp('r3a-http', `http://127.0.0.1:${httpPort}/mcp`, 'MCP_HTTP_TOKEN');
    await connect('r3a-http');
    const names = await toolNames('r3a-http');
    assert.deepEqual(names.sort(), ['mcp__r3a-http__http_add', 'mcp__r3a-http__http_echo', 'mcp__r3a-http__http_env']);
    const text = await card('r3a-http').innerText();
    assert.match(text, /Streamable HTTP/);
    assert.match(text, /bearer_token_env ← MCP_HTTP_TOKEN/);
    assert.ok(!text.includes(HTTP_SECRET));
    await page.locator('#settings-close').click();
    return { port: httpPort, tools: names };
  });

  await step('D09b_plan_approval_denied', async () => {
    // Activate external_tools in Chat first (the fixture provider emits one
    // real capability call), then switch to Plan. In Plan the provider emits
    // one MCP echo call; the Desktop must show an approval card, and denying
    // it must keep the call away from the server.
    activationPhase = 'request';
    await sendTurn('R3A_ACTIVATE_EXTERNAL_TOOLS');
    assert.equal(activationPhase, 'idle', 'the external_tools capability was enabled');
    await switchMode('计划');
    planPhase = 'call';
    const before = await callCount();
    const requestMark = (await providerRequests()).length;
    const turn = await sendTurn('R3A_PLAN_MCP 请调用 echo', { approval: 'deny' });
    const planRequests = (await providerRequests()).slice(requestMark).filter(request => request.planPhase === 'call');
    const exposed = planRequests.some(request => request.tools.some(name => name.startsWith('mcp__')));
    if (exposed) {
      assert.equal(turn.denied >= 1, true, 'an exposed Plan MCP call reached the approval boundary');
    } else {
      // The stronger product behavior: the Plan surface does not offer MCP at
      // all, so there is nothing to approve and nothing can reach the server.
      assert.equal(turn.denied, 0, 'no approval card without an exposed MCP tool');
    }
    const calls = (await serverCalls()).slice(before);
    assert.equal(calls.filter(call => call.tool === 'echo' && call.args?.text === 'PLAN_MUST_NOT_RUN').length, 0, 'the denied Plan call never reached the server');
    await switchMode('对话');
    await capture('r3a-02-plan-denied.png');
    return { denied: turn.denied, tools_exposed: exposed, request_tools: planRequests.at(-1)?.tools ?? [], server_called: false };
  });

  await step('D09c_reload_failure_is_not_success', async () => {
    // A malformed file must produce an error, never a "configuration reloaded"
    // success. Restore the exact bytes and prove the next reload applies.
    await openMcp();
    const original = await readFile(configPath, 'utf8');
    try {
      await writeFile(configPath, `${original}\n[[mcp_servers]\n`);
      await page.getByRole('button', { name: '重新加载配置' }).click();
      await page.waitForFunction(() => {
        const banner = document.querySelector('#settings-content .schedule-banner.error');
        return (banner?.textContent ?? '').includes('configuration could not be applied');
      }, null, { timeout: 30000 });
      const body = await page.locator('#settings-content').innerText();
      assert.ok(!body.includes('已重新加载配置'), `a failed reload must not toast success: ${body}`);
    } finally {
      await writeFile(configPath, original, { mode: 0o600 });
    }
    await page.getByRole('button', { name: '重新加载配置' }).click();
    await page.waitForFunction(() => (document.querySelector('#settings-content')?.innerText ?? '').includes('已重新加载配置'), null, { timeout: 30000 });
    return { failed_reload_reported: true, restored: true };
  });

  await step('D09d_multi_session_switch', async () => {
    // A second workspace session shares the same daemon but has its own
    // selected session. Switching back must rebind the MCP projection instead
    // of showing the previous session's rows as if they were this one's.
    await page.locator('#settings-close').click();
    const first = createdSessions[0];
    // The workspace group's own "new session" action creates the second
    // session in the SAME workspace (never a no-workspace session).
    await page.locator(`.workspace-group[data-workspace-path="${canonicalWorkspace}"] .group-new`).click();
    await page.waitForFunction(
      previous => {
        const row = document.querySelector('.session-row[aria-selected="true"]');
        return row && row.dataset.sessionId !== previous;
      },
      first,
      { timeout: 30000 },
    );
    const second = await selectedId();
    assert.notEqual(second, first);
    createdSessions.push(second);
    await page.waitForSelector(`.session-row[data-session-id="${first}"]`, { timeout: 30000 });
    await page.locator(`.session-row[data-session-id="${first}"]`).click();
    await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === id, first, { timeout: 30000 });
    await openMcp();
    await waitCard('r3a-renamed');
    await page.locator('#settings-close').click();
    return { second: second.slice(0, 8), rebound: true };
  });

  await step('D10_restart_rebuilds', async () => {
    const info = await page.evaluate(() => window.desktop.runtimeInfo());
    const sessionId = createdSessions[0];
    await Promise.race([app.close().catch(() => {}), new Promise(resolve => setTimeout(resolve, 8000))]);
    try { app.process()?.kill('SIGKILL'); } catch { /* gone */ }
    app = null;
    try { process.kill(info.pid, 'SIGKILL'); } catch { /* already gone */ }
    await new Promise(resolve => setTimeout(resolve, 1200));
    await launch();
    await page.waitForSelector(`.session-row[data-session-id="${sessionId}"]`, { timeout: 60000 });
    await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
    await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === id, sessionId, { timeout: 60000 });
    daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo()))?.pid ?? daemonPid;
    if (daemonPid) daemonPids.add(daemonPid);
    await openMcp();
    await waitCard('r3a-renamed');
    const state = await card('r3a-renamed').locator('.mcp-state-label').innerText();
    assert.equal(state, '已配置，未连接', 'a restarted runtime has no connection until it attempts one');
    await connect('r3a-renamed');
    await waitCard('r3a-http');
    await connect('r3a-http');
    return { before_restart: state, tools_after_restart: (await toolNames('r3a-renamed')).length };
  });

  await step('D11_delete_and_cleanup', async () => {
    await card('r3a-broken').getByRole('button', { name: '删除' }).click();
    await page.locator('.confirm-modal').getByRole('button', { name: '删除', exact: true }).click();
    await card('r3a-broken').waitFor({ state: 'detached', timeout: 30000 });
    let text = await readFile(configPath, 'utf8');
    assert.ok(!text.includes('name = "r3a-broken"'));
    // The other test declarations are removed through the same CLI writer,
    // leaving the unrelated provider/model configuration untouched.
    for (const name of testServers.filter(name => name !== 'r3a-broken')) {
      await cli(['mcp', 'remove', name]).catch(() => {});
    }
    text = await readFile(configPath, 'utf8');
    assert.ok(!/name = "(r3a-|cli-)/.test(text), text);
    assert.match(text, /\[providers\.fixture\]/);
    assert.match(text, /# R3a isolated profile/);
    assert.ok(!text.includes(SECRET) && !text.includes(HTTP_SECRET));
    return { deleted: true, config_clean: true };
  });

  await step('D12_secret_hygiene', async () => {
    const body = await page.evaluate(() => document.body.innerText);
    assert.ok(!body.includes(SECRET) && !body.includes(HTTP_SECRET), 'the page never renders a secret value');
    const storage = await page.evaluate(() => JSON.stringify(localStorage));
    assert.ok(!storage.includes(SECRET) && !storage.includes(HTTP_SECRET), 'local storage never holds a secret value');
    const text = await readFile(configPath, 'utf8');
    assert.ok(!text.includes(SECRET) && !text.includes(HTTP_SECRET));
    return { page: 'clean', localStorage: 'clean', config: 'clean' };
  });

  evidence.config_sha256_after = await configHash();
  assert.deepEqual(errors, [], 'no renderer page errors');
  evidence.result = evidence.failures.length === 0 ? 'PASS' : 'FAIL_PARTIAL';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r3a-isolated-failure.png').catch(() => {});
  throw error;
} finally {
  await writeFile(path.join(output, 'mcp-desktop-evidence.json'), JSON.stringify(evidence, null, 2));
  for (const id of createdSessions) {
    await page?.evaluate(sessionId => window.desktop.deliver({
      command_id: crypto.randomUUID(),
      session_id: sessionId,
      expected_version: null,
      issued_at: new Date().toISOString(),
      command: { type: 'archive_session', session_id: sessionId },
    }), id).catch(() => {});
  }
  await Promise.race([app?.close().catch(() => {}), new Promise(resolve => setTimeout(resolve, 8000))]);
  try { app?.process()?.kill('SIGKILL'); } catch { /* gone */ }
  for (const pid of daemonPids) { try { process.kill(pid, 'SIGKILL'); } catch { /* already gone */ } }
  try { httpServer?.kill('SIGKILL'); } catch { /* gone */ }
  // A failed run can stop before the GUI cleanup step. Remove every test
  // declaration through the same CLI writer; the temp HOME is disposable, but
  // a re-run must start from the original profile.
  for (const name of [...new Set(testServers)]) await cli(['mcp', 'remove', name]).catch(() => {});
  provider.close();
  await writeFile(path.join(output, 'mcp-desktop-evidence.json'), JSON.stringify(evidence, null, 2));
}
