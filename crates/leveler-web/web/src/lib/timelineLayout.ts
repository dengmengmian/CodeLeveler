// 时间线插位：Agent 运行摘要跟 TUI 一样，落在本轮用户问题之后、回答之前。
// 不改消息顺序，只切一刀给 Timeline 用。

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
