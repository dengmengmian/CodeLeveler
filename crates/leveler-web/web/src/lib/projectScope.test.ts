import { describe, expect, it } from 'vitest';
import { sessionBelongsToProject, sessionStatusCue, sessionsForProject } from './projectScope';

describe('project scope', () => {
  const rows = [
    { id: 'a1', repository: '/A' },
    { id: 'b1', repository: '/B' },
    { id: 'a2', repository: '/A' },
    { id: 'orphan', repository: null },
  ];

  it('filters sessions to the selected project path only', () => {
    expect(sessionsForProject(rows, '/A').map((s) => s.id)).toEqual(['a1', 'a2']);
    expect(sessionsForProject(rows, '/B').map((s) => s.id)).toEqual(['b1']);
  });

  it('returns no sessions when no project is selected', () => {
    expect(sessionsForProject(rows, null)).toEqual([]);
  });

  it('does not treat a missing repository as belonging to a project', () => {
    expect(sessionBelongsToProject(null, '/A')).toBe(false);
    expect(sessionBelongsToProject('/A', '/A')).toBe(true);
  });

  it('maps only persisted SessionStatus strings, never invented wait states', () => {
    expect(sessionStatusCue('running')).toEqual({ kind: 'running', label: 'Running' });
    expect(sessionStatusCue('failed')).toEqual({ kind: 'failed', label: 'Failed' });
    expect(sessionStatusCue('completed')).toEqual({ kind: 'completed', label: 'Completed' });
    expect(sessionStatusCue('blocked')).toEqual({ kind: 'blocked', label: 'Blocked' });
    expect(sessionStatusCue('interrupted')).toEqual({ kind: 'interrupted', label: 'Interrupted' });
    expect(sessionStatusCue('incomplete')).toEqual({ kind: 'incomplete', label: 'Incomplete' });
    expect(sessionStatusCue('created').kind).toBe('idle');
    // Not a persisted SessionStatus. Live waiting is overlaid from pending_interactions.
    expect(sessionStatusCue('waiting_approval').kind).toBe('idle');
    expect(sessionStatusCue('idle').kind).toBe('idle');
  });
});
