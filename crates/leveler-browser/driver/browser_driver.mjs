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
import http from 'node:http';
import https from 'node:https';

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
// Pages the agent EXPLICITLY navigated to a loopback dev URL. Loopback is a
// page-scoped dev grant, never a global exception: a public page must not reach
// loopback via redirect/click/form/window.open/fetch/ws just because loopback
// happens to be locally reachable.
const loopbackGrant = new Set(); // pageId
// A popup's INITIAL document request is frame-less (Playwright cannot attribute
// it to a page yet), so a popup opened BY A GRANTED dev page briefly carries the
// grant here — set on the opener's `popup` event. A popup opened by a public
// page gets no such window, so it still cannot reach loopback.
let pendingPopupGrantUntil = 0;

// ── SSRF network boundary (B-1) ──────────────────────────────────────────────
// The browser is where a redirect / link click / form submit / window.open /
// fetch / WebSocket actually becomes a network request, so this is the only
// place the runtime's per-request IP policy can hold. Invariants:
//   * Loopback is a PAGE-SCOPED dev grant (set on an explicit loopback
//     navigation), never a global exception — a public page cannot reach
//     loopback via redirect/click/form/window.open/fetch/ws.
//   * Verification FAILS CLOSED: any error resolving/validating/fetching a
//     request aborts it — never a `continue` past an unverified target.
//   * Navigations are followed hop-by-hop and PINNED at connect (resolve →
//     validate → pin → connect, like `web_fetch`), so a name that rebinds
//     safe→private between check and connect never reaches the private endpoint.
//   * WebSocket egress is gated by the same host policy.
//   * Service workers are blocked (they would otherwise bypass route interception).
// The blocked-range set mirrors Rust `web_fetch::is_blocked_ip`; keep in sync.
let lastBlock = null; // {url, reason, at} — surfaced on the action that caused it

function isLoopbackIp(ip) {
  if (net.isIPv4(ip)) return ip.split('.')[0] === '127';
  if (net.isIPv6(ip)) return ip.toLowerCase() === '::1';
  return false;
}

function v4Blocked(o) {
  const [a, b] = o;
  if (a === 127) return false; // loopback: classified separately as its own kind
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

// Classify a request host into loopback / blocked / allowed, resolving hostnames
// once and returning the validated addresses so the caller can PIN to them. Any
// blocked resolved address makes the host blocked; loopback is its own kind so a
// page grant can gate it. Resolution failure fails closed (blocked).
async function classifyHost(host) {
  if (net.isIP(host)) {
    if (isLoopbackIp(host)) return { kind: 'loopback', addrs: [host] };
    if (addrBlocked(host)) return { kind: 'blocked', reason: `blocked address ${host}` };
    return { kind: 'allowed', addrs: [host] };
  }
  let addrs;
  try {
    addrs = await dns.lookup(host, { all: true });
  } catch {
    return { kind: 'blocked', reason: `cannot resolve ${host}` };
  }
  let anyLoop = false;
  for (const a of addrs) {
    if (isLoopbackIp(a.address)) anyLoop = true;
    else if (addrBlocked(a.address)) {
      return { kind: 'blocked', reason: `${host} resolves to blocked ${a.address}` };
    }
  }
  const list = addrs.map((a) => a.address);
  return anyLoop ? { kind: 'loopback', addrs: list } : { kind: 'allowed', addrs: list };
}

function urlHostIsLoopbackLiteral(url) {
  let h;
  try {
    h = new URL(url).hostname;
  } catch {
    return false;
  }
  return h === 'localhost' || h === '::1' || (net.isIPv4(h) && h.split('.')[0] === '127');
}

function pageGrantedById(page) {
  for (const [k, v] of pages) if (v === page && loopbackGrant.has(k)) return true;
  return false;
}

// Whether loopback is permitted for a request from `page`. True when the page is
// (currently) a loopback dev page — its live top-level origin is loopback, or
// `loopbackGrant` (kept in lockstep with that origin) holds it — or when the
// request comes from a popup whose OPENER is a live loopback dev page. Takes the
// Page object (not an id) so it also decides correctly for a popup whose initial
// document request fires before the context 'page' event has registered it.
// Evaluated at request time, so it never depends on event ordering.
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

// The Page a request belongs to (null if detached).
function pageOf(req) {
  try {
    const frame = req.frame();
    return frame ? frame.page() : null;
  } catch {
    return null;
  }
}

// One hop of a navigation, connected to the PINNED validated IP (Host header +
// TLS SNI keep the hostname) so Chromium cannot re-resolve to a rebound address.
function fetchPinned(u, pinnedIp, method, body, reqHeaders) {
  return new Promise((resolve, reject) => {
    const isHttps = u.protocol === 'https:';
    const mod = isHttps ? https : http;
    const headers = {};
    for (const [k, v] of Object.entries(reqHeaders || {})) {
      const lk = k.toLowerCase();
      if (lk === 'content-length' || lk === 'connection') continue;
      headers[k] = v;
    }
    headers['host'] = u.host;
    const opts = {
      method: method || 'GET',
      hostname: u.hostname,
      port: u.port || (isHttps ? 443 : 80),
      path: (u.pathname || '/') + (u.search || ''),
      headers,
      servername: isHttps ? u.hostname : undefined,
      lookup: (_h, _o, cb) => cb(null, pinnedIp, net.isIPv6(pinnedIp) ? 6 : 4),
      timeout: 30000,
    };
    const rq = mod.request(opts, (res) => {
      const chunks = [];
      res.on('data', (d) => chunks.push(d));
      res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, body: Buffer.concat(chunks) }));
      res.on('error', reject);
    });
    rq.on('error', reject);
    rq.on('timeout', () => rq.destroy(new Error('request timeout')));
    if (body && body.length) rq.write(body);
    rq.end();
  });
}

async function fulfillPinned(route, resp) {
  const headers = {};
  for (const [k, v] of Object.entries(resp.headers)) {
    const lk = k.toLowerCase();
    if (lk === 'content-length' || lk === 'transfer-encoding' || lk === 'connection' || lk === 'keep-alive') {
      continue;
    }
    headers[k] = Array.isArray(v) ? v[0] : v; // keep it single-valued for fulfill
  }
  try {
    await route.fulfill({ status: resp.status, headers, body: resp.body });
  } catch {
    /* route already resolved — our fetch was validated + pinned, so this is benign */
  }
}

// A navigation, followed hop-by-hop with each hop validated and connected via a
// pinned IP. Fails closed on any resolve/validate/fetch error.
async function pinnedNavigate(route, page) {
  const req0 = route.request();
  let current = req0.url();
  let method = req0.method();
  let body = null;
  try {
    body = req0.postDataBuffer ? req0.postDataBuffer() : null;
  } catch {
    body = null;
  }
  const baseHeaders = req0.headers();
  try {
    for (let hop = 0; hop < 12; hop++) {
      let u;
      try {
        u = new URL(current);
      } catch {
        return recordBlockAndAbort(route, current, 'unparseable url');
      }
      if (u.protocol !== 'http:' && u.protocol !== 'https:') return safeContinue(route);
      const c = await classifyHost(u.hostname);
      if (c.kind === 'blocked') return recordBlockAndAbort(route, current, c.reason);
      if (c.kind === 'loopback') {
        // `page` is null for a popup's frame-less initial navigation; a popup
        // opened by a granted dev page carries a short pending grant instead.
        const allowed = page
          ? await loopbackAllowedForPage(page)
          : Date.now() < pendingPopupGrantUntil;
        if (!allowed) {
          return recordBlockAndAbort(route, current, `loopback ${u.hostname} without an explicit dev grant`);
        }
      }
      let resp;
      try {
        resp = await fetchPinned(u, c.addrs[0], method, body, baseHeaders);
      } catch (e) {
        // FAIL CLOSED: a validated target we could not fetch is refused, never continued.
        return recordBlockAndAbort(route, current, `verification fetch failed: ${e && e.message ? e.message : e}`);
      }
      if (resp.status >= 300 && resp.status < 400 && resp.headers.location) {
        let next;
        try {
          next = new URL(resp.headers.location, current);
        } catch {
          return fulfillPinned(route, resp);
        }
        current = next.href;
        if (resp.status !== 307 && resp.status !== 308) {
          method = 'GET';
          body = null;
        }
        continue;
      }
      return fulfillPinned(route, resp);
    }
    return recordBlockAndAbort(route, current, 'too many redirects');
  } catch (e) {
    // FAIL CLOSED on any unexpected gate error.
    return recordBlockAndAbort(route, current, `gate error: ${e && e.message ? e.message : e}`);
  }
}

// A subresource (script/img/xhr/fetch/…): host-classified at request time. Blocked
// ranges and ungranted loopback are refused. Loopback subresources are served via
// the same pinned fetch as navigations (dev assets are local and small, and this
// pins them against rebinding too); public subresources proceed normally (so
// streaming/range/large responses are not buffered).
async function gateSubresource(route, u, page) {
  let c;
  try {
    c = await classifyHost(u.hostname);
  } catch (e) {
    return recordBlockAndAbort(route, u.href, `classify error: ${e && e.message ? e.message : e}`);
  }
  if (c.kind === 'blocked') return recordBlockAndAbort(route, u.href, c.reason);
  if (c.kind === 'loopback') {
    if (!(await loopbackAllowedForPage(page))) {
      return recordBlockAndAbort(route, u.href, `loopback ${u.hostname} without an explicit dev grant`);
    }
    const req = route.request();
    let body = null;
    try {
      body = req.postDataBuffer ? req.postDataBuffer() : null;
    } catch {
      body = null;
    }
    let resp;
    try {
      resp = await fetchPinned(u, c.addrs[0], req.method(), body, req.headers());
    } catch (e) {
      return recordBlockAndAbort(route, u.href, `verification fetch failed: ${e && e.message ? e.message : e}`);
    }
    return fulfillPinned(route, resp);
  }
  return safeContinue(route);
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
    // Non-network schemes (file:/data:/about:/blob:/chrome:) never egress to an IP.
    if (u.protocol !== 'http:' && u.protocol !== 'https:') return safeContinue(route);
    const page = pageOf(req);
    if (req.resourceType() === 'document') return pinnedNavigate(route, page);
    return gateSubresource(route, u, page);
  });
}

// WebSocket egress is a separate Playwright boundary; gate it with the same host
// policy (private/link-local/metadata denied; loopback only with a page grant).
async function installWsGate(page) {
  try {
    await page.routeWebSocket('**/*', async (ws) => {
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
      let c;
      try {
        c = await classifyHost(host);
      } catch {
        c = { kind: 'blocked', reason: `ws classify failed for ${host}` };
      }
      const blocked =
        c.kind === 'blocked' || (c.kind === 'loopback' && !(await loopbackAllowedForPage(page)));
      if (blocked) {
        const reason = c.reason || `loopback ws ${host} without a dev grant`;
        lastBlock = { url: ws.url(), reason, at: Date.now() };
        event({ kind: 'blocked_request', url: ws.url(), reason });
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
  } catch {
    /* routeWebSocket unavailable — best effort */
  }
}

async function urlIsLoopback(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
  const c = await classifyHost(u.hostname);
  return c.kind === 'loopback';
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
  // Idempotent: the context 'page' event and an action path can both reach a
  // freshly opened popup; never register the same Page under two ids.
  for (const [k, v] of pages) if (v === page) return k;
  const id = 'page-' + nextPageId++;
  pages.set(id, page);
  consoleLog.set(id, []);
  dialogPolicy.set(id, { action: 'dismiss' });
  // Gate this page's WebSocket egress with the same host policy (B-1).
  installWsGate(page);
  // Keep the loopback grant in lockstep with the page's LIVE top-level origin:
  // a page that navigates (by click/JS/redirect, not just the navigate() method)
  // to a public origin must lose loopback access; one that lands on loopback
  // gains it. This closes the stale-grant hole without depending on navigate().
  page.on('framenavigated', (frame) => {
    if (frame !== page.mainFrame()) return;
    if (urlHostIsLoopbackLiteral(frame.url())) loopbackGrant.add(id);
    else loopbackGrant.delete(id);
  });
  // When a GRANTED dev page opens a popup, briefly authorize that popup's
  // frame-less initial loopback navigation (a public page's popup gets nothing).
  page.on('popup', () => {
    if (loopbackGrant.has(id)) pendingPopupGrantUntil = Date.now() + 4000;
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
    const opts = { headless: p.headless !== false };
    if (p.channel) opts.channel = p.channel; // e.g. "chrome"
    if (p.executablePath) opts.executablePath = p.executablePath;
    // Block service workers: because request interception IS the security
    // boundary here, a SW-mediated fetch must not be able to route around it (B-1).
    opts.serviceWorkers = 'block';
    // Never inherit ambient proxy/user state beyond the given user-data-dir.
    context = await chromium.launchPersistentContext(p.userDataDir, opts);
    // B-1: enforce the SSRF network boundary on every request BEFORE any page
    // can navigate — covers redirects, clicks, window.open, form submits, fetch.
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
    // An EXPLICIT navigation to a loopback dev URL grants this page loopback
    // access; any other explicit target revokes it. Set before goto so the
    // document request (and its subresources) are gated with the right grant —
    // and so a public page can never reach loopback via redirect/click/fetch.
    if (await urlIsLoopback(p.url)) loopbackGrant.add(p.pageId);
    else loopbackGrant.delete(p.pageId);
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
    // If THIS page is a loopback dev page and the click may open a popup, arm the
    // pending-popup grant BEFORE the click — synchronously, in the same driver
    // call — so the popup's frame-less initial loopback navigation is authorized
    // without racing the opener's async `popup` event. A public page never
    // reaches here with a grant, so its popup gets nothing.
    if (loopbackGrant.has(p.pageId)) pendingPopupGrantUntil = Date.now() + 4000;
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
