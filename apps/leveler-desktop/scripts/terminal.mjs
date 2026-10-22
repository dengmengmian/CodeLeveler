// Real Electron + Runtime PTY acceptance; no provider or simulated terminal.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { realpath } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { setTimeout as delay } from 'node:timers/promises';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-terminal-');
const workspace = path.join(temporary, 'workspace');
const home = path.join(temporary, 'home');
await mkdir(workspace); await mkdir(home);
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:9"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n');
let app;
let terminalParams;
try {
  app = await electron.launch({ args: [root, `--user-data-dir=${temporary}/electron`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, ZDOTDIR: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler') } });
  const page = await app.firstWindow();
  page.on('response', response => {if(response.status()>=400)console.error('RESOURCE',response.status(),response.url());});
  page.on('requestfailed', request => console.error('REQUEST FAILED',request.url(),request.failure()));
  page.on('pageerror', error => console.error('PAGE ERROR', error));
  page.on('console', message => { if (message.type() === 'error') console.error(message.text()); });
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.includes('v'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const id = await page.evaluate(async () => { const folder = await window.desktop.chooseFolder(); return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id; });
  await page.reload();
  await page.locator(`.session-row[data-session-id="${id}"]`).click();
  await page.locator('#header-right-toggle:visible, #floating-right-toggle:visible').click();
  await page.getByRole('button', { name: '新建终端' }).click();
  await page.locator('.xterm-helper-textarea').waitFor({ state: 'attached', timeout: 5000 });
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, {timeout:10000}).catch(async error => { console.error('TERMINAL STATUS', await page.locator('.terminal-status').innerText()); throw error; });
  const input = page.locator('.xterm-helper-textarea');
  await input.focus();
  await page.keyboard.type("printf 'TERMINAL_REAL_%s\\n' OK; pwd; echo $$"); await page.keyboard.press('Enter');
  const content = () => page.locator('.xterm-rows').innerText();
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('TERMINAL_REAL_OK'));
  assert.ok((await content()).includes(await realpath(workspace)));
  const terminalId = await page.locator('.terminal-view').getAttribute('data-terminal-id');
  terminalParams={session_id:id,terminal_id:terminalId};
  await input.focus();
  await page.keyboard.type("test -t 0 && test -t 1 && printf 'PTY_%s\\n' REAL"); await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('PTY_REAL'));
  await page.evaluate(params => window.desktop.resizeTerminal({...params,cols:93,rows:31}),{session_id:id,terminal_id:terminalId});
  await page.keyboard.type(`printf 'SIZE_%s\\n' "$(stty size)"`); await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('SIZE_31 93'));
  await page.locator('#rightbar [aria-label="收起右侧边栏"]').click();
  await page.locator('#header-right-toggle:visible, #floating-right-toggle:visible').click();
  assert.equal(await page.locator('.terminal-view').getAttribute('data-terminal-id'), terminalId);
  const active=await page.evaluate(params=>window.desktop.observeTerminal({...params,cursor:0,max_bytes:65536}),{session_id:id,terminal_id:terminalId});
  await input.focus(); await page.keyboard.type('sleep 30'); await page.keyboard.press('Enter');
  let sleepStarted=false;
  const deadline=Date.now()+5000;
  while(Date.now()<deadline) {
    try { sleepStarted=Boolean(execFileSync('/usr/bin/pgrep',['-P',String(active.pid),'-x','sleep'],{encoding:'utf8',stdio:['ignore','pipe','ignore']}).trim()); } catch { sleepStarted=false; }
    if(sleepStarted)break;
    await delay(20);
  }
  assert.ok(sleepStarted,'the real foreground sleep must exist before sending Ctrl+C');
  await page.keyboard.press('Control+c');
  await page.keyboard.type("printf 'INTERRUPTED_%s\\n' OK"); await page.keyboard.press('Enter');
  await page.waitForFunction(() => document.querySelector('.xterm-rows')?.textContent.includes('INTERRUPTED_OK'),null,{timeout:5000}).catch(async error=>{ console.error('CTRL-C SCREEN',await content()); throw error; });
  await page.keyboard.type(`sleep 30 & printf 'CHILD_%s\\n' $!`); await page.keyboard.press('Enter');
  await page.waitForFunction(() => /CHILD_\d+/.test(document.querySelector('.xterm-rows')?.textContent ?? ''),null,{timeout:5000}).catch(async error=>{ console.error('BACKGROUND SCREEN',await content()); throw error; });
  const childPid=Number((await content()).match(/CHILD_(\d+)/)[1]);
  process.kill(childPid,0);
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.locator('.terminal-view').waitFor({ state: 'hidden' });
  const observed = await page.evaluate(async ({ session_id, terminal_id }) => window.desktop.observeTerminal({ session_id, terminal_id, cursor: 0, max_bytes: 65536 }), { session_id: id, terminal_id: terminalId });
  assert.ok(['exited', 'killed'].includes(observed.status), JSON.stringify(observed));
  assert.throws(()=>process.kill(childPid,0),{code:'ESRCH'});
  assert.throws(()=>process.kill(observed.pid,0),{code:'ESRCH'});
  // A stale Execution Host (one started before terminal.pty existed) refuses
  // creation. The tab must explain that in operator terms, keep the runtime
  // text copyable, and close without asking the host to stop a task it never
  // admitted.
  await app.evaluate(({ ipcMain }) => {
    globalThis.__realTerminalCreate = ipcMain._invokeHandlers.get('desktop:terminal-create');
    globalThis.__realTerminalClose = ipcMain._invokeHandlers.get('desktop:terminal-close');
    globalThis.__terminalCloseCalls = 0;
    ipcMain.removeHandler('desktop:terminal-create');
    ipcMain.removeHandler('desktop:terminal-close');
    ipcMain.handle('desktop:terminal-create', () => { throw new Error('runtime error: Unsupported: Execution Host missing terminal.pty; existing services preserved'); });
    ipcMain.handle('desktop:terminal-close', (...args) => { globalThis.__terminalCloseCalls += 1; return globalThis.__realTerminalClose(...args); });
  });
  await page.getByRole('button', { name: '新建终端' }).click();
  await page.waitForFunction(() => document.querySelector('.terminal-status')?.textContent.includes('不支持终端'), null, { timeout: 10000 });
  const staleSummary = await page.locator('.terminal-status').innerText();
  assert.match(staleSummary, /当前执行宿主不支持终端/);
  await page.locator('.terminal-error summary').click();
  const staleRaw = await page.locator('.terminal-error-text').innerText();
  assert.match(staleRaw, /missing terminal\.pty/);
  await page.locator('.terminal-error-copy').click();
  await page.waitForTimeout(150);
  assert.equal(await app.evaluate(({ clipboard }) => clipboard.readText()), staleRaw);
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForFunction(() => !document.querySelector('.terminal-view'), null, { timeout: 5000 });
  assert.equal(await app.evaluate(() => globalThis.__terminalCloseCalls), 0, 'a never-admitted terminal must not be stopped');
  await app.evaluate(({ ipcMain }) => {
    ipcMain.removeHandler('desktop:terminal-create');
    ipcMain.removeHandler('desktop:terminal-close');
    ipcMain.handle('desktop:terminal-create', globalThis.__realTerminalCreate);
    ipcMain.handle('desktop:terminal-close', globalThis.__realTerminalClose);
  });
  console.log('Real terminal: shell output, cwd, isatty, stty resize, hidden/reopen identity, Ctrl+C, shell/job process disappearance passed.');
  console.log('Stale host refusal: friendly summary, copyable raw detail, local close without a stop call passed.');
} finally {
  if(app && terminalParams) { const page=await app.firstWindow().catch(()=>null); await page?.evaluate(params=>window.desktop.closeTerminal(params),terminalParams).catch(()=>{}); }
  if (app) { await app.evaluate(({ app }) => app.exit(0)).catch(() => {}); await app.close().catch(() => {}); }
  await rm(temporary, { recursive: true, force: true });
}
