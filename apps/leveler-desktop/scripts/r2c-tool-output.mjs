// R2c: full tool output in the real Desktop + Runtime.
//
// Test-only OpenAI-compatible provider over real SSE; real Electron, bridge,
// daemon and workspace. Every asserted fact is produced by a real tool call:
// the large output is a real `seq 30000`, the small one a real file read, and
// the failure a real `exit 7`. The viewer is read through the same runtime
// query the UI uses; nothing here reconstructs text from the card preview.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-tool-output-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home);
await mkdir(workspace);
await writeFile(path.join(workspace, 'hello.txt'), 'alpha\nbeta\ngamma\n');

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'isolated real Desktop/Runtime; test-only SSE provider; real tool execution', steps: {}, failures: [] };
const requests = [];

const chunk = (delta, finish) => JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] });
const sseText = text => [
  `data: ${chunk({ role: 'assistant', content: text }, null)}`,
  `data: ${chunk({}, 'stop')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';
const sseTools = calls => [
  `data: ${chunk({ role: 'assistant', tool_calls: calls.map((call, index) => ({ index, id: call.id, type: 'function', function: { name: call.name, arguments: JSON.stringify(call.arguments) } })) }, null)}`,
  `data: ${chunk({}, 'tool_calls')}`,
  'data: [DONE]',
].join('\n\n') + '\n\n';

const provider = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const toolNames = (body.tools ?? []).map(tool => tool.function?.name);
  if (!toolNames.length) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ choices: [{ message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
    return;
  }
  const messages = body.messages ?? [];
  const lastUserIndex = messages.reduce((last, message, index) => message.role === 'user'
    && typeof message.content === 'string' && /R2C_/.test(message.content) ? index : last, -1);
  const turnText = lastUserIndex >= 0 && typeof messages[lastUserIndex].content === 'string' ? messages[lastUserIndex].content : '';
  const toolResults = messages.slice(lastUserIndex + 1).filter(message => message.role === 'tool').length;
  requests.push({ turnText, toolResults, toolNames });
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  if (turnText.includes('R2C_BIG')) {
    if (!toolResults) {
      response.end(sseTools([{ id: 'big-1', name: 'run_command', arguments: { program: 'sh', args: ['-c', 'seq 1 30000; echo BIG_TAIL_MARKER'], reason: 'produce a large real output' } }]));
    } else {
      response.end(sseText('BIG_DONE'));
    }
    return;
  }
  if (turnText.includes('R2C_SMALL')) {
    if (!toolResults) {
      response.end(sseTools([{ id: 'small-1', name: 'read_file', arguments: { path: 'hello.txt' } }]));
    } else {
      response.end(sseText('SMALL_DONE'));
    }
    return;
  }
  if (turnText.includes('R2C_FAIL')) {
    if (!toolResults) {
      response.end(sseTools([{ id: 'fail-1', name: 'run_command', arguments: { program: 'sh', args: ['-c', 'echo boom >&2; exit 7'], reason: 'produce a real failing command' } }]));
    } else {
      response.end(sseText('FAIL_DONE'));
    }
    return;
  }
  response.end(sseText('CHAT_OK'));
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"
[providers.fixture]
protocol = "openai_chat"
base_url = "http://127.0.0.1:${provider.address().port}"
api_key = "test-only"
[models.m]
provider = "fixture"
model_id = "fixture-model"
context_window = 131072
parallel_tool_calls = false
vision = false
`, { mode: 0o600 });

let app;
let page;
const pageErrors = [];

async function launch() {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => pageErrors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } });
}
const receipts = () => page.locator('.turn-receipt').count();
const idle = () => page.waitForFunction(() => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, null, { timeout: 120000 });
async function waitTurn(before) {
  await page.waitForFunction(count => document.querySelectorAll('.turn-receipt').length >= count, before + 1, { timeout: 120000 });
  await idle();
}
async function addWorkspace() {
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.innerText.includes(name), path.basename(workspace));
}
async function send(text) {
  const before = await receipts();
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  await waitTurn(before);
}
async function openToolRow() {
  const row = page.locator('.tool-row[data-kind="command"]').last();
  await row.locator('summary').click();
  return row;
}
async function capture(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.screenshot({ path: path.join(output, name) });
}

try {
  await launch();
  await addWorkspace();

  // ---- Large output: the preview is bounded; the full result and the real
  // stdout artifact are readable from the tool card.
  await send('R2C_BIG 运行一个大输出命令');
  const bigRow = await openToolRow();
  const preview = await bigRow.locator('.tool-terminal-output').innerText();
  assert.ok(!preview.includes('BIG_TAIL_MARKER'), 'the card preview must not contain the tail');
  const fullButton = bigRow.locator('.tool-output-action button', { hasText: '查看完整结果' });
  assert.equal(await fullButton.count(), 1, 'a truncated preview offers the full output');
  await fullButton.click();
  await page.waitForSelector('.tool-output-view', { timeout: 30000 });
  await page.waitForFunction(() => (document.querySelector('.tool-output-pre')?.textContent ?? '').length > 0, null, { timeout: 30000 });
  const modelText = await page.locator('.tool-output-pre').innerText();
  assert.ok(modelText.includes('[stdout full output:'), 'the model-visible result carries its stream locator');
  assert.ok(modelText.length > 1200, 'the recorded result is larger than the card preview');
  const chip = page.locator('.tool-output-ref', { hasText: 'stdout' }).first();
  assert.equal(await chip.count(), 1, 'the complete stdout is offered as a reference');
  evidence.steps.model_result = { preview_bytes: preview.length, recorded_bytes: modelText.length, references: await page.locator('.tool-output-ref').allInnerTexts() };
  await chip.click();
  await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
  let loaded = await page.locator('.tool-output-pre').innerText();
  for (let attempt = 0; attempt < 8 && !loaded.includes('BIG_TAIL_MARKER'); attempt += 1) {
    const more = page.locator('.tool-output-footer button', { hasText: '加载更多' });
    if (!(await more.count())) break;
    await more.click();
    await page.waitForSelector('.tool-output-pre', { timeout: 30000 });
    await page.waitForTimeout(150);
    loaded = await page.locator('.tool-output-pre').innerText();
  }
  assert.ok(loaded.trimEnd().endsWith('BIG_TAIL_MARKER'), `the artifact pages to its real last line: ${loaded.slice(-80)}`);
  assert.ok(loaded.split('\n').length >= 30000, `all 30000 lines were readable in pages: ${loaded.split('\n').length}`);
  evidence.steps.big_output = { lines: loaded.split('\n').length, tail: loaded.trimEnd().slice(-20), references: await page.locator('.tool-output-ref').allInnerTexts() };
  await capture('r2c-tool-output-big.png');

  // ---- Small output: a complete short result must not pretend to have more.
  await send('R2C_SMALL 读取一个小文件');
  const smallRow = page.locator('.tool-row[data-kind="read"]').last();
  await smallRow.locator('summary').click();
  assert.ok((await smallRow.innerText()).includes('alpha'), 'the preview is the real file content');
  assert.equal(await smallRow.locator('.tool-output-action').count(), 0, 'a complete short result has no full-output action');
  evidence.steps.small_output = { action: false };

  // ---- Failing command: the real exit code is shown, no invented success.
  await send('R2C_FAIL 运行一个失败命令');
  const failRow = page.locator('.tool-row[data-kind="command"]').last();
  await failRow.locator('summary').click();
  const failText = await failRow.innerText();
  assert.ok(failText.includes('退出码 7'), `the real exit code is shown: ${failText}`);
  evidence.steps.failure = { exit_code: 7 };

  // ---- The raw answers came from the runtime, not reconstruction.
  const bigRequests = requests.filter(request => request.turnText.includes('R2C_BIG'));
  assert.ok(bigRequests.length >= 2, 'the model really drove the large-output turn');
  evidence.requests = requests.map(request => ({ turn: request.turnText.slice(0, 24), toolResults: request.toolResults }));
  assert.deepEqual(pageErrors, [], 'no renderer page errors');
  evidence.result = 'PASS';
} catch (error) {
  evidence.result = 'FAIL';
  evidence.error = error.stack ?? String(error);
  await capture('r2c-tool-output-failure.png').catch(() => {});
  throw error;
} finally {
  // Evidence first: Electron's close can wait on a process under load, and
  // losing the evidence would turn a real PASS into an unusable run.
  await writeFile(path.join(output, 'r2c-tool-output-evidence.json'), JSON.stringify(evidence, null, 2));
  await Promise.race([
    app?.close().catch(() => {}),
    new Promise(resolve => setTimeout(resolve, 8000)),
  ]);
  try { app?.process()?.kill('SIGKILL'); } catch { /* already gone */ }
  provider.close();
}
