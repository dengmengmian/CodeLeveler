// R5 real Desktop acceptance: supported capabilities only, on the real HOME,
// real provider, real DevOrder workspace and the shared runtime.
//
// Covered: running append / Enter steer, Goal + Plan, Background task card and
// cancel, a real blocked session's history, resume after restart, and the
// explicit refusal of plaintext credentials (Desktop has no /secret surface).
import { _electron as electron } from '../node_modules/playwright/index.mjs';
import assert from 'node:assert/strict';
import { writeFile, mkdir } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import os from 'node:os';

const root = process.env.DESKTOP_ROOT || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/apps/leveler-desktop';
const bin = process.env.LEVELER_BIN || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/target/release/leveler';
const workspace = process.env.LEVELER_WORKSPACE || '/Users/mengmian/Develop/app/csdn/DevOrder';
const out = process.env.R5_EVIDENCE || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/docs/dev/harness-reliability-final-closure-2026-11-26/evidence/desktop/r5';
const blockedSession = '7e9891f7-02af-4e65-8a98-5efcfc891dba';
const env = { ...process.env, LEVELER_BINARY: bin };
delete env.LEVELER_HOME;
delete env.LEVELER_CONFIG_DIR;
delete env.ELECTRON_RUN_AS_NODE;
const evidence = { binary: bin, home: path.join(os.homedir(), '.leveler'), workspace, checks: [] };
let app, page;
const check = (name, ok, detail = '') => {
  evidence.checks.push({ name, ok: !!ok, detail });
  console.log((ok ? 'ok   ' : 'FAIL ') + name + (detail ? ' — ' + detail : ''));
};
const shot = async (name) => {
  await page.screenshot({ path: path.join(out, name + '.png') }).catch(() => {});
};

async function launch() {
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
}

async function closeApp() {
  const exited = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('close timeout')), 15000);
    app.process().once('exit', () => { clearTimeout(timer); resolve(); });
  });
  await page.evaluate(() => window.close());
  await exited;
  app = null;
}

async function selectSession(id) {
  const row = page.locator(`.session-row[data-session-id="${id}"]`);
  await row.waitFor({ state: 'visible', timeout: 60000 });
  await row.click();
  await page.waitForTimeout(600);
}

async function waitIdle(timeout = 300000) {
  await page.waitForFunction(
    () => {
      const stop = document.querySelector('#stop');
      return !stop || stop.hidden === true;
    },
    null,
    { timeout },
  );
  await page.waitForTimeout(900);
}

async function ensureSessionVisible(id, timeout = 30000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    if (await page.locator(`.session-row[data-session-id="${id}"]`).count()) return true;
    const overflow = page.locator(`section.workspace-group[data-workspace-path="${workspace}"] .group-overflow`);
    if (!(await overflow.count())) return false;
    await overflow.first().click();
    await page.waitForTimeout(250);
  }
  return !!(await page.locator(`.session-row[data-session-id="${id}"]`).count());
}

async function waitAnswerCount(atLeast, timeout = 240000) {
  await page.waitForFunction(
    (n) => document.querySelectorAll('.message.assistant .markdown').length >= n,
    atLeast,
    { timeout },
  );
  return page.locator('.message.assistant .markdown').last().innerText();
}

function gitStatus() {
  try {
    return execFileSync('git', ['-C', workspace, 'status', '--porcelain'], { encoding: 'utf8' });
  } catch {
    return '';
  }
}

async function openNewSession() {
  await page.locator('#new-session').click();
  await page.waitForTimeout(500);
  await app.evaluate(({ dialog }, folder) => {
    dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] });
  }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(
    (folder) => document.querySelector('#workspace-picker')?.textContent.includes(folder.split('/').pop()),
    workspace,
    { timeout: 60000 },
  );
  // Attaching a workspace clears the current view and keeps the next send as
  // a draft. The first send (or model pick) creates the workspace session. Do
  // NOT click an existing row: that would continue an older session instead of
  // creating this run's session.
  return null;
}

async function selectModel() {
  const picker = page.getByRole('button', { name: /请选择模型/ });
  if (!(await picker.count())) return true;
  await picker.first().click();
  await page.waitForTimeout(500);
  const option = page.getByText('deepseek-v4.1-flash-1017', { exact: true });
  if (!(await option.count())) return false;
  await option.first().click();
  return true;
}

async function send(text) {
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
}

try {
  await mkdir(out, { recursive: true });
  await launch();
  check('real Electron renders the real Desktop shell', true);
  // Enter while busy means steer for this run, as a user would configure.
  await page.evaluate(() => {
    const key = 'codeleveler.settings.v1';
    const settings = JSON.parse(localStorage.getItem(key) || '{}');
    settings.busySend = 'steer';
    localStorage.setItem(key, JSON.stringify(settings));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await openNewSession();
  check('a real configured model is selected', await selectModel(), 'deepseek/deepseek-v4.1-flash-1017');
  const pickerText = await page.locator('#workspace-picker').innerText().catch(() => '');
  check('the real workspace is attached to the tested session', pickerText.includes(workspace.split('/').pop()), pickerText);

  // ---- Running append / Enter steer -------------------------------------
  const before = await page.locator('.message.assistant .markdown').count();
  await send('请必须调用 run_command 前台运行 `sleep 12`，等命令结束后在回答第一行写 R5-BUSY-OK。不要提前结束。');
  const commandSeen = await page.waitForFunction(
    () => [...document.querySelectorAll('.tool-row')].some((row) => row.textContent.includes('sleep 12')),
    null,
    { timeout: 120000 },
  ).then(() => true).catch(() => false);
  check('the foreground command is visible while the turn runs', commandSeen);
  await page.locator('#message').fill('运行中补充（必须采纳）：完成命令后，在回答的第一行同时写 R5-STEER-OK，并说明你收到了这次追加。');
  await page.locator('#message').press('Enter');
  await page.waitForTimeout(500);
  const toastText = await page.locator('body').innerText().catch(() => '');
  const steerAccepted = toastText.includes('已插话') || toastText.includes('R5-STEER-OK');
  check('the running append is accepted as a steer', steerAccepted, 'steer delivery is confirmed by the answer');
  await page.waitForFunction(
    () => {
      const answers = [...document.querySelectorAll('.message.assistant .markdown')];
      return answers.some((node) => node.textContent.includes('R5-BUSY-OK') && node.textContent.includes('R5-STEER-OK'));
    },
    null,
    { timeout: 300000 },
  ).catch(() => {});
  const steerAnswer = await page.locator('.message.assistant .markdown').last().innerText();
  check('the real model received and answered the running append',
    /R5-BUSY-OK/.test(steerAnswer) && /R5-STEER-OK/.test(steerAnswer), steerAnswer.slice(0, 160));
  const selected = page.locator('.session-row[aria-selected="true"]');
  await selected.waitFor({ state: 'visible', timeout: 60000 });
  const sessionId = await selected.getAttribute('data-session-id');
  check('a new Desktop session reaches the runtime', !!sessionId, String(sessionId));
  evidence.session = sessionId;
  await shot('10-steer');

  // ---- Goal + Plan -------------------------------------------------------
  await waitIdle();
  const modeBefore = await page.locator('#header-preset').innerText().catch(() => '');
  const goalAnswersBefore = await page.locator('.message.assistant .markdown').count();
  const dirtyBefore = gitStatus();
  await send('/goal 只读分析 DevOrder：阅读 README.md 与 package.json，列出 3 条项目事实；先制定计划再执行，不要修改任何文件。');
  await page.waitForTimeout(800);
  await waitIdle(420000);
  const goalText = await page.locator('#conversation').innerText();
  const planRows = await page.locator('.plan-card-row').count();
  const afterGoal = await page.locator('.message.assistant .markdown').last().innerText();
  const goalAnswerCount = await page.locator('.message.assistant .markdown').count();
  const dirtyAfter = gitStatus();
  check(
    'Goal mode starts and presents a plan',
    goalAnswerCount > goalAnswersBefore && (planRows > 0 || /计划/.test(goalText)),
    `rows=${planRows}, preset=${modeBefore}`,
  );
  check('the Goal answer reports the read-only analysis', /README|package\.json|事实/.test(afterGoal), afterGoal.slice(0, 400));
  check('the read-only Goal did not modify the workspace', dirtyBefore === dirtyAfter, dirtyAfter.slice(0, 300));
  await shot('20-goal');

  // ---- Background task card + cancel ------------------------------------
  await waitIdle();
  await send('请用 run_command 启动一个后台任务：`sleep 120`，background=true，background_lifetime=runtime；启动后只回一句“后台已启动”，不要等待它结束。');
  const jobsButton = page.locator('header [aria-label*="后台任务"]').first();
  const jobsVisible = await jobsButton.waitFor({ state: 'visible', timeout: 180000 }).then(() => true).catch(() => false);
  if (jobsVisible) await jobsButton.click();
  await page.waitForTimeout(400);
  const jobSeen = await page.waitForSelector('.job-live-card', { timeout: 30000 }).then(() => true).catch(() => false);
  check('a background task appears in the Desktop job card', jobsVisible && jobSeen);
  if (jobSeen) {
    await page.locator('.job-stop').first().click();
    await page.waitForTimeout(400);
    if (await page.locator('.job-stop-armed').count()) await page.locator('.job-stop').first().click();
    const settled = await page.waitForSelector('.job-settled-card', { timeout: 60000 }).then(() => true).catch(() => false);
    check('the background task can be stopped from the Desktop', settled);
  }
  await shot('30-background');

  // ---- Resume after restart --------------------------------------------
  await closeApp();
  await launch();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));

  // ---- A real blocked session's history --------------------------------
  await page.keyboard.press('Escape');
  await page.mouse.click(640, 400);
  await page.waitForTimeout(300);
  const blockedVisible = await ensureSessionVisible(blockedSession);
  const blockedRow = page.locator(`.session-row[data-session-id="${blockedSession}"]`);
  if (blockedVisible && (await blockedRow.count())) {
    const rowStatus = await blockedRow.getAttribute('data-status');
    await blockedRow.click();
    await page.waitForTimeout(1500);
    const blockedText = await page.locator('#conversation').innerText();
    const status = await page.locator('#conversation-status').innerText().catch(() => '');
    check(
      'the real blocked session opens with its history',
      rowStatus === 'blocked' && (status.includes('受阻') || /受阻|blocked/.test(blockedText)),
      `row=${rowStatus}, status=${status}, tail=${blockedText.slice(-200)}`,
    );
    await shot('40-blocked');
  } else {
    check('the real blocked session opens with its history', false, 'session row not found');
  }

  // ---- A plaintext credential must be reviewed, never silently sent ------
  await selectSession(sessionId);
  const usersBefore = await page.locator('.message.user').count();
  const canary = 'sk-' + 'A'.repeat(40);
  await page.locator('#message').fill(`请使用 DEMO_API_KEY=${canary} 调用服务。`);
  await page.evaluate(() => document.querySelector('#send').click());
  const review = page.locator('.secret-review-modal');
  const reviewed = await review.waitFor({ state: 'visible', timeout: 30000 }).then(() => true).catch(() => false);
  const reviewText = reviewed ? await review.innerText() : '';
  check('a plaintext credential opens the review instead of being sent', reviewed && !reviewText.includes(canary), reviewText.slice(0, 160));
  if (reviewed) await page.getByRole('button', { name: '返回' }).click();
  await page.waitForTimeout(500);
  const usersAfter = await page.locator('.message.user').count();
  check('backing out of the review sends nothing', usersAfter === usersBefore, `${usersBefore} -> ${usersAfter}`);
  await shot('50-refusal');

  // ---- Resume after restart --------------------------------------------
  await page.waitForFunction(
    (id) => !!document.querySelector(`.session-row[data-session-id="${id}"]`),
    sessionId,
    { timeout: 60000 },
  );
  await selectSession(sessionId);
  const resumed = await page.locator('#conversation').innerText();
  check('the session survives an app restart with its history', /R5-BUSY-OK|R5-STEER-OK/.test(resumed));
  await shot('60-resumed');

  evidence.passed = evidence.checks.every((entry) => entry.ok);
  await writeFile(path.join(out, 'r5-real.json'), JSON.stringify(evidence, null, 2));
  console.log(JSON.stringify({ passed: evidence.passed, checks: evidence.checks }, null, 2));
  process.exit(evidence.passed ? 0 : 1);
} catch (error) {
  evidence.passed = false;
  evidence.error = String(error);
  if (page && !page.isClosed()) await shot('failure');
  await writeFile(path.join(out, 'r5-real.json'), JSON.stringify(evidence, null, 2));
  console.error(error);
  process.exit(1);
} finally {
  if (app) await app.close().catch(() => {});
}
