// Execution presentation projection — the Web implementation of the
// execution presentation contract v1.
//
// One pure function per contract invariant, so rendering and the conformance
// test read the SAME projection. Nothing here invents a boundary: the round is
// the runtime's `model_step`, the batch is an observed overlap, and the
// Final/Progress split is decided by event order (never by reading prose).
//
// Shared packaging notes:
// - The Final/Progress split is decided by event order plus the runtime's
//   stated `answerEffect` on the call (never by reading prose, and never by
//   classifying the tool name: `leveler-tools` owns that classification and the
//   runtime stamps it on the wire).
// - Runtime notes (`[execution policy]`, `[mutation rejected]`, …) state HOW a
//   command ran; they are never WHY it failed (C8).

import { isTurnUser } from './presentationKind';
import type { ChatMessage, ToolCallView } from '../state/store';
/** Tool lifecycle — the frozen five-way reading of a call. */
export type ToolStatus = ToolCallView['status'];

/** A settled round's most severe outcome; `ok` requires every call Ok. */
export type RoundStatus = 'running' | 'ok' | 'failed' | 'cancelled' | 'unknown';

export interface ExecutionRoundView {
  /** The real `model_step`; null only for a legacy transcript without one. */
  modelStep: number | null;
  tools: ToolCallView[];
  status: RoundStatus;
  /** True only when EVERY visible call is `done` (Contract v1 §I7). */
  allOk: boolean;
  /** Observed concurrent bursts inside this round, in first-seen order. */
  batches: string[][];
}

export type ProjectedItem =
  | { kind: 'assistant_text'; text: string; seq: number }
  | { kind: 'final_answer'; text: string; seq: number }
  | { kind: 'execution_round'; round: ExecutionRoundView; seq: number };

/**
 * Runtime rows about HOW a command ran, never why it failed (Contract §I8).
 */
const RUNTIME_NOTE_TAGS: readonly string[] = [
  '[execution policy] ',
  '[mutation rejected] ',
  '[note] ',
];

export function isRuntimeNote(line: string): boolean {
  const trimmed = line.trimEnd();
  if (trimmed.startsWith('exit: ')) return true;
  if (trimmed.startsWith('--- ') && trimmed.endsWith(' ---')) return true;
  if (trimmed === '[timed out]') return true;
  if (trimmed.startsWith('[timed out after ') && trimmed.endsWith(']')) return true;
  return RUNTIME_NOTE_TAGS.some((tag) => trimmed.startsWith(tag));
}

/** The command's own output, with the runtime's execution rows removed. */
export function commandOutputBody(preview: string): string[] {
  return preview
    .split('\n')
    .map((line) => line.trimEnd())
    .filter((line) => line.trim() !== '' && !isRuntimeNote(line));
}

/**
 * The line that says what actually failed: the first line reporting a failure
 * (`error…`, `FAIL`, `✗`, `panic`), else the first thing the command printed —
 * never a runtime note.
 */
export function failureReason(preview: string): string | null {
  const lines = commandOutputBody(preview);
  const failure = lines.find((line) => /^(error|fail|✗|panic)/i.test(line.trim()));
  return failure ?? lines[0] ?? null;
}

/** Whether a tool names a command/shell call, whose preview mixes runtime rows. */
export function isCommandTool(name: string): boolean {
  // The reference implementation's `is_shell_call`: only the two shell tools
  // mix their own output with the runtime's execution rows.
  return name === 'run_command' || name === 'shell_command';
}

/** The preview a row renders: a command's output without the runtime's rows. */
export function displayPreview(tool: ToolCallView): string | null {
  if (tool.preview === null || tool.preview === '') return null;
  if (!isCommandTool(tool.name)) return tool.preview;
  const body = commandOutputBody(tool.preview).join('\n');
  return body === '' ? null : body;
}

function roundStatus(tools: readonly ToolCallView[]): RoundStatus {
  if (tools.some((tool) => tool.status === 'run')) return 'running';
  if (tools.length > 0 && tools.every((tool) => tool.status === 'done')) return 'ok';
  if (tools.some((tool) => tool.status === 'fail')) return 'failed';
  if (tools.some((tool) => tool.status === 'cancelled')) return 'cancelled';
  return 'unknown';
}

function roundAllOk(tools: readonly ToolCallView[]): boolean {
  return tools.length > 0 && tools.every((tool) => tool.status === 'done');
}

/** Group the observed batch ids of a settled round into per-call id lists. */
function roundBatches(tools: readonly ToolCallView[]): string[][] {
  const order: number[] = [];
  const batches: string[][] = [];
  for (const tool of tools) {
    if (tool.batch === null) continue;
    const index = order.indexOf(tool.batch);
    if (index >= 0) {
      batches[index].push(tool.id);
      continue;
    }
    order.push(tool.batch);
    batches.push([tool.id]);
  }
  return batches;
}

/** Activity class used ONLY by the legacy fallback (no `model_step` on the wire). */
function activityClass(name: string): string {
  const n = name.toLowerCase();
  if (/apply_patch|edit|write|patch/.test(n)) return 'edit';
  if (/read|cat|open|view/.test(n)) return 'read';
  if (/search|grep|find|glob|list/.test(n)) return 'search';
  if (/bash|shell|exec|command|run|terminal|cargo|npm|git_(?!diff)/.test(n)) return 'command';
  return 'other';
}

/**
 * Whether `tool` opens a new round instead of continuing `round`.
 *
 * The primary boundary is the runtime's `model_step` and it never consults
 * tool kind, timing or prose. When either side lacks the identity (a legacy
 * transcript), the pre-round activity-class fallback decides — and never
 * across a call that is still running.
 */
function startsNewRound(round: ExecutionRoundView, tool: ToolCallView): boolean {
  if (round.modelStep !== null && tool.modelStep !== null) {
    return round.modelStep !== tool.modelStep;
  }
  if (round.tools.some((t) => t.status === 'run')) return false;
  const previous = [...round.tools].reverse()[0];
  if (!previous) return false;
  return activityClass(previous.name) !== activityClass(tool.name);
}

/** Group a turn's tool calls into ExecutionRounds (Contract v1 §I1–§I3). */
export function groupExecutionRounds(tools: readonly ToolCallView[]): ExecutionRoundView[] {
  const rounds: ExecutionRoundView[] = [];
  for (const tool of tools) {
    const last = rounds[rounds.length - 1];
    if (last && !startsNewRound(last, tool)) {
      last.tools.push(tool);
      continue;
    }
    rounds.push({
      modelStep: tool.modelStep,
      tools: [tool],
      status: 'running',
      allOk: false,
      batches: [],
    });
  }
  for (const round of rounds) {
    round.status = roundStatus(round.tools);
    round.allOk = roundAllOk(round.tools);
    round.batches = roundBatches(round.tools);
  }
  return rounds;
}

interface OrderedEntry {
  seq: number;
  message?: ChatMessage;
  tool?: ToolCallView;
}

/** The current turn's messages and tools, merged by arrival order. */
function currentTurnEntries(
  messages: readonly ChatMessage[],
  tools: readonly ToolCallView[],
): OrderedEntry[] {
  let start = 0;
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    if (isTurnUser(messages[i])) {
      start = i;
      break;
    }
  }
  const entries: OrderedEntry[] = [];
  for (const message of messages.slice(start)) entries.push({ seq: message.seq, message });
  for (const tool of tools) entries.push({ seq: tool.seq, tool });
  entries.sort((a, b) => a.seq - b.seq);
  return entries;
}

interface AssistantNode {
  kind: 'assistant';
  text: string;
  seq: number;
  /** A work call followed it, so it was interim narration, not the answer. */
  demoted: boolean;
}

interface RoundNode {
  kind: 'round';
  round: ExecutionRoundView;
  seq: number;
}

type TurnNode = AssistantNode | RoundNode;

/**
 * Project one turn onto the contract's semantic items.
 *
 * `turnEnded` is the mechanical moment that decides the FinalAnswer: only once
 * the turn has a terminal may a still-undecided message become the answer.
 * Before that it is plain AssistantText, which is what streaming looks like.
 */
export function projectTurn(
  messages: readonly ChatMessage[],
  tools: readonly ToolCallView[],
  turnEnded: boolean,
): ProjectedItem[] {
  const nodes: TurnNode[] = [];
  for (const entry of currentTurnEntries(messages, tools)) {
    if (entry.message) {
      // A /btw side answer is its own surface and never joins the main
      // transcript (Contract v1 §I12).
      if (entry.message.btw !== undefined) continue;
      // Only the model's public assistant content is AssistantText. A user
      // line, a runtime notice and bookkeeping are not.
      const { role, kind } = entry.message;
      if (role !== 'assistant' && kind !== 'compaction_summary') continue;
      const text = entry.message.text;
      if (!text.trim()) continue;
      nodes.push({ kind: 'assistant', text, seq: entry.seq, demoted: false });
      continue;
    }
    const tool = entry.tool;
    if (!tool) continue;
    // The runtime's own classification, read verbatim. A surface that
    // re-derived this from `tool.name` would be the second truth source the
    // contract removed.
    if (tool.answerEffect === 'work') {
      for (const node of nodes) {
        if (node.kind === 'assistant') node.demoted = true;
      }
    }
    const last = nodes[nodes.length - 1];
    if (last && last.kind === 'round' && !startsNewRound(last.round, tool)) {
      last.round.tools.push(tool);
    } else {
      nodes.push({
        kind: 'round',
        seq: entry.seq,
        round: {
          modelStep: tool.modelStep,
          tools: [tool],
          status: 'running',
          allOk: false,
          batches: [],
        },
      });
    }
  }
  const items: ProjectedItem[] = [];
  for (const node of nodes) {
    if (node.kind === 'assistant') {
      items.push(
        turnEnded && !node.demoted
          ? { kind: 'final_answer', text: node.text, seq: node.seq }
          : { kind: 'assistant_text', text: node.text, seq: node.seq },
      );
      continue;
    }
    node.round.status = roundStatus(node.round.tools);
    node.round.allOk = roundAllOk(node.round.tools);
    node.round.batches = roundBatches(node.round.tools);
    items.push({ kind: 'execution_round', round: node.round, seq: node.seq });
  }
  return items;
}

/** The turn's committed answer, or null when it ended without one (§I9). */
export function committedFinalAnswer(
  messages: readonly ChatMessage[],
  tools: readonly ToolCallView[],
): string | null {
  const items = projectTurn(messages, tools, true);
  let answer: string | null = null;
  for (const item of items) {
    if (item.kind === 'final_answer') answer = item.text;
  }
  return answer;
}

/** Status glyph for a round head. Visual only; the headline carries the fact. */
export function roundGlyph(round: ExecutionRoundView): string {
  switch (round.status) {
    case 'running':
      return '●';
    case 'ok':
      return '✓';
    case 'failed':
      return '✗';
    case 'cancelled':
      return '■';
    default:
      return '◇';
  }
}

/**
 * A truthful one-line head for a round.
 *
 * "All ok" is only ever claimed when EVERY visible call succeeded: a settled
 * round may be `ok` without being an all-success claim (Contract v1 §I7).
 */
export function roundHeadline(round: ExecutionRoundView): string {
  const total = round.tools.length;
  switch (round.status) {
    case 'running':
      return `执行中 · ${total} 项`;
    case 'ok':
      return round.allOk ? `完成 ${total} 项` : `${total} 项已结束`;
    case 'failed': {
      const failed = round.tools.filter((t) => t.status === 'fail').length;
      return `完成 ${total} 项 · ${failed} 项失败`;
    }
    case 'cancelled':
      return `已停止 · ${total} 项`;
    default:
      return `结果未知 · ${total} 项`;
  }
}
