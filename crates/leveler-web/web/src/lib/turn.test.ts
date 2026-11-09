import { describe, expect, it } from 'vitest';
import type { RuntimeEvent } from '../types/protocol';
import { commandProgressLabel, finalizationStageLabel, presentTurnEnd, turnEndFromEvent, turnFooterPrimary } from './turn';

describe('turn truth', () => {
  it('maps every supported terminal event without a verification terminal', () => {
    const cases: Array<[RuntimeEvent, string]> = [
      [{ type: 'turn_completed' }, 'completed'],
      [{ type: 'turn_completed_with_warnings', reason: 'review unavailable' }, 'completed_with_warnings'],
      [{ type: 'turn_answered' }, 'answered'],
      [{ type: 'turn_truncated', error: 'limit' }, 'truncated'],
      [{ type: 'turn_incomplete', reason: 'budget' }, 'incomplete'],
      [{ type: 'turn_failed', error: 'boom' }, 'failed'],
      [{ type: 'turn_cancelled' }, 'cancelled'],
      [{ type: 'task_cancelled' }, 'cancelled'],
    ];
    for (const [event, outcome] of cases) expect(turnEndFromEvent(event)?.outcome).toBe(outcome);
  });

  it('finalization progress is not a terminal fact', () => {
    expect(turnEndFromEvent({ type: 'turn_finalizing', stage: 'publishing_terminal' })).toBeNull();
  });

  it('does not present an unregistered answer as a completed task', () => {
    const warning = 'required independent review reported 1 finding(s)';
    const end = turnEndFromEvent({
      type: 'turn_completed_with_warnings',
      reason: warning,
      unregistered_task: true,
    });
    expect(end?.outcome).toBe('answered_unregistered');
    const shown = presentTurnEnd(end!);
    expect(shown.label).toBe('回答结束 · 任务未登记');
    expect(shown.tone).toBe('warn');
    expect(shown.detail).toContain('本轮只回答了问题，未登记执行任务；不构成验收完成。');
    // The registration fact does not replace the terminal's warnings.
    expect(shown.detail).toContain(warning);
    // A legacy warning event carries no unregistered fact; it keeps its
    // historical completed-with-warnings reading.
    const plain = turnEndFromEvent({
      type: 'turn_completed_with_warnings',
      reason: 'review unavailable',
    });
    expect(plain?.outcome).toBe('completed_with_warnings');
    expect(presentTurnEnd(plain!).detail).toBe('review unavailable');
  });

  it('presents completion and incompletion directly', () => {
    expect(presentTurnEnd({ outcome: 'completed', detail: null }).label).toBe('任务已完成');
    const incomplete = presentTurnEnd({ outcome: 'incomplete', detail: 'budget exhausted' });
    expect(incomplete.label).toBe('未完成');
    expect(incomplete.tone).toBe('warn');
  });

  it('keeps ordinary command and review progress labels', () => {
    expect(commandProgressLabel('cargo test', 92_000)).toBe('运行 cargo test · 01:32');
    expect(finalizationStageLabel('review')).toBe('正在复核');
    expect(turnFooterPrimary({ outcome: 'answered', detail: null }, 2100)).toBe('已回答 · 2s');
  });
});
