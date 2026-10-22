import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const { installTerminalIpc } = require('../src/terminal-ipc.cjs');

const request = { session_id: 'session-1', terminal_id: '3f1a2b3c-4d5e-6f70-8192-a3b4c5d6e7f8', cols: 80, rows: 24 };

function harness(failure) {
  const handlers = new Map();
  const calls = [];
  const bridge = {
    request: async (method, params) => {
      calls.push({ method, params });
      if (method === 'create_terminal' && failure) throw Object.assign(new Error('runtime error: Unsupported: Execution Host missing terminal.pty'), { kind: failure });
      return { status: 'running' };
    },
  };
  const shutdown = installTerminalIpc({ handle: (channel, handler) => handlers.set(channel, handler), bridge });
  return { handlers, calls, shutdown };
}

test('a rejected terminal create is not offered to shutdown cleanup', async () => {
  const h = harness('rejected');
  await assert.rejects(h.handlers.get('desktop:terminal-create')(request), /missing terminal\.pty/);
  await h.shutdown();
  assert.equal(h.calls.filter(call => call.method === 'close_terminal').length, 0,
    'a terminal the runtime refused must have no process to close');
});

test('an outcome-unknown create keeps its id for cleanup', async () => {
  const h = harness('outcome_unknown');
  await assert.rejects(h.handlers.get('desktop:terminal-create')(request), /missing terminal\.pty/);
  await h.shutdown();
  assert.deepEqual(h.calls.filter(call => call.method === 'close_terminal').map(call => call.params.terminal_id), [request.terminal_id],
    'an unconfirmed create may still own a process');
});
