// R5 Desktop secret custody smoke: a plaintext-looking credential must open
// the review, be captured with the value absent from the UI/persisted message,
// and still produce a real model answer.
import { _electron as electron } from '../node_modules/playwright/index.mjs';
import { writeFile, mkdir } from 'node:fs/promises';
import { randomBytes } from 'node:crypto';
import { readdirSync, statSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import path from 'node:path';
import os from 'node:os';

const root = process.env.DESKTOP_ROOT || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/apps/leveler-desktop';
const bin = process.env.LEVELER_BIN || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/target/release/leveler';
const workspace = process.env.LEVELER_WORKSPACE || '/Users/mengmian/Develop/app/csdn/DevOrder';
const out = process.env.R5_EVIDENCE || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/docs/dev/harness-reliability-final-closure-2026-11-26/evidence/desktop/r5';
const canary = `sk-${randomBytes(20).toString('hex')}`;
const env = { ...process.env, LEVELER_BINARY: bin };
delete env.LEVELER_HOME;
delete env.LEVELER_CONFIG_DIR;
delete env.ELECTRON_RUN_AS_NODE;
const evidence = { binary: bin, home: path.join(os.homedir(), '.leveler'), checks: [] };
let app, page;
const check = (name, ok, detail = '') => {
  evidence.checks.push({ name, ok: !!ok, detail });
  console.log((ok ? 'ok   ' : 'FAIL ') + name + (detail ? ' — ' + detail : ''));
};

function sessionDb(sessionId) {
  const state = path.join(os.homedir(), '.leveler', 'state');
  const candidates = [path.join(state, 'no-workspace', 'sessions.db')];
  const projects = path.join(state, 'projects');
  for (const name of readdirSync(projects)) candidates.push(path.join(projects, name, 'sessions.db'));
  for (const db of candidates) {
    try {
      if (!statSync(db).isFile()) continue;
      const found = execFileSync('sqlite3', [db, `select count(*) from sessions where id='${sessionId}'`], { encoding: 'utf8' }).trim();
      if (found === '1') return db;
    } catch { /* next */ }
  }
  return null;
}

async function launchNewWorkspaceSession() {
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  await page.locator('#new-session').click();
  await page.waitForFunction(() => document.querySelector('#conversation-status')?.textContent !== '连接 Runtime…');
  await app.evaluate(({ dialog }, folder) => {
    dialog.showOpenDialog = async () => ({ canceled: false, filePaths: [folder] });
  }, workspace);
  await page.locator('#add-workspace').click();
  await page.waitForFunction(
    (folder) => document.querySelector('#workspace-picker')?.textContent.includes(folder.split('/').pop()),
    workspace,
    { timeout: 60000 },
  );
  const picker = page.getByRole('button', { name: /请选择模型/ });
  if (await picker.count()) {
    await picker.first().click();
    await page.waitForTimeout(400);
    const option = page.getByText('deepseek-v4.1-flash-1017', { exact: true });
    if (await option.count()) await option.first().click();
  }
  await page.waitForTimeout(800);
  return null;
}

try {
  await mkdir(out, { recursive: true });
  await launchNewWorkspaceSession();
  const scan = await page.evaluate(text => window.desktop.scanSecrets(text), `测试凭据 DEMO_API_KEY=${canary}`);
  check('the host scan reports the credential candidate', (scan?.findings ?? []).length > 0, JSON.stringify(scan?.findings?.length ?? scan));
  await page.locator('#message').fill(`测试凭据 DEMO_API_KEY=${canary}；只回答 SECRET-OK，不要调用工具，不要写文件，也不要在回答里复述凭据。`);
  await page.locator('#send').click();
  const modal = page.locator('.secret-review-modal');
  const opened = await modal.waitFor({ state: 'visible', timeout: 30000 }).then(() => true).catch(() => false);
  if (!opened) {
    await page.screenshot({ path: path.join(out, '50-secret-no-review.png') });
    const errorText = await page.locator('#composer-error').innerText().catch(() => '');
    const draftText = await page.locator('#message').inputValue().catch(() => '');
    console.log('composer-error:', errorText, 'draft-length:', draftText.length, 'modal-count:', await modal.count());
  }
  check('the Desktop opens the credential review', opened);
  if (opened) {
    const modalText = await modal.innerText();
    check('the review never paints the value', !modalText.includes(canary), modalText.slice(0, 200));
    await page.screenshot({ path: path.join(out, '50-secret-review.png') });
    await page.getByRole('button', { name: '安全接管并发送' }).click();
  } else {
    throw new Error('credential review did not open');
  }
  await page.waitForFunction(
    () => [...document.querySelectorAll('.message.assistant .markdown')].some((node) => node.textContent.includes('SECRET-OK')),
    null,
    { timeout: 180000 },
  );
  const selected = page.locator('.session-row[aria-selected="true"]');
  await selected.waitFor({ state: 'visible', timeout: 30000 });
  const sessionId = await selected.getAttribute('data-session-id');
  check('a real workspace session exists for the secret review', !!sessionId, String(sessionId));
  const transcript = await page.locator('#conversation').innerText();
  check('the real answer arrives after secure capture', transcript.includes('SECRET-OK'));
  check('the transcript never paints the value', !transcript.includes(canary), transcript.slice(-300));
  const db = sessionDb(sessionId);
  let persisted = '';
  if (db) {
    const { execFileSync } = await import('node:child_process');
    persisted = execFileSync('sqlite3', [db, `select payload from session_messages where session_id='${sessionId}'`], { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024 });
  }
  check('the persisted session message never carries the value', !!db && !persisted.includes(canary), db || 'session db not found');
  check('the persisted message carries an opaque secret reference or placeholder', /\[secret:sec_[0-9a-f]{32}\]|\{\{secret-capture:/.test(persisted), persisted.slice(0, 200));
  await page.screenshot({ path: path.join(out, '51-secret-after.png') });
  evidence.session = sessionId;
  evidence.passed = evidence.checks.every((entry) => entry.ok);
  await writeFile(path.join(out, 'r5-secret.json'), JSON.stringify(evidence, null, 2));
  process.exit(evidence.passed ? 0 : 1);
} catch (error) {
  evidence.passed = false;
  evidence.error = String(error);
  await writeFile(path.join(out, 'r5-secret.json'), JSON.stringify(evidence, null, 2));
  console.error(error);
  process.exit(1);
} finally {
  if (app) await app.close().catch(() => {});
}
