// One live Electron window driven over stdin, used by the simultaneous
// Desktop + TUI acceptance. The Python driver owns the shared LEVELER_HOME and
// workspace; this process only exercises the real Desktop UI.
import { _electron as electron } from 'playwright';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { createInterface } from 'node:readline';
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const exec = promisify(execFile);
const root = fileURLToPath(new URL('..', import.meta.url));
const binary = process.env.LEVELER_BINARY;
const home = process.env.LEVELER_HOME;
const workspace = process.env.LEVELER_WORKSPACE;
if (!binary || !home || !workspace) throw new Error('missing concurrent acceptance environment');

const cli = async args => {
  const { stdout } = await exec(binary, args, {
    env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: '' },
    maxBuffer: 8 * 1024 * 1024,
  });
  return JSON.parse(stdout);
};

const errors = [];
let app;
let page;

const openAutomation = async () => {
  await page.locator('#nav-automation').click();
  await page.waitForSelector('#entry-page:not([hidden]) .schedule-status');
  await page.waitForFunction(() => !document.querySelector('.schedule-status')?.innerText.includes('正在读取后台调度状态'));
};

const selectByName = async name => {
  const row = page.locator('.schedule-row').filter({ hasText: name });
  await row.waitFor();
  // The row is a toggle: clicking an already selected row closes its detail.
  // Select only when it is closed, so a repeated call is idempotent.
  if ((await row.getAttribute('aria-selected')) !== 'true') await row.click();
  await page.waitForFunction(value => document.querySelector('.schedule-detail h2')?.textContent === value, name);
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
  await row('工作目录').locator('input').fill(workspace);
  await row('动作类型').locator('select').selectOption('shell');
  await row('命令').locator('textarea').fill(command);
  await row('规则类型').locator('select').selectOption('daily');
  await row('每天时间').locator('input').fill('09:00');
  await row('IANA 时区').locator('input').fill('Asia/Shanghai');
  await dialog.locator('.modal-actions .button.primary').click();
  await dialog.waitFor({ state: 'detached' });
  await page.waitForFunction(value => document.querySelector('.schedule-detail h2')?.textContent === value, name);
};

const editCommand = async ({ name, command }) => {
  await selectByName(name);
  await page.locator('.schedule-actions .button', { hasText: '编辑' }).click();
  const editor = page.locator('dialog.schedule-editor');
  await editor.waitFor();
  await editor
    .locator('.schedule-editor-label')
    .filter({ hasText: /^命令$/ })
    .locator('xpath=..')
    .locator('textarea')
    .fill(command);
  await editor.locator('.modal-actions .button.primary', { hasText: '保存' }).click();
  await editor.waitFor({ state: 'detached' });
};

const state = async () => ({
  rows: await page.locator('.schedule-row').allInnerTexts(),
  detail: await page.locator('.schedule-detail').innerText().catch(() => ''),
  errors: [...errors],
});

const commands = {
  async state() {
    return state();
  },
  async create({ name, command }) {
    await createShellSchedule({ name, command });
    const rows = await cli(['schedule', 'list', '--repo', workspace, '--json']);
    return { id: rows.find(row => row.name === name)?.id ?? null };
  },
  async edit({ name, command }) {
    await editCommand({ name, command });
    return state();
  },
  async pause({ name }) {
    await selectByName(name);
    await page.locator('.schedule-actions .button', { hasText: '暂停' }).click();
    await page.waitForFunction(() => document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
    return state();
  },
  async resume({ name }) {
    await selectByName(name);
    await page.locator('.schedule-actions .button', { hasText: '恢复' }).click();
    await page.waitForFunction(() => !document.querySelector('.schedule-detail')?.innerText.includes('已暂停'));
    return state();
  },
  async runNow({ name }) {
    await selectByName(name);
    await page.locator('.schedule-actions .button', { hasText: '立即运行' }).click();
    await page.waitForFunction(
      () => document.querySelector('.schedule-runs')?.innerText.includes('已完成'),
      null,
      { timeout: 45000 },
    );
    return state();
  },
  async detail({ name }) {
    await selectByName(name);
    return state();
  },
  async close() {
    setImmediate(() => process.exit(0));
    return { closing: true };
  },
};

const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${home}/electron-profile`],
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
  page.setDefaultTimeout(60000);
  page.on('pageerror', error => {
    errors.push(error.message);
    process.stderr.write(`pageerror: ${error.message}\n`);
  });
  page.on('console', message => process.stderr.write(`console ${message.type()}: ${message.text()}\n`));
  try {
    await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  } catch (error) {
    const body = await page.content().catch(() => '');
    writeFileSync('/tmp/lv-electron-launch-debug.log', `url=${page.url()}\nerrors=${JSON.stringify(errors)}\nbody=${body.slice(0, 5000)}\n`);
    throw error;
  }
  try {
    await page.locator('#new-session').click();
    await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]') && document.querySelector('#conversation-status').textContent !== '连接 Runtime…');
  } catch (error) {
    const body = await page.locator('body').innerText().catch(() => '');
    throw new Error(`${error.message}\npage=${body.slice(0, 1500)}`);
  }
  await openAutomation();
};

try {
  await launch();
  process.stdout.write('READY\n');
  const lines = createInterface({ input: process.stdin });
  for await (const line of lines) {
    if (!line.trim()) continue;
    const request = JSON.parse(line);
    try {
      const result = await commands[request.cmd](request);
      process.stdout.write(`${JSON.stringify({ ok: true, result })}\n`);
    } catch (error) {
      // A failing command must report what the window shows; otherwise the
      // only evidence is a Playwright timeout string.
      const view = page ? await state().catch(() => null) : null;
      process.stdout.write(`${JSON.stringify({ ok: false, error: error.message, errors, view })}\n`);
    }
  }
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
}
