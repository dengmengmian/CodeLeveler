// R1a: the real Desktop, Runtime and definition-trust gate. A test-only
// provider asks the registered `load_skill` tool for an untrusted package;
// every decision, denial, cancellation and reopen is exercised through the
// real GUI and the real persistent trust store.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-definition-trust-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home); await mkdir(workspace); await mkdir(output, { recursive: true });

const skills = {
  'trust-deny': 'TRUST_DENY_BODY_MARKER',
  'trust-allow': 'TRUST_ALLOW_BODY_MARKER',
  'trust-persist': 'TRUST_PERSIST_BODY_MARKER',
  'trust-cancel': 'TRUST_CANCEL_BODY_MARKER',
  'trust-reopen': 'TRUST_REOPEN_BODY_MARKER',
};
for (const [name, marker] of Object.entries(skills)) {
  const dir = path.join(workspace, '.leveler', 'skills', name);
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, 'SKILL.md'), `---\nname: ${name}\ndescription: R1 definition trust fixture ${name}\n---\n\n# ${name}\n\n${marker}\n`);
}

const providerErrors = [];
const seen = [];
const toolContents = [];
const phases = new Map();
const server = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const names = (body.tools ?? []).map(tool => tool.function?.name);
    if (!names.length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    assert.ok(names.includes('capability'), 'the provider schema must expose the capability control tool');
    const users = body.messages.filter(message => message.role === 'user');
    const scenario = String(users.at(-1)?.content ?? '').match(/TRUST_[A-Z_]+/)?.[0] ?? 'UNKNOWN';
    const toolResult = body.messages.some(message => message.role === 'tool');
    for (const message of body.messages) if (message.role === 'tool') toolContents.push(String(message.content ?? ''));
    const phase = phases.get(scenario) ?? 'new';
    seen.push({ scenario, phase, toolResult, exposesLoadSkill: names.includes('load_skill') });
    const skill = {
      TRUST_DENY: 'trust-deny',
      TRUST_ALLOW_ONCE: 'trust-allow',
      TRUST_PERSIST: 'trust-persist',
      TRUST_PERSIST_AGAIN: 'trust-persist',
      TRUST_CANCEL: 'trust-cancel',
      TRUST_REOPEN: 'trust-reopen',
    }[scenario];
    let delta;
    let finish = 'stop';
    if (names.includes('load_skill') && phase !== 'load-sent') {
      // The first request that exposes the pack is the one that may call it.
      phases.set(scenario, 'load-sent');
      delta = { tool_calls: [{ index: 0, id: `r1a-${scenario}`, type: 'function', function: { name: 'load_skill', arguments: JSON.stringify({ name: skill }) } }] };
      finish = 'tool_calls';
    } else if (phase === 'new' && !toolResult) {
      // Skills is not a default-active pack: enable it, then reload the tools.
      phases.set(scenario, 'capability-sent');
      delta = { tool_calls: [{ index: 0, id: `r1a-cap-${scenario}`, type: 'function', function: { name: 'capability', arguments: JSON.stringify({ action: 'enable', id: 'skills' }) } }] };
      finish = 'tool_calls';
    } else if (phase === 'capability-sent' && !names.includes('load_skill')) {
      delta = { content: `${scenario}_SKILLS_NOT_EXPOSED` };
    } else {
      phases.set(scenario, 'done');
      delta = { content: `${scenario}_DONE` };
    }
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const [payload, stop] of [[delta, null], [{}, finish]]) {
      response.write(`data: ${JSON.stringify({ id: 'trust', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: payload, finish_reason: stop }] })}\n\n`);
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
const evidence = { scope: 'real Electron + Runtime + definition-trust store; test-only SSE provider and native-folder seam' };
let app;
let page;
const errors = [];

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
const openWorkspace = async () => {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
// The composer is briefly disabled while background bookkeeping settles; a
// click in that window is a no-op by design. Wait for the real enabled state,
// then confirm the message was accepted (a session may be created first).
const send = async text => {
  await page.locator('#message').fill(text);
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await page.waitForFunction(() => {
      const button = document.querySelector('#send');
      return button && !button.disabled && !button.hidden;
    });
    const before = await page.locator('.message.user').count();
    await page.locator('#send').click();
    try {
      await page.waitForFunction(before => document.querySelectorAll('.message.user').length > before, before, { timeout: 10000 });
      return;
    } catch {
      const inFlight = await page.evaluate(() => {
        const button = document.querySelector('#send');
        const input = document.querySelector('#message');
        return Boolean(button && button.disabled && input && input.value !== '');
      });
      if (inFlight) {
        await page.waitForFunction(before => document.querySelectorAll('.message.user').length > before, before, { timeout: 30000 });
        return;
      }
    }
  }
  throw new Error(`message was not sent: ${text}`);
};
const waitDone = async (scenario, timeout = 60000) => {
  await page.waitForFunction(async text => {
    if (![...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text))) return false;
    const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
    if (!id) return false;
    const snapshot = await window.desktop.snapshot(id);
    return !['running', 'waiting_user'].includes(snapshot.task_status);
  }, `${scenario}_DONE`, { timeout });
};
const card = () => page.locator('.definition-trust-card');
const expand = async () => {
  const details = card().locator('details.definition-trust-content').first();
  if (!(await details.evaluate(node => node.open))) await details.locator('summary').click();
  await card().locator('pre').first().waitFor();
};
const decide = async label => {
  await card().getByRole('button', { name: label, exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
};

try {
  await launch();
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'assisted';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await openWorkspace();

  // 1. Deny: the card shows identity, state, digest and exact content; the
  // denial reaches the waiter and the skill body never enters the transcript.
  await send('TRUST_DENY');
  await card().waitFor();
  const denyCard = card();
  const denyRequestId = await denyCard.getAttribute('data-request-id');
  assert.ok(denyRequestId);
  const denyText = await denyCard.innerText();
  assert.match(denyText, /trust-deny/);
  assert.match(denyText, /CodeLeveler/);
  assert.match(denyText, /skill/);
  assert.match(denyText, /project/);
  assert.match(denyText, /未批准/);
  await expand();
  assert.match(await denyCard.locator('pre').first().innerText(), /TRUST_DENY_BODY_MARKER/);
  const id = await sessionId();
  const pending = (await page.evaluate(id => window.desktop.snapshot(id), id)).pending_interactions;
  const trustPending = pending.find(item => item.type === 'definition_trust');
  assert.ok(trustPending, `the real snapshot must project the live trust waiter: ${JSON.stringify(pending)}`);
  assert.equal(trustPending.request.id, denyRequestId);
  assert.ok(trustPending.request.current_digest);
  assert.match(trustPending.request.current_content, /TRUST_DENY_BODY_MARKER/);
  evidence.snapshot_pending_trust = { id: trustPending.request.id, state: trustPending.request.state, digest: trustPending.request.current_digest };
  await decide('拒绝');
  await waitDone('TRUST_DENY');
  assert.equal(toolContents.some(content => content.includes('TRUST_DENY_BODY_MARKER')), false, 'a denied definition must never enter the model context');
  evidence.denied_content_not_loaded = true;

  // An already-resolved answer is refused by the runtime, not silently applied.
  const duplicate = await page.evaluate(({ sessionId, requestId }) => window.desktop.deliver({
    command_id: crypto.randomUUID(),
    session_id: sessionId,
    expected_version: null,
    issued_at: new Date().toISOString(),
    command: { type: 'definition_trust_decision', request_id: requestId, decision: 'deny', session_id: sessionId },
  }), { sessionId: id, requestId: denyRequestId });
  assert.equal(duplicate.ok, false);
  assert.match(duplicate.error.message, /not found|already resolved|no longer waiting/i);
  evidence.resolved_answer_refused = duplicate.error.message;

  // 2. Allow once: the exact bytes are admitted for this session and load.
  await send('TRUST_ALLOW_ONCE');
  await card().waitFor();
  await decide('允许本次使用');
  await waitDone('TRUST_ALLOW_ONCE');
  for (let attempt = 0; attempt < 50 && !toolContents.some(content => content.includes('TRUST_ALLOW_BODY_MARKER')); attempt += 1) {
    await page.waitForTimeout(100);
  }
  assert.ok(toolContents.some(content => content.includes('TRUST_ALLOW_BODY_MARKER')), 'the admitted content must reach the model');
  evidence.allow_once_loaded = true;

  // 3. Trust this version persists for a later session.
  await send('TRUST_PERSIST');
  await card().waitFor();
  await decide('信任当前版本');
  await waitDone('TRUST_PERSIST');
  for (let attempt = 0; attempt < 50 && !toolContents.some(content => content.includes('TRUST_PERSIST_BODY_MARKER')); attempt += 1) {
    await page.waitForTimeout(100);
  }
  assert.ok(toolContents.some(content => content.includes('TRUST_PERSIST_BODY_MARKER')));
  // A later session in the same workspace must inherit the persistent
  // approval without a second prompt.
  await page.locator('.workspace-group').filter({ hasText: 'workspace' }).locator('.group-new').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await send('TRUST_PERSIST_AGAIN');
  await waitDone('TRUST_PERSIST_AGAIN');
  assert.equal(await card().count(), 0, 'a persistent approval must answer a later session without a second prompt');
  for (let attempt = 0; attempt < 50 && toolContents.filter(content => content.includes('TRUST_PERSIST_BODY_MARKER')).length < 2; attempt += 1) {
    await page.waitForTimeout(100);
  }
  assert.ok(toolContents.filter(content => content.includes('TRUST_PERSIST_BODY_MARKER')).length >= 2, 'the later session must load the approved content');
  evidence.persistent_trust_across_sessions = true;

  // 4. Cancelling the turn releases the waiter and dismisses the card. The
  // composer (and its stop button) is hidden while a takeover card is shown;
  // the documented double-Escape stop is the user gesture in this state.
  await send('TRUST_CANCEL');
  await card().waitFor();
  await page.keyboard.press('Escape');
  await page.waitForTimeout(150);
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
  await page.waitForFunction(() => document.querySelector('#conversation')?.innerText.includes('已取消'), null, { timeout: 30000 });
  assert.equal(toolContents.some(content => content.includes('TRUST_CANCEL_BODY_MARKER')), false);
  evidence.cancelled_waiter_dismissed = true;

  // 5. A Desktop restart re-projects the still-live waiter from the snapshot.
  await send('TRUST_REOPEN');
  await card().waitFor();
  const reopenId = await card().getAttribute('data-request-id');
  const reopenSession = await sessionId();
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), reopenSession);
  await page.locator(`.session-row[data-session-id="${reopenSession}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await card().waitFor();
  assert.equal(await card().getAttribute('data-request-id'), reopenId);
  evidence.reopen_restored_waiter = { request_id: reopenId };
  await decide('拒绝');
  await waitDone('TRUST_REOPEN');

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  assert.ok(seen.some(item => item.scenario === 'TRUST_DENY' && item.toolResult));
  Object.assign(evidence, { page_errors: errors, provider_errors: providerErrors, passed: true });
  await writeFile(path.join(output, 'definition-trust-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors, provider_errors: providerErrors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'definition-trust-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'definition-trust-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
