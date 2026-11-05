// Presentation words for scheduled automation. The runtime projection is the
// only source of state; nothing here parses names, descriptions or model text.

const TONE = Object.freeze({ muted: 'muted', run: 'run', ok: 'ok', warn: 'warn', danger: 'danger' });

export function scheduleState(view) {
  if (view.running) return { label: '正在执行', tone: TONE.run };
  if (!view.enabled) return { label: '已暂停', tone: TONE.muted };
  if (view.grant?.requires_regrant) {
    return { label: '需要授权', tone: TONE.warn };
  }
  return runState(view.run_state, { planned: true });
}

export function runState(state, { planned = false } = {}) {
  switch (state) {
    case 'dispatching': return { label: planned ? '正在执行' : '正在派发', tone: TONE.run };
    case 'running': return { label: '正在执行', tone: TONE.run };
    case 'submitted': return { label: '已提交', tone: TONE.run };
    case 'succeeded': return { label: '已完成', tone: TONE.ok };
    case 'failed': return { label: '失败', tone: TONE.danger };
    case 'unknown': return { label: '结果未确认', tone: TONE.warn };
    case 'waiting_approval': return { label: '需要授权', tone: TONE.warn };
    case 'blocked': return { label: '已阻塞', tone: TONE.warn };
    case 'skipped': return { label: '已跳过', tone: TONE.muted };
    case 'missed': return { label: '已错过', tone: TONE.muted };
    case 'cancelled': return { label: '已取消', tone: TONE.muted };
    case null:
    case undefined:
      return planned ? { label: '已计划', tone: TONE.muted } : { label: '未知', tone: TONE.muted };
    default: return { label: String(state), tone: TONE.muted };
  }
}

const pad = value => String(value).padStart(2, '0');

export function formatDateTime(raw) {
  if (!raw) return '—';
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) return String(raw);
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

/** Local wall-clock `HH:MM` for a `<input type="datetime-local">` value. */
export function localInputValue(raw) {
  if (!raw) return '';
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) return '';
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function triggerLabel(trigger) {
  if (!trigger) return '—';
  const zone = timezone => (timezone ? ` (${timezone})` : '');
  switch (trigger.kind) {
    case 'once': return `一次 · ${formatDateTime(trigger.at)}`;
    case 'every': {
      if (trigger.every_secs % 3600 === 0 && trigger.every_secs >= 3600) return `每隔 ${trigger.every_secs / 3600} 小时`;
      if (trigger.every_secs % 60 === 0 && trigger.every_secs >= 60) return `每隔 ${trigger.every_secs / 60} 分钟`;
      return `每隔 ${trigger.every_secs} 秒`;
    }
    case 'daily': return `每天 ${trigger.time}${zone(trigger.timezone)}`;
    case 'weekly': return `每周 ${trigger.weekdays} ${trigger.time}${zone(trigger.timezone)}`;
    case 'cron': return `Cron ${trigger.expression}${zone(trigger.timezone)}`;
    default: return '—';
  }
}

export function filterLabel(filter) {
  return {
    all: '全部',
    enabled: '已启用',
    paused: '已暂停',
    running: '正在执行',
    needs_attention: '需要处理',
    recent_failure: '最近失败',
  }[filter] ?? '全部';
}

export function wakeupLabel(status) {
  if (!status) return { label: '后台唤醒状态未知', tone: TONE.muted, actionable: false, enabled: false };
  const wakeup = status.wakeup;
  if (!wakeup?.supported) {
    return { label: '本平台不支持系统唤醒', tone: TONE.muted, actionable: false, enabled: false };
  }
  if (wakeup.registered) {
    return {
      label: `系统唤醒已注册${wakeup.interval_secs === 60 ? ' · 每分钟检查一次' : ''}`,
      tone: TONE.ok,
      actionable: true,
      enabled: true,
    };
  }
  return { label: '后台唤醒未启用', tone: TONE.warn, actionable: true, enabled: false };
}

export function formatCost(micros) {
  if (!micros) return '$0';
  if (micros < 10_000) return `$${(micros / 1_000_000).toFixed(6)}`;
  return `$${(micros / 1_000_000).toFixed(4)}`;
}

export function runUsage(run) {
  if (!run?.usage) return '—';
  const usage = run.usage;
  const tokens = `${usage.input_tokens.toLocaleString()} in / ${usage.output_tokens.toLocaleString()} out`;
  const missing = usage.rows_without_cost > 0 ? ` · ${usage.rows_without_cost} 条未计价` : '';
  return `${usage.requests} 次请求 · ${tokens} · ${formatCost(usage.cost_usd_micros)}${missing}`;
}

export function actionSummary(action) {
  if (!action) return '—';
  switch (action.kind) {
    case 'agent': return `Agent · ${action.goal}`;
    case 'shell': return `命令 · ${action.command}`;
    case 'native': return `原生操作 · ${action.action}`;
    default: return '—';
  }
}

export function intentLabel(intent) {
  return {
    create: '已创建',
    update: '已保存',
    pause: '已暂停',
    resume: '已恢复',
    run_now: '已请求立即运行',
    cancel: '已请求取消',
    grant: '已授权无人值守',
    revoke_grant: '已撤销授权',
    wakeup_enable: '已启用系统唤醒',
    wakeup_disable: '已关闭系统唤醒',
    remove: '已删除',
    doctor: '后台调度检查',
  }[intent] ?? '已保存';
}

/** The actionable line for a schedule that needs a person. Derived from the
 *  same structured fields that drive the state word, never from runtime prose. */
export function attentionLabel(view) {
  if (view.grant?.requires_regrant) {
    return '无人值守执行尚未授权；下次运行前需要授权。';
  }
  switch (view.run_state) {
    case 'waiting_approval': return '上次运行停在等待授权。';
    case 'blocked': return '上次运行受阻。';
    case 'unknown': return '上次运行结果无法确认。';
    case 'failed': return '上次运行失败。';
    default: return null;
  }
}

export function modeLabel(mode) {
  return { full_access: '完全权限', assisted: '工作区内修改', request_approval: '需批准' }[mode] ?? mode ?? '—';
}

export function policyLabel(policy) {
  return { continue: '继续同一会话', fresh: '每次新会话' }[policy] ?? policy ?? '—';
}

export function overlapLabel(overlap) {
  return {
    'skip-if-running': '跳过重叠运行',
    'queue-one': '排队一次',
    'allow-concurrent': '允许并发',
  }[overlap] ?? overlap ?? '—';
}

export function misfireLabel(misfire) {
  return { skip: '跳过', 'run-once': '补跑一次' }[misfire] ?? misfire ?? '—';
}

export function doctorCheckLabel(name) {
  return {
    platform: '系统唤醒支持',
    registration: '系统唤醒注册',
    executable: '唤醒可执行文件',
    executable_exists: '可执行文件存在',
    arguments: '唤醒参数',
    interval: '唤醒间隔',
    environment: '唤醒环境',
    overdue: '待执行任务',
    last_wake: '最近系统唤醒',
  }[name] ?? name;
}

export function severityLabel(severity) {
  return { ok: '正常', warn: '注意', fail: '失败' }[severity] ?? severity ?? '—';
}
