// Real Electron + Runtime PTY acceptance; no provider or simulated terminal.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-terminal-');
const workspace = path.join(temporary, 'workspace');
const home = path.join(temporary, 'home');
await mkdir(workspace); await mkdir(home);
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:9"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n');
let app;
try {
  app = await electron.launch({ args: [root, `--user-data-dir=${temporary}/electron`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler') } });
  const page = await app.firstWindow();
  page.on('pageerror', error => console.error('PAGE ERROR', error));
  page.on('console', message => { if (message.type() === 'error') console.error(message.text()); });
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const id = await page.evaluate(async () => { const folder = await window.desktop.chooseFolder(); return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id; });
  await page.reload();
  await page.locator(`.session-row[data-session-id="${id}"]`).click();
  await page.keyboard.press('Control+Backquote');
  await page.locator('.xterm-helper-textarea').waitFor({ state: 'attached', timeout: 5000 });
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running');
  const input = page.locator('.xterm-helper-textarea');
  await input.focus();
  await page.keyboard.type("printf 'TERMINAL_REAL_%s\\n' OK; pwd; echo $$"); await page.keyboard.press('Enter');
  const content = () => page.locator('.xterm-rows').innerText();
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('TERMINAL_REAL_OK'));
  assert.ok((await content()).includes(workspace));
  const terminalId = await page.locator('.terminal-view').getAttribute('data-terminal-id');
  await page.locator('[aria-label="收起右侧边栏"]').click();
  await page.keyboard.press('Control+Backquote');
  assert.equal(await page.locator('.terminal-view').getAttribute('data-terminal-id'), terminalId);
  await input.focus(); await page.keyboard.type('sleep 30'); await page.keyboard.press('Enter'); await page.keyboard.press('Control+c');
  await page.keyboard.type("printf 'INTERRUPTED_%s\\n' OK"); await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('INTERRUPTED_OK'));
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.locator('.terminal-view').waitFor({ state: 'detached' });
  const observed = await page.evaluate(async ({ session_id, terminal_id }) => window.desktop.observeTerminal({ session_id, terminal_id, cursor: 0, max_bytes: 65536 }), { session_id: id, terminal_id: terminalId });
  assert.ok(['exited', 'killed'].includes(observed.status), JSON.stringify(observed));
  console.log('Real terminal: shell output, cwd, hidden/reopen identity, Ctrl+C, close with observed process exit passed.');
} finally {
  if (app) { await app.evaluate(({ app }) => app.exit(0)).catch(() => {}); await app.close().catch(() => {}); }
  await rm(temporary, { recursive: true, force: true });
}
