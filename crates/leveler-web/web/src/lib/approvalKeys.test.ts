import { describe, expect, it } from 'vitest';
import { approvalKeyDecision } from './approvalKeys';

describe('approvalKeyDecision', () => {
  it('maps the answer keys', () => {
    expect(approvalKeyDecision('y', { always_persists: true })).toBe('approve_once');
    expect(approvalKeyDecision('s', { always_persists: false })).toBe('approve_session');
    expect(approvalKeyDecision('n', { always_persists: false })).toBe('deny');
    expect(approvalKeyDecision('x', { always_persists: true })).toBeNull();
  });

  it('answers "always" only when the runtime would persist a rule for it', () => {
    // save_agent / remember：runtime 只能按本会话处理，按 a 不能假装写了规则。
    expect(approvalKeyDecision('a', { always_persists: true })).toBe('approve_always');
    expect(approvalKeyDecision('a', { always_persists: false })).toBeNull();
    expect(approvalKeyDecision('a', {})).toBeNull();
  });
});
