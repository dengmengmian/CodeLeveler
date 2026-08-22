// 中栏时间线：文档式对话流 —— 用户消息（左侧细强调线引用块）+ Agent 正文（无卡片），
// 不显示身份名称/头像；工具调用不平铺。过程留在该轮问题与回答之间，
// 「已回答」脚注在回答之后；上一轮过程冻结在原位，不随新问题消失。
// 滚动：在底部时跟随流式输出；用户上滚后立即停止跟随，悬浮提示累计新活动条数，
// 点击回到底部并恢复跟随；回合完成时不强制拉回，只更新提示。

import { useEffect, useRef, useState } from 'react';
import {
  assistantResultText,
  groupConversationTurns,
  layoutTimeline,
  type TimelineSlot,
} from '../lib/timelineLayout';
import { useAppState, type ChatMessage, type LastTurn, type TurnTrace } from '../state/store';
import { AgentRunBlock } from './AgentRunBlock';
import { CompactionSummary } from './CompactionSummary';
import { ApprovalCard } from './ApprovalCard';
import { ClarificationCard } from './ClarificationCard';
import { CopyButton } from './CopyButton';
import { MessageBody } from './MessageBody';

function UserTurn({ m }: { m: ChatMessage }) {
  return (
    <div className="turn turn-user">
      <div className="message-user" title={m.time ?? undefined}>
        {m.text}
      </div>
    </div>
  );
}

function AssistantTurn({ m }: { m: ChatMessage }) {
  return (
    <div className="turn turn-assistant">
      <div className="message-assistant">
        <MessageBody text={m.text} streaming={m.streaming} />
      </div>
    </div>
  );
}

function renderTurn(m: ChatMessage) {
  if (m.kind === 'compaction_summary') return <CompactionSummary key={m.id} text={m.text} />;
  if (m.btw !== undefined) return <BtwTurn key={m.id} m={m} />;
  if (m.role === 'user') return <UserTurn key={m.id} m={m} />;
  return <AssistantTurn key={m.id} m={m} />;
}

function renderSlot(
  slot: TimelineSlot,
  index: number,
  turnActive: boolean,
  lastTurn: LastTurn | null,
  traces: Map<number, TurnTrace>,
  copyText: string | null,
) {
  if (slot.kind === 'message') return renderTurn(slot.message);
  if (slot.kind === 'footer') {
    return (
      <AgentRunBlock
        key={`footer-${slot.userSeq}`}
        variant="footer"
        lastTurn={slot.live ? lastTurn : traces.get(slot.userSeq)?.lastTurn}
        copyText={copyText}
        live={slot.live}
      />
    );
  }
  if (slot.live && turnActive) {
    return <AgentRunBlock key={`live-${slot.userSeq}`} variant="live" />;
  }
  const tools = slot.live ? undefined : traces.get(slot.userSeq)?.tools;
  const backgroundTasks = slot.live ? undefined : traces.get(slot.userSeq)?.backgroundTasks;
  return (
    <AgentRunBlock
      key={`process-${slot.userSeq}-${index}`}
      variant="process"
      tools={tools}
      backgroundTasks={backgroundTasks}
    />
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
          {!m.streaming && m.text && <CopyButton text={m.text} className="copy-btn-compact" />}
        </div>
        <div className="btw-body">
          <MessageBody text={m.text} streaming={m.streaming} />
        </div>
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
  const reasoningLen = current?.reasoning.length ?? 0;
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
  }, [messageCount, lastLen, toolCount, reasoningLen, pendingCount, turnActive]);

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

  const traces = current.traces ?? [];
  const slots = layoutTimeline(current.messages, {
    turnActive: current.turnActive,
    hasLastTurn: Boolean(current.lastTurn) && current.pendingApprovals.length === 0 && current.pendingClarifications.length === 0,
    frozenProcessSeqs: traces.filter((t) => t.tools.length > 0 || t.backgroundTasks.length > 0).map((t) => t.userSeq),
    footerSeqs: traces.filter((t) => t.lastTurn).map((t) => t.userSeq),
  });
  const traceBySeq = new Map(traces.map((t) => [t.userSeq, t]));

  return (
    <div className="timeline" ref={scrollRef} onScroll={onScroll}>
      <div className="tl-inner">
        {groupConversationTurns(slots).map((turn) => {
          const copyText = assistantResultText(turn.items);
          return (
          <div className="conv-turn" key={`turn-${turn.userSeq}`} data-turn={turn.userSeq}>
            {turn.items.map((slot, i) =>
              renderSlot(slot, i, current.turnActive, current.lastTurn, traceBySeq, copyText),
            )}
          </div>
          );
        })}

        {current.pendingApprovals.map((a) => (
          <ApprovalCard key={a.id} request={a} variant="record" />
        ))}
        {current.pendingClarifications.map((c) => (
          <ClarificationCard key={c.id} request={c} variant="record" />
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
