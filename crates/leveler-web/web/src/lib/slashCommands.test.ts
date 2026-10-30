import { describe, expect, it } from 'vitest';
import {
  SLASH_COMMANDS,
  filterSlashCommands,
  groupSlashCommands,
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


});
