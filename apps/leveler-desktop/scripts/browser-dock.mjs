// Real native Browser inside the rewritten right dock: navigation, float,
// fullscreen, external open, multi-tab and close. No model inference.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-browser-dock-');
const home = path.join(temporary, 'home');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:1"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n', { mode: 0o600 });

const server = createServer((request, response) => {
  const name = request.url === '/b' ? 'B' : 'A';
  response.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' });
  response.end(`<!doctype html><title>Dock ${name}</title><h1>DOCK_BODY_${name}</h1>`);
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;

let application;
let page;
const errors = [];
const evidence = { scope: 'real Electron native WebContentsView in the rewritten right dock; no inference' };
const probe = () => application.evaluate(() => {
  const manager = globalThis.__dock?.manager;
  if (!manager?.selectedId) return null;
  const tab = manager.tabs.get(manager.selectedId);
  if (!tab) return null;
  const contents = tab.view.webContents;
  return {
    id: tab.id,
    url: contents.getURL(),
    title: contents.getTitle(),
    visible: tab.view.getVisible(),
    bounds: tab.view.getBounds(),
    preferences: contents.getLastWebPreferences(),
    state: manager.state(),
  };
});
const waitBrowser = async (predicate, message) => {
  for (let index = 0; index < 100; index += 1) {
    const value = await probe();
    if (value && predicate(value)) return value;
    await new Promise(resolve => setTimeout(resolve, 100));
  }
  throw new Error(message);
};
const navigate = async url => {
  const input = page.locator('.browser-address-wrap input');
  await input.fill(url);
  await input.press('Enter');
};
const capture = async name => {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, `${name}.png`) });
};

try {
  application = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await application.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.locator('#new-session').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') && document.querySelector('#conversation-status').textContent !== '连接 Runtime…');
  await application.evaluate((_, modulePath) => {
    const { ManualBrowser } = process.getBuiltinModule('module').createRequire(modulePath)(modulePath);
    const original = ManualBrowser.prototype.add;
    globalThis.__dock = { original, manager: null, tab: null };
    ManualBrowser.prototype.add = function () {
      globalThis.__dock.manager = this;
      const tab = original.call(this);
      globalThis.__dock.tab = tab;
      return tab;
    };
  }, path.join(root, 'src/browser.cjs'));

  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 1));
  await navigate(`${base}/a`);
  let current = await waitBrowser(value => value.url === `${base}/a` && !value.state.tabs.find(tab => tab.id === value.id)?.loading, 'A did not load');
  assert.equal(current.visible, true);
  assert.equal(current.title, 'Dock A');
  assert.equal(current.preferences.nodeIntegration, false);
  assert.equal(current.preferences.sandbox, true);
  const firstId = current.id;
  evidence.native_preferences = { nodeIntegration: current.preferences.nodeIntegration, sandbox: current.preferences.sandbox };
  evidence.navigation = { url: current.url, title: current.title, visible: current.visible, bounds: current.bounds };
  await capture('browser-dock-a');

  const tab = page.locator('.pane-tab[aria-selected="true"]');
  await tab.click({ button: 'right' });
  await page.getByRole('menuitem', { name: '浮动窗口' }).click();
  await page.waitForSelector('.rightbar-float .browser-surface');
  await page.waitForTimeout(250);
  current = await waitBrowser(value => value.visible, 'floating browser is not visible');
  const floatBox = await page.locator('.rightbar-float .browser-surface').boundingBox();
  assert.ok(floatBox);
  assert.ok(current.bounds.x >= Math.floor(floatBox.x) - 2 && current.bounds.x <= Math.ceil(floatBox.x) + 2);
  assert.ok(current.bounds.width <= Math.ceil(floatBox.width) + 2);
  evidence.float = { bounds: current.bounds, surface: floatBox };
  await capture('browser-dock-float');

  const head = await page.locator('.rightbar-float .float-head').boundingBox();
  await page.mouse.move(head.x + 80, head.y + 18);
  await page.mouse.down();
  await page.mouse.move(head.x + 180, head.y + 98, { steps: 8 });
  await page.mouse.up();
  await page.waitForTimeout(250);
  const moved = await probe();
  assert.ok(moved.visible);
  assert.notDeepEqual(moved.bounds, current.bounds);
  evidence.float_drag = { before: current.bounds, after: moved.bounds };
  await page.locator('.rightbar-float button[aria-label="返回停靠区"]').click();
  await page.waitForFunction(() => !document.querySelector('.rightbar-float'));
  assert.equal((await probe()).visible, true);

  await page.locator('.pane-chrome button[aria-label="全屏"]').click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-expanded'));
  await page.waitForTimeout(250);
  current = await waitBrowser(value => value.visible, 'browser hidden in fullscreen');
  assert.ok(current.bounds.width > 900, `fullscreen browser width ${current.bounds.width}`);
  evidence.fullscreen = { bounds: current.bounds };
  await capture('browser-dock-fullscreen');
  await page.locator('.pane-chrome button[aria-label="退出全屏"]').click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-expanded'));

  await application.evaluate(({ shell }) => {
    globalThis.__externalOpened = null;
    shell.openExternal = async url => { globalThis.__externalOpened = url; };
  });
  await page.getByRole('button', { name: '在系统浏览器中打开' }).click();
  await page.waitForTimeout(300);
  const opened = await application.evaluate(() => globalThis.__externalOpened);
  assert.equal(opened, `${base}/a`);
  evidence.external = opened;

  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 2));
  await navigate(`${base}/b`);
  current = await waitBrowser(value => value.url === `${base}/b` && !value.state.tabs.find(tabItem => tabItem.id === value.id)?.loading, 'B did not load');
  const secondId = current.id;
  assert.notEqual(secondId, firstId);
  evidence.multi_tab = { firstId, secondId, selectedId: current.state.selectedId };
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 1));
  current = await waitBrowser(value => value.id === firstId, 'first browser tab was not restored');
  assert.equal(current.visible, true);
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 0));
  if (await page.locator('#rightbar').evaluate(node => node.hidden)) await page.locator('#header-right-toggle').click();
  await page.getByText('开始', { exact: true }).waitFor();
  evidence.close_last = { browserTabs: 0, startRestored: true };
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'browser-dock-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) {
    evidence.failure_state = await page.evaluate(async () => ({
      state: await window.desktop.browserState(),
      paneTabs: [...document.querySelectorAll('.pane-tab')].map(node => ({ text: node.innerText, selected: node.getAttribute('aria-selected') })),
      address: document.querySelector('.browser-address-wrap input')?.value ?? null,
    })).catch(captureError => ({ captureError: captureError.message }));
    await capture('browser-dock-failure').catch(() => {});
  }
  await writeFile(path.join(output, 'browser-dock-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (application) await application.close();
  await new Promise(resolve => server.close(resolve));
}
