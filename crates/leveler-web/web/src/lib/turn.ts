// Turn Truth：runtime 7 个终态 → web 状态与展示的唯一映射点。
// 原则：状态层永不丢失 outcome/detail；presentation 层的措辞与 TUI 对齐
// （crates/leveler-tui/src/render/transcript_lines.rs::turn_end_lines），
// 软 token（no_code_changes / no_automatic_verification）折叠成平静收场，
// 其余 unverified/incomplete 一律带 reason 的警示——truth 优先于视觉简洁。

import type { RuntimeEvent } from '../types/protocol';

export type TurnOutcome =
  | 'completed'
  | 'answered'
  | 'incomplete'
  | 'unverified'
  | 'truncated'
  | 'failed'
  | 'cancelled';

export interface TurnEnd {
  outcome: TurnOutcome;
  /** runtime 的 reason/error 原文；completed/answered/cancelled 为 null。 */
  detail: string | null;
}

/** 终态事件 → TurnEnd；非终态返回 null。 */
export function turnEndFromEvent(ev: RuntimeEvent): TurnEnd | null {
  switch (ev.type) {
    case 'turn_completed':
      return { outcome: 'completed', detail: null };
    case 'turn_answered':
      return { outcome: 'answered', detail: null };
    case 'turn_truncated':
      return { outcome: 'truncated', detail: ev.error };
    case 'turn_incomplete':
      return { outcome: 'incomplete', detail: ev.reason };
    case 'turn_completed_unverified':
      return { outcome: 'unverified', detail: ev.reason };
    case 'turn_failed':
      return { outcome: 'failed', detail: ev.error };
    case 'turn_cancelled':
      return { outcome: 'cancelled', detail: null };
    default:
      return null;
  }
}

/** 与 Rust event.rs 的稳定 reason token 一致。 */
const REASON_NO_CODE_CHANGES = 'no_code_changes';
const REASON_NO_AUTOMATIC_VERIFICATION = 'no_automatic_verification';

export type TurnTone = 'success' | 'calm' | 'warn' | 'error' | 'muted';

export interface TurnEndPresentation {
  glyph: string;
  label: string;
  tone: TurnTone;
  /** 需要额外展示的 reason；已折叠进 label 的软 token 为 null。 */
  detail: string | null;
}

/** 机器 reason token → 短产品文案（TUI localized_turn_detail 的最小移植）。 */
function localizeDetail(detail: string): string {
  const d = detail.trim();
  if (d === REASON_NO_AUTOMATIC_VERIFICATION) return '无自动验证配置';
  if (d.includes('budget_exhausted') || d.includes('budget exhausted') || d.includes('预算已耗尽')) {
    return '预算用尽 · 说「继续」接着做';
  }
  if (d.includes('observe thrash') && d.includes('plan complete')) return '计划已完成 · 重复观察已中止';
  if (d.includes('observe thrash') || d.startsWith('no-progress streak')) return '无进展 · 重复观察已中止';
  return d;
}

/** Conversation Turn Footer primary line: Turn Truth + duration, no wall-clock. */
export function turnFooterPrimary(end: TurnEnd, ms: number): string {
  const p = presentTurnEnd(end);
  const sec = Math.round(ms / 1000);
  return sec > 0 ? `${p.label} · ${sec}s` : p.label;
}

export function presentTurnEnd(end: TurnEnd): TurnEndPresentation {
  const token = end.detail?.trim() ?? null;
  switch (end.outcome) {
    case 'completed':
      return { glyph: '✓', label: '任务已完成', tone: 'success', detail: null };
    case 'answered':
      return { glyph: '✓', label: '已回答', tone: 'success', detail: null };
    case 'truncated':
      return { glyph: '✕', label: '执行被截断', tone: 'error', detail: token };
    case 'incomplete': {
      const gateFailure = token?.startsWith('failed gate(s)') ?? false;
      return {
        glyph: '⚠',
        label: gateFailure ? '验证未通过' : '未完成',
        tone: 'warn',
        detail: token ? localizeDetail(token) : null,
      };
    }
    case 'unverified':
      if (token === REASON_NO_CODE_CHANGES) {
        return { glyph: '◇', label: '结束 · 未改仓库', tone: 'calm', detail: null };
      }
      if (token === REASON_NO_AUTOMATIC_VERIFICATION) {
        return { glyph: '✓', label: '完成 · 未自动验证', tone: 'calm', detail: null };
      }
      return {
        glyph: '⚠',
        label: '已完成但未验证',
        tone: 'warn',
        detail: token ? localizeDetail(token) : null,
      };
    case 'failed':
      return { glyph: '✕', label: '执行失败', tone: 'error', detail: token };
    case 'cancelled':
      return { glyph: '■', label: '已停止', tone: 'muted', detail: null };
  }
}

function fmtElapsed(totalSecs: number): string {
  const m = Math.floor(totalSecs / 60);
  const s = totalSecs % 60;
  return `${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
}

/** command_progress → activity 文案（TUI runtime_apply.rs::CommandProgress 同款）。 */
export function commandProgressLabel(label: string, elapsedMs: number): string {
  return `运行 ${label} · ${fmtElapsed(Math.floor(elapsedMs / 1000))}`;
}

/** turn_progress → activity 文案；无需展示时返回 null（TUI 同款规则）。 */
export function turnProgressLabel(
  phase: string,
  closing: boolean,
  noProgressStreak: number,
): string | null {
  if (closing) return `收口中 · ${phase}`;
  if (noProgressStreak > 0) return `无进展 ×${noProgressStreak} · ${phase}`;
  return null;
}
