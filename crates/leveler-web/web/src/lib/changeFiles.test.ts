import { describe, expect, it } from 'vitest';
import { groupChangeFiles, inferChangeKind, linkedChangePath } from './changeFiles';

describe('inferChangeKind', () => {
  it('reads /dev/null markers from the existing patch, not a new protocol field', () => {
    expect(
      inferChangeKind({
        path: 'new.rs',
        added: 3,
        removed: 0,
        patch: '--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,3 @@\n+a\n',
      }),
    ).toBe('added');
    expect(
      inferChangeKind({
        path: 'gone.rs',
        added: 0,
        removed: 2,
        patch: '--- a/gone.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n',
      }),
    ).toBe('deleted');
    expect(
      inferChangeKind({ path: 'edit.rs', added: 1, removed: 1, patch: '--- a/edit.rs\n+++ b/edit.rs\n' }),
    ).toBe('modified');
  });
});

describe('groupChangeFiles', () => {
  it('splits the same UiDiff.files list', () => {
    const g = groupChangeFiles([
      { path: 'a.rs', added: 1, removed: 0, patch: '--- /dev/null\n+++ b/a.rs\n' },
      { path: 'b.rs', added: 1, removed: 1, patch: '--- a/b.rs\n+++ b/b.rs\n' },
    ]);
    expect(g.added.map((f) => f.path)).toEqual(['a.rs']);
    expect(g.modified.map((f) => f.path)).toEqual(['b.rs']);
    expect(g.deleted).toEqual([]);
  });
});

describe('linkedChangePath', () => {
  it('jumps only with an identity match in current diff files', () => {
    const files = ['src/auth.rs', 'src/lib.rs'];
    expect(linkedChangePath('src/auth.rs', files)).toBe('src/auth.rs');
    expect(linkedChangePath('auth.rs', files)).toBe('src/auth.rs');
    expect(linkedChangePath('src', files)).toBeNull();
    expect(linkedChangePath('missing.rs', files)).toBeNull();
  });
});
