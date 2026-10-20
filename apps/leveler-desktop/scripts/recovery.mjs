// Failure and recovery acceptance: missing workspace, deleted preview file,
// provider rejection, and a lost Desktop bridge. Each path must report the
// real failure and must not fabricate success.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, rm, readdir } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-recovery-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, 'acceptance-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'keep.txt'), 'RECOVERY_KEEP\n');
let failOnce = true;
const server = createServer(async (request, response) => {
  let text = '';
  for await (const chunk of request) text += chunk;
  let body = {};
  try { body = JSON.parse(text); } catch {}
  const names = (body.tools ?? []).map(tool => tool.function?.name);
  if (!names.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: 'mem', choices: [{ index: 0, message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  const lastUser = [...(body.messages ?? [])].reverse().find(message => message.role === 'user');
  const lastText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
  if (lastText.includes('FAIL_HARD')) {
    response.writeHead(401, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'fixture authentication rejected' } }));
    return;
  }
  if (lastText.includes('FAIL_ONCE') && failOnce) {
    failOnce = false;
    response.writeHead(500, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'fixture upstream exploded' } }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const chunk = (delta, finish) => JSON.stringify({ id: 'r', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] });
  response.write(`data: ${chunk({ content: 'RECOVERY_OK' }, null)}\n\n`);
  response.write(`data: ${chunk({}, 'stop')}\n\n`);
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });
const evidence = { scope: 'failure and recovery; real Runtime; test-only provider' };
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
const ui = () => page.evaluate(() => ({
  status: document.querySelector('#conversation-status')?.textContent,
  banner: document.querySelector('#error-banner')?.hidden ? null : document.querySelector('#error-banner')?.innerText,
  composer: document.querySelector('#composer-error')?.hidden ? null : document.querySelector('#composer-error')?.innerText,
  assistant: [...document.querySelectorAll('.message.assistant .markdown')].map(node => node.textContent),
}));
const send = async text => {
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
};
try {
  await launch();
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: '使用系统选择器' }).click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await send('SEED');
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('RECOVERY_OK'));
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await page.locator('#header-right-toggle').click();
  const filesTab = page.locator('.pane-tab').filter({ hasText: /^文件$/ });
  if (await filesTab.count()) await filesTab.first().click();
  else {
    await page.waitForSelector('.start-card');
    await page.locator('.start-card').filter({ hasText: '工作区文件' }).click();
  }
  await page.waitForSelector('.files-row[data-path="keep.txt"]');
  await page.locator('.files-row[data-path="keep.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.innerText.includes('RECOVERY_KEEP'));
  await rm(path.join(workspace, 'keep.txt'));
  await page.locator('button[aria-label="重新读取文件"]').first().click();
  await page.waitForFunction(() => { const node = document.querySelector('.preview-body .review-status.error'); return !!node && node.getClientRects().length > 0; });
  const previewError = await page.locator('.preview-body .review-status.error').innerText();
  assert.match(previewError, /文件或目录不存在/);
  assert.doesNotMatch(previewError, /Error invoking remote method|desktop:files-read/);
  await page.locator('.pane-tab').filter({ hasText: /^文件$/ }).first().click();
  await page.locator('button[aria-label="刷新文件列表"]').click();
  await page.waitForSelector('.files-empty-dir');
  assert.deepEqual(await readdir(workspace), []);
  assert.equal(await page.locator('.files-row').count(), 0);
  assert.equal(await page.locator('.files-error').count(), 0);
  evidence.empty_directory_matches_disk = true;
  await writeFile(path.join(workspace, 'keep.txt'), 'RECOVERY_RESTORED\n');
  await page.locator('.pane-tab').filter({ hasText: /^keep\.txt$/ }).first().click();
  await page.locator('.preview-body .review-status.error button').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.innerText.includes('RECOVERY_RESTORED'));
  await page.locator('.pane-tab').filter({ hasText: /^文件$/ }).first().click();
  await page.locator('button[aria-label="刷新文件列表"]').click();
  await page.waitForSelector('.files-row[data-path="keep.txt"]');
  assert.equal(await page.locator('.files-empty-dir').count(), 0);
  await page.locator('.files-row[data-path="keep.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.innerText.includes('RECOVERY_RESTORED'));
  evidence.deleted_preview_recovered = true;
  await send('FAIL_HARD');
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent === '失败', null, { timeout: 60000 });
  assert.match((await ui()).banner, /fixture authentication rejected/);
  await send('RECOVER');
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent === '已回答' && document.querySelector('#error-banner').hidden, null, { timeout: 60000 });
  evidence.provider_failure_recovered = true;
  const runtime = await page.evaluate(() => window.desktop.runtimeInfo());
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await rm(workspace, { recursive: true, force: true });
  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), sessionId);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => !document.querySelector('#error-banner').hidden);
  const missing = await page.locator('#error-banner').innerText();
  assert.match(missing, /工作区文件夹不存在或无法访问/);
  assert.doesNotMatch(missing, /os error|No such file/);
  assert.equal((await ui()).status, '');
  const direct = await page.evaluate(async id => {
    try { const task = (await window.desktop.listTasks()).tasks.find(item => item.id === id); await window.desktop.openTask(task); return null; }
    catch (error) { return error.message; }
  }, sessionId);
  assert.match(direct, /工作区文件夹不存在或无法访问/);
  await mkdir(workspace, { recursive: true });
  // Restore the fixture content too: a restored directory alone is genuinely
  // empty, even when the persisted pane still has an old file preview tab.
  await writeFile(path.join(workspace, 'keep.txt'), 'RECOVERY_RESTORED\n');
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await page.waitForTimeout(500);
  assert.equal((await ui()).banner, null);
  assert.ok((await ui()).assistant.some(text => text.includes('RECOVERY_OK')));
  evidence.missing_workspace_message_and_recovery = true;
  if (!(await page.locator('.pane-tab').filter({ hasText: /^文件$/ }).isVisible())) await page.locator('#header-right-toggle').click();
  await page.locator('.pane-tab').filter({ hasText: /^文件$/ }).first().click();
  await page.locator('button[aria-label="刷新文件列表"]').click();
  await page.waitForSelector('.files-row[data-path="keep.txt"]');
  assert.equal(await page.locator('.files-empty-dir').count(), 0);
  await page.locator('.files-row[data-path="keep.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.innerText.includes('RECOVERY_RESTORED'));
  evidence.restored_workspace_has_real_files = true;
  // A dead daemon is revived below Electron. The Desktop must keep the same
  // session, keep history, and accept the next message after the resync.
  process.kill(runtime.pid, 'SIGKILL');
  await page.waitForFunction(async old => {
    try { const info = await window.desktop.runtimeInfo(); return info.pid && info.pid !== old; }
    catch { return false; }
  }, runtime.pid, { timeout: 30000 });
  await send('AFTER_DAEMON_LOSS');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].filter(node => node.textContent.includes('RECOVERY_OK')).length >= 3, null, { timeout: 60000 });
  assert.equal(await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id'), sessionId);
  evidence.daemon_loss_recovered_same_session = true;

  const mainPid = app.process().pid;
  const childIds = execFileSync('pgrep', ['-P', String(mainPid)], { encoding: 'utf8' }).trim().split(/\s+/).filter(Boolean);
  const children = childIds.length ? execFileSync('ps', ['-o', 'pid=,command=', '-p', childIds.join(',')], { encoding: 'utf8' }) : '';
  const bridgeLine = String(children).split('\n').find(line => line.includes('desktop-bridge'));
  assert.ok(bridgeLine, 'desktop bridge child');
  process.kill(Number(bridgeLine.trim().split(/\s+/)[0]), 'SIGKILL');
  await page.waitForFunction(() => !document.querySelector('#connection-banner').hidden, null, { timeout: 15000 });
  await page.waitForFunction(() => document.querySelector('#connection-banner').hidden, null, { timeout: 30000 });
  await send('AFTER_BRIDGE_LOSS');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].filter(node => node.textContent.includes('RECOVERY_OK')).length >= 4, null, { timeout: 60000 });
  assert.equal(await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id'), sessionId);
  const loss = await ui();
  assert.equal(loss.assistant.some(text => text.includes('reopen Desktop')), false);
  evidence.bridge_loss_recovered_automatically = true;
  evidence.runtime_pid = runtime.pid;
  await page.screenshot({ path: path.join(output, 'recovery.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'recovery-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'recovery-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'recovery-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
