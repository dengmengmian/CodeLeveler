// Real Desktop acceptance for scheduled automation (Phase 5, DF-54/DF-56).
// Everything runs against the real Electron app and the real `leveler`
// runtime: the definitions live in the runtime's user-global schedule store,
// the shell action is executed by the workspace runtime, and the CLI reads and
// mutates the same store. No static mock is used.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile, realpath } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const exec = promisify(execFile);
const root = fileURLToPath(new URL('..', import.meta.url));
const binary = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const temporary = await mkdtemp('/tmp/leveler-desktop-schedules-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.resolve(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(output, { recursive: true });
await mkdir(workspace, { recursive: true });
const canonicalWorkspace = await realpath(workspace);

// The provider is only present so the app boots with a valid default model;
// every assertion below is about the runtime schedule store and the real
// workspace shell, not about model output.
const server = createServer(async (request, response) => {
  for await (const _chunk of request) { /* drain */ }
  response.writeHead(500, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ error: { message: 'schedule acceptance does not call a model' } }));
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });

const evidence = {
  scope: 'real Electron UI + real leveler runtime + real workspace shell; CLI and Desktop share one schedule store',
  leveler_binary: binary,
  leveler_home: home,
  workspace: canonicalWorkspace,
};
const errors = [];
let app;
let page;

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: {
      ...process.env,
      ELECTRON_RUN_AS_NODE: undefined,
      LEVELER_HOME: home,
      LEVELER_CONFIG_DIR: undefined,
      LEVELER_BINARY: binary,
      LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '30',
    },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
};

const cli = async args => {
  const { stdout, stderr } = await exec(binary, args, {
    env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: '' },
    maxBuffer: 8 * 1024 * 1024,
  });
  return { stdout, stderr };
};
const cliJson = async args => JSON.parse((await cli(args)).stdout);
const cliList = () => cliJson(['schedule', 'list', '--repo', canonicalWorkspace, '--json']);

const openAutomation = async () => {
  await page.locator('#nav-automation').click();
  await page.waitForSelector('#entry-page:not([hidden]) .schedule-status');
  await page.waitForFunction(() => !document.querySelector('.schedule-status')?.innerText.includes('正在读取后台调度状态'));
};

const createShellSchedule = async ({ name, command }) => {
  await page.locator('#entry-page .entry-head .button.primary', { hasText: '新建任务' }).click();
  const dialog = page.locator('dialog.schedule-editor');
  await dialog.waitFor();
  const row = label => dialog
    .locator('.schedule-editor-label')
    .filter({ hasText: new RegExp(`^${label}$`) })
    .locator('xpath=..');
  await row('名称').locator('input').fill(name);
  await row('工作目录').locator('input').fill(canonicalWorkspace);
  await row('动作类型').locator('select').selectOption('shell');
  await row('命令').locator('textarea').fill(command);
  await row('规则类型').locator('select').selectOption('daily');
  await row('每天时间').locator('input').fill('09:00');
  await row('IANA 时区').locator('input').fill('Asia/Shanghai');
  await dialog.locator('.modal-actions .button.primary').click();
  await dialog.waitFor({ state: 'detached' });
};

const grant = async () => {
  const external = page.locator('.schedule-grant input[type="checkbox"]');
  if (await external.count()) {
    await external.check();
  }
  await page.locator('.schedule-grant .button', { hasText: '授权无人值守' }).click();
  const confirm = page.locator('.confirm-modal');
  await confirm.waitFor();
  await confirm.locator('.button.primary', { hasText: '授权' }).click();
  await confirm.waitFor({ state: 'detached' });
  await page.waitForFunction(() => document.querySelector('.schedule-grant')?.innerText.includes('已授权无人值守'));
};

const refresh = async () => {
  await page.locator('#entry-page .entry-head .button.secondary', { hasText: '刷新' }).click();
};

const waitForDetail = async text => {
  await page.waitForFunction(value => document.querySelector('.schedule-detail')?.innerText.includes(value), text);
};

const selectByName = async name => {
  const row = page.locator('.schedule-row').filter({ hasText: name });
  await row.waitFor();
  await row.click();
  await waitForDetail(name);
};

try {
  // Seed one definition through the CLI before the UI opens. The Desktop must
  // project the same store, not its own copy.
  const seeded = await cliJson(['schedule', 'add', '--repo', canonicalWorkspace, '--name', 'DF-56 CLI seed', '--every', '3600', '--command', 'printf cli-seed > cli-seed.txt', '--json']);
  evidence.cli_seeded_id = seeded.id;
  evidence.cli_seeded_name = seeded.name;

  await launch();
  await page.locator('#new-session').click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') && document.querySelector('#conversation-status').textContent !== '连接 Runtime…');
  const sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  evidence.session_id = sessionId;

  await openAutomation();
  const statusText = await page.locator('.schedule-status').innerText();
  assert.match(statusText, /后台唤醒未启用|系统唤醒已注册/);
  assert.match(statusText, /待处理/);
  evidence.wakeup_not_enabled_by_default = /后台唤醒未启用/.test(statusText);
  assert.equal(evidence.wakeup_not_enabled_by_default, true, 'adding schedules must not silently enable OS wakeup');

  // The CLI-seeded definition is visible through the runtime projection,
  // without a model call.
  await page.waitForFunction(() => [...document.querySelectorAll('.schedule-row')].some(row => row.innerText.includes('DF-56 CLI seed')));
  evidence.cli_seed_visible_in_desktop = true;

  await createShellSchedule({
    name: 'DF-54 Desktop shell',
    command: 'printf DF54 > df54-desktop.txt',
  });
  await page.waitForFunction(() => document.querySelector('.schedule-detail h2')?.textContent === 'DF-54 Desktop shell');
  const created = (await cliList()).find(item => item.name === 'DF-54 Desktop shell');
  assert.ok(created, 'the Desktop-created definition must be in the runtime store');
  assert.equal(created.workspace, canonicalWorkspace);
  assert.equal(created.action.kind, 'shell');
  assert.equal(created.action.command, 'printf DF54 > df54-desktop.txt');
  assert.equal(created.trigger.kind, 'daily');
  assert.equal(created.trigger.time, '09:00');
  assert.equal(created.trigger.timezone, 'Asia/Shanghai');
  evidence.created_id = created.id;
  evidence.created_trigger = created.trigger;
  assert.match(await page.locator('.schedule-detail').innerText(), /每天 09:00 \(Asia\/Shanghai\)/);
  evidence.grant_after_create = created.unattended_grant;
  assert.equal(created.unattended_grant, null, 'creating a schedule must not grant unattended execution');

  await waitForDetail('需要授权');
  evidence.selected_after_create = await page.evaluate(() => document.querySelector('.schedule-row[aria-selected="true"] .entry-row-title')?.textContent ?? null);
  evidence.detail_after_create = await page.evaluate(() => document.querySelector('.schedule-detail h2')?.textContent ?? null);
  await grant();
  const granted = (await cliList()).find(item => item.id === created.id);
  assert.ok(granted.unattended_grant, 'the grant must be persisted in the runtime store');
  evidence.grant_persisted = true;

  await page.locator('.schedule-actions .button', { hasText: '立即运行' }).click();
  await page.waitForFunction(() => document.querySelector('.schedule-runs')?.innerText.includes('已完成'), null, { timeout: 45000 });
  await page.waitForFunction(() => document.querySelector('.schedule-runs')?.innerText.includes('退出码 0'));
  assert.equal((await readFile(path.join(workspace, 'df54-desktop.txt'), 'utf8')), 'DF54');
  evidence.run_now_succeeded = true;
  evidence.run_now_exit = 0;
  evidence.selected_after_run = await page.evaluate(() => document.querySelector('.schedule-row[aria-selected="true"] .entry-row-title')?.textContent ?? null);
  evidence.detail_after_run = await page.evaluate(() => document.querySelector('.schedule-detail h2')?.textContent ?? null);

  await page.locator('.schedule-actions .button', { hasText: '暂停' }).click();
  await page.waitForFunction(() => document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
  const paused = (await cliList()).find(item => item.id === created.id);
  assert.equal(paused.enabled, false);
  evidence.desktop_pause_visible_to_cli = true;

  await page.locator('.schedule-actions .button', { hasText: '恢复' }).click();
  await page.waitForFunction(() => !document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
  assert.equal((await cliList()).find(item => item.id === created.id).enabled, true);

  // Editing the action through the UI must invalidate the old unattended
  // grant: the permission was attached to the old action, not the definition
  // name.
  await page.locator('.schedule-actions .button', { hasText: '编辑' }).click();
  const editor = page.locator('dialog.schedule-editor');
  await editor.waitFor();
  await editor.locator('.schedule-editor-label').filter({ hasText: /^命令$/ }).locator('xpath=..').locator('textarea').fill('printf DF54B > df54-desktop.txt');
  await editor.locator('.modal-actions .button.primary', { hasText: '保存' }).click();
  await editor.waitFor({ state: 'detached' });
  await waitForDetail('需要授权');
  await page.waitForFunction(() => document.querySelector('.schedule-grant')?.innerText.includes('授权已失效'));
  const edited = (await cliList()).find(item => item.id === created.id);
  assert.equal(edited.action.command, 'printf DF54B > df54-desktop.txt');
  assert.equal(edited.unattended_grant, null, 'changing the action must revoke the old grant');
  evidence.edit_invalidated_grant = true;

  await grant();
  await page.locator('.schedule-actions .button', { hasText: '立即运行' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('.schedule-run')].filter(row => row.innerText.includes('已完成') && row.innerText.includes('退出码 0')).length >= 2, null, { timeout: 45000 });
  assert.equal((await readFile(path.join(workspace, 'df54-desktop.txt'), 'utf8')), 'DF54B');
  evidence.second_run_after_regrant = true;

  // Cross-entry: the CLI pauses the definition; the open Desktop page must
  // show the runtime's new state after a refresh, and the Desktop resume is
  // visible to the CLI again.
  await cli(['schedule', 'pause', created.id, '--repo', canonicalWorkspace]);
  await refresh();
  await page.waitForFunction(() => document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
  evidence.cli_pause_visible_in_desktop = true;
  await page.locator('.schedule-actions .button', { hasText: '恢复' }).click();
  await page.waitForFunction(() => !document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
  assert.equal((await cliList()).find(item => item.id === created.id).enabled, true);
  evidence.desktop_resume_visible_to_cli = true;

  // Cross-entry, the other direction of an edit: a deterministic CLI update
  // (no model) replaces the action and rule, drops the old grant, and the open
  // Desktop page must show exactly that after a refresh.
  const observed = await cliJson(['schedule', 'show', created.id, '--repo', canonicalWorkspace, '--json']);
  const cliUpdated = await cliJson([
    'schedule', 'update', created.id, '--repo', canonicalWorkspace,
    '--command', 'printf DF56 > df56-cli.txt',
    '--daily', '10:15', '--timezone', 'Asia/Shanghai',
    '--expected-revision', observed.revision, '--json',
  ]);
  assert.equal(cliUpdated.action.command, 'printf DF56 > df56-cli.txt');
  assert.equal(cliUpdated.trigger.time, '10:15');
  assert.equal(cliUpdated.unattended_grant, null, 'a CLI edit must drop the grant bound to the old action');
  await refresh();
  await waitForDetail('10:15');
  await page.waitForFunction(() => document.querySelector('.schedule-grant')?.innerText.includes('授权已失效'));
  evidence.cli_update_visible_in_desktop = true;
  await grant();

  await page.screenshot({ path: path.join(output, 'schedules-desktop.png') });

  // Closing the window must not delete or stop anything. Reopening the app
  // must project the same definitions and the same run history.
  const exited = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('window close did not exit')), 15000);
    app.process().once('exit', () => { clearTimeout(timer); resolve(); });
  });
  await page.evaluate(() => window.close());
  await exited;
  app = null;
  evidence.window_close_exited = true;

  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), sessionId);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await openAutomation();
  await page.waitForFunction(() => [...document.querySelectorAll('.schedule-row')].some(row => row.innerText.includes('DF-54 Desktop shell')));
  await selectByName('DF-54 Desktop shell');
  await page.waitForFunction(() => [...document.querySelectorAll('.schedule-run')].length >= 2);
  assert.match(await page.locator('.schedule-detail').innerText(), /已授权无人值守/);
  evidence.reopened_with_definition_and_history = true;
  await page.screenshot({ path: path.join(output, 'schedules-desktop-reopened.png') });

  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'schedules-desktop-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'schedules-desktop-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'schedules-desktop-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
