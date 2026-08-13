// CodeLeveler browser driver — the thin Playwright bridge.
//
// CodeLeveler owns the browser DOMAIN (runtime, refs, generations, tools,
// permissions, profile). This process is ONLY the engine adapter: it exposes a
// minimal set of primitives over newline-delimited JSON-RPC 2.0 on stdio and
// does no ref-generation bookkeeping, no snapshot budgeting, no session/page
// ownership — those live in Rust.
//
// The ONE policy this process enforces is the SSRF network boundary (B-1): the
// browser is where a redirect, link click, form submit, window.open or
// window.location actually becomes a network request, so that is the only place
// the runtime's per-request IP gate can be applied. The blocked-range set below
// mirrors Rust `web_fetch::is_blocked_ip` (loopback is the allowed dev
// exception) and MUST stay in sync with it; the B-1 regressions pin that.
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
import dns from 'node:dns/promises';
import net from 'node:net';

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

// ── SSRF network boundary (B-1) ──────────────────────────────────────────────
// Every http/https request the browser is about to make is checked here, so a
// redirect / link click / window.open / form submit to a private, link-local or
// metadata address is aborted at the wire — not merely at the `navigate` URL
// argument. Loopback is the allowed dev-server exception. Non-network schemes
// (file:, data:, about:, blob:, chrome:) never egress to an IP and pass through.
let lastBlock = null; // {url, reason, at} — surfaced on the action that caused it

function v4Blocked(o) {
  const [a, b] = o;
  if (a === 127) return false; // loopback: allowed dev exception (handled here, not blocked)
  if (a === 0) return true; // 0.0.0.0/8 incl unspecified
  if (a === 10) return true; // 10/8 private
  if (a === 172 && (b & 0xf0) === 16) return true; // 172.16/12 private
  if (a === 192 && b === 168) return true; // 192.168/16 private
  if (a === 169 && b === 254) return true; // 169.254/16 link-local (incl 169.254.169.254 metadata)
  if (a === 100 && (b & 0xc0) === 64) return true; // 100.64/10 CGNAT
  if (a >= 224) return true; // multicast + reserved + broadcast
  return false;
}

function v6Blocked(ip) {
  const low = ip.toLowerCase();
  if (low === '::1') return false; // loopback allowed
  if (low === '::') return true; // unspecified
  const mapped = low.match(/^::ffff:(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})$/);
  if (mapped) return addrBlocked(mapped[1]);
  const head = low.startsWith('::') ? '' : low.split(':')[0];
  if (/^f[cd]/.test(head)) return true; // fc00::/7 unique-local
  if (/^fe[89ab]/.test(head)) return true; // fe80::/10 link-local
  if (/^ff/.test(head)) return true; // ff00::/8 multicast
  return false; // public IPv6 allowed
}

function addrBlocked(ip) {
  if (net.isIPv4(ip)) return v4Blocked(ip.split('.').map(Number));
  if (net.isIPv6(ip)) return v6Blocked(ip);
  return true; // unparseable → refuse
}

// Verdict for one request host. A literal IP is checked directly; a hostname is
// resolved and refused if ANY resolved address is blocked — evaluated here, at
// request time, so a name that rebinds to a private IP after the Rust arg-gate
// is still caught at the actual connect.
async function hostVerdict(host) {
  if (net.isIP(host)) {
    return addrBlocked(host)
      ? { allowed: false, reason: `blocked address ${host}` }
      : { allowed: true };
  }
  let addrs;
  try {
    addrs = await dns.lookup(host, { all: true });
  } catch {
    return { allowed: false, reason: `cannot resolve ${host}` };
  }
  for (const a of addrs) {
    if (addrBlocked(a.address)) {
      return { allowed: false, reason: `${host} resolves to blocked ${a.address}` };
    }
  }
  return { allowed: true };
}

async function installNetworkGate(ctx) {
  await ctx.route('**/*', async (route) => {
    const req = route.request();
    let u;
    try {
      u = new URL(req.url());
    } catch {
      return safeContinue(route);
    }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return safeContinue(route);
    const verdict = await hostVerdict(u.hostname);
    if (!verdict.allowed) return recordBlockAndAbort(route, req.url(), verdict.reason);
    // Playwright follows redirects INTERNALLY and does not re-invoke this handler
    // for the redirect hops — verified — so a public→3xx→private hop would slip
    // past a per-request host check. For navigations, follow redirects manually
    // (maxRedirects:0) and validate EVERY hop's host before fetching it.
    if (req.resourceType() === 'document') return followAndGate(route, req.url());
    return safeContinue(route);
  });
}

// Manually walk the redirect chain, refusing the first hop that targets a
// blocked host, and fulfilling the page with the final validated response.
async function followAndGate(route, startUrl) {
  let current = startUrl;
  try {
    for (let hop = 0; hop < 12; hop++) {
      const resp = await route.fetch({ url: current, maxRedirects: 0 });
      const status = resp.status();
      if (status < 300 || status >= 400) {
        return await route.fulfill({ response: resp });
      }
      const loc = resp.headers()['location'];
      if (!loc) return await route.fulfill({ response: resp });
      let next;
      try {
        next = new URL(loc, current);
      } catch {
        return await route.fulfill({ response: resp });
      }
      if (next.protocol === 'http:' || next.protocol === 'https:') {
        const v = await hostVerdict(next.hostname);
        if (!v.allowed) return recordBlockAndAbort(route, next.href, v.reason);
      }
      current = next.href;
    }
    return recordBlockAndAbort(route, current, 'too many redirects');
  } catch {
    // A fetch error against an already-validated host — hand back to the browser
    // rather than masking it; never a silent success against a private target.
    return safeContinue(route);
  }
}

function recordBlockAndAbort(route, url, reason) {
  lastBlock = { url, reason, at: Date.now() };
  event({ kind: 'blocked_request', url, reason });
  return safeAbort(route);
}

async function safeAbort(route) {
  try {
    return await route.abort('blockedbyclient');
  } catch {
    /* request already resolved/aborted */
  }
}

async function safeContinue(route) {
  try {
    return await route.continue();
  } catch {
    /* request already resolved/aborted */
  }
}

// If an action failed because the network gate aborted its request, turn the
// opaque failure into a typed `denied` — never a silent success, never a shell
// fallback. A block that fired within the grace window IS the cause of a
// same-navigation failure: a direct abort surfaces as net::ERR_*, while a
// redirect hop that we abort surfaces as Playwright "interrupted by another
// navigation to chrome-error://" (the aborted hop becomes an error page). Both
// are the gate; attribute them to it when a block just landed.
function blockedOr(err) {
  if (lastBlock && Date.now() - lastBlock.at < 3000) {
    const denied = new Error(`SSRF policy blocked ${lastBlock.url} (${lastBlock.reason})`);
    denied.kind = 'denied';
    lastBlock = null;
    return denied;
  }
  return err;
}

// A block that landed within the grace window, for surfacing on a non-navigating
// action (e.g. a link click whose navigation was aborted). Consumes the record.
function takeRecentBlock() {
  if (lastBlock && Date.now() - lastBlock.at < 3000) {
    const r = lastBlock.reason;
    lastBlock = null;
    return r;
  }
  return null;
}

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
    // B-1: enforce the SSRF network boundary on every request BEFORE any page
    // can navigate — covers redirects, clicks, window.open, form submits.
    await installNetworkGate(context);
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
    let resp;
    try {
      resp = await page.goto(p.url, { timeout: p.timeoutMs || 30000, waitUntil: 'domcontentloaded' });
    } catch (e) {
      throw blockedOr(e); // a gate-aborted navigation (incl. redirect) → denied
    }
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
    const urlBefore = page.url();
    // A target=_blank/window.open popup registers asynchronously; arm the popup
    // wait BEFORE clicking so the event is never missed. Resolves immediately
    // when a popup opens, else null after a short grace (no popup expected).
    const popupP = page.waitForEvent('popup', { timeout: 800 }).catch(() => null);
    await page.locator('aria-ref=' + p.ref).click({ timeout: p.timeoutMs || 15000 });
    const popup = await popupP;
    let newPage = null;
    if (popup) {
      // The context 'page' handler may already have registered it; find its id.
      for (const [id, pg] of pages) {
        if (pg === popup) {
          newPage = id;
          break;
        }
      }
      if (!newPage) newPage = registerPage(popup);
    }
    // A click whose resulting navigation (same tab, or a target=_blank/new tab)
    // was refused by the network gate is surfaced, not swallowed.
    const blocked = takeRecentBlock();
    return {
      url: page.url(),
      title: await titleSafe(page),
      navigated: page.url() !== urlBefore,
      newPage,
      ...(blocked ? { blocked } : {}),
    };
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
