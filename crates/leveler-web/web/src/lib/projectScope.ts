// Project → Sessions: one selected repository path owns the session list.
// Identity is the canonical repo path. Not a ProjectId.

export function sessionsForProject<T extends { repository?: string | null }>(
  sessions: readonly T[],
  projectPath: string | null,
): T[] {
  if (!projectPath) return [];
  return sessions.filter((s) => (s.repository ?? '') === projectPath);
}

export function sessionBelongsToProject(
  repository: string | null | undefined,
  projectPath: string | null,
): boolean {
  if (!projectPath) return false;
  return (repository ?? '') === projectPath;
}

/** Honest labels from persisted `SessionStatus`. Live waiting is not a list status. */
export type SessionStatusKind =
  | 'running'
  | 'waiting'
  | 'failed'
  | 'completed'
  | 'blocked'
  | 'interrupted'
  | 'incomplete'
  | 'idle';

export function sessionStatusCue(status: string): {
  kind: SessionStatusKind;
  label: string;
} {
  switch (status) {
    case 'running':
      return { kind: 'running', label: 'Running' };
    case 'failed':
      return { kind: 'failed', label: 'Failed' };
    case 'completed':
      return { kind: 'completed', label: 'Completed' };
    case 'blocked':
      return { kind: 'blocked', label: 'Blocked' };
    case 'interrupted':
      return { kind: 'interrupted', label: 'Interrupted' };
    case 'incomplete':
      return { kind: 'incomplete', label: 'Incomplete' };
    default:
      return { kind: 'idle', label: '' };
  }
}
