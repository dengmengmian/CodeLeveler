// CodeLeveler browser driver — the thin Playwright bridge.
//
// CodeLeveler owns the browser DOMAIN (runtime, refs, generations, tools,
// permissions, profile). This process is ONLY the engine adapter: it exposes a
// minimal set of primitives over newline-delimited JSON-RPC 2.0 on stdio and
// does no ref-generation bookkeeping, no snapshot budgeting, no session/page
// ownership — those live in Rust.
//
// The ONE security policy this process enforces is the SSRF network boundary
// (B-1), and it enforces it through a SINGLE fail-closed connect-time authority:
// a CodeLeveler-owned forward proxy that Chromium is launched to use. Every
// http/https/ws/wss egress therefore hands the proxy the target host and the
// proxy is the sole resolver+connector (resolve → validate → pin → connect), so
// a DNS name that rebinds safe→private between check and connect can never reach
// the private endpoint. Redirects, cookies, origin and TLS stay 100% native.
// Loopback LITERALS keep Chrome's built-in bypass — a fixed address that cannot
// rebind — and the route layer applies the PAGE-SCOPED loopback grant to them.
// The blocked-range set mirrors Rust `web_fetch::is_blocked_ip`; keep in sync.
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
import http from 'node:http';

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
let egressProxy = null; // the single connect-time network authority
const pages = new Map(); // pageId -> Page
let nextPageId = 1;
const consoleLog = new Map(); // pageId -> [{level,text}]
const dialogPolicy = new Map(); // pageId -> {action, promptText}
const loopbackGrant = new Set(); // pageId — pages whose live top-level origin is loopback
let lastBlock = null; // {url, reason, at} — surfaced on the action that caused it

// ── IP-range classifier (mirrors Rust web_fetch::is_blocked_ip) ───────────────
// Loopback is NOT "blocked" here; it is a separate allowed-but-page-scoped case.
function isLoopbackIp(ip) {
  if (net.isIPv4(ip)) return ip.split('.')[0] === '127';
  if (net.isIPv6(ip)) return ip.toLowerCase() === '::1';
  return false;
}

function v4Blocked(o) {
  const [a, b] = o;
  if (a === 127) return false; // loopback: classified separately
  if (a === 0) return true; // 0.0.0.0/8 incl unspecified
  if (a === 10) return true; // 10/8 private
  if (a === 172 && (b & 0xf0) === 16) return true; // 172.16/12 private
  if (a === 192 && b === 168) return true; // 192.168/16 private
  if (a === 169 && b === 254) return true; // 169.254/16 link-local (incl metadata)
  if (a === 100 && (b & 0xc0) === 64) return true; // 100.64/10 CGNAT
  if (a >= 224) return true; // multicast + reserved + broadcast
  return false;
}

function v6Blocked(ip) {
  const low = ip.toLowerCase();
  if (low === '::1') return false; // loopback: classified separately
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

function isLoopbackLiteralHost(h) {
  return (
    h === 'localhost' ||
    h === '::1' ||
    (net.isIPv4(h) && h.split('.')[0] === '127')
  );
}

function urlHostIsLoopbackLiteral(url) {
  try {
    return isLoopbackLiteralHost(new URL(url).hostname);
  } catch {
    return false;
  }
}

// ── page-scoped loopback grant (the only per-page network decision) ───────────
function pageGrantedById(page) {
  for (const [k, v] of pages) if (v === page && loopbackGrant.has(k)) return true;
  return false;
}

// Whether loopback is permitted for a request from `page`: the page is currently
// a loopback dev page (its live top-level origin is loopback; `loopbackGrant` is
// kept in lockstep with that origin), or the request comes from a popup whose
// opener is a live loopback dev page. Evaluated at request time — never depends
// on event ordering. `null` (a popup's frame-less initial navigation that cannot
// be attributed) is refused.
async function loopbackAllowedForPage(page) {
  if (!page) return false;
  try {
    if (urlHostIsLoopbackLiteral(page.mainFrame().url())) return true;
  } catch {
    /* frame gone */
  }
  if (pageGrantedById(page)) return true;
  try {
    let op = await page.opener();
    let depth = 0;
    while (op && depth < 5) {
      try {
        if (urlHostIsLoopbackLiteral(op.mainFrame().url())) return true;
      } catch {
        /* opener frame gone */
      }
      if (pageGrantedById(op)) return true;
      op = await op.opener();
      depth += 1;
    }
  } catch {
    /* opener detached */
  }
  return false;
}

function pageOf(req) {
  try {
    const frame = req.frame();
    return frame ? frame.page() : null;
  } catch {
    return null; // a popup's frame-less initial navigation
  }
}

// ── the egress proxy: the single connect-time authority ───────────────────────
function recordBlock(target, reason) {
  lastBlock = { url: target, reason, at: Date.now() };
  event({ kind: 'blocked_request', url: target, reason });
}

// Resolve a target host and refuse anything that resolves into a blocked range or
// rebinds to loopback; return the validated IP to connect to (the pin). Loopback
// literals are allowed here (the route layer already applied the page grant to
// them); a NON-loopback name that resolves to loopback is a rebind → refused.
// Throws (fail closed) on any resolution/validation failure.
async function resolveAndPin(host) {
  if (net.isIP(host)) {
    if (isLoopbackIp(host)) return host; // loopback literal (route page-scoped)
    if (addrBlocked(host)) throw new Error(`blocked address ${host}`);
    return host;
  }
  if (isLoopbackLiteralHost(host)) return '127.0.0.1'; // "localhost" (route page-scoped)
  let addrs;
  try {
    addrs = await dns.lookup(host, { all: true });
  } catch {
    throw new Error(`cannot resolve ${host}`);
  }
  for (const a of addrs) {
    if (isLoopbackIp(a.address)) throw new Error(`${host} resolves to loopback ${a.address}`);
    if (addrBlocked(a.address)) throw new Error(`${host} resolves to blocked ${a.address}`);
  }
  return addrs[0].address;
}

// Proxied http:// request: resolve+validate+pin the origin, then forward. https
// never arrives here (it tunnels via CONNECT). A refusal destroys the socket so
// the navigation/subresource FAILS (surfaced as Denied/blocked), never a silent
// success and never a connection to the target.
async function handleProxyHttp(creq, cres) {
  cres.on('error', () => {});
  creq.on('error', () => {});
  let u;
  try {
    u = new URL(creq.url);
  } catch {
    try {
      cres.destroy();
    } catch {
      /* ignore */
    }
    return;
  }
  let ip;
  try {
    ip = await resolveAndPin(u.hostname);
  } catch (e) {
    recordBlock(creq.url, e && e.message ? e.message : String(e));
    try {
      cres.destroy();
    } catch {
      /* ignore */
    }
    return;
  }
  const headers = { ...creq.headers };
  delete headers['proxy-connection'];
  const preq = http.request(
    {
      host: ip,
      port: u.port || 80,
      method: creq.method,
      path: u.pathname + u.search,
      headers,
    },
    (pres) => {
      try {
        cres.writeHead(pres.statusCode, pres.headers);
      } catch {
        /* client gone */
      }
      pres.on('error', () => {});
      pres.pipe(cres);
    }
  );
  preq.on('error', () => {
    try {
      cres.destroy();
    } catch {
      /* ignore */
    }
  });
  creq.pipe(preq);
}

// CONNECT (https://, wss://): resolve+validate+pin, then blind-tunnel so Chromium
// does TLS + any WebSocket handshake end-to-end (native origin/cert semantics).
async function handleProxyConnect(creq, csock, head) {
  csock.on('error', () => {});
  const sep = creq.url.lastIndexOf(':');
  const host = sep >= 0 ? creq.url.slice(0, sep) : creq.url;
  const port = parseInt(sep >= 0 ? creq.url.slice(sep + 1) : '443', 10) || 443;
  let ip;
  try {
    ip = await resolveAndPin(host);
  } catch (e) {
    recordBlock(creq.url, e && e.message ? e.message : String(e));
    try {
      csock.destroy();
    } catch {
      /* ignore */
    }
    return;
  }
  const up = net.connect(port, ip, () => {
    try {
      csock.write('HTTP/1.1 200 Connection Established\r\n\r\n');
    } catch {
      /* ignore */
    }
    if (head && head.length) up.write(head);
    up.pipe(csock);
    csock.pipe(up);
  });
  up.on('error', () => {
    try {
      csock.destroy();
    } catch {
      /* ignore */
    }
  });
}

// Plaintext ws:// through the proxy is NOT relayed (Node closes unhandled
// 'upgrade' connections) → public ws:// fails closed. wss:// tunnels via CONNECT
// and is pinned; loopback ws is handled by the route layer below.
async function startEgressProxy() {
  const server = http.createServer();
  server.on('request', handleProxyHttp);
  server.on('connect', handleProxyConnect);
  server.on('clientError', (_e, sock) => {
    try {
      sock.destroy();
    } catch {
      /* ignore */
    }
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  server.port = server.address().port;
  return server;
}

// ── route layer: the ONLY per-page decision (the loopback grant) ──────────────
// Loopback literals are the only non-proxied path (a fixed address that cannot
// rebind); everything else proceeds to the single proxy authority, which
// resolves/validates/pins. A literal private/link-local/metadata backstop guards
// against Chrome bypassing those directly.
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
    const host = u.hostname;
    if (isLoopbackLiteralHost(host)) {
      const page = pageOf(req); // null for a frame-less popup nav → deny (unattributable)
      const allowed = page ? await loopbackAllowedForPage(page) : false;
      if (!allowed) {
        return recordBlockAndAbort(route, u.href, `loopback ${host} without an explicit dev grant`);
      }
      return safeContinue(route);
    }
    if (net.isIP(host) && !isLoopbackIp(host) && addrBlocked(host)) {
      return recordBlockAndAbort(route, u.href, `blocked address ${host}`);
    }
    return safeContinue(route); // → the proxy resolves/validates/pins
  });
}

// Context-wide WebSocket boundary, installed BEFORE any page can create a socket.
// Loopback ws is refused (no page handle at the context level, and dev/HMR ws to
// localhost is out of V1 scope); every other ws goes through the pinning proxy
// (wss via CONNECT) or fails closed (public ws://).
async function installWsGate(ctx) {
  await ctx.routeWebSocket('**/*', (ws) => {
    let host = '';
    try {
      host = new URL(ws.url()).hostname;
    } catch {
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      return;
    }
    if (isLoopbackLiteralHost(host)) {
      recordBlock(ws.url(), 'loopback WebSocket is not permitted in V1');
      try {
        ws.close();
      } catch {
        /* ignore */
      }
      return;
    }
    try {
      ws.connectToServer();
    } catch {
      /* ignore */
    }
  });
}

function recordBlockAndAbort(route, url, reason) {
  recordBlock(url, reason);
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

// If an action failed because the gate refused a request within the grace window,
// turn the opaque net error into a typed `denied` — never a silent success.
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
// action (e.g. a link click whose navigation was refused). Consumes the record.
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
  // Idempotent: the context 'page' event and an action path can both reach a
  // freshly opened popup; never register the same Page under two ids.
  for (const [k, v] of pages) if (v === page) return k;
  const id = 'page-' + nextPageId++;
  pages.set(id, page);
  consoleLog.set(id, []);
  dialogPolicy.set(id, { action: 'dismiss' });
  // Keep the loopback grant in lockstep with the page's LIVE top-level origin: a
  // page that navigates (by click/JS/redirect, not just the navigate() method) to
  // a public origin must lose loopback access; one that lands on loopback gains
  // it. This closes the stale-grant hole without depending on navigate().
  page.on('framenavigated', (frame) => {
    if (frame !== page.mainFrame()) return;
    if (urlHostIsLoopbackLiteral(frame.url())) loopbackGrant.add(id);
    else loopbackGrant.delete(id);
  });

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
    loopbackGrant.delete(id);
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
    // Start the single egress authority FIRST. If it cannot bind, the launch
    // fails closed — there is no browser without the network boundary (B-1).
    egressProxy = await startEgressProxy();
    const opts = { headless: p.headless !== false };
    if (p.channel) opts.channel = p.channel; // e.g. "chrome"
    if (p.executablePath) opts.executablePath = p.executablePath;
    // Service workers off: request interception is the security boundary, and a
    // SW-mediated fetch must not be able to route around it.
    opts.serviceWorkers = 'block';
    // Route ALL non-loopback egress through our proxy (the sole resolver/
    // connector). Loopback literals keep Chrome's built-in bypass — a fixed
    // address that cannot rebind, page-scoped by the route layer. Nothing
    // inherits an ambient system proxy.
    opts.proxy = { server: `http://127.0.0.1:${egressProxy.port}` };
    context = await chromium.launchPersistentContext(p.userDataDir, opts);
    // Install the security boundary BEFORE any page can run. A setup failure
    // throws → the launch fails closed (no best-effort).
    await installNetworkGate(context);
    await installWsGate(context);
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
    // An EXPLICIT navigation to a loopback dev URL grants this page loopback
    // access; any other explicit target revokes it. Set before goto so the
    // document request is gated with the right grant. `framenavigated` keeps it
    // in sync afterwards for in-page navigations.
    if (urlHostIsLoopbackLiteral(p.url)) loopbackGrant.add(p.pageId);
    else loopbackGrant.delete(p.pageId);
    let resp;
    try {
      resp = await page.goto(p.url, { timeout: p.timeoutMs || 30000, waitUntil: 'domcontentloaded' });
    } catch (e) {
      throw blockedOr(e); // a gate-refused navigation (incl. redirect) → denied
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
    try {
      if (egressProxy) egressProxy.close();
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
  // Proxy client sockets can reset/EPIPE as Chromium tears connections down; that
  // is normal relay noise, not a driver defect.
  if (e && (e.code === 'EPIPE' || e.code === 'ECONNRESET')) return;
  process.stderr.write('driver: uncaught: ' + (e && e.stack ? e.stack : e) + '\n');
});
