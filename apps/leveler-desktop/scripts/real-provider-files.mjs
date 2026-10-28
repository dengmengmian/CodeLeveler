// Real Provider acceptance for the file workflow. The isolated HOME links the
// user's real config (read-only use) and never prints or copies provider keys.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, symlink, rm } from 'node:fs/promises';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import os from 'node:os';

const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-real-provider-');
const home = path.join(temporary, 'home');
const workspace = path.join(temporary, 'workspace');
const output = path.join(root, '../../test-output/desktop/round5-real-provider');
await mkdir(home);
await mkdir(workspace);
await mkdir(output, { recursive: true });
await writeFile(path.join(workspace, 'example.txt'), 'ORIGINAL_CONTENT\n', { mode: 0o600 });
await writeFile(path.join(workspace, 'README.md'), '# Real provider fixture\n');
execFileSync('git', ['init', '-q'], { cwd: workspace });
execFileSync('git', ['add', '.'], { cwd: workspace });
execFileSync('git', ['-c', 'user.email=fixture@example.test', '-c', 'user.name=Fixture', 'commit', '-qm', 'fixture'], { cwd: workspace });
const config = path.join(os.homedir(), '.leveler', 'config.toml');
await symlink(config, path.join(home, 'config.toml'));

const evidence = { scope: 'real provider, isolated HOME via config symlink, temporary workspace' };
let app;
let page;
const errors = [];
try {
  app = await electron.launch({
    args: [root, `--user-data-dir=${path.join(temporary, 'electron')}`],
    env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined, LEVELER_HOME: home, LEVELER_CONFIG_DIR: undefined, LEVELER_BINARY: process.env.LEVELER_BINARY ?? path.resolve(root, '../../target/debug/leveler'), LEVELER_DAEMON_IDLE_TIMEOUT_SECS: '10' },
  });
  page = await app.firstWindow();
  page.setDefaultTimeout(20000);
  page.on('pageerror', error => errors.push(error.message));
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await app.evaluate(({ dialog }, folder) => { dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] }); }, workspace);
  const sessionId = await page.evaluate(async () => {
    const folder = await window.desktop.chooseFolder();
    return (await window.desktop.createTask(folder.id, crypto.randomUUID())).session.id;
  });
  await page.reload();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.waitForFunction(id => !!document.querySelector(`.session-row[data-session-id="${id}"]`), sessionId);
  await page.locator(`.session-row[data-session-id="${sessionId}"]`).click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');

  await page.locator('#message').fill('请使用工具读取 example.txt，然后把文件内容替换为 EDITED_BY_AGENT 并保存。只修改 example.txt，不要修改其他文件。完成后只回复 REAL_PROVIDER_DONE。');
  await page.locator('#send').click();

  const deadline = Date.now() + 240000;
  let edited = false;
  while (Date.now() < deadline) {
    if (await page.getByRole('button', { name: '允许一次', exact: true }).count()) {
      await page.getByRole('button', { name: '允许一次', exact: true }).click();
    }
    const content = await readFile(path.join(workspace, 'example.txt'), 'utf8').catch(() => '');
    if (content.includes('EDITED_BY_AGENT')) { edited = true; break; }
    const failed = await page.evaluate(() => document.getElementById('conversation-status')?.textContent === '失败');
    if (failed) break;
    await page.waitForTimeout(2000);
  }
  evidence.fileEdited = edited;
  assert.equal(edited, true, 'the real model did not edit example.txt');
  const finalText = await readFile(path.join(workspace, 'example.txt'), 'utf8');
  assert.match(finalText, /EDITED_BY_AGENT/);
  await page.waitForFunction(() => ['已回答', '已完成'].includes(document.querySelector('#conversation-status')?.textContent), null, { timeout: 120000 });

  await page.locator('#header-right-toggle').click();
  const filesTab = page.locator('.pane-tab').filter({ hasText: /^文件$/ });
  if (await filesTab.count()) await filesTab.first().click();
  else {
    await page.waitForSelector('.start-card');
    await page.locator('.start-card').filter({ hasText: '工作区文件' }).click();
  }
  await page.waitForSelector('.files-row[data-path="example.txt"]');
  await page.locator('.files-row[data-path="example.txt"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('EDITED_BY_AGENT'));
  evidence.previewReadsEditedFile = true;

  await page.locator('#message').fill('/diff');
  await page.locator('#message').press('Enter');
  await page.waitForSelector('.review-head');
  await page.waitForFunction(() => [...document.querySelectorAll('.review-line.added')].some(line => line.textContent.includes('EDITED_BY_AGENT')));
  evidence.reviewShowsEdit = true;
  await page.locator('.review-tools button[aria-label="打开完整文件"]').click();
  await page.waitForFunction(() => document.querySelector('.preview-body')?.textContent.includes('EDITED_BY_AGENT'));
  evidence.reviewToPreview = true;
  await page.screenshot({ path: path.join(output, 'real-provider-files.png') });

  assert.deepEqual(errors, []);
  evidence.pageErrors = errors;
  evidence.passed = true;
  console.log(JSON.stringify(evidence, null, 2));
} catch (error) {
  evidence.passed = false;
  evidence.error = error.message;
  evidence.pageErrors = errors;
  console.error(JSON.stringify(evidence, null, 2));
  throw error;
} finally {
  await writeFile(path.join(output, 'real-provider-evidence.json'), JSON.stringify(evidence, null, 2) + '\n').catch(() => {});
  if (app) await app.close().catch(() => {});
  await rm(temporary, { recursive: true, force: true }).catch(() => {});
}
