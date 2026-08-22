import { describe, expect, it } from 'vitest';
import {
  SLASH_COMMANDS,
  filterSlashCommands,
  groupSlashCommands,
  slashTarget,
} from './slashCommands';

describe('slash commands', () => {
  it('does not expose ClientCommand type names to the user', () => {
    const blob = JSON.stringify(SLASH_COMMANDS);
    expect(blob).not.toMatch(/SelectModel|SetProductAxes|RestoreCheckpoint|CompactContext|CancelCurrentTurn/);
  });

  it('classifies checkpoint as a picker and btw as input-mode', () => {
    expect(SLASH_COMMANDS.find((c) => c.command === '/checkpoint')?.kind).toBe('entity-picker');
    expect(SLASH_COMMANDS.find((c) => c.command === '/btw')?.kind).toBe('input-mode');
    expect(SLASH_COMMANDS.find((c) => c.command === '/diff')?.kind).toBe('navigation');
  });

  it('filters by command prefix', () => {
    expect(filterSlashCommands('bt').map((c) => c.command)).toEqual(['/btw']);
  });

  it('groups for the palette', () => {
    const groups = groupSlashCommands(SLASH_COMMANDS);
    expect(groups.map((g) => g.label)).toEqual([
      'RUN CONFIGURATION',
      'CONVERSATION',
      'WORKSPACE',
      'CONTEXT',
    ]);
  });

  it('maps each command to a GUI interaction, not a CLI rewrite', () => {
    expect(slashTarget('/model')).toEqual({ kind: 'selector', popup: 'model' });
    expect(slashTarget('/work-mode')).toEqual({ kind: 'selector', popup: 'work' });
    expect(slashTarget('/collab')).toEqual({ kind: 'selector', popup: 'collab' });
    expect(slashTarget('/perm')).toEqual({ kind: 'selector', popup: 'perm' });
    expect(slashTarget('/checkpoint')).toEqual({ kind: 'entity-picker', popup: 'checkpoint' });
    expect(slashTarget('/btw')).toEqual({ kind: 'input-mode' });
    expect(slashTarget('/diff')).toEqual({ kind: 'navigation', dest: 'diff' });
    expect(slashTarget('/memory')).toEqual({ kind: 'navigation', dest: 'memory' });
    expect(slashTarget('/compact')).toEqual({ kind: 'action', command: '/compact' });
    expect(slashTarget('/clear')).toEqual({ kind: 'action', command: '/clear' });
    expect(slashTarget('/cancel')).toEqual({ kind: 'action', command: '/cancel' });
    expect(slashTarget('/unknown')).toBeNull();
  });
});
