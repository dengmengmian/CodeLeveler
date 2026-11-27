// R5 real-IME harness: a real Electron window driven by the macOS input
// method. The fixture provider only records requests; the product under test
// is the composer's IME handling. External keystrokes come from the OS.
import { _electron as electron } from 'playwright';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-ime-harness-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
const requests = [];
const provider = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const text = (body.messages ?? []).filter(message => message.role === 'user')
    .map(message => typeof message.content === 'string' ? message.content : '')
    .join('\n');
  requests.push(text.slice(-200));
  const answer = text.includes('IME_SEED') ? 'IME_SEED_OK' : 'IME_SENT_OK';
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = (delta, finish) => `data: ${JSON.stringify({ id: 'ime', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  response.write(frame({ content: answer }, null));
  response.write(frame({}, 'stop'));
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\n`, { mode: 0o600 });

const app = await electron.launch({
  args: [root, `--user-data-dir=${path.join(temporary, 'profile')}`],
  env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '600' },
});
const page = await app.firstWindow();
page.setDefaultTimeout(30000);
await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
await page.locator('#add-workspace').click();
await page.waitForFunction(() => document.querySelector('#workspace-picker')?.textContent.includes('workspace'));
await page.locator('#message').fill('IME_SEED');
await page.locator('#send').click();
await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('IME_SEED_OK')), null, { timeout: 60000 });
await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 60000 });

const server = createServer(async (request, response) => {
  try {
    if (request.url === '/focus') {
      await page.locator('#message').click();
      await page.locator('#message').fill('');
      await page.evaluate(() => {
        const textarea = document.querySelector('#message');
        window.__imeLog = [];
        for (const type of ['compositionstart', 'compositionupdate', 'compositionend']) {
          textarea.addEventListener(type, event => window.__imeLog.push({ type, data: event.data ?? '' }));
        }
        textarea.addEventListener('keydown', event => window.__imeLog.push({ type: 'keydown', key: event.key, keyCode: event.keyCode, isComposing: event.isComposing }));
        textarea.addEventListener('input', event => window.__imeLog.push({ type: 'input', data: event.data ?? '', isComposing: event.isComposing }));
      });
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ ok: true, pid: app.process().pid }));
      return;
    }
    if (request.url === '/state') {
      const state = await page.evaluate(() => ({
        value: document.querySelector('#message').value,
        log: window.__imeLog ?? [],
        messages: [...document.querySelectorAll('.message.user')].map(node => node.querySelector('.bubble')?.textContent ?? ''),
        status: document.querySelector('#conversation-status')?.textContent ?? '',
      }));
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ ...state, requests }));
      return;
    }
    if (request.url === '/close') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ ok: true }));
      setTimeout(async () => {
        try { await app.evaluate(({ app }) => app.exit(0)); } catch { /* already gone */ }
        await new Promise(resolve => provider.close(resolve));
        server.close();
        process.exit(0);
      }, 200);
      return;
    }
    response.writeHead(404);
    response.end();
  } catch (error) {
    response.writeHead(500, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: String(error?.message ?? error) }));
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
await writeFile('/tmp/ime-harness-port', String(server.address().port));
console.log(`IME_HARNESS_READY ${server.address().port}`);
