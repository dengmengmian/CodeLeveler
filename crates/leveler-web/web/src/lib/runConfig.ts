// Composer 默认态的一条运行配置摘要。轴的含义以 runtime 为准。

import { collaborationLabel, permissionMeta, workProfileLabel } from './format';
import type { PermissionProfile } from '../types/protocol';

export function runConfigSummary(input: {
  modelLabel: string;
  reasoning: string | null;
  workProfile: string;
  collaboration: string;
  permission: PermissionProfile;
}): string {
  const parts = [
    input.modelLabel,
    input.reasoning,
    workProfileLabel(input.workProfile),
    collaborationLabel(input.collaboration),
    permissionMeta(input.permission).label,
  ].filter((p): p is string => Boolean(p));
  return parts.join(' · ');
}
