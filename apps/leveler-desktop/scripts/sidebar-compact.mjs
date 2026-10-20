// macOS narrow-window sidebar contract: reference Electron shell geometry and real actions.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { provider } from '../test/fixtures/provider.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = path.join(root, 'acceptance-output', 'desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-sidebar-compact-');
const home = path.join(temporary, 'home');
await mkdir(home);
const tools = await provider();
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${tools.baseURL}"\napi_key = "test-only-fixture"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`, { mode: 0o600 });
let app, page;
const errors = [], evidence = { platform: process.platform, states: [] };
async function size(w, h) {
  await app.evaluate(({ BrowserWindow }, [w, h]) => BrowserWindow.getAllWindows()[0].setSize(w, h), [w, h]);
  await page.waitForFunction(([w, h]) => innerWidth === w && innerHeight === h && document.body.dataset.sidebarBand === (w < 1024 ? 'narrow' : 'wide'), [w, h]);
  await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
}
const state = async (label) => {
  const value = await page.evaluate(() => {
    const sidebar = document.querySelector('#sidebar').getBoundingClientRect();
    const toggle = document.querySelector('#sidebar-toggle').getBoundingClientRect();
    const create = document.querySelector('#compact-new-task').getBoundingClientRect();
    const compact = getComputedStyle(document.querySelector('#compact-new-task')).display !== 'none';
    return { viewport: [innerWidth, innerHeight], sidebar: [sidebar.x, sidebar.y, sidebar.width, sidebar.height], collapsed: document.body.classList.contains('sidebar-collapsed'), toggle: [toggle.x, toggle.y, toggle.width, toggle.height], create: [create.x, create.y, create.width, create.height], compact, workbench: document.querySelector('#workbench').hidden === false, overflow: document.documentElement.scrollWidth > innerWidth };
  });
  evidence.states.push({ label, ...value });
  return value;
};
const capture = async (name) => {
  await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
  const bytes = await app.evaluate(async ({ BrowserWindow, desktopCapturer }) => { const window = BrowserWindow.getAllWindows()[0]; const [width, height] = window.getSize(); const sources = await desktopCapturer.getSources({ types: ['window'], thumbnailSize: { width: width * 2, height: height * 2 } }); const source = sources.find(item => item.id === window.getMediaSourceId()); return source.thumbnail.toPNG().toString('base64'); });
  await writeFile(path.join(output, name + '.png'), Buffer.from(bytes, 'base64'));
};
try {
  app = await electron.launch({ args: [root, '--user-data-dir=' + path.join(temporary, 'profile')], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' } });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && !document.querySelector('#refresh')?.disabled);
  const platform = await app.evaluate(() => process.platform);
  evidence.platform = platform;
  if (platform !== 'darwin') { evidence.skipped = 'macOS-only compact sidebar contract'; console.log(JSON.stringify(evidence, null, 2)); await app.close(); await tools.close(); process.exit(0); }
  await size(1280, 800);
  const wide = await state('1280-wide');
  assert.equal(wide.sidebar[2], 280);
  assert.equal(wide.compact, false);
  await size(1023, 800);
  const narrow = await state('1023-narrow');
  assert.equal(narrow.sidebar[2], 0, 'narrow sidebar must be fully hidden');
  assert.equal(narrow.compact, true);
  assert.deepEqual(narrow.toggle.map(Math.round), [88, 11, 28, 28]);
  assert.deepEqual(narrow.create.map(Math.round), [124, 11, 28, 28]);
  assert.equal(narrow.overflow, false);
  assert.equal(await page.evaluate(() => document.querySelector('#settings-open').closest('[inert]') !== null), true, 'hidden sidebar content must leave the tab order');
  await capture('sidebar-compact-hidden-1023');
  // The compact open control is a real action and closes the competing right panel.
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  const opened = await state('1023-opened');
  assert.equal(opened.sidebar[2], 280);
  assert.equal(opened.compact, false);
  assert.equal(opened.workbench, false);
  assert.equal(await page.evaluate(() => document.querySelector('#settings-open').closest('[inert]') !== null), false, 'opened sidebar content must re-enter the tab order');
  await capture('sidebar-compact-open-1023');
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('1023-reclosed')).sidebar[2], 0);
  await capture('sidebar-compact-hidden-1023-closed');
  // Reference home is captured with the competing right panel closed.
  if (await page.locator('#workbench').isVisible()) await page.locator('#workbench-close').click();
  await page.waitForTimeout(250);
  await size(960, 600);
  assert.equal((await state('960-hidden')).sidebar[2], 0);
  await capture('sidebar-compact-hidden-960');
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('960-opened')).sidebar[2], 280);
  await capture('sidebar-compact-open-960');
  await size(1023, 800);
  assert.equal((await state('1023-open-resize')).sidebar[2], 280, 'a narrow resize must not close a manually opened sidebar');
  await size(960, 600);
  assert.equal((await state('960-open-resize')).sidebar[2], 280);
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('960-reclosed')).sidebar[2], 0);
  await size(520, 600);
  assert.equal((await state('520-hidden-home')).sidebar[2], 0);
  await capture('sidebar-compact-hidden-520');
  await size(1023, 800);
  // 1024 restores the wide preference (open), 1023 hides it.
  await size(1024, 800);
  assert.equal((await state('1024-wide-pref-open')).sidebar[2], 280);
  await capture('sidebar-compact-open-1024');
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('1024-manual-collapsed')).sidebar[2], 0);
  await capture('sidebar-compact-hidden-1024');
  for (const width of [1023, 1024, 1280]) { await size(width, 800); assert.equal((await state(`${width}-manual-collapse-persists`)).sidebar[2], 0, 'manual wide collapse must persist'); }
  await size(1023, 800);
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('1023-manual-open')).sidebar[2], 280);
  await size(1024, 800);
  assert.equal((await state('1024-wide-pref-collapsed')).sidebar[2], 0, 'narrow open must not overwrite the wide preference');
  await size(1280, 800);
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('1280-wide-pref-reopened')).sidebar[2], 280);
  await size(1023, 800); assert.equal((await state('1023-wide-pref-open')).sidebar[2], 0);
  await size(1024, 800); assert.equal((await state('1024-wide-pref-open')).sidebar[2], 280);
  // 520: full-width main, compact controls, no overflow, draft preserved.
  await size(520, 600);
  const small = await state('520-hidden');
  assert.equal(small.sidebar[2], 0);
  assert.equal(small.overflow, false);
  assert.deepEqual(small.toggle.map(Math.round), [88, 11, 28, 28]);
  assert.deepEqual(small.create.map(Math.round), [124, 11, 28, 28]);
  const drag = await page.evaluate(() => ({ strip: getComputedStyle(document.querySelector('.sidebar-top-strip')).getPropertyValue('-webkit-app-region'), toggle: getComputedStyle(document.querySelector('#sidebar-toggle')).getPropertyValue('-webkit-app-region'), create: getComputedStyle(document.querySelector('#compact-new-task')).getPropertyValue('-webkit-app-region'), band: getComputedStyle(document.querySelector('#compact-drag')).display, bandRegion: getComputedStyle(document.querySelector('#compact-drag')).getPropertyValue('-webkit-app-region') }));
  assert.equal(drag.strip, 'drag');
  assert.equal(drag.toggle, 'no-drag');
  assert.equal(drag.create, 'no-drag');
  assert.equal(drag.band, 'block');
  assert.equal(drag.bandRegion, 'drag');
  evidence.drag = drag;
  await page.locator('#message').fill('compact sidebar draft');
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal(await page.locator('#message').inputValue(), 'compact sidebar draft');
  await page.locator('#sidebar-toggle').click();
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal(await page.locator('#message').inputValue(), 'compact sidebar draft');
  await page.locator('#sidebar-toggle').hover();
  await page.waitForTimeout(700);
  const tooltip = await page.evaluate(() => { const node = document.querySelector('#lv-tooltip'); const rect = node?.getBoundingClientRect(); return { hidden: node?.hidden, text: node?.innerText, rect: rect ? [rect.x, rect.y, rect.width, rect.height].map(value => Math.round(value)) : null }; });
  assert.equal(tooltip.hidden, false);
  assert.match(tooltip.text, /打开侧边栏/);
  assert.ok(Math.abs(tooltip.rect[0] - 126) <= 2 && Math.abs(tooltip.rect[1] - 12) <= 2, `tooltip must sit at the reference point: ${JSON.stringify(tooltip.rect)}`);
  evidence.tooltip = tooltip;
  await capture('sidebar-compact-tooltip-520');
  // Real new-session action.
  const before = (await page.evaluate(() => window.desktop.listTasks())).tasks.length;
  await page.locator('#compact-new-task').click();
  await page.waitForFunction(count => window.desktop.listTasks().then(index => index.tasks.length > count), before);
  assert.equal((await state('520-after-new')).compact, true);
  await page.waitForFunction(() => !!document.querySelector('#new-session') && !document.querySelector('#new-task').disabled);
  const beforeKey = (await page.evaluate(() => window.desktop.listTasks())).tasks.length;
  await page.keyboard.press('Meta+n');
  await page.waitForFunction(count => window.desktop.listTasks().then(index => index.tasks.length > count), beforeKey);
  evidence.command_n_created_task = true;
  // Keyboard toggles the same state.
  await page.keyboard.press('Meta+b');
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('520-keyboard-open')).sidebar[2], 280);
  await page.evaluate(() => document.querySelector('#settings-open').focus());
  assert.equal(await page.evaluate(() => document.activeElement.id), 'settings-open');
  await page.keyboard.press('Meta+b');
  await page.waitForFunction(() => !document.getAnimations().some(animation => animation.playState === 'running'));
  assert.equal((await state('520-keyboard-closed')).sidebar[2], 0);
  assert.equal(await page.evaluate(() => document.activeElement.id), 'sidebar-toggle', 'hiding must not strand focus in the clipped sidebar');
  await capture('sidebar-compact-520');
  assert.deepEqual(errors, []);
  evidence.passed = true;
  await writeFile(path.join(output, 'sidebar-compact-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  await writeFile(path.join(output, 'sidebar-compact-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  throw error;
} finally {
  if (app) await app.close();
  await tools.close();
}
