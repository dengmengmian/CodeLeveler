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
    ];
    for (const [event, outcome] of cases) expect(turnEndFromEvent(event)?.outcome).toBe(outcome);
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
