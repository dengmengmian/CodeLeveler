// R3b isolated Desktop acceptance: real Electron, real leveler runtime, a
// test-only SSE provider, and a temporary HOME/workspace. It proves Skills
// discovery, detail, explicit slash invocation, on-demand trust, denial,
// cancellation, content change, restart persistence and workspace isolation.
// Nothing here fabricates trust events; every decision goes through the real
// Definition Trust waiter.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { appendFileSync, existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-r3b-skills-');
const home = path.join(temporary, 'home');
const userHome = path.join(temporary, 'user-home');
const workspaceA = path.join(temporary, 'workspace-a');
const workspaceB = path.join(temporary, 'workspace-b');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(userHome, { recursive: true });
await mkdir(workspaceA, { recursive: true });
await mkdir(workspaceB, { recursive: true });
await mkdir(output, { recursive: true });
const providerLog = path.join(temporary, 'provider-requests.jsonl');

const skill = async (workspace, name, description, body) => {
  const dir = path.join(workspace, '.leveler', 'skills', name);
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, 'SKILL.md'), `---\nname: ${name}\ndescription: ${description}\n---\n\n${body}\n`);
};
await skill(workspaceA, 'r3b-allow', 'R3b allow fixture', 'R3B_ALLOW_BODY');
await skill(workspaceA, 'r3b-deny', 'R3b deny fixture', 'R3B_DENY_BODY');
await skill(workspaceA, 'r3b-cancel', 'R3b cancel fixture', 'R3B_CANCEL_BODY');
await skill(workspaceA, 'r3b-change', 'R3b change fixture', 'R3B_CHANGE_V1');
await skill(workspaceA, 'r3b-persist', 'R3b persistence fixture', 'R3B_PERSIST_BODY');
await skill(workspaceA, 'r3b-shared', 'R3b workspace A fixture', 'R3B_SHARED_BODY_A');
await writeFile(path.join(workspaceA, 'AGENTS.md'), 'R3B_AGENTS_RULE\n');
await skill(workspaceB, 'r3b-shared', 'R3b workspace B fixture', 'R3B_SHARED_BODY_B');
await skill(workspaceB, 'r3b-full', 'R3b Full fixture', 'R3B_FULL_BODY');
const brokenDir = path.join(workspaceA, '.leveler', 'skills', 'r3b-broken');
await mkdir(brokenDir, { recursive: true });
await writeFile(path.join(brokenDir, 'SKILL.md'), Buffer.from([0xff, 0xfe, 0x00, 0x01]));

const requests = [];
const providerErrors = [];
const sse = payloads => payloads
  .map(payload => `data: ${JSON.stringify({ id: 'r3b', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: payload, finish_reason: null }] })}\n\n`)
  .join('') + 'data: [DONE]\n\n';
const text = value => sse([{ role: 'assistant', content: value }, {}]);
const tools = calls => sse([
  {
    role: 'assistant',
    tool_calls: calls.map((call, index) => ({
      index,
      id: call.id,
      type: 'function',
      function: { name: call.name, arguments: JSON.stringify(call.arguments) },
    })),
  },
  { finish_reason: 'tool_calls' },
]);
let toolSeq = 0;
const phases = new Map();
const provider = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const names = (body.tools ?? []).map(tool => tool.function?.name ?? tool.name).filter(Boolean);
    if (!names.length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    const serialized = JSON.stringify(body.messages ?? []);
    const users = (body.messages ?? []).filter(message => message.role === 'user');
    const lastUser = String(users.at(-1)?.content ?? '');
    const marker = lastUser.match(/R3B_[A-Z_0-9]+/)?.[0] ?? 'UNKNOWN';
    const hasToolResult = (body.messages ?? []).some(message => message.role === 'tool');
    requests.push({
      marker,
      tools: names,
      loadSkill: names.includes('load_skill'),
      hasAllowBody: serialized.includes('R3B_ALLOW_BODY'),
      hasDenyBody: serialized.includes('R3B_DENY_BODY'),
      hasCancelBody: serialized.includes('R3B_CANCEL_BODY'),
      hasChangeV1: serialized.includes('R3B_CHANGE_V1'),
      hasChangeV2: serialized.includes('R3B_CHANGE_V2'),
      hasPersistBody: serialized.includes('R3B_PERSIST_BODY'),
      hasSharedA: serialized.includes('R3B_SHARED_BODY_A'),
      hasSharedB: serialized.includes('R3B_SHARED_BODY_B'),
      hasFullBody: serialized.includes('R3B_FULL_BODY'),
      hasPostBaseline: serialized.includes('R3B_POST_BASELINE_SECRET'),
      hasModelBody: serialized.includes('R3B_MODEL_BODY'),
      hasAgentsRule: serialized.includes('R3B_AGENTS_RULE'),
    });
    appendFileSync(providerLog, `${JSON.stringify(requests.at(-1))}\n`);
    let payload;
    if (marker === 'R3B_MODEL_LOAD') {
      // Model-initiated path: enable Skills, then ask for the package once the
      // capability tool result is in. A phase, not "any tool result", so the
      // capability round cannot be mistaken for the load round.
      const phase = phases.get(marker) ?? 'new';
      if (phase === 'new') {
        phases.set(marker, 'capability-sent');
        toolSeq += 1;
        payload = tools([{ id: `r3b-cap-${toolSeq}`, name: 'capability', arguments: { action: 'enable', id: 'skills' } }]);
      } else if (phase === 'capability-sent') {
        phases.set(marker, 'load-sent');
        toolSeq += 1;
        payload = tools([{ id: `r3b-load-${toolSeq}`, name: 'load_skill', arguments: { name: 'r3b-model' } }]);
      } else {
        phases.set(marker, 'done');
        payload = text('R3B_MODEL_LOAD_DONE');
      }
    } else {
      payload = text(`${marker}_DONE`);
    }
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    response.end(payload);
  } catch (error) {
    providerErrors.push(error.message);
    response.writeHead(500);
    response.end('fixture failed');
  }
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture-model"\ncontext_window=131072\nparallel_tool_calls = false\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const profile = path.join(temporary, 'profile');
const evidence = { scope: 'real Electron + Runtime + definition-trust store; test-only SSE provider; temporary HOME/workspaces' };
let app;
let page;
const errors = [];

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome, USERPROFILE: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await page.evaluate(() => {
    window.__r3bEvents = [];
    window.desktop.onEvent(frame => {
      const type = String(frame?.data?.type ?? '');
      if (type.startsWith('definition_trust') || type.startsWith('skill')) window.__r3bEvents.push({ type, query_id: frame.data.query_id, id: frame.data.id ?? frame.data.request?.id ?? null, decision: frame.data.decision ?? null });
    });
  });
};
const restart = async () => {
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
};
const openWorkspace = async workspace => {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.textContent.includes(name), path.basename(workspace));
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const waitSend = () => page.waitForFunction(() => {
  const button = document.querySelector('#send');
  return button && !button.disabled && !button.hidden;
});
const submit = async value => {
  await page.locator('#message').fill(value);
  await waitSend();
  await page.locator('#send').click();
};
const userCount = () => page.locator('.message.user').count();
const waitUserAdded = async before => {
  await page.waitForFunction(count => document.querySelectorAll('.message.user').length > count, before, { timeout: 15000 });
};
const waitReply = async (value, timeout = 60000) => {
  await page.waitForFunction(text => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text)), value, { timeout });
  await page.waitForFunction(async () => {
    const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
    if (!id) return false;
    const snapshot = await window.desktop.snapshot(id);
    return !['running', 'waiting_user'].includes(snapshot.task_status);
  }, null, { timeout: 60000 });
};
const card = () => page.locator('.definition-trust-card');
const decide = async label => {
  await card().getByRole('button', { name: label, exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
};
const expand = async (label = '查看当前内容') => {
  const details = card().locator('details.definition-trust-content').filter({ hasText: label }).first();
  if (!(await details.evaluate(node => node.open))) await details.locator('summary').click();
};
const openSettingsSkills = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForFunction(() => document.querySelector('#settings-dialog')?.open);
  await page.getByRole('button', { name: 'Agent 与技能' }).click();
  await page.waitForFunction(() => document.querySelector('#settings-content')?.textContent.includes('技能'));
};
const closeSettings = async () => {
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog')?.open);
};
const skillCard = name => page.locator(`.skill-card[data-skill="${name}"]`);
const providerRequests = () => existsSync(providerLog)
  ? readFileSync(providerLog, 'utf8').split('\n').filter(Boolean).map(line => JSON.parse(line))
  : [];

try {
  await launch();
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'assisted';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await openWorkspace(workspaceA);
  // Selecting a workspace clears the current view; the next submission
  // creates the session IN that workspace (the global New Session button
  // deliberately creates a workspace-less chat).
  await submit('R3B_WARMUP');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await waitReply('R3B_WARMUP_DONE');
  const firstSession = await sessionId();

  // D02/D03: discovery and detail come from the real registry, including a
  // broken package and untrusted states.
  await openSettingsSkills();
  await page.waitForFunction(() => document.querySelectorAll('.skill-card').length >= 6);
  for (const name of ['r3b-allow', 'r3b-deny', 'r3b-cancel', 'r3b-change', 'r3b-persist', 'r3b-shared']) {
    assert.equal(await skillCard(name).count(), 1, `${name} must be discovered`);
    assert.match(await skillCard(name).innerText(), /未信任/, `${name} starts untrusted`);
    assert.match(await skillCard(name).innerText(), /CodeLeveler/);
    assert.match(await skillCard(name).innerText(), /项目/);
  }
  await page.waitForFunction(() => document.querySelector('.skill-card[data-skill="r3b-broken"]')?.textContent.includes('定义无效'));
  await skillCard('r3b-allow').getByRole('button', { name: '查看详情' }).click();
  await page.waitForFunction(() => document.querySelector('.skill-detail .skill-body')?.textContent.includes('R3B_ALLOW_BODY'));
  assert.match(await skillCard('r3b-allow').innerText(), /仅供你审阅/);
  evidence.discovery = { workspace: workspaceA, rows: await page.locator('.skill-card').count() };
  evidence.detail_untrusted_preview = (await page.locator('.skill-detail .skill-body').innerText()).slice(0, 120);
  await page.screenshot({ path: path.join(output, 'r3b-skills-settings.png') });
  await closeSettings();

  // D05: an untrusted Skill that the turn does not need is never a prompt.
  let before = await userCount();
  await submit('R3B_ORDINARY_TASK');
  await waitUserAdded(before);
  await waitReply('R3B_ORDINARY_TASK_DONE');
  assert.equal(await card().count(), 0, 'unused untrusted Skills must not interrupt');
  assert.equal(providerRequests().some(item => item.hasAllowBody || item.hasDenyBody), false);
  // AGENTS.md is default project context in the Runtime, not a trust object:
  // it is delivered without any prompt. There is no "untrusted AGENTS.md"
  // state for the Desktop to remind about.
  assert.equal(providerRequests().some(item => item.marker === 'R3B_ORDINARY_TASK' && item.hasAgentsRule), true);
  assert.equal(await page.locator('.definition-trust-card').count(), 0);
  evidence.unused_untrusted_silent = true;
  evidence.agents_md_default_context = true;

  // D06/D07: an explicit /name for an untrusted Skill pauses the submission,
  // shows the real request, and loads the exact bytes after Allow once.
  before = await userCount();
  await submit('/r3b-allow R3B_ALLOW_ONCE');
  await card().waitFor();
  const allowText = await card().innerText();
  assert.match(allowText, /需要信任技能/);
  assert.match(allowText, /r3b-allow/);
  assert.match(allowText, /未批准/);
  assert.ok(await card().getAttribute('data-request-id'));
  await expand();
  assert.match(await card().locator('pre').first().innerText(), /R3B_ALLOW_BODY/);
  await page.screenshot({ path: path.join(output, 'r3b-skills-trust-card.png') });
  await decide('允许本次使用');
  await waitUserAdded(before);
  await waitReply('R3B_ALLOW_ONCE_DONE');
  const allowRequests = providerRequests().filter(item => item.marker === 'R3B_ALLOW_ONCE');
  assert.ok(allowRequests.some(item => item.hasAllowBody), 'the admitted procedure must reach the model');
  assert.match(await page.locator('#conversation').innerText(), /已加载技能 \/r3b-allow/);
  await page.screenshot({ path: path.join(output, 'r3b-skills-delivered.png') });
  evidence.allow_once = { requests: allowRequests.length, body_delivered: true };

  // D08: a denial settles the request without staging a turn and without
  // leaking the body.
  before = await userCount();
  await submit('/r3b-deny R3B_DENY_TASK');
  await card().waitFor();
  await decide('拒绝');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent.includes('已拒绝加载技能'));
  assert.equal(await userCount(), before, 'a denied submission must not create a user message');
  assert.equal(providerRequests().some(item => item.hasDenyBody), false);
  evidence.denyloaded = false;
  evidence.denied_not_sent = true;

  // D09: dismissing the prompt is a refusal, never an implicit approval and
  // never a silent send. (The terminal overlay has the same rule.)
  before = await userCount();
  await submit('/r3b-cancel R3B_CANCEL_TASK');
  await card().waitFor();
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent.includes('已拒绝加载技能'));
  assert.equal(await userCount(), before, 'a dismissed request must not create a user message');
  assert.equal(providerRequests().some(item => item.hasCancelBody), false);
  evidence.dismiss_is_refusal = true;

  // D10: an approval covers exact bytes; editing invalidates it.
  before = await userCount();
  await submit('/r3b-change R3B_CHANGE_FIRST');
  await card().waitFor();
  await decide('信任当前版本');
  await waitUserAdded(before);
  await waitReply('R3B_CHANGE_FIRST_DONE');
  assert.ok(providerRequests().some(item => item.hasChangeV1));
  await skill(workspaceA, 'r3b-change', 'R3b change fixture', 'R3B_CHANGE_V2');
  before = await userCount();
  await submit('/r3b-change R3B_CHANGE_SECOND');
  await card().waitFor();
  const changedText = await card().innerText();
  assert.match(changedText, /内容已变更/);
  await expand('查看当前内容');
  await expand('查看已批准的版本');
  const previews = await card().locator('pre').allInnerTexts();
  assert.ok(previews.some(text => text.includes('R3B_CHANGE_V2')), 'current changed content is shown');
  assert.ok(previews.some(text => text.includes('R3B_CHANGE_V1')), 'the approved version is shown');
  await decide('拒绝');
  assert.equal(providerRequests().some(item => item.hasChangeV2), false);
  evidence.content_change_requires_new_decision = true;

  // D16: a persistent approval survives a Desktop restart and a reopened
  // session; a later unedited Skill loads without a second prompt.
  before = await userCount();
  await submit('/r3b-persist R3B_PERSIST_FIRST');
  await card().waitFor();
  await decide('信任当前版本');
  await waitUserAdded(before);
  await waitReply('R3B_PERSIST_FIRST_DONE');
  await restart();
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), firstSession, { timeout: 30000 });
  await page.locator(`.session-row[data-session-id="${firstSession}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await waitReply('R3B_PERSIST_FIRST_DONE', 10000);
  await openSettingsSkills();
  await page.waitForFunction(() => document.querySelector('.skill-card[data-skill="r3b-persist"]'));
  assert.match(await skillCard('r3b-persist').innerText(), /可用/);
  await closeSettings();
  before = await userCount();
  await submit('/r3b-persist R3B_PERSIST_SECOND');
  assert.equal(await card().count(), 0, 'persistent trust must not prompt again');
  await waitUserAdded(before);
  await waitReply('R3B_PERSIST_SECOND_DONE');
  assert.ok(providerRequests().filter(item => item.hasPersistBody).length >= 2);
  evidence.restart_persistence = true;

  // D17: project-scoped trust never covers the same name in another project.
  before = await userCount();
  await submit('/r3b-shared R3B_SHARED_A');
  await card().waitFor();
  await decide('信任当前版本');
  await waitUserAdded(before);
  await waitReply('R3B_SHARED_A_DONE');
  await openWorkspace(workspaceB);
  await submit('R3B_WARMUP_B');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await waitReply('R3B_WARMUP_B_DONE');
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).repository?.includes('workspace-b'));
  before = await userCount();
  await submit('/r3b-shared R3B_SHARED_B');
  await card().waitFor();
  await expand('查看当前内容');
  assert.match(await card().locator('pre').first().innerText(), /R3B_SHARED_BODY_B/);
  await decide('拒绝');
  assert.equal(providerRequests().some(item => item.hasSharedB), false);
  await openSettingsSkills();
  await page.waitForFunction(() => document.querySelector('.skill-card[data-skill="r3b-shared"]'));
  assert.match(await skillCard('r3b-shared').innerText(), /未信任/);
  assert.equal(await skillCard('r3b-shared').innerText().then(text => text.includes('workspace-b')), true);
  evidence.workspace_isolation = true;
  await closeSettings();

  // D11: FullAccess follows the Runtime's baseline. A Skill present before
  // the session was created is adopted without a GUI prompt; a later edit is
  // refused by delivery, still without inventing a prompt.
  const beforeFullSession = await sessionId();
  await page.locator('.workspace-group').filter({ hasText: path.basename(workspaceB) }).locator('.group-new').click();
  await page.waitForFunction(previous => {
    const row = document.querySelector('.session-row[aria-selected="true"]');
    return row && row.dataset.sessionId !== previous;
  }, beforeFullSession);
  await submit('R3B_FULL_WARMUP');
  await waitReply('R3B_FULL_WARMUP_DONE');
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '完全权限' }).click();
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).mode === 'full_access');
  before = await userCount();
  await submit('/r3b-full R3B_FULL_TASK');
  await waitUserAdded(before);
  await waitReply('R3B_FULL_TASK_DONE');
  assert.equal(await card().count(), 0, 'Full must not prompt');
  assert.ok(providerRequests().some(item => item.hasFullBody), 'Full adopts the pre-session baseline');
  await skill(workspaceB, 'r3b-full', 'R3b Full fixture', 'R3B_POST_BASELINE_SECRET');
  before = await userCount();
  await submit('/r3b-full R3B_FULL_SECOND_TASK');
  await waitUserAdded(before);
  await waitReply('R3B_FULL_SECOND_TASK_DONE');
  assert.equal(await card().count(), 0, 'Full never prompts');
  await page.waitForFunction(() => document.querySelector('#conversation')?.innerText.includes('技能 /r3b-full 未加载'));
  assert.equal(providerRequests().some(item => item.hasPostBaseline), false, 'a post-baseline edit is not adopted');
  evidence.full = { no_prompt: true, pre_session_adopted: true, post_baseline_refused: true };

  // D13: the model-initiated path stays intact: enable Skills, load an
  // untrusted package, deny it, and prove the body never reached the model.
  await skill(workspaceB, 'r3b-model', 'R3b model fixture', 'R3B_MODEL_BODY');
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '工作区内修改' }).click();
  before = await userCount();
  await submit('R3B_MODEL_LOAD');
  await waitUserAdded(before);
  await card().waitFor();
  await decide('拒绝');
  await waitReply('R3B_MODEL_LOAD_DONE');
  assert.equal(providerRequests().some(item => item.hasModelBody), false);
  evidence.model_initiated_denied = true;

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  Object.assign(evidence, { page_errors: errors, provider_errors: providerErrors, provider_requests: requests.length, passed: true });
  await writeFile(path.join(output, 'skills-desktop-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors, provider_errors: providerErrors });
  if (page && !page.isClosed()) {
    evidence.diagnostics = await page.evaluate(() => ({
      trustCards: document.querySelectorAll('.definition-trust-card').length,
      errorBanner: document.querySelector('#error-banner')?.textContent ?? null,
      composerError: document.querySelector('#composer-error')?.textContent ?? null,
      status: document.querySelector('#conversation-status')?.textContent ?? null,
      events: window.__r3bEvents ?? [],
    })).catch(() => null);
  }
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'skills-desktop-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'skills-desktop-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => provider.close(resolve));
  // Stop the isolated daemons this run may have started. `runtime stop` is
  // the same drain path an operator uses; it never kills a runtime with work.
  for (const workspace of [workspaceA, workspaceB]) {
    await execFile(leveler, ['runtime', 'stop'], {
      cwd: workspace,
      env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome },
      timeout: 20000,
    }).catch(() => {});
  }
  await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
