// Real Electron/Runtime regression for draggable header tabs and history entry.
// Only model output uses a test-only local provider; session state is persisted by Runtime.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-session-views-');
const home = path.join(temporary, 'home');
await mkdir(home);
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  if (!body.tools?.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  for (const [delta, finish] of [[{ content: 'SESSION_VIEWS_COMPLETE' }, null], [{}, 'stop']]) {
    response.write(`data: ${JSON.stringify({ id: 'views', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
  }
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"
[providers.fixture]
protocol="openai_chat"
base_url="http://127.0.0.1:${server.address().port}"
api_key="test-only"
[models.m]
provider="fixture"
model_id="m"
context_window=131072
`, { mode: 0o600 });

let app, page;
const errors = [], failures = [], evidence = {};
const check = (name, actual, expected) => {
  evidence[name] = actual;
  try { assert.deepEqual(actual, expected, name); }
  catch (error) { failures.push(error.message); }
};
const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => document.querySelector('#brand-version')?.textContent.startsWith('v'));
};
const selected = () => page.locator('#view-tabs [aria-selected="true"]').getAttribute('data-view');
const openHistory = async id => {
  await page.locator(`.session-row[data-session-id="${id}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status').textContent !== '连接 Runtime…' && !document.querySelector('#view-tabs').hidden);
};
try {
  await launch();
  await page.locator('#message').fill('Check desktop session views');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SESSION_VIEWS_COMPLETE') && document.querySelector('#conversation-status').textContent === '已回答');
  const id = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), id);
  assert.equal(snapshot.repository, null, 'regression must use a real session without a workspace');
  // DOM clicks alone bypass native drag hit testing. Inspect Chromium's effective
  // app-region too, so a passing click cannot hide the Electron-specific bug.
  check('tab_app_regions', await page.locator('#view-tabs [data-view]').evaluateAll(tabs => tabs.map(tab => {
    for (let node = tab; node; node = node.parentElement) {
      const region = getComputedStyle(node).getPropertyValue('-webkit-app-region');
      if (region === 'drag' || region === 'no-drag') return region;
    }
    return 'none';
  })), ['no-drag', 'no-drag']);
  await page.locator('[data-view="trajectory"]').click();
  check('click_trajectory', await selected(), 'trajectory');
  check('chat_hidden_in_trajectory', await page.locator('#conversation-scroll').evaluate(node => node.hidden), true);
  check('trajectory_visible', await page.locator('#trajectory-root').isVisible(), true);
  await page.locator('[data-view="chat"]').click();
  check('click_chat', await selected(), 'chat');
  check('transcript_visible_in_chat', await page.locator('#conversation-scroll').isVisible(), true);
  check('trajectory_hidden_in_chat', await page.locator('#trajectory-root').isVisible(), false);
  await page.keyboard.press('ArrowRight');
  check('keyboard_trajectory', await selected(), 'trajectory');
  await page.keyboard.press('Home');
  check('keyboard_chat', await selected(), 'chat');
  await page.locator('[data-view="trajectory"]').click();
  await page.locator('#new-session').click();
  await openHistory(id);
  check('reopened_history', await selected(), 'chat');
  // Old profiles can still contain the previous persisted tab preference.
  await page.evaluate(id => localStorage.setItem('codeleveler.views.v1', JSON.stringify({ [id]: 'trajectory' })), id);
  await app.close();
  app = null;
  await launch();
  await openHistory(id);
  check('history_after_restart_with_old_preference', await selected(), 'chat');
  check('transcript_visible_after_restart', await page.locator('#conversation-scroll').evaluate(node => !node.hidden), true);
  check('page_errors', errors, []);
  assert.deepEqual(failures, []);
} catch (error) {
  console.error(error);
  throw error;
} finally {
  console.log(JSON.stringify({ ...evidence, failures }, null, 2));
  if (app) await app.evaluate(({ app }) => app.exit(0));
  await new Promise(resolve => server.close(resolve));
}
