// R1c: model picker lifecycle against the real local HOME, profile and
// Runtime. Only a new test session is created; existing state is read-only.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const evidence = {
  scope: 'real local HOME/profile, real configured providers, real Desktop binary; no isolated HOME; only new test sessions are created',
  binary: { path: leveler, sha256: binarySha },
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
const sourceErrors = async () => (await tasks()).source_errors ?? [];
const openPicker = async () => {
  await page.locator('#model-picker').click();
  await page.waitForSelector('.menu');
};
const closePicker = async () => { await page.keyboard.press('Escape'); await page.waitForFunction(() => !document.querySelector('.menu')); };
const selectFirstModel = async () => {
  const item = page.locator('.menu .menu-item').filter({ has: page.locator('.menu-label') }).first();
  const label = (await item.innerText()).trim().split('\n')[0];
  await item.click();
  await page.waitForFunction(() => !document.querySelector('.menu'));
  return label;
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');

try {
  await launch();
  // The real bridge must be the binary this run named, not a shared target copy.
  const ps = execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
  const bridge = ps.split('\n').find(line => line.includes(`${leveler} desktop-bridge`));
  assert.ok(bridge, `the running bridge must be ${leveler}`);
  evidence.binary.bridge_process = bridge.trim();
  assert.equal(await page.evaluate(() => document.body.classList.contains('home')), true, 'the real profile must start at the home surface for the no-session scenarios');

  const baseline = await taskCount();
  const baselineErrors = await sourceErrors();
  assert.equal(baselineErrors.length, 0, `existing task sources must stay readable: ${JSON.stringify(baselineErrors)}`);
  evidence.baseline_tasks = baseline;

  await openPicker();
  assert.equal(await page.locator('.menu .menu-item').count() > 0, true, 'the real provider configuration must list models');
  assert.equal(await taskCount(), baseline, 'opening the picker must not create a session');
  evidence.open_picker_creates_no_session = true;

  await page.keyboard.press('ArrowDown');
  await page.keyboard.press('ArrowUp');
  assert.equal(await taskCount(), baseline, 'browsing the picker must not create a session');
  await closePicker();
  assert.equal(await taskCount(), baseline, 'closing the picker must not create a session');
  evidence.browse_and_close_create_no_session = true;

  await openPicker();
  const chosen = await selectFirstModel();
  assert.match(chosen, /\S/);
  assert.equal(await taskCount(), baseline, 'selecting a model must not create a session');
  assert.ok((await page.locator('#model-picker').innerText()).includes(chosen.split(/\s/)[0]), `the pending model must be shown: ${chosen}`);
  evidence.pending_model_without_session = { chosen };
  evidence.select_without_send_creates_no_session = true;

  // First send creates exactly one session and pins the chosen model. This is
  // a real configured provider/model request.
  const beforeSend = await taskCount();
  await page.locator('#message').fill('请只回复：R1C_OK');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'), null, { timeout: 30000 });
  const created = await sessionId();
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), created);
  assert.ok(snapshot.model, `the created session must carry a model: ${JSON.stringify(snapshot.model)}`);
  assert.equal(await taskCount(), beforeSend + 1, 'the first send must create exactly one session');
  evidence.created_session = { id: created, model: snapshot.model };
  try {
    await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('R1C_OK'), null, { timeout: 180000 });
    evidence.real_model_reply = 'PASS';
  } catch (error) {
    const status = await page.locator('#conversation-status').textContent();
    const errorText = await page.locator('#error-banner').innerText().catch(() => '');
    evidence.real_model_reply = { state: 'BLOCKED', status, error: errorText || error.message };
  }

  // Existing session model switch must not add a session.
  const beforeSwitch = await taskCount();
  await openPicker();
  await selectFirstModel();
  assert.equal(await taskCount(), beforeSwitch, 'switching a model on an existing session must not create a session');
  evidence.existing_session_switch_creates_no_session = true;
  await page.waitForTimeout(500);
  assert.equal(await taskCount(), beforeSwitch);

  // Restart: no ghost session from the picker, and the existing tasks stay
  // fully readable.
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  const beforeRestart = beforeSwitch;
  await launch();
  const afterRestart = await taskCount();
  assert.equal(afterRestart, beforeRestart, 'restarting must not create or lose sessions');
  assert.equal((await sourceErrors()).length, 0, 'the real task index must stay readable after restart');
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), created);
  await page.locator(`.session-row[data-session-id="${created}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  assert.equal(await page.evaluate(id => window.desktop.snapshot(id).then(s => s.id), created), created);
  evidence.restart_no_ghost_and_history_readable = true;

  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'model-picker-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'model-picker-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'model-picker-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
