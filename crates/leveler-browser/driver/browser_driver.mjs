// CodeLeveler browser driver — the thin Playwright bridge.
//
// CodeLeveler owns the browser DOMAIN (runtime, refs, generations, tools,
// permissions, profile). This process is ONLY the engine adapter: it exposes a
// minimal set of primitives over newline-delimited JSON-RPC 2.0 on stdio and
// does no policy, no ref-generation bookkeeping, no snapshot budgeting — those
// live in Rust.
//
// Protocol contract:
//   - stdout carries EXACTLY one JSON message per line and nothing else.
//   - all diagnostics go to stderr.
//   - requests:  {"jsonrpc":"2.0","id":N,"method":M,"params":{...}}
//   - responses: {"jsonrpc":"2.0","id":N,"result":{...}}
//                {"jsonrpc":"2.0","id":N,"error":{"code":C,"message":S,"kind":K}}
//   - events (no id): {"jsonrpc":"2.0","method":"event","params":{...}}
//
// Playwright is resolved from the CodeLeveler-managed install (this file lives
// at <home>/runtimes/browser/driver/ and playwright at <home>/runtimes/browser/
// node_modules), never the user's project.

import { chromium } from 'playwright';

// ── stdout protocol writer (line-buffered, JSON only) ───────────────────────
function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}
function ok(id, result) {
  send({ jsonrpc: '2.0', id, result });
}
function fail(id, kind, message) {
  send({ jsonrpc: '2.0', id, error: { code: -32000, message: String(message), kind } });
}
function event(params) {
  send({ jsonrpc: '2.0', method: 'event', params });
}

// ── runtime state ───────────────────────────────────────────────────────────
let context = null; // Playwright BrowserContext (persistent)
let engine = null; // reported engine label
let browserVersion = null;
const pages = new Map(); // pageId -> Page
let nextPageId = 1;
const consoleLog = new Map(); // pageId -> [{level,text}]
const dialogPolicy = new Map(); // pageId -> {action, promptText}

function pageMeta(id, page) {
  return { pageId: id, url: page.url(), title: undefined };
}

async function titleSafe(page) {
  try {
    return await page.title();
  } catch {
    return '';
  }
}

function registerPage(page) {
  const id = 'page-' + nextPageId++;
  pages.set(id, page);
  consoleLog.set(id, []);
  dialogPolicy.set(id, { action: 'dismiss' });

  page.on('console', (msg) => {
    const level = msg.type();
    if (level === 'error' || level === 'warning' || level === 'warn') {
      const log = consoleLog.get(id);
      if (log) {
        log.push({ level, text: msg.text() });
        if (log.length > 200) log.shift();
      }
      event({ kind: 'console', pageId: id, level, text: msg.text() });
    }
  });
  page.on('pageerror', (err) => {
    const log = consoleLog.get(id);
    if (log) {
      log.push({ level: 'pageerror', text: String(err && err.message ? err.message : err) });
      if (log.length > 200) log.shift();
    }
    event({ kind: 'pageerror', pageId: id, text: String(err && err.message ? err.message : err) });
  });
  // A dialog must be answered or the triggering JS hangs. Consume the per-page
  // policy (default: dismiss — the safe no-op), surface it, then reset.
  page.on('dialog', async (dialog) => {
    const policy = dialogPolicy.get(id) || { action: 'dismiss' };
    dialogPolicy.set(id, { action: 'dismiss' });
    event({
      kind: 'dialog',
      pageId: id,
      type: dialog.type(),
      message: dialog.message(),
      defaultValue: dialog.defaultValue() || undefined,
    });
    try {
      if (policy.action === 'accept') await dialog.accept(policy.promptText);
      else await dialog.dismiss();
    } catch {
      /* dialog already handled */
    }
  });
  page.on('close', () => {
    pages.delete(id);
    consoleLog.delete(id);
    dialogPolicy.delete(id);
  });
  return id;
}

function requirePage(pageId) {
  const page = pages.get(pageId);
  if (!page) {
    const e = new Error('no such page: ' + pageId);
    e.kind = 'page_closed';
    throw e;
  }
  return page;
}

// ── methods ──────────────────────────────────────────────────────────────────
const methods = {
  async ping() {
    return { ok: true };
  },

  async launch(p) {
    if (context) return { engine, browserVersion };
    const opts = { headless: p.headless !== false };
    if (p.channel) opts.channel = p.channel; // e.g. "chrome"
    if (p.executablePath) opts.executablePath = p.executablePath;
    // Never inherit ambient proxy/user state beyond the given user-data-dir.
    context = await chromium.launchPersistentContext(p.userDataDir, opts);
    engine = p.channel ? 'system:' + p.channel : (p.executablePath ? 'system:chromium' : 'managed:chromium');
    try {
      browserVersion = context.browser() ? context.browser().version() : 'unknown';
    } catch {
      browserVersion = 'unknown';
    }
    // Adopt any auto-created initial page.
    for (const pg of context.pages()) registerPage(pg);
    // Track pages opened by target=_blank / window.open.
    context.on('page', (pg) => registerPage(pg));
    return { engine, browserVersion };
  },

  async newPage() {
    const page = await context.newPage();
    // context 'page' event may have already registered it; find its id.
    for (const [id, pg] of pages) if (pg === page) return { pageId: id, url: page.url(), title: await titleSafe(page) };
    const id = registerPage(page);
    return { pageId: id, url: page.url(), title: await titleSafe(page) };
  },

  async listPages() {
    const out = [];
    for (const [id, page] of pages) {
      out.push({ pageId: id, url: page.url(), title: await titleSafe(page), active: false });
    }
    if (out.length) out[out.length - 1].active = true;
    return { pages: out };
  },

  async navigate(p) {
    const page = requirePage(p.pageId);
    const resp = await page.goto(p.url, { timeout: p.timeoutMs || 30000, waitUntil: 'domcontentloaded' });
    return { url: page.url(), title: await titleSafe(page), status: resp ? resp.status() : null };
  },

  async snapshot(p) {
    const page = requirePage(p.pageId);
    // `_snapshotForAI` is the ref-annotated ("[ref=e5]") accessibility snapshot
    // Playwright exposes for agents (also used by @playwright/mcp); it is what
    // makes `aria-ref=` interaction possible. Fall back to the public
    // ariaSnapshot (no refs — read-only) only if the engine lacks it.
    let aria;
    if (typeof page._snapshotForAI === 'function') aria = await page._snapshotForAI();
    else aria = await page.locator('body').ariaSnapshot();
    return { url: page.url(), title: await titleSafe(page), aria };
  },

  async click(p) {
    const page = requirePage(p.pageId);
    const before = context.pages().length;
    const urlBefore = page.url();
    await page.locator('aria-ref=' + p.ref).click({ timeout: p.timeoutMs || 15000 });
    const after = context.pages().length;
    let newPage = null;
    if (after > before) {
      // The last registered page is the freshly opened one.
      const ids = [...pages.keys()];
      newPage = ids[ids.length - 1];
    }
    return { url: page.url(), title: await titleSafe(page), navigated: page.url() !== urlBefore, newPage };
  },

  async type(p) {
    const page = requirePage(p.pageId);
    const loc = page.locator('aria-ref=' + p.ref);
    const t = p.timeoutMs || 15000;
    if (p.append) {
      await loc.click({ timeout: t });
      await loc.pressSequentially(p.text, { timeout: t });
    } else {
      await loc.fill(p.text, { timeout: t }); // focus + clear + set
    }
    return { url: page.url(), title: await titleSafe(page) };
  },

  async select(p) {
    const page = requirePage(p.pageId);
    const loc = page.locator('aria-ref=' + p.ref);
    // Accept by label first, then value — real DOM events fire.
    const by = (p.by === 'value') ? p.values.map((v) => ({ value: v })) : p.values.map((v) => ({ label: v }));
    const selected = await loc.selectOption(by, { timeout: p.timeoutMs || 15000 });
    return { url: page.url(), title: await titleSafe(page), selected };
  },

  async press(p) {
    const page = requirePage(p.pageId);
    const t = p.timeoutMs || 15000;
    if (p.ref) await page.locator('aria-ref=' + p.ref).press(p.key, { timeout: t });
    else await page.keyboard.press(p.key);
    return { url: page.url(), title: await titleSafe(page) };
  },

  async waitFor(p) {
    const page = requirePage(p.pageId);
    const t = p.timeoutMs || 15000;
    const c = p.condition || {};
    if (c.load) await page.waitForLoadState('load', { timeout: t });
    else if (c.urlContains) await page.waitForURL((u) => u.href.includes(c.urlContains), { timeout: t });
    else if (c.textVisible) await page.getByText(c.textVisible, { exact: false }).first().waitFor({ state: 'visible', timeout: t });
    else if (c.textGone) await page.getByText(c.textGone, { exact: false }).first().waitFor({ state: 'hidden', timeout: t });
    else if (c.refVisible) await page.locator('aria-ref=' + c.refVisible).waitFor({ state: 'visible', timeout: t });
    else if (c.refHidden) await page.locator('aria-ref=' + c.refHidden).waitFor({ state: 'hidden', timeout: t });
    else await page.waitForTimeout(Math.min(t, 250));
    return { satisfied: true, url: page.url(), title: await titleSafe(page) };
  },

  async setDialogPolicy(p) {
    dialogPolicy.set(p.pageId, { action: p.action || 'dismiss', promptText: p.promptText });
    return { ok: true };
  },

  async consoleTail(p) {
    return { entries: consoleLog.get(p.pageId) || [] };
  },

  async screenshot(p) {
    const page = requirePage(p.pageId);
    const buf = await page.screenshot({ timeout: p.timeoutMs || 15000, fullPage: !!p.fullPage });
    return { base64: buf.toString('base64'), mediaType: 'image/png' };
  },

  async closePage(p) {
    const page = pages.get(p.pageId);
    if (page) await page.close();
    return { ok: true };
  },

  async shutdown() {
    try {
      if (context) await context.close();
    } catch {
      /* ignore */
    }
    return { ok: true };
  },
};

function classifyError(err) {
  if (err && err.kind) return err.kind;
  const m = String(err && err.message ? err.message : err);
  if (/aria-ref/.test(m) || /No node found for selector/.test(m) || /resolved to \d+ elements/.test(m)) return 'ref_stale';
  if (/Timeout .* exceeded/.test(m) || /timeout/i.test(m)) return 'action_timeout';
  if (/Target (page|closed)/.test(m) || /has been closed/.test(m)) return 'page_closed';
  return 'action_failed';
}

async function handle(msg) {
  const { id, method, params } = msg;
  const fn = methods[method];
  if (!fn) return fail(id, 'action_failed', 'unknown method: ' + method);
  try {
    const result = await fn(params || {});
    ok(id, result);
  } catch (err) {
    fail(id, classifyError(err), err && err.message ? err.message : err);
  }
}

// ── stdin line reader (robust to partial lines) ─────────────────────────────
let buf = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buf += chunk;
  let nl;
  while ((nl = buf.indexOf('\n')) >= 0) {
    const line = buf.slice(0, nl);
    buf = buf.slice(nl + 1);
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      process.stderr.write('driver: bad json line: ' + e + '\n');
      continue;
    }
    handle(msg);
  }
});
process.stdin.on('end', () => {
  methods.shutdown().finally(() => process.exit(0));
});
process.on('uncaughtException', (e) => {
  process.stderr.write('driver: uncaught: ' + (e && e.stack ? e.stack : e) + '\n');
});
