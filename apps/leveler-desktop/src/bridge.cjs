const { spawn } = require('node:child_process');
const { createInterface } = require('node:readline');

// Bounded backoff: the first reconnect is immediate enough to feel automatic,
// later attempts stay sparse, and the list ends so a broken install stops
// instead of polling forever. The renderer offers a manual retry at that point.
const DEFAULT_RETRY_DELAYS = [500, 1000, 2000, 4000, 8000, 15000, 30000, 30000];

class Bridge {
  constructor(binary, options = {}) {
    this.binary = binary;
    this.onEvent = options.onEvent ?? (() => {});
    this.onState = options.onState ?? (() => {});
    this.retryDelays = options.retryDelaysMs ?? DEFAULT_RETRY_DELAYS;
    this.env = options.env ?? process.env;
    this.pending = new Map(); this.nextId = 0;
    this.child = null; this.connectedChild = null; this.retryTimer = null;
    this.closed = false; this.everConnected = false; this.attempt = 0;
    this.spawn();
  }

  state(state, extra = {}) {
    this.current = { event: 'connection', state, ...extra };
    try { this.onState(this.current); } catch { /* a UI listener must not break transport */ }
  }

  status() {
    return this.current ?? { event: 'connection', state: this.closed ? 'closed' : 'connecting' };
  }

  spawn() {
    this.retryTimer = null;
    if (this.closed) return;
    let child;
    try {
      child = spawn(this.binary, ['desktop-bridge'], { stdio: ['pipe', 'pipe', 'pipe'], env: this.env, windowsHide: true });
    } catch (error) { this.lost(null, error); return; }
    this.child = child;
    child.stderr.on('data', data => process.stderr.write(data));
    const lines = createInterface({ input: child.stdout });
    lines.on('line', line => {
      if (this.child !== child) return;
      let frame; try { frame = JSON.parse(line); } catch { this.drop(child, new Error('Runtime bridge returned invalid JSON')); return; }
      this.markConnected(child);
      if (frame.event) { this.onEvent(frame); return; }
      const pending = this.pending.get(String(frame.id));
      if (!pending) return;
      this.pending.delete(String(frame.id)); clearTimeout(pending.timer);
      if (frame.ok) pending.resolve(frame.result);
      else {
        const error = new Error(frame.error?.message ?? 'Runtime request failed');
        error.kind = frame.error?.kind; error.state = frame.error?.state; error.nextStep = frame.error?.next_step;
        pending.reject(error);
      }
    });
    child.on('error', error => { if (this.child === child) this.lost(child, error); });
    child.on('exit', (code, signal) => { if (this.child === child) this.lost(child, new Error(`Runtime bridge disconnected (${signal ?? code})`)); });
    // A fresh child speaks only when asked. The probe proves the pipe before
    // `connected` is reported, which is also what triggers session restore.
    this.request('ping').catch(() => {});
  }

  markConnected(child) {
    if (this.connectedChild === child) return;
    this.connectedChild = child;
    const resumed = this.everConnected;
    this.everConnected = true;
    this.attempt = 0;
    this.state('connected', { resumed });
  }

  // A live child that violates the protocol is not trustworthy. Kill it, then
  // use the same bounded respawn path instead of leaving a half-dead pipe.
  drop(child, error) {
    try { child.kill(); } catch { /* already gone */ }
    this.lost(child, error);
  }

  lost(child, error) {
    if (child !== null && this.child !== child) return;
    this.child = null; this.connectedChild = null;
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      const failure = new Error(error?.message ?? 'Runtime bridge disconnected');
      // A request already written to the pipe may have reached the runtime
      // before the process died; one that never got that far definitely did not.
      failure.kind = pending.sent ? 'outcome_unknown' : 'not_sent';
      pending.reject(failure);
    }
    this.pending.clear();
    if (this.closed) return;
    this.state('disconnected', { message: error?.message ?? 'Runtime bridge disconnected' });
    this.schedule();
  }

  schedule() {
    const delay = this.retryDelays[this.attempt];
    if (delay === undefined) {
      this.state('failed', { attempt: this.attempt, message: '无法连接 Runtime，请重试' });
      return;
    }
    this.attempt += 1;
    this.state('reconnecting', { attempt: this.attempt, retry_in_ms: delay });
    this.retryTimer = setTimeout(() => this.spawn(), delay);
  }

  retry() {
    if (this.closed) return;
    if (this.retryTimer) { clearTimeout(this.retryTimer); this.retryTimer = null; }
    if (this.child) return;
    this.attempt = 0;
    this.spawn();
  }

  request(method, params = {}) {
    if (this.closed) {
      const error = new Error('Desktop is closing'); error.kind = 'closed';
      return Promise.reject(error);
    }
    const child = this.child;
    if (!child || child.stdin.destroyed) {
      const error = new Error('Runtime bridge is reconnecting; the request was not sent');
      error.kind = 'not_sent';
      return Promise.reject(error);
    }
    const id = String(++this.nextId);
    return new Promise((resolve, reject) => {
      // A timeout is uncertain delivery, never a successful task or automatic retry.
      const timer = setTimeout(() => {
        this.pending.delete(id);
        const error = new Error('Runtime request timed out; delivery may have occurred. Refresh before sending again.');
        error.kind = 'outcome_unknown';
        reject(error);
      }, 45000);
      const pending = { resolve, reject, timer, sent: false };
      this.pending.set(id, pending);
      try {
        child.stdin.write(JSON.stringify({ id, method, params }) + '\n', error => {
          if (!error) { pending.sent = true; return; }
          clearTimeout(timer); this.pending.delete(id);
          const failure = new Error(error.message); failure.kind = 'not_sent';
          reject(failure);
        });
      } catch (error) {
        clearTimeout(timer); this.pending.delete(id);
        const failure = new Error(error.message); failure.kind = 'not_sent';
        reject(failure);
      }
    });
  }

  close() {
    if (this.closing) return this.closing;
    this.closed = true;
    if (this.retryTimer) { clearTimeout(this.retryTimer); this.retryTimer = null; }
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      const error = new Error('Desktop closed'); error.kind = 'closed';
      pending.reject(error);
    }
    this.pending.clear();
    const child = this.child;
    this.child = null; this.connectedChild = null;
    if (!child) return (this.closing = Promise.resolve());
    try { child.stdin.end(); } catch { /* already gone */ }
    this.closing = new Promise(resolve => {
      if (child.exitCode !== null || child.signalCode !== null) return resolve();
      const timer = setTimeout(() => { try { child.kill(); } catch { /* gone */ } resolve(); }, 1500);
      child.once('exit', () => { clearTimeout(timer); resolve(); });
    });
    return this.closing;
  }
}
module.exports = { Bridge };
