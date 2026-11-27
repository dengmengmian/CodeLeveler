// R3b real local dogfood: the actual macOS HOME/profile, the configured
// provider and model, the real Desktop, Runtime, trust store and filesystem.
// Only NEW sessions in a temporary workspace are created; the workspace, its
// sessions and its trust records are cleaned up by their exact identities.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { execFile as execFileCb } from 'node:child_process';
import { promisify } from 'node:util';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { mkdtemp, mkdir, readdir, readFile, rm, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const execFile = promisify(execFileCb);
const root = fileURLToPath(new URL('..', import.meta.url));
const output = process.env.LEVELER_ACCEPTANCE_OUTPUT || path.join(root, '../../test-output/desktop');
await mkdir(output, { recursive: true });
if (!process.env.LEVELER_BINARY) throw new Error("Set LEVELER_BINARY to this worktree's leveler build");
const leveler = process.env.LEVELER_BINARY;
const binarySha = createHash('sha256').update(readFileSync(leveler)).digest('hex');
const workspace = await mkdtemp('/tmp/leveler-r3b-dogfood-');
const suffix = createHash('sha256').update(workspace).digest('hex').slice(0, 10);
const names = {
  allow: `r3b-dogfood-${suffix}`,
  deny: `r3b-dogfood-deny-${suffix}`,
  change: `r3b-dogfood-change-${suffix}`,
  persist: `r3b-dogfood-persist-${suffix}`,
  full: `r3b-dogfood-full-${suffix}`,
};
const proof = {
  allow: `R3B_DOGFOOD_ALLOW_${suffix}`,
  deny: `R3B_DOGFOOD_DENY_${suffix}`,
  changeV1: `R3B_DOGFOOD_CHANGE_V1_${suffix}`,
  changeV2: `R3B_DOGFOOD_CHANGE_V2_${suffix}`,
  persist: `R3B_DOGFOOD_PERSIST_${suffix}`,
  full: `R3B_DOGFOOD_FULL_${suffix}`,
};
const skillBody = (marker, file) => `# Procedure\n\nWhen this skill is invoked, you MUST call the write_file tool with path \`${file}\` and content \`${marker}\`. Then reply exactly ${marker}_OK.\n\nDo not skip the file write.\n`;
const writeSkill = async (name, marker, file, note) => {
  const dir = path.join(workspace, '.leveler', 'skills', name);
  await mkdir(dir, { recursive: true });
  await writeFile(
    path.join(dir, 'SKILL.md'),
    `---\nname: ${name}\ndescription: R3b dogfood ${note}\n---\n\n${skillBody(marker, file)}\n`,
  );
};
const allowFile = `r3b-proof-${suffix}.txt`;
const denyFile = `r3b-deny-${suffix}.txt`;
const changeFile = `r3b-change-${suffix}.txt`;
const persistFile = `r3b-persist-${suffix}.txt`;
const fullFile = `r3b-full-${suffix}.txt`;
await writeSkill(names.allow, proof.allow, allowFile, 'allow');
await writeSkill(names.deny, proof.deny, denyFile, 'deny');
await writeSkill(names.change, proof.changeV1, changeFile, 'change');
await writeSkill(names.persist, proof.persist, persistFile, 'persist');
await writeSkill(names.full, proof.full, fullFile, 'full');
await writeFile(path.join(workspace, 'AGENTS.md'), 'R3B_DOGFOOD_AGENTS_RULE\n');

const realHome = process.env.HOME;
const configPath = path.join(realHome, '.leveler', 'config.toml');
const configHash = async () => existsSync(configPath)
  ? createHash('sha256').update(await readFile(configPath)).digest('hex')
  : null;
const configBefore = await configHash();
const evidence = {
  scope: 'real macOS HOME/profile, real configured provider/model, real Desktop/Runtime/trust store; temporary workspace',
  binary: { path: leveler, sha256: binarySha },
  workspace,
  skills: names,
  config_sha256_before: configBefore,
};
let app;
let page;
let stage = 'launch';
const createdSessions = [];
const errors = [];
const launch = async () => {
  const env = { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_BINARY: leveler };
  delete env.LEVELER_HOME;
  delete env.LEVELER_CONFIG_DIR;
  delete env.LEVELER_DAEMON_IDLE_TIMEOUT_SECS;
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session') && document.querySelector('#brand-version')?.textContent.startsWith('v'));
  await page.waitForFunction(async () => (await window.desktop.connectionStatus()).state === 'connected', null, { timeout: 30000 });
};
const sessionId = () => page.locator('.session-row[aria-selected="true"]').getAttribute('data-session-id');
const rememberSession = async () => {
  const id = await sessionId();
  if (id && !createdSessions.includes(id)) createdSessions.push(id);
  return id;
};
const waitSend = () => page.waitForFunction(() => {
  const button = document.querySelector('#send');
  return button && !button.disabled && !button.hidden;
});
const submit = async value => {
  await page.locator('#message').fill(value);
  // The composer must agree with the settled snapshot before a new turn:
  // while it still shows 运行中, Enter is a steer, and a mid-turn skill
  // command is correctly refused by the runtime.
  await page.waitForFunction(() => {
    const status = document.querySelector('#conversation-status')?.textContent ?? '';
    return status !== '连接 Runtime…' && !['运行中', '等待你处理'].includes(status);
  }, null, { timeout: 300000 });
  await waitSend();
  await page.locator('#send').click();
};
const userCount = () => page.locator('.message.user').count();
const waitUserAdded = before => page.waitForFunction(count => document.querySelectorAll('.message.user').length > count, before, { timeout: 20000 });
const waitIdle = () => page.waitForFunction(async () => {
  const id = document.querySelector('.session-row[aria-selected="true"]')?.dataset.sessionId;
  if (!id) return false;
  const snapshot = await window.desktop.snapshot(id);
  return !['running', 'waiting_user'].includes(snapshot.task_status);
}, null, { timeout: 300000 });
const waitFile = async (file, marker, timeout = 300000) => {
  const at = Date.now();
  while (Date.now() - at < timeout) {
    if (existsSync(path.join(workspace, file))) {
      const text = await readFile(path.join(workspace, file), 'utf8');
      if (text.includes(marker)) return true;
    }
    await page.waitForTimeout(500);
  }
  return false;
};
const card = () => page.locator('.definition-trust-card');
const decide = async label => {
  await card().getByRole('button', { name: label, exact: true }).click();
  await page.waitForFunction(() => !document.querySelector('.definition-trust-card'));
};
const openSettings = async () => {
  await page.keyboard.press('Meta+,');
  await page.waitForFunction(() => document.querySelector('#settings-dialog')?.open);
  await page.getByRole('button', { name: 'Agent 与技能' }).click();
  await page.waitForFunction(() => document.querySelector('#settings-content')?.textContent.includes('技能'));
};
const closeSettings = async () => {
  await page.locator('#settings-close').click();
  await page.waitForFunction(() => !document.querySelector('#settings-dialog')?.open);
};
const skillCard = name => page.locator(`.skill-card[data-skill="${name}"]`);

try {
  await launch();
  stage = 'open workspace';
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(name => document.querySelector('#workspace-picker')?.textContent.includes(name), path.basename(workspace));
  // Create the first session in the selected workspace: the global New
  // Session button deliberately creates a workspace-less chat.
  await submit('你好，简短回答即可。');
  await page.waitForFunction(() => document.querySelector('.session-row[aria-selected="true"]'));
  await rememberSession();
  await waitIdle();
  evidence.startup = { session: await sessionId(), provider_ready: true };

  stage = 'discovery';
  await openSettings();
  await page.waitForFunction(name => !!document.querySelector(`.skill-card[data-skill="${name}"]`), names.allow, { timeout: 30000 });
  for (const name of Object.values(names)) {
    assert.equal(await skillCard(name).count(), 1, `${name} must be discovered from the real workspace`);
    assert.match(await skillCard(name).innerText(), /未信任/);
    assert.match(await skillCard(name).innerText(), /CodeLeveler/);
  }
  await skillCard(names.allow).getByRole('button', { name: '查看详情' }).click();
  await page.waitForFunction(marker => document.querySelector('.skill-detail .skill-body')?.textContent.includes(marker), proof.allow);
  evidence.discovery = { rows: await page.locator('.skill-card').count(), untrusted: Object.keys(names).length };
  evidence.detail = (await page.locator('.skill-detail .skill-body').innerText()).slice(0, 160);
  await closeSettings();

  stage = 'unused untrusted';
  let before = await userCount();
  await submit('只回复 OK，不要使用任何技能。');
  await waitUserAdded(before);
  await waitIdle();
  assert.equal(await card().count(), 0, 'an unused untrusted skill must not prompt');
  evidence.unused_untrusted_silent = true;

  stage = 'allow once + real skill use';
  before = await userCount();
  await submit(`/${names.allow} 按技能步骤执行。`);
  await card().waitFor();
  assert.match(await card().innerText(), /未批准/);
  await page.screenshot({ path: path.join(output, 'r3b-skills-dogfood-trust.png'), animations: 'disabled', timeout: 60000 });
  await decide('允许本次使用');
  await waitUserAdded(before);
  const allowUsed = await waitFile(allowFile, proof.allow);
  await waitIdle();
  assert.equal(allowUsed, true, 'the real model must follow the admitted skill and write the proof file');
  await page.screenshot({ path: path.join(output, 'r3b-skills-dogfood-used.png'), animations: 'disabled', timeout: 60000 });
  evidence.real_skill_use = { file: allowFile, marker: proof.allow, used: true };

  stage = 'deny';
  before = await userCount();
  await submit(`/${names.deny} 按技能步骤执行。`);
  await card().waitFor();
  await decide('拒绝');
  await page.waitForFunction(() => document.querySelector('#composer-error')?.textContent.includes('已拒绝加载技能'));
  assert.equal(await userCount(), before);
  assert.equal(existsSync(path.join(workspace, denyFile)), false);
  evidence.deny = { not_sent: true, file_absent: true };

  stage = 'content change';
  before = await userCount();
  await submit(`/${names.change} 按技能步骤执行。`);
  await card().waitFor();
  await decide('信任当前版本');
  await waitUserAdded(before);
  assert.equal(await waitFile(changeFile, proof.changeV1), true, 'the trusted v1 skill must run');
  await writeSkill(names.change, proof.changeV2, changeFile, 'change');
  before = await userCount();
  await submit(`/${names.change} 按技能步骤执行。`);
  await card().waitFor();
  assert.match(await card().innerText(), /内容已变更/);
  await decide('拒绝');
  assert.equal(await userCount(), before);
  evidence.content_change = { v1_used: true, changed_detected: true, v2_refused: true };

  stage = 'persistent trust + restart';
  before = await userCount();
  await submit(`/${names.persist} 按技能步骤执行。`);
  await card().waitFor();
  await decide('信任当前版本');
  await waitUserAdded(before);
  assert.equal(await waitFile(persistFile, proof.persist), true);
  const trustStore = path.join(realHome, '.leveler', 'definition-trust.yaml');
  assert.equal(existsSync(trustStore), true, 'a persistent approval writes the trust store');
  assert.equal((await readFile(trustStore, 'utf8')).includes(names.persist), true, 'the store names the approved skill');
  evidence.persistent_record_written = true;
  const persistedSession = await sessionId();
  await app.evaluate(({ app }) => app.exit(0));
  app = null;
  await launch();
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), persistedSession, { timeout: 30000 });
  await page.locator(`.session-row[data-session-id="${persistedSession}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await openSettings();
  await page.waitForFunction(name => !!document.querySelector(`.skill-card[data-skill="${name}"]`), names.persist);
  assert.match(await skillCard(names.persist).innerText(), /可用/);
  await closeSettings();
  await rm(path.join(workspace, persistFile), { force: true });
  before = await userCount();
  await submit(`/${names.persist} 按技能步骤执行。`);
  assert.equal(await card().count(), 0, 'persistent trust must survive the Desktop restart');
  await waitUserAdded(before);
  assert.equal(await waitFile(persistFile, proof.persist), true);
  evidence.restart = { session: persistedSession, no_second_prompt: true };

  stage = 'full access';
  const previous = await sessionId();
  await page.locator('.workspace-group').filter({ hasText: path.basename(workspace) }).locator('.group-new').click();
  await page.waitForFunction(prev => {
    const row = document.querySelector('.session-row[aria-selected="true"]');
    return row && row.dataset.sessionId !== prev;
  }, previous);
  await rememberSession();
  await page.locator('#permission-picker').click();
  await page.locator('.menu .menu-item').filter({ hasText: '完全权限' }).click();
  await page.waitForFunction(async () => (await window.desktop.snapshot(document.querySelector('.session-row[aria-selected="true"]').dataset.sessionId)).mode === 'full_access');
  before = await userCount();
  await submit(`/${names.full} 按技能步骤执行。`);
  await waitUserAdded(before);
  assert.equal(await card().count(), 0, 'Full must not prompt for a pre-session skill');
  assert.equal(await waitFile(fullFile, proof.full), true, 'Full adopts the pre-session skill and the model uses it');
  await waitIdle();
  await page.screenshot({ path: path.join(output, 'r3b-skills-dogfood-full.png'), animations: 'disabled', timeout: 60000 });
  evidence.full = { no_prompt: true, real_use: true };

  // AGENTS.md is default project context, not a trust object: the real model
  // sees it without any definition-trust prompt. There is no untrusted state.
  assert.equal(await card().count(), 0);
  evidence.agents_md_default_context = true;

  assert.deepEqual(errors, []);
  const configAfter = await configHash();
  assert.equal(configAfter, configBefore, 'the real config.toml must be byte-identical');
  Object.assign(evidence, { config_sha256_after: configAfter, page_errors: errors, passed: true });
  await writeFile(path.join(output, 'skills-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  Object.assign(evidence, { passed: false, stage, error: error.message, page_errors: errors });
  if (page && !page.isClosed()) await page.screenshot({ path: path.join(output, 'skills-dogfood-failure.png') }).catch(() => {});
  await writeFile(path.join(output, 'skills-dogfood-evidence.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  throw error;
} finally {
  if (app) await app.evaluate(({ app }) => app.exit(0)).catch(() => {});
  // Exact cleanup: remove the test sessions, revoke this workspace's trust
  // records, then delete the temporary workspace. Nothing in the real profile
  // outside these identities is touched.
  for (const id of createdSessions) {
    await execFile(leveler, ['sessions', 'delete', id], { timeout: 30000 }).catch(() => {});
  }
  await execFile(leveler, ['trust', 'revoke'], { cwd: workspace, timeout: 30000 }).catch(() => {});
  // The daemon may still be settling its last turn when the window closes.
  // Ask it to drain repeatedly before deleting the state it owns.
  await new Promise(resolve => setTimeout(resolve, 1000));
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await execFile(leveler, ['runtime', 'stop'], { cwd: workspace, timeout: 30000 }).catch(() => {});
    await new Promise(resolve => setTimeout(resolve, 800));
  }
  // The runtime host keys its per-source state and socket by a hash of the
  // canonical workspace path. Remove only the entries whose project directory
  // carries this run's unique workspace suffix. A second pass catches a
  // directory the daemon re-created while draining.
  const projectsRoot = path.join(realHome, '.leveler', 'state', 'projects');
  const socketsRoot = path.join(realHome, '.leveler', 'run', 'sockets');
  const workspaceTag = path.basename(workspace);
  const removeRunState = async () => {
    for (const name of await readdir(projectsRoot).catch(() => [])) {
      if (!name.includes(workspaceTag)) continue;
      const hash = name.slice(name.lastIndexOf('-') + 1);
      await rm(path.join(projectsRoot, name), { recursive: true, force: true }).catch(() => {});
      for (const extension of ['sock', 'lock', 'token']) {
        await rm(path.join(socketsRoot, `${hash}.${extension}`), { force: true }).catch(() => {});
      }
    }
  };
  await removeRunState();
  await rm(workspace, { recursive: true, force: true }).catch(() => {});
  await new Promise(resolve => setTimeout(resolve, 500));
  await removeRunState();
}
