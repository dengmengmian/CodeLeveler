// 对话内 Agent 运行状态：从现有会话视图派生（无需新增后端契约——
// approval / tool / streaming / activity / lastTurn 已经承载了阶段信息）。
// 顶部全局状态栏之外，这是对话流内的主要运行反馈来源。

import type { SessionView } from '../state/store';

export type AgentRunState =
  | 'queued'
  | 'thinking'
  | 'planning'
  | 'searching'
  | 'reading'
  | 'tool_running'
  | 'waiting_approval'
  | 'generating'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface RunView {
  state: AgentRunState;
  /** 主状态文案（如「正在生成回答」） */
  primary: string;
  /** 当前具体动作，可空（如「正在读取 docs/target-architecture.md」） */
  detail: string | null;
  /** 是否终态（完成/失败/取消）——终态不再转圈 */
  terminal: boolean;
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
  if (s.pendingApprovals.length > 0) {
    return { state: 'waiting_approval', primary: '等待你的确认', detail: null, terminal: false };
  }
  if (s.pendingClarifications.length > 0) {
    return { state: 'waiting_approval', primary: '等待你的补充', detail: null, terminal: false };
  }

  if (s.turnActive) {
    const streaming = s.messages.find((m) => m.role === 'assistant' && m.streaming);
    if (streaming && streaming.text.length > 0) {
      return { state: 'generating', primary: '正在生成回答', detail: null, terminal: false };
    }

    const runningTool = s.tools.find((t) => t.status === 'run');
    if (runningTool) {
      const { state, label } = classifyTool(runningTool.name);
      return { state, primary: label, detail: toolTarget(runningTool.arguments), terminal: false };
    }

    if (s.plan && s.plan.steps.some((st) => st.status === 'running')) {
      const step = s.plan.steps.find((st) => st.status === 'running');
      return {
        state: 'planning',
        primary: '正在执行计划',
        detail: step ? truncate(step.description) : null,
        terminal: false,
      };
    }

    // agent_activity 的 label 本身是人类可读的阶段描述，优先直接展示。
    if (s.activity) {
      return { state: 'thinking', primary: s.activity, detail: null, terminal: false };
    }
    return { state: 'thinking', primary: '正在思考', detail: null, terminal: false };
  }

  if (s.lastTurn) {
    const sec = Math.round(s.lastTurn.ms / 1000);
    if (s.lastTurn.outcome === 'failed') {
      return { state: 'failed', primary: '执行失败', detail: s.lastTurn.error, terminal: true };
    }
    if (s.lastTurn.outcome === 'cancelled') {
      return { state: 'cancelled', primary: '已停止', detail: null, terminal: true };
    }
    return { state: 'completed', primary: `已完成 · 用时 ${sec}s`, detail: null, terminal: true };
  }

  return null;
}
