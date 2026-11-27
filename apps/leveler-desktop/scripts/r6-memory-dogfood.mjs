// R6 real local dogfood: the four memory commands on the real HOME, real
// config/provider, real Runtime and Desktop. The test workspace, Electron
// profile and any memory scope it creates are isolated and removed after the
// run; only a uniquely tagged `/remember` note touches the global scope, and it
// is forgotten through the page before cleanup so the user's other notes stay
// byte-identical.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, existsSync, readdirSync, writeFileSync } from 'node:fs';
import { mkdir, writeFile, readdir, rm } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import os from 'node:os';
import path from 'node:path';
import { _electron as electron } from '../node_modules/playwright/index.mjs';

const ROOT = '/Users/mengmian/Develop/app/CodeLeveler';
const OUT = path.join(ROOT, 'test-output/desktop');
const BINARY = path.join(ROOT, 'target/debug/leveler');
const HOME = os.homedir();
const REAL_HOME = path.join(HOME, '.leveler');
const CONFIG = path.join(REAL_HOME, 'config.toml');
const GLOBAL_MEMORY = path.join(REAL_HOME, 'memory/global');
const sha = buffer => createHash('sha256').update(buffer).digest('hex');
const stamp = Date.now();
const TEST_ROOT = path.join(HOME, `.leveler-r6-dogfood-${stamp}`);
const WORKSPACE = path.join(TEST_ROOT, 'workspace');
const PROFILE = path.join(TEST_ROOT, 'profile');
const TOKEN = `R6_DOGFOOD_${stamp}`;
const NOTE = `R6 真机记忆验收 ${TOKEN}\n\n临时验收笔记：本轮结束后会通过记忆页遗忘。`;
const configBefore = existsSync(CONFIG) ? readFileSync(CONFIG) : null;
const configHashBefore = configBefore ? sha(configBefore) : null;

await mkdir(WORKSPACE, { recursive: true });
await mkdir(PROFILE, { recursive: true });
await mkdir(OUT, { recursive: true });
writeFileSync(path.join(WORKSPACE, 'README.md'), '# R6 memory dogfood\n\nDisposable verification project.\n');

/* ------------------------------------------------------------ file snapshots */

async function walkFiles(directory) {
  const found = [];
  const visit = async current => {
    let entries;
    try { entries = await readdir(current, { withFileTypes: true }); } catch { return; }
    for (const entry of entries) {
      const full = path.join(current, entry.name);
      if (entry.isDirectory()) await visit(full);
      else if (entry.isFile()) found.push(full);
    }
  };
  await visit(directory);
  return found.sort();
}
async function contentSnapshot(directory) {
  const snapshot = {};
  for (const file of await walkFiles(directory)) {
    const name = path.relative(directory, file).split(path.sep).join('/');
    // Generated manifests and the store's databases are runtime state, not
    // user topics; the comparison below is about the user's content files.
    if (name === 'MEMORY.md' || name === '.MEMORY.md.lock' || name.endsWith('.sqlite') || name.includes('.sqlite-')) continue;
    try { snapshot[name] = sha(readFileSync(file)); } catch { /* a racing writer owns it */ }
  }
  return snapshot;
}
const preGlobal = await contentSnapshot(GLOBAL_MEMORY);
const preWorkspaceScopes = new Set(existsSync(path.join(REAL_HOME, 'memory/workspaces'))
  ? readdirSync(path.join(REAL_HOME, 'memory/workspaces'))
  : []);
const preStateProjects = new Set(existsSync(path.join(REAL_HOME, 'state/projects'))
  ? readdirSync(path.join(REAL_HOME, 'state/projects'))
  : []);

const evidence = {
  scope: 'real HOME/config/provider/runtime/desktop; isolated test workspace, Electron profile and memory scope',
  binary: {
    path: BINARY,
    sha256: sha(readFileSync(BINARY)),
    version: execFileSync(BINARY, ['--version'], { encoding: 'utf8' }).trim(),
  },
  head: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim(),
  worktree_dirty: execFileSync('git', ['status', '--porcelain'], { cwd: ROOT, encoding: 'utf8' }).trim().split('\n').length,
  home: HOME,
  workspace: WORKSPACE,
  token: TOKEN,
  config_before: configHashBefore,
  checks: [],
};
const errors = [];
let app;
let page;
const check = (name, ok, detail = '') => {
  evidence.checks.push({ name, ok: !!ok, detail });
  assert.ok(ok, `${name}${detail ? ` — ${detail}` : ''}`);
};
const shot = async name => page.screenshot({ path: path.join(OUT, `r6-dogfood-${name}.png`) });

async function launch() {
  app = await electron.launch({
    args: [path.join(ROOT, 'apps/leveler-desktop'), `--user-data-dir=${PROFILE}`],
    env: (() => { const env = { ...process.env, LEVELER_BINARY: BINARY, LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '1800' }; delete env.ELECTRON_RUN_AS_NODE; delete env.LEVELER_HOME; delete env.LEVELER_CONFIG_DIR; return env; })(),
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#brand-version')?.textContent.startsWith('v'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
  if (!evidence.desktop_version) evidence.desktop_version = await page.locator('#brand-version').innerText().catch(() => null);
  await page.evaluate(() => {
    const stored = JSON.parse(localStorage.getItem('codeleveler.settings.v1') || '{}');
    stored.defaultPermission = 'full_access';
    localStorage.setItem('codeleveler.settings.v1', JSON.stringify(stored));
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'), null, { timeout: 60000 });
  await page.waitForFunction(async () => { try { return (await window.desktop.connectionStatus()).state === 'connected'; } catch { return false; } }, null, { timeout: 60000 });
}
async function closeApp() {
  if (!app) return;
  try { await Promise.race([app.evaluate(({ app: electronApp }) => electronApp.exit(0)), new Promise(resolve => setTimeout(resolve, 5000))]); } catch { /* gone */ }
  app = null;
}
const submit = async text => { await page.locator('#message').fill(text); await page.locator('#send').click(); };
const settle = () => page.waitForFunction(async () => {
  const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
  if (!id) return false;
  try { const value = await window.desktop.snapshot(id); return !['running', 'waiting_user'].includes(value.task_status); } catch { return false; }
}, null, { timeout: 300000 });
const toast = async (text, timeout = 120000) => {
  await page.waitForFunction(value => document.querySelector('.toast-text')?.textContent.includes(value), text, { timeout });
  return page.locator('.toast-text').innerText();
};
const memoryOutcome = async (label, timeout = 420000) => {
  await page.waitForFunction(value => {
    const text = document.querySelector('.toast-text')?.textContent ?? '';
    return text.includes(`${value}完成 ·`) || text.includes(`${value}失败 ·`);
  }, label, { timeout });
  return page.locator('.toast-text').innerText();
};
async function openMemory() {
  await submit('/memory');
  await page.waitForSelector('dialog.memory-dialog[open]', { timeout: 30000 });
  await page.waitForFunction(() => !!document.querySelector('.memory-dialog .memory-rollout'), null, { timeout: 30000 });
  return page.locator('.memory-summary').innerText();
}
async function selectDogfoodNote() {
  const rows = page.locator('.memory-dialog .memory-file');
  const count = await rows.count();
  for (let index = 0; index < count; index += 1) {
    const row = rows.nth(index);
    const listed = await row.locator('.memory-file-path').innerText();
    if (!listed.startsWith('observations/_inbox/')) continue;
    if ((await row.getAttribute('aria-selected')) !== 'true') await row.click();
    await page.waitForFunction(name => (document.querySelector('.memory-preview-head')?.textContent ?? '').includes(name)
      && !!document.querySelector('.memory-dialog .memory-content'), listed, { timeout: 15000 });
    if ((await page.locator('.memory-dialog .memory-content').innerText()).includes(TOKEN)) return listed;
  }
  assert.fail('the dogfood note is listed and previews its token');
}
async function waitMemoryIdle(rounds = 15) {
  // The open page re-reads the runtime every 4 seconds; two quiet samples
  // after a full poll interval mean capture and Dream settled.
  let idle = 0;
  for (let index = 0; index < rounds; index += 1) {
    const active = await page.locator('.memory-activity').count();
    idle = active ? 0 : idle + 1;
    if (idle >= 2) return true;
    await new Promise(resolve => setTimeout(resolve, 3000));
  }
  return false;
}

let sessionId = null;
let notePath = null;
try {
  await launch();
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, WORKSPACE);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(target => document.querySelector('#workspace-picker')?.dataset.tooltip === target, WORKSPACE, { timeout: 30000 });
  await submit('请只回复一行：R6_MEMORY_READY。不要调用任何工具，不要向我提问。');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'), null, { timeout: 60000 });
  sessionId = await page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
  await settle();
  const snapshot = await page.evaluate(id => window.desktop.snapshot(id), sessionId);
  evidence.session = { id: sessionId, model: snapshot.model ?? null, status: snapshot.task_status ?? snapshot.status };
  check('the real session carries the configured model', !!snapshot.model?.model, JSON.stringify(snapshot.model ?? null));

  // Discovery on the real runtime.
  await page.locator('#attachment-add').click();
  await page.waitForSelector('#command-popover');
  const catalog = await page.evaluate(() => [...document.querySelectorAll('#command-popover .menu-item')].map(item => ({
    id: item.dataset.commandId,
    disabled: item.disabled,
    badge: item.querySelector('.badge-unconnected')?.textContent ?? null,
  })));
  await page.keyboard.press('Escape');
  const byId = Object.fromEntries(catalog.map(item => [item.id, item]));
  for (const id of ['memory', 'remember', 'dream', 'flush']) {
    check(`catalog lists ${id}`, byId[id] && byId[id].disabled === false && byId[id].badge === null, JSON.stringify(byId[id]));
  }

  // `/remember` writes one uniquely tagged global note.
  const beforeInbox = (await walkFiles(path.join(GLOBAL_MEMORY, 'observations/_inbox'))).map(file => path.basename(file));
  await submit(`/remember ${NOTE}`);
  const rememberToast = await toast('记忆写入完成', 120000);
  const afterInbox = (await walkFiles(path.join(GLOBAL_MEMORY, 'observations/_inbox'))).map(file => path.basename(file));
  const newFiles = afterInbox.filter(name => !beforeInbox.includes(name));
  check('remember created exactly one global observation', newFiles.length === 1, newFiles.join(','));
  notePath = path.join(GLOBAL_MEMORY, 'observations/_inbox', newFiles[0]);
  const noteBytes = readFileSync(notePath, 'utf8');
  check('the persisted note is the typed text', noteBytes.includes(TOKEN) && noteBytes.includes('临时验收笔记'), noteBytes.slice(0, 120));
  evidence.remember = { toast: rememberToast, file: path.basename(notePath) };

  // The CLI reads the same store while the Desktop page is open: one client's
  // write is another client's listed fact, with the same preview bytes.
  const cliStatusAfterRemember = JSON.parse(execFileSync(BINARY, ['memory', 'status', '--json'], { cwd: WORKSPACE, encoding: 'utf8' }));
  check('the CLI lists the Desktop-written note', (cliStatusAfterRemember.global?.files ?? []).some(file => file.path === `observations/_inbox/${path.basename(notePath)}`), path.basename(notePath));
  const cliShow = execFileSync(BINARY, ['memory', 'show', 'global', `observations/_inbox/${path.basename(notePath)}`], { encoding: 'utf8' });
  check('the CLI preview reads the same note bytes', cliShow.includes(TOKEN));

  // `/memory` lists and previews the runtime's own bytes.
  const summary = await openMemory();
  evidence.rollout = await page.locator('.memory-rollout').innerText();
  const listed = await selectDogfoodNote();
  check('the page previews the remembered note', (await page.locator('.memory-dialog .memory-content').innerText()).includes(TOKEN), listed);
  await shot('memory');
  evidence.memory_page = { summary, listed };

  // Wait for any automatic capture/Dream activity the real turn triggered.
  check('the memory pipeline is idle before the manual actions', await waitMemoryIdle());

  // `/dream` and `/flush` run the real store; every message is the runtime's.
  await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('dialog.memory-dialog[open]'), null, { timeout: 15000 });
  await submit('/dream');
  evidence.dream = await memoryOutcome('记忆整理');
  await submit('/flush');
  try {
    evidence.flush = await memoryOutcome('记忆捕获刷新');
  } catch {
    evidence.flush = null;
  }

  // Restart: the note is still listed and previewed from durable state.
  await closeApp();
  await launch();
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).waitFor({ state: 'visible', timeout: 60000 });
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'), null, { timeout: 30000 });
  await openMemory();
  await selectDogfoodNote();
  check('the note survives a Desktop restart', (await page.locator('.memory-dialog .memory-content').innerText()).includes(TOKEN));
  await shot('restart');

  // Forget through the page: a real confirmation, then the file is gone.
  await page.locator('.memory-dialog').getByRole('button', { name: '遗忘此文件' }).click();
  await page.waitForSelector('dialog.confirm-modal[open]', { timeout: 30000 });
  await shot('forget-confirm');
  await page.locator('dialog.confirm-modal .modal-actions button', { hasText: '遗忘' }).click();
  const forgetToast = await toast('遗忘完成', 120000);
  check('forget removed the test note', !existsSync(notePath), forgetToast);
  evidence.forget = forgetToast;
  await page.waitForFunction(name => ![...document.querySelectorAll('.memory-dialog .memory-file-path')].some(node => node.textContent.endsWith(name)), path.basename(notePath), { timeout: 30000 });
  check('the page no longer lists the forgotten note', true);
  const cliStatusAfterForget = JSON.parse(execFileSync(BINARY, ['memory', 'status', '--json'], { cwd: WORKSPACE, encoding: 'utf8' }));
  check('the CLI no longer lists the forgotten note', !(cliStatusAfterForget.global?.files ?? []).some(file => file.path === `observations/_inbox/${path.basename(notePath)}`));

  // The user's other global notes are untouched (content files, not generated
  // manifests or the store's databases).
  const postGlobal = await contentSnapshot(GLOBAL_MEMORY);
  const postAdjusted = { ...postGlobal };
  delete postAdjusted[path.relative(GLOBAL_MEMORY, notePath).split(path.sep).join('/')];
  check('no other global memory file changed', JSON.stringify(Object.keys(postAdjusted).sort()) === JSON.stringify(Object.keys(preGlobal).sort())
    && Object.keys(preGlobal).every(name => postAdjusted[name] === preGlobal[name]), JSON.stringify({ before: Object.keys(preGlobal).length, after: Object.keys(postAdjusted).length }));

  assert.deepEqual(errors, []);
  evidence.passed = true;
} catch (error) {
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(OUT, 'r6-dogfood-failure.png') }).catch(() => {});
  evidence.passed = false;
  evidence.error = error.message;
  evidence.page_errors = errors;
  const visible = await page?.evaluate(() => ({
    toast: document.querySelector('.toast-text')?.textContent ?? null,
    composerError: !document.querySelector('#composer-error')?.hidden ? document.querySelector('#composer-error')?.textContent : null,
    memoryDialogOpen: !!document.querySelector('dialog.memory-dialog[open]'),
  })).catch(() => null);
  evidence.visible = visible;
} finally {
  await closeApp();
  evidence.cleanup = { sessions: [], workspace_scope: null, state_project: null };
  // Identify this run's state database before deleting the session.
  let stateProject = null;
  try {
    for (const project of readdirSync(path.join(REAL_HOME, 'state/projects'))) {
      const db = path.join(REAL_HOME, 'state/projects', project, 'sessions.db');
      if (!existsSync(db)) continue;
      try {
        const count = execFileSync('sqlite3', [db, `select count(*) from sessions where id='${sessionId}'`], { encoding: 'utf8' }).trim();
        if (count === '1') { stateProject = project; break; }
      } catch { /* not this project */ }
    }
  } catch { /* no state tree */ }
  // The memory scope this workspace used, read by the same CLI the product
  // documents. It is removed only when the run created it.
  let workspaceScopeRoot = null;
  try {
    const status = JSON.parse(execFileSync(BINARY, ['memory', 'status', '--json'], { cwd: WORKSPACE, encoding: 'utf8', timeout: 30000 }));
    workspaceScopeRoot = status.workspace?.root ?? null;
  } catch { /* no scope was created */ }
  try { execFileSync(BINARY, ['runtime', '--repo', WORKSPACE, 'stop'], { stdio: 'ignore', timeout: 20000 }); } catch { /* no runtime */ }
  if (sessionId) {
    try {
      execFileSync(BINARY, ['sessions', 'delete', sessionId, '--repo', WORKSPACE], { stdio: 'ignore', timeout: 20000 });
      evidence.cleanup.sessions.push(sessionId);
    } catch (error) { evidence.cleanup.session_error = String(error.message || error); }
  }
  // Kill only this run's daemons (unique test root), then remove its files.
  const { spawnSync } = await import('node:child_process');
  spawnSync('pkill', ['-f', TEST_ROOT]);
  await new Promise(resolve => setTimeout(resolve, 800));
  spawnSync('pkill', ['-f', TEST_ROOT]);
  await new Promise(resolve => setTimeout(resolve, 400));
  if (workspaceScopeRoot && existsSync(workspaceScopeRoot)) {
    const name = path.basename(workspaceScopeRoot);
    const parent = path.dirname(workspaceScopeRoot);
    if (parent === path.join(REAL_HOME, 'memory/workspaces') && !preWorkspaceScopes.has(name)) {
      const notes = await contentSnapshot(workspaceScopeRoot);
      await rm(workspaceScopeRoot, { recursive: true, force: true }).catch(() => {});
      evidence.cleanup.workspace_scope = { path: workspaceScopeRoot, removed: !existsSync(workspaceScopeRoot), content_files: Object.keys(notes).length };
    } else {
      evidence.cleanup.workspace_scope = { path: workspaceScopeRoot, removed: false, reason: 'pre-existing scope' };
    }
  }
  if (stateProject && !preStateProjects.has(stateProject)) {
    const dir = path.join(REAL_HOME, 'state/projects', stateProject);
    await rm(dir, { recursive: true, force: true }).catch(() => {});
    evidence.cleanup.state_project = { name: stateProject, removed: !existsSync(dir) };
  }
  await rm(TEST_ROOT, { recursive: true, force: true }).catch(() => {});
  // If the UI flow failed before forgetting, remove the test note through the
  // product's own CLI forget (hash-bound) instead of leaving test data behind.
  if (notePath && existsSync(notePath)) {
    try {
      const relative = path.relative(GLOBAL_MEMORY, notePath).split(path.sep).join('/');
      const shown = spawnSync(BINARY, ['memory', 'show', 'global', relative], { encoding: 'utf8', timeout: 30000 });
      const digest = /\b[0-9a-f]{64}\b/.exec(String(shown.stderr ?? ''))?.[0];
      if (digest) {
        execFileSync(BINARY, ['memory', 'forget', 'global', relative, '--hash', digest], { stdio: 'ignore', timeout: 30000 });
        evidence.cleanup.global_note_forgotten = !existsSync(notePath);
      }
    } catch (error) { evidence.cleanup.global_note_error = String(error.message || error); }
  }
  evidence.config_after = existsSync(CONFIG) ? sha(readFileSync(CONFIG)) : null;
  evidence.config_unchanged = evidence.config_after === configHashBefore;
  writeFileSync(path.join(OUT, 'r6-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
  if (!evidence.passed) process.exitCode = 1;
}
