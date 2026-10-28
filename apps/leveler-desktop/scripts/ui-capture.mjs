// Capture the rewritten Desktop UI for reference/actual/overlay evidence.
// The provider is configured but never called; no session content is fabricated.
import { _electron as electron } from 'playwright';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-ui-capture-');
const home = path.join(temporary, 'home');
const output = path.join(root, '../../test-output/desktop/ui-parity-round2-2026-10-20');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:1"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\nvision=true\n', { mode: 0o600 });

let application;
let page;
const errors = [];
const shot = async name => {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, `${name}.png`) });
};
const resize = async (width, height = 820) => {
  await application.evaluate(({ BrowserWindow }, size) => BrowserWindow.getAllWindows()[0].setContentSize(size.width, size.height), { width, height });
  await page.waitForFunction(expected => innerWidth === expected, width);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
};
const closeOverlays = async () => {
  if (await page.locator('#settings-dialog').evaluate(node => node.open).catch(() => false)) await page.locator('#settings-close').click();
  if (!(await page.locator('#rightbar').evaluate(node => node.hidden).catch(() => true))) {
    const close = page.locator('.pane-chrome button[aria-label="收起右侧边栏"]').first();
    if (await close.count()) await close.click();
  }
};
const setTheme = async theme => {
  if (!(await page.locator('#settings-dialog').evaluate(node => node.open))) {
    await page.locator('#account-menu').click();
    await page.getByRole('menuitem', { name: /设置/ }).click();
    await page.waitForFunction(() => document.querySelector('#settings-dialog').open);
  }
  await page.locator('.theme-card').filter({ hasText: theme }).click();
};
const openRightbar = async () => {
  if (await page.locator('#rightbar').evaluate(node => node.hidden)) {
    if (await page.locator('#floating-right-toggle').isVisible()) await page.locator('#floating-right-toggle').click();
    else if (await page.locator('#header-right-toggle').isVisible()) await page.locator('#header-right-toggle').click();
    else await page.keyboard.press('Meta+Alt+B');
  }
  if (!(await page.locator('.pane-tab').filter({ hasText: '开始' }).count())) {
    const add = page.getByRole('button', { name: '新标签页' }).first();
    if (await add.count()) await add.click();
  }
  await page.getByText('开始', { exact: true }).waitFor();
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
  await resize(1280, 820);
  await shot('actual-home-1280-light');

  await openRightbar();
  await shot('actual-start-1280-light');
  await page.getByText('工作区文件', { exact: true }).click();
  await shot('actual-files-1280-light');
  await page.getByRole('button', { name: '新标签页' }).click();
  await page.getByRole('button', { name: /^新建终端/ }).click();
  await shot('actual-terminal-1280-light');
  await page.getByRole('button', { name: '新标签页' }).click();
  await page.getByRole('button', { name: /^浏览器/ }).click();
  await shot('actual-browser-1280-light');

  await page.locator('#account-menu').click();
  await page.getByRole('menuitem', { name: /设置/ }).click();
  await page.waitForFunction(() => document.querySelector('#settings-dialog').open);
  await page.getByText('通用设置', { exact: true }).click();
  await page.locator('.theme-card').filter({ hasText: '浅色' }).click();
  await shot('actual-settings-1280-light');
  for (const [label, name] of [['模型', 'models'], ['内置插件', 'plugins'], ['Agent 预设', 'presets']]) {
    await page.locator('#settings-nav-list button').filter({ hasText: label }).click();
    await shot(`actual-settings-${name}-1280-light`);
  }
  await page.locator('#settings-nav-list button').filter({ hasText: '通用设置' }).click();
  await page.locator('.theme-card').filter({ hasText: '深色' }).click();
  await shot('actual-settings-1280-dark');
  await page.locator('#settings-close').click();
  await shot('actual-home-1280-dark');
  await setTheme('浅色');
  await page.locator('#settings-close').click();

  for (const width of [1440, 1024, 1023, 960, 768, 700, 520]) {
    await closeOverlays();
    await resize(width, width === 520 ? 600 : 820);
    await shot(`actual-home-${width}-light`);
  }

  await resize(1024, 768);
  await openRightbar();
  await shot('actual-start-1024-light');
  await page.keyboard.press('Meta+,');
  await page.waitForFunction(() => document.querySelector('#settings-dialog').open);
  await shot('actual-settings-1024-light');
  await page.locator('#settings-close').click();
  await closeOverlays();
  await resize(520, 600);
  await openRightbar();
  await shot('actual-start-520-light');
  await page.keyboard.press('Meta+,');
  await page.waitForFunction(() => document.querySelector('#settings-dialog').open);
  await shot('actual-settings-520-light');

  const state = {
    reference: { head: '687f282e8625c6b8d11ff58211a90524be2e4317', version: '0.2.1-rc.2', electron: '44.7.0' },
    actual: { head: (await import('node:child_process')).execFileSync('git', ['rev-parse', 'HEAD'], { cwd: root, encoding: 'utf8' }).trim(), working_tree: 'uncommitted' },
    captures: [
      'actual-home-1280-light', 'actual-home-1280-dark', 'actual-start-1280-light', 'actual-files-1280-light',
      'actual-terminal-1280-light', 'actual-browser-1280-light', 'actual-settings-1280-light',
      'actual-settings-1280-dark', 'actual-home-1440-light', 'actual-home-1024-light', 'actual-home-1023-light',
      'actual-home-960-light', 'actual-home-768-light', 'actual-home-700-light', 'actual-home-520-light',
      'actual-start-1024-light', 'actual-settings-1024-light', 'actual-start-520-light', 'actual-settings-520-light',
    ],
    provider_calls: 0,
    page_errors: errors,
  };
  await writeFile(path.join(output, 'capture-state.json'), `${JSON.stringify(state, null, 2)}\n`);
  console.log(JSON.stringify(state, null, 2));
} finally {
  if (application) await application.close();
}
