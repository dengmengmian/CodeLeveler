// LaunchServices entry-point acceptance (the Finder/Dock path).
// The app is started by `open` (launchd environment, no developer shell
// inheritance) and driven through the renderer's remote-debugging endpoint.
// It uses the real LEVELER_HOME and an existing session so the native folder
// picker is not required; the terminal command only creates and removes a
// probe file under HOME.
import { chromium } from 'playwright';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { homedir } from 'node:os';
import path from 'node:path';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-terminal-finder-');
const output = path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
const electronApp = path.join(root, 'node_modules/electron/dist/Electron.app');
const profile = path.join(temporary, 'electron');
const port = 9400 + Math.floor(Math.random() * 300);
const stdout = path.join(temporary, 'stdout.log');
const stderr = path.join(temporary, 'stderr.log');
const evidence = { scope: 'LaunchServices (open) entry, real LEVELER_HOME, existing session, real zsh', passed: false };
let browser, page, electronPid;
const rows = () => page.locator('.terminal-view .xterm-rows').innerText();
const helper = () => page.locator('.terminal-view .xterm-helper-textarea');
async function line(text) {
  await helper().focus();
  await page.keyboard.type(text);
  await page.keyboard.press('Enter');
}
async function waitRows(marker, timeout = 20000) {
  await page.waitForFunction(value => document.querySelector('.terminal-view .xterm-rows')?.textContent.includes(value), marker, { timeout });
}
async function probe(tag) {
  await line(`printf 'PROBE_%s_HOME=%s\\n' ${tag} "$HOME"`);
  await waitRows(`PROBE_${tag}_HOME=${homedir()}`);
  await line(`test_file="$(mktemp "$HOME/.codeleveler-pty-test.XXXXXX")" && printf 'pty-ok\\n' > "$test_file" && cat "$test_file" && rm "$test_file" && printf 'HOME_WRITE_OK_%s\\n' ${tag}`);
  await waitRows(`HOME_WRITE_OK_${tag}`).catch(async error => { throw new Error(`HOME write failed: ${error.message}\n${await rows()}`); });
  await line(`autoload -Uz compinit; compinit -d "$HOME/.zcompdump-codeleveler-${tag}"; printf 'COMPINIT_EXIT_%s=%s\\n' ${tag} $?; rm -f "$HOME/.zcompdump-codeleveler-${tag}"`);
  await waitRows(`COMPINIT_EXIT_${tag}=0`);
  await line(`printf 'SHELL=%s\\n' "$SHELL"; printf 'TERM=%s\\n' "$TERM"; printf 'UID=%s\\n' "$(id -u)"; pwd`);
  await page.waitForFunction(uid => document.querySelector('.terminal-view .xterm-rows')?.textContent.includes(`UID=${uid}`), process.getuid());
  const text = await rows();
  assert.doesNotMatch(text, /Operation not permitted|locking failed|permission denied/i, `restricted shell initialization:\n${text}`);
  assert.match(text, /TERM=xterm-256color/);
  return text;
}
function electronProcess() {
  const listing = execFileSync('/bin/ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
  const match = listing.split('\n').find(entry => entry.includes(profile) && entry.includes('Electron.app/Contents/MacOS/Electron'));
  return match ? Number(match.trim().split(/\s+/)[0]) : null;
}
try {
  execFileSync('/usr/bin/open', ['-n', '-a', electronApp, '--stdout', stdout, '--stderr', stderr, '--args', root, `--user-data-dir=${profile}`, `--remote-debugging-port=${port}`]);
  let connected;
  for (let attempt = 0; attempt < 100 && !connected; attempt += 1) {
    await new Promise(resolve => setTimeout(resolve, 200));
    connected = await chromium.connectOverCDP(`http://127.0.0.1:${port}`).catch(() => null);
  }
  assert.ok(connected, `Electron did not expose CDP at ${port}: ${await readFileSafe(stderr)}`);
  browser = connected;
  for (let attempt = 0; attempt < 100 && !page; attempt += 1) {
    page = browser.contexts().flatMap(context => context.pages()).find(candidate => candidate.url().startsWith('leveler-desktop://desktop/'));
    if (!page) await new Promise(resolve => setTimeout(resolve, 200));
  }
  assert.ok(page, 'the desktop renderer did not appear');
  page.setDefaultTimeout(20000);
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(() => document.querySelectorAll('.session-row').length > 0, null, { timeout: 30000 });
  const index = await page.evaluate(() => window.desktop.listTasks());
  const candidates = (index.tasks ?? []).filter(task => task.primary_workspace && !task.archived_at && task.workspace_available !== false);
  let selected = null;
  for (const task of candidates.slice(0, 8)) {
    const row = page.locator(`.session-row[data-session-id="${task.id}"]`);
    if (!(await row.count())) continue;
    await row.click();
    try {
      await page.waitForFunction(id => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId === id && document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…', task.id, { timeout: 10000 });
      const snapshot = await page.evaluate(sessionId => window.desktop.snapshot(sessionId), task.id);
      if (!snapshot.repository) continue;
      selected = { id: task.id, repository: snapshot.repository, mode: snapshot.mode };
      break;
    } catch { /* a session that cannot open is not the acceptance target */ }
  }
  assert.ok(selected, 'no existing session with a workspace could be opened');
  evidence.session = selected;
  await page.locator('#header-right-toggle:visible, #floating-right-toggle:visible').click();
  await page.getByRole('button', { name: '新建终端' }).click();
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, { timeout: 20000 });
  const text = await probe('finder');
  const terminalId = await page.locator('.terminal-view').getAttribute('data-terminal-id');
  const pid = await page.evaluate(async params => (await window.desktop.observeTerminal({ ...params, cursor: 0, max_bytes: 1024 })).pid, { session_id: selected.id, terminal_id: terminalId });
  const chain = [];
  for (let current = pid, depth = 0; current && depth < 5; depth += 1) {
    const entry = execFileSync('/bin/ps', ['-o', 'pid=,ppid=,uid=,lstart=,command=', '-p', String(current)], { encoding: 'utf8' }).trim();
    if (!entry) break;
    chain.push(entry);
    current = Number(/^\s*(\d+)\s+(\d+)/.exec(entry)?.[2]);
  }
  evidence.terminal = { id: terminalId, pid, uid: process.getuid(), process_chain: chain, sandbox_wrapped: chain.some(entry => /sandbox-exec/.test(entry)), rows: text };
  assert.equal(evidence.terminal.sandbox_wrapped, false, 'the user terminal must not run under the Agent seatbelt');
  assert.match(chain[0] ?? '', /\/bin\/zsh -l/);
  assert.ok(chain.some(entry => /execution-host/.test(entry)));
  assert.equal(electronProcess() !== null, true, 'the Electron app was launched by LaunchServices');
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.locator('.terminal-view').waitFor({ state: 'hidden' });
  Object.assign(evidence, { passed: true });
} catch (error) {
  evidence.error = error.message;
  evidence.last_rows = page ? await rows().catch(() => null) : null;
  throw error;
} finally {
  await writeFile(path.join(output, 'terminal-permissions-finder-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify({ passed: evidence.passed, session: evidence.session, terminal: evidence.terminal && { pid: evidence.terminal.pid, sandbox_wrapped: evidence.terminal.sandbox_wrapped, chain: evidence.terminal.process_chain }, error: evidence.error }, null, 2));
  electronPid = electronProcess();
  if (electronPid) {
    try { process.kill(electronPid, 'SIGTERM'); } catch { /* already gone */ }
  }
  if (browser) await browser.close().catch(() => {});
}
async function readFileSafe(file) {
  try { return await readFile(file, 'utf8'); } catch { return ''; }
}
