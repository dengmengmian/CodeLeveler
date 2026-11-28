// The pending-state machine for one memory management action.
//
// The runtime answers a memory command with exactly one `MemoryActionFinished`.
// A batch Dream may legitimately run for the store's 30-minute budget, so a
// client that has waited longer than `SETTLE_MS` must call the outcome UNKNOWN:
// it may not claim success, and it may not cancel or resend the command. A real
// answer that arrives late is still the answer.
//
// This module owns only that decision. It has no timers and no DOM so the
// boundary can be tested with a simulated clock: a shortened timeout is
// deterministic time simulation, not a fabricated real 35-minute wait.

export const MEMORY_ACTION_SETTLE_MS = 35 * 60 * 1000;

/** How many settled query ids are remembered to reject a duplicate answer. */
const SETTLED_MEMORY = 32;

export function createMemoryActionTracker({
  now = () => Date.now(),
  settleMs = MEMORY_ACTION_SETTLE_MS,
} = {}) {
  let pending = null;
  const settled = new Set();

  const remember = queryId => {
    settled.add(queryId);
    if (settled.size > SETTLED_MEMORY) {
      // Sets preserve insertion order; dropping the oldest keeps the window
      // bounded without allowing a duplicate from the recent past.
      settled.delete(settled.values().next().value);
    }
  };

  return {
    /** Start waiting for one action. Returns null while another is pending. */
    begin(queryId, label, at = now()) {
      if (pending) return null;
      pending = { queryId, label, startedAt: at };
      return { ...pending };
    },
    /** Whether the pending action has crossed the unknown-result bound. */
    expired(at = now()) {
      return !!pending && at - pending.startedAt >= settleMs;
    },
    /** The pending action, without changing state. */
    current() {
      return pending ? { ...pending } : null;
    },
    /**
     * Take the pending action for `queryId`.
     *
     * - `null`: no pending action and no prior answer for this id (the caller
     *   may still display an unmatched real result once).
     * - `{duplicate: true}`: this id already settled; a repeated answer must
     *   not be presented as a second outcome.
     * - `{label, startedAt, expired}`: the awaited outcome. `expired` says the
     *   client had already reported UNKNOWN; the result is still real.
     */
    settle(queryId, at = now()) {
      if (settled.has(queryId)) return { duplicate: true };
      if (!pending || pending.queryId !== queryId) return null;
      const entry = { ...pending, expired: at - pending.startedAt >= settleMs };
      remember(queryId);
      pending = null;
      return entry;
    },
    /** Drop the pending action (session switch, connection loss). */
    drop() {
      const label = pending?.label ?? null;
      pending = null;
      return label;
    },
  };
}
