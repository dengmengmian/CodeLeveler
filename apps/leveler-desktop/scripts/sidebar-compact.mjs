// Narrow-window sidebar contract on the current shell: collapsed by default,
// a visible leading control opens it, a session/new action closes it, Escape
// dismisses it, and Settings still fits at 520px.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-sidebar-');
const home = path.join(temporary, 'home');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:1"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n', { mode: 0o600 });
const evidence = { scope: 'current narrow sidebar contract; real Electron', states: [] };
const errors = [];
let app;
let page;
const resize = async (width, height) => {
  await app.evaluate(({ BrowserWindow }, size) => BrowserWindow.getAllWindows()[0].setContentSize(size.width, size.height), { width, height });
  await page.waitForFunction(size => innerWidth === size.width && innerHeight === size.height, { width, height });
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
};
const state = async label => {
  const value = await page.evaluate(() => {
    const sidebar = document.querySelector('#sidebar');
    const seat = document.querySelector('#leading-seat');
    const box = sidebar.getBoundingClientRect();
    return {
      body: [...document.body.classList],
      sidebar: { x: box.x, width: box.width, inert: sidebar.inert, hidden: sidebar.getAttribute('aria-hidden') },
      leadingVisible: !seat.hidden && seat.getClientRects().length > 0,
      overflow: document.documentElement.scrollWidth > innerWidth,
    };
  });
  evidence.states.push({ label, ...value });
  return value;
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
  await resize(1280, 800);
  const wide = await state('1280');
  assert.equal(wide.sidebar.width, 280);
  assert.equal(wide.sidebar.inert, false);
  assert.equal(wide.leadingVisible, false);
  await resize(900, 700);
  const narrow = await state('900');
  assert.ok(narrow.sidebar.x + narrow.sidebar.width <= 0, 'narrow sidebar must be fully off-canvas');
  assert.equal(narrow.sidebar.inert, true);
  assert.equal(narrow.leadingVisible, true);
  assert.equal(narrow.overflow, false);
  await page.locator('#leading-toggle').click();
  await page.waitForFunction(() => document.body.classList.contains('sidebar-open'));
  const opened = await state('900-open');
  assert.equal(opened.sidebar.width, 280);
  assert.equal(opened.sidebar.inert, false);
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.body.classList.contains('sidebar-open'));
  const escaped = await state('900-escaped');
  assert.ok(escaped.sidebar.x + escaped.sidebar.width <= 0);
  await page.locator('#leading-new').click();
  await page.waitForFunction(() => document.querySelectorAll('.session-row').length >= 2);
  const afterNew = await state('900-new-session');
  assert.ok(afterNew.sidebar.x + afterNew.sidebar.width <= 0);
  await resize(520, 700);
  const minimum = await state('520');
  assert.ok(minimum.sidebar.x + minimum.sidebar.width <= 0);
  assert.equal(minimum.leadingVisible, true);
  assert.equal(minimum.overflow, false);
  await page.locator('#leading-toggle').click();
  await page.waitForFunction(() => document.body.classList.contains('sidebar-open'));
  await page.locator('#account-menu').click();
  await page.locator('.menu .menu-item').filter({ hasText: '设置' }).click();
  await page.waitForSelector('#settings-dialog[open]');
  const dialog = await page.locator('#settings-dialog').boundingBox();
  assert.ok(dialog.x >= 0 && dialog.x + dialog.width <= 520);
  assert.ok(dialog.y >= 0 && dialog.y + dialog.height <= 700);
  await page.locator('#settings-close').click();
  evidence.settings_fits_520 = { x: dialog.x, y: dialog.y, width: dialog.width, height: dialog.height };
  await page.screenshot({ path: path.join(output, 'sidebar-compact.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'sidebar-compact-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'sidebar-compact-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'sidebar-compact-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
