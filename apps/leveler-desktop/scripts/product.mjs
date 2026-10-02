// Real Electron / Rust Runtime with an explicitly test-only provider.
// Browser fixtures are real HTTP pages, not production mocks.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { provider } from '../test/fixtures/provider.mjs';

const root = fileURLToPath(new URL('..', import.meta.url));
const output = path.join(root, 'acceptance-output', 'desktop');
await mkdir(output, { recursive: true });
const temporary = await mkdtemp('/tmp/leveler-desktop-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home); await mkdir(workspace); await mkdir(path.join(workspace, 'scratch'));
await writeFile(path.join(workspace, 'fixture.txt'), 'desktop-real-tool-evidence');
await writeFile(path.join(workspace, 'scratch', 'keep.txt'), 'must-survive-denial');
const tools = await provider();
const errors = [];
const server = createServer(async (request, response) => {
  try {
    if (request.method === 'POST') {
      let text = ''; for await (const part of request) text += part;
      const body = JSON.parse(text);
      if (!(body.tools ?? []).length) {
        response.writeHead(200, { 'content-type': 'application/json' });
        response.end(JSON.stringify({ id: 'memory', choices: [{ index: 0, message: { role: 'assistant', content: '{"candidates":[]}' }, finish_reason: 'stop' }] }));
        return;
      }
      const last = [...(body.messages ?? [])].reverse().find(message => message.role === 'user');
      const content = typeof last?.content === 'string' ? last.content : JSON.stringify(last?.content);
      if (!content.includes('desktop-chat')) {
        const upstream = await fetch(`${tools.baseURL}/chat/completions`, { method: 'POST', headers: { 'content-type': 'application/json' }, body: text });
        response.writeHead(upstream.status, { 'content-type': upstream.headers.get('content-type') });
        response.end(await upstream.text()); return;
      }
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      const answer = content.includes('long')
        ? Array.from({ length: 220 }, (_, i) => `第 ${i + 1} 段：这是测试提供方生成的长对话内容，用于检查滚动、输入焦点和工作台并行显示。\n\n`).join('')
        : '可以，我们从你要完成的事情开始。你可以直接聊天，也可以选择工作区，让我检查项目、解释代码或推进修改。';
      const chunk = (delta, finish_reason = null) => ({ id: 'desktop-chat', object: 'chat.completion.chunk', created: 1, model: 'fixture-model', choices: [{ index: 0, delta, finish_reason }] });
      for (let offset = 0; offset < answer.length; offset += content.includes('long') ? 120 : 8) {
        response.write(`data: ${JSON.stringify(chunk({ content: answer.slice(offset, offset + (content.includes('long') ? 120 : 8)) }))}\n\n`);
        await new Promise(resolve => setTimeout(resolve, content.includes('long') ? 20 : 70));
      }
      response.write(`data: ${JSON.stringify(chunk({}, 'stop'))}\n\n`);
      response.end('data: [DONE]\n\n'); return;
    }
    if (request.url === '/download') {
      response.writeHead(200, { 'content-type': 'application/octet-stream', 'content-disposition': 'attachment; filename="denied.txt"' });
      response.end('must-not-save'); return;
    }
    const second = request.url === '/second';
    response.writeHead(200, { 'content-type': 'text/html' });
    response.end(`<!doctype html><html><head><meta charset="utf-8"><title>${second ? 'Second page' : 'Workbench test'}</title></head><body style="font:16px system-ui;background:#faf9f6;color:#242424;padding:40px"><small>CODELEVELER · TEST PAGE</small><h1>${second ? '第二个页面' : '工作台中的真实网页'}</h1><p>此页面通过独立的 WebContentsView 加载。</p><p>它不能访问 CodeLeveler 的桌面 API 或 Runtime 凭据。</p><a id="next" href="/second">前往第二页</a> · <a href="/download">下载测试</a><p id="ticks"></p><script>setInterval(()=>document.querySelector('#ticks').textContent='页面持续更新：'+Date.now(),100)</script></body></html>`);
  } catch (error) {
    errors.push(error.message); response.writeHead(500); response.end('fixture failure');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const baseURL = `http://127.0.0.1:${server.address().port}`;
await writeFile(path.join(home, 'config.toml'), `default_model = "fixture/m"\n[providers.fixture]\nprotocol = "openai_chat"\nbase_url = "${baseURL}"\napi_key = "test-only-fixture"\n[models.m]\nprovider = "fixture"\nmodel_id = "fixture-model"\ncontext_window = 131072\nparallel_tool_calls = false\n`, { mode: 0o600 });
let application, page;
const evidence = { provider: 'test-only fixture through real Rust Runtime', browser: 'real HTTP fixture / isolated native WebContentsView', screenshots: [], temporary_home: home };
async function screenshot(name) {
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  // Force the UI compositor to publish the new frame before capturing the window.
  await page.screenshot();
  const image = await application.evaluate(async ({ BrowserWindow, desktopCapturer, systemPreferences }) => {
    const window = BrowserWindow.getAllWindows()[0];
    if (process.platform === 'darwin' && systemPreferences.getMediaAccessStatus('screen') !== 'granted') throw new Error('Native window capture is unavailable: existing screen permission required');
    const [width, height] = window.getSize();
    const sources = await desktopCapturer.getSources({ types: ['window'], thumbnailSize: { width: width * 2, height: height * 2 } });
    const source = sources.find(source => source.id === window.getMediaSourceId());
    if (!source || source.thumbnail.isEmpty()) throw new Error('Native CodeLeveler window capture is unavailable');
    return source.thumbnail.toPNG().toString('base64');
  });
  await writeFile(path.join(output, `${name}.png`), Buffer.from(image, 'base64'));
  evidence.screenshots.push(`${name}.png`);
}
async function browserState() { return page.evaluate(() => window.desktop.browserState()); }
async function browser(command) { return page.evaluate(command => window.desktop.browserCommand(command), command); }
async function settled() {
  await page.waitForFunction(() => ['已回答', '已完成'].includes(document.querySelector('#status').textContent) && !document.querySelector('#refresh').disabled, null, { timeout: 60000 });
}
async function send(content) {
  await page.locator('#message').fill(content); await page.locator('#message').press('Enter'); await settled();
}
try {
  application = await electron.launch({ args: [root, `--user-data-dir=${path.join(temporary, 'electron')}`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '30' } });
  page = await application.firstWindow(); page.setDefaultTimeout(60000);
  const rendererErrors = []; page.on('pageerror', error => rendererErrors.push(error.message));
  await page.waitForFunction(() => window.desktop && !document.querySelector('#refresh')?.disabled);
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1280, 860));
  await page.locator('#theme').selectOption('light');
  const light = await page.evaluate(() => getComputedStyle(document.documentElement).backgroundColor);
  await screenshot('home-light');
  await page.locator('#theme').selectOption('dark');
  const dark = await page.evaluate(() => getComputedStyle(document.documentElement).backgroundColor);
  assert.notEqual(light, dark, 'Light and Dark must change the rendered surface');
  await screenshot('home-dark');
  await page.locator('#theme').selectOption('light');
  await page.locator('#message').focus();
  await page.keyboard.press('Meta+b');
  await page.waitForFunction(() => document.querySelector('#sidebar').getBoundingClientRect().width <= 60);
  const collapsed = await page.locator('#sidebar').boundingBox();
  assert.ok(collapsed.width <= 60);
  await page.keyboard.press('Meta+b');
  await page.locator('#message').fill('desktop-chat: 第一次运行');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelector('.message.assistant .body')?.textContent.length > 0);
  await screenshot('running-agent');
  await settled();
  await send('desktop-chat: 帮我梳理一下今天的工作。');
  const sessionId = await page.locator('.task.selected').getAttribute('data-session-id');
  assert.equal((await page.evaluate(id => window.desktop.snapshot(id), sessionId)).repository, null);
  await screenshot('normal-chat');

  await screenshot('workbench-overview');
  await page.locator('#browser-new').click();
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.length > 0));
  await page.locator('#browser-url').fill(baseURL);
  await page.locator('#browser-url').press('Enter');
  const first = await browserState();
  const firstId = first.selectedId;
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && tab.title === 'Workbench test' && !tab.loading)), firstId);
  const isolation = await application.evaluate(async ({ webContents }, url) => {
    const contents = webContents.getAllWebContents().find(contents => contents.getURL().startsWith(url));
    if (!contents) throw new Error(`Real Browser contents missing: ${webContents.getAllWebContents().map(contents => contents.getURL()).join(', ')}`);
    const prefs = contents.getLastWebPreferences();
    return { prefs: { sandbox: prefs.sandbox, contextIsolation: prefs.contextIsolation, nodeIntegration: prefs.nodeIntegration, preload: prefs.preload ?? null }, probes: await contents.executeJavaScript(`({desktop:typeof window.desktop,leveler:typeof window.leveler,require:typeof window.require,process:typeof window.process})`) };
  }, baseURL);
  assert.deepEqual(isolation.probes, { desktop: 'undefined', leveler: 'undefined', require: 'undefined', process: 'undefined' });
  assert.equal(isolation.prefs.sandbox, true); assert.equal(isolation.prefs.contextIsolation, true); assert.equal(isolation.prefs.nodeIntegration, false); assert.ok(!isolation.prefs.preload);
  evidence.browser_isolation = isolation;
  await page.waitForFunction(() => window.desktop.browserState().then(state => state.tabs.some(tab => !tab.loading && !tab.error)));
  await screenshot('browser');
  await browser({ type: 'navigate', id: firstId, url: `${baseURL}/second` });
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && tab.title === 'Second page' && !tab.loading)), firstId);
  assert.equal((await browserState()).tabs.find(tab => tab.id === firstId).canGoBack, true);
  await browser({ type: 'back', id: firstId });
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && tab.title === 'Workbench test' && !tab.loading)), firstId);
  await browser({ type: 'forward', id: firstId });
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && tab.title === 'Second page' && !tab.loading)), firstId);
  await browser({ type: 'reload', id: firstId });
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && !tab.loading)), firstId);
  for (const url of ['file:///etc/passwd', 'javascript:alert(1)', 'leveler-desktop://desktop/index.html', 'data:text/html,unsafe']) {
    const rejected = await page.evaluate(async ({ id, url }) => { try { await window.desktop.browserCommand({ type: 'navigate', id, url }); return false; } catch { return true; } }, { id: firstId, url });
    assert.equal(rejected, true);
  }
  const second = await browser({ type: 'new', url: baseURL });
  assert.equal(second.tabs.length, 2);
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && !tab.loading)), second.selectedId);
  await screenshot('browser-multiple-tabs');
  await browser({ type: 'select', id: firstId });
  assert.equal((await browserState()).selectedId, firstId);
  await browser({ type: 'close', id: second.selectedId });
  assert.equal((await browserState()).tabs.length, 1);
  evidence.browser_navigation = { back: true, forward: true, reload: true, new: true, select: true, close: true, privileged_protocols_rejected: true };
  const publicPage = await browser({ type: 'new', url: 'https://example.com' });
  const publicTab = publicPage.tabs.find(tab => tab.id === publicPage.selectedId);
  if (publicTab.error) throw new Error(`Public Browser navigation failed: ${publicTab.error}`);
  await page.waitForFunction(id => window.desktop.browserState().then(state => state.tabs.some(tab => tab.id === id && tab.title === 'Example Domain' && !tab.loading)), publicPage.selectedId);
  await screenshot('browser-public-page');
  evidence.public_browser = { url: publicTab.url, title: 'Example Domain', loaded: true };
  await browser({ type: 'close', id: publicPage.selectedId });
  // Long streamed conversation and continuously repainting native Browser coexist.
  await page.locator('#message').fill('desktop-chat long');
  await page.locator('#message').press('Enter');
  await page.waitForFunction(() => document.querySelectorAll('.message.assistant .body').length >= 3 && [...document.querySelectorAll('.message.assistant .body')].at(-1).textContent.length > 200);
  await page.locator('#message').fill('输入中的草稿，保留焦点');
  const beforeTyping = Date.now();
  await page.locator('#message').press('End'); await page.locator('#message').type('。');
  const inputLatency = Date.now() - beforeTyping;
  await settled();
  assert.equal(await page.locator('#message').inputValue(), '输入中的草稿，保留焦点。');
  assert.equal(await page.locator('#message').evaluate(node => node === document.activeElement), true);
  assert.ok((await page.locator('.message.assistant .body').last().innerText()).length > 10000);
  await screenshot('long-conversation');
  evidence.long_session = { generated_paragraphs: 220, input_preserved: true, focus_preserved: true, input_automation_ms: inputLatency, continuous_browser_repaint: true };
  await page.locator('#message').fill('');
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(700, 560));
  await page.waitForFunction(() => window.innerWidth <= 700);
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth), false);
  await screenshot('narrow-window');
  await page.locator('#workbench-close').click();
  await application.evaluate(({ BrowserWindow }) => new Promise((resolve, reject) => {
    const deadline = Date.now() + 2000;
    const check = () => {
      if (!BrowserWindow.getAllWindows()[0].contentView.children.some(view => view.getVisible())) return resolve();
      if (Date.now() >= deadline) return reject(new Error('Closed Workbench left a visible native Browser view'));
      setTimeout(check, 16);
    };
    check();
  }));
  await application.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0].setSize(1280, 860));
  await application.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  if(await page.evaluate(()=>document.body.classList.contains('sidebar-collapsed')))await page.locator('#sidebar-toggle').click();
  await page.locator('#space-add').click();
  await page.waitForFunction(() => document.querySelector('#workspace').textContent.includes('workspace')&&!document.querySelector('#refresh').disabled);
  await page.locator('#message').fill('fixture-interactions: read fixture.txt and propose removing scratch; I will deny approval.');
  await page.locator('#message').press('Enter');
  await page.getByRole('button', { name: '拒绝', exact: true }).waitFor();
  await screenshot('approval');
  await page.getByRole('button', { name: '拒绝', exact: true }).click(); await settled();
  await page.waitForFunction(() => document.querySelector('#interactions').children.length === 0);
  await screenshot('workspace-task');
  assert.equal(await page.locator('.message.assistant .body').last().innerText(), 'fixture-interactions-complete');
  await page.locator('.tool[data-tool-id="fixture-read"] summary').click();
  await page.locator('.tool[data-tool-id="fixture-read"] .tool-output').waitFor();
  assert.match(await page.locator('.tool[data-tool-id="fixture-read"] .tool-output').innerText(), /desktop-real-tool-evidence/);
  evidence.workspace = { live_approval: true, rejected: true, actual_tool_output: await page.locator('.tool[data-tool-id="fixture-read"]').innerText() };
  evidence.theme = { light, dark };
  evidence.ui_renderer = await page.evaluate(() => ({ require: typeof window.require, process: typeof window.process }));
  assert.deepEqual(evidence.ui_renderer, { require: 'undefined', process: 'undefined' });
  assert.deepEqual(errors, []); assert.deepEqual(tools.errors, []); assert.deepEqual(rendererErrors, []);
  evidence.renderer_errors = rendererErrors;
  await writeFile(path.join(output, 'product-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log('Desktop product verification PASS');
} catch (error) {
  if (page && !page.isClosed()) await screenshot('product-failure').catch(() => {});
  throw error;
} finally {
  if (application) await application.close();
  await tools.close(); await new Promise(resolve => server.close(resolve));
}
