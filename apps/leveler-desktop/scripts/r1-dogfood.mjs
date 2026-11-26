// R1 unified dogfood on the real local HOME/profile with the real configured
// provider. It creates only new sessions in a temporary workspace; existing
// sessions, configuration and workspaces are read-only.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
// This script touches the real profile: require an explicit binary from this
// worktree instead of silently using a shared target/debug copy.
if (!process.env.LEVELER_BINARY) throw new Error('Set LEVELER_BINARY to this worktree\'s leveler build');
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const workspace = await mkdtemp('/tmp/leveler-r1-dogfood-');
const suffix = randomUUID().slice(0, 8);
const allowSkill = `r1-dogfood-allow-${suffix}`;
const denySkill = `r1-dogfood-deny-${suffix}`;
for (const [name, marker] of [[allowSkill, 'R1_TRUST_ALLOW_BODY'], [denySkill, 'R1_TRUST_DENY_BODY']]) {
  const dir = path.join(workspace, '.leveler', 'skills', name);
  await mkdir(dir, { recursive: true });
  await writeFile(path.join(dir, 'SKILL.md'), `---\nname: ${name}\ndescription: R1 dogfood fixture ${name}\n---\n\n# ${name}\n\n${marker}\n`);
}
const FAKE_KEY = 'sk-r1dogfood0123456789abcd';
const evidence = {
  scope: 'real local HOME/profile, real configured provider, real Desktop/Runtime; temp workspace; only new sessions are created',
  binary: { path: leveler, sha256: binarySha },
  workspace,
  skills: { allow: allowSkill, deny: denySkill },
  steps: {},
};
let app;
let page;
const errors = [];

const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 30000 });
};
const tasks = () => page.evaluate(() => window.desktop.listTasks({ include_archived: false }));
const taskCount = async () => (await tasks()).tasks?.length ?? 0;
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const userCount = () => page.locator('.message.user').count();
const statusIdle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 240000 });
// A trust card appears while the turn is paused; an idle status from the
// previous turn is not an answer. Wait for the card or for a new reply.
const assistantCount = () => page.locator('.message.assistant .markdown').count();
const settledReply = previous => page.waitForFunction(prev => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return document.querySelectorAll('.message.assistant .markdown').length > prev
    && status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, previous, { timeout: 240000 });
const settledOnTrust = previous => page.waitForFunction(prev => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  const replied = document.querySelectorAll('.message.assistant .markdown').length > prev;
  return !!document.querySelector('.definition-trust-card')
    || (replied && status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status));
}, previous, { timeout: 240000 });
const noTakeover = () => page.waitForFunction(() => !document.querySelector('.interaction-card, .secret-review-modal'));
const send = async text => {
  const before = await userCount();
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  await page.waitForFunction(before => document.querySelectorAll('.message.user').length > before, before, { timeout: 30000 });
  // The status flips to running just after the message is projected.
  await page.waitForTimeout(400);
};
const replyText = () => page.locator('.message.assistant .markdown').last().innerText().catch(() => '');
const openPicker = async () => { await page.locator('#model-picker').click(); await page.waitForSelector('.menu'); };
const selectModel = async avoid => {
  const items = page.locator('.menu .menu-item');
  for (let index = 0; index < await items.count(); index += 1) {
    const item = items.nth(index);
    if (!(await item.locator('.menu-label').count())) continue;
    const label = (await item.locator('.menu-label').innerText()).trim();
    if (!label || (avoid && label === avoid)) continue;
    await item.click();
    await page.waitForFunction(() => !document.querySelector('.menu'));
    return label;
  }
  throw new Error(`no model to select${avoid ? ` other than ${avoid}` : ''}`);
};
const switchToAssisted = async () => {
  await noTakeover();
  await page.locator('#permission-picker').click();
  await page.waitForSelector('.menu');
  const item = page.locator('.menu .menu-item').filter({ hasText: '工作区内修改' }).first();
  await item.click();
  await page.waitForFunction(async () => {
    const row = document.querySelector('.session-row[aria-selected="true"]');
    return row && (await window.desktop.snapshot(row.dataset.sessionId)).mode === 'assisted';
  }, null, { timeout: 30000 });
};

try {
  await launch();
  const ps = execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
  const bridge = ps.split('\n').find(line => line.includes(`${leveler} desktop-bridge`));
  assert.ok(bridge, `the running bridge must be ${leveler}`);
  evidence.binary.bridge_process = bridge.trim();
  assert.equal(await page.evaluate(() => document.body.classList.contains('home')), true);
  const baseline = await taskCount();
  assert.equal((await tasks()).source_errors?.length ?? 0, 0);
  evidence.steps.startup = {
    state: 'PASS',
    baseline_tasks: baseline,
    workspace_groups: await page.locator('.workspace-group').count(),
    model_picker: (await page.locator('#model-picker').innerText()).trim(),
    source_errors: 0,
  };

  // Temp workspace via the real folder-choice seam; no session yet.
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('leveler-r1-dogfood-'));
  await openPicker();
  const chosenModel = await selectModel();
  assert.equal(await taskCount(), baseline);
  evidence.steps.pending_model = { state: 'PASS', model: chosenModel, sessions_created: 0 };

  // 1) Real model + real tool call in the temp workspace.
  const beforeAgent = await assistantCount();
  await send('在当前工作区创建文件 r1-dogfood.txt，内容为 R1_DOGFOOD_TOOL_OK，然后读取它确认。完成后只回复 R1_DOGFOOD_TOOL_OK。');
  await settledReply(beforeAgent);
  const createdId = await sessionId();
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), createdId);
  const toolFile = path.join(workspace, 'r1-dogfood.txt');
  const toolOk = existsSync(toolFile) && readFileSync(toolFile, 'utf8').includes('R1_DOGFOOD_TOOL_OK');
  const reply1 = await replyText();
  assert.equal(await taskCount(), baseline + 1, 'the first send must create exactly one session');
  evidence.steps.agent_and_tool = { state: toolOk ? 'PASS' : 'BLOCKED', session: createdId, mode: snapshot.mode, model: snapshot.model, file: toolOk, reply: reply1.slice(0, 160) };

  // Definition Trust prompts only outside FullAccess; switch this new test
  // session to Assisted through the normal UI (user configuration untouched).
  await switchToAssisted();
  evidence.steps.session_mode = { state: 'PASS', mode: 'assisted' };

  // 2) Definition Trust: allow once through the real load_skill tool.
  const beforeAllow = await assistantCount();
  await send(`请先调用 capability 工具，参数 {"action":"enable","id":"skills"}。等工具返回后，调用 load_skill 工具，参数 {"name":"${allowSkill}"}，按其内容操作。完成后回复 R1_TRUST_ALLOW_OK。`);
  let trustAllow = { state: 'BLOCKED', reason: 'no trust card; the model did not request the definition' };
  await settledOnTrust(beforeAllow);
  if (await page.locator('.definition-trust-card').count()) {
    const card = page.locator('.definition-trust-card').first();
    trustAllow = { state: 'PASS', request: await card.getAttribute('data-request-id'), text: (await card.innerText()).slice(0, 200) };
    await card.getByRole('button', { name: '允许本次使用', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
    await statusIdle();
    trustAllow.reply = (await replyText()).slice(0, 160);
  }
  evidence.steps.definition_trust_allow = trustAllow;

  // 3) Definition Trust: deny.
  await noTakeover();
  const beforeDeny = await assistantCount();
  await send(`请调用 load_skill 工具，参数 {"name":"${denySkill}"}。完成后回复 R1_TRUST_DENY_OK。`);
  let trustDeny = { state: 'BLOCKED', reason: 'no trust card; the model did not request the definition' };
  await settledOnTrust(beforeDeny);
  if (await page.locator('.definition-trust-card').count()) {
    const card = page.locator('.definition-trust-card').first();
    trustDeny = { state: 'PASS', request: await card.getAttribute('data-request-id'), text: (await card.innerText()).slice(0, 200) };
    await card.getByRole('button', { name: '拒绝', exact: true }).click();
    await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
    await statusIdle();
    trustDeny.reply = (await replyText()).slice(0, 160);
  }
  evidence.steps.definition_trust_deny = trustDeny;

  // 4) Credential capture and revoke through the real GUI. A captured secret
  // is only usable for a purpose the user stated; if the model tries to apply
  // it elsewhere the runtime asks, and the test denies that request.
  await noTakeover();
  const beforeSecret = await userCount();
  let secret = { state: 'BLOCKED', reason: 'review did not open' };
  await page.locator('#message').fill(`下面是一个测试凭据：API_KEY=${FAKE_KEY}。请不要调用任何工具，也不要写入文件；完成后只回复 R1_SECRET_OK。`);
  await page.locator('#send').click();
  try {
    await page.locator('.secret-review-modal').waitFor({ timeout: 60000 });
    const reviewText = await page.locator('.secret-review-modal').innerText();
    assert.match(reviewText, /安全接管/);
    assert.equal(reviewText.includes(FAKE_KEY), false, 'the review must show a mask, not the value');
    const beforeSecretReply = await assistantCount();
    await page.locator('.secret-review-modal').getByRole('button', { name: '安全接管并发送', exact: true }).click();
    await page.waitForFunction(before => document.querySelectorAll('.message.user').length > before, beforeSecret, { timeout: 30000 });
    await page.waitForTimeout(400);
    const settled = () => page.waitForFunction(prev => {
      const status = document.querySelector('#conversation-status')?.textContent ?? '';
      const replied = document.querySelectorAll('.message.assistant .markdown').length > prev;
      return !!document.querySelector('.approval-card')
        || (replied && status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status));
    }, beforeSecretReply, { timeout: 240000 });
    await settled();
    const denied = [];
    for (let attempt = 0; attempt < 3 && await page.locator('.approval-card').count(); attempt += 1) {
      denied.push((await page.locator('.approval-card').first().innerText()).slice(0, 160));
      await page.locator('.approval-card').first().getByRole('button', { name: '拒绝', exact: true }).click();
      await page.waitForFunction(() => !document.querySelector('.approval-card'), null, { timeout: 30000 });
      await page.waitForTimeout(400);
      await settled();
    }
    await statusIdle();
    const token = (await page.locator('.message.user').last().innerText()).match(/\[secret:sec_[0-9a-f]{32}\]/);
    const drafts = await page.evaluate(() => localStorage.getItem('codeleveler.drafts.v1') ?? '');
    assert.ok(token, 'the admitted message must carry a reference token');
    assert.equal(drafts.includes(FAKE_KEY), false);
    secret = { state: 'PASS', token: token[0], drafts_clean: true, denied_secret_use: denied, reply: (await replyText()).slice(0, 120) };
    const revoke = page.locator('.secret-revoke').last();
    await revoke.waitFor({ timeout: 15000 });
    await revoke.click();
    await page.getByRole('button', { name: '撤销', exact: true }).click();
    await page.waitForFunction(() => document.querySelector('.toast')?.textContent.includes('已撤销'), null, { timeout: 30000 });
    secret.revoke = 'PASS';
  } catch (error) {
    secret.reason = error.message;
    await statusIdle().catch(() => {});
  }
  evidence.steps.credential = secret;

  // 5) Existing-session model switch adds no session and reconciles the
  // runtime's confirmed model.
  await noTakeover();
  const beforeSwitch = await taskCount();
  const modelBefore = (await page.evaluate(id => window.desktop.snapshot(id), createdId)).model.model;
  await openPicker();
  const switched = await selectModel(modelBefore);
  assert.equal(await taskCount(), beforeSwitch);
  await page.waitForFunction(async ({ id, model }) => (await window.desktop.snapshot(id)).model.model === model, { id: createdId, model: switched }, { timeout: 30000 });
  evidence.steps.model_switch = { state: 'PASS', from: modelBefore, to: switched, sessions_created: 0 };

  // 6) Restart and read back the session.
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  assert.equal(await taskCount(), beforeSwitch, 'restart must not create or lose sessions');
  assert.equal((await tasks()).source_errors?.length ?? 0, 0);
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), createdId);
  await page.locator(`.session-row[data-session-id="${createdId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  const historyText = await page.locator('#conversation').innerText();
  assert.ok(historyText.includes('r1-dogfood.txt'), 'the created session history must survive the restart');
  evidence.steps.restart = { state: 'PASS', tasks_unchanged: true, history_readable: true, conversation_chars: historyText.length };

  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: Object.values(evidence.steps).every(step => step.state === 'PASS') });
  await writeFile(path.join(output, 'r1-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r1-dogfood-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'r1-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
