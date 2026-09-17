import type { ApprovalDecision, UiApprovalRequest } from '../types/protocol';

const APPROVAL_KEYS: Record<string, ApprovalDecision> = {
  y: 'approve_once',
  s: 'approve_session',
  a: 'approve_always',
  n: 'deny',
};

export function approvalKeyDecision(
  key: string,
  request: Pick<UiApprovalRequest, 'always_persists'>,
): ApprovalDecision | null {
  const decision = APPROVAL_KEYS[key] ?? null;
  // 「始终允许」只在 runtime 真会写入规则时才算数；否则按键无效，不假装。
  if (decision === 'approve_always' && !request.always_persists) return null;
  return decision;
}
