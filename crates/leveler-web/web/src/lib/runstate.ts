// 对话内 Agent 运行状态：从现有会话视图派生（无需新增后端契约——
// approval / tool / streaming / activity / lastTurn 已经承载了阶段信息）。
// 顶部全局状态栏之外，这是对话流内的主要运行反馈来源。
// 终态呈现走 lib/turn.ts 的 presentTurnEnd —— Turn Truth 的唯一映射点。

import type { SessionView } from '../state/store';
import { presentTurnEnd, type TurnTone } from './turn';

export type AgentRunState =
  | 'queued'
  | 'thinking'
  | 'planning'
  | 'searching'
  | 'reading'
  | 'tool_running'
  | 'waiting_approval'
  | 'generating'
  | 'terminal';

export interface RunView {
  state: AgentRunState;
  /** 主状态文案（如「正在生成回答」） */
  primary: string;
  /** 当前具体动作 / 终态 reason，可空 */
  detail: string | null;
  /** 是否终态——终态不再转圈 */
  terminal: boolean;
  /** 终态语气（success/calm/warn/error/muted）；运行中为 null */
  tone: TurnTone | null;
  /** 终态符号（✓ ⚠ ✕ ■ ◇）；运行中为 null */
  glyph: string | null;
  /** 终态 outcome（重试按钮等交互用）；运行中为 null */
  outcome: import('./turn').TurnOutcome | null;
}

function truncate(text: string, max = 72): string {
  const t = text.trim();
  return t.length > max ? `${t.slice(0, max - 1)}…` : t;
}

/** 从工具参数里抽一个可读的目标（多为 JSON，取 path/pattern/command 字段）。 */
function toolTarget(args: string): string | null {
  try {
    const obj = JSON.parse(args) as Record<string, unknown>;
    const key = ['path', 'file', 'pattern', 'query', 'command', 'cmd'].find(
      (k) => typeof obj[k] === 'string',
    );
    if (key) return truncate(String(obj[key]));
  } catch {
    // 非 JSON：直接截断原文
  }
  return args ? truncate(args) : null;
}

/** 运行中的工具名归类到 reading / searching / tool_running。 */
function classifyTool(name: string): { state: AgentRunState; label: string } {
  const n = name.toLowerCase();
  if (/read|cat|open|view/.test(n)) return { state: 'reading', label: '正在读取文件' };
  if (/search|grep|find|glob|list/.test(n)) return { state: 'searching', label: '正在搜索代码' };
  return { state: 'tool_running', label: `正在执行 ${name}` };
}

/**
 * 派生当前应展示的运行状态；返回 null 表示无需展示（纯空闲）。
 */
export function deriveRunState(s: SessionView): RunView | null {
  const live = (state: AgentRunState, primary: string, detail: string | null): RunView => ({
    state,
    primary,
    detail,
    terminal: false,
    tone: null,
    glyph: null,
    outcome: null,
  });

  if (s.pendingApprovals.length > 0) {
    return live('waiting_approval', '等待你的确认', null);
  }
  if (s.pendingClarifications.length > 0) {
    return live('waiting_approval', '等待你的补充', null);
  }

  if (s.turnActive) {
    const streaming = s.messages.find((m) => m.role === 'assistant' && m.streaming);
    if (streaming && streaming.text.length > 0) {
      return live('generating', '正在生成回答', null);
    }

    const runningTool = s.tools.find((t) => t.status === 'run');
    if (runningTool) {
      const { state, label } = classifyTool(runningTool.name);
      return live(state, label, toolTarget(runningTool.arguments));
    }

    // 计划步骤不在中间区域重复展示（右侧面板负责完整执行计划）。

    // agent_activity 是阶段标题；完整推理走 ReasoningDisclosure，不塞进 detail。
    if (s.activity) {
      return live('thinking', s.activity, null);
    }
    return live('thinking', '正在思考', null);
  }

  if (s.lastTurn) {
    // 7 个终态逐一保真：incomplete/unverified 有自己的语气与 reason，
    // 绝不显示成绿色「已完成」。
    const p = presentTurnEnd(s.lastTurn);
    const sec = Math.round(s.lastTurn.ms / 1000);
    const primary = sec > 0 && p.tone === 'success' ? `${p.label} · 用时 ${sec}s` : p.label;
    return {
      state: 'terminal',
      primary,
      detail: p.detail,
      terminal: true,
      tone: p.tone,
      glyph: p.glyph,
      outcome: s.lastTurn.outcome,
    };
  }

  return null;
}
