// R5 command convergence: the commands Desktop now owns really reach the
// Runtime — side question, recap, diagnostics, checkpoint restore, crash-window
// acknowledgement, turn jump, trust entry and the permission-contract
// migration — and the commands that stay terminal-only say so.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-r5-commands-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });

const requests = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const messages = body.messages ?? [];
  const userText = messages
    .filter(message => message.role === 'user')
    .map(message => typeof message.content === 'string' ? message.content
      : Array.isArray(message.content) ? message.content.filter(part => part.type === 'text').map(part => part.text ?? '').join('') : '')
    .join('\n');
  requests.push(userText.slice(-160));
  const answer = userText.includes('BTW_Q') ? 'BTW_ANSWER_OK' : 'TURN_OK';
  // The side question uses the non-streaming shape; a turn streams.
  if (!body.stream) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: 'r5-commands', object: 'chat.completion', created: 1, model: 'fixture', choices: [{ index: 0, message: { role: 'assistant', content: answer }, finish_reason: 'stop' }] }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = (delta, finish) => `data: ${JSON.stringify({ id: 'r5-commands', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  response.write(frame({ content: answer }, null));
  response.write(frame({}, 'stop'));
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'real Electron/Runtime commands; isolated home/workspace; test-only SSE provider' };
const errors = [];
let app;
let page;
const submit = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); };
const waitAnswer = value => page.waitForFunction(text => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text)), value, { timeout: 90000 });
const settle = () => page.waitForFunction(async () => {
  const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
  if (!id) return false;
  try { const value = await window.desktop.snapshot(id); return !['running', 'waiting_user'].includes(value.task_status ?? value.status); } catch { return false; }
}, null, { timeout: 90000 });
const openCatalog = async () => {
  await page.locator('#attachment-add').click();
  await page.waitForSelector('#command-popover');
  const entries = await page.evaluate(() => [...document.querySelectorAll('#command-popover .menu-item')].map(item => ({
    id: item.dataset.commandId,
    disabled: item.disabled,
    badge: item.querySelector('.badge-unconnected')?.textContent ?? null,
  })));
  await page.keyboard.press('Escape');
  return entries;
};

try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await submit('SEED_COMMANDS');
  await waitAnswer('TURN_OK');
  await settle();
  await submit('SECOND_TURN');
  await waitAnswer('TURN_OK');
  await settle();

  // The catalog states each command's real status.
  const catalog = await openCatalog();
  const byId = Object.fromEntries(catalog.map(entry => [entry.id, entry]));
  for (const id of ['btw', 'recap', 'restore', 'recover', 'doctor', 'jump', 'trust', 'permission-contract']) {
    assert.equal(byId[id]?.disabled, false, `${id} must be usable`);
    assert.equal(byId[id]?.badge, null, `${id} must not claim an unavailable badge`);
  }
  for (const [id, badge] of [['clean', '终端专属'], ['update', '终端专属'], ['exit', '等效入口'], ['memory', '未接入'], ['resume-claude', '未接入']]) {
    assert.equal(byId[id]?.disabled, true, `${id} stays unavailable`);
    assert.equal(byId[id]?.badge, badge, `${id} names its real status`);
  }
  evidence.catalog = Object.fromEntries(catalog.filter(entry => entry.badge || ['btw', 'recap', 'restore', 'recover', 'doctor', 'jump', 'trust', 'permission-contract'].includes(entry.id)).map(entry => [entry.id, entry.badge]));

  // /btw runs beside the main transcript and never becomes a user message.
  await submit('/btw BTW_Q 为什么用队列');
  await page.waitForFunction(() => !document.querySelector('#btw-panel').hidden, null, { timeout: 30000 });
  await page.waitForFunction(() => document.querySelector('#btw-answer').textContent.includes('BTW_ANSWER_OK'), null, { timeout: 60000 });
  assert.equal(await page.locator('.message.user').filter({ hasText: 'BTW_Q' }).count(), 0, 'a side question is not a transcript row');
  await page.screenshot({ path: path.join(output, 'r5-command-btw.png') });
  await page.locator('#btw-close').click();
  await page.waitForFunction(() => document.querySelector('#btw-panel').hidden);
  evidence.btw = true;

  // A chat session has no Goal to recap: the runtime says so.
  await submit('/recap');
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('没有可回顾的 Goal'), null, { timeout: 30000 });
  evidence.recap_notice = await page.locator('.toast-text').innerText();

  // Diagnostics are the runtime's structured findings.
  await submit('/doctor');
  await page.waitForSelector('dialog.diagnostics-dialog[open]', { timeout: 30000 });
  await page.waitForFunction(() => document.querySelectorAll('.diagnostic-row').length > 0, null, { timeout: 30000 });
  evidence.diagnostics = { rows: await page.locator('.diagnostic-row').count() };
  await page.screenshot({ path: path.join(output, 'r5-command-doctor.png') });
  await page.locator('.diagnostics-dialog .modal-actions button', { hasText: '关闭' }).click();

  // A healthy session has nothing to acknowledge; the command says so.
  await submit('/recover');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent.includes('没有需要确认的中断调用'), null, { timeout: 30000 });
  evidence.recover_refusal = await page.locator('#composer-error').innerText();

  // The turn rail is addressable by number; no new surface is created.
  await submit('/jump 1');
  await page.waitForFunction(() => !document.querySelector('#composer-error') || document.querySelector('#composer-error').hidden, null, { timeout: 10000 });
  evidence.jump = true;

  // Trust stays on the existing Skills page instead of a duplicate surface.
  await submit('/trust');
  await page.waitForSelector('#settings-dialog[open]', { timeout: 30000 });
  evidence.trust_opens_settings = true;
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));

  // The V2 contract adoption is explicit and confirmed.
  await submit('/permission-contract v2');
  await page.waitForSelector('dialog.confirm-modal[open]', { timeout: 30000 });
  await page.locator('dialog.confirm-modal .modal-actions button', { hasText: '迁移到 V2' }).click();
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('已迁移到权限契约 V2'), null, { timeout: 30000 });
  evidence.permission_contract = true;

  // Restore really truncates: choose the oldest checkpoint and confirm.
  await submit('/restore');
  await page.waitForSelector('dialog.checkpoint-dialog[open]', { timeout: 30000 });
  const rows = page.locator('.checkpoint-dialog .checkpoint-row');
  const checkpointCount = await rows.count();
  assert.ok(checkpointCount > 0, 'a turn leaves an in-memory checkpoint');
  const beforeMessages = await page.locator('.message.user').count();
  await rows.last().locator('button').click();
  await page.waitForSelector('dialog.confirm-modal[open]', { timeout: 30000 });
  await page.locator('dialog.confirm-modal .modal-actions button', { hasText: '恢复' }).click();
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('已请求恢复检查点'), null, { timeout: 30000 });
  await page.waitForFunction(count => document.querySelectorAll('.message.user').length < count, beforeMessages, { timeout: 30000 });
  evidence.restore = { checkpoints: checkpointCount, before_messages: beforeMessages, after_messages: await page.locator('.message.user').count() };
  await page.screenshot({ path: path.join(output, 'r5-command-restore.png') });

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r5-commands-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, requests: requests.slice(-8) }, null, 2));
  process.exitCode = 1;
} finally {
  try {
    await Promise.race([
      app?.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
