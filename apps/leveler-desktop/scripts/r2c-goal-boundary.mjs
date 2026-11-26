// R2c: the Goal automatic-continuation boundary, on the real Desktop + Runtime.
//
// Two scripted models, both real turns through the real daemon:
// - R2C_GOAL_WORK performs real work in its first window, yields, and completes
//   in the second: the runtime must keep continuing it.
// - R2C_GOAL_EMPTY only ever yields: the runtime must stop opening empty
//   windows after the structured no-work guard, keep the goal owed, and never
//   present the window as completion.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-goal-boundary-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home);
await mkdir(workspace);

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'isolated real Desktop/Runtime; test-only SSE provider; real goal windows', steps: {} };
const requests = [];
let emptyRequests = 0;
let workRequests = 0;

const chunk = (delta, finish) => JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] });
const sseText = text => [
  `data: ${chunk({ role: 'assistant', content: text }, null)}`,
  `data: ${chunk({}, 'stop')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';
const sseTools = calls => [
  `data: ${chunk({ role: 'assistant', tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } })) }, null)}`,
  `data: ${chunk({}, 'tool_calls')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';

const provider = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const toolNames = (body.tools ?? []).map(tool => tool.function?.name);
  if (!toolNames.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  const messages = body.messages ?? [];
  const lastUserIndex = messages.reduce((last, message, index) => message.role === 'user'
    && typeof message.content === 'string' && /R2C_GOAL_/.test(message.content) ? index : last, -1);
  const turnText = lastUserIndex >= 0 && typeof messages[lastUserIndex].content === 'string' ? messages[lastUserIndex].content : '';
  const toolResults = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool').length;
  requests.push({ turnText, toolResults });
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (turnText.includes('R2C_GOAL_EMPTY')) {
    emptyRequests += 1;
    response.end(sseTools([{ id: `empty-${emptyRequests}`, name: 'update_goal', arguments: { status: 'yield', summary: '本窗口没有执行任何工作' } }]));
    return;
  }
  if (turnText.includes('R2C_GOAL_WORK')) {
    workRequests += 1;
    // Window 1: a real command, then yield. Window 2: complete, then deliver
    // the answer in the round that sees the accepted resolution.
    if (workRequests === 1) {
      response.end(sseTools([{ id: 'work-1', name: 'run_command', arguments: { program: 'sh', args: ['-c', 'echo GOAL_WORK_REAL'], reason: 'do real work before yielding' } }]));
      return;
    }
    if (workRequests === 2) {
      response.end(sseTools([{ id: 'yield-1', name: 'update_goal', arguments: { status: 'yield', summary: '窗口一完成了真实工作，窗口二继续' } }]));
      return;
    }
    if (workRequests === 3) {
      response.end(sseTools([{ id: 'work-complete', name: 'update_goal', arguments: { status: 'complete', summary: '两窗口完成', deliver_answer: true } }]));
      return;
    }
    response.end(sseText('GOAL_WORK_DONE'));
    return;
  }
  response.end(sseText('CHAT_OK'));
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
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 120000 });
const receipts = () => page.locator('.turn-receipt').count();
async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
async function chooseGoal() {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForFunction(() => document.querySelector('#mode-picker') && !document.querySelector('#mode-picker').disabled, null, { timeout: 30000 });
    await page.locator('#mode-picker').click();
    try {
      await page.waitForSelector('.menu .menu-item', { timeout: 1000 });
      break;
    } catch {
      await page.waitForTimeout(100);
    }
  }
  await page.locator('.menu .menu-item').filter({ hasText: /^目标$/ }).first().click();
  await page.waitForFunction(() => document.querySelector('#mode-picker .trigger-label')?.textContent === '目标');
}
async function waitTurn(before) {
  await page.waitForFunction(count => document.querySelectorAll('.turn-receipt').length >= count, before + 1, { timeout: 120000 });
  await idle();
}
async function capture(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
}

try {
  await launch();
  await addWorkspace();

  // ---- A productive goal still runs multiple windows.
  await chooseGoal();
  const workReceipts = await receipts();
  await page.locator('#message').fill('R2C_GOAL_WORK 完成一个两窗口任务');
  await page.locator('#send').click();
  await waitTurn(workReceipts);
  assert.ok(workRequests >= 3, `window 1 work + yield, window 2 complete + delivery: ${workRequests}`);
  assert.equal(emptyRequests, 0);
  const workSnapshot = await snapshot();
  assert.equal(workSnapshot.collaboration, 'chat', 'a completed goal returns to chat: ' + workSnapshot.collaboration);
  evidence.steps.productive_goal = { requests: workRequests, axis_after: workSnapshot.collaboration };

  // ---- An empty-yield goal is stopped by the structured guard, in its own
  //      session so the previous goal's history cannot explain the result.
  await page.locator('#new-session').click();
  await chooseGoal();
  const emptyReceipts = await receipts();
  await page.locator('#message').fill('R2C_GOAL_EMPTY 只做 yield 的目标');
  await page.locator('#send').click();
  await page.waitForSelector('.goal-stop-dock', { timeout: 120000 });
  const stopText = await page.locator('.goal-stop-dock').innerText();
  assert.ok(stopText.includes('自动续跑已停止'), stopText);
  assert.ok(stopText.includes('连续 2 个窗口没有执行工作'), stopText);
  assert.ok(!stopText.includes('已完成'), 'the stop must never read as completion');
  assert.equal(emptyRequests, 2, `exactly two empty windows were opened: ${emptyRequests}`);
  const emptySnapshot = await snapshot();
  assert.equal(emptySnapshot.collaboration, 'goal', 'the goal is still the session axis');
  assert.ok(emptySnapshot.execution_task, 'the goal still owes its work');
  assert.equal(await receipts(), emptyReceipts, 'an empty window and its policy stop paint no receipt');
  evidence.steps.empty_goal = {
    requests: emptyRequests,
    axis: emptySnapshot.collaboration,
    owed: Boolean(emptySnapshot.execution_task),
    stop_text: stopText,
  };
  await capture('r2c-goal-continuation-stopped.png');

  assert.deepEqual(pageErrors, [], 'no renderer page errors');
  evidence.result = 'PASS';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r2c-goal-boundary-failure.png').catch(() => {});
  throw error;
} finally {
  // Evidence first: Electron's close can wait on a process under load, and
  // losing the evidence would turn a real PASS into an unusable run.
  await writeFile(path.join(output, 'r2c-goal-boundary-evidence.json'), JSON.stringify(evidence, null, 2));
  await Promise.race([
    app?.close().catch(() => {}),
    new Promise(resolve => setTimeout(resolve, 8000)),
  ]);
  try { app?.process()?.kill('SIGKILL'); } catch { /* already gone */ }
  provider.close();
}
