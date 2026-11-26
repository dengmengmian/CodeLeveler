// R2c real-HOME viewer check on an already-recorded session.
//
// No model turn: this opens the newest real dogfood session (whose workspace
// is under /tmp/leveler-r2c-dogfood), expands its execution groups, and reads
// the recorded full output / delivery panel through the real Desktop. It
// complements the dogfood script, whose model turn is the expensive part.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const evidence = {
  scope: 'real local HOME/profile, existing real session; no model turn',
  binary: { path: leveler, sha256: createHash('sha256').update(readFileSync(leveler)).digest('hex') },
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
  page.setDefaultTimeout(60000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
};
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 120000 });
async function expandExecutionGroups() {
  const groups = page.locator('.execution-group');
  const count = await groups.count();
  for (let index = 0; index < count; index += 1) {
    const group = groups.nth(index);
    if ((await group.getAttribute('open')) !== null) continue;
    const toggle = group.locator('.execution-toggle');
    if (await toggle.count()) await toggle.click().catch(() => {});
    else await group.locator(':scope > summary').click().catch(() => {});
    await page.waitForTimeout(30);
  }
  const rows = page.locator('.tool-row');
  const rowCount = await rows.count();
  for (let index = 0; index < rowCount; index += 1) {
    const row = rows.nth(index);
    if ((await row.getAttribute('open')) !== null) continue;
    await row.locator(':scope > summary').click().catch(() => {});
  }
}
async function capture(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
}

try {
  await launch();
  const tasks = (await page.evaluate(() => window.desktop.listTasks({ include_archived: true }))).tasks ?? [];
  // The failed dogfood attempts archived their own sessions; archived tasks
  // are still the real record and remain openable by identity.
  const candidates = tasks.filter(task => String(task.primary_workspace ?? '').includes('leveler-r2c-dogfood')
    || String(task.title ?? '').includes('R2C_DOGFOOD'));
  evidence.task_samples = tasks.slice(0, 5).map(task => ({ title: task.title, workspace: task.primary_workspace, archived: task.archived }));
  assert.ok(candidates.length > 0, 'no real R2c dogfood session was found');
  let session = null;
  let actionCount = 0;
  const checked = [];
  for (const candidate of candidates.slice(0, 8)) {
    await page.evaluate(task => window.desktop.openTask({ id: task.id, source_id: task.source_id }), candidate);
    await idle();
    const hasRows = await page.waitForFunction(
      () => document.querySelectorAll('.tool-row, .execution-group').length > 0,
      null,
      { timeout: 60000 },
    ).then(() => true).catch(() => false);
    if (!hasRows) { checked.push({ id: candidate.id.slice(0, 8), rows: false }); continue; }
    await expandExecutionGroups();
    await page.waitForTimeout(200);
    const count = await page.locator('.tool-row[data-kind="command"]').filter({ has: page.locator('.tool-output-action') }).count();
    checked.push({ id: candidate.id.slice(0, 8), workspace: candidate.primary_workspace, actions: count });
    if (count > 0) { session = candidate; actionCount = count; break; }
  }
  evidence.steps.checked = checked;
  assert.ok(session, 'no recorded session rendered a bounded, recoverable command output');
  evidence.session = { id: session.id.slice(0, 8), workspace: session.primary_workspace, title: session.title };
  evidence.steps.actions = actionCount;
  const actionRows = page.locator('.tool-row[data-kind="command"]').filter({ has: page.locator('.tool-output-action') });

  let chosen = null;
  for (let index = 0; index < actionCount; index += 1) {
    const row = actionRows.nth(index);
    if ((await row.innerText()).includes('seq 1 40000')) { chosen = row; break; }
  }
  chosen ??= actionRows.first();
  const rowText = await chosen.innerText();
  const isSeq = rowText.includes('seq 1 40000');
  if ((await chosen.getAttribute('open')) === null) await chosen.locator(':scope > summary').click();
  const previewText = await chosen.locator('.tool-terminal-output').innerText();
  evidence.steps.preview_bytes = previewText.length;
  await chosen.locator('.tool-output-action button', { hasText: '查看完整结果' }).click();
  await page.waitForSelector('.tool-output-view', { timeout: 30000 });
  await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
  const modelResult = await page.locator('.tool-output-pre').innerText();
  evidence.steps.model_result_bytes = modelResult.length;
  const reference = page.locator('.tool-output-ref', { hasText: 'stdout' }).first();
  evidence.steps.artifact_offered = (await reference.count()) > 0;
  if (await reference.count()) {
    await reference.click();
    await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
  }
  let loaded = await page.locator('.tool-output-pre').innerText();
  let pages = 1;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    const more = page.locator('.tool-output-footer button', { hasText: '加载更多' });
    if (!(await more.count())) break;
    const before = loaded.length;
    await more.click();
    await page.waitForFunction(
      length => (document.querySelector('.tool-output-pre')?.textContent ?? '').length > length,
      before,
      { timeout: 20000 },
    );
    loaded = await page.locator('.tool-output-pre').innerText();
    pages += 1;
  }
  assert.ok(loaded.length > previewText.length, `recorded output ${loaded.length} > preview ${previewText.length}`);
  if (isSeq) {
    assert.ok(loaded.split('\n').length >= 40000, `40000-line output: ${loaded.split('\n').length}`);
    assert.ok(loaded.trimEnd().endsWith('40000'), 'the real last line');
  }
  evidence.steps.full_output = { is_seq: isSeq, pages, loaded_bytes: loaded.length, lines: loaded.split('\n').length, tail: loaded.trimEnd().slice(-20) };
  await capture('r2c-real-viewer.png');

  // The delivery panel for the same session.
  const editGroup = page.locator('.execution-group[data-variant="edit"]').last();
  if (await editGroup.count()) {
    await editGroup.locator(':scope > summary').click();
  } else {
    const editRow = page.locator('.tool-row[data-kind="edit"]').first();
    assert.ok(await editRow.count(), 'an edit row exists');
    if ((await editRow.getAttribute('open')) === null) await editRow.locator(':scope > summary').click();
    await editRow.locator('button', { hasText: '查看完整变更' }).first().click();
  }
  await page.waitForSelector('.turn-delivery', { timeout: 30000 });
  const delivery = await page.locator('.turn-delivery').innerText();
  evidence.steps.delivery = delivery.slice(0, 600);
  assert.match(delivery, /本轮执行/);
  assert.match(delivery, /calc\.js/);
  assert.match(delivery, /test\.js/);
  assert.match(delivery, /退出码/);
  await capture('r2c-real-delivery.png');

  // The real failing command is visible with its real code or observed code.
  const commands = await page.locator('.turn-delivery-command').allInnerTexts();
  evidence.steps.commands = commands;
  assert.ok(commands.some(item => /退出码 4/.test(item)), `a real exit code 4 is shown: ${commands}`);
  assert.ok(commands.some(item => /node\s+test\.js/.test(item)), `node test.js is listed: ${commands}`);
  assert.deepEqual(errors, [], 'no renderer page errors');
  evidence.result = 'PASS';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r2c-real-viewer-failure.png').catch(() => {});
  throw error;
} finally {
  await writeFile(path.join(output, 'r2c-real-viewer-evidence.json'), JSON.stringify(evidence, null, 2));
  await Promise.race([app?.close().catch(() => {}), new Promise(resolve => setTimeout(resolve, 8000))]);
  try { app?.process()?.kill('SIGKILL'); } catch { /* already gone */ }
}
