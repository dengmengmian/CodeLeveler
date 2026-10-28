// Native Browser acceptance for the current right dock: real WebContentsView
// navigation, history, reload, network failure recovery, blocked protocols,
// dirty address handling, narrow clipping and isolation.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-browser-native-');
const home = path.join(temporary, 'home');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:1"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n', { mode: 0o600 });
let failing = true;
const requests = [];
const server = createServer((request, response) => {
  requests.push(request.url);
  if (request.url === '/fail' && failing) { request.socket.destroy(); return; }
  if (request.url === '/privileged-redirect') { response.writeHead(302, { location: 'leveler-desktop://desktop/index.html' }); response.end(); return; }
  const name = request.url.startsWith('/b') ? 'B' : 'A';
  response.writeHead(200, { 'content-type': 'text/html', 'cache-control': 'no-store' });
  response.end(`<!doctype html><html><head><title>Native ${name}</title></head><body><h1 id="marker">NATIVE_BODY_${name}</h1><a id="next" href="/b">B</a><script>globalThis.fixtureMarker='${name}';globalThis.safety=Promise.all([fetch('leveler-desktop://desktop/index.html').then(()=>true,()=>false),navigator.permissions.query({name:'geolocation'}).then(x=>x.state,()=> 'unavailable')]).then(x=>({privilegedFetchSucceeded:x[0],geolocation:x[1],desktop:typeof window.desktop,require:typeof require,process:typeof process}));</script></body></html>`);
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const evidence = { scope: 'real Electron native WebContentsView in the current right dock; fixture HTTP pages' };
const errors = [];
let app;
let page;
const probe = (body = false) => app.evaluate(async (_event, options) => {
  const manager = globalThis.__native?.manager;
  if (!manager?.selectedId) return { state: manager?.state?.() ?? null, selected: null };
  const tab = manager.tabs.get(manager.selectedId);
  if (!tab) return { state: manager.state(), selected: null };
  const contents = tab.view.webContents;
  return {
    state: manager.state(),
    selected: {
      id: tab.id,
      url: contents.getURL(),
      title: contents.getTitle(),
      loading: contents.isLoading(),
      visible: tab.view.getVisible(),
      bounds: tab.view.getBounds(),
      preferences: contents.getLastWebPreferences(),
      sameSession: contents.session === manager.window.webContents.session,
      ...(options.body ? { body: await contents.executeJavaScript('({marker:document.querySelector("#marker")?.textContent,href:location.href,fixtureMarker:globalThis.fixtureMarker})') } : {}),
    },
  };
}, { body });
const loaded = async (name, body = true) => {
  const expected = `${base}/${name.toLowerCase()}`;
  for (let index = 0; index < 100; index += 1) {
    const value = await probe(body);
    if (value.selected?.url === expected && !value.selected.loading) {
      if (body) assert.equal(value.selected.body.marker, `NATIVE_BODY_${name}`);
      return value;
    }
    await delay(100);
  }
  throw new Error(`native page ${name} did not load`);
};
const navigate = async url => {
  const input = page.locator('.browser-address-wrap input');
  await input.fill(url);
  await input.press('Enter');
};
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
  await app.evaluate((_event, modulePath) => {
    const { ManualBrowser } = process.getBuiltinModule('module').createRequire(modulePath)(modulePath);
    const original = ManualBrowser.prototype.add;
    globalThis.__native = { manager: null };
    ManualBrowser.prototype.add = function () {
      globalThis.__native.manager = this;
      return original.call(this);
    };
  }, path.join(root, 'src/browser.cjs'));
  await page.keyboard.press('Meta+T');
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length === 1));
  await navigate(`${base}/a`);
  let current = await loaded('A');
  assert.equal(current.selected.visible, true);
  evidence.navigation = { url: current.selected.url, title: current.selected.title, bounds: current.selected.bounds };
  await navigate(`${base}/b`);
  await loaded('B');
  await page.locator('button[aria-label="后退"]').click();
  await loaded('A');
  await page.locator('button[aria-label="前进"]').click();
  await loaded('B');
  const beforeReload = requests.filter(url => url === '/b').length;
  await page.locator('button[aria-label="刷新"]').click();
  await loaded('B');
  assert.ok(requests.filter(url => url === '/b').length > beforeReload);
  evidence.history_reload = true;
  // An edited address is the person's draft, not a native title update.
  await page.locator('.browser-address-wrap input').fill(`${base}/unsent-dirty`);
  await app.evaluate(async () => {
    const manager = globalThis.__native.manager;
    await manager.tabs.get(manager.selectedId).view.webContents.executeJavaScript('document.title="Dirty native title"');
  });
  await page.waitForFunction(() => document.querySelector('#rightbar')?.textContent.includes('Dirty native title'));
  assert.equal(await page.locator('.browser-address-wrap input').inputValue(), `${base}/unsent-dirty`);
  await page.locator('#message').click();
  assert.equal(await page.locator('.browser-address-wrap input').inputValue(), `${base}/b`);
  evidence.dirty_address = true;
  // A failed main-frame load is a product error, not a blank success. Retry
  // reloads the actual address once the endpoint recovers.
  await navigate(`${base}/fail`);
  await page.waitForFunction(() => { const node = document.querySelector('.browser-error'); return !!node && node.getClientRects().length > 0; });
  assert.equal((await probe()).selected.visible, false);
  failing = false;
  await page.locator('.browser-error button').filter({ hasText: '重试' }).click();
  for (let index = 0; index < 100; index += 1) {
    const value = await probe(true);
    if (value.selected?.url === `${base}/fail` && !value.selected.loading && value.selected.body?.marker === 'NATIVE_BODY_A') break;
    if (index === 99) throw new Error('retry did not reload the failed address');
    await delay(100);
  }
  evidence.network_recovery = true;
  const safety = await app.evaluate(async () => {
    const manager = globalThis.__native.manager;
    return manager.tabs.get(manager.selectedId).view.webContents.executeJavaScript('globalThis.safety');
  });
  assert.deepEqual(safety, { privilegedFetchSucceeded: false, geolocation: 'denied', desktop: 'undefined', require: 'undefined', process: 'undefined' });
  current = await probe();
  assert.equal(current.selected.preferences.nodeIntegration, false);
  assert.equal(current.selected.preferences.sandbox, true);
  assert.equal(current.selected.preferences.contextIsolation, true);
  assert.equal(current.selected.sameSession, false);
  evidence.native_safety = safety;
  // The address field refuses unsupported protocols before navigation while
  // keeping the typed value editable.
  await navigate('file:///etc/passwd');
  await page.waitForFunction(() => { const node = document.querySelector('.browser-error'); return !!node && node.getClientRects().length > 0; });
  assert.match(await page.locator('.browser-error').innerText(), /仅支持|无法打开|不受支持|协议/);
  assert.equal((await probe()).selected.url, `${base}/fail`);
  await page.locator('.browser-error button').filter({ hasText: '修改地址' }).click();
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), '输入 HTTP(S) 地址');
  assert.equal(await page.locator('.browser-address-wrap input').inputValue(), 'file:///etc/passwd');
  await navigate(`${base}/a`);
  await loaded('A');
  evidence.blocked_protocol_product_error = true;
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setContentSize(700, 1000));
  await page.waitForFunction(() => innerWidth === 700);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  current = await probe();
  assert.ok(current.selected.bounds.x + current.selected.bounds.width <= 700);
  assert.ok(current.selected.bounds.y + current.selected.bounds.height <= 1000);
  assert.equal(current.selected.visible, true);
  evidence.narrow_clipping = current.selected.bounds;
  await page.locator('.pane-chrome button[aria-label="全屏"]').click();
  await page.waitForFunction(() => document.body.classList.contains('rightbar-expanded'));
  await page.waitForTimeout(250);
  current = await probe();
  assert.ok(current.selected.bounds.width > 600);
  await page.locator('.pane-chrome button[aria-label="退出全屏"]').click();
  await page.waitForFunction(() => !document.body.classList.contains('rightbar-expanded'));
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await page.screenshot({ path: path.join(output, 'browser-native.png') });
  await writeFile(path.join(output, 'browser-native-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'browser-native-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'browser-native-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
