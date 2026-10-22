// Real Electron + Runtime PTY acceptance; no provider or simulated terminal.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises';
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

  // ── Phase 2: a fresh terminal after the stale-host interlude. ───────────
  const startTab = () => page.locator('.pane-tab').filter({ hasText: '开始' }).first();
  const activeTerminal = () => page.locator('.terminal-view');
  const terminalRows = () => page.locator('.terminal-view .xterm-rows').innerText();
  const helper = () => page.locator('.terminal-view .xterm-helper-textarea');
  const line = async text => { await helper().focus(); await page.keyboard.type(text); await page.keyboard.press('Enter'); };
  const waitRows = (marker, timeout = 20000) => page.waitForFunction(m => document.querySelector('.terminal-view .xterm-rows')?.textContent.includes(m), marker, { timeout });
  const newTerminal = async () => {
    await startTab().click();
    await page.getByRole('button', { name: '新建终端' }).click();
    await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, { timeout: 10000 });
    return activeTerminal().getAttribute('data-terminal-id');
  };
  let captureSeq = 0;
  const capture = async (name, send) => {
    const seq = ++captureSeq;
    await line(`printf 'CAPREADY_${seq}_%s\\n' OK`);
    await waitRows(`CAPREADY_${seq}_OK`).catch(async error => {
      console.error('CAPREADY TIMEOUT', JSON.stringify(await terminalRows()));
      throw error;
    });
    await line(`cat > ${name}`);
    await page.waitForTimeout(300);
    await send();
    await page.keyboard.press('Enter');
    await page.keyboard.press('Control+d');
    await page.waitForTimeout(200);
    await line(`printf 'CAPDONE_${seq}_%s\\n' OK`);
    await waitRows(`CAPDONE_${seq}_OK`);
  };

  const ansiId = await newTerminal();
  await line("printf '\\033[31mANSI_RED\\033[0m\\n'");
  await waitRows('ANSI_RED');
  const ansiColor = await page.evaluate(() => {
    const rows = document.querySelector('.terminal-view .xterm-rows');
    const normal = getComputedStyle(rows).color;
    const hits = [...rows.querySelectorAll('span')].filter(span => span.textContent.includes('ANSI_RED'));
    const colored = [...rows.querySelectorAll('span')].filter(span => {
      const color = getComputedStyle(span).color;
      return color !== normal && /\S/.test(span.textContent ?? '');
    });
    return { normal, hits: hits.map(span => ({ cls: span.className, color: getComputedStyle(span).color, html: span.outerHTML.slice(0, 240) })), colored: colored.slice(0, 5).map(span => ({ cls: span.className, color: getComputedStyle(span).color })) };
  });
  assert.ok(ansiColor.colored.length > 0, `ANSI color was not rendered: ${JSON.stringify(ansiColor)}`);

  await line("seq 1 3000; printf 'LONG_END\\n'");
  await waitRows('LONG_END', 30000);
  assert.equal(await activeTerminal().getAttribute('data-status'), 'running');

  await capture('tabs.bin', async () => { await page.keyboard.type('A'); await page.keyboard.press('Tab'); await page.keyboard.type('B'); });
  assert.equal((await readFile(path.join(workspace, 'tabs.bin'), 'utf8')).replace(/\n$/, ''), 'A\tB');
  await capture('arrows.bin', async () => { await page.keyboard.press('ArrowUp'); await page.keyboard.type('X'); });
  assert.match((await readFile(path.join(workspace, 'arrows.bin'), 'utf8')).replace(/\n$/, ''), /\x1b\[[A-D]X/, 'arrow key did not reach the PTY');
  await capture('chinese.bin', async () => { await page.keyboard.insertText('中文输入'); });
  assert.equal((await readFile(path.join(workspace, 'chinese.bin'), 'utf8')).replace(/\n$/, ''), '中文输入');
  await app.evaluate(({ clipboard }) => clipboard.writeText('PASTE_MARK'));
  await capture('paste.bin', async () => { await page.keyboard.press('Meta+v'); await page.waitForTimeout(250); });
  assert.match((await readFile(path.join(workspace, 'paste.bin'), 'utf8')).replace(/\n$/, ''), /PASTE_MARK/);
  await line("printf 'COPY_MARK_123\\n'");
  await waitRows('COPY_MARK_123');
  const copyRow = page.locator('.terminal-view .xterm-rows > div').filter({ hasText: 'COPY_MARK_123' }).last();
  const copyBox = await copyRow.boundingBox();
  assert.ok(copyBox, 'the copy row is not measurable');
  await page.mouse.move(copyBox.x + 2, copyBox.y + copyBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(copyBox.x + Math.min(copyBox.width, 400) - 2, copyBox.y + copyBox.height / 2, { steps: 8 });
  await page.mouse.up();
  await page.keyboard.press('Meta+c');
  await page.waitForTimeout(200);
  assert.match(await app.evaluate(({ clipboard }) => clipboard.readText()), /COPY_MARK_123/);

  const sizeLine = async marker => {
    await line(`printf '${marker}_%s\\n' \"$(stty size)\"`);
    await page.waitForFunction(m => new RegExp(m + '_\\d+ \\d+').test(document.querySelector('.terminal-view .xterm-rows')?.textContent ?? ''), marker, { timeout: 20000 });
    const text = await terminalRows();
    const match = text.match(new RegExp(`${marker}_(\\d+) (\\d+)`));
    return match ? { rows: Number(match[1]), cols: Number(match[2]) } : { rows: NaN, cols: NaN };
  };
  const sizeBefore = await sizeLine('SIZE');
  await page.locator('.pane-chrome [aria-label="全屏"]').click();
  await page.waitForTimeout(600);
  const sizeFull = await sizeLine('FULL');
  assert.ok(sizeFull.cols > sizeBefore.cols, `fullscreen did not resize the PTY: ${JSON.stringify({ sizeBefore, sizeFull })}`);
  await page.locator('.pane-chrome [aria-label="退出全屏"]').click();
  await page.waitForTimeout(400);

  await line("sleep 2; printf 'HIDDEN_DONE\\n'");
  await page.locator('#rightbar-head [aria-label="收起右侧边栏"], .pane-chrome [aria-label="收起右侧边栏"]').first().click().catch(() => {});
  await page.waitForFunction(() => document.querySelector('#rightbar')?.hidden);
  await page.waitForTimeout(3000);
  await page.locator('#header-right-toggle:visible, #floating-right-toggle:visible').click();
  await waitRows('HIDDEN_DONE', 10000);

  const ansiPid = Number((await (async () => { await line("printf 'APID_%s\\n' $$"); await waitRows('APID_'); return (await terminalRows()).match(/APID_(\d+)/)?.[1]; })()));
  assert.ok(ansiPid > 0);

  const sessionIdBeforeSwitch = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await page.locator('#new-session').click();
  await page.waitForTimeout(500);
  await page.locator(`.session-row[data-session-id="${sessionIdBeforeSwitch}"]`).click();
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, { timeout: 10000 });
  assert.equal(await activeTerminal().getAttribute('data-terminal-id'), ansiId, 'the terminal tab was lost on a session switch');

  const bridgePid = Number(execFileSync('pgrep', ['-P', String(app.process().pid), '-f', 'desktop-bridge'], { encoding: 'utf8' }).trim().split('\n')[0]);
  process.kill(bridgePid, 'SIGKILL');
  await page.waitForTimeout(2500);
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, { timeout: 20000 });
  console.error('POST-KILL STATE', JSON.stringify(await page.evaluate(() => ({ status: document.querySelector('.terminal-view')?.dataset.status, error: document.querySelector('.terminal-error-text')?.textContent }))));
  await line("printf 'AFTER_RECONNECT\\n'");
  await waitRows('AFTER_RECONNECT', 20000).catch(async error => {
    console.error('RECONNECT SCREEN', JSON.stringify(await page.evaluate(() => ({ status: document.querySelector('.terminal-view')?.dataset.status, text: document.querySelector('.terminal-status')?.innerText, rows: document.querySelector('.terminal-view .xterm-rows')?.textContent?.slice(-500) }))));
    throw error;
  });

  const exitId = await newTerminal();
  await line('exit 7');
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'exited', null, { timeout: 10000 });
  assert.match(await page.locator('.terminal-view .terminal-status').innerText(), /退出码 7/);
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForFunction(id => !document.querySelector(`.terminal-view[data-terminal-id="${id}"]`), exitId);

  const thirdId = await newTerminal();
  await line("printf 'TERM_C\\n'");
  await waitRows('TERM_C');
  const tabs = page.locator('.pane-tab');
  await tabs.nth(1).click();
  await page.waitForTimeout(300);
  assert.doesNotMatch(await terminalRows(), /TERM_C/, 'terminal tabs shared output');
  await tabs.nth(2).click();
  await page.waitForTimeout(300);
  assert.match(await terminalRows(), /TERM_C/);
  const thirdPid = Number((await (async () => { await line("printf 'CPID_%s\\n' $$"); await waitRows('CPID_'); return (await terminalRows()).match(/CPID_(\d+)/)?.[1]; })()));
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForTimeout(300);
  assert.throws(() => process.kill(thirdPid, 0), { code: 'ESRCH' });
  // Close the first phase-2 terminal and prove its shell is gone too.
  await tabs.nth(1).click();
  await page.waitForTimeout(200);
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.waitForTimeout(400);
  assert.throws(() => process.kill(ansiPid, 0), { code: 'ESRCH' });
  assert.equal(thirdId.length > 0, true);
  console.log('Terminal dogfood: ANSI, long output, Tab/arrow bytes, IME insert, copy/paste, fullscreen resize, hidden output, session switch, bridge reconnect, exit code, tab isolation and orphan cleanup passed.');
} finally {
  if(app && terminalParams) { const page=await app.firstWindow().catch(()=>null); await page?.evaluate(params=>window.desktop.closeTerminal(params),terminalParams).catch(()=>{}); }
  if (app) { await app.evaluate(({ app }) => app.exit(0)).catch(() => {}); await app.close().catch(() => {}); }
  await rm(temporary, { recursive: true, force: true });
}
