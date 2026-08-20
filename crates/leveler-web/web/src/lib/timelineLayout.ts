// 时间线插位：
// - 运行中：活过程落在本轮问题之后、回答之前。
// - 终态：过程仍留在问题与回答之间；「已回答」脚注在回答之后。
// - 上一轮的工具过程冻结在该轮问答里，不随新问题清空。

import type { ChatMessage } from '../state/store';

export function splitAroundCurrentTurn(messages: ChatMessage[]): {
  beforeRun: ChatMessage[];
  afterRun: ChatMessage[];
} {
  let lastUser = -1;
  for (let i = 0; i < messages.length; i++) {
    if (messages[i].role === 'user' && messages[i].btw === undefined) {
      lastUser = i;
    }
  }
  if (lastUser < 0) return { beforeRun: messages, afterRun: [] };
  return {
    beforeRun: messages.slice(0, lastUser + 1),
    afterRun: messages.slice(lastUser + 1),
  };
}

export type TimelineSlot =
  | { kind: 'message'; message: ChatMessage }
  | { kind: 'process'; userSeq: number; live: boolean }
  | { kind: 'footer'; userSeq: number; live: boolean };

export function layoutTimeline(
  messages: ChatMessage[],
  opts: {
    turnActive: boolean;
    hasLastTurn: boolean;
    frozenProcessSeqs: readonly number[];
  },
): TimelineSlot[] {
  let lastUserIdx = -1;
  for (let i = 0; i < messages.length; i++) {
    if (messages[i].role === 'user' && messages[i].btw === undefined) lastUserIdx = i;
  }
  const frozen = new Set(opts.frozenProcessSeqs);
  const slots: TimelineSlot[] = [];
  let pendingFooter: TimelineSlot | null = null;

  const flushFooter = () => {
    if (pendingFooter) {
      slots.push(pendingFooter);
      pendingFooter = null;
    }
  };

  for (let i = 0; i < messages.length; i++) {
    const m = messages[i];
    if (m.role === 'user' && m.btw === undefined) flushFooter();
    slots.push({ kind: 'message', message: m });
    if (m.role !== 'user' || m.btw !== undefined) continue;

    const isCurrent = i === lastUserIdx;
    if (isCurrent) {
      slots.push({ kind: 'process', userSeq: m.seq, live: true });
      if (!opts.turnActive && opts.hasLastTurn) {
        pendingFooter = { kind: 'footer', userSeq: m.seq, live: true };
      }
    } else if (frozen.has(m.seq)) {
      slots.push({ kind: 'process', userSeq: m.seq, live: false });
    }
  }
  flushFooter();
  return slots;
}

/** One conversation turn: prompt + execution + result. Spacing owner in the DOM. */
export interface ConversationTurn {
  userSeq: number;
  items: TimelineSlot[];
}

export function groupConversationTurns(slots: readonly TimelineSlot[]): ConversationTurn[] {
  const turns: ConversationTurn[] = [];
  for (const slot of slots) {
    const startsTurn =
      slot.kind === 'message' && slot.message.role === 'user' && slot.message.btw === undefined;
    if (startsTurn) {
      turns.push({ userSeq: slot.message.seq, items: [slot] });
      continue;
    }
    const current = turns[turns.length - 1];
    if (current) {
      current.items.push(slot);
    } else {
      turns.push({ userSeq: -1, items: [slot] });
    }
  }
  return turns;
}
