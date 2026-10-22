// Permanent regressions for the Desktop functional acceptance round:
// plan dock refresh, menu rename, sidebar fork, single-question clarification,
// cross-pane tab drag, and draft survive-restart. Test-only provider; real
// Electron, Runtime, filesystem and native browser.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-functional-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home); await mkdir(workspace);
await writeFile(path.join(workspace, 'a.txt'), 'a\n');
execFileSync('git', ['init', '-q'], { cwd: workspace });
execFileSync('git', ['add', '-A'], { cwd: workspace });
execFileSync('git', ['-c', 'user.email=a@b.c', '-c', 'user.name=accept', 'commit', '-qm', 'seed'], { cwd: workspace });

const pageServer = createServer((request, response) => {
  const name = request.url === '/b' ? 'B' : 'A';
  response.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' });
  response.end(`<!doctype html><title>Page ${name}</title><h1>PAGE_${name}</h1>`);
});
await new Promise(resolve => pageServer.listen(0, '127.0.0.1', resolve));
const pageBase = `http://127.0.0.1:${pageServer.address().port}`;
let held = null;
const provider = createServer(async (request, response) => {
  let raw = ''; for await (const chunk of request) raw += chunk;
  const body = JSON.parse(raw);
  const names = (body.tools ?? []).map(tool => tool.function?.name);
  if (!names.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  const userText = body.messages.filter(message => message.role === 'user').map(message => typeof message.content === 'string' ? message.content : '').join('\n');
  const toolCount = body.messages.filter(message => message.role === 'tool').length;
  const chunk = (delta, finish) => JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] });
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (userText.includes('PLAN')) {
    const plan = toolCount === 0
      ? [{ step: '读取项目', status: 'in_progress' }, { step: '修改代码', status: 'pending' }, { step: '验证', status: 'pending' }]
      : [{ step: '读取项目', status: 'completed' }, { step: '修改代码', status: 'in_progress' }, { step: '验证', status: 'pending' }];
    if (toolCount >= 2) { held = response; return; }
    response.write(`data: ${chunk({ role: 'assistant', tool_calls: [{ index: 0, id: `plan-${toolCount}`, type: 'function', function: { name: 'update_plan', arguments: JSON.stringify({ plan, explanation: 'regression plan' }) } }] }, null)}\n\n`);
    response.write(`data: ${chunk({}, 'tool_calls')}\n\n`); response.end('data: [DONE]\n\n'); return;
  }
  if (userText.includes('ASK')) {
    if (names.includes('request_user_input') && toolCount < 2) {
      response.write(`data: ${chunk({ role: 'assistant', tool_calls: [{ index: 0, id: 'ask-1', type: 'function', function: { name: 'request_user_input', arguments: JSON.stringify({ question: '午餐吃什么？', options: ['面条', '米饭'] }) } }] }, null)}\n\n`);
      response.write(`data: ${chunk({}, 'tool_calls')}\n\n`);
    } else if (!names.includes('request_user_input')) {
      response.write(`data: ${chunk({ role: 'assistant', tool_calls: [{ index: 0, id: 'cap-1', type: 'function', function: { name: 'capability', arguments: JSON.stringify({ action: 'enable', id: 'host_interaction' }) } }] }, null)}\n\n`);
      response.write(`data: ${chunk({}, 'tool_calls')}\n\n`);
    } else {
      response.write(`data: ${chunk({ role: 'assistant', content: 'ASK-DONE' }, null)}\n\n`);
      response.write(`data: ${chunk({}, 'stop')}\n\n`);
    }
    response.end('data: [DONE]\n\n'); return;
  }
  response.write(`data: ${chunk({ role: 'assistant', content: 'NORMAL-OK' }, null)}\n\n`);
  response.write(`data: ${chunk({}, 'stop')}\n\n`); response.end('data: [DONE]\n\n');
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "http://127.0.0.1:${provider.address().port}"\napi_key = "test-only"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\nvision = true\n`, { mode: 0o600 });

const evidence = { scope: 'permanent functional regressions on real Electron/Runtime; test-only provider; isolated home/workspace' };
const cases = {};
let app, page;
async function launch() {
  app = await electron.launch({ args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' } });
  page = await app.firstWindow(); page.setDefaultTimeout(30000);
  await app.evaluate(({ dialog }) => { dialog.showMessageBox = async () => ({ response: 0 }); });
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } });
}
const selectedId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const addWorkspace = async () => {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-empty .button.primary').click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).first().click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
};
const send = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); await page.waitForSelector('.session-row[aria-selected="true"]'); };
try {
  await launch();
  await addWorkspace();
  // 1. Plan dock refreshes from plan_updated before any work message commits.
  await send('PLAN regression');
  const planSession = await selectedId();
  await page.waitForSelector('.plan-card-row', { timeout: 30000 });
  await page.waitForSelector('.todo-dock', { timeout: 10000 });
  const todoText = await page.locator('.todo-dock').innerText();
  assert.match(todoText, /\d+ 已完成/);
  assert.match(todoText, /\d+ 进行中/);
  assert.doesNotMatch(todoText, /任务 \d+\/\d+/);
  cases.plan_dock_on_event = true;
  await page.locator('#stop').click();
  await page.waitForFunction(() => document.querySelector('#conversation-status').textContent === '已中断');

  // 2. The session ··· menu rename enters the header editor and lands in Runtime.
  const row = page.locator(`.session-row[data-session-id="${planSession}"]`);
  await row.hover();
  await row.locator('.session-more').click();
  await page.locator('.menu .menu-item').filter({ hasText: '重命名' }).click();
  await page.locator('#conversation-header .title-input').waitFor({ timeout: 5000 });
  await page.locator('#conversation-header .title-input').fill('Renamed by menu');
  await page.locator('#conversation-header .title-input').press('Enter');
  await page.waitForFunction(id => window.desktop.snapshot(id).then(snapshot => snapshot.goal === 'Renamed by menu'), planSession);
  cases.menu_rename = true;

  // 3. A second session, then fork the first from the sidebar while it is not open.
  await page.locator('.group-new').first().click(); await page.waitForTimeout(400);
  const secondSession = await selectedId();
  await send('NORMAL regression');
  await page.waitForFunction(() => document.querySelector('#conversation').innerText.includes('NORMAL-OK'));
  await page.locator(`.session-row[data-session-id="${planSession}"]`).hover();
  await page.locator(`.session-row[data-session-id="${planSession}"] .session-more`).click();
  await page.locator('.menu .menu-item').filter({ hasText: '分叉会话' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('.session-title')].some(node => node.textContent.includes('(分叉)')), null, { timeout: 15000 });
  const forkCount = await page.evaluate(() => window.desktop.listTasks().then(index => index.tasks.filter(task => task.title?.includes('(分叉)')).length));
  assert.ok(forkCount >= 1);
  cases.sidebar_fork = true;

  // 4. One clarification question must render once and complete after answering.
  await page.locator('.group-new').first().click(); await page.waitForTimeout(400);
  await send('ASK regression');
  await page.waitForSelector('#takeover .interaction-card', { timeout: 30000 });
  const card = await page.locator('#takeover').innerText();
  assert.equal((card.match(/午餐吃什么/g) ?? []).length, 1, 'single question is not duplicated');
  await page.getByText('面条', { exact: true }).click();
  await page.getByRole('button', { name: '提交回答' }).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status').textContent === '已回答', null, { timeout: 30000 });
  const clarificationSession = await selectedId();
  const clarified = await page.evaluate(id => window.desktop.snapshot(id), clarificationSession);
  assert.equal(clarified.pending_interactions.length, 0);
  cases.single_question_clarification = true;

  // 5. Cross-pane tab drag moves the tab; it must not become a floating window.
  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 1));
  const address = page.locator('.browser-address-wrap input');
  await address.fill(`${pageBase}/a`); await address.press('Enter');
  await page.waitForTimeout(700);
  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 2));
  await address.fill(`${pageBase}/b`); await address.press('Enter');
  await page.waitForTimeout(700);
  await page.locator('.split-button').click();
  await page.waitForSelector('.right-pane[data-pane="pane2"]');
  const dragTab = page.locator('.right-pane[data-pane="pane1"] .pane-tab').filter({ hasText: /Page [AB]/ }).first();
  const dragTitle = (await dragTab.innerText()).trim();
  await dragTab.dragTo(page.locator('.right-pane[data-pane="pane2"] .tab-items'));
  await page.waitForTimeout(500);
  const moved = await page.evaluate(() => [...document.querySelectorAll('.right-pane .pane-tab')].map(node => node.innerText.trim()));
  assert.ok(moved.includes(dragTitle), `dragged tab ${dragTitle} missing after drop: ${moved}`);
  assert.equal(await page.locator('.rightbar-float').count(), 0);
  cases.cross_pane_drag = true;

  // 6. A draft survives a real Desktop restart.
  const draftSession = await selectedId();
  await page.locator('#message').fill('DRAFT-SURVIVES-RESTART');
  await app.evaluate(({ app }) => app.exit(0));
  await new Promise(resolve => setTimeout(resolve, 1200));
  await launch();
  await page.locator(`.session-row[data-session-id="${draftSession}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  assert.equal(await page.locator('#message').inputValue(), 'DRAFT-SURVIVES-RESTART');
  cases.draft_survives_restart = true;

  Object.assign(evidence, { cases, passed: true });
  await writeFile(path.join(output, 'functional-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { cases, passed: false, error: error.message });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'functional-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'functional-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  if (held) held.destroy();
  await new Promise(resolve => provider.close(resolve));
  await new Promise(resolve => pageServer.close(resolve));
}
