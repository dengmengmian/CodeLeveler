// R1b: credential custody through the real Desktop, Runtime and provider.
// The fixture provider records the exact model-visible request bodies; the
// assertions compare them against localStorage and the runtime transcript.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-secret-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home); await mkdir(workspace); await mkdir(output, { recursive: true });

const CAPTURE_KEY = 'sk-abcdefghijklmnop1234';
const PLAINTEXT_KEY = 'sk-zyxwvutsrqponmlk9876';
const PASTE_KEY = 'sk-pastepastepaste1234';
const requests = [];
const providerErrors = [];
const server = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    if (!(body.tools ?? []).length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    requests.push(body);
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const [delta, finish] of [[{ content: 'SECRET_OK' }, null], [{}, 'stop']]) {
      response.write(`data: ${JSON.stringify({ id: 'secret', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`);
    }
    response.end('data: [DONE]\n\n');
  } catch (error) {
    providerErrors.push(error.message);
    response.writeHead(500);
    response.end('fixture failed');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "http://127.0.0.1:${server.address().port}"\napi_key = "test-only"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const profile = path.join(temporary, 'profile');
const evidence = { scope: 'real Electron + Runtime + provider; test-only SSE provider and native-folder seam; isolated HOME' };
let app;
let page;
const errors = [];
const DRAFTS_KEY = 'codeleveler.drafts.v1';
const requestText = () => requests.map(body => (body.messages ?? []).map(message => typeof message.content === 'string' ? message.content : JSON.stringify(message.content ?? '')).join('\n')).join('\n');
// Evidence must never carry a credential, even from an assertion's actual
// value or a transport error.
const redact = text => String(text ?? '')
  .replaceAll(CAPTURE_KEY, '[redacted]')
  .replaceAll(PLAINTEXT_KEY, '[redacted]')
  .replaceAll(PASTE_KEY, '[redacted]');

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
};
const waitConnected = async () => {
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await page.evaluate(() => window.desktop.scanSecrets('probe text'));
};
const storedDrafts = () => page.evaluate(key => localStorage.getItem(key) ?? '', DRAFTS_KEY);
const send = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); };
const okCount = () => page.evaluate(() => [...document.querySelectorAll('.message.assistant .markdown')].filter(node => node.textContent.includes('SECRET_OK')).length);
const waitOk = async (previous, timeout = 60000) => {
  // The durable session status is the authority; the header text can lag or be
  // empty while a takeover owns the composer.
  await page.waitForFunction(async previous => {
    const replies = [...document.querySelectorAll('.message.assistant .markdown')]
      .filter(node => node.textContent.includes('SECRET_OK')).length;
    if (!(replies > previous)) return false;
    const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
    if (!id) return false;
    const snapshot = await window.desktop.snapshot(id);
    return !['running', 'waiting_user'].includes(snapshot.task_status);
  }, previous, { timeout });
};

try {
  await launch();
  // 0. Draft migration: an old plaintext draft must not survive the upgrade;
  // a clean draft must. Storage is cleared first and only rescanned-safe text
  // is written back.
  await page.evaluate(key => localStorage.setItem(key, JSON.stringify({
    safe: 'hello saved draft',
    unsafe: `API_KEY=${'sk-abcdefghijklmnop1234'}`,
  })), DRAFTS_KEY);
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await waitConnected();
  await page.waitForFunction(key => {
    const stored = JSON.parse(localStorage.getItem(key) || '{}');
    return stored.safe === 'hello saved draft' && !Object.hasOwn(stored, 'unsafe');
  }, DRAFTS_KEY, { timeout: 30000 });
  assert.doesNotMatch(await storedDrafts(), new RegExp(CAPTURE_KEY));
  evidence.legacy_draft_sanitized = { stored: await storedDrafts() };

  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));

  // 1. Capture: the draft is never persisted; the review appears; the model
  // receives a reference, never the value.
  await page.locator('#message').fill(`请把 API_KEY=${CAPTURE_KEY} 写入 .env`);
  await page.waitForTimeout(700);
  assert.doesNotMatch(await storedDrafts(), new RegExp(CAPTURE_KEY), 'the raw candidate must not reach localStorage');
  await page.locator('#send').click();
  await page.locator('.secret-review-modal').waitFor();
  const review = page.locator('.secret-review-modal');
  const reviewText = await review.innerText();
  assert.match(reviewText, /检测到疑似凭据/);
  assert.match(reviewText, /安全接管/);
  assert.match(reviewText, /明文发送/);
  assert.doesNotMatch(reviewText, new RegExp(CAPTURE_KEY), 'the review must show a mask, not the value');
  evidence.review_mask_only = true;
  const beforeCapture = await okCount();
  await review.getByRole('button', { name: '安全接管并发送', exact: true }).click();
  await waitOk(beforeCapture);
  // The snapshot terminal can arrive before the transcript paints the user
  // row; the reference must be visible before it is read.
  await page.waitForFunction(() => document.querySelector('#conversation')?.innerText.includes('[secret:sec_'), null, { timeout: 30000 });
  const captureRequest = requestText();
  assert.doesNotMatch(captureRequest, new RegExp(CAPTURE_KEY), 'the capture path must not send the value to the provider');
  assert.match(captureRequest, /\[secret:sec_[0-9a-f]{32}\]/);
  assert.match(await page.locator('#conversation').innerText(), /\[secret:sec_[0-9a-f]{32}\]/);
  assert.doesNotMatch(await storedDrafts(), new RegExp(CAPTURE_KEY));
  evidence.capture_reference_only = true;

  // 2. Plaintext: the explicit acknowledgement is the only path that may
  // show the bytes to the model/provider; the UI said so before sending.
  await page.locator('#message').fill(`明文发送 API_KEY=${PLAINTEXT_KEY}`);
  await page.locator('#send').click();
  await page.locator('.secret-review-modal').waitFor();
  assert.match(await page.locator('.secret-review-modal').innerText(), /模型与 Provider 会看到/);
  const beforePlaintext = await okCount();
  await page.locator('.secret-review-modal').getByRole('button', { name: '明文发送（模型与 Provider 可见）', exact: true }).click();
  await waitOk(beforePlaintext);
  const plaintextRequest = requestText();
  assert.match(plaintextRequest, new RegExp(PLAINTEXT_KEY), 'the acknowledged plaintext is the documented exception');
  assert.doesNotMatch(await storedDrafts(), new RegExp(PLAINTEXT_KEY));
  evidence.plaintext_acknowledged = true;

  // 3. Secure paste: a value captured without a message becomes a reference
  // in the composer; sending that reference never exposes the value.
  await page.locator('#attachment-add').click();
  await page.getByRole('option', { name: '安全粘贴凭据', exact: true }).click();
  await page.locator('.secret-paste-modal').waitFor();
  await page.locator('.secret-paste-input').fill(PASTE_KEY);
  await page.locator('.secret-paste-modal').getByRole('button', { name: '接管并插入', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('#message').value.includes('[secret:sec_'), null, { timeout: 30000 });
  assert.doesNotMatch(await page.evaluate(() => document.querySelector('#message').value), new RegExp(PASTE_KEY));
  const beforePaste = await okCount();
  await page.locator('#send').click();
  await waitOk(beforePaste);
  assert.doesNotMatch(requestText(), new RegExp(PASTE_KEY));
  assert.match(requestText(), /\[secret:sec_[0-9a-f]{32}\]/);
  assert.doesNotMatch(await storedDrafts(), new RegExp(PASTE_KEY));
  evidence.secure_paste_reference_only = true;

  // 4. Revoke: the runtime's own notification is the outcome; the client only
  // reports that the request was sent.
  const revoke = page.locator('.secret-revoke').last();
  await revoke.waitFor();
  await revoke.click();
  await page.getByRole('button', { name: '撤销', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.toast')?.textContent.includes('已撤销'), null, { timeout: 30000 });
  assert.match(await page.locator('.toast').innerText(), /已撤销/);
  evidence.revoke_reported_by_runtime = true;

  // 5. The IPC boundary refuses malformed secret commands before the runtime.
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  const malformed = await page.evaluate(async ({ sessionId }) => {
    const envelope = command => ({ command_id: crypto.randomUUID(), session_id: sessionId, expected_version: null, issued_at: new Date().toISOString(), command });
    const results = [];
    for (const command of [
      { type: 'submit_user_input', session_id: sessionId, content: 'use {{s}}', attachments: [], secret_captures: [{ placeholder: '{{s}}', value: 'sk-x' }], plaintext_secret_ack: true },
      { type: 'revoke_secret', session_id: sessionId, secret_ref: 'sec_short' },
    ]) {
      try { await window.desktop.deliver(envelope(command)); results.push(null); }
      catch (error) { results.push(String(error?.message ?? error)); }
    }
    return results;
  }, { sessionId });
  assert.ok(malformed.every(message => /Invalid secret/.test(String(message))), `the IPC boundary must refuse malformed secret commands: ${JSON.stringify(malformed)}`);
  evidence.ipc_rejects_malformed_secret_commands = malformed;

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  Object.assign(evidence, { provider_requests: requests.length, page_errors: errors, provider_errors: providerErrors, passed: true });
  await writeFile(path.join(output, 'secret-custody-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: redact(error.message), page_errors: errors.map(redact), provider_errors: providerErrors.map(redact) });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'secret-custody-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'secret-custody-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
