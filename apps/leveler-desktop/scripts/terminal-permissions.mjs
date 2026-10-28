// Real Electron + Runtime PTY acceptance for the USER terminal contract.
// The user terminal is the person's own interactive shell: it must run with the
// system user's file permissions regardless of the Agent Permission Mode. The
// Agent's own commands keep their confinement; that half is covered by the
// execution-crate seatbelt tests and by the tool-path acceptance.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { homedir, userInfo } from 'node:os';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-terminal-permissions-');
const workspace = path.join(temporary, 'workspace');
const home = path.join(temporary, 'home');
const output = path.join(root, '../../test-output/desktop');
await mkdir(workspace); await mkdir(home); await mkdir(output, { recursive: true });
await writeFile(path.join(home, 'config.toml'), 'default_model="fixture/m"\n[providers.fixture]\nprotocol="openai_chat"\nbase_url="http://127.0.0.1:9"\napi_key="test-only"\n[models.m]\nprovider="fixture"\nmodel_id="fixture"\ncontext_window=131072\n');
const evidence = {
  scope: 'isolated Runtime state, real user HOME and real zsh; no provider call',
  user: userInfo().username, expected_uid: process.getuid(), home: homedir(),
  modes: {}, passed: false,
};
let app, page;
const modes = ['assisted', 'request_approval', 'full_access'];
const labels = { request_approval: '需批准', assisted: '工作区内修改', full_access: '完全权限' };
const terminalRows = () => page.locator('.terminal-view .xterm-rows').innerText();
const helper = () => page.locator('.terminal-view .xterm-helper-textarea');
async function line(text) {
  await helper().focus();
  await page.keyboard.type(text);
  await page.keyboard.press('Enter');
}
async function waitRows(marker, timeout = 20000) {
  await page.waitForFunction(m => document.querySelector('.terminal-view .xterm-rows')?.textContent.includes(m), marker, { timeout });
}
async function probe(tag) {
  // Markers are expanded by the shell, so the echoed command line cannot
  // satisfy a wait before the command actually ran.
  await line(`printf 'PROBE_%s_HOME=%s\\n' ${tag} "$HOME"`);
  await waitRows(`PROBE_${tag}_HOME=${homedir()}`);
  await line(`test_file="$(mktemp "$HOME/.codeleveler-pty-test.XXXXXX")" && printf 'pty-ok\\n' > "$test_file" && cat "$test_file" && rm "$test_file" && printf 'HOME_WRITE_OK_%s\\n' ${tag}`);
  await waitRows(`HOME_WRITE_OK_${tag}`).catch(async error => {
    const rows = await terminalRows();
    throw new Error(`HOME write failed in ${tag}: ${error.message}\n${rows}`);
  });
  await line(`: >> "$HOME/.zsh_history_probe_${tag}" && printf 'HISTORY_FILE_OK_%s\\n' ${tag} && rm -f "$HOME/.zsh_history_probe_${tag}"`);
  await waitRows(`HISTORY_FILE_OK_${tag}`);
  await line(`autoload -Uz compinit; compinit -d "$HOME/.zcompdump-codeleveler-${tag}"; printf 'COMPINIT_EXIT_%s=%s\\n' ${tag} $?; rm -f "$HOME/.zcompdump-codeleveler-${tag}"`);
  await waitRows(`COMPINIT_EXIT_${tag}=0`).catch(async error => {
    throw new Error(`compinit failed in ${tag}: ${error.message}\n${await terminalRows()}`);
  });
  await line(`printf 'SHELL=%s\\n' "$SHELL"; printf 'ZDOTDIR=%s\\n' "\${ZDOTDIR-<unset>}"; printf 'HISTFILE=%s\\n' "\${HISTFILE-<unset>}"; printf 'XDG_STATE_HOME=%s\\n' "\${XDG_STATE_HOME-<unset>}"; printf 'TERM=%s\\n' "$TERM"; printf 'PATH_SET=%s\\n' "$([ -n "$PATH" ] && printf yes || printf no)"; printf 'UID=%s\\n' "$(id -u)"; pwd`);
  await page.waitForFunction(uid => document.querySelector('.terminal-view .xterm-rows')?.textContent.includes(`UID=${uid}`), process.getuid());
  const rows = await terminalRows();
  assert.doesNotMatch(rows, /Operation not permitted|locking failed|permission denied/i, `shell initialization was restricted in ${tag}:\n${rows}`);
  assert.match(rows, new RegExp(`UID=${process.getuid()}`));
  assert.match(rows, /TERM=xterm-256color/);
  assert.match(rows, /PATH_SET=yes/);
  assert.match(rows, new RegExp(`SHELL=${process.env.SHELL ?? '/bin/zsh'}`));
  return rows;
}
async function choosePermission(mode) {
  await page.locator('#permission-picker').click();
  await page.waitForFunction(() => !!document.querySelector('.menu .menu-item'));
  for (let i = 0; i < 4; i += 1) {
    const text = await page.evaluate(() => document.activeElement.innerText);
    if (text.includes(labels[mode])) break;
    await page.keyboard.press('ArrowDown');
  }
  await page.keyboard.press('Enter');
}
async function waitMode(mode) {
  for (let i = 0; i < 100; i += 1) {
    const id = await page.evaluate(() => document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId);
    const snapshot = await page.evaluate(session => window.desktop.snapshot(session), id);
    if (snapshot.mode === mode) return;
    await page.waitForTimeout(50);
  }
  throw new Error(`permission ${mode} was not confirmed`);
}
async function newTerminal() {
  await page.getByRole('button', { name: '新建终端' }).click();
  await page.waitForFunction(() => document.querySelector('.terminal-view')?.dataset.status === 'running', null, { timeout: 15000 });
  return page.locator('.terminal-view').getAttribute('data-terminal-id');
}
try {
  app = await electron.launch({ args: [root, `--user-data-dir=${temporary}/electron`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler') } });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => console.error('PAGE ERROR', error));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.includes('v'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const id = await page.evaluate(async () => { const folder = await window.desktop.chooseFolder(); return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id; });
  await page.reload();
  await page.locator(`.session-row[data-session-id="${id}"]`).click();
  await page.locator('#header-right-toggle:visible, #floating-right-toggle:visible').click();
  // Default session is Assisted ("工作区内修改"), the mode that used to confine
  // the user terminal to the workspace.
  let terminalId = await newTerminal();
  const firstRows = await probe('assisted');
  const observedPid = await page.evaluate(async params => (await window.desktop.observeTerminal({ ...params, cursor: 0, max_bytes: 1024 })).pid, { session_id: id, terminal_id: terminalId });
  const chain = [];
  for (let pid = observedPid, depth = 0; pid && depth < 5; depth += 1) {
    try {
      const line = execFileSync('/bin/ps', ['-o', 'pid=,ppid=,uid=,lstart=,command=', '-p', String(pid)], { encoding: 'utf8' }).trim();
      if (!line) break;
      chain.push(line);
      pid = Number(/^\s*(\d+)\s+(\d+)/.exec(line)?.[2]);
    } catch { break; }
  }
  // The shell itself is the authority for its environment; macOS `ps` does not
  // expose another process's environment here.
  const shellFacts = Object.fromEntries(['SHELL', 'ZDOTDIR', 'HISTFILE', 'XDG_STATE_HOME', 'TERM', 'PATH_SET', 'UID'].map(name => [name, new RegExp(`(?:^|\\n)${name}=([^\\n]*)`).exec(firstRows)?.[1]?.trim() ?? null]));
  evidence.modes.assisted = {
    first_rows: firstRows,
    pid: observedPid,
    uid: process.getuid(),
    process_chain: chain,
    shell_facts: shellFacts,
    home_from_probe: /PROBE_assisted_HOME=(.*)/.exec(firstRows)?.[1] ?? null,
    sandbox_wrapped: chain.some(entry => /sandbox-exec/.test(entry)),
  };
  assert.equal(evidence.modes.assisted.sandbox_wrapped, false, 'the user terminal must not run under the Agent seatbelt');
  assert.equal(evidence.modes.assisted.home_from_probe, homedir());
  assert.match(evidence.modes.assisted.process_chain[0] ?? '', /\/bin\/zsh -l/);
  assert.ok(evidence.modes.assisted.process_chain.some(entry => /execution-host/.test(entry)), 'the PTY must be owned by the Execution Host');
  // A mode switch must neither restart the process nor change its permissions.
  const before = await page.evaluate(params => window.desktop.observeTerminal({ ...params, cursor: 0, max_bytes: 1024 }), { session_id: id, terminal_id: terminalId });
  await choosePermission('request_approval'); await waitMode('request_approval');
  const same = await page.locator('.terminal-view').getAttribute('data-terminal-id');
  assert.equal(same, terminalId, 'switching mode must not replace the terminal tab');
  const after = await page.evaluate(params => window.desktop.observeTerminal({ ...params, cursor: 0, max_bytes: 1024 }), { session_id: id, terminal_id: terminalId });
  assert.equal(after.pid, before.pid, 'switching mode must not restart the terminal process');
  assert.equal(after.status, 'running');
  await probe('after_mode_switch');
  evidence.modes.request_approval = { same_pid: after.pid, after_switch_rows: await terminalRows() };
  // A terminal created under each profile gets the same system-user rights.
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.locator('.terminal-view').waitFor({ state: 'hidden' });
  terminalId = await newTerminal();
  evidence.modes.request_approval.created_rows = await probe('restricted');
  await page.locator('.pane-tab[aria-selected="true"] .tab-close').click();
  await page.locator('.terminal-view').waitFor({ state: 'hidden' });
  await choosePermission('full_access'); await waitMode('full_access');
  terminalId = await newTerminal();
  evidence.modes.full_access = { created_rows: await probe('full') };
  Object.assign(evidence, { passed: true });
} catch (error) {
  evidence.error = error.message;
  if (page && !page.isClosed()) {
    evidence.last_rows = await terminalRows().catch(() => null);
    await app.evaluate(async ({ BrowserWindow }) => (await BrowserWindow.getAllWindows()[0].capturePage()).toPNG().toString('base64')).then(bytes => writeFile(path.join(output, 'terminal-permissions-failure.png'), Buffer.from(bytes, 'base64'))).catch(() => {});
  }
  throw error;
} finally {
  await writeFile(path.join(output, 'terminal-permissions-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
  if (app) await app.close();
}
