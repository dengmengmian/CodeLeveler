// Presentation contract for scheduled automation in the Desktop surface.
// State words come from structured runtime fields only; text is never parsed.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  scheduleState,
  runState,
  triggerLabel,
  filterLabel,
  wakeupLabel,
  formatDateTime,
  formatCost,
  runUsage,
  actionSummary,
  attentionLabel,
  modeLabel,
  policyLabel,
  overlapLabel,
  misfireLabel,
  doctorCheckLabel,
  severityLabel,
} from '../src/schedules.mjs';

function view(over = {}) {
  return {
    id: 'a',
    name: 'job',
    description: '',
    action_kind: 'shell',
    action: { kind: 'shell', command: 'echo' },
    workspace: '/w',
    trigger: { kind: 'daily', time: '09:00', timezone: 'Asia/Shanghai' },
    trigger_text: 'Every day at 09:00',
    timezone: 'Asia/Shanghai',
    enabled: true,
    running: false,
    run_state: null,
    next_run_at: null,
    last_run: null,
    needs_attention: false,
    attention_reason: null,
    session_policy: 'continue',
    overlap: 'skip-if-running',
    mode: 'assisted',
    model: null,
    timeout_secs: 600,
    misfire: 'skip',
    misfire_grace_secs: 0,
    session_id: null,
    grant: {
      granted: true,
      external_side_effects: false,
      granted_at: null,
      requires_regrant: false,
    },
    created_at: 't',
    revision: 'r',
    ...over,
  };
}

function status(wakeup = {}) {
  return {
    definitions: 1,
    enabled: 1,
    paused: 0,
    running: 0,
    overdue_pending: 0,
    next_occurrence: null,
    wakeup: {
      backend: 'launchd',
      label: 'com.leveler.schedule',
      supported: true,
      registered: false,
      loaded: false,
      interval_secs: 60,
      ...wakeup,
    },
  };
}

test('a live run outranks every earlier result', () => {
  assert.equal(scheduleState(view({ running: true, run_state: 'failed' })).label, '正在执行');
  assert.equal(scheduleState(view({ running: true, enabled: false })).label, '正在执行');
});

test('a paused schedule never reads as merely scheduled', () => {
  assert.equal(scheduleState(view({ enabled: false, run_state: 'succeeded' })).label, '已暂停');
});

test('waiting for authorization is not running and unknown is not failed', () => {
  assert.equal(scheduleState(view({ run_state: 'waiting_approval' })).label, '需要授权');
  assert.equal(runState('waiting_approval').label, '需要授权');
  assert.equal(scheduleState(view({ run_state: 'unknown' })).label, '结果未确认');
  assert.equal(runState('unknown').label, '结果未确认');
  assert.notEqual(runState('unknown').tone, 'danger');
});

test('a missing grant needs attention under every permission profile', () => {
  const grant = {
    granted: false,
    external_side_effects: false,
    granted_at: null,
    requires_regrant: true,
  };
  for (const mode of ['assisted', 'full_access']) {
    assert.equal(scheduleState(view({ mode, run_state: 'succeeded', grant })).label, '需要授权');
  }
});

test('submitted is not done', () => {
  assert.equal(runState('submitted').label, '已提交');
});

test('triggers are described from the structured rule including the zone', () => {
  assert.equal(triggerLabel({ kind: 'daily', time: '09:00', timezone: 'Asia/Shanghai' }), '每天 09:00 (Asia/Shanghai)');
  assert.equal(triggerLabel({ kind: 'every', every_secs: 7200 }), '每隔 2 小时');
  assert.equal(triggerLabel({ kind: 'every', every_secs: 90 }), '每隔 90 秒');
  assert.equal(triggerLabel({ kind: 'weekly', time: '08:30', weekdays: 'mon,wed', timezone: null }), '每周 mon,wed 08:30');
  assert.equal(triggerLabel({ kind: 'cron', expression: '0 9 * * mon-fri', timezone: 'UTC' }), 'Cron 0 9 * * mon-fri (UTC)');
});

test('filters use the product vocabulary', () => {
  assert.equal(filterLabel('all'), '全部');
  assert.equal(filterLabel('needs_attention'), '需要处理');
  assert.equal(filterLabel('recent_failure'), '最近失败');
});

test('the wakeup chip only says registered when the runtime observed it', () => {
  assert.equal(wakeupLabel(null).label, '后台唤醒状态未知');
  assert.deepEqual(wakeupLabel(status({ supported: false, backend: 'unsupported' })), {
    label: '本平台不支持系统唤醒',
    tone: 'muted',
    actionable: false,
    enabled: false,
  });
  assert.equal(wakeupLabel(status()).label, '后台唤醒未启用');
  assert.equal(wakeupLabel(status()).tone, 'warn');
  assert.equal(wakeupLabel(status({ registered: true })).label, '系统唤醒已注册 · 每分钟检查一次');
  assert.equal(wakeupLabel(status({ registered: true })).enabled, true);
});

test('usage stays missing instead of being padded with zeros', () => {
  assert.equal(runUsage({ usage: null }), '—');
  assert.equal(
    runUsage({
      usage: {
        requests: 3,
        input_tokens: 1200,
        output_tokens: 340,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
        cost_usd_micros: 12345,
        rows_without_cost: 1,
      },
    }),
    '3 次请求 · 1,200 in / 340 out · $0.0123 · 1 条未计价',
  );
  assert.equal(formatCost(0), '$0');
});

test('date formatting never invents a value for a missing timestamp', () => {
  assert.equal(formatDateTime(null), '—');
  assert.equal(formatDateTime(''), '—');
});

test('the action summary names the action kind without inferring it', () => {
  assert.equal(actionSummary({ kind: 'agent', goal: 'write the report' }), 'Agent · write the report');
  assert.equal(actionSummary({ kind: 'shell', command: 'echo hi' }), '命令 · echo hi');
  assert.equal(actionSummary({ kind: 'native', action: 'open-url', input: '' }), '原生操作 · open-url');
  assert.equal(actionSummary(null), '—');
});

test('attention wording is chosen from structured state, not runtime prose', () => {
  const grant = {
    granted: false,
    external_side_effects: false,
    granted_at: null,
    requires_regrant: true,
  };
  assert.equal(attentionLabel(view({ grant })), '无人值守执行尚未授权；下次运行前需要授权。');
  assert.equal(attentionLabel(view({ run_state: 'waiting_approval' })), '上次运行停在等待授权。');
  assert.equal(attentionLabel(view({ run_state: 'unknown' })), '上次运行结果无法确认。');
  assert.equal(attentionLabel(view({ run_state: 'failed' })), '上次运行失败。');
  assert.equal(attentionLabel(view({ run_state: 'succeeded' })), null);
});

test('editor vocabulary is static and never echoes wire codes', () => {
  assert.equal(modeLabel('full_access'), '完全权限');
  assert.equal(modeLabel('assisted'), '工作区内修改');
  assert.equal(modeLabel('request_approval'), '需批准');
  assert.equal(policyLabel('continue'), '继续同一会话');
  assert.equal(policyLabel('fresh'), '每次新会话');
  assert.equal(overlapLabel('skip-if-running'), '跳过重叠运行');
  assert.equal(overlapLabel('queue-one'), '排队一次');
  assert.equal(overlapLabel('allow-concurrent'), '允许并发');
  assert.equal(misfireLabel('skip'), '跳过');
  assert.equal(misfireLabel('run-once'), '补跑一次');
});

test('doctor rows speak product language and keep unknown checks visible', () => {
  assert.equal(doctorCheckLabel('executable_exists'), '可执行文件存在');
  assert.equal(doctorCheckLabel('last_wake'), '最近系统唤醒');
  assert.equal(doctorCheckLabel('future_check'), 'future_check');
  assert.equal(severityLabel('ok'), '正常');
  assert.equal(severityLabel('warn'), '注意');
  assert.equal(severityLabel('fail'), '失败');
  assert.equal(severityLabel('info'), 'info');
});
