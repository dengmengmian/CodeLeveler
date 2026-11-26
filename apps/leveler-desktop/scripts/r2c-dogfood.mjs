// R2c real-local dogfood: the current machine's real HOME/profile.
//
// Real configured provider and model, real Desktop, real daemon, real
// workspace and real history. Only new sessions are created in a temporary
// workspace; existing sessions and the configuration are read-only, and the
// config file is hashed before and after. Every asserted fact comes from the
// Runtime snapshot, durable history, the real tool execution or a real file.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash, randomUUID } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import os from 'node:os';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const head = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: path.join(root, '../..') }).toString().trim();
const workspace = await mkdtemp('/tmp/leveler-r2c-dogfood-');
const configPath = path.join(os.homedir(), '.leveler', 'config.toml');
const configHash = () => createHash('sha256').update(readFileSync(configPath)).digest('hex');
const configBefore = configHash();

// A tiny real project with a real bug for the model to fix.
await writeFile(path.join(workspace, 'calc.js'), [
  "'use strict';",
  'function add(a, b) {',
  '  return a - b;',
  '}',
  'module.exports = { add };',
  '',
].join('\n'));

const evidence = {
  scope: 'real local HOME/profile, real provider/model, real Desktop/Runtime; temp workspace; only new sessions',
  binary: { path: leveler, sha256: binarySha },
  head,
  workspace,
  config_sha256_before: configBefore,
  steps: {},
  failures: [],
};
let app;
let page;
const errors = [];
const created = [];

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
}, null, { timeout: 900000 });
const receipts = () => page.locator('.turn-receipt').count();
async function waitTurn(before) {
  await page.waitForFunction(count => document.querySelectorAll('.turn-receipt').length >= count, before + 1, { timeout: 900000 });
  await idle();
}
async function capture(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
}
// Collapsed execution rounds paint no child rows; the conversation opens
// them on demand. Opening is the reader's own gesture, not a test shortcut.
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
  // A tool's details (and therefore its full-output action) are painted only
  // while that row is open; opening the rows is the reader's own gesture.
  const rows = page.locator('.tool-row');
  const rowCount = await rows.count();
  for (let index = 0; index < rowCount; index += 1) {
    const row = rows.nth(index);
    if ((await row.getAttribute('open')) !== null) continue;
    await row.locator(':scope > summary').click().catch(() => {});
  }
}

async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
async function archive(sessionId) {
  await page.evaluate(id => window.desktop.deliver({
    command_id: crypto.randomUUID(),
    session_id: id,
    expected_version: null,
    issued_at: new Date().toISOString(),
    command: { type: 'archive_session', session_id: id },
  }), sessionId);
}

try {
  await launch();
  const tasksBefore = await taskCount();
  assert.ok(tasksBefore > 0, 'the real profile already has sessions');
  const models = await page.evaluate(() => window.desktop.models(null)).catch(() => null);
  evidence.steps.D01_startup = { tasks: tasksBefore, model_picker: true, models_status: models ? 'ok' : 'unavailable' };
  await capture('r2c-01-startup.png');

  // D15: the real configured model is the one the Runtime reports.
  await addWorkspace();
  const before = await receipts();
  await page.locator('#message').fill([
    'R2C_DOGFOOD 这是真实工作区。请完成：',
    '1) 读取 calc.js；2) 修复 add 的错误；3) 新建 test.js，用 node 断言 add(2,3)===5 与 add(-1,1)===0；',
    '4) 运行 node test.js；5) 运行 node -e "console.error(\'EXPECTED_FAIL\');process.exit(4)"，确认它以 4 退出；',
    '6) 运行 seq 1 40000 查看大量输出；7) 最后说明改了什么、测试结果、哪些未验证。不要提问。',
  ].join('\n'));
  await page.locator('#send').click();
  await waitTurn(before);
  const sessionId = await selectedId();
  created.push(sessionId);
  // The runtime bridge is installed by the first connect (session creation),
  // so runtime identity is read once a session owns it.
  const info = await page.evaluate(() => window.desktop.runtimeInfo());
  evidence.runtime = { version: info.version, build: info.build?.revision ?? null, pid: info.pid };
  const state = await snapshot();
  evidence.steps.D02_model = { model: state?.model?.model ?? null, provider: state?.model?.provider ?? null };

  // D06/D07/D08 must run before any focused follow-up: the delivery panel
  // projects the NEWEST turn, and the main task is the newest turn right now.
  // D06/D07/D08: the changes view carries the turn's real delivery facts.
  await expandExecutionGroups();
  const editGroup = page.locator('.execution-group[data-variant="edit"]').last();
  if (await editGroup.count()) {
    // The edit round's own summary opens the confirmed changes.
    await editGroup.locator(':scope > summary').click();
  } else {
    const editRow = page.locator('.tool-row[data-kind="edit"]').first();
    if (!(await editRow.count())) throw new Error('the model did not edit a file');
    await editRow.locator(':scope > summary').click();
    await editRow.locator('button', { hasText: '查看完整变更' }).first().click();
  }
  await page.waitForSelector('.turn-delivery', { timeout: 30000 });
  const deliveryText = await page.locator('.turn-delivery').innerText();
  assert.ok(deliveryText.includes('本轮执行'), deliveryText);
  // Any Runtime terminal is a valid delivery state; the dogfood records what
  // the real model produced instead of demanding a completion it did not claim.
  assert.ok(/已完成|回答结束|任务已完成|部分未验证|目标受阻|未完成|已暂停|失败|已取消/.test(deliveryText), deliveryText);
  const changed = await page.locator('.turn-delivery-file').allInnerTexts();
  assert.ok(changed.some(item => item.includes('calc.js')), `calc.js is a confirmed change: ${changed}`);
  assert.ok(changed.some(item => item.includes('test.js')), `test.js is a confirmed change: ${changed}`);
  // D07: the real commands and their exit codes are listed from the record,
  // not inferred from the model's prose.
  const commands = await page.locator('.turn-delivery-command').allInnerTexts();
  assert.ok(commands.some(item => /node\s+test\.js/.test(item)), `the real test command is listed: ${commands}`);
  assert.ok(commands.some(item => /退出码 0/.test(item)), `a real exit code is shown: ${commands}`);
  evidence.steps.D06_D08 = { changed, commands, terminal: deliveryText.split('\n').slice(0, 3), delivery: deliveryText.slice(0, 400) };
  await capture('r2c-03-delivery.png');

  // D02/D03: a real command output whose card preview is bounded is readable
  // in full, page by page. The model may redirect the requested command, so
  // the row is selected by the product's own "preview is partial" signal, not
  // by assuming a particular argument shape.
  try {
  await expandExecutionGroups();
  const fullOutputRows = () => page.locator('.tool-row[data-kind="command"]').filter({ has: page.locator('.tool-output-action') });
  let longRow = fullOutputRows().filter({ hasText: 'seq 1 40000' }).first();
  if (!(await longRow.count())) longRow = fullOutputRows().first();
  if (!(await longRow.count())) {
    // The real model sometimes skips a listed step or redirects it. A focused
    // follow-up is a real turn; it does not fabricate the output.
    const followUp = await receipts();
    await page.locator('#message').fill('请运行命令：seq 1 40000（不要重定向、不要管道），然后直接回复 LARGE_DONE。');
    await page.locator('#send').click();
    await waitTurn(followUp);
    await expandExecutionGroups();
    longRow = fullOutputRows().filter({ hasText: 'seq 1 40000' }).first();
    if (!(await longRow.count())) longRow = fullOutputRows().first();
  }
  if (!(await longRow.count())) throw new Error('no tool call produced a bounded, recoverable output');
  // A bounded preview proves the RESULT was cut, not that this card kind
  // paints a terminal body. Walk the candidates until one shows a real
  // preview; the full-output action stays the same.
  let chosenRow = null;
  let previewText = '';
  const candidates = await fullOutputRows().all();
  for (const row of [longRow, ...candidates]) {
    if ((await row.getAttribute('open')) === null) await row.locator(':scope > summary').click().catch(() => {});
    const text = await row.locator('.tool-terminal-output').innerText().catch(() => '');
    if (text.length > 0) { chosenRow = row; previewText = text; break; }
  }
  if (!chosenRow) throw new Error('no bounded output rendered a real preview');
  const rowText = await chosenRow.innerText();
  const isSeq = rowText.includes('seq 1 40000');
  await longRow.locator('.tool-output-action button', { hasText: '查看完整结果' }).click();
  await page.waitForSelector('.tool-output-view', { timeout: 30000 });
  await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
  const reference = page.locator('.tool-output-ref', { hasText: 'stdout' }).first();
  let artifactMode = false;
  if (await reference.count()) {
    artifactMode = true;
    await reference.click();
    await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
  }
  let loaded = await page.locator('.tool-output-pre').innerText();
  let pages = 1;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    const more = page.locator('.tool-output-footer button', { hasText: '加载更多' });
    if (!(await more.count())) break;
    const beforeLength = loaded.length;
    await more.click();
    await page.waitForFunction(
      length => (document.querySelector('.tool-output-pre')?.textContent ?? '').length > length,
      beforeLength,
      { timeout: 20000 },
    );
    loaded = await page.locator('.tool-output-pre').innerText();
    pages += 1;
  }
  assert.ok(loaded.length > previewText.length, `the recorded output is larger than the card preview (${loaded.length} vs ${previewText.length})`);
  if (isSeq) {
    assert.ok(loaded.split('\n').length >= 40000, `the real 40000-line output was paged: ${loaded.split('\n').length}`);
    assert.ok(loaded.trimEnd().endsWith('40000'), 'the real last line is present');
  }
  evidence.steps.D03_large_output = {
    artifact: artifactMode,
    preview_bytes: previewText.length,
    loaded_bytes: loaded.length,
    lines: loaded.split('\n').length,
    pages,
    tail: loaded.trimEnd().slice(-20),
  };
  await capture('r2c-02-full-output.png');
  } catch (error) {
    // A real model may redirect the command or skip it; D03 is proven by the
    // isolated acceptance and, when the model cooperates, here too.
    evidence.failures.push({ step: 'D03', detail: error.message ?? String(error) });
  }

  // D04: the failing command's real exit code is shown.
  await expandExecutionGroups();
  let failRow = page.locator('.tool-row[data-kind="command"]').filter({ hasText: 'process.exit(4)' }).first();
  if (!(await failRow.count())) {
    const followUp = await receipts();
    await page.locator('#message').fill('请运行：node -e "process.exit(4)"，不要包裹、不要忽略退出码；然后回复 FAIL_DONE。');
    await page.locator('#send').click();
    await waitTurn(followUp);
    await expandExecutionGroups();
    failRow = page.locator('.tool-row[data-kind="command"]').filter({ hasText: 'process.exit(4)' }).first();
  }
  if (await failRow.count()) {
    if ((await failRow.getAttribute('open')) === null) await failRow.locator(':scope > summary').click();
    const text = await failRow.innerText();
    // The real model wrapped the command to observe its exit (`...; code=$?`),
    // so the TOOL exited 0 and the UI must not paint a failure badge. The
    // output must still show the real observed failure. A tool that itself
    // exits non-zero is covered by the isolated r2c-tool-output script.
    // A direct failing call shows the real exit code; a wrapper the model
    // chose instead exited 0 and the output carries the observed failure.
    // Both are truthful; neither may paint a code the process did not return.
    const badge = /退出码 (-?\d+)/.exec(text)?.[1] ?? null;
    const observed = text.includes('observed_exit=4');
    assert.ok(badge === '4' || observed, `the real failure is visible: ${text}`);
    if (badge !== null) assert.equal(badge, '4', `the visible exit code is the real one: ${text}`);
    assert.ok(text.includes('EXPECTED_FAIL'), `the real failure output is visible: ${text}`);
    evidence.steps.D04_failure = { exit_badge: badge, observed_exit: observed, output_visible: true };
  } else {
    evidence.failures.push({ step: 'D04', detail: 'the model did not run the requested failing command' });
  }

  // D13: restart and reopen — the same recorded output is still readable.
  // Under a loaded machine Playwright's close can wait on the process; bound
  // it and fall back to the OS so the restart assertion is not lost.
  await Promise.race([
    app.close().catch(() => {}),
    new Promise(resolve => setTimeout(resolve, 8000)),
  ]);
  try { app.process()?.kill('SIGKILL'); } catch { /* already gone */ }
  app = null;
  await new Promise(resolve => setTimeout(resolve, 1500));
  await launch();
  await page.waitForSelector(`.session-row[data-session-id="${sessionId}"]`, { timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  // Selecting a row is not the conversation: wait for the UI to actually own
  // this session before reading its transcript.
  await page.waitForFunction(expected => {
    const row = document.querySelector('.session-row[aria-selected="true"]');
    return row?.dataset.sessionId === expected;
  }, sessionId, { timeout: 60000 });
  await idle();
  await page.waitForFunction(
    text => (document.querySelector('#conversation')?.innerText ?? '').includes(text),
    'R2C_DOGFOOD',
    { timeout: 60000 },
  );
  // A reopened session replays its durable history asynchronously; the rows
  // must exist before they can be opened.
  await page.waitForFunction(
    () => document.querySelectorAll('.tool-row, .execution-group').length > 0,
    null,
    { timeout: 60000 },
  );
  await expandExecutionGroups();
  const reopenedRow = page.locator('.tool-row[data-kind="command"]').filter({ has: page.locator('.tool-output-action') }).first();
  if (await reopenedRow.count()) {
    if ((await reopenedRow.getAttribute('open')) === null) await reopenedRow.locator(':scope > summary').click();
    await reopenedRow.locator('.tool-output-action button', { hasText: '查看完整结果' }).click();
    await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
    const reopened = await page.locator('.tool-output-pre').innerText();
    assert.ok(reopened.length > 0, 'the reopened session still serves a recorded result');
    evidence.steps.D13_restart = { recorded_result: true, bytes: reopened.length };
  } else {
    evidence.failures.push({ step: 'D13', detail: 'no bounded command output in the reopened session' });
  }
  await capture('r2c-04-reopened.png');

  // Archive the sessions this run created while one of them is selected; the
  // historical check below must not leave a test session behind.
  for (const id of created) await archive(id).catch(error => evidence.failures.push({ step: 'archive', detail: String(error) }));
  created.length = 0;

  // D14: an existing (pre-R2c) session opens without read degradation. Its
  // tool rows may have no full-output entry; whatever it shows must be honest.
  const tasks = (await page.evaluate(() => window.desktop.listTasks({ include_archived: false }))).tasks ?? [];
  const historical = tasks.find(task => task.id !== sessionId && task.primary_workspace);
  if (historical) {
    try {
      await page.locator(`.session-row[data-session-id="${historical.id}"]`).click().catch(async () => {
        // The row may not be in the virtualized sidebar; open by identity.
        await Promise.race([
          page.evaluate(task => window.desktop.openTask({ id: task.id, source_id: task.source_id }), historical),
          new Promise((_, reject) => setTimeout(() => reject(new Error('opening the historical session timed out')), 60000)),
        ]);
      });
      await idle();
      await expandExecutionGroups();
      const rows = page.locator('.tool-row');
      const count = await rows.count();
      let action = 0;
      for (let index = 0; index < Math.min(count, 12); index += 1) {
        if ((await rows.nth(index).getAttribute('open')) === null) await rows.nth(index).locator(':scope > summary').click().catch(() => {});
        action += await rows.nth(index).locator('.tool-output-action').count();
      }
      evidence.steps.D14_history = { session: historical.id.slice(0, 8), tool_rows: count, full_output_entries: action };
    } catch (error) {
      evidence.failures.push({ step: 'D14', detail: error.message ?? String(error) });
    }
  } else {
    evidence.steps.D14_history = { skipped: 'no other workspace session was visible' };
  }

  // The real profile's configuration was read-only for this run.
  const configAfter = configHash();
  assert.equal(configAfter, configBefore, 'the real config must not change');
  evidence.config_sha256_after = configAfter;
  assert.deepEqual(errors, [], 'no renderer page errors');
  // Recorded step failures are real: a run with any of them is not a PASS.
  evidence.result = evidence.failures.length === 0 ? 'PASS' : 'FAIL_PARTIAL';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r2c-dogfood-failure.png').catch(() => {});
  throw error;
} finally {
  // Evidence first: Electron's close can wait on a process under load, and
  // losing the evidence would turn a real PASS into an unusable run.
  await writeFile(path.join(output, 'r2c-dogfood-evidence.json'), JSON.stringify(evidence, null, 2));
  for (const id of created) await archive(id).catch(() => {});
  await Promise.race([
    app?.close().catch(() => {}),
    new Promise(resolve => setTimeout(resolve, 8000)),
  ]);
  try { app?.process()?.kill('SIGKILL'); } catch { /* already gone */ }
  await writeFile(path.join(output, 'r2c-dogfood-evidence.json'), JSON.stringify(evidence, null, 2));
}
