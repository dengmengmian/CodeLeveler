// R3a real-local dogfood: the current machine's real HOME/profile.
//
// Real configured provider/model, real Desktop, real daemon, real workspace and
// real history. The only new MCP server is a real local process whose
// operations are in-memory; it logs its own calls so the Agent's invocation is
// proven by the server, not by model prose. The real config file is hashed
// before/after and the test server is removed through the same GUI that added
// it. No secret value is ever read, logged or asserted by content.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const head = execFile('git', ['rev-parse', 'HEAD'], { cwd: path.join(root, '../..') }).then(({ stdout }) => stdout.trim());
const workspace = await mkdtemp('/tmp/leveler-r3a-dogfood-');
const callsPath = path.join(workspace, 'mcp-calls.jsonl');
const configPath = path.join(os.homedir(), '.leveler', 'config.toml');
const configBytes = readFileSync(configPath);
const configHash = () => createHash('sha256').update(readFileSync(configPath)).digest('hex');
const configBefore = configHash();
const marker = `R3A_SYNTHETIC_TOKEN_${randomUUID().replaceAll('-', '')}`;
const serverName = `r3a-dog-${randomUUID().slice(0, 8)}`;
const nodeBin = process.env.LEVELER_TEST_NODE ?? '/opt/homebrew/bin/node';
const serverScript = path.join(root, 'scripts/mcp-test-server.mjs');

await writeFile(path.join(workspace, 'README.md'), '# R3a MCP dogfood workspace\n\nThis workspace exists only for the R3a acceptance.\n');

const evidence = {
  scope: 'real local HOME/profile, real provider/model, real Desktop/Runtime; temp workspace; only a new session and one test MCP server',
  binary: { path: leveler, sha256: binarySha },
  head: await head,
  workspace,
  server_name: serverName,
  config_sha256_before: configBefore,
  steps: {},
  failures: [],
};
let app;
let page;
let daemonPid = null;
const daemonPids = new Set();
const errors = [];
const createdSessions = [];
const serverCalls = async () => {
  if (!existsSync(callsPath)) return [];
  return (await readFile(callsPath, 'utf8')).split('\n').filter(Boolean).map(line => JSON.parse(line)).filter(entry => entry.tool && entry.tool !== 'undefined');
};
const callCount = async () => (await serverCalls()).length;

const launch = async () => {
  const env = {
    ...process.env,
    ELECTRON_RUN_AS_NODE: undefined,
    LEVELER_BINARY: leveler,
    MCP_TEST_TOKEN: marker,
    MCP_TEST_LOG: callsPath,
  };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(60000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
  daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo().catch(() => null)))?.pid ?? null;
  if (daemonPid) daemonPids.add(daemonPid);
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 60000 });
};
const capture = async name => {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
};
const selectedId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const taskCount = async () => (await page.evaluate(() => window.desktop.listTasks({ include_archived: false }))).tasks?.length ?? 0;
const receipts = () => page.locator('.turn-receipt').count();
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 900000 });
async function expandExecutionGroups() {
  const groups = page.locator('.execution-group');
  const count = await groups.count();
  for (let index = 0; index < count; index += 1) {
    const group = groups.nth(index);
    if ((await group.getAttribute('open')) !== null) continue;
    const toggle = group.locator('.execution-toggle');
    if (await toggle.count()) await toggle.click().catch(() => {});
    else await group.locator(':scope > summary').click().catch(() => {});
    await page.waitForTimeout(60);
  }
  await page.waitForTimeout(150);
  const rows = page.locator('.tool-row');
  const rowCount = await rows.count();
  for (let index = 0; index < rowCount; index += 1) {
    const row = rows.nth(index);
    if ((await row.getAttribute('open')) !== null) continue;
    await row.locator(':scope > summary').click().catch(() => {});
  }
}
/** Settle one turn. `approval` decides what to do with a human approval:
 *  "allow" grants exactly one action, "deny" refuses it. */
async function waitTurn(before, { approval = 'allow', timeout = 900000 } = {}) {
  const started = Date.now();
  let approved = 0;
  let denied = 0;
  while (Date.now() - started < timeout) {
    const card = page.locator('.approval-card').first();
    if (await card.count()) {
      const choice = approval === 'deny' ? '拒绝' : '允许一次';
      const button = card.getByRole('button', { name: choice });
      if (await button.count()) {
        await button.click();
        if (approval === 'deny') denied += 1; else approved += 1;
        await page.waitForTimeout(200);
        continue;
      }
    }
    const count = await page.locator('.turn-receipt').count();
    if (count >= before + 1) {
      const status = await page.locator('#conversation-status').innerText();
      if (!['运行中', '等待你处理'].includes(status)) {
        await idle();
        return { approved, denied };
      }
    }
    await page.waitForTimeout(400);
  }
  throw new Error(`turn did not settle (approved=${approved}, denied=${denied})`);
}
async function sendTurn(text, options) {
  const before = await receipts();
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  return waitTurn(before, options);
}
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
  { timeout: 90000 },
);
async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
const triggerMode = () => page.locator('#mode-picker .trigger-label').innerText();
async function openModeMenu() {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForFunction(() => document.querySelector('#mode-picker') && !document.querySelector('#mode-picker').disabled, null, { timeout: 30000 });
    await page.locator('#mode-picker').click();
    try {
      await page.waitForSelector('.menu .menu-item', { timeout: 1000 });
      return;
    } catch {
      await page.waitForTimeout(100);
    }
  }
  throw new Error('the execution-mode menu did not open');
}
async function switchMode(label) {
  await openModeMenu();
  await page.locator('.menu .menu-item').filter({ hasText: new RegExp(`^${label}$`) }).first().click();
  await page.waitForFunction(expected => document.querySelector('#mode-picker .trigger-label')?.textContent === expected, label, { timeout: 30000 });
}
const step = async (name, f) => {
  try {
    evidence.steps[name] = { result: await f() };
  } catch (error) {
    evidence.failures.push({ step: name, detail: error.stack ?? String(error) });
    throw error;
  }
};
async function addTestServer() {
  await page.getByRole('button', { name: '添加服务' }).click();
  const dialog = page.locator('dialog.provider-editor');
  await dialog.waitFor();
  await dialog.getByPlaceholder('例如 filesystem').fill(serverName);
  await dialog.getByPlaceholder('例如 npx 或 /usr/local/bin/my-mcp-server').fill(nodeBin);
  await dialog.getByPlaceholder('每行一个参数').fill(serverScript);
  await dialog.getByPlaceholder('每行一个 KEY=ENV_VAR；只写名称，不写密钥').fill('MCP_TEST_TOKEN=MCP_TEST_TOKEN');
  await dialog.getByRole('button', { name: '添加', exact: true }).click();
  await dialog.waitFor({ state: 'detached', timeout: 30000 });
  await page.waitForSelector(`.mcp-card[data-server="${serverName}"]`, { timeout: 30000 });
}
async function connectTestServer() {
  await card(serverName).getByRole('button', { name: /连接测试|重新连接/ }).click();
  await waitState(serverName, '已连接');
}
async function removeTestServer() {
  await card(serverName).getByRole('button', { name: '删除' }).click();
  await page.locator('.confirm-modal').getByRole('button', { name: '删除', exact: true }).click();
  await card(serverName).waitFor({ state: 'detached', timeout: 30000 });
}
const mcpRows = () => page.locator('.tool-row[data-kind="mcp"]');
/** Resolve a tool row by its structured model-facing name. Opening a row is a
 *  separate step; the identity comes from the wire name, not the display text. */
async function mcpRow(tool) {
  for (const row of await mcpRows().all()) {
    if ((await row.getAttribute('data-tool-name'))?.endsWith(`__${tool}`)) return row;
  }
  return null;
}

try {
  await launch();
  const tasksBefore = await taskCount();
  assert.ok(tasksBefore > 0, 'the real profile already has sessions');
  const modelIndex = await page.evaluate(() => window.desktop.models(null)).catch(() => null);
  const configuredModels = modelIndex?.models ?? [];
  evidence.model = configuredModels[0] ?? null;
  evidence.steps.D01_startup = { tasks: tasksBefore, model: evidence.model, configured_models: configuredModels.length };
  await capture('r3a-dog-01-startup.png');

  await step('D02_real_profile_mcp_list', async () => {
    await addWorkspace();
    await sendTurn('R3A_DOGFOOD_SETUP 这是真实工作区的准备消息。请只回复 READY，不要使用任何工具。');
    const id = await selectedId();
    createdSessions.push(id);
    const snapshot = await page.evaluate(sessionId => window.desktop.snapshot(sessionId), id);
    assert.ok(snapshot.repository?.endsWith(path.basename(workspace)), `the session owns the real workspace: ${snapshot.repository}`);
    evidence.model = snapshot.model ?? evidence.model;
    daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo()))?.pid ?? daemonPid;
    if (daemonPid) daemonPids.add(daemonPid);
    await openMcp();
    await page.waitForFunction(() => document.querySelector('#settings-content')?.innerText.includes('MCP 服务'));
    evidence.steps.D02_real_profile_mcp_list = { session: id.slice(0, 8), initial_rows: await page.locator('.mcp-card').count() };
    return { session: id.slice(0, 8), initial_rows: await page.locator('.mcp-card').count() };
  });

  let sessionId;
  await step('D03_add_and_connect', async () => {
    sessionId = createdSessions[0];
    await addTestServer();
    const text = await readFile(configPath, 'utf8');
    assert.match(text, new RegExp(`name = "${serverName}"`));
    assert.match(text, /MCP_TEST_TOKEN = "MCP_TEST_TOKEN"/);
    assert.ok(!text.includes(marker), 'the synthetic token value never enters the config');
    await connectTestServer();
    // Adding a server selects it, so its tool list is already open. Only ask
    // for it when a later render collapsed the card.
    if (!(await page.locator(`.mcp-card[data-server="${serverName}"] .mcp-tool-name`).count())) {
      await card(serverName).locator('button', { hasText: /工具|收起工具/ }).first().click();
    }
    await page.waitForSelector(`.mcp-card[data-server="${serverName}"] .mcp-tool-name`, { timeout: 30000 });
    const tools = await page.locator(`.mcp-card[data-server="${serverName}"] .mcp-tool-name`).allInnerTexts();
    for (const expected of ['echo', 'add', 'big', 'fail', 'env_probe']) {
      assert.ok(tools.some(tool => tool.endsWith(`__${expected}`)), `${expected} discovered: ${tools}`);
    }
    await capture('r3a-dog-02-connected.png');
    return { tools };
  });

  await step('D04_agent_echo_and_add', async () => {
    await page.locator('#settings-close').click();
    const before = await callCount();
    const turn = await sendTurn([
      `R3A_MCP_DOGFOOD 请使用真实 MCP 服务完成工具调用。`,
      `如果 external_tools 能力尚未启用，先调用 capability 启用它。`,
      `然后依次调用 mcp__${serverName}__echo（参数 text="R3A_ROUNDTRIP_OK"）、mcp__${serverName}__add（参数 a=19, b=23）和 mcp__${serverName}__env_probe。`,
      `最后如实报告三个工具的真实返回。不要修改文件，不要提问。`,
    ].join('\n'));
    await expandExecutionGroups();
    try {
      await page.waitForSelector('.tool-row[data-kind="mcp"]', { timeout: 30000 });
    } catch (error) {
      evidence.steps.D04_dom = await page.evaluate(() => ({
        tool_rows: [...document.querySelectorAll('.tool-row')].map(row => ({ kind: row.dataset.kind ?? null, text: row.innerText.slice(0, 120) })),
        groups: [...document.querySelectorAll('.execution-group')].map(group => ({ variant: group.dataset.variant ?? null, open: group.hasAttribute('open') })),
      }));
      throw error;
    }
    const echoRow = await mcpRow('echo');
    const addRow = await mcpRow('add');
    const envRow = await mcpRow('env_probe');
    assert.ok(echoRow, 'a real echo MCP tool row exists');
    assert.ok(addRow, 'a real add MCP tool row exists');
    assert.ok(envRow, 'a real env_probe MCP tool row exists');
    if ((await echoRow.getAttribute('open')) === null) await echoRow.locator(':scope > summary').click();
    if ((await addRow.getAttribute('open')) === null) await addRow.locator(':scope > summary').click();
    if ((await envRow.getAttribute('open')) === null) await envRow.locator(':scope > summary').click();
    const echoText = await echoRow.innerText();
    const addText = await addRow.innerText();
    assert.match(echoText, /MCP · .+ \/ echo/, echoText);
    assert.match(echoText, /R3A_ROUNDTRIP_OK/, echoText);
    assert.match(addText, /42/, addText);
    assert.match(await envRow.innerText(), /token_present/, 'the env probe answered with its structured fact');
    assert.ok(!(await envRow.innerText()).includes(marker), 'the env probe never returns the token value');
    const calls = (await serverCalls()).slice(before);
    assert.ok(calls.some(call => call.tool === 'echo' && call.args?.text === 'R3A_ROUNDTRIP_OK' && call.outcome === 'ok'), JSON.stringify(calls));
    assert.ok(calls.some(call => call.tool === 'add' && Number(call.args?.a) === 19 && Number(call.args?.b) === 23 && call.outcome === 'ok'), JSON.stringify(calls));
    assert.ok(calls.some(call => call.tool === 'env_probe' && call.outcome === 'ok'), JSON.stringify(calls));
    await capture('r3a-dog-03-agent-mcp.png');
    return { approved: turn.approved, server_calls: calls.map(call => call.tool), echo_visible: true, add_visible: true };
  });

  await step('D05_full_output', async () => {
    const before = await callCount();
    await sendTurn(`R3A_MCP_DOGFOOD 再调用 mcp__${serverName}__big，参数 lines=6000。拿到结果后只回复 BIG_DONE，不要复述全部内容。`);
    await expandExecutionGroups();
    const bigRow = await mcpRow('big');
    assert.ok(bigRow, 'the big MCP tool row exists');
    if ((await bigRow.getAttribute('open')) === null) await bigRow.locator(':scope > summary').click();
    const full = bigRow.locator('.tool-output-action button', { hasText: '查看完整结果' });
    try {
      await full.first().waitFor({ state: 'visible', timeout: 30000 });
    } catch (error) {
      evidence.steps.D05_dom = await page.evaluate(() => [...document.querySelectorAll('.tool-row')]
        .filter(row => row.dataset.kind === 'mcp')
        .map(row => ({ name: row.dataset.toolName, status: row.dataset.status, open: row.open, action: Boolean(row.querySelector('.tool-output-action')), text: row.innerText.slice(0, 500) })));
      throw error;
    }
    await full.first().click();
    await page.waitForSelector('.tool-output-view', { timeout: 30000 });
    // The model-visible result is itself bounded. The full artifact is a
    // separate, runtime-served part; loading it is the viewer's own action.
    const artifact = page.locator('.tool-output-ref').first();
    await artifact.waitFor({ state: 'visible', timeout: 30000 });
    await artifact.click();
    await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
    let loaded = await page.locator('.tool-output-pre').innerText();
    let pages = 1;
    for (let attempt = 0; attempt < 12; attempt += 1) {
      await page.waitForTimeout(250);
      const more = page.locator('.tool-output-footer button', { hasText: '加载更多' });
      if (!(await more.count())) break;
      const length = loaded.length;
      await more.first().click();
      await page.waitForFunction(previous => (document.querySelector('.tool-output-pre')?.textContent ?? '').length > previous, length, { timeout: 20000 });
      loaded = await page.locator('.tool-output-pre').innerText();
      pages += 1;
    }
    assert.ok(loaded.split('\n').length >= 6000, `all 6000 lines were paged: ${loaded.split('\n').length}`);
    assert.ok(loaded.includes('MCP_BIG_LINE_06000'), 'the real last line is present');
    const calls = (await serverCalls()).slice(before);
    assert.ok(calls.some(call => call.tool === 'big' && call.outcome === 'ok'), JSON.stringify(calls));
    await capture('r3a-dog-04-full-output.png');
    return { bytes: loaded.length, lines: loaded.split('\n').length, pages, server_called: true };
  });

  await step('D06_tool_failure_is_real', async () => {
    const before = await callCount();
    await sendTurn(`R3A_MCP_DOGFOOD 调用 mcp__${serverName}__fail，并如实说明它失败了。不要把它描述成成功。`);
    await expandExecutionGroups();
    const failRow = await mcpRow('fail');
    assert.ok(failRow, 'the fail MCP tool row exists');
    const text = await failRow.innerText();
    assert.match(text, /intentional MCP tool failure/i, text);
    assert.notEqual(await failRow.getAttribute('data-status'), 'success');
    const calls = (await serverCalls()).slice(before);
    assert.ok(calls.some(call => call.tool === 'fail' && call.outcome === 'error'), JSON.stringify(calls));
    await capture('r3a-dog-05-tool-failure.png');
    return { status: await failRow.getAttribute('data-status'), server_called: true };
  });

  await step('D07_plan_requires_approval_and_denial_is_real', async () => {
    await switchMode('计划');
    await page.waitForTimeout(300);
    const before = await callCount();
    const turn = await sendTurn(
      `R3A_MCP_DOGFOOD 计划模式：请调用 mcp__${serverName}__echo（参数 text="PLAN_MUST_NOT_RUN"）并报告结果。`,
      { approval: 'deny', timeout: 300000 },
    );
    const calls = (await serverCalls()).slice(before);
    assert.equal(calls.filter(call => call.tool === 'echo' && call.args?.text === 'PLAN_MUST_NOT_RUN').length, 0, 'a denied Plan call never reached the server');
    await switchMode('对话');
    await capture('r3a-dog-06-plan-denied.png');
    return { approved: turn.approved, denied: turn.denied, server_called: false };
  });

  await step('D08_goal_mode_can_use_mcp', async () => {
    await switchMode('目标');
    const before = await callCount();
    let settled = true;
    try {
      await sendTurn(
        `R3A_MCP_DOGFOOD 目标模式：使用 mcp__${serverName}__add 计算 5+7，报告真实结果；完成后用 update_goal 结束目标。不要修改文件。`,
        { timeout: 480000 },
      );
    } catch (error) {
      settled = false;
      evidence.steps.D08_goal_timeout = { detail: String(error.message ?? error) };
      await page.locator('#stop').click().catch(() => {});
      await page.evaluate(sessionId => window.desktop.deliver({
        command_id: crypto.randomUUID(),
        session_id: sessionId,
        expected_version: null,
        issued_at: new Date().toISOString(),
        command: { type: 'cancel_task', session_id: sessionId },
      }), createdSessions[0]).catch(() => {});
      await page.waitForTimeout(1500);
      await idle().catch(() => {});
    }
    const calls = (await serverCalls()).slice(before);
    const add = calls.find(call => call.tool === 'add' && Number(call.args?.a) === 5 && Number(call.args?.b) === 7);
    assert.ok(add, `the Goal turn reached the server: ${JSON.stringify(calls)}`);
    const snapshot = await page.evaluate(sessionId => window.desktop.snapshot(sessionId), createdSessions[0]);
    await switchMode('对话').catch(() => {});
    return { server_called: true, settled, axis_after: snapshot.collaboration ?? null };
  });

  await step('D09_disable_removes_the_tool', async () => {
    await idle().catch(() => {});
    await openMcp();
    await card(serverName).locator('.switch').click();
    await waitState(serverName, '已停用');
    await page.locator('#settings-close').click();
    const beforeTable = await callCount();
    await sendTurn(`R3A_MCP_DOGFOOD 尝试调用 mcp__${serverName}__echo（参数 text="DISABLED_MUST_NOT_RUN"）。如果工具不可用，请直接说明。`, { timeout: 300000 });
    const calls = (await serverCalls()).slice(beforeTable);
    assert.equal(calls.filter(call => call.tool === 'echo' && call.args?.text === 'DISABLED_MUST_NOT_RUN').length, 0, 'a disabled server never received the call');
    await openMcp();
    await card(serverName).locator('.switch').click();
    await waitState(serverName, '已配置，未连接');
    await connectTestServer();
    await page.locator('#settings-close').click();
    return { disabled: true, server_called: false };
  });

  await step('D10_restart_and_reconnect', async () => {
    const info = await page.evaluate(() => window.desktop.runtimeInfo());
    const id = createdSessions[0];
    await Promise.race([app.close().catch(() => {}), new Promise(resolve => setTimeout(resolve, 8000))]);
    try { app.process()?.kill('SIGKILL'); } catch { /* gone */ }
    app = null;
    try { process.kill(info.pid, 'SIGKILL'); } catch { /* already gone */ }
    await new Promise(resolve => setTimeout(resolve, 1500));
    await launch();
    await page.waitForSelector(`.session-row[data-session-id="${id}"]`, { timeout: 60000 });
    await page.locator(`.session-row[data-session-id="${id}"]`).click();
    await page.waitForFunction(expected => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === expected, id, { timeout: 60000 });
    await idle();
    await page.waitForFunction(() => document.querySelector('#conversation')?.innerText.includes('R3A_DOGFOOD'), null, { timeout: 60000 });
    daemonPid = (await page.evaluate(() => window.desktop.runtimeInfo()))?.pid ?? daemonPid;
    if (daemonPid) daemonPids.add(daemonPid);
    await openMcp();
    await page.waitForSelector(`.mcp-card[data-server="${serverName}"]`, { timeout: 30000 });
    const state = await card(serverName).locator('.mcp-state-label').innerText();
    assert.equal(state, '已配置，未连接', 'a restarted runtime owns no connection until it attempts one');
    await connectTestServer();
    await page.locator('#settings-close').click();
    await capture('r3a-dog-07-restarted.png');
    return { before_connect: state, reconnected: true };
  });

  await step('D11_remove_and_config_roundtrip', async () => {
    await openMcp();
    await removeTestServer();
    await page.locator('#settings-close').click();
    const text = await readFile(configPath, 'utf8');
    assert.ok(!text.includes(serverName), 'the test declaration is gone');
    const after = configHash();
    assert.equal(after, configBefore, 'the real configuration is byte-identical to its snapshot');
    return { removed: true, config_sha256_after: after };
  });

  await step('D12_secret_hygiene', async () => {
    const body = await page.evaluate(() => document.body.innerText);
    assert.ok(!body.includes(marker), 'the page never renders the synthetic MCP token');
    const storage = await page.evaluate(() => JSON.stringify(localStorage));
    assert.ok(!storage.includes(marker), 'local storage never holds the synthetic MCP token');
    const text = await readFile(configPath, 'utf8');
    assert.ok(!text.includes(marker), 'the config never stores the synthetic MCP token');
    return { page: 'clean', localStorage: 'clean', config: 'clean' };
  });

  evidence.config_sha256_after = configHash();
  evidence.tasks_after_archiving = await taskCount();
  assert.deepEqual(errors, [], 'no renderer page errors');
  evidence.result = evidence.failures.length === 0 ? 'PASS' : 'FAIL_PARTIAL';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r3a-dogfood-failure.png').catch(() => {});
  throw error;
} finally {
  await writeFile(path.join(output, 'r3a-dogfood-evidence.json'), JSON.stringify(evidence, null, 2));
  // Remove the test declaration even after a failed run, through the same
  // runtime authority when the page is still alive; otherwise the CLI edits
  // the same file with the same lock.
  const cliEnv = { ...process.env };
  delete cliEnv.LEVELER_HOME;
  delete cliEnv.LEVELER_CONFIG_DIR;
  try {
    await execFile(leveler, ['mcp', 'remove', serverName], { env: cliEnv });
  } catch { /* already removed or never added */ }
  const finalHash = configHash();
  evidence.config_sha256_final = finalHash;
  if (finalHash !== configBefore) {
    evidence.failures.push({ step: 'cleanup', detail: 'the real configuration is not byte-identical to its snapshot' });
    if (evidence.result === 'PASS') evidence.result = 'FAIL_PARTIAL';
  }
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
  await writeFile(path.join(output, 'r3a-dogfood-evidence.json'), JSON.stringify(evidence, null, 2));
}
