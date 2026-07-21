// 中栏时间线：用户/助手消息 + 工具轨 + 审批/澄清卡 + 对话内运行状态块。
// 不做每轮 TURN 分割——回合边界由消息本身的角色标签表达，避免切断阅读流。
// 滚动：在底部时跟随流式输出；用户上滚后停止跟随并显示回到底部的悬浮按钮。

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useAppState, type ChatMessage, type ToolCallView } from '../state/store';
import { AgentRunBlock, useElapsedSeconds } from './AgentRunBlock';
import { ApprovalCard } from './ApprovalCard';
import { ClarificationCard } from './ClarificationCard';
import { CopyButton } from './CopyButton';
import { MessageBody } from './MessageBody';
import { ToolCallRow } from './ToolCallRow';

export function Timeline() {
  const current = useAppState().current;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [atBottom, setAtBottom] = useState(true);

  const toggleCollapsed = (id: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  // 滚动跟随：仅当用户已在底部时，新内容才自动滚到底。
  const messageCount = current?.messages.length ?? 0;
  const lastLen = current?.messages[messageCount - 1]?.text.length ?? 0;
  const toolCount = current?.tools.length ?? 0;
  const pendingCount =
    (current?.pendingApprovals.length ?? 0) + (current?.pendingClarifications.length ?? 0);
  const turnActive = current?.turnActive ?? false;

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    setAtBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 48);
  };

  useEffect(() => {
    const el = scrollRef.current;
    if (el && atBottom) el.scrollTop = el.scrollHeight;
  }, [messageCount, lastLen, toolCount, pendingCount, turnActive, atBottom]);

  const scrollToBottom = () => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    setAtBottom(true);
  };

  const elapsed = useElapsedSeconds(current?.turnStartedAt ?? null, turnActive);

  if (!current) {
    return (
      <div className="timeline" ref={scrollRef}>
        <div className="tl-inner">
          <div className="insp-empty">加载会话中…</div>
        </div>
      </div>
    );
  }

  // Merge messages and tool calls into one chronological stream (by `seq`) so a
  // turn's exploration tools sit where they happened — before the summary that
  // followed — instead of piling into one block at the very bottom.
  type Row =
    | { kind: 'msg'; seq: number; m: ChatMessage }
    | { kind: 'tool'; seq: number; t: ToolCallView };
  const stream: Row[] = [
    ...current.messages.map((m): Row => ({ kind: 'msg', seq: m.seq, m })),
    ...current.tools.map((t): Row => ({ kind: 'tool', seq: t.seq, t })),
  ].sort((a, b) => a.seq - b.seq);

  const rows: ReactNode[] = [];
  let toolBuffer: ToolCallView[] = [];
  const flushTools = () => {
    if (toolBuffer.length === 0) return;
    rows.push(
      <div className="tools" key={`tools-${toolBuffer[0].id}`}>
        {toolBuffer.map((t) => (
          <ToolCallRow key={t.id} tool={t} />
        ))}
      </div>,
    );
    toolBuffer = [];
  };
  for (const row of stream) {
    if (row.kind === 'tool') {
      toolBuffer.push(row.t);
      continue;
    }
    flushTools(); // consecutive tools render as one grouped track
    const m = row.m;
    const isUser = m.role === 'user';
    const isCollapsed = collapsed.has(m.id);
    rows.push(
      <div className={`msg ${isUser ? 'user' : 'assistant'}`} key={m.id}>
        <div className="who">
          {isUser ? '你' : 'Leveler'}
          {m.time && <time>{m.time}</time>}
          {!isUser && (
            <span className="msg-actions">
              <CopyButton text={m.text} />
              <button className="msg-fold" onClick={() => toggleCollapsed(m.id)}>
                {isCollapsed ? '展开' : '收起'}
              </button>
            </span>
          )}
        </div>
        {isCollapsed ? null : isUser ? (
          <div className="body">{m.text}</div>
        ) : (
          <MessageBody text={m.text} streaming={m.streaming} />
        )}
      </div>,
    );
  }
  flushTools();

  return (
    <div className="timeline" ref={scrollRef} onScroll={onScroll}>
      <div className="tl-inner">
        {rows}

        <AgentRunBlock />

        {current.pendingApprovals.map((a) => (
          <ApprovalCard key={a.id} request={a} />
        ))}
        {current.pendingClarifications.map((c) => (
          <ClarificationCard key={c.id} request={c} />
        ))}
      </div>

      {!atBottom && (
        <button className="scroll-fab" onClick={scrollToBottom}>
          ↓ {turnActive ? `Agent 运行中 · ${elapsed}s` : '查看最新'}
        </button>
      )}
    </div>
  );
}
