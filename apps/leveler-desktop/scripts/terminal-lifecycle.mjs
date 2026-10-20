// Real xterm renderer, controlled RPC races. This complements real host acceptance.
import { _electron as electron } from 'playwright';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
const root = fileURLToPath(new URL('..', import.meta.url));
const temporary = await mkdtemp('/tmp/leveler-terminal-lifecycle-');
const app = await electron.launch({ args: [root, `--user-data-dir=${temporary}`], env: { ...process.env, ELECTRON_RUN_AS_NODE: undefined } });
try {
  const page = await app.firstWindow();
  await page.waitForFunction(() => window.desktop && document.querySelector('#new-session'));
  const evidence = await page.evaluate(async () => {
    const { DesktopTerminals } = await import('./terminal.mjs');
    const running = { status: 'running', shell: '/bin/zsh', next_cursor: 0 };
    const tick = () => new Promise(resolve => requestAnimationFrame(resolve));
    const session = { id: crypto.randomUUID(), repository: '/tmp' };
    const calls = [];
    let resolveCreate;
    const api = { createTerminal: () => { calls.push('create'); return new Promise(resolve => { resolveCreate = resolve; }); }, observeTerminal: async () => running, resizeTerminal: async () => {}, closeTerminal: async () => { calls.push('close'); return { status: 'killed' }; }, writeTerminal: async () => {} };
    const controller = new DesktopTerminals(api, () => {});
    const tab = { terminalId: crypto.randomUUID() };
    document.body.append(controller.view(tab, session));
    const closing = controller.close(tab, session.id);
    await tick(); await tick();
    if (calls.join(',') !== 'create') throw new Error(`close raced ahead of create: ${calls}`);
    resolveCreate(running); await closing;
    if (calls.join(',') !== 'create,close') throw new Error(`wrong lifecycle: ${calls}`);

    let observeResolve;
    let closeResolve;
    const secondApi = { ...api, createTerminal: async () => running, observeTerminal: () => new Promise(resolve => { observeResolve = resolve; }), closeTerminal: () => new Promise(resolve => { closeResolve = resolve; }) };
    const second = new DesktopTerminals(secondApi, () => {});
    const tab2 = { terminalId: crypto.randomUUID() };
    document.body.append(second.view(tab2, session));
    const entry = second.entries.get(`${session.id}:${tab2.terminalId}`);
    await entry.started;
    const closing2 = second.close(tab2, session.id); await Promise.resolve(); await Promise.resolve();
    observeResolve(running); await Promise.resolve(); await Promise.resolve();
    if (entry.status !== 'killing' || !entry.term.options.disableStdin) throw new Error('late observe reopened input');
    closeResolve({ status: 'killed' }); await closing2;

    let writes = 0;
    const third = new DesktopTerminals({ ...api, createTerminal: async () => running, observeTerminal: async () => ({ status: 'exited', next_cursor: 0 }), writeTerminal: async () => { writes++; throw new Error('outcome unknown'); } }, () => {});
    const tab3 = { terminalId: crypto.randomUUID() };
    document.body.append(third.view(tab3, session));
    const e3 = third.entries.get(`${session.id}:${tab3.terminalId}`); await e3.started;
    await tick(); third.setStatus(e3, 'running');
    third.write(e3, new TextEncoder().encode('first\r')); third.write(e3, new TextEncoder().encode('second\r')); await e3.writes;
    if (writes !== 1 || e3.status !== 'unknown') throw new Error('unknown input replayed');
    e3.term.write('STALE_TEXT\x1b[31m'); await tick();
    third.apply(e3, { ...running, truncated: true, data_base64: btoa('FRESH_OUTPUT') }); await tick(); await tick();
    const buffer = Array.from({ length: e3.term.buffer.active.length }, (_, i) => e3.term.buffer.active.getLine(i)?.translateToString()).join('\n');
    if (buffer.includes('STALE_TEXT') || !buffer.includes('FRESH_OUTPUT')) throw new Error('truncation retained stale terminal state');
    await third.close(tab3, session.id);
    for (const item of [controller, second, third]) item.themeObserver.disconnect();
    return { immediateClose: true, lateObserve: true, unknownWriteNoRetry: true, truncationReset: true };
  });
  assert.deepEqual(evidence, { immediateClose: true, lateObserve: true, unknownWriteNoRetry: true, truncationReset: true });
  console.log(JSON.stringify(evidence));
} finally {
  await app.evaluate(({ app }) => app.exit(0)).catch(() => {}); await app.close().catch(() => {});
  await rm(temporary, { recursive: true, force: true });
}
