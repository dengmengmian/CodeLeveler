import { describe, expect, it } from 'vitest';
import {
  nextPath,
  numberPatch,
  prevPath,
  shouldCollapsePatch,
} from './diffWorkspace';

const FILES = ['src/auth.rs', 'src/api.rs', 'tests/login.rs'];

describe('file navigation', () => {
  it('selects next and wraps at the end', () => {
    expect(nextPath(FILES, 'src/auth.rs')).toBe('src/api.rs');
    expect(nextPath(FILES, 'tests/login.rs')).toBe('src/auth.rs');
  });

  it('selects previous and wraps at the start', () => {
    expect(prevPath(FILES, 'src/auth.rs')).toBe('tests/login.rs');
    expect(prevPath(FILES, 'src/api.rs')).toBe('src/auth.rs');
  });

  it('empty list is a no-op', () => {
    expect(nextPath([], null)).toBeNull();
    expect(prevPath([], 'x')).toBeNull();
  });
});

describe('numberPatch', () => {
  it('assigns old/new line numbers and marks hunks', () => {
    const lines = numberPatch(
      [
        'diff --git a/a.rs b/a.rs',
        '@@ -10,3 +10,4 @@ fn main() {',
        ' keep',
        '-old',
        '+new',
        ' tail',
      ].join('\n'),
    );
    const hunk = lines.find((l) => l.kind === 'hunk');
    expect(hunk?.text).toContain('@@');
    const add = lines.find((l) => l.kind === 'add');
    const del = lines.find((l) => l.kind === 'del');
    const ctx = lines.filter((l) => l.kind === 'ctx');
    expect(add?.newNo).toBeTypeOf('number');
    expect(del?.oldNo).toBeTypeOf('number');
    expect(ctx.length).toBe(2);
  });
});

describe('large patch collapse', () => {
  it('collapses only large patches', () => {
    expect(shouldCollapsePatch(20)).toBe(false);
    expect(shouldCollapsePatch(500)).toBe(true);
  });
});
