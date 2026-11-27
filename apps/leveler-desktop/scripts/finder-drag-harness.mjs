// R5 real Finder drag harness: the drop comes from the OS, the fixture
// provider only records what the Runtime actually sent.
import { _electron as electron } from 'playwright';
import { createServer } from 'node:http';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-dnd-harness-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
await mkdir(home, { recursive: true });
await mkdir(workspace, { recursive: true });
const requests = [];
const provider = createServer(async (request, response) => {
  let raw = '';
  for await (const part of request) raw += part;
  const body = JSON.parse(raw);
  const userMessages = (body.messages ?? []).filter(message => message.role === 'user');
  const last = userMessages.at(-1);
  const text = typeof last?.content === 'string' ? last.content
    : Array.isArray(last?.content) ? last.content.filter(part => part.type === 'text').map(part => part.text ?? '').join('') : '';
  const images = Array.isArray(last?.content) ? last.content.filter(part => part.type === 'image_url').map(part => {
    const url = String(part.image_url?.url ?? '');
    const bytes = Buffer.from(url.slice(url.indexOf(',') + 1), 'base64');
    return bytes.length > 24 ? [bytes.readUInt32BE(16), bytes.readUInt32BE(20)] : [0, 0];
  }) : [];
  requests.push({ text: text.slice(-120), images });
  const answer = text.includes('DND_REAL') ? 'DND_DONE' : 'DND_SEED_OK';
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  const frame = (delta, finish) => `data: ${JSON.stringify({ id: 'dnd', object: 'chat.completion.chunk', created: 1, model: 'fixture', choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;
  response.write(frame({ content: answer }, null));
  response.write(frame({}, 'stop'));
  response.end('data: [DONE]\n\n');
});
await new Promise(resolve => provider.listen(0, '127.0.0.1', resolve));
await writeFile(path.join(home, 'config.toml'), `default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:${provider.address().port}"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="m"\ncontext_window=131072\nvision=true\n`, { mode: 0o600 });

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
await page.locator('#message').fill('DND_SEED');
await page.locator('#send').click();
await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('DND_SEED_OK')), null, { timeout: 60000 });
await page.waitForFunction(() => !document.querySelector('#model-picker')?.disabled, null, { timeout: 60000 });
await page.locator('#message').fill('');

const server = createServer(async (request, response) => {
  try {
    if (request.url === '/state') {
      const state = await page.evaluate(() => {
        const card = document.querySelector('#composer-card').getBoundingClientRect();
        return {
          pid: window.desktop ? undefined : undefined,
          chips: [...document.querySelectorAll('.attachment-chip')].map(chip => ({
            status: chip.dataset.attachmentStatus,
            text: chip.innerText,
          })),
          messages: [...document.querySelectorAll('.message.user')].map(node => node.querySelector('.bubble')?.textContent ?? ''),
          status: document.querySelector('#conversation-status')?.textContent ?? '',
          composer: { x: card.x, y: card.y, width: card.width, height: card.height },
        };
      });
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ ...state, pid: app.process().pid, requests }));
      return;
    }
    if (request.url === '/send') {
      await page.locator('#message').fill('DND_REAL 拖入图片');
      await page.locator('#send').click();
      await page.waitForFunction(() => [...document.querySelectorAll('.message.assistant .markdown')].some(node => node.textContent.includes('DND_DONE')), null, { timeout: 120000 });
      const sent = requests.find(item => item.text.includes('DND_REAL'));
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ ok: true, sent: sent ?? null, requests }));
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
await writeFile('/tmp/dnd-harness-port', String(server.address().port));
console.log(`DND_HARNESS_READY ${server.address().port} ${app.process().pid}`);
