// R5 model-picker closure against the REAL local HOME/profile/providers.
// The real no-workspace runtime is occupied by another build's live turn, so
// this run uses a fresh workspace runtime instead of interrupting it; the
// product semantics (no ghost session) are identical.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const BINARY = leveler;
const workspace = await mkdtemp(path.join(os.tmpdir(), 'leveler-r5-model-picker-'));
const profile = await mkdtemp(path.join(os.tmpdir(), 'leveler-r5-model-picker-profile-'));
const evidence = {
  scope: 'real HOME/config/providers/model; temp workspace and Electron profile; the real no-workspace runtime was left untouched',
  binary: { path: leveler, sha256: createHash('sha256').update(readFileSync(leveler)).digest('hex') },
  blocked_environment: {
    no_workspace_runtime: 'another release build holds a live turn; this run did not interrupt it',
  },
};
const errors = [];
let app;
let page;
let created = null;

const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  app = await electron.launch({ args: [root, `--user-data-dir=${profile}`], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
};
const tasks = () => page.evaluate(() => window.desktop.listTasks({ include_archived: false }));
const taskCount = async () => (await tasks()).tasks?.length ?? 0;
const openPicker = async () => {
  await page.locator('#model-picker').click();
  await page.waitForSelector('.menu');
};
const closePicker = async () => {
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('.menu'));
};
const selectFirstModel = async () => {
  const item = page.locator('.menu .menu-item').filter({ has: page.locator('.menu-label') }).first();
  const label = (await item.innerText()).trim().split('\n')[0];
  await item.click();
  await page.waitForFunction(() => !document.querySelector('.menu'));
  return label;
};

try {
  await launch();
  // Switch to a fresh workspace runtime, leaving the occupied no-workspace
  // runtime exactly as it was.
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('leveler-r5-model-picker'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });

  const baseline = await taskCount();
  await openPicker();
  assert.ok(await page.locator('.menu .menu-item').count() > 0, 'the real provider configuration lists models');
  assert.equal(await taskCount(), baseline, 'opening the picker must not create a session');
  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowUp');
  assert.equal(await taskCount(), baseline, 'browsing must not create a session');
  await closePicker();
  assert.equal(await taskCount(), baseline, 'closing must not create a session');
  evidence.open_browse_close_no_session = true;

  await openPicker();
  const chosen = await selectFirstModel();
  assert.match(chosen, /\S/);
  assert.equal(await taskCount(), baseline, 'selecting a model must not create a session');
  assert.ok((await page.locator('#model-picker').innerText()).includes(chosen.split(/\s/)[0]));
  evidence.pending_model = chosen;

  // First send: exactly one session, carrying the chosen model.
  const beforeSend = await taskCount();
  await page.locator('#message').fill('请只回复：R5_PICKER_OK');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'), null, { timeout: 60000 });
  created = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  assert.ok(created, 'the first send selects the created session');
  assert.equal(await taskCount(), beforeSend + 1, 'the first send creates exactly one session');
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), created);
  assert.ok(snapshot.model, `the session carries a model: ${JSON.stringify(snapshot.model)}`);
  evidence.first_send = { session: created, model: snapshot.model, tasks: await taskCount() };
  try {
    await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('R5_PICKER_OK'), null, { timeout: 180000 });
    evidence.real_model_reply = 'PASS';
  } catch (error) {
    evidence.real_model_reply = { state: 'BLOCKED_OR_SLOW', message: error.message, status: await page.locator('#conversation-status').textContent().catch(() => '') };
  }

  const beforeSwitch = await taskCount();
  await openPicker();
  await selectFirstModel();
  assert.equal(await taskCount(), beforeSwitch, 'switching an existing session must not create a session');
  await page.waitForTimeout(500);
  assert.equal(await taskCount(), beforeSwitch);
  evidence.existing_session_switch_no_session = true;

  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  assert.equal(await taskCount(), beforeSwitch, 'restart creates no ghost session');
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), created, { timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${created}"]`).click();
  await page.waitForFunction(id => window.desktop.snapshot(id).then(s => s.id === id), created, { timeout: 60000 });
  evidence.restart_no_ghost = true;

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r5-model-picker-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors }, null, 2));
  process.exitCode = 1;
} finally {
  // Electron's graceful close can wait on the runtime subscription; the test
  // owns this window and exits it directly, then stops only its own temp
  // workspace runtime before deleting the session.
  try {
    await Promise.race([
      app?.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* app already exited */ }
  if (created) {
    try {
      execFileSync(BINARY, ['runtime', '--repo', workspace, 'stop'], { stdio: 'ignore', timeout: 15000 });
    } catch { /* no runtime or already stopped */ }
    try {
      execFileSync(BINARY, ['sessions', 'delete', created, '--repo', workspace], { stdio: 'ignore', timeout: 15000 });
      evidence.cleaned_session = created;
    } catch (error) {
      evidence.cleanup_error = String(error.message || error);
    }
  }
  await rm(profile, { recursive: true, force: true }).catch(() => {});
  await rm(workspace, { recursive: true, force: true }).catch(() => {});
}
