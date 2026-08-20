import { describe, expect, it } from 'vitest';
import { SIDEBAR_NAV } from './railNav';

describe('sidebar primary navigation', () => {
  it('keeps global destinations only: Conversations, Files, Search', () => {
    expect(SIDEBAR_NAV.map((i) => i.id)).toEqual(['sessions', 'files', 'search']);
    expect(SIDEBAR_NAV.map((i) => i.label)).toEqual(['Conversations', 'Files', 'Search']);
  });

  it('does not duplicate Changes or Activity in the sidebar', () => {
    const ids = SIDEBAR_NAV.map((i) => i.id);
    expect(ids).not.toContain('changes');
    expect(ids).not.toContain('activity');
    expect(ids).not.toContain('workspace');
  });

  it('does not attach Unicode glyphs to nav items', () => {
    for (const item of SIDEBAR_NAV) {
      expect(item).not.toHaveProperty('glyph');
      expect(JSON.stringify(item)).not.toMatch(/[◉▣⌕⑂◎⚙]/);
    }
  });
});

