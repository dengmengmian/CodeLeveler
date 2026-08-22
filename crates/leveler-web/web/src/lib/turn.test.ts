// Turn Truth 契约测试：7 个 runtime 终态必须逐一保真进入 web 状态，
// reason/detail 不许丢；presentation 层的软 token 折叠规则与 TUI 对齐
// （crates/leveler-tui/src/render/transcript_lines.rs::turn_end_lines）。

import { describe, expect, it } from 'vitest';
import type { RuntimeEvent } from '../types/protocol';
import {
  commandProgressLabel,
  presentTurnEnd,
  turnEndFromEvent,
  turnFooterPrimary,
  turnProgressLabel,
} from './turn';

describe('turnEndFromEvent', () => {
  it('maps each terminal event to its own outcome — never collapses to completed', () => {
    const cases: Array<[RuntimeEvent, string, string | null]> = [
      [{ type: 'turn_completed' }, 'completed', null],
      [{ type: 'turn_answered' }, 'answered', null],
      [{ type: 'turn_truncated', error: 'token limit' }, 'truncated', 'token limit'],
      [{ type: 'turn_incomplete', reason: 'round budget' }, 'incomplete', 'round budget'],
      [
        { type: 'turn_completed_unverified', reason: 'no verification gate' },
        'unverified',
        'no verification gate',
      ],
      [{ type: 'turn_failed', error: 'boom' }, 'failed', 'boom'],
      [{ type: 'turn_cancelled' }, 'cancelled', null],
    ];
    for (const [ev, outcome, detail] of cases) {
      const end = turnEndFromEvent(ev);
      expect(end, ev.type).not.toBeNull();
      expect(end?.outcome, ev.type).toBe(outcome);
      expect(end?.detail ?? null, ev.type).toBe(detail);
    }
  });

  it('returns null for non-terminal events', () => {
    expect(turnEndFromEvent({ type: 'turn_progress', phase: 'active', closing: false, no_progress_streak: 0, closeout_deny_rounds: 0 })).toBeNull();
    expect(turnEndFromEvent({ type: 'runtime_ready' })).toBeNull();
  });
});

describe('presentTurnEnd', () => {
  it('incomplete is never presented as success and keeps its reason', () => {
    const p = presentTurnEnd({ outcome: 'incomplete', detail: 'goal unresolved' });
    expect(p.tone).toBe('warn');
    expect(p.label).toBe('未完成');
    expect(p.detail).toBe('goal unresolved');
    // 机器 token 走 TUI 同款本地化，但语义仍是未完成 + 有 detail。
    const budget = presentTurnEnd({ outcome: 'incomplete', detail: 'budget_exhausted dimension=rounds' });
    expect(budget.tone).toBe('warn');
    expect(budget.detail).toBe('预算用尽 · 说「继续」接着做');
  });

  it('incomplete with a failed verification gate says so instead of a generic block', () => {
    const p = presentTurnEnd({ outcome: 'incomplete', detail: 'failed gate(s): cargo test' });
    expect(p.label).toBe('验证未通过');
    expect(p.tone).toBe('warn');
  });

  it('unverified is never presented as plain completed', () => {
    const p = presentTurnEnd({ outcome: 'unverified', detail: 'verification unavailable' });
    expect(p.tone).toBe('warn');
    expect(p.label).toBe('已完成但未验证');
    expect(p.detail).toBe('verification unavailable');
  });

  it('soft token no_code_changes folds into a calm marker (TUI parity)', () => {
    const p = presentTurnEnd({ outcome: 'unverified', detail: 'no_code_changes' });
    expect(p.tone).toBe('calm');
    expect(p.label).toBe('结束 · 未改仓库');
    expect(p.detail).toBeNull(); // token folded into the label, not re-shown
  });

  it('soft token no_automatic_verification folds into a calm marker (TUI parity)', () => {
    const p = presentTurnEnd({ outcome: 'unverified', detail: 'no_automatic_verification' });
    expect(p.tone).toBe('calm');
    expect(p.label).toBe('完成 · 未自动验证');
    expect(p.detail).toBeNull();
  });

  it('failed / cancelled / truncated are distinct', () => {
    expect(presentTurnEnd({ outcome: 'failed', detail: 'boom' }).tone).toBe('error');
    expect(presentTurnEnd({ outcome: 'failed', detail: 'boom' }).label).toBe('执行失败');
    expect(presentTurnEnd({ outcome: 'cancelled', detail: null }).tone).toBe('muted');
    expect(presentTurnEnd({ outcome: 'cancelled', detail: null }).label).toBe('已停止');
    expect(presentTurnEnd({ outcome: 'truncated', detail: 'output limit' }).tone).toBe('error');
    expect(presentTurnEnd({ outcome: 'truncated', detail: 'output limit' }).label).toBe('执行被截断');
  });

  it('completed and answered are both success but keep distinct labels', () => {
    expect(presentTurnEnd({ outcome: 'completed', detail: null }).label).toBe('任务已完成');
    expect(presentTurnEnd({ outcome: 'answered', detail: null }).label).toBe('已回答');
    expect(presentTurnEnd({ outcome: 'completed', detail: null }).tone).toBe('success');
    expect(presentTurnEnd({ outcome: 'answered', detail: null }).tone).toBe('success');
  });
});

describe('turnFooterPrimary', () => {
  it('appends duration, not a wall-clock', () => {
    expect(turnFooterPrimary({ outcome: 'answered', detail: null }, 2100)).toBe('已回答 · 2s');
    expect(turnFooterPrimary({ outcome: 'completed', detail: null }, 13000)).toBe('任务已完成 · 13s');
    expect(turnFooterPrimary({ outcome: 'failed', detail: 'boom' }, 8000)).toBe('执行失败 · 8s');
    expect(turnFooterPrimary({ outcome: 'answered', detail: null }, 0)).toBe('已回答');
    expect(turnFooterPrimary({ outcome: 'answered', detail: null }, 2100)).not.toMatch(/\d{1,2}:\d{2}:\d{2}/);
  });
});

describe('progress → activity labels (TUI parity)', () => {
  it('command progress names the command with a live elapsed', () => {
    expect(commandProgressLabel('cargo test', 92_000)).toBe('运行 cargo test · 01:32');
  });

  it('turn progress surfaces closing and no-progress streaks', () => {
    expect(turnProgressLabel('verification', true, 0)).toBe('收口中 · verification');
    expect(turnProgressLabel('implementation', false, 2)).toBe('无进展 ×2 · implementation');
    expect(turnProgressLabel('active', false, 0)).toBeNull();
  });
});
