// R5 blocked-session display: open a real blocked session in the real Desktop
// and verify the runtime's state is what the UI shows. Read-only; sends nothing.
import { _electron as electron } from '../node_modules/playwright/index.mjs';
import { writeFile, mkdir } from 'node:fs/promises';
import path from 'node:path';
import os from 'node:os';

const root = process.env.DESKTOP_ROOT || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/apps/leveler-desktop';
const bin = process.env.LEVELER_BIN || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/target/release/leveler';
const workspace = process.env.LEVELER_WORKSPACE || '/Users/mengmian/Develop/app/csdn/DevOrder';
const out = process.env.R5_EVIDENCE || '/Users/mengmian/Develop/app/CodeLeveler-harness-closure-20261126/docs/dev/harness-reliability-final-closure-2026-11-26/evidence/desktop/r5';
const blocked = '7e9891f7-02af-4e65-8a98-5efcfc891dba';
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

async function reveal() {
  const section = page.locator(`section.workspace-group[data-workspace-path="${workspace}"]`);
  await section.waitFor({ state: 'visible', timeout: 60000 });
  for (let i = 0; i < 12; i += 1) {
    if (await page.locator(`.session-row[data-session-id="${blocked}"]`).count()) return true;
    const fold = section.locator('.group-fold').first();
    if ((await fold.getAttribute('aria-expanded')) === 'false') await fold.click();
    const overflow = section.locator('.group-overflow').first();
    if (await overflow.count()) {
      await overflow.click();
      await page.waitForTimeout(250);
    } else {
      await page.waitForTimeout(250);
    }
  }
  return !!(await page.locator(`.session-row[data-session-id="${blocked}"]`).count());
}

try {
  await mkdir(out, { recursive: true });
  app = await electron.launch({ args: [root], env });
  page = await app.firstWindow();
  page.setDefaultTimeout(30000);
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  const visible = await reveal();
  check('the real blocked session is visible in the sidebar', visible);
  if (!visible) throw new Error('blocked session row not found');
  const row = page.locator(`.session-row[data-session-id="${blocked}"]`);
  const status = await row.getAttribute('data-status');
  await row.click();
  await page.waitForTimeout(1500);
  const conversation = await page.locator('#conversation').innerText();
  const header = await page.locator('#conversation-status').innerText().catch(() => '');
  check(
    'the blocked session opens with its durable blocked history',
    status === 'blocked' && (header.includes('受阻') || /受阻|blocked|失败/.test(conversation)),
    `row=${status}, header=${header}, tail=${conversation.slice(-240)}`,
  );
  await page.screenshot({ path: path.join(out, '41-blocked-session.png') });
  evidence.passed = evidence.checks.every((entry) => entry.ok);
  await writeFile(path.join(out, 'r5-blocked.json'), JSON.stringify(evidence, null, 2));
  process.exit(evidence.passed ? 0 : 1);
} catch (error) {
  evidence.passed = false;
  evidence.error = String(error);
  await writeFile(path.join(out, 'r5-blocked.json'), JSON.stringify(evidence, null, 2));
  console.error(error);
  process.exit(1);
} finally {
  if (app) await app.close().catch(() => {});
}
