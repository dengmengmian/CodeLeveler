// R4 command-surface acceptance: discovery, keyboard, typed goal and thinking.
// Real Electron/bridge/runtime; test-only SSE provider and folder picker.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-slash-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'a.txt'), 'alpha\n');

const providerRequests = [];
const providerErrors = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  try {
    const body = JSON.parse(raw);
    const messages = body.messages ?? [];
    const lastUser = [...messages].reverse().find(message => message.role === 'user');
    const userText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
    providerRequests.push({ userText, messages: messages.length });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    const text = 'GOAL_OK';
    response.write(`data: ${JSON.stringify({ id: 'slash', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { content: text }, finish_reason: null }] })}\n\n`);
    response.write(`data: ${JSON.stringify({ id: 'slash', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
    response.end('data: [DONE]\n\n');
  } catch (error) {
    providerErrors.push(error.message);
    response.writeHead(500);
    response.end('fixture failed');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\nreasoning=true\nreasoning_style="thinking_flag"\nsupported_efforts=["low","high","max"]\nreasoning_effort="high"\n`, { mode: 0o600 });

const evidence = { scope: 'real Electron + bridge + runtime; local SSE provider and folder-picker seam' };
const errors = [];
let app;
let page;
const menuIds = () => page.evaluate(() => [...document.querySelectorAll('#command-popover .menu-item')].map(item => item.dataset.commandId));
const snapshot = async () => {
  const id = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  return page.evaluate(sessionId => window.desktop.snapshot(sessionId), id);
};
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('命令验收');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('GOAL_OK'));
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');

  // Discovery: `/` shows the quick set, typing searches the full catalog, and
  // Aliases resolve to their primary command.
  await page.locator('#message').fill('/');
  await page.waitForSelector('#command-popover');
  const quick = await menuIds();
  assert.ok(quick.includes('help') && quick.includes('thinking') && quick.includes('model'), JSON.stringify(quick));
  assert.ok(!quick.includes('doctor'), 'searchable-only commands stay out of the bare list');
  await page.locator('#message').fill('/mode');
  await page.waitForFunction(() => [...document.querySelectorAll('#command-popover .menu-item')].some(item => item.dataset.commandId === 'permission'));
  await page.locator('#message').fill('/doct');
  await page.waitForFunction(() => [...document.querySelectorAll('#command-popover .menu-item')].some(item => item.dataset.commandId === 'doctor'));
  await page.keyboard.press('Escape');
  assert.equal(await page.locator('#message').inputValue(), '/doct', 'Escape keeps the draft');
  evidence.discovery = { quick, alias_resolves: true, escape_keeps_draft: true };

  // Keyboard: Tab confirms the highlighted candidate and opens Thinking.
  await page.locator('#message').fill('/thinking');
  await page.waitForFunction(() => document.querySelector('#command-popover')?.dataset.selected === 'thinking');
  await page.keyboard.press('Tab');
  // The candidate list must be gone before reading the control it opens;
  // otherwise a loaded machine can sample the old popover's own item.
  await page.waitForFunction(() => !document.querySelector('#command-popover'));
  await page.waitForFunction(() => !!document.querySelector('.menu .menu-item'));
  const thinkingMenu = await page.evaluate(() => [...document.querySelectorAll('.menu .menu-item')].map(item => item.textContent));
  assert.ok(thinkingMenu.some(item => item.includes('高')), JSON.stringify(thinkingMenu));
  await page.locator('.menu .menu-item').filter({ hasText: '高' }).first().click();
  await page.waitForFunction(async (id) => (await window.desktop.snapshot(id))?.thinking?.session_override === 'high', sessionId);
  const afterThinking = await snapshot();
  assert.equal(afterThinking.thinking.session_override, 'high');
  // The thinking command is a real delivery; wait for its action to settle
  // before the next command, exactly as the UI's send control reports it.
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 20000 });
  evidence.thinking = { choices: thinkingMenu, session_override: afterThinking.thinking.session_override };

  // Typed goal: the objective reaches the Runtime as RunGoal, never as the
  // literal "/goal …" prose the model would have to interpret.
  const beforeAnswers = await page.locator('.message.assistant .markdown').count();
  await page.locator('#message').fill('/goal 完成命令验收');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(count => document.querySelectorAll('.message.assistant .markdown').length > count, beforeAnswers, { timeout: 90000 });
  await page.waitForFunction(() => ['已回答', '已完成', '已中断', '执行失败'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 90000 });
  const goalRequest = providerRequests.find(item => item.userText.includes('完成命令验收'));
  assert.ok(goalRequest, 'the objective must reach the provider');
  assert.doesNotMatch(goalRequest.userText, /^\/goal/, 'the command itself must not be sent as prose');
  const goalSnapshot = await snapshot();
  assert.equal(goalSnapshot.collaboration, 'goal');
  evidence.typed_goal = { provider_user_text: goalRequest.userText, collaboration: goalSnapshot.collaboration };

  // `/goal status` answers from the Runtime projection; `/goal clear` returns
  // the session to chat through the typed axis command.
  await page.locator('#message').fill('/goal status');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('.toast, .notification')?.textContent?.includes('目标状态') || document.body.textContent.includes('目标状态'));
  await page.locator('#message').fill('/goal clear');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(async (id) => (await window.desktop.snapshot(id))?.collaboration === 'chat', sessionId);
  assert.equal((await snapshot()).collaboration, 'chat');
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 20000 });
  evidence.goal_status_and_clear = true;

  // A product command that opens a real surface does so through the same
  // control a click would use: `/settings` opens the settings dialog.
  await page.locator('#message').fill('/settings');
  await page.locator('#message').press('Enter');
  await page.waitForSelector('#settings-dialog[open]');
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));
  // `/usage` without recorded token data reports that fact instead of opening
  // an invented dialog.
  await page.locator('#message').fill('/usage');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent?.includes('用量'));
  evidence.typed_settings_dialog = true;

  await page.screenshot({ path: path.join(output, 'commands-slash.png') });
  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'commands-slash-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, providerErrors, requests: providerRequests.slice(-4) }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
