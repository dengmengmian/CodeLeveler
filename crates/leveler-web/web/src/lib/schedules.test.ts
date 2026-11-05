// 产品词汇契约：状态词只由结构化字段决定，绝不从文本推断。
// unknown 不许读成"失败，可重试"；等待授权不许读成"正在执行"。

import { describe, expect, it } from 'vitest';
import type { ScheduleStatusView, ScheduleView } from '../types/protocol';
import {
  formatCost,
  runStateLabel,
  runUsage,
  scheduleStateLabel,
  triggerLabel,
  wakeupLabel,
} from './schedules';

function view(over: Partial<ScheduleView> = {}): ScheduleView {
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

function status(over: Partial<ScheduleStatusView['wakeup']>): ScheduleStatusView {
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
      ...over,
    },
  };
}

describe('schedule vocabulary', () => {
  it('a live run outranks every earlier result', () => {
    expect(scheduleStateLabel(view({ running: true, run_state: 'failed' })).label).toBe('正在执行');
    expect(scheduleStateLabel(view({ running: true, enabled: false })).label).toBe('正在执行');
  });

  it('a paused schedule never reads as merely scheduled', () => {
    expect(scheduleStateLabel(view({ enabled: false, run_state: 'succeeded' })).label).toBe('已暂停');
  });

  it('waiting for authorization is not “running” and unknown is not “failed”', () => {
    expect(scheduleStateLabel(view({ run_state: 'waiting_approval' })).label).toBe('需要授权');
    expect(runStateLabel('waiting_approval').label).toBe('需要授权');
    expect(scheduleStateLabel(view({ run_state: 'unknown' })).label).toBe('结果未确认');
    expect(runStateLabel('unknown').label).toBe('结果未确认');
    expect(runStateLabel('unknown').tone).not.toBe('danger');
  });

  it('a missing grant needs authorization under every permission profile', () => {
    for (const mode of ['assisted', 'full_access']) {
      expect(
        scheduleStateLabel(
          view({
            mode,
            run_state: 'succeeded',
            grant: {
              granted: false,
              external_side_effects: false,
              granted_at: null,
              requires_regrant: true,
            },
          }),
        ).label,
      ).toBe('需要授权');
    }
  });

  it('submitted is not done', () => {
    expect(runStateLabel('submitted').label).toBe('已提交');
  });

  it('triggers are described from the structured rule, including the zone', () => {
    expect(triggerLabel({ kind: 'daily', time: '09:00', timezone: 'Asia/Shanghai' })).toBe(
      '每天 09:00 (Asia/Shanghai)',
    );
    expect(triggerLabel({ kind: 'every', every_secs: 7200 })).toBe('每隔 2 小时');
    expect(triggerLabel({ kind: 'every', every_secs: 90 })).toBe('每隔 90 秒');
    expect(triggerLabel({ kind: 'weekly', time: '08:30', weekdays: 'mon,wed', timezone: null })).toBe(
      '每周 mon,wed 08:30',
    );
    expect(triggerLabel({ kind: 'cron', expression: '0 9 * * mon-fri', timezone: 'UTC' })).toBe(
      'Cron 0 9 * * mon-fri (UTC)',
    );
  });

  it('the wakeup chip only says registered when the runtime observed it', () => {
    expect(wakeupLabel(null).label).toBe('后台唤醒状态未知');
    expect(wakeupLabel(status({ supported: false, backend: 'unsupported' }))).toMatchObject({
      label: '本平台不支持系统唤醒',
      actionable: false,
    });
    expect(wakeupLabel(status({ registered: false }))).toMatchObject({
      label: '后台唤醒未启用',
      tone: 'warn',
      actionable: true,
    });
    expect(wakeupLabel(status({ registered: true, interval_secs: 60 }))).toMatchObject({
      label: '系统唤醒已注册 · 每分钟检查一次',
      tone: 'ok',
      enabled: true,
    });
  });

  it('usage stays missing rather than being padded with zeros', () => {
    expect(
      runUsage({
        id: 'r',
        scheduled_for: 't',
        started_at: 't',
        state: 'succeeded',
        usage: null,
      }),
    ).toBe('—');
    expect(
      runUsage({
        id: 'r',
        scheduled_for: 't',
        started_at: 't',
        state: 'succeeded',
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
    ).toBe('3 次请求 · 1,200 in / 340 out · $0.0123 · 1 条未计价');
    expect(formatCost(0)).toBe('$0');
  });
});
