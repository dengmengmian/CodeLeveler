// 中栏时间线：文档式对话流 —— 用户消息（左侧细强调线引用块）+ Agent 正文（无卡片），
// 不显示身份名称/头像；工具调用不平铺，全部归入底部的轻量运行摘要（AgentRunBlock）。
// 滚动：在底部时跟随流式输出；用户上滚后立即停止跟随，悬浮提示累计新活动条数，
// 点击回到底部并恢复跟随；回合完成时不强制拉回，只更新提示。

import { useEffect, useRef, useState } from 'react';
import { useAppState, type ChatMessage } from '../state/store';
import { AgentRunBlock } from './AgentRunBlock';
import { ApprovalCard } from './ApprovalCard';
import { ClarificationCard } from './ClarificationCard';
import { CopyButton } from './CopyButton';
import { MessageBody } from './MessageBody';

function UserTurn({ m }: { m: ChatMessage }) {
  return (
    <div className="turn turn-user">
      <div className="message-user" title={m.time ?? undefined}>
        {m.text}
        {m.time && <time className="msg-time">{m.time}</time>}
      </div>
    </div>
  );
}

function AssistantTurn({ m }: { m: ChatMessage }) {
  return (
    <div className="turn turn-assistant">
      <div className="message-assistant">
        <MessageBody text={m.text} streaming={m.streaming} />
        {!m.streaming && m.text && (
          <div className="msg-foot">
            <CopyButton text={m.text} />
            {m.time && <time className="msg-time">{m.time}</time>}
          </div>
        )}
      </div>
    </div>
  );
}

// 旁问侧答：独立卡片，回显问题 + 答案，标注不打断主回合。
function BtwTurn({ m }: { m: ChatMessage }) {
  return (
    <div className="turn turn-btw">
      <div className="btw-card">
        <div className="btw-head">
          <span className="btw-badge">旁问</span>
          <span className="btw-q">{m.btw}</span>
          <span className="btw-note">不打断当前回合</span>
        </div>
        <div className="btw-body">
          <MessageBody text={m.text} streaming={m.streaming} />
        </div>
        {!m.streaming && m.text && (
          <div className="msg-foot">
            <CopyButton text={m.text} />
            {m.time && <time className="msg-time">{m.time}</time>}
          </div>
        )}
      </div>
    </div>
  );
}

export function Timeline() {
  const current = useAppState().current;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [atBottom, setAtBottom] = useState(true);
  const [newCount, setNewCount] = useState(0);
  const [donePing, setDonePing] = useState(false);
  const atBottomRef = useRef(true);
  const prevTurnActive = useRef(false);
  atBottomRef.current = atBottom;

  // 滚动跟随：仅当用户已在底部时，新内容才自动滚到底；
  // 不在底部则累计新活动条数，供悬浮提示展示。
  const messageCount = current?.messages.length ?? 0;
  const lastLen = current?.messages[messageCount - 1]?.text.length ?? 0;
  const toolCount = current?.tools.length ?? 0;
  const pendingCount =
    (current?.pendingApprovals.length ?? 0) + (current?.pendingClarifications.length ?? 0);
  const turnActive = current?.turnActive ?? false;

  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
    setAtBottom(bottom);
    if (bottom) {
      setNewCount(0);
      setDonePing(false);
    }
  };

  useEffect(() => {
    const el = scrollRef.current;
    const wasActive = prevTurnActive.current;
    prevTurnActive.current = turnActive;
    if (atBottomRef.current) {
      if (el) el.scrollTop = el.scrollHeight;
      return;
    }
    // 用户停留在历史位置：累计新活动；回合刚结束时给出「已完成」提示。
    setNewCount((n) => n + 1);
    if (wasActive && !turnActive) setDonePing(true);
  }, [messageCount, lastLen, toolCount, pendingCount, turnActive]);

  // 切换会话：回到底部并清空提示计数
  const sessionId = current?.id ?? null;
  useEffect(() => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    setAtBottom(true);
    setNewCount(0);
    setDonePing(false);
  }, [sessionId]);

  const scrollToBottom = () => {
    const el = scrollRef.current;
    if (el) el.scrollTop = el.scrollHeight;
    setAtBottom(true);
    setNewCount(0);
    setDonePing(false);
  };

  if (!current) {
    return (
      <div className="timeline" ref={scrollRef}>
        <div className="tl-inner">
          <div className="insp-empty">加载会话中…</div>
        </div>
      </div>
    );
  }

  const fabLabel = turnActive
    ? newCount > 0
      ? `↓ Agent 仍在运行 · ${newCount} 条新活动`
      : '↓ Agent 仍在运行'
    : donePing
      ? '↓ Agent 已完成 · 查看结果'
      : '↓ 回到底部';

  return (
    <div className="timeline" ref={scrollRef} onScroll={onScroll}>
      <div className="tl-inner">
        {current.messages.map((m) =>
          m.btw !== undefined ? (
            <BtwTurn key={m.id} m={m} />
          ) : m.role === 'user' ? (
            <UserTurn key={m.id} m={m} />
          ) : (
            <AssistantTurn key={m.id} m={m} />
          ),
        )}

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
          {fabLabel}
        </button>
      )}
    </div>
  );
}
