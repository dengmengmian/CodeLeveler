// R6 memory acceptance: the four cross-session memory commands reach the
// Runtime from the Desktop and every result is the Runtime's own.
//
// Real Electron + bridge + runtime on an isolated HOME; a test-only SSE
// provider. `/remember` persists a uniquely tagged note, `/memory` lists and
// previews it, `/flush` relays both the configured-disabled outcome and a real
// capture drain, and `/dream` reports the store's real eligibility. The note is
// forgotten through the page and the file is verified gone; a restart proves
// the persisted state the UI showed.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile, readdir, rm } from 'node:fs/promises';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

const root = fileURLToPath(new URL('..', import.meta.url));
// Not a system temp directory: a temp workspace has no workspace memory scope
// (by contract), and a path inside the CodeLeveler repository would borrow its
// git identity. These projects are plain directories under the real HOME and
// are removed at the end of this isolated run.
const temporary = await mkdtemp(path.join(os.homedir(), '.leveler-r6-memory-'));
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const secondWorkspace = path.join(temporary, 'workspace-b');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(secondWorkspace, { recursive: true });
await mkdir(output, { recursive: true });

const TOKEN = `R6_MEMORY_${Date.now()}`;
const NOTE = `R6 记忆验收 ${TOKEN}\n\n用户偏好：回答保持简洁；<img src=x onerror=alert(1)> 是文本，不是 HTML。`;
const configPath = path.join(home, 'config.toml');
const memoryConfig = captureEnabled => `default_model="fixture/m"
[providers.fixture]
protocol="openai_chat"
base_url="http://127.0.0.1:${server.address().port}"
api_key="test-only"
[models.m]
provider="fixture"
model_id="m"
context_window=131072
[memory]
rollout="active"
capture_enabled=${captureEnabled ? 'true' : 'false'}
automatic_dream_enabled=false
manual_dream_enabled=true
batch_dream_enabled=true
`;

const requests = { turns: 0, captures: 0, system: '' };
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const tools = (body.tools ?? []).map(tool => tool.function?.name);
  // Capture extraction must answer with the forced tool call. A noop keeps
  // this acceptance model-free beyond the call itself: no observations means
  // no Dream work and no file churn.
  if (tools.includes('record_memory_observations')) {
    requests.captures += 1;
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({
      id: 'r6-capture', object: 'chat.completion', created: 1, model: 'm',
      choices: [{
        index: 0,
        message: {
          role: 'assistant', content: null,
          tool_calls: [{
            id: 'call_capture', type: 'function',
            function: { name: 'record_memory_observations', arguments: JSON.stringify({ outcome: 'noop', observations: [] }) },
          }],
        },
        finish_reason: 'tool_calls',
      }],
      usage: { prompt_tokens: 8, completion_tokens: 4 },
    }));
    return;
  }
  requests.turns += 1;
  const lastUser = [...(body.messages ?? [])].reverse().find(message => message.role === 'user');
  const userText = typeof lastUser?.content === 'string' ? lastUser.content : '';
  // The runtime's own prompt payload is the evidence that the memory index
  // reaches the model; no model claim is involved.
  const system = (body.messages ?? []).find(message => message.role === 'system');
  requests.system = typeof system?.content === 'string' ? system.content : JSON.stringify(system?.content ?? '');
  // One slow turn so the test can observe the IdleOnly policy while a turn
  // is genuinely running.
  if (userText.includes('SLOW_TURN')) await new Promise(resolve => setTimeout(resolve, 3000));
  const answer = userText.includes('SLOW_TURN') ? 'SLOW_OK' : 'MEMORY_TURN_OK';
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = (delta, finish) => `data: ${JSON.stringify({ id: 'r6-memory', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  response.write(frame({ content: answer }, null));
  response.write(frame({}, 'stop'));
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(configPath, memoryConfig(false), { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'real Electron/Runtime memory commands; isolated HOME/workspaces; test-only SSE provider', token: TOKEN, checks: [] };
const errors = [];
let app;
let page;
const check = (name, ok, detail = '') => {
  evidence.checks.push({ name, ok: !!ok, detail });
  assert.ok(ok, `${name}${detail ? ` — ${detail}` : ''}`);
};
const submit = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); };
const waitAnswer = () => page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('MEMORY_TURN_OK')), null, { timeout: 90000 });
const settle = () => page.waitForFunction(async () => {
  const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
  if (!id) return false;
  try { const value = await window.desktop.snapshot(id); return !['running', 'waiting_user'].includes(value.task_status ?? value.status); } catch { return false; }
}, null, { timeout: 90000 });
const toast = async text => {
  await page.waitForFunction(value => document.querySelector('.toast-text')?.textContent.includes(value), text, { timeout: 30000 });
  evidence.last_toast = await page.locator('.toast-text').innerText();
};
const composerError = async text => page.waitForFunction(value => document.querySelector('#composer-error')?.textContent.includes(value) && !document.querySelector('#composer-error').hidden, text, { timeout: 30000 });
const composerValue = () => page.locator('#message').inputValue();
const globalInbox = path.join(home, 'memory', 'global', 'observations', '_inbox');
const inboxFiles = async () => {
  try { return (await readdir(globalInbox)).filter(name => name.endsWith('.md')).sort(); } catch { return []; }
};
const sessionRows = () => page.locator('.session-row');
const restart = async () => {
  const exited = new Promise(resolve => app.process().once('exit', resolve));
  await page.evaluate(() => window.close());
  await exited;
  app = null;
  await launch();
};
async function launch() {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '15' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
}
async function openWorkspace(folder) {
  await app.evaluate(({ dialog }, target) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [target] }); }, folder);
  await page.locator('#add-workspace').click();
  // The picker's tooltip is the selected directory's exact path; the visible
  // label is only its basename.
  await page.waitForFunction(target => document.querySelector('#workspace-picker')?.dataset.tooltip === target, folder, { timeout: 30000 });
}
async function openMemory() {
  await submit('/memory');
  await page.waitForSelector('dialog.memory-dialog[open]', { timeout: 30000 });
  await page.waitForFunction(() => document.querySelector('.memory-dialog .memory-rollout')?.textContent.includes('rollout: active'), null, { timeout: 30000 });
}
async function selectGlobalNote() {
  // Select by the token the preview must contain, never by a guessed file name.
  const rows = page.locator('.memory-dialog .memory-file');
  const count = await rows.count();
  for (let index = 0; index < count; index += 1) {
    const row = rows.nth(index);
    const listed = await row.locator('.memory-file-path').innerText();
    // The generated manifest also mentions the note (as a pending-observation
    // path and its first line), so only the inbox observation itself is the
    // selectable note.
    if (!listed.startsWith('observations/_inbox/')) continue;
    if ((await row.getAttribute('aria-selected')) !== 'true') await row.click();
    // The head names the selection before the read lands; wait for the
    // preview's bytes, not just for the label.
    await page.waitForFunction(path => (document.querySelector('.memory-preview-head')?.textContent ?? '').includes(path)
      && !!document.querySelector('.memory-dialog .memory-content'), listed, { timeout: 15000 });
    const content = page.locator('.memory-dialog .memory-content');
    if ((await content.innerText()).includes(TOKEN)) return row;
  }
  assert.fail('the remembered note is listed and previews its token');
}

try {
  await launch();
  await openWorkspace(workspace);
  await submit('MEMORY_SEED');
  await waitAnswer();
  await settle();
  const firstSession = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  evidence.session = firstSession;

  // Discovery: a bare `/` lists the four memory commands as usable, with no
  // "unconnected" claim.
  await page.locator('#attachment-add').click();
  await page.waitForSelector('#command-popover');
  const catalog = await page.evaluate(() => [...document.querySelectorAll('#command-popover .menu-item')].map(item => ({
    id: item.dataset.commandId,
    disabled: item.disabled,
    badge: item.querySelector('.badge-unconnected')?.textContent ?? null,
  })));
  await page.keyboard.press('Escape');
  const byId = Object.fromEntries(catalog.map(item => [item.id, item]));
  for (const id of ['memory', 'remember', 'dream', 'flush']) {
    assert.ok(byId[id], `${id} is listed under a bare slash`);
    assert.equal(byId[id].disabled, false, `${id} is usable`);
    assert.equal(byId[id].badge, null, `${id} claims no unavailable badge`);
  }
  evidence.catalog = Object.fromEntries(['memory', 'remember', 'dream', 'flush'].map(id => [id, byId[id]]));

  // `/remember` writes exactly the note; the file on disk is the evidence that
  // the UI text was not the only thing that happened.
  const beforeFiles = await inboxFiles();
  await submit(`/remember ${NOTE}`);
  await toast('记忆写入完成');
  const afterFiles = await inboxFiles();
  assert.equal(afterFiles.length, beforeFiles.length + 1, 'one global observation was written');
  const noteFile = path.join(globalInbox, afterFiles.find(name => !beforeFiles.includes(name)));
  const noteBytes = await readFile(noteFile, 'utf8');
  assert.ok(noteBytes.includes(TOKEN), 'the note file carries the unique token');
  assert.ok(noteBytes.includes('用户偏好'), 'the note file keeps the full text');
  assert.ok(noteBytes.includes('<img src=x onerror=alert(1)>'), 'the note stores exactly the text the user typed');
  evidence.remember = { file: path.basename(noteFile), bytes: noteBytes.length };

  // `/memory` projects the runtime's listing and previews the same bytes.
  await openMemory();
  await selectGlobalNote();
  const previewText = await page.locator('.memory-dialog .memory-content').innerText();
  check('memory preview contains the remembered note', previewText.includes(TOKEN));
  assert.equal(await page.locator('.memory-dialog .memory-content img').count(), 0, 'untrusted memory HTML stays text');
  check('memory page states the configured rollout', true, await page.locator('.memory-rollout').innerText());
  await page.screenshot({ path: path.join(output, 'r6-memory-page.png') });

  // A narrow window must still show both panes without a horizontal overflow.
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(760, 700));
  await new Promise(resolve => setTimeout(resolve, 600));
  check('the memory page fits a narrow window', await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth + 1), await page.evaluate(() => `${document.documentElement.scrollWidth}/${window.innerWidth}`));
  await page.screenshot({ path: path.join(output, 'r6-memory-narrow.png') });
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1280, 800));
  await new Promise(resolve => setTimeout(resolve, 400));

  // Pause / resume is the runtime's session-scoped switch, never a config edit.
  await page.locator('.memory-dialog').getByRole('button', { name: '暂停本会话记忆' }).click();
  await toast('暂停记忆完成');
  await page.waitForFunction(() => (document.querySelector('.memory-state')?.textContent ?? '').includes('本会话已暂停'), null, { timeout: 15000 });
  await page.locator('.memory-dialog').getByRole('button', { name: '恢复本会话记忆' }).click();
  await toast('恢复记忆完成');
  await page.waitForFunction(() => (document.querySelector('.memory-state')?.textContent ?? '').includes('本会话已开启'), null, { timeout: 15000 });
  check('the session pause switch round-trips through the runtime', true);
  evidence.memory = { rollout: await page.locator('.memory-rollout').innerText(), scopes: await page.locator('.memory-scope').count() };

  // `/flush` while capture is configured off: the runtime's own refusal.
  await page.keyboard.press('Escape');
  await submit('/flush');
  await toast('记忆捕获刷新失败');
  check('flush relays the configured-off refusal', (await page.locator('.toast-text').innerText()).includes('Memory capture is disabled'));

  // `/dream` with no pending observations: a real, settled no-work outcome.
  await submit('/dream');
  await toast('记忆整理完成');
  check('dream reports the store has no work', (await page.locator('.toast-text').innerText()).includes('no pending observations'));
  await submit('/dream batch');
  await toast('批量记忆整理完成');
  check('batch dream reports no pending batch work', (await page.locator('.toast-text').innerText()).includes('no pending observations to batch'));

  // While a turn runs, the two maintenance commands are refused locally
  // (IdleOnly); the page/read commands stay available.
  await page.locator('#message').fill('SLOW_TURN');
  await page.locator('#send').click();
  await page.locator('#stop').waitFor({ state: 'visible', timeout: 15000 });
  await submit('/flush');
  await composerError('回合运行中不能刷新记忆索引');
  await submit('/dream');
  await composerError('回合运行中不能运行记忆整理');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('SLOW_OK')), null, { timeout: 30000 });
  await settle();
  check('maintenance commands stay IdleOnly while a turn runs', true);

  // The argument vocabulary is local; the runtime never receives prose.
  await submit('/dream fast');
  await composerError('用法：/dream 或 /dream batch');
  await page.locator('#message').fill('/remember');
  await page.locator('#send').click();
  check('a bare /remember primes the composer instead of sending', (await composerValue()) === '/remember ');
  await page.locator('#message').fill('');

  // The next turn's real prompt payload carries the memory index; the agent
  // contract is "the model decides whether to read it", not a forced behavior.
  await page.keyboard.press('Escape');
  await submit('MEMORY_PROMPT_PROBE');
  await waitAnswer();
  await settle();
  check('the next turn prompt carries the memory index', requests.system.includes('Cross-session memory') || requests.system.includes('跨会话记忆'), requests.system.slice(-800));
  check('the index names the remembered note', requests.system.includes(TOKEN));

  // Restart: the persisted note is still the listing and the preview.
  await restart();
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${firstSession}"]`).waitFor({ state: 'visible', timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${firstSession}"]`).click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'),
    null, { timeout: 30000 });
  await openMemory();
  await selectGlobalNote();
  check('the note survives a Desktop restart', (await page.locator('.memory-dialog .memory-content').innerText()).includes(TOKEN));

  // Cross-session: a second session in the same workspace sees the same global
  // note; the workspace scope is the same root.
  const globalRoot = await page.locator('.memory-scope').first().locator('.memory-scope-root').innerText();
  const workspaceRootOne = await page.locator('.memory-scope').nth(1).locator('.memory-scope-root').innerText();
  await page.keyboard.press('Escape');
  await page.locator('#new-session').click();
  // `/new-session` creates a no-workspace session and selects it; wait for that
  // action to settle, then select workspace A for a same-workspace session.
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') !== null, null, { timeout: 60000 });
  await openWorkspace(workspace);
  await submit('MEMORY_SESSION_TWO');
  await waitAnswer();
  await settle();
  const secondSession = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  assert.notEqual(secondSession, firstSession);
  await openMemory();
  await selectGlobalNote();
  const workspaceRootTwo = await page.locator('.memory-scope').nth(1).locator('.memory-scope-root').innerText();
  assert.equal(workspaceRootTwo, workspaceRootOne, 'both sessions share one workspace memory scope');
  check('a second session sees the same global note', (await page.locator('.memory-dialog .memory-content').innerText()).includes(TOKEN));

  // Cross-project: another workspace keeps the global scope but has its own
  // workspace root, and the note is not duplicated into it.
  await page.keyboard.press('Escape');
  await openWorkspace(secondWorkspace);
  await submit('MEMORY_OTHER_PROJECT');
  await waitAnswer();
  await settle();
  await openMemory();
  const workspaceRootOther = await page.locator('.memory-scope').nth(1).locator('.memory-scope-root').innerText();
  assert.notEqual(workspaceRootOther, workspaceRootTwo, 'a different workspace is a different memory scope');
  await selectGlobalNote();
  const otherProjectText = await page.locator('.memory-dialog .memory-content').innerText();
  check('another project still sees only the global note', otherProjectText.includes(TOKEN));
  evidence.scopes = { global: globalRoot, workspace_a: workspaceRootTwo, workspace_b: workspaceRootOther };

  // Forget is explicit, bound to the previewed bytes, and the file is gone.
  await page.locator('.memory-dialog').getByRole('button', { name: '遗忘此文件' }).click();
  await page.waitForSelector('dialog.confirm-modal[open]', { timeout: 30000 });
  await page.locator('dialog.confirm-modal .modal-actions button', { hasText: '遗忘' }).click();
  await toast('遗忘完成');
  const remaining = await inboxFiles();
  check('the forgotten file is removed from the store', !remaining.includes(path.basename(noteFile)));
  await page.waitForFunction(name => ![...document.querySelectorAll('.memory-dialog .memory-file-path')].some(node => node.textContent.endsWith(name)), path.basename(noteFile), { timeout: 30000 });
  check('the page no longer lists the forgotten note', true);
  await page.screenshot({ path: path.join(output, 'r6-memory-forgotten.png') });

  // Real capture drain: enable capture, reload the live config, and `/flush`
  // calls the extraction route once, then reports the store's own outcome.
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('dialog.memory-dialog[open]'), null, { timeout: 15000 });
  await writeFile(configPath, memoryConfig(true), { mode: 0o600 });
  await submit('/reload');
  // Record whatever the reload surface said; the live memory summary below is
  // the authority that the new configuration reached the runtime.
  await page.waitForFunction(() => !!document.querySelector('.toast-text'), null, { timeout: 10000 }).catch(() => {});
  evidence.reload_toast = await page.locator('.toast-text').innerText().catch(() => null);
  // The live config is the runtime's fact: the memory page reports capture as
  // enabled only after the reload applied.
  await openMemory();
  await page.waitForFunction(() => (document.querySelector('.memory-summary')?.textContent ?? '').includes('捕获 开'), null, { timeout: 30000 }).catch(() => {});
  const summary = await page.locator('.memory-summary').innerText();
  check('the reload applied the live memory configuration', summary.includes('捕获 开'), summary);
  await page.keyboard.press('Escape');
  // A turn that settles while capture is enabled is the real capture input;
  // the automatic worker performs the extraction call.
  const captureCallsBefore = requests.captures;
  await submit('MEMORY_CAPTURE_TURN');
  await waitAnswer();
  await settle();
  const deadline = Date.now() + 60000;
  while (requests.captures === captureCallsBefore && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 300));
  }
  check('the settled turn reached the capture model', requests.captures > captureCallsBefore, `captures ${requests.captures}`);
  await openMemory();
  await page.waitForFunction(() => document.querySelector('.memory-summary') && [...document.querySelectorAll('.memory-counters')].some(node => /已完成 [1-9]/.test(node.textContent)), null, { timeout: 30000 });
  check('the page projects the completed capture job', true);
  await page.screenshot({ path: path.join(output, 'r6-memory-capture.png') });
  // `/flush` on the now-drained queue is the runtime's own settled outcome.
  await page.keyboard.press('Escape');
  await submit('/flush');
  await toast('记忆捕获刷新完成');
  check('flush drains the real capture queue', (await page.locator('.toast-text').innerText()).includes('Capture queue drained'));

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r6-memory-failure.png') }).catch(() => {});
  const visible = await page?.evaluate(() => ({
    toast: document.querySelector('.toast-text')?.textContent ?? null,
    composerError: !document.querySelector('#composer-error')?.hidden ? document.querySelector('#composer-error')?.textContent : null,
    errorBanner: !document.querySelector('#error-banner')?.hidden ? document.querySelector('#error-banner')?.textContent : null,
    memoryDialogOpen: !!document.querySelector('dialog.memory-dialog[open]'),
    memorySummary: document.querySelector('.memory-summary')?.textContent ?? null,
  })).catch(() => null);
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, requests, visible }, null, 2));
  process.exitCode = 1;
} finally {
  try {
    await Promise.race([
      app?.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
  // This run owns the daemons its bridge spawned; stop them before the
  // isolated home disappears so they cannot recreate files under a deleted
  // path. No other runtime matches this run's unique directory.
  // A runtime host can fork its daemon while the first kill lands; two passes
  // with a settle window leave nothing behind that could recreate the home.
  spawnSync('pkill', ['-f', temporary]);
  await new Promise(resolve => setTimeout(resolve, 800));
  spawnSync('pkill', ['-f', temporary]);
  await new Promise(resolve => setTimeout(resolve, 500));
  // The isolated HOME is disposable; the real HOME is never touched here.
  if (!process.env.R6_KEEP_TEMP) await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
