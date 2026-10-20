// Session-scoped UI state across model/permission switches: the draft and the
// native browser tab must survive, while the snapshot must show the change.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-settings-branches-');
const home = path.join(temporary, 'home');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:1"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n[models.n]\nprovider="fixture"\nmodel_id="n"\ncontext_window=131072\n', { mode: 0o600 });
const server = createServer((request, response) => {
  response.writeHead(200, { 'content-type': 'text/html' });
  response.end('<!doctype html><title>Branch fixture</title><h1>BRANCH_BODY</h1>');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const url = `http://127.0.0.1:${server.address().port}/branch`;
const evidence = { scope: 'real Electron + real native WebContentsView; no inference', url };
const errors = [];
let app;
let page;
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.locator('#new-session').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') && document.querySelector('#conversation-status').textContent !== '连接 Runtime…');
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 1));
  await page.locator('.browser-address-wrap input').fill(url);
  await page.locator('.browser-address-wrap input').press('Enter');
  await page.waitForFunction(url => window.desktop.browserState().then(state => state.selectedId && state.tabs.find(tab => tab.id === state.selectedId)?.url === url && !state.tabs.find(tab => tab.id === state.selectedId)?.loading), url);
  const tabId = (await page.evaluate(() => window.desktop.browserState())).selectedId;
  await page.locator('#message').fill('DRAFT_SURVIVES_SWITCHES');
  await page.locator('#model-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: 'n' }).first().click();
  await page.waitForFunction(async id => (await window.desktop.snapshot(id)).model.model === 'n', sessionId);
  assert.equal(await page.locator('#message').inputValue(), 'DRAFT_SURVIVES_SWITCHES');
  assert.equal((await page.evaluate(() => window.desktop.browserState())).selectedId, tabId);
  evidence.model_switch = (await page.evaluate(id => window.desktop.snapshot(id), sessionId)).model;
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '需批准' }).first().click();
  await page.waitForFunction(async id => (await window.desktop.snapshot(id)).mode === 'request_approval', sessionId);
  assert.equal(await page.locator('#message').inputValue(), 'DRAFT_SURVIVES_SWITCHES');
  assert.equal((await page.evaluate(() => window.desktop.browserState())).selectedId, tabId);
  evidence.permission_switch = (await page.evaluate(id => window.desktop.snapshot(id), sessionId)).mode;
  await page.locator('.pane-chrome button[aria-label="全屏"]').click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-expanded'));
  assert.equal((await page.evaluate(() => window.desktop.browserState())).selectedId, tabId);
  await page.locator('.pane-chrome button[aria-label="退出全屏"]').click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-expanded'));
  await page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-open'));
  assert.equal(await page.evaluate(() => ['header-right-toggle','floating-right-toggle'].some(id => { const node = document.getElementById(id); return node && !node.hidden && node.getClientRects().length > 0; })), true, 'closing the dock must leave a visible reopen control');
  const reopen = (await page.locator('#header-right-toggle').isVisible()) ? '#header-right-toggle' : '#floating-right-toggle';
  await page.locator(reopen).click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-open'));
  await page.waitForFunction(tabId => window.desktop.browserState().then(state => state.tabs.length === 1 && state.selectedId === tabId), tabId);
  assert.equal(await page.locator('#message').inputValue(), 'DRAFT_SURVIVES_SWITCHES');
  evidence.browser_tab_survived_layout = true;
  await page.screenshot({ path: path.join(output, 'settings-branches.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'settings-branches-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'settings-branches-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'settings-branches-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
