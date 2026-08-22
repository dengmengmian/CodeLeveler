/** Session list day buckets. Calendar-local, not rolling 24h. */

export type DayBucket = 'today' | 'yesterday' | 'earlier';

export function dayBucket(iso: string, nowMs: number = Date.now()): DayBucket {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return 'earlier';
  const now = new Date(nowMs);
  const startToday = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
  const startYesterday = startToday - 86_400_000;
  if (t >= startToday) return 'today';
  if (t >= startYesterday) return 'yesterday';
  return 'earlier';
}

export function dayLabel(bucket: DayBucket): string {
  switch (bucket) {
    case 'today':
      return 'Today';
    case 'yesterday':
      return 'Yesterday';
    case 'earlier':
      return 'Earlier';
  }
}

const ORDER: DayBucket[] = ['today', 'yesterday', 'earlier'];

export function groupByDay<T extends { updated_at: string }>(
  items: readonly T[],
  nowMs: number = Date.now(),
): Array<{ bucket: DayBucket; label: string; items: T[] }> {
  const bags: Record<DayBucket, T[]> = { today: [], yesterday: [], earlier: [] };
  for (const item of items) {
    bags[dayBucket(item.updated_at, nowMs)].push(item);
  }
  return ORDER.filter((b) => bags[b].length > 0).map((bucket) => ({
    bucket,
    label: dayLabel(bucket),
    items: bags[bucket],
  }));
}
