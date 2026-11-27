// R5 settings convergence: the global Thinking default and the Agent browser
// product are edited through the shared config lock, the Thinking default
// reaches a real provider request, and model capability facts are read-only.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-desktop-r5-settings-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const profile = path.join(temporary, 'profile');
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
const configPath = path.join(home, 'config.toml');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
await mkdir(output, { recursive: true });

const requests = [];
const server = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const userText = (body.messages ?? []).filter(message => message.role === 'user')
    .map(message => typeof message.content === 'string' ? message.content
      : Array.isArray(message.content) ? message.content.filter(part => part.type === 'text').map(part => part.text ?? '').join('') : '')
    .join('\n');
  requests.push({ userText: userText.slice(-200), reasoning_effort: body.reasoning_effort ?? null, thinking: body.thinking ?? null });
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = (delta, finish) => `data: ${JSON.stringify({ id: 'r5-settings', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  response.write(frame({ content: 'SETTINGS_TURN_OK' }, null));
  response.write(frame({}, 'stop'));
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const initialConfig = `default_model="fixture/m"
[providers.fixture]
protocol="openai_chat"
base_url="http://127.0.0.1:${server.address().port}"
api_key="test-only"
[models.m]
provider="fixture"
model_id="m"
context_window=131072
vision=true
reasoning=true
reasoning_style="openai_effort"
supported_efforts=["minimal","low","medium","high"]
reasoning_effort="medium"
`;
await writeFile(configPath, initialConfig, { mode: 0o600 });

const leveler = process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler');
const evidence = { scope: 'real Electron/Runtime/provider request; isolated home/workspace; test-only SSE provider' };
const errors = [];
let app;
let page;
const launch = async () => {
  app = await electron.launch({
    args: [root, `--user-data-dir=${profile}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: leveler, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
};
const openPresets = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForSelector('#settings-dialog[open]');
  await page.locator('#settings-nav-list button').filter({ hasText: 'Agent 与技能' }).click();
  await page.waitForFunction(() => [...document.querySelectorAll('#settings-content h3')].some(node => node.textContent.includes('全局 Thinking 默认')), null, { timeout: 30000 });
  await page.waitForFunction(() => [...document.querySelectorAll('#settings-content h3')].some(node => node.textContent.includes('Agent 浏览器')), null, { timeout: 30000 });
};
const selectControl = async (label, optionText) => {
  await page.locator(`.select-button[aria-label="${label}"]`).click();
  await page.waitForSelector('.menu');
  await page.locator('.menu .menu-item').filter({ hasText: optionText }).first().click();
};

try {
  await launch();
  await openPresets();

  // Thinking starts at the product default (high), then an explicit low level
  // is written and used by a real request.
  const thinkingSelect = page.locator('.select-button[aria-label="Thinking 默认强度"]');
  assert.match(await thinkingSelect.innerText(), /跟随默认（高）/);
  await selectControl('Thinking 默认强度', /^低$/);
  await page.locator('#settings-content button', { hasText: '保存 Thinking 默认' }).click();
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('Thinking 默认已保存'), null, { timeout: 30000 });
  const afterThinking = await readFile(configPath, 'utf8');
  assert.match(afterThinking, /thinking = "low"/);
  evidence.thinking_config = 'thinking = "low"';
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('#settings-dialog[open]'));

  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
  await page.locator('#message').fill('THINK_WIRE');
  await page.locator('#send').click();
  await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('SETTINGS_TURN_OK')), null, { timeout: 90000 });
  const wire = requests.find(item => item.userText.includes('THINK_WIRE'));
  assert.ok(wire, 'the turn reached the provider');
  assert.equal(wire.reasoning_effort, 'low', `the saved default is what the provider receives: ${JSON.stringify(wire)}`);
  evidence.thinking_wire = wire.reasoning_effort;

  // The browser product is a real config choice with an explicit restart scope.
  await openPresets();
  const browserSelect = page.locator('.select-button[aria-label="Agent 默认浏览器"]');
  assert.ok((await browserSelect.innerText()).includes('自动'));
  await selectControl('Agent 默认浏览器', /^Google Chrome$/);
  await page.locator('#settings-content button', { hasText: '保存浏览器默认' }).click();
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('浏览器默认已保存'), null, { timeout: 30000 });
  const afterBrowser = await readFile(configPath, 'utf8');
  assert.match(afterBrowser, /\[browser\]\s*\ndefault = "chrome"/, afterBrowser);
  assert.match(await page.locator('#settings-content').innerText(), /重启 Runtime 后生效/);
  evidence.browser_default = 'chrome';
  await selectControl('Agent 默认浏览器', /^自动/);
  await page.locator('#settings-content button', { hasText: '保存浏览器默认' }).click();
  await page.waitForFunction(() => document.querySelector('.toast-text')?.textContent.includes('浏览器默认已保存'), null, { timeout: 30000 });
  const afterAuto = await readFile(configPath, 'utf8');
  assert.doesNotMatch(afterAuto, /\[browser\]/);
  evidence.browser_auto_removed = true;
  await page.screenshot({ path: path.join(output, 'r5-settings-presets.png') });
  await page.keyboard.press('Escape');

  // Model capability facts are shown read-only on the model page.
  await page.keyboard.press('Meta+,');
  await page.locator('#settings-nav-list button').filter({ hasText: '模型与 Provider' }).click();
  await page.waitForFunction(() => document.querySelectorAll('.provider-model-row').length > 0, null, { timeout: 30000 });
  const rowText = await page.locator('.provider-model-row').first().innerText();
  assert.match(rowText, /视觉/);
  assert.match(rowText, /推理/);
  assert.match(rowText, /上下文/);
  assert.match(await page.locator('#settings-content').innerText(), /只读展示/);
  assert.equal(await page.locator('.provider-model-row input, .provider-model-row select').count(), 0, 'capability facts are not editable controls');
  evidence.model_facts = rowText.replace(/\n/g, ' | ');
  await page.screenshot({ path: path.join(output, 'r5-settings-model-facts.png') });
  await page.keyboard.press('Escape');

  // A restart re-reads the same file; the chosen level survives.
  try {
    await Promise.race([
      app.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* already closed */ }
  await launch();
  await openPresets();
  assert.match(await page.locator('.select-button[aria-label="Thinking 默认强度"]').innerText(), /^低$/, 'the saved level survives restart');
  evidence.restart_persisted = true;

  assert.deepEqual(errors, []);
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r5-settings-failure.png') }).catch(() => {});
  console.error(JSON.stringify({ ...evidence, passed: false, error: error.message, errors, requests: requests.slice(-4) }, null, 2));
  process.exitCode = 1;
} finally {
  try {
    await Promise.race([
      app?.evaluate(({ app }) => app.exit(0)),
      new Promise(resolve => setTimeout(resolve, 5000)),
    ]);
  } catch { /* the test owns the app */ }
  await new Promise(resolve => server.close(resolve));
}
