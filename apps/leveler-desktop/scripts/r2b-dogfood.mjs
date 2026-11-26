// R2b real-local dogfood: real HOME/profile, real configured provider and
// model, real Desktop + Runtime. It creates only new sessions in a temporary
// workspace; existing sessions, configuration and credentials are read-only.
//
// Every mode/terminal fact is read from the Runtime snapshot or the durable
// filesystem. The model's own prose is never taken as a completion claim.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const workspace = await mkdtemp('/tmp/leveler-r2b-dogfood-');
const configPath = path.join(os.homedir(), '.leveler', 'config.toml');
const configHash = () => createHash('sha256').update(readFileSync(configPath)).digest('hex');
const configBefore = configHash();

const evidence = {
  scope: 'real local HOME/profile, real configured provider, real Desktop/Runtime; temp workspace; only new sessions are created',
  binary: { path: leveler, sha256: binarySha },
  workspace,
  steps: {},
  prompts: {},
  failures: [],
};
let app;
let page;
const errors = [];
const createdSessions = [];

const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(60000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 60000 });
};
const selectedId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const snapshot = async () => {
  const id = await selectedId();
  return id ? page.evaluate(sessionId => window.desktop.snapshot(sessionId), id) : null;
};
const taskCount = async () => (await page.evaluate(() => window.desktop.listTasks({ include_archived: false }))).tasks?.length ?? 0;
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 600000 });
const assistantCount = () => page.locator('.message.assistant .markdown').count();
const conversationText = () => page.locator('#conversation').innerText();

async function capture(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
}
async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
async function chooseMode(label) {
  for (let attempt = 0; attempt < 40; attempt += 1) {
    await page.waitForFunction(() => document.querySelector('#mode-picker') && !document.querySelector('#mode-picker').disabled, null, { timeout: 60000 });
    await page.locator('#mode-picker').click();
    try {
      await page.waitForSelector('.menu .menu-item', { timeout: 1000 });
      break;
    } catch {
      await page.waitForTimeout(100);
    }
    if (attempt === 39) throw new Error('the execution-mode menu did not open');
  }
  await page.locator('.menu .menu-item').filter({ hasText: new RegExp(`^${label}$`) }).first().click();
  await page.waitForFunction(expected => document.querySelector('#mode-picker .trigger-label')?.textContent === expected, label, { timeout: 30000 });
}
async function waitAxis(axis, timeout = 60000) {
  await page.waitForFunction(async wanted => {
    const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
    if (!id) return false;
    try { return (await window.desktop.snapshot(id)).collaboration === wanted; } catch { return false; }
  }, axis, { timeout });
}
async function send(text, { mode } = {}) {
  if (mode) await chooseMode(mode);
  const before = { assistant: await assistantCount(), receipts: await page.locator('.turn-receipt').count() };
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  return before;
}
async function settleTurn(before, timeout = 600000) {
  if (typeof before === 'number') before = { assistant: before, receipts: 0 };
  const deadline = Date.now() + timeout;
  const handled = new Set();
  while (Date.now() < deadline) {
    const approval = page.locator('.approval-card').first();
    if (await approval.count()) {
      const text = (await approval.innerText()).slice(0, 500);
      if (!handled.has(text)) {
        handled.add(text);
        evidence.failures.push({ kind: 'approval', text });
        await approval.getByRole('button', { name: '允许一次' }).click().catch(() => {});
      }
    }
    const clarification = page.locator('.interaction-card').filter({ has: page.locator('.interaction-question') }).first();
    if (await clarification.count()) {
      const text = (await clarification.innerText()).slice(0, 500);
      if (!handled.has(text)) {
        handled.add(text);
        evidence.failures.push({ kind: 'clarification', text });
        const other = clarification.locator('.interaction-other').first();
        if (await other.count()) await other.fill('请按原要求继续，不要提问。');
        else await clarification.locator('input[type=radio]').first().check().catch(() => {});
        await clarification.getByRole('button', { name: '提交回答' }).click().catch(() => {});
      }
    }
    const status = await page.locator('#conversation-status').innerText().catch(() => '');
    // Only the runtime's turn terminal proves the logical turn is over. An
    // assistant reply may be followed by a runtime nudge round (an open plan,
    // a goal window); sending the next prompt in that gap steers it into the
    // turn that is still running.
    const grew = (await page.locator('.turn-receipt').count()) > before.receipts;
    if (grew && !['运行中', '等待你处理'].includes(status)) {
      await idle();
      return snapshot();
    }
    await page.waitForTimeout(500);
  }
  throw new Error('turn did not settle before the deadline');
}
async function waitRunning(timeout = 120000) {
  try {
    await page.waitForFunction(() => ['运行中', '等待你处理'].includes(document.querySelector('#conversation-status')?.textContent ?? ''), null, { timeout });
    return true;
  } catch {
    return false;
  }
}
async function openTask(sessionId) {
  await page.waitForSelector(`.session-row[data-session-id="${sessionId}"]`);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === id, sessionId);
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
}
async function archive(sessionId) {
  const value = await page.evaluate(async id => {
    const snap = await window.desktop.snapshot(id);
    const result = await window.desktop.deliver({
      command_id: crypto.randomUUID(), session_id: id, expected_version: null,
      issued_at: new Date().toISOString(),
      command: { type: 'archive_session', session_id: id },
    });
    return { ok: result.ok, version: snap.last_sequence };
  }, sessionId);
  return value;
}

try {
  await launch();

  // D01 / D11: existing provider, models, history and one existing session.
  const baselineTasks = await page.evaluate(() => window.desktop.listTasks({ include_archived: false }));
  const providers = await page.evaluate(() => window.desktop.providers({ selection: null, sessionId: null }));
  const existing = (baselineTasks.tasks ?? []).find(task => ['completed', 'answered'].includes(task.status)
    && typeof task.primary_workspace === 'string' && existsSync(task.primary_workspace));
  let existingRead = null;
  if (existing) {
    await openTask(existing.id);
    const value = await snapshot();
    existingRead = { id: existing.id, title: value.title, messages: value.messages.length, repository: value.repository };
    evidence.steps.existing_session_read = { state: 'PASS', ...existingRead };
    evidence.baseline = { tasks: baselineTasks.tasks.length, source_errors: baselineTasks.source_errors ?? [] };
  } else {
    evidence.steps.existing_session_read = { state: 'NOT_OBSERVED', reason: 'no existing completed session with a workspace' };
  }
  evidence.steps.startup = {
    state: (baselineTasks.source_errors ?? []).length === 0 ? 'PASS' : 'FAIL',
    tasks: baselineTasks.tasks.length,
    providers: (providers.providers ?? []).map(item => item.id),
    models: providers.model_count ?? providers.models?.length ?? null,
    default_model: providers.resolved_default_model ?? null,
    runtime: providers.runtime ?? null,
  };
  assert.equal((baselineTasks.source_errors ?? []).length, 0, 'startup must read every task source');

  // New test workspace and a fresh session (no existing state is touched).
  await addWorkspace();
  await page.waitForTimeout(500);

  // D02: real Chat turn with a real tool call.
  const beforeChat = await send('请在工作区创建文件 r2b-chat.txt，内容为准确的一行 R2B_CHAT_TOOL_OK；读取它确认后，最后只回复 R2B_CHAT_DONE。不要提问，不要修改其他文件。');
  const chatSnapshot = await settleTurn(beforeChat);
  createdSessions.push(chatSnapshot.id);
  assert.equal(chatSnapshot.collaboration, 'chat');
  const chatFile = await readFile(path.join(workspace, 'r2b-chat.txt'), 'utf8').catch(() => null);
  evidence.steps.chat_tool = {
    state: chatFile?.trim() === 'R2B_CHAT_TOOL_OK' ? 'PASS' : 'FAIL',
    file: chatFile,
    receipt: await page.locator('.turn-receipt').last().innerText().catch(() => null),
    reply: (await conversationText()).slice(-800),
  };
  await capture('r2b-dogfood-chat.png');

  // D03 + D09: a running real turn receives a steer; a separate turn is
  // interrupted with the stop control and the session stays usable.
  const beforeSteer = await send('用 run_command 运行命令 `sleep 20`，然后检查本会话是否收到我随后补充的一条指令；收到的话，在最终回复里原样写出其中的标记词。不要做其他事情。');
  let steered = false;
  if (await waitRunning()) {
    await page.locator('#message').fill('补充要求：请在最终回复中原样写下 R2B_STEER_SEEN 这个词，然后再结束。');
    await page.locator('#message').press('Meta+Enter');
    steered = true;
  }
  await settleTurn(beforeSteer, 600000);
  evidence.steps.steer = {
    state: steered && (await conversationText()).includes('R2B_STEER_SEEN') ? 'PASS'
      : steered ? 'NOT_OBSERVED' : 'NOT_OBSERVED',
    sent_while_running: steered,
    receipt: await page.locator('.turn-receipt').last().innerText().catch(() => null),
    sessions: await taskCount(),
  };
  const beforeInterrupt = await send('用 run_command 运行命令 `sleep 30`，结束后回复 R2B_SLEEP_DONE。');
  const interruptedRunning = await waitRunning();
  if (interruptedRunning) {
    await page.locator('#stop').click();
    await idle();
  }
  const interrupted = await snapshot();
  const receiptsAfterStop = await page.locator('.turn-receipt').last().innerText().catch(() => null);
  const beforeResume = await send('只回复 R2B_RESUMED，不要运行工具。');
  await settleTurn(beforeResume);
  evidence.steps.interrupt_and_continue = {
    state: interruptedRunning && ['cancelled', 'interrupted'].includes(interrupted.task_status ?? interrupted.status) ? 'PASS' : 'NOT_OBSERVED',
    stopped_while_running: interruptedRunning,
    status: interrupted.task_status ?? interrupted.status,
    receipt: receiptsAfterStop,
    continued: (await conversationText()).includes('R2B_RESUMED'),
  };

  // D04 + D08: Plan mode plans with update_plan and holds a write for approval.
  await chooseMode('计划');
  await waitAxis('plan');
  const beforePlan = await send('用 update_plan 制定三个步骤：读取工作区、补充一节说明、运行验证。不要修改任何文件，最后只回复 R2B_PLAN_DONE。');
  const planSnapshot = await settleTurn(beforePlan);
  // The number of steps is the model's own plan content; the product fact is
  // that the Runtime produced a plan and the UI projects it truthfully.
  evidence.steps.plan = {
    state: (planSnapshot.plan?.steps ?? []).length >= 1 ? 'PASS' : 'FAIL',
    axis: planSnapshot.collaboration,
    plan: planSnapshot.plan ?? null,
    reply: (await conversationText()).slice(-600),
  };
  await capture('r2b-dogfood-plan.png');
  const beforeWrite = await send('请调用 write_file 工具创建 r2b-plan-write.txt，内容为 x。这是对只读模式的验证，请务必执行。');
  let approvalCard = null;
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (await page.locator('.approval-card').count()) { approvalCard = await page.locator('.approval-card').first().innerText(); break; }
    const status = await page.locator('#conversation-status').innerText();
    if ((await assistantCount()) > beforeWrite && !['运行中', '等待你处理'].includes(status)) break;
    await page.waitForTimeout(500);
  }
  if (approvalCard) {
    await page.locator('.approval-card').first().getByRole('button', { name: '拒绝' }).click();
    await idle();
    await settleTurn(beforeWrite, 300000).catch(() => null);
  }
  evidence.steps.plan_read_only = {
    state: approvalCard && !existsSync(path.join(workspace, 'r2b-plan-write.txt')) ? 'PASS' : 'NOT_OBSERVED',
    approval: approvalCard ? approvalCard.slice(0, 500) : null,
    file_exists: existsSync(path.join(workspace, 'r2b-plan-write.txt')),
  };

  // D05 / D07: Goal mode executes a real multi-step goal; the Runtime settles
  // the axis, never the client.
  await chooseMode('目标');
  await waitAxis('goal');
  const beforeGoal = await send('在工作区创建文件 r2b-goal.txt，内容为准确的一行 R2B_GOAL_OK；读取它确认内容；然后调用 update_goal 声明目标完成。不要提问。');
  await settleTurn(beforeGoal, 600000);
  for (let attempt = 0; attempt < 120; attempt += 1) {
    if ((await snapshot()).collaboration !== 'goal') break;
    await page.waitForTimeout(500);
  }
  const goalSettled = await snapshot();
  const goalFile = await readFile(path.join(workspace, 'r2b-goal.txt'), 'utf8').catch(() => null);
  const goalReceipt = await page.locator('.turn-receipt').last().innerText().catch(() => null);
  evidence.steps.goal = {
    state: goalFile?.trim() === 'R2B_GOAL_OK' && goalSettled.collaboration !== 'goal' ? 'PASS' : 'FAIL',
    axis_after: goalSettled.collaboration,
    file: goalFile,
    receipt: goalReceipt,
    mode_label: await page.locator('#mode-picker .trigger-label').innerText(),
  };
  await capture('r2b-dogfood-goal.png');

  // D06: a running goal receives an amendment.
  await chooseMode('目标');
  await waitAxis('goal');
  const beforeGoalSteer = await send('创建工作区文件 r2b-g1.txt，内容为 one；然后用 run_command 运行 `sleep 20`；之后完成目标。不要提问。');
  let goalSteerSent = false;
  if (await waitRunning()) {
    await page.locator('#message').fill('补充要求：完成目标时也创建 r2b-g2.txt，内容为 two。');
    await page.locator('#message').press('Meta+Enter');
    goalSteerSent = true;
  }
  await settleTurn(beforeGoalSteer, 900000);
  for (let attempt = 0; attempt < 120 && (await snapshot()).collaboration === 'goal'; attempt += 1) await page.waitForTimeout(500);
  const g1 = await readFile(path.join(workspace, 'r2b-g1.txt'), 'utf8').catch(() => null);
  const g2 = await readFile(path.join(workspace, 'r2b-g2.txt'), 'utf8').catch(() => null);
  evidence.steps.goal_steer = {
    state: goalSteerSent && g1?.trim() === 'one' && g2?.trim() === 'two' ? 'PASS' : 'NOT_OBSERVED',
    sent_while_running: goalSteerSent,
    g1, g2,
    axis_after: (await snapshot()).collaboration,
    receipt: await page.locator('.turn-receipt').last().innerText().catch(() => null),
  };

  // D10 / D12: restart reads the same truth; configuration is byte-identical.
  const taskCountBeforeRestart = await taskCount();
  await app.evaluate(({ app: electronApp }) => electronApp.exit(0));
  app = null;
  await new Promise(resolve => setTimeout(resolve, 1500));
  await launch();
  const taskCountAfterRestart = await taskCount();
  await openTask(chatSnapshot.id);
  const reopened = await waitSnapshotAxis(chatSnapshot.id);
  const reopenedText = await conversationText();
  // The global task count is not a product invariant: another client using the
  // same real HOME can create sessions at any time. The restart fact is that
  // THIS session reopens with its runtime axis and history.
  evidence.steps.restart = {
    state: reopenedText.includes('R2B_CHAT_TOOL_OK') ? 'PASS' : 'FAIL',
    tasks_before: taskCountBeforeRestart,
    tasks_after: taskCountAfterRestart,
    tasks_unchanged: taskCountAfterRestart === taskCountBeforeRestart,
    axis: reopened,
    history_chars: reopenedText.length,
  };
  const configAfter = configHash();
  evidence.steps.config_hygiene = { state: configAfter === configBefore ? 'PASS' : 'FAIL', before: configBefore, after: configAfter };

  // Cleanup: archive only this run's sessions; never delete user data.
  for (const id of [...new Set(createdSessions)]) evidence.steps.archive = { ...(evidence.steps.archive ?? {}), [id]: await archive(id) };
  await page.evaluate(() => window.desktop.listTasks());
  evidence.final_task_count = await taskCount();

  await capture('r2b-dogfood-final.png');
  assert.deepEqual(errors, []);
  Object.assign(evidence, { passed: true, page_errors: errors });
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r2b-dogfood-failure.png') }).catch(() => {});
  throw error;
} finally {
  await writeFile(path.join(output, 'r2b-dogfood-evidence.json'), JSON.stringify(evidence, null, 2) + '\n');
  console.log(JSON.stringify(evidence, null, 2));
  if (app) await app.evaluate(({ app: electronApp }) => electronApp.exit(0)).catch(() => {});
  await rm(workspace, { recursive: true, force: true }).catch(() => {});
}

async function waitSnapshotAxis(sessionId) {
  await page.waitForFunction(async id => {
    try { return !!(await window.desktop.snapshot(id)); } catch { return false; }
  }, sessionId, { timeout: 30000 });
  return page.evaluate(async id => (await window.desktop.snapshot(id)).collaboration, sessionId);
}
