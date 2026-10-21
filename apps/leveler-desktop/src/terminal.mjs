import { Terminal } from '../node_modules/@xterm/xterm/lib/xterm.mjs';
import { FitAddon } from '../node_modules/@xterm/addon-fit/lib/addon-fit.mjs';

// Each tab owns one emulator; DOM redraws only move this node. Processes and
// their lifecycle remain exclusively owned by the existing ExecutionHost.
export class DesktopTerminals {
  constructor(api, onTitle) {
    this.api = api; this.onTitle = onTitle; this.entries = new Map();
    this.themeObserver = new MutationObserver(() => this.refresh());
    this.themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['style', 'class', 'data-theme'] });
  }
  view(tab, session) {
    if (!session?.id || !session.repository) {
      const node = document.createElement('div'); node.className = 'terminal-offline';
      node.textContent = '请先打开有工作区的会话，再新建终端'; return node;
    }
    tab.terminalId ??= crypto.randomUUID();
    const key = `${session.id}:${tab.terminalId}`;
    let entry = this.entries.get(key);
    if (!entry) { entry = this.create(tab, session); this.entries.set(key, entry); }
    requestAnimationFrame(() => { this.fit(entry); if (entry.status === 'running') entry.term.focus(); });
    return entry.view;
  }
  create(tab, session) {
    const view = document.createElement('div'); view.className = 'terminal-view'; view.dataset.terminalId = tab.terminalId;
    const screen = document.createElement('div'); screen.className = 'terminal-screen';
    const status = document.createElement('div'); status.className = 'terminal-status'; status.setAttribute('role', 'status');
    const detail = document.createElement('span'); detail.className = 'terminal-status-detail';
    const retry = document.createElement('button'); retry.type = 'button'; retry.textContent = '重新查询状态'; retry.hidden = true;
    status.append(detail, retry); view.append(screen, status);
    const term = new Terminal({ cursorBlink: true, scrollback: 5000, allowProposedApi: false, allowTransparency: false });
    const fit = new FitAddon(); term.loadAddon(fit);
    const entry = { tab, view, screen, detail, retry, term, fit, status: 'starting', cursor: 0, closed: false, shell: '', params: { session_id: session.id, terminal_id: tab.terminalId }, writes: Promise.resolve() };
    this.setStatus(entry, 'starting');
    term.open(screen); this.style(entry);
    // Keep terminal keys (Ctrl+C, arrows, Escape) inside the terminal. Native
    // copy/paste still works through xterm's textarea and Electron's menu.
    screen.addEventListener('keydown', event => event.stopPropagation());
    screen.addEventListener('pointerdown', () => term.focus());
    term.onData(data => this.write(entry, new TextEncoder().encode(data)));
    term.onBinary(data => this.write(entry, Uint8Array.from(data, char => char.charCodeAt(0))));
    term.onResize(({ cols, rows }) => {
      if (entry.status !== 'running') return;
      this.api.resizeTerminal({ ...entry.params, cols, rows }).catch(error => this.unknown(entry, error));
    });
    entry.observer = new ResizeObserver(() => this.fit(entry)); entry.observer.observe(screen);
    retry.addEventListener('click', () => { retry.hidden = true; void this.observe(entry); });
    // Wait for attachment to obtain real terminal dimensions, never 0 x 0.
    entry.started = new Promise(resolve => requestAnimationFrame(resolve)).then(() => {
      this.fit(entry);
      return this.api.createTerminal({ ...entry.params, cols: term.cols, rows: term.rows }).then(result => {
        this.apply(entry, result); if (!entry.closed && !entry.closing) void this.observe(entry);
      }).catch(error => this.unknown(entry, error));
    });
    return entry;
  }
  setStatus(entry, status, message = '') {
    entry.status = status; entry.view.dataset.status = status;
    entry.term.options.disableStdin = status !== 'running';
    const labels = { starting: '正在启动…', running: '运行中', killing: '正在关闭…', exited: '已退出', killed: '已关闭', unknown: '状态未知' };
    entry.detail.textContent = `${entry.shell ? `${entry.shell} · ` : ''}${labels[status] ?? status}${message ? ` · ${message}` : ''}`;
    entry.retry.hidden = status !== 'unknown';
  }
  unknown(entry, error) {
    clearTimeout(entry.timer);
    this.setStatus(entry, 'unknown', `${error?.message ?? error}；输入未自动重发`);
  }
  apply(entry, result) {
    if (result.truncated) { entry.term.reset(); entry.term.writeln('[较早的终端输出已超出保留范围，已重置显示]\r\n'); }
    if (result.data_base64) entry.term.write(Uint8Array.from(atob(result.data_base64), char => char.charCodeAt(0)));
    if (Number.isSafeInteger(result.next_cursor)) entry.cursor = result.next_cursor;
    if (result.shell) {
      entry.shell = result.shell.split(/[\\/]/).at(-1);
      if (entry.tab.title !== entry.shell) { entry.tab.title = entry.shell; this.onTitle(); }
    }
    this.setStatus(entry, result.output_error && ['running', 'killing'].includes(result.status) ? 'unknown' : result.status, result.output_error ?? (result.exit_code != null ? `退出码 ${result.exit_code}` : ''));
  }
  async observe(entry) {
    if (entry.closed || entry.closing) return;
    try {
      const result = await this.api.observeTerminal({ ...entry.params, cursor: entry.cursor, max_bytes: 65536 });
      if (entry.closed || entry.closing) return;
      this.apply(entry, result);
      if (['running', 'killing'].includes(entry.status)) entry.timer = setTimeout(() => this.observe(entry), 75);
    } catch (error) { if (!entry.closed && !entry.closing) this.unknown(entry, error); }
  }
  write(entry, bytes) {
    if (entry.status !== 'running') return;
    // Sequential writes preserve input ordering. A failed/unknown delivery stops
    // queued input; replaying any of it could execute a command twice.
    for (let offset = 0; offset < bytes.length; offset += 16384) {
      const data_base64 = btoa(String.fromCharCode(...bytes.subarray(offset, offset + 16384)));
      entry.writes = entry.writes.then(async () => {
        if (entry.status !== 'running') return;
        try { await this.api.writeTerminal({ ...entry.params, data_base64 }); }
        catch (error) { this.unknown(entry, error); }
      });
    }
  }
  style(entry) {
    const css = getComputedStyle(document.documentElement);
    entry.term.options.fontFamily = css.getPropertyValue('--terminal-font').trim();
    entry.term.options.fontSize = parseFloat(css.getPropertyValue('--terminal-font-size')) || 13;
    entry.term.options.theme = { background: css.getPropertyValue('--bg-base').trim(), foreground: css.getPropertyValue('--text-primary').trim(), cursor: css.getPropertyValue('--text-primary').trim(), selectionBackground: '#4488cc66' };
  }
  fit(entry) {
    if (!entry.screen.isConnected || !entry.screen.clientWidth || !entry.screen.clientHeight || entry.closed) return;
    entry.fit.fit();
  }
  refresh() { for (const entry of this.entries.values()) { this.style(entry); this.fit(entry); } }
  async close(tab, sessionId) {
    const entry = this.entries.get(`${sessionId}:${tab.terminalId}`);
    if (!entry) return;
    if (entry.closing) return entry.closing;
    entry.closing = (async () => {
      await entry.started;
      clearTimeout(entry.timer); this.setStatus(entry, 'killing');
      try {
        const result = await this.api.closeTerminal(entry.params);
        this.apply(entry, result);
        if (!['exited', 'killed'].includes(result.status)) throw new Error('尚未确认终端进程退出');
        entry.closed = true; entry.observer.disconnect(); entry.term.dispose();
        this.entries.delete(`${sessionId}:${tab.terminalId}`);
      } catch (error) { this.unknown(entry, error); throw error; }
      finally { entry.closing = null; }
    })();
    return entry.closing;
  }
}
