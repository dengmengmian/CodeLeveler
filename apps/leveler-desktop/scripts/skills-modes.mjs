// R3b Chat / Plan / Goal skill boundaries on the real Desktop + Runtime.
//
// A trusted skill is delivered through the same registry in all three axes.
// Plan keeps its read-only admission (a write the skill asks for is held for
// approval and a denial leaves the workspace untouched); Goal keeps its
// executor and settles back to chat. A test-only SSE provider makes the tool
// request; nothing here calls a real model.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { existsSync } from 'node:fs';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-r3b-skills-modes-');
const home = path.join(temporary, 'home');
const userHome = path.join(temporary, 'user-home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home, { recursive: true });
await mkdir(userHome, { recursive: true });
await mkdir(path.join(workspace, '.leveler', 'skills', 'r3b-plan'), { recursive: true });
await mkdir(path.join(workspace, '.leveler', 'skills', 'r3b-goal'), { recursive: true });
await writeFile(
  path.join(workspace, '.leveler', 'skills', 'r3b-plan', 'SKILL.md'),
  '---\nname: r3b-plan\ndescription: R3b plan fixture\n---\n\nR3B_PLAN_BODY\n',
);
await writeFile(
  path.join(workspace, '.leveler', 'skills', 'r3b-goal', 'SKILL.md'),
  '---\nname: r3b-goal\ndescription: R3b goal fixture\n---\n\nR3B_GOAL_BODY\n',
);
const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
// The human approval is the same trust transaction the CLI writes; this only
// removes the interactive confirm for an isolated fixture.
await execFile(leveler, ['trust', 'allow', '--yes'], {
  cwd: workspace,
  env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome },
});

const chunk = (delta, finish) => JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] });
const sseText = value => [
  `data: ${chunk({ role: 'assistant', content: value }, null)}`,
  `data: ${chunk({}, 'stop')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';
const sseTools = calls => [
  `data: ${chunk({ role: 'assistant', tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } })) }, null)}`,
  `data: ${chunk({}, 'tool_calls')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';

const requests = [];
const providerErrors = [];
const provider = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const toolNames = (body.tools ?? []).map(tool => tool.function?.name).filter(Boolean);
    if (!toolNames.length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    const messages = body.messages ?? [];
    const serialized = JSON.stringify(messages);
    const lastUserIndex = messages.reduce((last, message, index) => (
      message.role === 'user' && typeof message.content === 'string' && message.content.includes('R3B_')
        ? index : last
    ), -1);
    const turnText = lastUserIndex >= 0 ? String(messages[lastUserIndex].content ?? '') : '';
    const toolResults = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool').length;
    requests.push({
      turnText,
      toolNames,
      toolResults,
      hasPlanBody: serialized.includes('R3B_PLAN_BODY'),
      hasGoalBody: serialized.includes('R3B_GOAL_BODY'),
    });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    if (turnText.includes('R3B_PLAN_TASK')) {
      if (!toolResults) {
        response.end(sseTools([{ id: 'r3b-plan-write', name: 'write_file', arguments: { path: 'r3b-plan-wrote.txt', content: 'must not exist' } }]));
      } else {
        response.end(sseText('R3B_PLAN_DONE'));
      }
      return;
    }
    if (turnText.includes('R3B_GOAL_TASK')) {
      if (!toolResults) {
        response.end(sseTools([{ id: 'r3b-goal-complete', name: 'update_goal', arguments: { status: 'complete', summary: 'R3B_GOAL_DONE', deliver_answer: true } }]));
      } else {
        response.end(sseText('R3B_GOAL_DONE'));
      }
      return;
    }
    response.end(sseText('R3B_CHAT_DONE'));
  } catch (error) {
    providerErrors.push(error.message);
    response.writeHead(500);
    response.end('fixture failed');
  }
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture-model"\ncontext_window=131072\nparallel_tool_calls = false\n`, { mode: 0o600 });

const profile = path.join(temporary, 'profile');
const evidence = { scope: 'real Electron + Runtime; isolated HOME/workspace; test-only SSE provider' };
let app;
let page;
const errors = [];
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome, USERPROFILE: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.textContent.includes(name), path.basename(workspace));
  const send = async value => {
    await page.locator('#message').fill(value);
    await page.waitForFunction(() => { const button = document.querySelector('#send'); return button && !button.disabled && !button.hidden; });
    await page.locator('#send').click();
  };
  const waitReply = async value => {
    await page.waitForFunction(text => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(text)), value, { timeout: 60000 });
    await page.waitForFunction(async () => {
      const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
      if (!id) return false;
      const snapshot = await window.desktop.snapshot(id);
      return !['running', 'waiting_user'].includes(snapshot.task_status);
    }, null, { timeout: 60000 });
  };
  const chooseMode = async label => {
    await page.locator('#mode-picker').click();
    await page.waitForFunction(() => !!document.querySelector('.menu .menu-item'));
    await page.locator('.menu .menu-item').filter({ hasText: label }).first().click();
    await page.waitForFunction(expected => document.querySelector('#mode-picker .trigger-label')?.textContent === expected, label);
  };
  const snapshot = () => page.evaluate(() => window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId));

  await send('R3B_CHAT_TASK seed the session');
  await waitReply('R3B_CHAT_DONE');

  // Plan: the skill is delivered, but its requested write is held by the
  // existing read-only admission. A denial leaves the workspace untouched.
  await chooseMode('计划');
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).collaboration === 'plan');
  await send('/r3b-plan R3B_PLAN_TASK');
  await page.waitForSelector('.approval-card', { timeout: 60000 });
  const approvalText = await page.locator('.approval-card').innerText();
  assert.match(approvalText, /write|写入|r3b-plan-wrote/);
  await page.screenshot({ path: path.join(output, 'r3b-skills-plan-approval.png') });
  await page.locator('.approval-card').getByRole('button', { name: '拒绝', exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.approval-card'));
  await waitReply('R3B_PLAN_DONE');
  const planRequest = requests.find(item => item.turnText.includes('R3B_PLAN_TASK'));
  assert.ok(planRequest, 'the plan turn must reach the provider');
  assert.equal(planRequest.hasPlanBody, true, 'the trusted skill body reaches Plan');
  assert.equal(planRequest.toolNames.includes('update_goal'), false, 'Plan never gains the goal executor');
  assert.equal(existsSync(path.join(workspace, 'r3b-plan-wrote.txt')), false, 'a denied Plan write must not touch the workspace');
  const planSnapshot = await snapshot();
  assert.equal(planSnapshot.collaboration, 'plan');
  evidence.plan = { skill_loaded: true, approval_denied: true, goal_tool_hidden: true };

  // Goal: the same skill is delivered through the goal route; the goal
  // executor stays available and the completed goal returns to chat.
  await chooseMode('目标');
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).collaboration === 'goal');
  await send('/r3b-goal R3B_GOAL_TASK');
  await waitReply('R3B_GOAL_DONE');
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).collaboration === 'chat', null, { timeout: 60000 });
  const goalRequest = requests.find(item => item.turnText.includes('R3B_GOAL_TASK'));
  assert.ok(goalRequest, 'the goal turn must reach the provider');
  assert.equal(goalRequest.hasGoalBody, true, 'the trusted skill body reaches Goal');
  assert.equal(goalRequest.toolNames.includes('update_goal'), true, 'Goal keeps its completion protocol');
  const goalSnapshot = await snapshot();
  assert.equal(goalSnapshot.collaboration, 'chat');
  evidence.goal = { skill_loaded: true, update_goal_exposed: true, returned_to_chat: true };

  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  Object.assign(evidence, { page_errors: errors, provider_errors: providerErrors, passed: true });
  await writeFile(path.join(output, 'skills-modes-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors, provider_errors: providerErrors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'skills-modes-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'skills-modes-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => provider.close(resolve));
  const isolatedEnv = { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, HOME: userHome };
  await execFile(leveler, ['trust', 'revoke'], { cwd: workspace, env: isolatedEnv, timeout: 20000 }).catch(() => {});
  await execFile(leveler, ['runtime', 'stop'], { cwd: workspace, env: isolatedEnv, timeout: 20000 }).catch(() => {});
  await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
