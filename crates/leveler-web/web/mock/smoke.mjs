// 冒烟脚本：走完契约全路径。用法：node mock/smoke.mjs <token> [sessionId]
import WebSocket from 'ws';

const TOKEN = process.argv[2] ?? 'deadbeefcafe1234';
const TARGET_SESSION = process.argv[3] ?? 'sess-demo-webui';
const url = `ws://127.0.0.1:7331/ws?session=${TARGET_SESSION}&token=${TOKEN}`;
const ws = new WebSocket(url);

let step = 0;
let approved = false;
let turnDone = false;
const failures = [];
const ok = (name) => console.log(`  ✓ ${name}`);
const fail = (name, extra) => {
  failures.push(name);
  console.log(`  ✗ ${name}`, extra ?? '');
};

const send = (frame) => ws.send(JSON.stringify(frame));
const deliver = (command, commandId = crypto.randomUUID()) =>
  send({ type: 'deliver', command_id: commandId, session_id: command.session_id ?? TARGET_SESSION, command });

ws.on('open', () => {
  console.log('WS connected');
});

ws.on('message', (data) => {
  const frame = JSON.parse(String(data));
  if (frame.type === 'snapshot') {
    ok(`snapshot 帧（session=${frame.session.id}, messages=${frame.session.messages.length}）`);
    if (step === 0) {
      step = 1;
      deliver({ type: 'request_session_list' });
    }
    return;
  }
  if (frame.type === 'ack') return;
  if (frame.type === 'error') {
    if (frame.code === 'unknown_command') ok('未知命令回 error 帧');
    else fail('意外 error 帧', frame);
    return;
  }
  if (frame.type !== 'event') return fail('未知帧类型', frame.type);

  const ev = frame.event;
  switch (ev.type) {
    case 'session_list':
      ok(`session_list（${ev.sessions.length} 个会话）`);
      if (step === 1) {
        step = 2;
        deliver({ type: 'submit_message', session_id: TARGET_SESSION, content: '帮我看一下 ws handler 的 Lagged 处理' });
      }
      return;
    case 'user_message_added':
      ok('user_message_added');
      return;
    case 'assistant_message_started':
      process.stdout.write('  …assistant 流式: ');
      return;
    case 'assistant_text_delta':
      process.stdout.write(ev.delta);
      return;
    case 'assistant_message_completed':
      process.stdout.write('\n');
      ok('assistant_message_completed');
      return;
    case 'tool_call_started':
      console.log(`  …tool started: ${ev.name}`);
      return;
    case 'tool_call_completed':
      ok(`tool_call_completed ok=${ev.ok} dur=${ev.duration_ms}ms`);
      return;
    case 'approval_requested':
      ok(`approval_requested（${ev.request.command}）→ 发送 approve_once`);
      deliver({ type: 'approval_decision', request_id: ev.request.id, decision: 'approve_once' }, `approval:${ev.request.id}`);
      approved = true;
      return;
    case 'plan_updated':
      ok(`plan_updated（${ev.plan.steps.length} 步）`);
      return;
    case 'verification_updated':
      ok(`verification_updated（checks=${ev.verification.checks.length}, passed=${ev.verification.passed}）`);
      return;
    case 'diff_updated':
      ok(`diff_updated（${ev.diff.files.length} 文件）`);
      return;
    case 'checkpoint_created':
      ok(`checkpoint_created（${ev.checkpoint.label}）`);
      return;
    case 'token_usage':
      ok(`token_usage（in=${ev.input_tokens} out=${ev.output_tokens}）`);
      return;
    case 'turn_completed':
      turnDone = true;
      ok('turn_completed');
      step = 3;
      // 未知命令 → error 帧；snapshot 上行 → snapshot 帧
      deliver({ type: 'set_product_axes', session_id: TARGET_SESSION, work_profile: 'balanced', collaboration: 'chat' });
      send({ type: 'snapshot', session_id: TARGET_SESSION });
      setTimeout(() => {
        ws.close();
        console.log('');
        if (!approved) fail('未收到 approval_requested');
        if (!turnDone) fail('未收到 turn_completed');
        console.log(failures.length === 0 ? 'SMOKE OK' : `SMOKE FAILED: ${failures.join(', ')}`);
        process.exit(failures.length === 0 ? 0 : 1);
      }, 800);
      return;
    default:
      return;
  }
});

ws.on('error', (err) => {
  fail('WS error', err.message);
  process.exit(1);
});

setTimeout(() => {
  fail('超时（60s 未完成 turn）');
  process.exit(1);
}, 60_000);
