import { describe, expect, it } from 'vitest';
import type { ChatMessage } from '../state/store';
import { groupConversationTurns, layoutTimeline, splitAroundCurrentTurn } from './timelineLayout';

function msg(
  role: ChatMessage['role'],
  text: string,
  extra: Partial<ChatMessage> = {},
): ChatMessage {
  return { id: text, role, text, streaming: false, time: null, seq: extra.seq ?? 0, ...extra };
}

describe('splitAroundCurrentTurn', () => {
  it('places the live run summary after the latest user prompt, before the answer', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([
      msg('user', 'q1', { seq: 1 }),
      msg('assistant', 'a1', { seq: 2 }),
      msg('user', 'q2', { seq: 3 }),
      msg('assistant', 'a2', { seq: 4 }),
    ]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q1', 'a1', 'q2']);
    expect(afterRun.map((m) => m.text)).toEqual(['a2']);
  });

  it('keeps the run summary after the prompt when there is no answer yet', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([msg('user', 'q', { seq: 1 })]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q']);
    expect(afterRun).toEqual([]);
  });

  it('does not treat a btw side-answer as the current prompt', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([
      msg('user', 'q', { seq: 1 }),
      msg('assistant', 'side', { btw: '顺便问', seq: 2 }),
      msg('assistant', 'answer', { seq: 3 }),
    ]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q']);
    expect(afterRun.map((m) => m.text)).toEqual(['side', 'answer']);
  });
});

describe('layoutTimeline', () => {
  it('keeps a finished turn process between its question and answer, footer after the answer', () => {
    const slots = layoutTimeline(
      [
        msg('user', 'q1', { seq: 1 }),
        msg('assistant', 'a1', { seq: 2 }),
        msg('user', 'q2', { seq: 3 }),
        msg('assistant', 'a2', { seq: 4 }),
      ],
      { turnActive: false, hasLastTurn: true, frozenProcessSeqs: [1, 3] },
    );
    expect(slots.map((s) => (s.kind === 'message' ? s.message.text : `${s.kind}:${s.userSeq}`))).toEqual([
      'q1',
      'process:1',
      'a1',
      'q2',
      'process:3',
      'a2',
      'footer:3',
    ]);
  });

  it('does not drop a previous turn process when a new prompt arrives', () => {
    const slots = layoutTimeline(
      [
        msg('user', '这是什么玩意儿', { seq: 1 }),
        msg('assistant', '我来看看这个仓库里到底是什么。', { seq: 2 }),
        msg('assistant', '这是 CodeLeveler', { seq: 3 }),
        msg('user', '你好', { seq: 4 }),
        msg('assistant', '我在呢', { seq: 5 }),
      ],
      { turnActive: false, hasLastTurn: true, frozenProcessSeqs: [1] },
    );
    const kinds = slots.map((s) =>
      s.kind === 'message' ? s.message.text : `${s.kind}:${s.userSeq}${s.live ? ':live' : ''}`,
    );
    expect(kinds).toEqual([
      '这是什么玩意儿',
      'process:1',
      '我来看看这个仓库里到底是什么。',
      '这是 CodeLeveler',
      '你好',
      'process:4:live',
      '我在呢',
      'footer:4:live',
    ]);
  });

  it('while running, only the live process sits after the current prompt', () => {
    const slots = layoutTimeline(
      [msg('user', 'q', { seq: 1 }), msg('assistant', 'a', { seq: 2 })],
      { turnActive: true, hasLastTurn: false, frozenProcessSeqs: [] },
    );
    expect(slots.map((s) => (s.kind === 'message' ? s.message.text : s.kind))).toEqual([
      'q',
      'process',
      'a',
    ]);
  });
});

describe('groupConversationTurns', () => {
  it('wraps User / process / Assistant into one turn, next User starts the next', () => {
    const slots = layoutTimeline(
      [
        msg('user', 'q1', { seq: 1 }),
        msg('assistant', 'a1', { seq: 2 }),
        msg('user', 'q2', { seq: 3 }),
        msg('assistant', 'a2', { seq: 4 }),
      ],
      { turnActive: false, hasLastTurn: true, frozenProcessSeqs: [1, 3] },
    );
    const turns = groupConversationTurns(slots);
    expect(turns).toHaveLength(2);
    expect(turns[0].userSeq).toBe(1);
    expect(turns[1].userSeq).toBe(3);
    expect(turns[0].items.map((s) => (s.kind === 'message' ? s.message.text : s.kind))).toEqual([
      'q1',
      'process',
      'a1',
    ]);
    expect(turns[1].items.map((s) => (s.kind === 'message' ? s.message.text : s.kind))).toEqual([
      'q2',
      'process',
      'a2',
      'footer',
    ]);
  });

  it('keeps AgentRunBlock between the current User and Assistant inside the same turn', () => {
    const slots = layoutTimeline(
      [msg('user', '你好', { seq: 1 }), msg('assistant', '我在', { seq: 2 })],
      { turnActive: true, hasLastTurn: false, frozenProcessSeqs: [] },
    );
    const [turn] = groupConversationTurns(slots);
    expect(turn.items.map((s) => s.kind)).toEqual(['message', 'process', 'message']);
  });
});
