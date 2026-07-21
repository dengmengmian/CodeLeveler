// CodeLeveler WebUI mock 服务端（开发演示用，非生产代码）。
//
// 实现前后端契约的全部端点：
//   GET  /api/health                     → {"ok":true}
//   POST /api/sessions                   → SessionBootstrap
//   GET  /api/sessions/:id/snapshot      → UiSessionSnapshot
//   GET  / 及任意非 /api、/ws 路径        → dist/ 静态资源（SPA fallback 到 index.html）
//   WS   /ws?session=<id>&token=<token>  → 全局事件流 + snapshot 帧
//
// 认证：?token= 或 Authorization: Bearer，常数时间比较，失败 401。
// token 启动时生成（256-bit hex）并在终端打印一次完整 URL；MOCK_TOKEN 可固定。
//
// 行为：内置两个 demo 会话；收到 submit_message 后模拟一轮完整 turn
// （assistant 流式输出 → 3 个工具调用，其中 run_command 触发审批等待
// ApprovalDecision → plan/verify/diff/checkpoint → turn_completed）。

import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { existsSync, readFileSync, statSync } from 'node:fs';
import { extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
import { WebSocketServer } from 'ws';

const PORT = Number(process.env.MOCK_PORT ?? 7331);
const HOST = '127.0.0.1';
const TOKEN = process.env.MOCK_TOKEN ?? randomBytes(32).toString('hex');
const REPO = '~/Develop/app/codeleveler';
const CONTEXT_WINDOW = 131_072;
const HERE = fileURLToPath(new URL('.', import.meta.url));
const DIST = join(HERE, '..', 'dist');

const MODELS = [
  { provider: 'moonshot', model: 'k2-thinking' },
  { provider: 'moonshot', model: 'k2' },
  { provider: 'openai', model: 'gpt-5-codex' },
];

// ── 工具 ────────────────────────────────────────────────────────────

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const uuid = () => crypto.randomUUID();
const nowIso = () => new Date().toISOString();

function tokenOk(presented) {
  if (!presented) return false;
  const a = createHash('sha256').update(String(presented)).digest();
  const b = createHash('sha256').update(TOKEN).digest();
  return timingSafeEqual(a, b);
}

function authOf(req, url) {
  const bearer = req.headers.authorization;
  if (bearer?.startsWith('Bearer ')) return bearer.slice(7);
  return url.searchParams.get('token');
}

function sendJson(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8' });
  res.end(payload);
}

// ── 会话存储 ────────────────────────────────────────────────────────

/** @type {Map<string, object>} */
const sessions = new Map();
/** sessionId → { cancelled: boolean, pendingApproval: null | { requestId, toolId, resolve } } */
const runners = new Map();

function makeSnapshot(id, goal, overrides = {}) {
  return {
    id,
    repository: REPO,
    goal,
    model: MODELS[0],
    mode: 'assisted',
    branch: 'main',
    status: 'idle',
    messages: [],
    pending_interactions: [],
    available_models: MODELS,
    vision: false,
    last_sequence: null,
    active_tools: [],
    plan: null,
    verification: null,
    diff: null,
    checkpoints: [],
    completion_report: null,
    ...overrides,
  };
}

function summaryOf(s) {
  return {
    id: s.id,
    goal: s.goal,
    status: s.status,
    model: s.model ? `${s.model.provider}/${s.model.model}` : '',
    updated_at: s.updated_at,
  };
}

function seed() {
  const s1 = makeSnapshot('sess-demo-webui', '给 leveler 加 WebUI', {
    status: 'completed',
    messages: [
      { id: 'm-1', role: 'user', text: '给 leveler-web 的 ws handler 加上 Lagged 重同步逻辑，参考 local-transport 的做法。' },
      {
        id: 'm-2',
        role: 'assistant',
        text: 'Lagged 分支已经接好：订阅任务检测到 `RecvError::Lagged` 时先给客户端推一帧 `resync_required`，再主动关闭 WS。浏览器重连后走 snapshot 整量重同步，和 TUI 的语义完全一致。',
      },
    ],
    checkpoints: [{ id: 'chk-demo-1', label: 'ws handler 骨架完成', ordinal: 2 }],
    updated_at: new Date(Date.now() - 2 * 60_000).toISOString(),
  });
  const s2 = makeSnapshot('sess-demo-migration', '修复 storage 迁移 0014 索引', {
    status: 'waiting',
    updated_at: new Date(Date.now() - 62 * 60_000).toISOString(),
  });
  const s3 = makeSnapshot('sess-demo-evals', 'evals smoke 提速', {
    status: 'completed',
    updated_at: new Date(Date.now() - 3 * 86_400_000).toISOString(),
  });
  for (const s of [s1, s2, s3]) sessions.set(s.id, s);
}

// ── 广播 ────────────────────────────────────────────────────────────

/** @type {Set<import('ws').WebSocket>} */
const clients = new Set();

function broadcast(frame) {
  const data = JSON.stringify(frame);
  for (const ws of clients) {
    if (ws.readyState === ws.OPEN) ws.send(data);
  }
}

const emit = (event) => broadcast({ type: 'event', event });

function touch(s) {
  s.updated_at = nowIso();
}

function broadcastSessionList() {
  emit({ type: 'session_list', sessions: [...sessions.values()].map(summaryOf) });
}

// ── demo turn 剧本 ──────────────────────────────────────────────────

const ASSISTANT_PART_1 = `看一下现有 transport 层怎么处理 broadcast 滞后，然后照搬到 web 网关。

先读 \`crates/leveler-local-transport/src/lib.rs\`，再全仓搜 \`RecvError::Lagged\` 的处理点：`;

const ASSISTANT_PART_2 = `Lagged 分支已经接好：订阅任务检测到 \`RecvError::Lagged\` 时先给客户端推一帧 \`resync_required\`，再主动关闭 WS。浏览器重连后走 snapshot 整量重同步，和 TUI 的语义完全一致。

\`\`\`rust
RecvError::Lagged(_) => {
    send(&mut ws, DownMsg::ResyncRequired { session_id }).await?;
    ws.close().await?;
    break;
}
\`\`\`

| 步骤 | 状态 |
| --- | --- |
| WS 桥接 | ✅ |
| Lagged 重同步 | ✅ |
| 退避重连 | 进行中 |

测试还在跑，通过后我会把 \`subscription_loop\` 的重连退避也对齐到 200ms 起步。`;

async function streamText(messageId, text, runner, chunkMs = 24) {
  for (let i = 0; i < text.length; i += 3) {
    if (runner.cancelled) return false;
    emit({ type: 'assistant_text_delta', message_id: messageId, delta: text.slice(i, i + 3) });
    await sleep(chunkMs);
  }
  return true;
}

async function runDemoTurn(sessionId) {
  const s = sessions.get(sessionId);
  if (!s) return;
  const runner = { cancelled: false, pendingApproval: null };
  runners.set(sessionId, runner);
  s.status = 'running';
  touch(s);
  broadcastSessionList();

  const cancelled = async () => {
    if (!runner.cancelled) return false;
    emit({ type: 'turn_cancelled' });
    s.status = 'idle';
    touch(s);
    broadcastSessionList();
    runners.delete(sessionId);
    return true;
  };

  // 1) 第一段助手消息（流式）
  const m1 = `m-${uuid()}`;
  s.active_tools = [];
  emit({ type: 'assistant_message_started', message_id: m1 });
  emit({ type: 'token_usage', input_tokens: 18_400, output_tokens: 320, cached_input_tokens: 4_100 });
  if (!(await streamText(m1, ASSISTANT_PART_1, runner))) return void (await cancelled());
  emit({ type: 'assistant_message_completed', message_id: m1 });
  s.messages.push({ id: m1, role: 'assistant', text: ASSISTANT_PART_1 });
  if (await cancelled()) return;

  // 2) 工具 1：read（成功）
  const t1 = `tc-${uuid()}`;
  const started1 = Date.now();
  s.active_tools = [{ id: t1, name: 'read_file', arguments: '{"path":"crates/leveler-local-transport/src/lib.rs"}' }];
  emit({ type: 'tool_call_started', id: t1, name: 'read_file', arguments: '{"path":"crates/leveler-local-transport/src/lib.rs"}', parallel: false });
  await sleep(600);
  if (await cancelled()) return;
  s.active_tools = [];
  emit({ type: 'tool_call_completed', id: t1, ok: true, preview: '// Trusted local transport between CodeLeveler UI clients and the runtime.\n#![forbid(unsafe_code)]\n…（共 812 行）', duration_ms: Date.now() - started1 });

  // 3) 工具 2：grep（成功，preview 3 hits）
  const t2 = `tc-${uuid()}`;
  const started2 = Date.now();
  s.active_tools = [{ id: t2, name: 'grep', arguments: '{"pattern":"RecvError::Lagged","type":"rust"}' }];
  emit({ type: 'tool_call_started', id: t2, name: 'grep', arguments: '{"pattern":"RecvError::Lagged","type":"rust"}', parallel: false });
  await sleep(500);
  if (await cancelled()) return;
  s.active_tools = [];
  emit({
    type: 'tool_call_completed', id: t2, ok: true,
    preview: 'crates/leveler-local-transport/src/lib.rs:270\ncrates/leveler-local-transport/src/lib.rs:638\ncrates/leveler-tui/src/run.rs:272',
    duration_ms: Date.now() - started2,
  });

  // 4) 工具 3：run_command → 触发审批，等待 ApprovalDecision
  const t3 = `tc-${uuid()}`;
  const started3 = Date.now();
  s.active_tools = [{ id: t3, name: 'run_command', arguments: '{"command":"cargo clippy --workspace --all-targets --fix"}' }];
  emit({ type: 'tool_call_started', id: t3, name: 'run_command', arguments: '{"command":"cargo clippy --workspace --all-targets --fix"}', parallel: false });
  await sleep(700);
  if (await cancelled()) return;

  const requestId = `ap-${uuid()}`;
  const approval = {
    id: requestId,
    tool: 'run_command',
    summary: '允许执行以下命令？该命令会修改工作区文件。',
    command: 'cargo clippy --workspace --all-targets --fix',
    risks: ['修改工作区文件（--fix）', '进程执行'],
  };
  s.pending_interactions = [{ type: 'approval', request: approval }];
  emit({ type: 'approval_requested', request: approval });

  const decision = await new Promise((resolve) => {
    runner.pendingApproval = { requestId, toolId: t3, resolve };
  });
  s.pending_interactions = [];
  runner.pendingApproval = null;
  if (await cancelled()) return;

  const approved = decision !== 'deny';
  s.active_tools = [];
  if (approved) await sleep(1_400);
  emit({
    type: 'tool_call_completed', id: t3, ok: approved,
    preview: approved ? 'warning: 3 warnings emitted after --fix' : 'permission denied by user',
    duration_ms: Date.now() - started3,
  });

  // 5) plan / verify / diff / checkpoint / token 用量
  s.plan = {
    steps: [
      { index: 0, description: '探索 client-protocol 契约', status: 'done' },
      { index: 1, description: '搭建 leveler-web 骨架', status: 'done' },
      { index: 2, description: 'WS 桥接 + Lagged 重同步', status: 'running' },
      { index: 3, description: 'REST: sessions / snapshot', status: 'pending' },
      { index: 4, description: '嵌入前端 + CLI 接入', status: 'pending' },
    ],
  };
  emit({ type: 'plan_updated', plan: s.plan });
  await sleep(400);
  s.verification = {
    checks: [
      { name: 'cargo check --workspace', status: 'passed', evidence: null },
      { name: 'cargo test -p leveler-web', status: 'passed', evidence: null },
      { name: 'cargo clippy --all-targets', status: approved ? 'running' : 'skipped', evidence: null },
    ],
    passed: null,
  };
  emit({ type: 'verification_updated', verification: s.verification });
  await sleep(500);
  s.diff = {
    files: [
      { path: 'crates/leveler-web/src/ws.rs', added: 46, removed: 4, patch: '@@ -266,6 +266,12 @@\n-    // TODO: lag 处理\n+    RecvError::Lagged(_) => {\n+        send(&mut ws, DownMsg::ResyncRequired { session_id }).await?;\n+        ws.close().await?;\n+        break;\n+    }' },
      { path: 'crates/leveler-web/src/server.rs', added: 12, removed: 1, patch: null },
    ],
  };
  emit({ type: 'diff_updated', diff: s.diff });
  const ckpt = { id: `chk-${uuid()}`, label: 'Lagged 重同步完成', ordinal: s.messages.length + 1 };
  s.checkpoints.push(ckpt);
  emit({ type: 'checkpoint_created', checkpoint: ckpt });
  emit({ type: 'token_usage', input_tokens: 79_600, output_tokens: 2_140, cached_input_tokens: 31_000 });
  if (await cancelled()) return;

  // 6) 第二段助手消息（流式，含代码块与表格）
  const m2 = `m-${uuid()}`;
  emit({ type: 'assistant_message_started', message_id: m2 });
  if (!(await streamText(m2, ASSISTANT_PART_2, runner))) return void (await cancelled());
  emit({ type: 'assistant_message_completed', message_id: m2 });
  s.messages.push({ id: m2, role: 'assistant', text: ASSISTANT_PART_2 });

  // 7) turn 完成
  s.verification = { ...s.verification, passed: approved };
  emit({ type: 'verification_updated', verification: s.verification });
  emit({ type: 'turn_completed' });
  s.status = 'completed';
  touch(s);
  broadcastSessionList();
  runners.delete(sessionId);
}

// ── 命令处理 ────────────────────────────────────────────────────────

function sendFrame(ws, frame) {
  if (ws.readyState === ws.OPEN) ws.send(JSON.stringify(frame));
}

async function handleDeliver(ws, frame) {
  const { command_id: commandId, command } = frame;
  sendFrame(ws, { type: 'ack', command_id: commandId });

  const sid = command.session_id ?? frame.session_id ?? null;
  const session = sid ? sessions.get(sid) : null;

  switch (command.type) {
    case 'submit_message':
    case 'run_goal': {
      if (!session) return sendFrame(ws, { type: 'error', code: 'session_not_found', message: `session not found: ${sid}`, command_id: commandId });
      if (runners.has(session.id)) return sendFrame(ws, { type: 'error', code: 'turn_active', message: '当前回合进行中，消息应排队', command_id: commandId });
      const msg = { id: `m-${uuid()}`, role: 'user', text: command.content };
      session.messages.push(msg);
      touch(session);
      emit({ type: 'user_message_added', message: msg });
      void runDemoTurn(session.id);
      return;
    }
    case 'cancel_current_turn': {
      const runner = session && runners.get(session.id);
      if (runner) runner.cancelled = true;
      return;
    }
    case 'force_cancel_current_turn':
      return;
    case 'approval_decision': {
      for (const runner of runners.values()) {
        if (runner.pendingApproval?.requestId === command.request_id) {
          runner.pendingApproval.resolve(command.decision);
          return;
        }
      }
      return sendFrame(ws, { type: 'error', code: 'not_pending', message: `没有等待中的审批：${command.request_id}`, command_id: commandId });
    }
    case 'answer_clarification':
      return;
    case 'select_model': {
      if (!session) return;
      session.model = command.model;
      touch(session);
      emit({ type: 'session_updated', session });
      return;
    }
    case 'set_permission_profile': {
      if (!session) return;
      session.mode = command.mode;
      touch(session);
      emit({ type: 'session_updated', session });
      return;
    }
    case 'set_agent_mode':
      emit({ type: 'notification', level: 'info', message: `agent 模式已切换：orchestrate=${command.orchestrate}` });
      return;
    case 'request_diff': {
      if (!session) return;
      emit({ type: 'diff_updated', diff: session.diff ?? { files: [] } });
      return;
    }
    case 'compact_context':
      emit({ type: 'notification', level: 'info', message: '上下文已压缩' });
      emit({ type: 'token_usage', input_tokens: 6_200, output_tokens: 0, cached_input_tokens: 0 });
      return;
    case 'clear_conversation': {
      if (!session) return;
      session.messages = [];
      touch(session);
      emit({ type: 'session_updated', session });
      return;
    }
    case 'request_session_list':
    case 'request_session_list_for':
      broadcastSessionList();
      return;
    case 'open_session':
    case 'open_session_for': {
      const target = sessions.get(command.session_id);
      if (target) sendFrame(ws, { type: 'snapshot', session: target });
      return;
    }
    case 'delete_session':
    case 'delete_session_for': {
      sessions.delete(command.session_id);
      broadcastSessionList();
      return;
    }
    case 'restore_checkpoint':
      emit({ type: 'notification', level: 'info', message: `已回滚到检查点 ${command.checkpoint_id}` });
      return;
    case 'list_memory':
      emit({ type: 'memory_list', memory_dir: '.leveler/memory', active: [], archived: [] });
      return;
    case 'btw': {
      emit({ type: 'btw_started', question: command.question });
      await sleep(400);
      emit({ type: 'btw_text_delta', delta: `这是对侧问「${command.question}」的回答：demo 模式下只回一句，不打断当前回合。` });
      await sleep(200);
      emit({ type: 'btw_completed' });
      return;
    }
    case 'quit':
      return;
    case 'set_product_axes':
    case 'confirm_plan_to_goal':
    case 'add_attachment':
    case 'add_clipboard_image':
    case 'forget_memory':
      // 协议内合法但 demo 未实现：ack 已回，给个提示事件而不是 error
      emit({ type: 'notification', level: 'info', message: `mock 未实现命令：${command.type}` });
      return;
    default:
      return sendFrame(ws, { type: 'error', code: 'unknown_command', message: `unknown command: ${command?.type ?? '(missing type)'}`, command_id: commandId });
  }
}

// ── HTTP ────────────────────────────────────────────────────────────

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.map': 'application/json',
  '.woff2': 'font/woff2',
};

function readBody(req) {
  return new Promise((resolve, reject) => {
    let data = '';
    req.on('data', (chunk) => {
      data += chunk;
      if (data.length > 1_000_000) reject(new Error('body too large'));
    });
    req.on('end', () => resolve(data));
    req.on('error', reject);
  });
}

function serveStatic(req, res, pathname) {
  if (!existsSync(join(DIST, 'index.html'))) {
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8' });
    res.end('leveler-web mock 已启动，但 web/dist 尚未构建。先运行 npm run build，或用 npm run dev 走 vite。\n');
    return;
  }
  let file = normalize(join(DIST, pathname));
  if (!file.startsWith(DIST) || !existsSync(file) || !statSync(file).isFile()) {
    file = join(DIST, 'index.html'); // SPA fallback
  }
  const body = readFileSync(file);
  res.writeHead(200, { 'content-type': MIME[extname(file)] ?? 'application/octet-stream' });
  res.end(body);
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url ?? '/', `http://${HOST}:${PORT}`);
  const pathname = url.pathname;

  if (pathname.startsWith('/api/')) {
    if (!tokenOk(authOf(req, url))) return sendJson(res, 401, { error: 'unauthorized' });

    if (req.method === 'GET' && pathname === '/api/health') {
      return sendJson(res, 200, { ok: true });
    }

    if (req.method === 'POST' && pathname === '/api/sessions') {
      let body;
      try {
        body = JSON.parse(await readBody(req));
      } catch {
        return sendJson(res, 400, { error: 'invalid json' });
      }
      if (typeof body?.goal !== 'string' || !body.goal.trim()) {
        return sendJson(res, 400, { error: 'goal is required' });
      }
      const id = `sess-${uuid()}`;
      const snapshot = makeSnapshot(id, body.goal.trim(), {
        model: body.model ?? MODELS[0],
        mode: body.mode ?? 'assisted',
        updated_at: nowIso(),
      });
      sessions.set(id, snapshot);
      emit({ type: 'session_opened', session: snapshot });
      broadcastSessionList();
      return sendJson(res, 200, { session: snapshot, context_window: CONTEXT_WINDOW });
    }

    const snapMatch = pathname.match(/^\/api\/sessions\/([^/]+)\/snapshot$/);
    if (req.method === 'GET' && snapMatch) {
      const session = sessions.get(decodeURIComponent(snapMatch[1]));
      if (!session) return sendJson(res, 404, { error: 'session not found' });
      return sendJson(res, 200, session);
    }

    return sendJson(res, 404, { error: 'not found' });
  }

  // 静态资源 / SPA fallback
  return serveStatic(req, res, pathname);
});

// ── WebSocket ───────────────────────────────────────────────────────

const wss = new WebSocketServer({ noServer: true });

server.on('upgrade', (req, socket, head) => {
  const url = new URL(req.url ?? '/', `http://${HOST}:${PORT}`);
  if (url.pathname !== '/ws' || !tokenOk(authOf(req, url))) {
    socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
    socket.destroy();
    return;
  }
  wss.handleUpgrade(req, socket, head, (ws) => {
    ws.levelerSession = url.searchParams.get('session');
    wss.emit('connection', ws, req);
  });
});

wss.on('connection', (ws) => {
  clients.add(ws);

  // 连接带了 session 参数：先主动推一帧该会话 snapshot
  if (ws.levelerSession) {
    const session = sessions.get(ws.levelerSession);
    if (session) sendFrame(ws, { type: 'snapshot', session });
  }

  ws.on('message', (data) => {
    let frame;
    try {
      frame = JSON.parse(String(data));
    } catch {
      return sendFrame(ws, { type: 'error', code: 'bad_json', message: '无法解析的帧', command_id: null });
    }
    switch (frame?.type) {
      case 'deliver':
        void handleDeliver(ws, frame);
        return;
      case 'snapshot': {
        const session = sessions.get(frame.session_id);
        if (session) sendFrame(ws, { type: 'snapshot', session });
        else sendFrame(ws, { type: 'error', code: 'session_not_found', message: `session not found: ${frame.session_id}`, command_id: null });
        return;
      }
      default:
        sendFrame(ws, { type: 'error', code: 'unknown_frame', message: `unknown frame type: ${frame?.type}`, command_id: null });
    }
  });

  ws.on('close', () => clients.delete(ws));
});

// ── 启动 ────────────────────────────────────────────────────────────

seed();
server.listen(PORT, HOST, () => {
  console.log('leveler-web mock 服务端已启动（仅绑 127.0.0.1）');
  console.log('');
  console.log(`  直连 mock:   http://${HOST}:${PORT}/?token=${TOKEN}`);
  console.log(`  vite dev:    http://127.0.0.1:5173/?token=${TOKEN}`);
  console.log('');
  console.log('提示：固定 token 可用 MOCK_TOKEN=<64-hex> npm run mock');
});
