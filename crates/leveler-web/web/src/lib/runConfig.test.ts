import { describe, expect, it } from 'vitest';
import { runConfigSummary } from './runConfig';

describe('runConfigSummary', () => {
  it('projects the five product axes as one quiet line', () => {
    expect(
      runConfigSummary({
        modelLabel: 'deepseek-v4-flash',
        reasoning: 'Max',
        workProfile: 'balanced',
        collaboration: 'goal',
        permission: 'assisted',
      }),
    ).toBe('deepseek-v4-flash · Max · Balanced · Goal · 辅助模式');
  });

  it('omits reasoning when the model has no effort', () => {
    expect(
      runConfigSummary({
        modelLabel: 'glm-5.2',
        reasoning: null,
        workProfile: 'economy',
        collaboration: 'chat',
        permission: 'request_approval',
      }),
    ).toBe('glm-5.2 · Economy · Chat · 逐次确认');
  });
});
