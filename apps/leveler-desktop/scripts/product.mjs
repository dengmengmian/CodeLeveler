// First-use smoke on the real Desktop: blank session, workspace session,
// file read, settings persistence, real window close and history reopen.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, realpath } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-product-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await writeFile(path.join(workspace, 'readme.txt'), 'PRODUCT_FIRST_USE\n');
const server = createServer(async (request, response) => {
  for await (const _chunk of request) { /* drain */ }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.write(`data: ${JSON.stringify({ id: 'p', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { content: 'PRODUCT_OK' }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ id: 'p', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });
const evidence = { scope: 'first use, workspace files, settings persistence, real window close and history reopen; test-only provider' };
const errors = [];
let app;
let page;
const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
};
try {
  await launch();
  assert.deepEqual(await page.evaluate(() => ({ require: typeof window.require, process: typeof window.process })), { require: 'undefined', process: 'undefined' });
  await page.locator('#new-session').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') && document.querySelector('#conversation-status').textContent !== '连接 Runtime…');
  assert.match(await page.locator('#workspace-picker').innerText(), /选择工作区/);
  await page.locator('#message').fill('first blank session');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('PRODUCT_OK'));
  const blankId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  evidence.blank_session = blankId;
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: '使用系统选择器' }).click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('workspace first use');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('PRODUCT_OK'));
  const workspaceId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), workspaceId);
  assert.equal(snapshot.repository, await realpath(workspace));
  evidence.workspace_session = workspaceId;
  await page.locator('#header-right-toggle').click();
  const filesTab = page.locator('.pane-tab').filter({ hasText: /^文件$/ });
  if (await filesTab.count()) await filesTab.first().click();
  else {
    await page.waitForSelector('.start-card');
    await page.locator('.start-card').filter({ hasText: '工作区文件' }).click();
  }
  await page.waitForSelector('.files-row[data-path="readme.txt"]');
  await page.locator('.files-row[data-path="readme.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.innerText.includes('PRODUCT_FIRST_USE'));
  evidence.file_preview = true;
  // Settings persist across a renderer reload.
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
  await page.locator('.theme-card').filter({ hasText: '深色' }).click();
  await page.locator('#settings-close').click();
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'dark');
  evidence.theme_persisted = true;
  // A real window close after the turn settles must exit, not ask about a
  // phantom running task.
  const exited = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('window close did not exit')), 10000);
    app.process().once('exit', () => { clearTimeout(timer); resolve(); });
  });
  await page.evaluate(() => window.close());
  await exited;
  app = null;
  evidence.close_exited = true;
  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), workspaceId);
  await page.locator(`.session-row[data-session-id="${workspaceId}"]`).click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('PRODUCT_OK'));
  assert.equal(await page.locator('.message.user').count(), 1);
  assert.equal(await page.evaluate(id => window.desktop.snapshot(id).then(snapshot => snapshot.repository), workspaceId), await realpath(workspace));
  evidence.history_reopened = true;
  await page.screenshot({ path: path.join(output, 'product-first-use.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'product-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'product-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'product-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
