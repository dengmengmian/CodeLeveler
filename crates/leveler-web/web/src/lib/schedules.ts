// 定时任务的呈现词汇。运行时的投影是事实源；这里只把结构化字段翻译成
// 产品语言，绝不从名称、文本或模型输出反推状态。

import type {
  ScheduleFilter,
  ScheduleRunView,
  ScheduleStatusView,
  ScheduleTriggerWire,
  ScheduleView,
} from '../types/protocol';

export type ScheduleTone = 'muted' | 'run' | 'ok' | 'warn' | 'danger';

export interface ScheduleStateLabel {
  label: string;
  tone: ScheduleTone;
}

/** 列表/详情共用的状态词。顺序与 TUI 一致：暂停优先于上次结果；
 *  等待授权不是"正在执行"；unknown 不是可重试的失败。 */
export function scheduleStateLabel(view: ScheduleView): ScheduleStateLabel {
  if (view.running) return { label: '正在执行', tone: 'run' };
  if (!view.enabled) return { label: '已暂停', tone: 'muted' };
  if (view.grant.requires_regrant) {
    return { label: '需要授权', tone: 'warn' };
  }
  switch (view.run_state) {
    case 'dispatching':
    case 'running':
      return { label: '正在执行', tone: 'run' };
    case 'waiting_approval':
      return { label: '需要授权', tone: 'warn' };
    case 'blocked':
      return { label: '已阻塞', tone: 'warn' };
    case 'unknown':
      return { label: '结果未确认', tone: 'warn' };
    case 'skipped':
      return { label: '已跳过', tone: 'muted' };
    case 'missed':
      return { label: '已错过', tone: 'muted' };
    case 'failed':
      return { label: '失败', tone: 'danger' };
    case 'cancelled':
      return { label: '已取消', tone: 'muted' };
    case 'submitted':
      return { label: '已提交', tone: 'run' };
    case 'succeeded':
      return { label: '已完成', tone: 'ok' };
    default:
      return { label: '已计划', tone: 'muted' };
  }
}

export function runStateLabel(state: string): ScheduleStateLabel {
  switch (state) {
    case 'dispatching':
      return { label: '正在派发', tone: 'run' };
    case 'running':
      return { label: '正在执行', tone: 'run' };
    case 'succeeded':
      return { label: '已完成', tone: 'ok' };
    case 'submitted':
      return { label: '已提交', tone: 'run' };
    case 'failed':
      return { label: '失败', tone: 'danger' };
    case 'unknown':
      return { label: '结果未确认', tone: 'warn' };
    case 'skipped':
      return { label: '已跳过', tone: 'muted' };
    case 'missed':
      return { label: '已错过', tone: 'muted' };
    case 'waiting_approval':
      return { label: '需要授权', tone: 'warn' };
    case 'blocked':
      return { label: '已阻塞', tone: 'warn' };
    case 'cancelled':
      return { label: '已取消', tone: 'muted' };
    default:
      return { label: state, tone: 'muted' };
  }
}

/** 结构化规则 → 产品语言。不使用运行时返回的英文 trigger_text。 */
export function triggerLabel(trigger: ScheduleTriggerWire): string {
  const zone = (timezone: string | null | undefined) => (timezone ? ` (${timezone})` : '');
  switch (trigger.kind) {
    case 'once':
      return `一次 · ${formatDateTime(trigger.at)}`;
    case 'every': {
      if (trigger.every_secs % 3600 === 0 && trigger.every_secs >= 3600) {
        return `每隔 ${trigger.every_secs / 3600} 小时`;
      }
      if (trigger.every_secs % 60 === 0 && trigger.every_secs >= 60) {
        return `每隔 ${trigger.every_secs / 60} 分钟`;
      }
      return `每隔 ${trigger.every_secs} 秒`;
    }
    case 'daily':
      return `每天 ${trigger.time}${zone(trigger.timezone)}`;
    case 'weekly':
      return `每周 ${trigger.weekdays} ${trigger.time}${zone(trigger.timezone)}`;
    case 'cron':
      return `Cron ${trigger.expression}${zone(trigger.timezone)}`;
  }
}

export function filterLabel(filter: ScheduleFilter): string {
  switch (filter) {
    case 'enabled':
      return '已启用';
    case 'paused':
      return '已暂停';
    case 'running':
      return '正在执行';
    case 'needs_attention':
      return '需要处理';
    case 'recent_failure':
      return '最近失败';
    default:
      return '全部';
  }
}

export interface WakeupLabel {
  label: string;
  tone: ScheduleTone;
  /** 能否在 UI 里直接启用/禁用。 */
  actionable: boolean;
  enabled: boolean;
}

/** 系统后台唤醒的权威状态。注册后才说"已注册"，绝不因为 UI 打开就说启用。 */
export function wakeupLabel(status: ScheduleStatusView | null): WakeupLabel {
  if (!status) {
    return { label: '后台唤醒状态未知', tone: 'muted', actionable: false, enabled: false };
  }
  const wakeup = status.wakeup;
  if (!wakeup.supported) {
    return {
      label: '本平台不支持系统唤醒',
      tone: 'muted',
      actionable: false,
      enabled: false,
    };
  }
  if (wakeup.registered) {
    const everyMinute = wakeup.interval_secs === 60 ? ' · 每分钟检查一次' : '';
    return {
      label: `系统唤醒已注册${everyMinute}`,
      tone: 'ok',
      actionable: true,
      enabled: true,
    };
  }
  return {
    label: '后台唤醒未启用',
    tone: 'warn',
    actionable: true,
    enabled: false,
  };
}

export function formatDateTime(raw: string | null | undefined): string {
  if (!raw) return '—';
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) return raw;
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function formatCost(micros: number): string {
  if (!micros) return '$0';
  if (micros < 10_000) return `$${(micros / 1_000_000).toFixed(6)}`;
  return `$${(micros / 1_000_000).toFixed(4)}`;
}

/** 一次运行的用量。缺数据就显示 —，不补零伪装成已计量。 */
export function runUsage(run: ScheduleRunView): string {
  if (!run.usage) return '—';
  const u = run.usage;
  const tokens = `${u.input_tokens.toLocaleString()} in / ${u.output_tokens.toLocaleString()} out`;
  const cost = formatCost(u.cost_usd_micros);
  const missing = u.rows_without_cost > 0 ? ` · ${u.rows_without_cost} 条未计价` : '';
  return `${u.requests} 次请求 · ${tokens} · ${cost}${missing}`;
}

export type ScheduleIntent =
  | 'create'
  | 'update'
  | 'pause'
  | 'resume'
  | 'run_now'
  | 'cancel'
  | 'grant'
  | 'revoke_grant'
  | 'wakeup_enable'
  | 'wakeup_disable'
  | 'remove'
  | 'doctor';

export function intentLabel(intent: ScheduleIntent | null): string {
  switch (intent) {
    case 'create':
      return '已创建';
    case 'update':
      return '已保存';
    case 'pause':
      return '已暂停';
    case 'resume':
      return '已恢复';
    case 'run_now':
      return '已请求立即运行';
    case 'cancel':
      return '已请求取消';
    case 'grant':
      return '已授权无人值守';
    case 'revoke_grant':
      return '已撤销授权';
    case 'wakeup_enable':
      return '已启用系统唤醒';
    case 'wakeup_disable':
      return '已关闭系统唤醒';
    case 'remove':
      return '已删除';
    case 'doctor':
      return '后台调度检查';
    default:
      return '已保存';
  }
}
