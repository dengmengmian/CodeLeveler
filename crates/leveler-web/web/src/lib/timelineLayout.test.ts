import { describe, expect, it } from 'vitest';
import type { ChatMessage } from '../state/store';
import { splitAroundCurrentTurn } from './timelineLayout';

function msg(
  role: ChatMessage['role'],
  text: string,
  extra: Partial<ChatMessage> = {},
): ChatMessage {
  return { id: text, role, text, streaming: false, time: null, seq: 0, ...extra };
}

describe('splitAroundCurrentTurn', () => {
  it('places the run summary after the latest user prompt, before the answer', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([
      msg('user', 'q1'),
      msg('assistant', 'a1'),
      msg('user', 'q2'),
      msg('assistant', 'a2'),
    ]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q1', 'a1', 'q2']);
    expect(afterRun.map((m) => m.text)).toEqual(['a2']);
  });

  it('keeps the run summary after the prompt when there is no answer yet', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([msg('user', 'q')]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q']);
    expect(afterRun).toEqual([]);
  });

  it('does not treat a btw side-answer as the current prompt', () => {
    const { beforeRun, afterRun } = splitAroundCurrentTurn([
      msg('user', 'q'),
      msg('assistant', 'side', { btw: '顺便问' }),
      msg('assistant', 'answer'),
    ]);
    expect(beforeRun.map((m) => m.text)).toEqual(['q']);
    expect(afterRun.map((m) => m.text)).toEqual(['side', 'answer']);
  });
});
