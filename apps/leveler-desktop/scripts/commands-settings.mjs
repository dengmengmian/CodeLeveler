// Settings and command-surface acceptance on the real Electron shell.
// Test-only provider; isolated HOME/workspace; every assertion is driven by a
// real pointer/keyboard operation, not a renderer function call.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-commands-settings-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'a.txt'), 'alpha\n');
const server = createServer(async (request, response) => {
  for await (const _chunk of request) { /* drain */ }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.write(`data: ${JSON.stringify({ id: 'settings', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { content: 'SETTINGS_OK' }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ id: 'settings', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n[models.n]\nprovider="fixture"\nmodel_id="n"\ncontext_window=131072\n`, { mode: 0o600 });

const evidence = { scope: 'real Electron + real Runtime; test-only SSE provider and native-folder chooser seam' };
const settings = () => page.evaluate(() => JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}'));
let app;
let page;
const errors = [];
const openSettings = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
};
const closeSettings = async () => {
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));
};
const chooseMenu = async (rowText, option) => {
  const row = page.locator('.setting-row').filter({ hasText: rowText }).first();
  await row.locator('button').first().click();
  await page.locator('.menu .menu-item').filter({ hasText: option }).first().click();
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
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: '使用系统选择器' }).click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('settings probe');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SETTINGS_OK'));
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');

  // Settings controls are real pointer operations; dropdowns must be inside
  // the modal dialog's top layer or the pointer never reaches them.
  await openSettings();
  await page.locator('.theme-card').filter({ hasText: '深色' }).click();
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'dark');
  await page.locator('.theme-card').filter({ hasText: '浅色' }).click();
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'light');
  const fontBefore = await page.locator('.font-stepper-value').first().innerText();
  await page.locator('.font-stepper-arrow[aria-label="增大字号"]').first().click();
  const fontAfter = await page.locator('.font-stepper-value').first().innerText();
  assert.notEqual(fontBefore, fontAfter);
  await page.locator('.font-expander').first().click();
  await page.locator('input[aria-label="文本字体"]').fill('Menlo, monospace');
  await page.locator('input[aria-label="文本字体"]').press('Enter');
  await chooseMenu('权限', '完全权限');
  await chooseMenu('工作步骤展示', '详细');
  await chooseMenu('工作步骤收起时机', '下次有新消息时');
  await chooseMenu('网页链接默认打开方式', '默认浏览器');
  await chooseMenu('繁忙时的发送行为', '插话发送');
  await chooseMenu('性能与用量', '简洁');
  let saved = await settings();
  assert.equal(saved.defaultPermission, 'full_access');
  assert.equal(saved.workSteps, 'detailed');
  assert.equal(saved.collapseAfter, 'next');
  assert.equal(saved.linkOpen, 'system');
  assert.equal(saved.busySend, 'steer');
  assert.equal(saved.performance, 'compact');
  assert.equal(saved.textFontFamily, 'Menlo, monospace');
  evidence.settings_controls = saved;
  await page.getByRole('button', { name: '编辑快捷键' }).click();
  assert.ok(await page.locator('.menu .menu-item').count() >= 8);
  await page.keyboard.press('Escape');
  await page.locator('#settings-nav-list button').filter({ hasText: '模型' }).click();
  const useModel = page.locator('.provider-row button').filter({ hasText: '使用' }).first();
  await useModel.click();
  await page.waitForFunction(async id => (await window.desktop.snapshot(id)).model.model === 'n', sessionId);
  evidence.model_switch = (await page.evaluate(id => window.desktop.snapshot(id), sessionId)).model;
  await page.locator('#settings-nav-list button').filter({ hasText: '内置插件' }).click();
  assert.match(await page.locator('#settings-content').innerText(), /未提供插件目录/);
  await page.locator('#settings-nav-list button').filter({ hasText: 'Agent 预设' }).click();
  await page.waitForFunction(() => document.querySelector('#settings-content')?.innerText.includes('code-architect'));
  await page.getByRole('button', { name: '查看配置' }).first().click();
  await page.waitForSelector('.preset-dialog[open]');
  await page.locator('.preset-dialog .modal-head button').click();
  assert.equal(await page.getByRole('button', { name: '设为默认' }).first().isDisabled(), true);
  await closeSettings();
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'light');
  saved = await settings();
  assert.equal(saved.defaultPermission, 'full_access');
  assert.equal(saved.busySend, 'steer');
  evidence.settings_persisted_after_reload = true;

  // The same anchor opens the view menu repeatedly; each selection must
  // survive. A stale outside-click listener used to swallow every menu after
  // the first.
  await page.locator('.session-row').first().click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  for (const [option, key, value] of [['按工作区树', 'groupMode', 'tree'], ['平铺', 'groupMode', 'flat'], ['名称', 'sortMode', 'name'], ['创建时间', 'sortMode', 'created']]) {
    await page.locator('#view-options').click();
    await page.waitForSelector('.menu');
    await page.locator('.menu .menu-item').filter({ hasText: option }).first().click();
    assert.equal((await settings())[key], value, `${option} did not persist`);
  }
  evidence.repeated_view_menu = 'tree/flat/name/created';
  await page.locator('#view-options').click();
  await page.waitForSelector('.menu');
  await page.locator('.menu .menu-item').filter({ hasText: '按工作区' }).first().click();
  await page.locator('#view-options').click();
  await page.waitForSelector('.menu');
  await page.locator('.menu .menu-item').filter({ hasText: '最近更新' }).first().click();
  for (let index = 0; index < 2; index += 1) {
    await page.locator('#header-more').click();
    await page.waitForSelector('.menu');
    assert.ok(await page.locator('.menu .menu-item').count() >= 2);
    await page.keyboard.press('Escape');
  }
  evidence.repeated_header_menu = true;

  // Slash commands are keyboard-driven and must operate on the same session.
  await page.locator('#message').fill('/diff');
  await page.waitForFunction(() => document.querySelector('#command-popover'));
  await page.locator('#message').press('Enter');
  await page.waitForSelector('.review-head, .review-status', { timeout: 15000 });
  evidence.slash_diff = true;
  await page.locator('#message').fill('@');
  await page.waitForFunction(() => document.querySelector('#at-popover'));
  assert.match(await page.locator('#at-popover').innerText(), /尚未接入/);
  await page.locator('#message').press('Escape');
  await page.waitForFunction(() => !document.querySelector('#at-popover'));
  evidence.at_reference_is_explicit_ui_only = true;
  assert.deepEqual(errors, []);
  evidence.page_errors = errors;
  evidence.passed = true;
  await writeFile(path.join(output, 'commands-settings-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'commands-settings-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'commands-settings-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
