import { beforeEach, describe, expect, it } from 'vitest';
import { loadLastSession, saveLastSession } from './lastSession';

const store = new Map<string, string>();

beforeEach(() => {
  store.clear();
  const g = globalThis as Record<string, unknown>;
  g.localStorage = {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => {
      store.set(key, value);
    },
    removeItem: (key: string) => {
      store.delete(key);
    },
  };
});

describe('last session persistence', () => {
  it('round-trips a session id', () => {
    expect(loadLastSession()).toBeNull();
    saveLastSession('sess-1');
    expect(loadLastSession()).toBe('sess-1');
  });

  it('clears on new task', () => {
    saveLastSession('sess-1');
    saveLastSession(null);
    expect(loadLastSession()).toBeNull();
  });
});
