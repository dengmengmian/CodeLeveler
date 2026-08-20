import { describe, expect, it } from 'vitest';
import { reasoningLabel } from './format';
import { runConfigCompact, runConfigSummary } from './runConfig';

describe('runConfigSummary', () => {
  it('projects snapshot.reasoning.effective max as Max, never a client-invented value', () => {
    expect(reasoningLabel('max')).toBe('Max');
    expect(
      runConfigSummary({
        modelLabel: 'GLM-5.2',
        reasoning: reasoningLabel('max'),
        workProfile: 'balanced',
        collaboration: 'goal',
        permission: 'assisted',
      }),
    ).toBe('GLM-5.2 · Max · Balanced · Goal · 辅助模式');
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

  it('compact control shows model and work profile only', () => {
    expect(runConfigCompact({ modelLabel: 'DeepSeek V4', workProfile: 'balanced' })).toBe(
      'DeepSeek V4 · Balanced',
    );
  });
});
