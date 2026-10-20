// Real Electron acceptance for view tabs and the read-only Workspace file tree.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, mkdir, writeFile, readFile, readdir, symlink, rm } from 'node:fs/promises';
import { createServer } from 'node:http';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-files-acceptance-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = path.join(root, 'acceptance-output/round4-files');
await mkdir(home);
await mkdir(path.join(workspace, 'src', 'nested'), { recursive: true });
await mkdir(path.join(workspace, 'empty'));
await writeFile(path.join(workspace, 'README.md'), '# Files fixture\n\n真实 Markdown 内容。\n');
await writeFile(path.join(workspace, 'src', 'main.ts'), 'export const answer = 42;\n');
await writeFile(path.join(workspace, 'src', 'nested', '中文 文件名.txt'), '含空格与中文的文件内容\n');
await writeFile(path.join(workspace, 'large.txt'), 'large-line\n'.repeat(120000));
const outside = path.join(temporary, 'outside.txt');
await writeFile(outside, 'OUTSIDE_SECRET\n');
await symlink(outside, path.join(workspace, 'escape-link'));
await mkdir(output, { recursive: true });
const server = createServer(async (request, response) => {
  if (request.method === 'GET') { response.writeHead(200, { 'content-type': 'text/plain' }); response.end('fixture'); return; }
  for await (const _chunk of request) { /* drain */ }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  response.write(`data: ${JSON.stringify({ id: 'tabs', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: { content: 'TAB_SEED_OK' }, finish_reason: null }] })}\n\n`);
  response.write(`data: ${JSON.stringify({ id: 'tabs', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"
[providers.fixture]
protocol="openai_chat"
base_url="${base}"
api_key="test-only"
[models.m]
provider="fixture"
model_id="fixture"
context_window=131072
`, { mode: 0o600 });

const evidence = { scope: 'isolated real Electron/Runtime, dialog seam for workspace choice, no provider request' };
let app;
let page;
const errors = [];
const mouseClick = async locator => {
  await locator.scrollIntoViewIfNeeded();
  const box = await locator.boundingBox();
  assert.ok(box, 'row must have a box');
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
};
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'electron')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));

  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const created = await page.evaluate(async () => {
    const folder = await window.desktop.chooseFolder();
    const task = await window.desktop.createTask(folder.id, crypto.randomUUID());
    return { sessionId: task.session.id };
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), created.sessionId);
  await page.locator(`.session-row[data-session-id="${created.sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');

  // Chat / Trajectory: 20 real mouse alternations, keyboard, and draft retention.
  await page.locator('#message').fill('fixture tab seed');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('TAB_SEED_OK'));
  const chat = page.locator('#view-tabs [data-view="chat"]');
  const trajectory = page.locator('#view-tabs [data-view="trajectory"]');
  await page.locator('#message').fill('TAB_DRAFT_SENTINEL');
  const tabFailures = [];
  for (let index = 0; index < 20; index += 1) {
    await mouseClick(trajectory);
    await page.waitForTimeout(60);
    if (await trajectory.getAttribute('aria-selected') !== 'true') tabFailures.push({ index, at: 'trajectory' });
    await mouseClick(chat);
    await page.waitForTimeout(60);
    if (await chat.getAttribute('aria-selected') !== 'true') tabFailures.push({ index, at: 'chat' });
  }
  assert.deepEqual(tabFailures, []);
  assert.equal(await page.locator('#message').inputValue(), 'TAB_DRAFT_SENTINEL');
  await chat.focus();
  await page.keyboard.press('ArrowRight');
  assert.equal(await trajectory.getAttribute('aria-selected'), 'true');
  await page.keyboard.press('ArrowLeft');
  assert.equal(await chat.getAttribute('aria-selected'), 'true');
  evidence.chatTrajectory = { alternations: 20, failures: tabFailures, keyboard: true, draft: true };
  await page.screenshot({ path: path.join(output, 'tabs-chat.png') });
  await trajectory.click();
  await page.screenshot({ path: path.join(output, 'tabs-trajectory.png') });
  await chat.click();

  // Files: tree, expansion, previews, refresh, symlink boundary.
  const openFilesTab = async () => {
    const open = await page.evaluate(() => document.body.classList.contains('rightbar-open'));
    if (!open) {
      if (await page.locator('#header-right-toggle').isVisible()) await page.locator('#header-right-toggle').click();
      else await mouseClick(page.locator('#floating-right-toggle'));
    }
    const filesTab = page.locator('.pane-tab').filter({ hasText: /^文件$/ });
    if (await filesTab.count()) await filesTab.first().click();
    else {
      if (!(await page.locator('.start-card').count())) await page.locator('.add-tab').click();
      await page.waitForSelector('.start-card');
      await mouseClick(page.locator('.start-card').filter({ hasText: '工作区文件' }));
    }
    await page.waitForSelector('.files-view');
  };
  await openFilesTab();
  await page.waitForSelector('.files-row');
  const rootNames = await page.locator('.files-row').evaluateAll(rows => rows.map(row => row.dataset.path));
  for (const expected of ['README.md', 'src', 'empty', 'large.txt', 'escape-link']) assert.ok(rootNames.includes(expected), `missing ${expected}`);
  evidence.filesRoot = rootNames;

  await mouseClick(page.locator('.files-row[data-path="src"]'));
  await page.waitForSelector('.files-row[data-path="src/main.ts"]');
  await mouseClick(page.locator('.files-row[data-path="src/nested"]'));
  await page.waitForFunction(() => [...document.querySelectorAll('.files-row')].some(row => row.dataset.path.includes('nested/') && row.dataset.path.endsWith('.txt')));
  evidence.expansion = true;
  evidence.unicodeNamePresent = true;

  await mouseClick(page.locator('.files-row[data-path="README.md"]'));
  await page.waitForFunction(() => document.querySelector('.pane-body .markdown')?.textContent.includes('真实 Markdown 内容'));
  const readme = await readFile(path.join(workspace, 'README.md'), 'utf8');
  assert.ok((await page.locator('.pane-body .markdown').textContent()).includes(readme.split('\n')[2]));

  await openFilesTab();
  const unicodePoint = await page.evaluate(() => {
    const row = [...document.querySelectorAll('.files-row')].find(item => item.dataset.path.includes('nested/') && item.dataset.path.endsWith('.txt'));
    row.scrollIntoView({ block: 'center' });
    const rect = row.getBoundingClientRect();
    return { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 };
  });
  await page.mouse.click(unicodePoint.x, unicodePoint.y);
  await page.waitForFunction(() => document.querySelector('.pane-body')?.textContent.includes('含空格与中文的文件内容'));
  evidence.unicodePreview = true;

  await openFilesTab();
  await mouseClick(page.locator('.files-row[data-path="large.txt"]'));
  await page.waitForSelector('.preview-truncated');
  evidence.largeTruncated = true;

  await openFilesTab();
  assert.equal(await page.locator('.files-row[data-path="escape-link"]').isDisabled(), true);
  const escaped = await page.evaluate(async ({ id, outsidePath }) => {
    for (const request of [{ sessionId: id, path: '../../etc/passwd' }, { sessionId: id, path: outsidePath }]) {
      try { await window.desktop.filesRead(request); return 'ALLOWED'; } catch { /* rejected */ }
    }
    return 'REJECTED';
  }, { id: created.sessionId, outsidePath: outside });
  assert.equal(escaped, 'REJECTED');
  evidence.symlinkAndTraversalRejected = true;

  const before = createHash('sha256').update(await readFile(path.join(workspace, 'README.md'))).digest('hex');
  await writeFile(path.join(workspace, 'new-file.txt'), 'added after open\n');
  await openFilesTab();
  await mouseClick(page.locator('.view-toolbar button[aria-label="刷新文件列表"]'));
  await page.waitForSelector('.files-row[data-path="new-file.txt"]');
  evidence.refresh = true;
  const after = createHash('sha256').update(await readFile(path.join(workspace, 'README.md'))).digest('hex');
  assert.equal(before, after);
  evidence.readOnly = true;
  await page.screenshot({ path: path.join(output, 'files-tree.png') });

  const closeTab = async title => {
    const tab = page.locator('.pane-tab').filter({ hasText: new RegExp(`^${title.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`) }).first();
    await tab.waitFor();
    await tab.locator('.tab-close').click();
  };
  const openStart = async () => {
    await page.locator('.add-tab').click();
    await page.waitForSelector('.start-card');
  };
  await openStart();
  await mouseClick(page.locator('.start-card').filter({ hasText: '新建终端' }));
  await page.waitForSelector('.terminal-view');
  assert.ok((await page.locator('.terminal-view').innerText()).includes('交互式终端尚未接入'));
  evidence.terminalTab = true;
  await closeTab('终端');
  await page.waitForSelector('.terminal-view', { state: 'detached' });
  await openStart();
  await mouseClick(page.locator('.start-card').filter({ hasText: '浏览器' }));
  await page.waitForSelector('.browser-address-wrap input');
  evidence.browserTab = true;
  await closeTab('浏览器');
  await page.waitForSelector('.browser-address-wrap input', { state: 'detached' });
  await closeTab('large.txt');
  await openFilesTab();
  await closeTab('文件');
  await page.waitForFunction(() => !document.querySelector('.files-view'));
  evidence.reopenBefore = await page.evaluate(() => ({ tabs: [...document.querySelectorAll('.pane-tab')].map(tab => tab.textContent.trim()), cards: document.querySelectorAll('.start-card').length, body: document.querySelector('.pane-body')?.innerText.slice(0, 80), rightbar: document.body.className }));
  await openFilesTab();
  await page.waitForSelector('.files-row[data-path="README.md"]');
  evidence.closeAndReopen = true;

  // No-workspace session keeps the honest empty state.
  const bare = await page.evaluate(async () => (await window.desktop.createTask(null, crypto.randomUUID())).session.id);
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), bare);
  await page.locator(`.session-row[data-session-id="${bare}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await openFilesTab();
  await page.waitForFunction(() => document.querySelector('.pane-body')?.textContent.includes('当前会话没有工作区'));
  evidence.noWorkspaceEmptyState = true;
  await page.screenshot({ path: path.join(output, 'files-no-workspace.png') });

  await page.locator(`.session-row[data-session-id="${created.sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await openFilesTab();
  await page.waitForSelector('.files-row[data-path="README.md"]');
  evidence.sessionSwitch = true;

  await app.close();
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'electron')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), created.sessionId);
  await page.locator(`.session-row[data-session-id="${created.sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  for (let index = 0; index < 5; index += 1) {
    await page.locator('#view-tabs [data-view="trajectory"]').click();
    assert.equal(await page.locator('#view-tabs [data-view="trajectory"]').getAttribute('aria-selected'), 'true');
    await page.locator('#view-tabs [data-view="chat"]').click();
    assert.equal(await page.locator('#view-tabs [data-view="chat"]').getAttribute('aria-selected'), 'true');
  }
  await openFilesTab();
  await page.waitForSelector('.files-row[data-path="README.md"]');
  await mouseClick(page.locator('.files-row[data-path="README.md"]'));
  await page.waitForFunction(() => document.querySelector('.pane-body .markdown')?.textContent.includes('真实 Markdown 内容'));
  evidence.restart = true;

  assert.deepEqual(errors, []);
  evidence.pageErrors = errors;
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.passed = false;
  evidence.error = error.message;
  evidence.pageErrors = errors;
  console.error(JSON.stringify(evidence, null, 2));
  throw error;
} finally {
  await writeFile(path.join(output, 'files-evidence.json'), JSON.stringify(evidence, null, 2) + '\n').catch(() => {});
  if (app) await app.close().catch(() => {});
  await new Promise(resolve => server.close(resolve)).catch(() => {});
  await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
