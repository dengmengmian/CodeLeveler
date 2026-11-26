// R2a real-local dogfood: the user's actual macOS HOME/profile, real Desktop,
// real Runtime, real configured providers and real history.
//
// The only configuration change is the persisted default model: it is moved to
// another configured model for the new-session check and then restored. The
// evidence asserts the file is byte-identical to its pre-test state afterwards.
// Existing sessions and workspaces are read-only; exactly one new test session
// is created in a temporary workspace.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
// Real-profile acceptance is manual: it reads the user's local provider
// configuration (in memory only) and can make real provider requests. It must
// never run implicitly in CI or a routine test sweep.
if (process.env.LEVELER_REAL_PROFILE_ACCEPTANCE !== '1') {
  console.error('Set LEVELER_REAL_PROFILE_ACCEPTANCE=1 to run the real-profile acceptance.');
  process.exit(2);
}
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const rendererSha = createHash('sha256').update(readFileSync(path.join(root, 'src/renderer.mjs'))).digest('hex');
const providerUiSha = createHash('sha256').update(readFileSync(path.join(root, 'src/provider-settings.mjs'))).digest('hex');
const configPath = path.join(homedir(), '.leveler', 'config.toml');
const configBefore = readFileSync(configPath, 'utf8');
const configHashBefore = createHash('sha256').update(configBefore).digest('hex');
// Kept in memory only, to prove the key never reaches a client surface.
function realProviderKey() {
  let inSection = false;
  for (const line of configBefore.split(/\r?\n/)) {
    const section = /^\[providers\.(?:"?)([A-Za-z0-9._-]+)(?:"?)\]\s*$/.exec(line.trim());
    if (section) { inSection = section[1] === 'bigmodel'; continue; }
    if (!inSection) continue;
    const match = /^api_key\s*=\s*"((?:[^"\\]|\\.)*)"\s*$/.exec(line.trim());
    if (match) return JSON.parse(`"${match[1]}"`);
  }
  return null;
}
const realKey = realProviderKey();
const workspace = await mkdtemp('/tmp/leveler-r2a-dogfood-');
const evidence = {
  scope: 'real local HOME/profile, real Desktop/Runtime/providers/history; temp workspace; one new test session; default model restored',
  binary: { path: leveler, sha256: binarySha },
  renderer: { sha256: rendererSha },
  provider_ui: { sha256: providerUiSha },
  workspace,
  real_config: { path: configPath, sha256_before: configHashBefore },
  steps: {},
};
let app;
let page;
const errors = [];
const consoleText = [];

const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => consoleText.push(message.text()));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
  await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 30000 });
};
const tasks = () => page.evaluate(() => window.desktop.listTasks({ include_archived: false }));
const taskCount = async () => (await tasks()).tasks?.length ?? 0;
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const assistantCount = () => page.locator('.message.assistant .markdown').count();
const userCount = () => page.locator('.message.user').count();
const settledReply = previous => page.waitForFunction(prev => {
  const status = document.querySelector('#conversation-status')?.textContent ?? '';
  return document.querySelectorAll('.message.assistant .markdown').length > prev
    && status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
}, previous, { timeout: 420000 });
const send = async text => {
  const before = await userCount();
  await page.locator('#message').fill(text);
  await page.locator('#send').click();
  await page.waitForFunction(before => document.querySelectorAll('.message.user').length > before, before, { timeout: 30000 });
  await page.waitForTimeout(300);
};
const replyText = () => page.locator('.message.assistant .markdown').last().innerText().catch(() => '');
const openModels = async () => {
  await page.locator('#account-menu').click();
  await page.getByRole('menuitem', { name: '设置' }).click();
  await page.waitForFunction(() => document.querySelector('#settings-dialog')?.open === true);
  await page.locator('#settings-nav-list').getByRole('button', { name: '模型与 Provider' }).click();
  await page.waitForFunction(() => document.querySelector('.provider-page-head'));
};
const closeSettings = async () => {
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => document.querySelector('#settings-dialog')?.open === false);
};
const providerCard = id => page.locator('.provider-card').filter({ hasText: id }).first();
const escapeText = value => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const modelRow = (provider, model) => providerCard(provider)
  .locator('.provider-model-row')
  .filter({ has: page.locator('.provider-name', { hasText: new RegExp(`^${escapeText(model)}$`) }) })
  .first();
const hostView = (session = null) => page.evaluate(id => window.desktop.providers({ selection: null, sessionId: id }), session);
const waitRuntimeCurrent = async session => {
  await page.waitForFunction(async id => {
    const view = await window.desktop.providers({ selection: null, sessionId: id });
    return view.runtime?.config_current === true;
  }, session, { timeout: 30000 });
};
const ensureModels = async () => {
  const open = await page.evaluate(() => document.querySelector('#settings-dialog')?.open === true);
  if (!open) { await openModels(); return; }
  await page.locator('#settings-nav-list').getByRole('button', { name: '模型与 Provider' }).click();
  await page.waitForFunction(() => document.querySelector('.provider-page-head'));
};
const setDefault = async (provider, model) => {
  await ensureModels();
  await modelRow(provider, model).getByRole('button', { name: '设为默认' }).click();
  await page.waitForFunction(({ provider, model }) => [...document.querySelectorAll('.provider-card')]
    .find(node => node.textContent.includes(provider))?.textContent.includes('默认来源')
    && [...document.querySelectorAll('.provider-model-row')]
      .some(row => row.textContent.includes(model) && row.textContent.includes('默认')),
  { provider, model }, { timeout: 30000 });
  await closeSettings();
};

try {
  await launch();
  const bridgeLine = execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' })
    .split('\n').find(line => line.includes(`${leveler} desktop-bridge`));
  assert.ok(bridgeLine, `the running bridge must be ${leveler}`);
  evidence.binary.bridge_process = bridgeLine.trim();
  assert.equal(await page.evaluate(() => document.body.classList.contains('home')), true);
  const baseline = await taskCount();
  const baselineView = await hostView();
  assert.equal((await tasks()).source_errors?.length ?? 0, 0, 'existing task sources must stay readable');
  assert.ok(baselineView.configuration_complete, JSON.stringify(baselineView.issues));
  assert.ok(baselineView.providers.some(provider => provider.id === 'bigmodel'));
  assert.ok(baselineView.providers.some(provider => provider.id === 'deepseek'));
  assert.ok(baselineView.models.some(model => model.provider === 'bigmodel' && model.model === 'glm-5.3'));
  assert.ok(baselineView.models.some(model => model.provider === 'deepseek' && model.model === 'deepseek-v4.1-flash-1017'));
  const originalDefault = baselineView.default_model;
  const original = baselineView.resolved_default_model;
  assert.ok(original, 'the configured default must resolve');
  evidence.steps.baseline = {
    state: 'PASS',
    tasks: baseline,
    source_errors: 0,
    providers: baselineView.providers.map(provider => ({ id: provider.id, key: provider.key_source, models: provider.model_count })),
    default_model: originalDefault,
  };
  const testTarget = baselineView.models
    .map(model => ({ provider: model.provider, model: model.model }))
    .find(model => model.provider === 'bigmodel' && model.model === 'glm-5.3' && `${model.provider}/${model.model}` !== originalDefault)
    ?? baselineView.models
      .map(model => ({ provider: model.provider, model: model.model }))
      .find(model => `${model.provider}/${model.model}` !== originalDefault);
  assert.ok(testTarget, 'another configured model is required for the default check');

  // 1) The provider page reflects the real configuration and really connects.
  await openModels();
  const cardText = await providerCard('bigmodel').innerText();
  assert.match(cardText, /已保存密钥/);
  assert.match(cardText, /glm-5\.3/);
  await providerCard('bigmodel').getByRole('button', { name: '测试连接' }).click();
  await page.waitForFunction(() => {
    const line = [...document.querySelectorAll('.provider-test')].find(node => node.textContent.includes('连接') || node.textContent.includes('失败'));
    return line && !line.textContent.includes('正在测试');
  }, null, { timeout: 120000 });
  const testText = await page.locator('.provider-test').first().innerText();
  assert.match(testText, /连接正常/, testText);
  await page.screenshot({ path: path.join(output, 'r2a-04-real-provider-settings.png') });
  evidence.steps.provider_surface = { state: 'PASS', connection: testText.trim() };

  // 2) Set a different configured model as the default. No runtime is attached
  // yet, so the first daemon will load this generation; the host read is the
  // authority until then.
  await setDefault(testTarget.provider, testTarget.model);
  const afterDefault = await hostView();
  assert.equal(afterDefault.default_model, `${testTarget.provider}/${testTarget.model}`);
  assert.notEqual(await readFile(configPath, 'utf8'), configBefore, 'the default change must reach the real config');
  evidence.steps.default_change = { state: 'PASS', from: originalDefault, to: afterDefault.default_model };

  // 3) A new session inherits it and completes a real turn with a tool call.
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(() => (document.querySelector('#workspace-picker')?.textContent ?? '').includes('leveler-r2a-dogfood-'));
  assert.equal(await taskCount(), baseline, 'choosing a workspace must not create a session');
  const beforeTurn = await assistantCount();
  await send('在当前工作区创建文件 r2a-dogfood.txt，内容为 R2A_DOGFOOD_TOOL_OK，然后读取它确认。完成后只回复 R2A_DOGFOOD_TOOL_OK。');
  await settledReply(beforeTurn);
  const createdId = await sessionId();
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), createdId);
  assert.equal(snapshot.model.provider, testTarget.provider);
  assert.equal(snapshot.model.model, testTarget.model);
  assert.equal(await taskCount(), baseline + 1, 'the first send must create exactly one session');
  const toolFile = path.join(workspace, 'r2a-dogfood.txt');
  const toolOk = existsSync(toolFile) && (await readFile(toolFile, 'utf8')).includes('R2A_DOGFOOD_TOOL_OK');
  assert.ok(toolOk, 'the real Agent turn must create the file');
  assert.match(await replyText(), /R2A_DOGFOOD_TOOL_OK/);
  evidence.steps.real_turn = { state: 'PASS', session: createdId, model: snapshot.model, file: toolOk, reply: (await replyText()).slice(0, 120).trim() };

  // 4) Restore the original default. The real file must be byte-identical.
  await setDefault(original.provider, original.model);
  await waitRuntimeCurrent(createdId);
  const restored = await hostView(createdId);
  assert.equal(restored.default_model, originalDefault);
  const configAfter = readFileSync(configPath, 'utf8');
  assert.equal(createHash('sha256').update(configAfter).digest('hex'), configHashBefore, 'restoring the default must leave the real config byte-identical');
  evidence.steps.default_restored = { state: 'PASS', default_model: restored.default_model, config_byte_identical: true };

  // 5) Restart: configuration, model, the new session and history survive.
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  assert.equal(await taskCount(), baseline + 1, 'restart must keep exactly the sessions it had');
  assert.equal((await tasks()).source_errors?.length ?? 0, 0);
  const restarted = await hostView();
  assert.equal(restarted.default_model, originalDefault);
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), createdId);
  await page.locator(`.session-row[data-session-id="${createdId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  const historyText = await page.locator('#conversation').innerText();
  assert.ok(historyText.includes('r2a-dogfood.txt'), 'the new session history must survive the restart');
  evidence.steps.restart = { state: 'PASS', tasks: await taskCount(), default_model: restarted.default_model, new_session_readable: true };

  // 6) An existing historical session still opens read-only.
  const index = await tasks();
  const historical = (index.tasks ?? []).find(task => task.id !== createdId
    && task.workspace_available === true
    && ['completed', 'answered', 'idle'].includes(task.status)
    && typeof task.primary_workspace === 'string' && existsSync(task.primary_workspace));
  if (historical) {
    const beforeOpen = await taskCount();
    await page.locator(`.session-row[data-session-id="${historical.id}"]`).click();
    await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
    await page.waitForFunction(() => document.querySelectorAll('#conversation .message').length > 0, null, { timeout: 60000 });
    assert.equal(await taskCount(), beforeOpen, 'opening a historical session must not create one');
    evidence.steps.historical_session = { state: 'PASS', session: historical.id, messages: await page.locator('#conversation .message').count() };
  } else {
    evidence.steps.historical_session = { state: 'BLOCKED', reason: 'no historical session with an existing workspace was in the current list' };
  }

  // 7) Credential hygiene in the real client.
  const storage = await page.evaluate(() => JSON.stringify(localStorage));
  const pageHtml = await page.evaluate(() => document.documentElement.outerHTML);
  const configAfterAll = readFileSync(configPath, 'utf8');
  const keyPresent = text => Boolean(realKey) && text.includes(realKey);
  assert.equal(keyPresent(storage), false, 'no provider key may appear in localStorage');
  assert.equal(keyPresent(pageHtml), false, 'no provider key may appear in the DOM');
  assert.equal(keyPresent(consoleText.join('\n')), false, 'no provider key may appear in console logs');
  assert.equal(createHash('sha256').update(configAfterAll).digest('hex'), configHashBefore);
  evidence.steps.credential_hygiene = { state: 'PASS', local_storage: false, dom: false, console: false, real_config_unchanged: true };

  assert.deepEqual(errors, []);
  Object.assign(evidence, { passed: Object.values(evidence.steps).every(step => step.state === 'PASS'), page_errors: errors });
  await writeFile(path.join(output, 'r2a-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify({ passed: evidence.passed, steps: Object.keys(evidence.steps) }, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'r2a-dogfood-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'r2a-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) {
    await Promise.race([
      app.evaluate(({ app }) => app.exit(0)).catch(() => {}),
      new Promise(resolve => setTimeout(resolve, 8000)),
    ]);
    await Promise.race([app.close().catch(() => {}), new Promise(resolve => setTimeout(resolve, 4000))]);
  }
  // Stop the test workspace's daemon only; the real profile's runtime is not
  // this test's to stop.
  const daemons = [];
  try {
    const lines = execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' }).split('\n');
    for (const line of lines) {
      if (!line.includes(leveler) || !line.includes(' serve') || !line.includes(workspace)) continue;
      const pid = Number(line.trim().split(/\s+/)[0]);
      if (Number.isSafeInteger(pid) && pid > 0) { daemons.push(pid); try { process.kill(pid, 'SIGTERM'); } catch { /* gone */ } }
    }
  } catch { /* best effort */ }
  for (let attempt = 0; attempt < 25; attempt += 1) {
    if (!daemons.some(pid => { try { process.kill(pid, 0); return true; } catch { return false; } })) break;
    await new Promise(resolve => setTimeout(resolve, 200));
  }
  // The temporary workspace is this test's artifact; the archived test session
  // keeps its history in the runtime store.
  await new Promise(resolve => setTimeout(resolve, 500));
  await rm(workspace, { recursive: true, force: true }).catch(() => {});
}
