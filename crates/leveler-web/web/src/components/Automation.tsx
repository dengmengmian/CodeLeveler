// Automation workspace: the user-level scheduled tasks the runtime owns.
//
// Every fact on this page comes from a ScheduleManaged answer; the surface never
// reads state from display text and never runs a second scheduler. Creating or
// editing is a structured request, and an OS wakeup is only ever enabled by an
// explicit click that states what it registers.

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useBridge } from '../state/bridge';
import { useAppState } from '../state/store';
import type {
  ScheduleActionWire,
  ScheduleCheck,
  ScheduleDraft,
  ScheduleFilter,
  SchedulePatch,
  ScheduleRequest,
  ScheduleRunView,
  ScheduleStatusView,
  ScheduleTriggerWire,
  ScheduleView,
} from '../types/protocol';
import {
  filterLabel,
  formatDateTime,
  intentLabel,
  runStateLabel,
  runUsage,
  scheduleStateLabel,
  triggerLabel,
  wakeupLabel,
  type ScheduleIntent,
} from '../lib/schedules';
import type { RuntimeBridge } from '../lib/controller';

const FILTERS: ScheduleFilter[] = [
  'all',
  'enabled',
  'paused',
  'running',
  'needs_attention',
  'recent_failure',
];

interface EditorTarget {
  mode: 'create' | 'edit';
  schedule: ScheduleView | null;
}

export function AutomationView() {
  const state = useAppState();
  const bridge = useBridge();
  const schedules = state.schedules;
  const sessionId = state.current?.id ?? null;
  const [editor, setEditor] = useState<EditorTarget | null>(null);
  const [externalGrant, setExternalGrant] = useState(false);

  useEffect(() => {
    if (!sessionId) return;
    bridge.openSchedules();
    // While the page is open, re-read the runtime projection. This is a read,
    // never a second scheduler: the runtime decides what is due.
    const timer = window.setInterval(() => bridge.refreshSchedules(), 5000);
    return () => {
      window.clearInterval(timer);
      bridge.closeSchedules();
    };
  }, [bridge, sessionId]);

  useEffect(() => {
    setExternalGrant(false);
  }, [schedules.selected]);

  if (!sessionId) {
    return (
      <div className="auto-placeholder" role="status">
        <div className="auto-kicker">Automation</div>
        <p>先打开一个会话。定时任务通过该会话所属的运行时管理，不会另起一套后台进程。</p>
      </div>
    );
  }

  const wakeup = wakeupLabel(schedules.status);
  const detail = schedules.detail;

  return (
    <div className="auto-view">
      <header className="auto-head">
        <div>
          <div className="auto-kicker">Automation</div>
          <h2 className="auto-title">定时任务</h2>
          <p className="auto-sub">
            用户级计划，由运行时执行。关闭这个页面不会删除或停止任何计划。
          </p>
        </div>
        <div className="auto-head-actions">
          <button type="button" className="auto-btn ghost" onClick={() => bridge.refreshSchedules()}>
            刷新
          </button>
          <button
            type="button"
            className="auto-btn"
            disabled={schedules.busy}
            onClick={() => setEditor({ mode: 'create', schedule: null })}
          >
            + 新建定时任务
          </button>
        </div>
      </header>

      <StatusStrip status={schedules.status} />

      <div className={`auto-wakeup tone-${wakeup.tone}`}>
        <span className="auto-wakeup-label">
          <i className="dot" aria-hidden="true" />
          {wakeup.label}
        </span>
        {schedules.status?.last_os_wake ? (
          <span className="auto-muted">
            最近唤醒 {formatDateTime(schedules.status.last_os_wake.started_at)} · 认领{' '}
            {schedules.status.last_os_wake.claimed} · 成功{' '}
            {schedules.status.last_os_wake.succeeded} · 失败{' '}
            {schedules.status.last_os_wake.failed}
          </span>
        ) : (
          wakeup.actionable && <span className="auto-muted">还没有记录到系统唤醒。</span>
        )}
        <span className="auto-wakeup-actions">
          <button type="button" className="auto-btn ghost" onClick={() => bridge.checkScheduleWakeup()}>
            检查
          </button>
          {wakeup.actionable && (
            <button
              type="button"
              className="auto-btn ghost"
              disabled={schedules.busy}
              onClick={() => {
                const enable = !wakeup.enabled;
                const message = enable
                  ? '启用系统后台唤醒？\n\n这会在本机注册一个每分钟检查一次的后台任务；即使 CodeLeveler 窗口关闭，到期的计划也会被触发。计划本身不会因此被授权无人值守执行。'
                  : '关闭系统后台唤醒？\n\n关闭后，只有 CodeLeveler 运行时开着时计划才会按时执行。计划本身不会被删除。';
                if (window.confirm(message)) bridge.setScheduleWakeup(enable);
              }}
            >
              {wakeup.enabled ? '关闭后台唤醒' : '启用后台唤醒'}
            </button>
          )}
        </span>
      </div>

      {schedules.doctor && <DoctorReport report={schedules.doctor} />}

      {schedules.error && (
        <div className="auto-error" role="alert">
          {schedules.error}
        </div>
      )}
      {schedules.lastChange && !schedules.error && (
        <div className="auto-notice" role="status">
          {intentLabel(schedules.lastChange.intent)}
          {schedules.lastChange.message ? ` · ${schedules.lastChange.message}` : ''}
        </div>
      )}

      <div className="auto-filters" role="tablist" aria-label="筛选定时任务">
        {FILTERS.map((filter) => (
          <button
            key={filter}
            type="button"
            role="tab"
            aria-selected={schedules.filter === filter}
            className={`auto-filter${schedules.filter === filter ? ' on' : ''}`}
            onClick={() => bridge.setScheduleFilter(filter)}
          >
            {filterLabel(filter)}
          </button>
        ))}
      </div>

      <div className="auto-grid">
        <section className="auto-list" aria-label="定时任务列表">
          {!schedules.loaded && <div className="insp-empty">读取中…</div>}
          {schedules.loaded && schedules.items.length === 0 && (
            <div className="insp-empty">没有符合条件的定时任务。</div>
          )}
          {schedules.items.map((schedule) => {
            const label = scheduleStateLabel(schedule);
            return (
              <button
                key={schedule.id}
                type="button"
                className={`auto-row${schedules.selected === schedule.id ? ' on' : ''}`}
                onClick={() => bridge.selectSchedule(schedule.id)}
              >
                <span className="auto-row-main">
                  <span className="auto-row-name">{schedule.name}</span>
                  <span className={`auto-state tone-${label.tone}`}>{label.label}</span>
                </span>
                <span className="auto-row-meta">
                  {triggerLabel(schedule.trigger)}
                  {schedule.next_run_at ? ` · 下次 ${formatDateTime(schedule.next_run_at)}` : ''}
                </span>
                <span className="auto-row-meta">
                  {actionLabel(schedule.action)} · {schedule.workspace}
                </span>
              </button>
            );
          })}
        </section>

        <section className="auto-detail" aria-label="定时任务详情">
          {!detail && <div className="insp-empty">选择左侧的定时任务，或新建一个。</div>}
          {detail && (
            <ScheduleDetail
              schedule={detail}
              runs={schedules.runs}
              runsLoaded={schedules.runsLoaded}
              busy={schedules.busy}
              externalGrant={externalGrant}
              setExternalGrant={setExternalGrant}
              bridge={bridge}
              onEdit={() => setEditor({ mode: 'edit', schedule: detail })}
            />
          )}
        </section>
      </div>

      {editor && (
        <ScheduleEditor
          target={editor}
          defaultWorkspace={
            state.current?.repository ?? state.selectedProject ?? state.repository ?? ''
          }
          busy={schedules.busy}
          error={schedules.error}
          lastChange={schedules.lastChange}
          onClose={() => setEditor(null)}
          onSubmit={(request, intent) => bridge.manageSchedule(request, intent)}
        />
      )}
    </div>
  );
}

function StatusStrip({ status }: { status: ScheduleStatusView | null }) {
  if (!status) return null;
  return (
    <div className="auto-counts" aria-label="后台调度概览">
      <span>
        定义 <b>{status.definitions}</b>
      </span>
      <span>
        启用 <b>{status.enabled}</b>
      </span>
      <span>
        暂停 <b>{status.paused}</b>
      </span>
      <span>
        正在执行 <b>{status.running}</b>
      </span>
      {status.overdue_pending > 0 && (
        <span className="warn">
          待补执行 <b>{status.overdue_pending}</b>
        </span>
      )}
      <span>
        下次计划 <b>{formatDateTime(status.next_occurrence)}</b>
      </span>
    </div>
  );
}

function DoctorReport({ report }: { report: { healthy: boolean; checks: ScheduleCheck[] } }) {
  return (
    <div className={`auto-doctor${report.healthy ? ' ok' : ' bad'}`}>
      <div className="auto-doctor-h">{report.healthy ? '后台调度检查通过' : '后台调度检查发现问题'}</div>
      {report.checks.map((check) => (
        <div key={`${check.name}:${check.detail}`} className={`auto-doctor-row sev-${check.severity}`}>
          <span className="auto-doctor-name">{check.name}</span>
          <span>{check.detail}</span>
        </div>
      ))}
    </div>
  );
}

function actionLabel(action: ScheduleActionWire): string {
  switch (action.kind) {
    case 'shell':
      return `Shell · ${action.command}`;
    case 'native':
      return `Native · ${action.action}`;
    case 'agent':
      return `Agent · ${action.goal}`;
  }
}

function ScheduleDetail({
  schedule,
  runs,
  runsLoaded,
  busy,
  externalGrant,
  setExternalGrant,
  bridge,
  onEdit,
}: {
  schedule: ScheduleView;
  runs: ScheduleRunView[];
  runsLoaded: boolean;
  busy: boolean;
  externalGrant: boolean;
  setExternalGrant: (value: boolean) => void;
  bridge: RuntimeBridge;
  onEdit: () => void;
}) {
  const label = scheduleStateLabel(schedule);
  const grant = schedule.grant;
  const requiresExternal = schedule.action_kind === 'shell' || schedule.action_kind === 'agent';
  const grantText = grant.granted
    ? `已授权无人值守${grant.external_side_effects ? '（含外部副作用）' : '（仅此动作）'}`
    : grant.requires_regrant
      ? '需要重新授权：定义已改变'
      : '未授权无人值守';

  return (
    <div className="auto-detail-body">
      <div className="auto-detail-head">
        <div>
          <h3>{schedule.name}</h3>
          <p className="auto-sub">{schedule.description}</p>
        </div>
        <span className={`auto-state tone-${label.tone}`}>{label.label}</span>
      </div>

      {schedule.attention_reason && (
        <div className="auto-attention" role="status">
          {schedule.attention_reason}
        </div>
      )}

      <div className="auto-actions">
        <button
          type="button"
          className="auto-btn ghost"
          disabled={busy}
          onClick={() =>
            bridge.manageSchedule(
              { action: schedule.enabled ? 'pause' : 'resume', id: schedule.id },
              schedule.enabled ? 'pause' : 'resume',
            )
          }
        >
          {schedule.enabled ? '暂停' : '恢复'}
        </button>
        <button
          type="button"
          className="auto-btn ghost"
          disabled={busy}
          onClick={() => bridge.manageSchedule({ action: 'run_now', id: schedule.id }, 'run_now')}
        >
          立即运行
        </button>
        {schedule.running && (
          <button
            type="button"
            className="auto-btn ghost"
            disabled={busy}
            onClick={() => bridge.manageSchedule({ action: 'cancel', id: schedule.id }, 'cancel')}
          >
            取消执行
          </button>
        )}
        <button type="button" className="auto-btn ghost" disabled={busy} onClick={onEdit}>
          编辑
        </button>
        <button
          type="button"
          className="auto-btn ghost danger"
          disabled={busy}
          onClick={() => {
            if (window.confirm(`删除定时任务「${schedule.name}」？\n\n已产生的运行记录会保留。`)) {
              bridge.manageSchedule({ action: 'remove', id: schedule.id }, 'remove');
            }
          }}
        >
          删除
        </button>
      </div>

      <dl className="auto-fields">
        <Field label="动作">{actionLabel(schedule.action)}</Field>
        <Field label="工作目录">{schedule.workspace}</Field>
        <Field label="规则">{triggerLabel(schedule.trigger)}</Field>
        <Field label="时区">{schedule.timezone ?? '本机时区'}</Field>
        <Field label="下次运行">{formatDateTime(schedule.next_run_at)}</Field>
        <Field label="权限模式">{modeLabel(schedule.mode)}</Field>
        <Field label="会话策略">{schedule.session_policy === 'fresh' ? '每次新会话' : '延续会话'}</Field>
        <Field label="重叠策略">
          {schedule.overlap === 'allow-concurrent'
            ? '允许并发'
            : schedule.overlap === 'queue-one'
              ? '排队一次'
              : '运行中跳过'}
        </Field>
        <Field label="错过策略">
          {schedule.misfire === 'run-once'
            ? `恢复后补跑一次（宽限 ${schedule.misfire_grace_secs}s）`
            : '跳过'}
        </Field>
        <Field label="模型">{schedule.model ?? '会话默认'}</Field>
        <Field label="超时">{schedule.timeout_secs}s</Field>
        <Field label="无人值守">{grantText}</Field>
      </dl>

      <div className="auto-grant">
        {grant.granted ? (
          <button
            type="button"
            className="auto-btn ghost"
            disabled={busy}
            onClick={() =>
              bridge.manageSchedule({ action: 'revoke_grant', id: schedule.id }, 'revoke_grant')
            }
          >
            撤销无人值守授权
          </button>
        ) : (
          <>
            <label className="auto-check">
              <input
                type="checkbox"
                checked={externalGrant}
                onChange={(event) => setExternalGrant(event.target.checked)}
              />
              允许外部副作用（发邮件、推送等）
              {requiresExternal ? '（此类任务必须勾选）' : ''}
            </label>
            <button
              type="button"
              className="auto-btn"
              disabled={busy || (requiresExternal && !externalGrant)}
              onClick={() =>
                bridge.manageSchedule(
                  {
                    action: 'grant',
                    id: schedule.id,
                    allow_external_side_effects: externalGrant,
                  },
                  'grant',
                )
              }
            >
              授权无人值守
            </button>
          </>
        )}
      </div>

      {schedule.session_id && (
        <button
          type="button"
          className="auto-link"
          onClick={() => bridge.selectSession(schedule.session_id!)}
        >
          打开关联会话 · {schedule.session_id}
        </button>
      )}

      <div className="auto-runs">
        <div className="auto-runs-h">运行历史</div>
        {!runsLoaded && <div className="insp-empty">读取中…</div>}
        {runsLoaded && runs.length === 0 && <div className="insp-empty">还没有运行记录。</div>}
        {runs.map((run) => (
          <RunRow key={run.id} run={run} bridge={bridge} />
        ))}
      </div>
    </div>
  );
}

function RunRow({ run, bridge }: { run: ScheduleRunView; bridge: RuntimeBridge }) {
  const state = runStateLabel(run.state);
  const duration = runDuration(run);
  return (
    <div className="auto-run">
      <span className={`auto-state tone-${state.tone}`}>{state.label}</span>
      <span className="auto-run-time">
        {formatDateTime(run.scheduled_for)}
        {duration ? ` · 用时 ${duration}` : ''}
      </span>
      <span className="auto-run-usage">{runUsage(run)}</span>
      {run.exit_code != null && <span className="auto-muted">exit {run.exit_code}</span>}
      {run.error && <span className="auto-run-error">{run.error}</span>}
      {run.session_id && (
        <button type="button" className="auto-link" onClick={() => bridge.selectSession(run.session_id!)}>
          会话 {run.session_id.slice(0, 8)}
        </button>
      )}
    </div>
  );
}

function runDuration(run: ScheduleRunView): string | null {
  if (!run.finished_at) return null;
  const started = new Date(run.started_at).getTime();
  const finished = new Date(run.finished_at).getTime();
  if (!Number.isFinite(started) || !Number.isFinite(finished) || finished < started) return null;
  const seconds = Math.round((finished - started) / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  return `${minutes}m ${seconds % 60}s`;
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="auto-field">
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function modeLabel(mode: string): string {
  if (mode === 'full_access') return 'Full（完全放开）';
  if (mode === 'request_approval') return 'Restricted（逐项审批）';
  return 'Auto（普通动作默认允许）';
}

// ── 编辑器 ──────────────────────────────────────────────────────────

type TriggerKind = ScheduleTriggerWire['kind'];
type IntervalUnit = 'seconds' | 'minutes' | 'hours';

interface TriggerForm {
  kind: TriggerKind;
  at: string;
  time: string;
  weekdays: string[];
  every: string;
  unit: IntervalUnit;
  expression: string;
  timezone: string;
}

interface FormState {
  name: string;
  workspace: string;
  actionKind: ScheduleActionWire['kind'];
  command: string;
  nativeAction: string;
  nativeInput: string;
  goal: string;
  mode: string;
  model: string;
  overlap: string;
  sessionPolicy: string;
  misfire: string;
  misfireGrace: string;
  timeout: string;
  grantUnattended: boolean;
  allowExternal: boolean;
  trigger: TriggerForm;
}

const WEEKDAYS: Array<{ value: string; label: string }> = [
  { value: 'mon', label: '一' },
  { value: 'tue', label: '二' },
  { value: 'wed', label: '三' },
  { value: 'thu', label: '四' },
  { value: 'fri', label: '五' },
  { value: 'sat', label: '六' },
  { value: 'sun', label: '日' },
];

function hostTimezone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone ?? '';
  } catch {
    return '';
  }
}

function emptyForm(workspace: string): FormState {
  return {
    name: '',
    workspace,
    actionKind: 'agent',
    command: '',
    nativeAction: '',
    nativeInput: '',
    goal: '',
    mode: 'assisted',
    model: '',
    overlap: 'skip-if-running',
    sessionPolicy: 'continue',
    misfire: 'skip',
    misfireGrace: '0',
    timeout: '600',
    grantUnattended: false,
    allowExternal: false,
    trigger: {
      kind: 'daily',
      at: '',
      time: '09:00',
      weekdays: ['mon', 'tue', 'wed', 'thu', 'fri'],
      every: '1',
      unit: 'hours',
      expression: '',
      timezone: hostTimezone(),
    },
  };
}

function toLocalInput(raw: string): string {
  const date = new Date(raw);
  if (Number.isNaN(date.getTime())) return '';
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function formFromSchedule(schedule: ScheduleView): FormState {
  const base = emptyForm(schedule.workspace);
  const trigger = schedule.trigger;
  const form: FormState = {
    ...base,
    name: schedule.name,
    actionKind: schedule.action.kind,
    mode: schedule.mode,
    model: schedule.model ?? '',
    overlap: schedule.overlap,
    sessionPolicy: schedule.session_policy,
    misfire: schedule.misfire,
    misfireGrace: String(schedule.misfire_grace_secs),
    timeout: String(schedule.timeout_secs),
    trigger: {
      ...base.trigger,
      kind: trigger.kind,
      timezone: schedule.timezone ?? hostTimezone(),
    },
  };
  switch (schedule.action.kind) {
    case 'shell':
      form.command = schedule.action.command;
      break;
    case 'native':
      form.nativeAction = schedule.action.action;
      form.nativeInput = schedule.action.input ?? '';
      break;
    case 'agent':
      form.goal = schedule.action.goal;
      break;
  }
  switch (trigger.kind) {
    case 'once':
      form.trigger.at = toLocalInput(trigger.at);
      break;
    case 'daily':
      form.trigger.time = trigger.time;
      break;
    case 'weekly':
      form.trigger.time = trigger.time;
      form.trigger.weekdays = trigger.weekdays.split(',').map((day) => day.trim()).filter(Boolean);
      break;
    case 'every': {
      if (trigger.every_secs % 3600 === 0) {
        form.trigger.unit = 'hours';
        form.trigger.every = String(trigger.every_secs / 3600);
      } else if (trigger.every_secs % 60 === 0) {
        form.trigger.unit = 'minutes';
        form.trigger.every = String(trigger.every_secs / 60);
      } else {
        form.trigger.unit = 'seconds';
        form.trigger.every = String(trigger.every_secs);
      }
      break;
    }
    case 'cron':
      form.trigger.expression = trigger.expression;
      break;
  }
  return form;
}

function buildTrigger(trigger: TriggerForm): { value: ScheduleTriggerWire } | { error: string } {
  const timezone = trigger.timezone.trim() || null;
  switch (trigger.kind) {
    case 'once': {
      if (!trigger.at) return { error: '请选择一次执行的时间。' };
      const date = new Date(trigger.at);
      if (Number.isNaN(date.getTime())) return { error: '一次执行的时间无法解析。' };
      return { value: { kind: 'once', at: date.toISOString() } };
    }
    case 'daily':
      if (!trigger.time) return { error: '请选择每天执行的时间。' };
      return { value: { kind: 'daily', time: trigger.time, timezone } };
    case 'weekly':
      if (!trigger.time) return { error: '请选择每周执行的时间。' };
      if (trigger.weekdays.length === 0) return { error: '至少选择一个星期几。' };
      return {
        value: { kind: 'weekly', time: trigger.time, weekdays: trigger.weekdays.join(','), timezone },
      };
    case 'every': {
      const amount = Number(trigger.every);
      if (!Number.isFinite(amount) || amount <= 0) {
        return { error: '间隔必须是正数。' };
      }
      const factor = trigger.unit === 'hours' ? 3600 : trigger.unit === 'minutes' ? 60 : 1;
      const every_secs = Math.round(amount * factor);
      if (every_secs <= 0) return { error: '间隔必须大于 0 秒。' };
      return { value: { kind: 'every', every_secs } };
    }
    case 'cron':
      if (!trigger.expression.trim()) return { error: '请填写 cron 表达式。' };
      return { value: { kind: 'cron', expression: trigger.expression.trim(), timezone } };
  }
}

function buildAction(form: FormState): { value: ScheduleActionWire } | { error: string } {
  switch (form.actionKind) {
    case 'shell':
      if (!form.command.trim()) return { error: '请填写 shell 命令。' };
      return { value: { kind: 'shell', command: form.command } };
    case 'native':
      if (!form.nativeAction.trim()) return { error: '请填写原生动作名。' };
      return {
        value: {
          kind: 'native',
          action: form.nativeAction.trim(),
          input: form.nativeInput.trim(),
        },
      };
    case 'agent':
      if (!form.goal.trim()) return { error: '请填写 Agent 的目标。' };
      return { value: { kind: 'agent', goal: form.goal } };
  }
}

function ScheduleEditor({
  target,
  defaultWorkspace,
  busy,
  error,
  lastChange,
  onClose,
  onSubmit,
}: {
  target: EditorTarget;
  defaultWorkspace: string;
  busy: boolean;
  error: string | null;
  lastChange: { intent: ScheduleIntent; message: string } | null;
  onClose: () => void;
  onSubmit: (request: ScheduleRequest, intent: ScheduleIntent) => boolean;
}) {
  const [form, setForm] = useState<FormState>(() =>
    target.schedule ? formFromSchedule(target.schedule) : emptyForm(defaultWorkspace),
  );
  const [validation, setValidation] = useState<string | null>(null);
  const initialChange = useRef(lastChange);
  const intent: ScheduleIntent = target.mode === 'create' ? 'create' : 'update';

  useEffect(() => {
    if (busy) return;
    if (
      lastChange &&
      lastChange !== initialChange.current &&
      lastChange.intent === intent &&
      !error
    ) {
      onClose();
    }
  }, [busy, error, intent, lastChange, onClose]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) =>
    setForm((current) => ({ ...current, [key]: value }));
  const setTrigger = <K extends keyof TriggerForm>(key: K, value: TriggerForm[K]) =>
    setForm((current) => ({
      ...current,
      trigger: { ...current.trigger, [key]: value },
    }));

  const submit = () => {
    const name = form.name.trim();
    const workspace = form.workspace.trim();
    if (!name) return setValidation('请填写任务名。');
    if (!workspace) return setValidation('请填写工作目录。');
    const action = buildAction(form);
    if ('error' in action) return setValidation(action.error);
    const trigger = buildTrigger(form.trigger);
    if ('error' in trigger) return setValidation(trigger.error);
    const timeout = Number(form.timeout);
    if (!Number.isFinite(timeout) || timeout <= 0) return setValidation('超时必须是正数秒。');
    const grace = Number(form.misfireGrace);
    if (!Number.isFinite(grace) || grace < 0) return setValidation('宽限时间必须是非负数秒。');
    if (
      form.grantUnattended &&
      (form.actionKind === 'shell' || form.actionKind === 'agent') &&
      !form.allowExternal
    ) {
      return setValidation('Shell 和 Agent 任务可能访问工作目录之外，授权时必须勾选外部副作用。');
    }
    setValidation(null);

    if (target.mode === 'create') {
      const draft: ScheduleDraft = {
        name,
        workspace,
        action: action.value,
        trigger: trigger.value,
        mode: form.mode,
        model: form.model.trim() || null,
        timeout_secs: Math.round(timeout),
        overlap: form.overlap,
        session_policy: form.sessionPolicy,
        misfire: form.misfire,
        misfire_grace_secs: Math.round(grace),
        grant_unattended: form.grantUnattended,
        allow_external_side_effects: form.allowExternal,
      };
      onSubmit({ action: 'create', draft }, 'create');
      return;
    }

    const schedule = target.schedule!;
    // The revision is the one this editor was opened with: if the definition
    // changed underneath, the runtime refuses instead of silently overwriting.
    const patch: SchedulePatch = {
      name,
      workspace,
      action: action.value,
      trigger: trigger.value,
      mode: form.mode,
      model: form.model.trim(),
      timeout_secs: Math.round(timeout),
      overlap: form.overlap,
      session_policy: form.sessionPolicy,
      misfire: form.misfire,
      misfire_grace_secs: Math.round(grace),
    };
    onSubmit(
      {
        action: 'update',
        id: schedule.id,
        expected_revision: schedule.revision,
        patch,
      },
      'update',
    );
  };

  return (
    <div className="set-backdrop auto-backdrop" role="presentation" onClick={onClose}>
      <div
        className="auto-modal"
        role="dialog"
        aria-modal="true"
        aria-label={target.mode === 'create' ? '新建定时任务' : '编辑定时任务'}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="auto-modal-head">
          <h3>{target.mode === 'create' ? '新建定时任务' : '编辑定时任务'}</h3>
          <button type="button" className="auto-btn ghost" onClick={onClose}>
            关闭
          </button>
        </div>

        <label className="auto-input">
          <span>任务名</span>
          <input
            value={form.name}
            autoFocus
            placeholder="例如：每日构建报告"
            onChange={(event) => set('name', event.target.value)}
          />
        </label>
        <label className="auto-input">
          <span>工作目录</span>
          <input
            value={form.workspace}
            placeholder="/path/to/project"
            onChange={(event) => set('workspace', event.target.value)}
          />
        </label>

        <div className="auto-input">
          <span>动作类型</span>
          <div className="auto-seg">
            {(['agent', 'shell', 'native'] as const).map((kind) => (
              <button
                key={kind}
                type="button"
                className={form.actionKind === kind ? 'on' : ''}
                onClick={() => set('actionKind', kind)}
              >
                {kind === 'agent' ? 'Agent' : kind === 'shell' ? 'Shell' : 'Native'}
              </button>
            ))}
          </div>
        </div>
        {form.actionKind === 'agent' && (
          <label className="auto-input">
            <span>Agent 目标</span>
            <textarea
              value={form.goal}
              rows={3}
              placeholder="用自然语言描述要完成的任务；执行时由 Agent 自己决定步骤。"
              onChange={(event) => set('goal', event.target.value)}
            />
          </label>
        )}
        {form.actionKind === 'shell' && (
          <label className="auto-input">
            <span>Shell 命令</span>
            <textarea
              value={form.command}
              rows={3}
              placeholder="bash -lc '…'"
              onChange={(event) => set('command', event.target.value)}
            />
          </label>
        )}
        {form.actionKind === 'native' && (
          <>
            <label className="auto-input">
              <span>原生动作名</span>
              <input
                value={form.nativeAction}
                placeholder="例如：computer.screenshot"
                onChange={(event) => set('nativeAction', event.target.value)}
              />
            </label>
            <label className="auto-input">
              <span>参数（JSON，可留空）</span>
              <textarea
                value={form.nativeInput}
                rows={3}
                placeholder='{"path": "/tmp"}'
                onChange={(event) => set('nativeInput', event.target.value)}
              />
            </label>
          </>
        )}

        <div className="auto-input">
          <span>触发规则</span>
          <div className="auto-seg">
            {(['once', 'daily', 'weekly', 'every', 'cron'] as const).map((kind) => (
              <button
                key={kind}
                type="button"
                className={form.trigger.kind === kind ? 'on' : ''}
                onClick={() => setTrigger('kind', kind)}
              >
                {kind === 'once'
                  ? '一次'
                  : kind === 'daily'
                    ? '每天'
                    : kind === 'weekly'
                      ? '每周'
                      : kind === 'every'
                        ? '间隔'
                        : 'Cron'}
              </button>
            ))}
          </div>
        </div>

        {form.trigger.kind === 'once' && (
          <label className="auto-input">
            <span>执行时间</span>
            <input
              type="datetime-local"
              value={form.trigger.at}
              onChange={(event) => setTrigger('at', event.target.value)}
            />
          </label>
        )}
        {(form.trigger.kind === 'daily' ||
          form.trigger.kind === 'weekly' ||
          form.trigger.kind === 'cron') && (
          <label className="auto-input">
            <span>IANA 时区（留空使用本机时区）</span>
            <input
              value={form.trigger.timezone}
              placeholder="Asia/Shanghai"
              onChange={(event) => setTrigger('timezone', event.target.value)}
            />
          </label>
        )}
        {(form.trigger.kind === 'daily' || form.trigger.kind === 'weekly') && (
          <label className="auto-input">
            <span>本地时间</span>
            <input
              type="time"
              value={form.trigger.time}
              onChange={(event) => setTrigger('time', event.target.value)}
            />
          </label>
        )}
        {form.trigger.kind === 'weekly' && (
          <div className="auto-input">
            <span>星期几</span>
            <div className="auto-weekdays">
              {WEEKDAYS.map((day) => {
                const on = form.trigger.weekdays.includes(day.value);
                return (
                  <button
                    key={day.value}
                    type="button"
                    className={on ? 'on' : ''}
                    aria-pressed={on}
                    onClick={() =>
                      setTrigger(
                        'weekdays',
                        on
                          ? form.trigger.weekdays.filter((value) => value !== day.value)
                          : [...form.trigger.weekdays, day.value],
                      )
                    }
                  >
                    {day.label}
                  </button>
                );
              })}
            </div>
          </div>
        )}
        {form.trigger.kind === 'every' && (
          <div className="auto-input">
            <span>间隔</span>
            <div className="auto-every">
              <input
                type="number"
                min="1"
                value={form.trigger.every}
                onChange={(event) => setTrigger('every', event.target.value)}
              />
              <select
                value={form.trigger.unit}
                onChange={(event) => setTrigger('unit', event.target.value as IntervalUnit)}
              >
                <option value="seconds">秒</option>
                <option value="minutes">分钟</option>
                <option value="hours">小时</option>
              </select>
            </div>
          </div>
        )}
        {form.trigger.kind === 'cron' && (
          <label className="auto-input">
            <span>Cron 表达式（五段）</span>
            <input
              value={form.trigger.expression}
              placeholder="0 9 * * mon-fri"
              onChange={(event) => setTrigger('expression', event.target.value)}
            />
          </label>
        )}

        <div className="auto-form-grid">
          <label className="auto-input">
            <span>权限模式</span>
            <select value={form.mode} onChange={(event) => set('mode', event.target.value)}>
              <option value="assisted">Auto（普通动作默认允许）</option>
              <option value="request_approval">Restricted（逐项审批）</option>
              <option value="full_access">Full（完全放开）</option>
            </select>
          </label>
          <label className="auto-input">
            <span>模型（provider/model，可留空）</span>
            <input
              value={form.model}
              placeholder="deepseek/deepseek-v4"
              onChange={(event) => set('model', event.target.value)}
            />
          </label>
        </div>

        <details className="auto-advanced">
          <summary>高级</summary>
          <div className="auto-form-grid">
            <label className="auto-input">
              <span>重叠</span>
              <select value={form.overlap} onChange={(event) => set('overlap', event.target.value)}>
                <option value="skip-if-running">运行中跳过</option>
                <option value="queue-one">排队一次</option>
                <option value="allow-concurrent">允许并发</option>
              </select>
            </label>
            <label className="auto-input">
              <span>会话</span>
              <select
                value={form.sessionPolicy}
                onChange={(event) => set('sessionPolicy', event.target.value)}
              >
                <option value="continue">延续会话</option>
                <option value="fresh">每次新会话</option>
              </select>
            </label>
            <label className="auto-input">
              <span>错过</span>
              <select value={form.misfire} onChange={(event) => set('misfire', event.target.value)}>
                <option value="skip">跳过</option>
                <option value="run-once">补跑一次</option>
              </select>
            </label>
            <label className="auto-input">
              <span>补跑宽限（秒）</span>
              <input
                type="number"
                min="0"
                value={form.misfireGrace}
                onChange={(event) => set('misfireGrace', event.target.value)}
              />
            </label>
            <label className="auto-input">
              <span>超时（秒）</span>
              <input
                type="number"
                min="1"
                value={form.timeout}
                onChange={(event) => set('timeout', event.target.value)}
              />
            </label>
          </div>
        </details>

        {target.mode === 'create' ? (
          <div className="auto-grant-form">
            <label className="auto-check">
              <input
                type="checkbox"
                checked={form.grantUnattended}
                onChange={(event) => set('grantUnattended', event.target.checked)}
              />
              无人值守授权
            </label>
            <label className="auto-check">
              <input
                type="checkbox"
                checked={form.allowExternal}
                disabled={!form.grantUnattended}
                onChange={(event) => set('allowExternal', event.target.checked)}
              />
              允许外部副作用（发邮件、推送等）
            </label>
            <p className="auto-hint">
              授权只覆盖这次定义的动作、工作目录、时区、执行频率和参数；之后扩大这些范围会要求重新授权。
            </p>
          </div>
        ) : (
          <p className="auto-hint">
            修改动作、工作目录、参数、模型、权限、时区或执行频率会撤销旧的无人值守授权，下一次执行前需要重新授权。
          </p>
        )}

        {(validation || error) && (
          <div className="auto-error" role="alert">
            {validation ?? error}
          </div>
        )}

        <div className="auto-modal-actions">
          <button type="button" className="auto-btn ghost" onClick={onClose}>
            取消
          </button>
          <button type="button" className="auto-btn" disabled={busy} onClick={submit}>
            {busy ? '处理中…' : target.mode === 'create' ? '创建' : '保存'}
          </button>
        </div>
      </div>
    </div>
  );
}
