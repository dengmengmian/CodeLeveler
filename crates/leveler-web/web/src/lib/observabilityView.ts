// Frontend projection of QueryObservability DTOs.
// Components must not read SessionView.tools for session totals,
// and must not interpret raw RuntimeEvent shapes here.

import { formatClock } from './format';
import type {
  ObservationClass,
  RuntimeEvent,
  UiObservabilityLoaded,
  UiObservationRow,
  UiToolAggregate,
} from '../types/protocol';

export type ExecKind = 'model' | 'tool' | 'verify' | 'agent' | 'recovery' | 'system' | 'terminal';
export type ExecStatus = 'running' | 'ok' | 'fail' | 'info';

export interface ExecStep {
  sequence: number;
  time: string;
  kind: ExecKind;
  status: ExecStatus;
  title: string;
  detail: string;
  durationMs: number | null;
  eventType: string;
  turnId: string | null;
  class: ObservationClass;
}

export interface ExecTurnGroup {
  turnId: string | null;
  steps: ExecStep[];
}

export interface RuntimeSummary {
  model: string;
  status: string;
  goal: string;
  durationMs: number | null;
  /** Whole-session tool_call_started count from the observatory, not live tools. */
  toolStarted: number;
  toolFinished: number;
  requestCount: number;
  verificationRuns: number;
  inputTokens: number;
  outputTokens: number;
  lastSequence: number | null;
}

export interface ObservabilityView {
  summary: RuntimeSummary;
  groups: ExecTurnGroup[];
  tools: UiToolAggregate[];
}

const KIND: Record<ObservationClass, ExecKind> = {
  model: 'model',
  read: 'tool',
  search: 'tool',
  edit: 'tool',
  shell: 'tool',
  tool: 'tool',
  verify: 'verify',
  agent: 'agent',
  recovery: 'recovery',
  system: 'system',
  terminal: 'terminal',
};

function asStatus(raw: string): ExecStatus {
  if (raw === 'running' || raw === 'ok' || raw === 'fail' || raw === 'info') return raw;
  return 'info';
}

function clock(iso: string): string {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return '';
  return formatClock(new Date(t));
}

function sessionDurationMs(createdAt: string, updatedAt: string): number | null {
  const a = Date.parse(createdAt);
  const b = Date.parse(updatedAt);
  if (Number.isNaN(a) || Number.isNaN(b) || b < a) return null;
  return b - a;
}

export function projectRow(row: UiObservationRow): ExecStep {
  return {
    sequence: row.sequence,
    time: clock(row.created_at),
    kind: KIND[row.class] ?? 'system',
    status: asStatus(row.status),
    title: row.title,
    detail: row.target ?? '',
    durationMs: row.duration_ms ?? null,
    eventType: row.event_type,
    turnId: row.turn_id ?? null,
    class: row.class,
  };
}

export function groupByTurn(steps: readonly ExecStep[]): ExecTurnGroup[] {
  const order: string[] = [];
  const bags = new Map<string, ExecStep[]>();
  for (const step of steps) {
    const key = step.turnId ?? `__seq_${step.sequence}`;
    let bag = bags.get(key);
    if (!bag) {
      bag = [];
      bags.set(key, bag);
      order.push(key);
    }
    bag.push(step);
  }
  return order.map((key) => {
    const stepsFor = bags.get(key) ?? [];
    return { turnId: stepsFor[0]?.turnId ?? null, steps: stepsFor };
  });
}

export function projectObservability(loaded: UiObservabilityLoaded): ObservabilityView {
  const s = loaded.session;
  const steps = loaded.window.map(projectRow);
  return {
    summary: {
      model: s.model,
      status: s.status,
      goal: s.goal,
      durationMs: sessionDurationMs(s.created_at, s.updated_at),
      toolStarted: s.tool_started,
      toolFinished: s.tool_finished,
      requestCount: s.request_count,
      verificationRuns: s.verification_runs,
      inputTokens: s.input_tokens,
      outputTokens: s.output_tokens,
      lastSequence: s.last_sequence ?? null,
    },
    groups: groupByTurn(steps),
    tools: loaded.tools,
  };
}

/** Same refresh set the TUI uses for `/trace`. Do not invent extra event types. */
export function shouldRefreshObservability(ev: RuntimeEvent): boolean {
  switch (ev.type) {
    case 'tool_call_started':
    case 'tool_call_completed':
    case 'token_usage':
    case 'verification_updated':
    case 'sub_agent_updated':
    case 'turn_completed':
    case 'turn_answered':
    case 'turn_failed':
    case 'turn_incomplete':
    case 'turn_completed_unverified':
    case 'turn_cancelled':
    case 'context_compacted':
    case 'checkpoint_created':
    case 'background_task_started':
    case 'background_task_exited':
      return true;
    default:
      return false;
  }
}
