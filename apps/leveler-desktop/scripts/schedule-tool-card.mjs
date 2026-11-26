// R0 fixture: a real model-driven `schedule_manage` tool call must render the
// schedule card and its "打开自动化任务" action must reach the automation page.
// The provider is test-only; the tool, store and page are the real Runtime.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const exec = promisify(execFile);
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-schedule-card-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home); await mkdir(workspace); await mkdir(output, { recursive: true });

const scheduleName = 'R0 schedule card fixture';
const createArgs = {
  action: 'create',
  name: scheduleName,
  workspace,
  work: { kind: 'shell', command: 'printf card-started > card.txt' },
  trigger: { kind: 'every', every_secs: 3600 },
  mode: 'full_access',
};
const providerErrors = [];
const server = createServer(async (request, response) => {
  try {
    let raw = '';
    for await (const part of request) raw += part;
    const body = JSON.parse(raw);
    const names = (body.tools ?? []).map(tool => tool.function?.name);
    if (!names.length) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
      return;
    }
    assert.ok(names.includes('schedule_manage'), 'the real provider schema must expose the registered schedule_manage tool');
    const toolResults = body.messages.filter(message => message.role === 'tool').length;
    const delta = toolResults === 0
      ? { tool_calls: [{ index: 0, id: 'r0-schedule', type: 'function', function: { name: 'schedule_manage', arguments: JSON.stringify(createArgs) } }] }
      : { content: 'SCHEDULE_CARD_OK' };
    response.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const [payload, finish] of [[delta, null], [{}, toolResults === 0 ? 'tool_calls' : 'stop']]) {
      response.write(`data: ${JSON.stringify({ id: 'schedule-card', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta: payload, finish_reason: finish }] })}\n\n`);
    }
    response.end('data: [DONE]\n\n');
  } catch (error) {
    providerErrors.push(error.message);
    response.writeHead(500);
    response.end('fixture failed');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "http://127.0.0.1:${server.address().port}"\napi_key = "test-only"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const cli = async args => exec(leveler, args, {
  env: { ...process.env, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined },
});
const evidence = { scope: 'real Electron/Runtime tool execution, schedule store and automation page; isolated home/workspace; test-only SSE provider and native-folder seam' };
let app;
let page;
const errors = [];
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));

  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));

  // Full access keeps this fixture about the card, not about an approval the
  // schedule tool would otherwise need for a workspace write risk.
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '完全权限' }).click();
  await page.waitForFunction(async () => {
    const row = document.querySelector('.session-row[aria-selected="true"]');
    if (!row) return false;
    const snapshot = await window.desktop.snapshot(row.dataset.sessionId);
    return snapshot.mode === 'full_access';
  });

  await page.locator('#message').fill('Create a scheduled task for this isolated workspace');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('SCHEDULE_CARD_OK') && ['已回答', '已完成'].includes(document.querySelector('#conversation-status').textContent), null, { timeout: 60000 });

  // The tool really executed against the schedule store.
  const listed = JSON.parse((await cli(['schedule', 'list', '--repo', workspace, '--json'])).stdout);
  assert.ok(Array.isArray(listed), `schedule list --json must be an array: ${JSON.stringify(listed)}`);
  const created = listed.find(item => item.name === scheduleName);
  assert.ok(created, `the real schedule store must contain the created definition: ${JSON.stringify(listed)}`);
  evidence.real_schedule_created = { id: created.id, name: created.name, trigger: created.trigger ?? null };
  assert.equal(createArgs.action, 'create');

  const row = page.locator('.tool-row[data-tool-id="r0-schedule"]');
  await row.waitFor();
  const group = row.locator('xpath=ancestor::details[contains(@class,"execution-group")]').first();
  if (await group.count() && !(await group.evaluate(node => node.open))) await group.locator('.execution-summary').first().click();
  if (!(await row.evaluate(node => node.open))) await row.locator('.tool-summary').click();
  const card = page.locator('.tool-row[data-tool-id="r0-schedule"] .schedule-turn-card');
  await card.waitFor();
  assert.match(await card.innerText(), /R0 schedule card fixture/);
  assert.match(await card.innerText(), /每隔 1 小时/);
  assert.equal(await card.locator('.schedule-turn-frequency').count(), 1);
  evidence.schedule_card_rendered = true;

  await card.getByRole('button', { name: '打开自动化任务' }).click();
  await page.waitForSelector('#entry-page:not([hidden]) .schedule-status', { timeout: 15000 });
  await page.waitForFunction(() => document.querySelector('#nav-automation')?.getAttribute('aria-current') === 'page');
  assert.match(await page.locator('#entry-page').innerText(), /自动化/);
  evidence.automation_page_from_card = true;
  assert.deepEqual(errors, []);
  assert.deepEqual(providerErrors, []);
  Object.assign(evidence, { page_errors: errors, provider_errors: providerErrors, passed: true });
  await writeFile(path.join(output, 'schedule-tool-card-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors, provider_errors: providerErrors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'schedule-tool-card-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'schedule-tool-card-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
