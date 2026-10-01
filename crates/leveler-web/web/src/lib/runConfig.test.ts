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
        collaboration: 'goal',
        permission: 'assisted',
      }),
    ).toBe('GLM-5.2 · Max · Goal · 辅助模式');
  });

  it('omits reasoning when the model has no effort', () => {
    expect(
      runConfigSummary({
        modelLabel: 'glm-5.2',
        reasoning: null,
        collaboration: 'chat',
        permission: 'request_approval',
      }),
    ).toBe('glm-5.2 · Chat · 逐次确认');
  });

  it('compact control omits legacy work profiles', () => {
    expect(runConfigCompact({ modelLabel: 'DeepSeek V4' })).toBe(
      'DeepSeek V4',
    );
  });
});
