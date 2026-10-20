// Project/sidebar acceptance on the current UI: same-basename workspaces are
// distinct groups, rename and archive are real Runtime mutations, code copy is
// exact, and an oversized attachment fails visibly without losing the draft.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, realpath } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-project-');
const home = path.join(temporary, 'home');
const first = path.join(temporary, 'a/shared');
const second = path.join(temporary, 'b/shared');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(first, { recursive: true });
await mkdir(second, { recursive: true });
await mkdir(output, { recursive: true });
const code = 'console.log("actual clipboard");';
const answer = `# Project answer\n\n\`\`\`js\n${code}\n\`\`\`\n`;
const server = createServer(async (request, response) => {
  for await (const _chunk of request) { /* drain */ }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.write(`data: ${JSON.stringify({ id: 'p', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: { content: answer }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ id: 'p', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });
const evidence = { scope: 'real Electron + real Runtime; test-only provider and native chooser seam; clipboard restored' };
const errors = [];
let app;
let page;
let clipboardCaptured = false;
const chooseWorkspace = async folder => {
  await app.evaluate(({ dialog }, selected) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [selected] }); }, folder);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: '使用系统选择器' }).click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('shared'));
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
  const looseId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await chooseWorkspace(first);
  await page.locator('#message').fill('first workspace task');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.innerText.includes('actual clipboard'));
  const firstId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await chooseWorkspace(second);
  await page.locator('#message').fill('second workspace task');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelectorAll('.message.assistant .markdown').length >= 1 && document.querySelector('#conversation-status')?.textContent === '已回答');
  const secondId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  const paths = [await realpath(first), await realpath(second)];
  assert.notEqual(paths[0], paths[1]);
  for (const workspacePath of paths) {
    const group = page.locator(`.workspace-group[data-workspace-path="${workspacePath}"]`);
    assert.equal(await group.count(), 1, `group ${workspacePath}`);
    assert.equal(await group.locator('.session-row').count(), 1);
  }
  evidence.same_basename_groups = paths;
  const fold = page.locator(`.workspace-group[data-workspace-path="${paths[0]}"] .group-fold`);
  await page.locator('#message').fill('Draft survives group fold');
  await fold.click();
  assert.equal(await page.locator(`.workspace-group[data-workspace-path="${paths[0]}"] .workspace-sessions`).isVisible(), false);
  assert.equal(await page.locator('#message').inputValue(), 'Draft survives group fold');
  await fold.click();
  evidence.group_fold_draft = true;
  // Inline rename is the current row-level mutation; it must land in Runtime.
  const row = page.locator(`.session-row[data-session-id="${firstId}"]`);
  await row.locator('.session-title').dblclick();
  await page.locator('.sidebar-rename-input').fill('Renamed shared task');
  await page.locator('.sidebar-rename-input').press('Enter');
  await page.waitForFunction(id => window.desktop.snapshot(id).then(snapshot => snapshot.goal === 'Renamed shared task'), firstId);
  await page.waitForFunction(id => document.querySelector(`.session-row[data-session-id="${id}"] .session-title`)?.textContent === 'Renamed shared task', firstId, { timeout: 5000 });
  evidence.rename = (await page.evaluate(id => window.desktop.snapshot(id), firstId)).goal;
  // Archive hides from the default index; the record stays reachable with
  // the archived filter.
  const archiveRow = page.locator(`.session-row[data-session-id="${secondId}"]`);
  await archiveRow.hover();
  await archiveRow.locator('.session-action[aria-label^="归档"]').click();
  await page.locator('.confirm-modal .button.danger').click();
  await page.waitForFunction(id => !document.querySelector(`.session-row[data-session-id="${id}"]`), secondId);
  assert.equal((await page.evaluate(() => window.desktop.listTasks())).tasks.some(task => task.id === secondId), false);
  await page.locator('#view-options').click();
  await page.locator('.menu .menu-item').filter({ hasText: '包含归档' }).first().click();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), secondId);
  assert.match(await page.locator(`.session-row[data-session-id="${secondId}"] .session-meta`).innerText(), /已归档/);
  evidence.archive = true;
  await page.locator('#view-options').click();
  await page.locator('.menu .menu-item').filter({ hasText: '默认（不含归档）' }).first().click();
  // Exact code copy from a real assistant markdown response.
  await app.evaluate(async ({ clipboard, ClipboardItem }) => {
    const saved = [];
    for (const item of await clipboard.read()) {
      const data = Object.create(null);
      for (const type of item.types) data[type] = await item.getType(type);
      saved.push(new ClipboardItem(data));
    }
    globalThis.__projectClipboard = saved;
  });
  clipboardCaptured = true;
  await page.locator(`.session-row[data-session-id="${looseId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await page.locator('#message').fill('show code');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.innerText.includes('actual clipboard'));
  await page.locator('.message.assistant .code-copy[aria-label="复制代码"]').first().click();
  await page.waitForTimeout(200);
  assert.equal(await app.evaluate(({ clipboard }) => clipboard.readText()), code);
  evidence.code_copy = true;
  // An oversized pick fails before Runtime and leaves the draft intact.
  const oversized = path.join(temporary, 'oversized.bin');
  await writeFile(oversized, Buffer.alloc(21 * 1024 * 1024));
  await app.evaluate(({ dialog }, selected) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [selected] }); }, oversized);
  await page.locator('#message').fill('Draft survives upload failure');
  await page.locator('#attachment-add').click();
  await page.locator('#command-popover .menu-item[data-command-id="file"]').click();
  await page.waitForSelector('.attachment-chip[data-attachment-status="failed"]');
  assert.match(await page.locator('.attachment-chip').innerText(), /20 MiB/);
  assert.equal(await page.locator('#message').inputValue(), 'Draft survives upload failure');
  evidence.upload_failure = true;
  await page.screenshot({ path: path.join(output, 'project-interactions.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'project-interactions-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'project-interactions-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'project-interactions-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  try {
    if (app && clipboardCaptured) await app.evaluate(async ({ clipboard }) => {
      if (globalThis.__projectClipboard.length) await clipboard.write(globalThis.__projectClipboard);
      else clipboard.clear();
      delete globalThis.__projectClipboard;
    });
  } finally {
    if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
    await new Promise(resolve => server.close(resolve));
  }
}
