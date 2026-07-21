// 中栏时间线：用户/助手消息 + 工具轨 + 审批/澄清卡。
// 不做每轮 TURN 分割——回合边界由消息本身的角色标签表达，避免切断阅读流。

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { useAppState, type ChatMessage, type ToolCallView } from '../state/store';
import { ApprovalCard } from './ApprovalCard';
import { ClarificationCard } from './ClarificationCard';
import { MessageBody } from './MessageBody';
import { ToolCallRow } from './ToolCallRow';

export function Timeline() {
  const current = useAppState().current;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(new Set());
  const [copiedId, setCopiedId] = useState<string | null>(null);

  const toggleCollapsed = (id: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };

  const copyMessage = (m: ChatMessage) => {
    void navigator.clipboard.writeText(m.text);
    setCopiedId(m.id);
    setTimeout(() => setCopiedId((cur) => (cur === m.id ? null : cur)), 1500);
  };

  // 新内容自动滚到底
  const messageCount = current?.messages.length ?? 0;
  const lastLen = current?.messages[messageCount - 1]?.text.length ?? 0;
  const toolCount = current?.tools.length ?? 0;
  const pendingCount =
    (current?.pendingApprovals.length ?? 0) + (current?.pendingClarifications.length ?? 0);
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [messageCount, lastLen, toolCount, pendingCount]);

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
              <button onClick={() => copyMessage(m)}>{copiedId === m.id ? '已复制' : '复制'}</button>
              <button onClick={() => toggleCollapsed(m.id)}>{isCollapsed ? '展开' : '收起'}</button>
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
    <div className="timeline" ref={scrollRef}>
      <div className="tl-inner">
        {rows}

        {current.pendingApprovals.map((a) => (
          <ApprovalCard key={a.id} request={a} />
        ))}
        {current.pendingClarifications.map((c) => (
          <ClarificationCard key={c.id} request={c} />
        ))}
      </div>
    </div>
  );
}
