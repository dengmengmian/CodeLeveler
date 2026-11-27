// R4 running-turn interaction acceptance: queue, steer, cancel, draft and
// scroll ownership. Real Electron/bridge/runtime; the provider streams slowly
// on purpose so the user can interact while a turn owns the session.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-running-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'a.txt'), 'alpha\n');
const imagePath = path.join(temporary, 'attach.png');
await writeFile(imagePath, Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64'));

const requests = [];
const providerErrors = [];
let cancelledTurns = 0;
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const chunk of request) raw += chunk;
  let body = {};
  try { body = JSON.parse(raw); } catch { /* probes */ }
  const messages = body.messages ?? [];
  const textOf = message => typeof message?.content === 'string' ? message.content : JSON.stringify(message?.content ?? '');
  const userTexts = messages.filter(message => message.role === 'user').map(textOf);
  requests.push({ userTexts, at: Date.now() });
  const tools = (body.tools ?? []).map(tool => tool.function?.name);
  if (!tools.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const chunk = (delta, finish) => JSON.stringify({ id: 'running', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] });
  const write = value => { try { response.write(value); return true; } catch { return false; } };
  const finished = value => { if (write(`data: ${chunk({ content: value }, null)}\n\n`)) { write(`data: ${chunk({}, 'stop')}\n\n`); write('data: [DONE]\n\n'); response.end(); } };
  const last = userTexts.at(-1) ?? '';
  if (last.includes('STEER_NOW')) { finished('STEER_SEEN'); return; }
  if (last.includes('QUEUED_NEXT')) { finished('QUEUED_DONE'); return; }
  if (last.includes('ISOLATED')) { finished('ISOLATED_DONE'); return; }
  if (last.includes('SEED')) { finished('SEED_OK'); return; }
  if (last.includes('RUN_SLOW')) {
    const marker = last.includes('CANCEL_ME') ? 'RUN_CANCEL' : 'RUN_SLOW_DONE';
    // Emit the first content with the role so even the opening frame carries
    // text; later frames are appended at a human-observable pace.
    write(`data: ${chunk({ role: 'assistant', content: `${marker}_0 ` }, null)}\n\n`);
    let tick = 0;
    const timer = setInterval(() => {
      if (response.destroyed || response.writableEnded) { clearInterval(timer); return; }
      tick += 1;
      if (!write(`data: ${chunk({ content: `${marker}_${tick} ` }, null)}\n\n`)) { clearInterval(timer); return; }
      if (tick >= 80) {
        clearInterval(timer);
        finished(marker);
      }
    }, 100);
    request.on('aborted', () => { cancelledTurns += 1; clearInterval(timer); });
    response.on('close', () => { if (!response.writableEnded) { cancelledTurns += 1; clearInterval(timer); } });
    return;
  }
  finished('PLAIN_OK');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });

const evidence = { scope: 'real Electron + bridge + runtime; slow local SSE provider' };
const errors = [];
let app;
let page;
const status = () => page.locator('#conversation-status').innerText();
const send = async value => { await page.locator('#message').fill(value); await page.locator('#send').click(); };
const chooseSetting = async (rowText, option) => {
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
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.evaluate(() => { globalThis.__runningEvents = []; window.desktop.onEvent(frame => globalThis.__runningEvents.push(frame)); });
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await send('SEED session');
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SEED_OK'));
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');

  // Queue (the default busy behaviour): Enter while running must not start a
  // second turn; the text waits in one visible dock and is delivered once.
  await send('RUN_SLOW queue case');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('RUN_SLOW_DONE_')));
  assert.equal(await page.evaluate(() => ['running', 'waiting_user'].includes(document.querySelector('#conversation-status').textContent)), false);
  // The slow turn is still running when the status first changes to running.
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await page.locator('#message').fill('QUEUED_NEXT');
  await page.locator('#message').press('Enter');
  await page.waitForSelector('.queue-dock');
  assert.match(await page.locator('.queue-dock').innerText(), /QUEUED_NEXT/);
  const queueDockWhileRunning = await page.locator('.queue-dock').count();
  assert.equal(queueDockWhileRunning, 1);
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('QUEUED_DONE')), null, { timeout: 60000 });
  const queuedTurns = requests.filter(item => item.userTexts.at(-1)?.includes('QUEUED_NEXT')).length;
  assert.equal(queuedTurns, 1, 'the queued message is delivered exactly once');
  assert.equal(await page.locator('.message.user').filter({ hasText: 'QUEUED_NEXT' }).count(), 1, 'the queued user message appears once');
  evidence.queue = { dock_while_running: 1, delivered: queuedTurns };

  // Steer: the setting changes the busy Enter into the Runtime's typed steer.
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
  await chooseSetting('繁忙时的发送行为', '插话发送');
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));
  const seedRequests = requests.length;
  await send('RUN_SLOW steer case');
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await page.locator('#message').fill('STEER_NOW 追加要求');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.body.textContent.includes('已插话'), null, { timeout: 10000 });
  assert.equal(await page.locator('.queue-dock').count(), 0, 'a steer is not a queued turn');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('STEER_SEEN')), null, { timeout: 60000 });
  const steerRequest = requests.slice(seedRequests).find(item => item.userTexts.some(text => text.includes('STEER_NOW')));
  assert.ok(steerRequest, 'the runtime must carry the steer into the next model call');
  evidence.steer = { typed_command: true, reached_model: true };

  // A steer carries text only: an attached image must be refused with a
  // visible reason, never silently dropped.
  await send('RUN_SLOW attachment steer case');
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await app.evaluate(({ dialog }, file) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [file] }); }, imagePath);
  await page.locator('#attachment-add').click();
  await page.getByRole('option', { name: '添加图片', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.attachment-chip[data-attachment-status="ready"]'));
  await page.locator('#message').fill('STEER_WITH_ATTACH');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent?.includes('插话不支持附件'));
  assert.equal(await page.locator('.attachment-chip').count(), 1, 'the refused attachment stays in the composer');
  assert.equal(await page.locator('.queue-dock').count(), 0);
  await page.locator('.attachment-chip .attachment-remove').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('RUN_SLOW_DONE')), null, { timeout: 60000 });
  evidence.steer_with_attachment = { refused: true, attachment_kept: true };

  // Cancel: the stop control ends the running turn, keeps the draft and the
  // session remains usable for the next real turn.
  await send('RUN_SLOW CANCEL_ME');
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await page.locator('#message').fill('DRAFT_AFTER_CANCEL');
  await page.locator('#stop').click();
  await page.waitForFunction(() => !['running', 'waiting_user'].includes((document.querySelector('#conversation-status')?.textContent ?? '')) || /取消|中断/.test(document.querySelector('#conversation-status')?.textContent ?? ''), null, { timeout: 30000 });
  await page.waitForFunction(() => ['已取消', '已中断'].some(label => document.querySelector('#conversation-status')?.textContent?.includes(label)), null, { timeout: 30000 });
  assert.equal(await page.locator('#message').inputValue(), 'DRAFT_AFTER_CANCEL');
  await page.locator('#message').fill('');
  await send('PLAIN_AFTER_CANCEL');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('PLAIN_OK')), null, { timeout: 30000 });
  evidence.cancel = { status: await status(), draft_kept: true, usable_after: true };

  // Multi-session isolation: a second session answers while the first still
  // runs; switching back shows the first session's own transcript.
  await send('RUN_SLOW isolation case');
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await page.locator('#new-session').click();
  await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId !== id, sessionId);
  await send('ISOLATED second session');
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('ISOLATED_DONE')), null, { timeout: 30000 });
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === id, sessionId);
  assert.equal(await page.locator('.message.assistant .markdown').filter({ hasText: 'ISOLATED_DONE' }).count(), 0);
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('RUN_SLOW_DONE')), null, { timeout: 60000 });
  evidence.multi_session = { second_answered: true, first_isolated: true };

  // Scroll ownership: while the reader is away from the bottom, new streamed
  // text must not yank the viewport down.
  await send('RUN_SLOW scroll case');
  await page.waitForFunction(() => document.querySelector('#stop') && !document.querySelector('#stop').hidden, null, { timeout: 10000 });
  await page.evaluate(() => { document.querySelector('#conversation-scroll').scrollTop = 0; });
  await page.waitForTimeout(400);
  const away = await page.evaluate(() => document.querySelector('#conversation-scroll').scrollTop);
  assert.ok(away < 120, `scrolled-away position moved: ${away}`);
  await page.evaluate(() => { const list = document.querySelector('#conversation-scroll'); list.scrollTop = list.scrollHeight; });
  await page.waitForTimeout(600);
  const followed = await page.evaluate(() => { const list = document.querySelector('#conversation-scroll'); return list.scrollHeight - list.scrollTop - list.clientHeight; });
  assert.ok(followed < 120, `near-bottom view did not follow: ${followed}`);
  evidence.scroll = { away, followed };
  await page.screenshot({ path: path.join(output, 'interaction-running.png') });

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'interaction-running-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, providerErrors, requests: requests.slice(-6) }, null, 2));
  process.exitCode = 1;
} finally {
  try { await app?.close(); } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
