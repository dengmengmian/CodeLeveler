// Background task acceptance: real Runtime process lifecycle driven through the
// Desktop composer and the header jobs popover.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-background-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });
const server = createServer(async (request, response) => {
  let text = '';
  for await (const chunk of request) text += chunk;
  let body = {};
  try { body = JSON.parse(text); } catch {}
  const names = (body.tools ?? []).map(tool => tool.function?.name);
  if (!names.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ id: 'mem', choices: [{ index: 0, message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  const messages = body.messages ?? [];
  const lastUserIndex = messages.findLastIndex(message => message.role === 'user');
  const toolResults = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool').length;
  const lastUser = [...(body.messages ?? [])].reverse().find(message => message.role === 'user');
  const lastText = typeof lastUser?.content === 'string' ? lastUser.content : JSON.stringify(lastUser?.content ?? '');
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const chunk = (delta, finish) => JSON.stringify({ id: 'bg', object: 'chat.completion.chunk', created: 1, model: 'm', choices: [{ index: 0, delta, finish_reason: finish }] });
  if (lastText.startsWith('SEED')) {
    response.write(`data: ${chunk({ role: 'assistant', content: 'BG_SEED_OK' }, null)}\n\n`);
    response.write(`data: ${chunk({}, 'stop')}\n\n`);
    response.end('data: [DONE]\n\n');
    return;
  }
  if (!toolResults) {
    const program = lastText.includes('BG_COMPLETE') ? ['-c', 'echo complete-ok']
      : lastText.includes('BG_FAIL') ? ['-c', 'echo failing-line; exit 7']
        : ['-c', 'i=0; while true; do i=$((i+1)); echo tick-$i; sleep 1; done'];
    const args = { program: '/bin/sh', args: program, background: true, background_lifetime: 'runtime' };
    response.write(`data: ${chunk({ role: 'assistant', tool_calls: [{ index: 0, id: `bg-${Date.now()}`, type: 'function', function: { name: 'run_command', arguments: JSON.stringify(args) } }] }, null)}\n\n`);
    response.write(`data: ${chunk({}, 'tool_calls')}\n\n`);
  } else {
    response.write(`data: ${chunk({ role: 'assistant', content: `BG_DONE_${lastText.includes('BG_FAIL') ? 'FAIL' : lastText.includes('BG_COMPLETE') ? 'COMPLETE' : 'RUN'}` }, null)}\n\n`);
    response.write(`data: ${chunk({}, 'stop')}\n\n`);
  }
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${server.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });
const evidence = { scope: 'real Runtime background process lifecycle; test-only provider; full access session' };
const errors = [];
let app;
let page;
const tickPids = () => {
  try { return execFileSync('pgrep', ['-f', 'tick-'], { encoding: 'utf8' }).trim().split(/\s+/).filter(Boolean); }
  catch { return []; }
};
const waitBgExit = ok => page.waitForFunction(expected => globalThis.__bgEvents.some(event => event.type === 'background_task_exited' && event.ok === expected), ok, { timeout: 15000 });
const openJobs = async () => {
  const button = page.locator('#header-session-actions .header-count-action').filter({ hasText: '后台任务' }).first();
  await button.click();
  await page.waitForSelector('.jobs-popover');
  return page.locator('.jobs-popover');
};
const send = async text => {
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  await page.waitForFunction(marker => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes(marker)), `BG_DONE_${text.includes('FAIL') ? 'FAIL' : text.includes('COMPLETE') ? 'COMPLETE' : 'RUN'}`, { timeout: 60000 });
};
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(15000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.evaluate(() => { globalThis.__bgEvents = []; window.desktop.onEvent(frame => { if (frame.event === 'runtime' && String(frame.data?.type).startsWith('background_')) globalThis.__bgEvents.push(frame.data); }); });
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForSelector('.directory-picker-modal[open]');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: '使用系统选择器' }).click();
  await page.waitForSelector('.directory-picker-modal .directory-selected:not([hidden])');
  await page.locator('.directory-picker-modal .button.primary').filter({ hasText: /^打开$/ }).click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  // Create the workspace session with a plain answer first, then switch to
  // full access before any command tool is requested.
  await page.locator('#message').fill('SEED workspace session');
  await page.locator('#send').click();
  await page.waitForFunction(() => document.querySelector('.message.assistant .markdown')?.textContent.includes('BG_SEED_OK'));
  await page.locator('#permission-picker').click();
  await page.waitForSelector('.menu .menu-item');
  await page.locator('.menu .menu-item').filter({ hasText: '完全权限' }).click();
  await page.waitForFunction(async id => (await window.desktop.snapshot(id)).mode === 'full_access', await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id'));
  await send('BG_COMPLETE');
  await waitBgExit(true);
  await page.waitForFunction(() => document.querySelector('#header-session-actions')?.innerText.includes('后台任务'));
  let popover = await openJobs();
  assert.match(await popover.innerText(), /已完成/);
  assert.match(await popover.innerText(), /echo complete-ok/);
  await page.keyboard.press('Escape');
  evidence.completed_job_visible = true;
  await send('BG_FAIL');
  await waitBgExit(false);
  await page.waitForFunction(() => document.querySelector('#header-session-actions')?.innerText.includes('后台任务'));
  popover = await openJobs();
  assert.match(await popover.innerText(), /已失败/);
  assert.match(await popover.innerText(), /exit 7/);
  await page.keyboard.press('Escape');
  evidence.failed_job_visible = true;
  await send('BG_RUN');
  await page.waitForFunction(() => document.querySelector('#header-session-actions')?.innerText.includes('后台任务运行中'));
  // The header is a window drag region; the count controls must opt out or
  // macOS consumes their real mouse events before the page sees them.
  assert.equal(
    await page.locator('#header-session-actions').evaluate(node => getComputedStyle(node).webkitAppRegion),
    'no-drag',
  );
  popover = await openJobs();
  assert.match(await popover.innerText(), /进行中/);
  assert.match(await popover.innerText(), /运行中/);
  await page.waitForTimeout(3000);
  // The whole live row is the disclosure: it opens the runtime-reported output.
  await popover.locator('.job-row').first().click();
  await popover.locator('.job-output').waitFor({ timeout: 5000 });
  assert.match(await popover.locator('.job-output').innerText(), /\[stdout\] tick-/);
  await page.screenshot({ path: path.join(output, 'background-tasks-live-output.png') });
  const pids = tickPids();
  assert.ok(pids.length > 0, 'the Runtime background process is alive before stop');
  // The stop control confirms on a second press; the first press only arms it.
  await popover.locator('.job-stop').first().click();
  assert.match(await popover.locator('.job-stop').first().innerText(), /确认停止/);
  await page.screenshot({ path: path.join(output, 'background-tasks-stop-armed.png') });
  await page.waitForTimeout(200);
  assert.ok(tickPids().length > 0, 'the armed first press must not stop the process');
  await popover.locator('.job-stop').first().click();
  await page.waitForFunction(() => !document.querySelector('.jobs-popover .job-live-card'), null, { timeout: 15000 });
  assert.match(await popover.innerText(), /已停止/);
  for (let index = 0; index < 40 && tickPids().length; index += 1) await page.waitForTimeout(100);
  assert.deepEqual(tickPids(), []);
  evidence.stop_killed_process = { before: pids, after: [] };
  // A Workbench on the right leaves a narrow conversation pane: the panel must
  // shrink to the anchor's available width, never drift right over the pane.
  await page.keyboard.press('Escape');
  await page.locator('#header-right-toggle').click();
  await page.waitForFunction(() => !document.querySelector('#rightbar').hidden);
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setContentSize(1200, 800));
  await page.waitForFunction(() => innerWidth === 1200);
  await page.waitForTimeout(300);
  popover = await openJobs();
  evidence.popoverAnchoring = await page.evaluate(() => {
    const trigger = [...document.querySelectorAll('#header-session-actions .header-count-action')]
      .find(node => node.textContent.includes('后台任务'));
    const panel = document.querySelector('.jobs-popover');
    return {
      triggerRight: Math.round(trigger.getBoundingClientRect().right),
      panelRight: Math.round(panel.getBoundingClientRect().right),
      panelLeft: Math.round(panel.getBoundingClientRect().left),
      panelWidth: Math.round(panel.getBoundingClientRect().width),
    };
  });
  assert.ok(
    Math.abs(evidence.popoverAnchoring.triggerRight - evidence.popoverAnchoring.panelRight) <= 2,
    `the jobs panel drifted from its trigger: ${JSON.stringify(evidence.popoverAnchoring)}`,
  );
  await page.screenshot({ path: path.join(output, 'background-tasks-narrow-pane.png') });
  await page.keyboard.press('Escape');
  const snapshotId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), snapshotId);
  assert.deepEqual(snapshot.active_background_tasks ?? [], []);
  evidence.snapshot_has_no_active_task = true;
  await page.screenshot({ path: path.join(output, 'background-tasks.png') });
  assert.deepEqual(errors, []);
  Object.assign(evidence, { page_errors: errors, passed: true });
  await writeFile(path.join(output, 'background-tasks-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors, live_processes: tickPids() });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'background-tasks-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'background-tasks-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  for (const pid of tickPids()) { try { process.kill(Number(pid)); } catch {} }
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  await new Promise(resolve => server.close(resolve));
}
