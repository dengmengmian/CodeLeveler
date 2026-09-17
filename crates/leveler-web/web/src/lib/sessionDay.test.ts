import { describe, expect, it } from 'vitest';
import { dayBucket, groupByDay } from './sessionDay';

describe('session day buckets', () => {
  const noon = new Date(2026, 7, 19, 12, 0, 0).getTime();

  it('splits today / yesterday / earlier on local midnight', () => {
    expect(dayBucket(new Date(2026, 7, 19, 8, 0, 0).toISOString(), noon)).toBe('today');
    expect(dayBucket(new Date(2026, 7, 18, 23, 0, 0).toISOString(), noon)).toBe('yesterday');
    expect(dayBucket(new Date(2026, 7, 17, 12, 0, 0).toISOString(), noon)).toBe('earlier');
  });

  it('groups preserving input order inside each bucket', () => {
    const rows = [
      { id: 'a', updated_at: new Date(2026, 7, 19, 10, 0, 0).toISOString() },
      { id: 'b', updated_at: new Date(2026, 7, 18, 10, 0, 0).toISOString() },
      { id: 'c', updated_at: new Date(2026, 7, 19, 11, 0, 0).toISOString() },
    ];
    const grouped = groupByDay(rows, noon);
    expect(grouped.map((g) => g.bucket)).toEqual(['today', 'yesterday']);
    expect(grouped[0].items.map((x) => x.id)).toEqual(['a', 'c']);
  });
});
