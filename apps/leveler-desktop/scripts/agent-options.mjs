// R4 execution options through the real settings surface and config file.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-options-');
const home = path.join(temporary, 'home');
await mkdir(home, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:9"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n', { mode: 0o600 });
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
const evidence = { scope: 'real Electron + host config transaction; isolated home' };
let app;
let page;
const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
};
const openOptions = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
  await page.locator('#settings-nav-list button').filter({ hasText: 'Agent 与技能' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('.setting-row')].some(row => row.textContent.includes('允许委派子智能体')));
};
try {
  await launch();
  await openOptions();
  const row = page.locator('.setting-row').filter({ hasText: '允许委派子智能体' }).first();
  await row.locator('.switch').click();
  await page.getByRole('button', { name: '保存执行选项' }).click();
  await page.waitForFunction(() => document.querySelector('.search-save-note')?.textContent.includes('已保存'));
  const text = await readFile(path.join(home, 'config.toml'), 'utf8');
  assert.match(text, /\[agents\]\s*\n\s*delegation = false/, text);
  assert.match(text, /\[vcs\]\s*\n\s*co_author = true/, text);
  const view = await page.evaluate(() => window.desktop.agentOptions());
  assert.equal(view.delegation, false);
  evidence.saved = { delegation: view.delegation, co_author: view.co_author, prompt_suggestions: view.prompt_suggestions, away_summary: view.away_summary };
  await page.screenshot({ path: path.join(output, 'agent-options.png') });
  await app.evaluate(({ app: electronApp }) => electronApp.exit(0));
  await new Promise(resolve => setTimeout(resolve, 1200));
  await launch();
  await openOptions();
  const persisted = await page.evaluate(() => window.desktop.agentOptions());
  assert.equal(persisted.delegation, false);
  evidence.restart_kept = true;
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'agent-options-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
}
