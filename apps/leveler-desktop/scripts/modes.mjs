// R2b Chat / Plan / Goal acceptance on the real Desktop + Runtime.
//
// Test-only OpenAI-compatible provider over real SSE; real Electron, real
// bridge, real daemon and real workspace filesystem. Every mode fact asserted
// here comes from the Runtime snapshot or a provider request body — never from
// the client's own selected state.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, stat } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-modes-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home);
await mkdir(workspace);

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'isolated real Desktop/Runtime; test-only SSE provider; real daemon, workspace and session rows' };
const requests = [];
const providerErrors = [];
let goalMultiWindows = 0;

const chunk = (delta, finish) => JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] });
const sseText = (text) => [
  `data: ${chunk({ role: 'assistant', content: text }, null)}`,
  `data: ${chunk({}, 'stop')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';
const sseTools = (calls) => [
  `data: ${chunk({ role: 'assistant', tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } })) }, null)}`,
  `data: ${chunk({}, 'tool_calls')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';

const provider = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const toolNames = (body.tools ?? []).map(tool => tool.function?.name);
    if (!toolNames.length) {
      // Prediction / classification calls carry no tools; they are not turns.
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    const messages = body.messages ?? [];
    const userText = messages.filter(message => message.role === 'user')
      .map(message => typeof message.content === 'string' ? message.content : '').join('\n');
    // The turn is identified by its OWN marker message: earlier turns' markers
    // and tool results stay in the model context forever, and the runtime may
    // inject internal notices as user-role rows between rounds.
    const lastUserIndex = messages.reduce((last, message, index) => message.role === 'user'
      && typeof message.content === 'string' && message.content.includes('MODE_') ? index : last, -1);
    const turnText = lastUserIndex >= 0 && typeof messages[lastUserIndex].content === 'string' ? messages[lastUserIndex].content : '';
    const toolResults = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool');
    requests.push({ at: Date.now(), turnText, userText, toolNames, messages, toolResults: toolResults.length });
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    if (turnText.includes('MODE_PLAN_WRITE')) {
      if (toolResults.length) {
        response.end(sseText('PLAN_WRITE_REFUSED'));
      } else {
        response.end(sseTools([{ id: 'write-1', name: 'write_file', arguments: { path: 'plan-wrote.txt', content: 'must not exist' } }]));
      }
      return;
    }
    if (turnText.includes('MODE_PLAN')) {
      if (!toolResults.length) {
        response.end(sseTools([{ id: 'plan-1', name: 'update_plan', arguments: { explanation: 'acceptance plan', plan: [
          { step: '读取工作区', status: 'completed' },
          { step: '分析入口', status: 'in_progress' },
          { step: '给出方案', status: 'pending' },
        ] } }]));
      } else {
        response.end(sseText('PLAN_DONE'));
      }
      return;
    }
    if (turnText.includes('MODE_GOAL_SINGLE')) {
      if (!toolResults.length) {
        // A real tool keeps the first window open long enough for the owed-
        // goal dock (read from the Runtime snapshot) to be observed.
        response.end(sseTools([{ id: 'goal-slow', name: 'run_command', arguments: { program: 'sh', args: ['-c', 'sleep 4'], reason: 'hold the goal window open' } }]));
      } else if (toolResults.length === 1) {
        response.end(sseTools([{ id: 'goal-single', name: 'update_goal', arguments: { status: 'complete', summary: 'GOAL_SINGLE_DONE', deliver_answer: true } }]));
      } else {
        response.end(sseText('GOAL_SINGLE_DONE'));
      }
      return;
    }
    if (turnText.includes('MODE_GOAL_MULTI')) {
      // Each goal window is a fresh turn: the previous window's tool result is
      // not the discriminator. Window 1 yields once; the runtime opens a new
      // window for the SAME objective, which completes; the window's delivery
      // round (its update_goal result in context) speaks the final text.
      goalMultiWindows += 1;
      if (toolResults.length) {
        response.end(sseText('GOAL_MULTI_DONE'));
      } else if (goalMultiWindows === 1) {
        response.end(sseTools([{ id: 'goal-yield', name: 'update_goal', arguments: { status: 'yield', summary: 'window one ended' } }]));
      } else {
        response.end(sseTools([{ id: 'goal-complete', name: 'update_goal', arguments: { status: 'complete', summary: 'GOAL_MULTI_DONE', deliver_answer: true } }]));
      }
      return;
    }
    if (turnText.includes('MODE_STEER')) {
      if (!toolResults.length) {
        response.end(sseTools([{ id: 'slow-1', name: 'run_command', arguments: { program: 'sh', args: ['-c', 'sleep 3'], reason: 'hold the turn open for a steer' } }]));
      } else {
        response.end(sseText('STEER_DONE'));
      }
      return;
    }
    response.end(sseText('CHAT_OK'));
  } catch (error) {
    providerErrors.push(error.message);
    response.destroy(error);
  }
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"
[providers.fixture]
protocol = "openai_chat"
base_url = "http://127.0.0.1:${provider.address().port}"
api_key = "test-only"
[models.m]
provider = "fixture"
model_id = "fixture-model"
context_window = 131072
parallel_tool_calls = false
vision = false
`, { mode: 0o600 });

let app;
let page;
const pageErrors = [];

async function launch() {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => pageErrors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } });
}

const selectedId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const snapshot = async () => {
  const id = await selectedId();
  return id ? page.evaluate(sessionId => window.desktop.snapshot(sessionId), id) : null;
};
async function waitSnapshot(predicate, message) {
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const value = await snapshot().catch(() => null);
    if (value && predicate(value)) return value;
    await page.waitForTimeout(50);
  }
  throw new Error(message);
}
const taskCount = async () => (await page.evaluate(() => window.desktop.listTasks({ include_archived: false }))).tasks?.length ?? 0;
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 180000 });
const receipts = () => page.locator('.turn-receipt').count();
async function waitTurn(before) {
  await page.waitForFunction(count => document.querySelectorAll('.turn-receipt').length >= count, before + 1, { timeout: 180000 });
  await idle();
}
const triggerMode = () => page.locator('#mode-picker .trigger-label').innerText();

async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
async function openModeMenu() {
  // A post-turn refresh briefly disables every composer control; a click that
  // races that window never reaches the button. Wait for the real enabled
  // state, then retry if the menu did not open.
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForFunction(() => document.querySelector('#mode-picker') && !document.querySelector('#mode-picker').disabled, null, { timeout: 30000 });
    await page.locator('#mode-picker').click();
    try {
      await page.waitForSelector('.menu .menu-item', { timeout: 1000 });
      return;
    } catch {
      await page.waitForTimeout(100);
    }
  }
  throw new Error('the execution-mode menu did not open');
}
async function chooseMode(label) {
  await openModeMenu();
  await page.locator('.menu .menu-item').filter({ hasText: new RegExp(`^${label}$`) }).first().click();
}
async function switchMode(label) {
  await chooseMode(label);
  await page.waitForFunction(expected => document.querySelector('#mode-picker .trigger-label')?.textContent === expected, label);
}
const send = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); };

try {
  await launch();
  await addWorkspace();

  // A. Browsing and choosing a pre-session mode creates nothing.
  const beforeTasks = await taskCount();
  await page.locator('#message').fill('MODE_PLAN 多步规划');
  await openModeMenu();
  const menuItems = await page.locator('.menu .menu-item').allInnerTexts();
  assert.ok(menuItems.some(item => item === '对话') && menuItems.some(item => item === '计划') && menuItems.some(item => item === '目标'), `mode menu: ${menuItems}`);
  await page.keyboard.press('Escape');
  assert.equal(await taskCount(), beforeTasks, 'browsing the mode menu must not create a session');
  assert.equal(await page.locator('#message').inputValue(), 'MODE_PLAN 多步规划');
  await chooseMode('计划');
  await page.waitForFunction(() => document.querySelector('#mode-picker .trigger-label')?.textContent === '计划');
  assert.equal(await taskCount(), beforeTasks, 'choosing a pending mode must not create a session');
  assert.equal(await page.locator('#message').inputValue(), 'MODE_PLAN 多步规划');
  evidence.pending_mode_is_inert = true;

  // B. Plan session: created with the requested axis, runs the Chat profile
  //    without the goal executor, and its plan comes from runtime events.
  const receiptsBeforePlan = await receipts();
  await page.locator('#send').click();
  await waitSnapshot(value => value.collaboration === 'plan', 'created session must carry the plan axis');
  const planId = await selectedId();
  await page.waitForSelector('.plan-card-row', { timeout: 120000 });
  await waitTurn(receiptsBeforePlan);
  const planSnapshot = await snapshot();
  assert.equal(planSnapshot.collaboration, 'plan');
  assert.ok((planSnapshot.plan?.steps ?? []).length >= 2, JSON.stringify(planSnapshot.plan));
  const planRequest = requests.find(request => request.turnText.includes('MODE_PLAN 多步规划'));
  assert.ok(planRequest, 'the plan turn must reach the provider');
  assert.equal(planRequest.toolNames.includes('update_goal'), false, 'plan must not expose the goal executor');
  assert.ok(planRequest.toolNames.includes('update_plan'), 'plan keeps the plan control');
  const planCardText = await page.locator('.plan-card-row').first().innerText();
  assert.match(planCardText, /执行计划/);
  assert.match(planCardText, /步骤已完成/);
  assert.equal(await page.locator('.todo-dock').count(), 0, 'the plan must not repeat beside its transcript summary');
  await page.locator('.plan-card-row').first().click();
  await page.waitForSelector('.plan-view');
  const planViewText = await page.locator('.plan-view').innerText();
  for (const step of planSnapshot.plan.steps) assert.ok(planViewText.includes(step.description), `plan view must show ${step.description}`);
  evidence.plan_runtime_plan = planSnapshot.plan;

  // C. Plan is genuinely read-only: a write is held for approval, and a
  //    denial leaves the workspace untouched.
  const receiptsBeforeWrite = await receipts();
  await send('MODE_PLAN_WRITE 尝试写入');
  await page.waitForSelector('.approval-card', { timeout: 120000 });
  const approvalText = await page.locator('.approval-card').innerText();
  assert.match(approvalText, /write_file|写入|plan-wrote/, approvalText);
  await page.locator('.approval-card').getByRole('button', { name: '拒绝' }).click();
  await waitTurn(receiptsBeforeWrite);
  await assert.rejects(() => stat(path.join(workspace, 'plan-wrote.txt')), 'plan must not write');
  evidence.plan_write_held_for_approval = true;

  // D. Switch an existing session to chat; only the Runtime's snapshot counts.
  await switchMode('对话');
  await waitSnapshot(value => value.collaboration === 'chat', 'runtime must confirm chat');
  const receiptsBeforeChat = await receipts();
  await send('MODE_CHAT 普通对话');
  await waitTurn(receiptsBeforeChat);
  const chatRequest = requests.find(request => request.turnText.includes('MODE_CHAT'));
  assert.ok(chatRequest);
  assert.equal(chatRequest.toolNames.includes('update_goal'), false);
  evidence.mode_switch_confirmed_by_runtime = true;

  // E. Goal session: the goal executor runs, completion settles the goal and
  //    the Runtime itself returns the axis to chat.
  await switchMode('目标');
  await waitSnapshot(value => value.collaboration === 'goal', 'runtime must confirm goal');
  const beforeGoal = await taskCount();
  const receiptsBeforeGoal = await receipts();
  await send('MODE_GOAL_SINGLE 完成目标');
  // The owed-goal dock must come from the Runtime snapshot and name the
  // objective; it is read once per turn at the first tool.
  await page.waitForSelector('.goal-dock', { timeout: 30000 });
  const goalDock = await page.locator('.goal-dock').innerText();
  assert.match(goalDock, /完成目标/, `goal dock: ${goalDock}`);
  assert.match(goalDock, /执行中/, `goal dock: ${goalDock}`);
  evidence.goal_dock = goalDock;
  await waitTurn(receiptsBeforeGoal);
  const closed = await waitSnapshot(value => value.collaboration === 'chat', 'a completed goal ends its goal route');
  assert.equal(await taskCount(), beforeGoal, 'the goal ran in the same session');
  assert.equal(await triggerMode(), '对话');
  const goalRequests = requests.filter(request => request.turnText.includes('MODE_GOAL_SINGLE'));
  assert.ok(goalRequests.length >= 1);
  assert.equal(goalRequests[0].toolNames.includes('update_goal'), true, 'goal must expose update_goal');
  assert.ok(goalRequests[0].userText.includes('完成目标'));
  evidence.goal_single_window = { requests: goalRequests.length, final_axis: closed.collaboration };

  // F. Multi-window goal: yield opens a fresh window for the SAME goal; the
  //    user sees one logical turn and one terminal, never a window "complete".
  await switchMode('目标');
  await waitSnapshot(value => value.collaboration === 'goal', 'runtime must confirm goal again');
  const receiptsBeforeMulti = await receipts();
  await send('MODE_GOAL_MULTI 长任务');
  await waitTurn(receiptsBeforeMulti);
  const multiRequests = requests.filter(request => request.turnText.includes('MODE_GOAL_MULTI'));
  const checkpointRequests = multiRequests.filter(request => request.turnText.startsWith('[GOAL CHECKPOINT]'));
  assert.equal(multiRequests.length >= 3, true, `yield → checkpoint window → delivery: ${multiRequests.length}`);
  assert.equal(multiRequests[0].toolNames.includes('update_goal'), true);
  // The runtime continues the SAME objective in a fresh window seeded from a
  // durable goal checkpoint, not by replaying the previous window's turn.
  assert.equal(checkpointRequests.length >= 2, true, `checkpoint window requests: ${checkpointRequests.length}`);
  assert.ok(checkpointRequests.at(-1).toolResults >= 1, 'the checkpoint window delivers after its own update_goal result');
  assert.equal(await receipts() - receiptsBeforeMulti, 1, 'a yielded window must not paint a completion receipt');
  await waitSnapshot(value => value.collaboration === 'chat', 'the finished goal returns to chat');
  evidence.goal_multi_window = {
    requests: multiRequests.length,
    checkpoint_requests: checkpointRequests.length,
    receipts: (await receipts()) - receiptsBeforeMulti,
  };

  // G. Steering a running turn: the text reaches the next model round of the
  //    same turn; no new session and no new logical turn.
  const receiptsBeforeSteer = await receipts();
  await send('MODE_STEER 长命令');
  await page.waitForFunction(() => ['运行中', '等待你处理'].includes(document.querySelector('#conversation-status')?.textContent ?? ''), null, { timeout: 60000 });
  const tasksBeforeSteer = await taskCount();
  await page.locator('#message').fill('STEER_TEXT_123');
  await page.locator('#message').press('Meta+Enter');
  await waitTurn(receiptsBeforeSteer);
  const steerSeen = requests.some(request => request.messages.some(message => message.role === 'user'
    && typeof message.content === 'string' && message.content.includes('STEER_TEXT_123')));
  assert.equal(steerSeen, true, 'the running turn must receive the steer text');
  assert.equal(await taskCount(), tasksBeforeSteer, 'steering must not create a session');
  evidence.running_steer = { delivered: true };

  // H. Reopening and restarting keep the Runtime's axis and plan. The axis was
  //    returned to plan first: a completed Goal legitimately left it as chat.
  await switchMode('计划');
  const reopened = await waitSnapshot(value => value.collaboration === 'plan', 'the plan axis is confirmed before reopening');
  assert.ok((reopened.plan?.steps ?? []).length >= 2, 'reopened plan steps');

  await app.evaluate(({ app: electronApp }) => electronApp.exit(0));
  app = null;
  await new Promise(resolve => setTimeout(resolve, 1500));
  await launch();
  await page.waitForSelector(`.session-row[data-session-id="${planId}"]`, { timeout: 30000 });
  await page.locator(`.session-row[data-session-id="${planId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  const restarted = await waitSnapshot(value => value.id === planId, 'the plan session reopens after restart');
  assert.equal(restarted.collaboration, 'plan', 'the runtime axis survives a Desktop restart');
  assert.ok((restarted.plan?.steps ?? []).length >= 2, 'restarted plan steps');
  await page.waitForFunction(() => document.querySelector('#mode-picker .trigger-label')?.textContent === '计划');
  evidence.restart = { axis: restarted.collaboration, plan_steps: (restarted.plan?.steps ?? []).length };

  // I. A refused switch keeps the Runtime's value on screen (no optimistic UI).
  const refusedAxis = restarted.collaboration;
  await app.evaluate(({ ipcMain }) => {
    const original = ipcMain._invokeHandlers.get('desktop:deliver');
    globalThis.__modesDeliverHandler = original;
    ipcMain.removeHandler('desktop:deliver');
    ipcMain.handle('desktop:deliver', (event, envelope) => envelope.command.type === 'set_product_axes'
      ? { ok: false, error: { kind: 'rejected', message: 'Test-only mode refusal' } }
      : original(event, envelope));
  });
  try {
    await chooseMode('对话');
    await page.waitForFunction(() => !document.querySelector('#composer-error').hidden
      && document.querySelector('#composer-error').textContent.includes('Test-only mode refusal'));
    assert.equal((await snapshot()).collaboration, refusedAxis, 'a refused switch must not change the runtime axis');
    assert.equal(await triggerMode(), '计划');
  } finally {
    await app.evaluate(({ ipcMain }) => {
      ipcMain.removeHandler('desktop:deliver');
      ipcMain.handle('desktop:deliver', globalThis.__modesDeliverHandler);
      delete globalThis.__modesDeliverHandler;
    });
  }
  evidence.refused_switch_not_optimistic = true;

  assert.deepEqual(pageErrors, []);
  assert.deepEqual(providerErrors, []);
  Object.assign(evidence, { passed: true, provider_requests: requests.length, page_errors: pageErrors });
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: pageErrors, provider_errors: providerErrors });
  // Request-level facts for diagnosing a stuck turn; message bodies stay out
  // of the evidence file.
  evidence.requests = requests.slice(-40).map(request => ({
    turn: String(request.turnText ?? '').slice(0, 80),
    tools: request.toolResults,
    has_goal: request.toolNames.includes('update_goal'),
    has_plan: request.toolNames.includes('update_plan'),
    roles: request.messages.map(message => message.role),
  }));
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r2b-modes-failure.png') }).catch(() => {});
  throw error;
} finally {
  await writeFile(path.join(output, 'r2b-modes-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
  if (app) await app.evaluate(({ app: electronApp }) => electronApp.exit(0)).catch(() => {});
  await new Promise(resolve => provider.close(resolve));
}
