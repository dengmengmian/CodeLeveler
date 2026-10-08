// Conversation Presentation Contract — the Web implementation.
//
// One conversation, one semantic tree. The TUI is the reference
// implementation and this module is the Web's projection of the SAME frozen
// contract (`testdata/conversation_presentation/v1/*.json`), built out of the
// same runtime facts:
//
//   User → Thinking (live) → Thought (completed, folded) → Exploration receipt
//   (collapsed, reversible) → Tool row → Run (collapsed; failure visible) →
//   Confirmed Diff (always in full) → Narration → Final.
//
// What this module refuses to do:
// - it never rebuilds a Diff from a tool's arguments. `ToolCallView.appliedDiff`
//   is the runtime's confirmed result; the requested patch is a different fact;
// - it never classifies a tool name into a product "stage" (no 项目检查 /
//   后端验证). Read-only exploration is a *presentation* fold over tool kinds
//   the runtime already ships, and the fold hides; it never rewrites;
// - it never invents a duration, a status or a failure reason.

import type { ThoughtView, ToolCallView } from '../state/store';
import type { ExecutionRoundView } from './executionRounds';

/**
 * The read-only exploration tools: they observe, they do not change anything.
 *
 * This list mirrors the reference implementation's read-only receipt kinds. It
 * decides presentation grouping only — never permission, never execution.
 */
const EXPLORATION_TOOLS: readonly string[] = [
  'read_file',
  'read',
  'view_file',
  'list_files',
  'list_directory',
  'glob',
  'find_files',
  'grep',
  'search',
  'code_search',
  'lsp',
  'lsp_lookup',
];

export function isExplorationTool(name: string): boolean {
  return EXPLORATION_TOOLS.includes(name);
}

/** Whether this call is the runtime's edit family (its diff is canonical). */
export function isEditTool(name: string): boolean {
  return name === 'apply_patch' || name === 'write_file' || name === 'edit_file';
}

export interface ExplorationMember {
  id: string;
  name: string;
  target: string;
  status: ToolCallView['status'];
}

/** The merged receipt consecutive read-only exploration collapses into. */
export interface ExplorationReceipt {
  kind: 'exploration_receipt';
  /** How many file reads the receipt counts. */
  reads: number;
  /** How many searches the receipt counts. */
  searches: number;
  /** The receipt's headline, exactly the reference wording. */
  label: string;
  /** Collapsed by default; the reader opens it. */
  folded: boolean;
  members: ExplorationMember[];
  /** Arrival stamp of the first member, so the receipt keeps its place. */
  seq: number;
}

function isReadTool(name: string): boolean {
  return /read|view|list|glob|find_files/.test(name);
}

function memberTarget(tool: ToolCallView): string {
  try {
    const args = typeof tool.arguments === 'string' ? JSON.parse(tool.arguments) : tool.arguments;
    if (args && typeof args === 'object') {
      for (const key of ['path', 'pattern', 'query', 'glob', 'file']) {
        const value = (args as Record<string, unknown>)[key];
        if (typeof value === 'string') return value;
      }
    }
  } catch {
    // A malformed argument blob is the runtime's business; the receipt shows
    // the tool without a target rather than guessing one.
  }
  return '';
}

/** The receipt headline: `读取 N 个文件 · 搜索 M 次`, only the parts that exist. */
export function explorationLabel(reads: number, searches: number): string {
  const parts: string[] = [];
  if (reads > 0) parts.push(`读取 ${reads} 个文件`);
  if (searches > 0) parts.push(`搜索 ${searches} 次`);
  return parts.join(' · ');
}

/**
 * Collapse CONSECUTIVE read-only exploration into one receipt.
 *
 * A single explorer is not a receipt group: it renders as its own direct row
 * (the reference does the same), so a lone read never gets an aggregate header
 * that hides exactly one thing.
 */
export function groupExploration(tools: readonly ToolCallView[]): ExplorationReceipt[] {
  const receipts: ExplorationReceipt[] = [];
  let run: ToolCallView[] = [];
  const flush = () => {
    if (run.length < 2) {
      run = [];
      return;
    }
    const reads = run.filter((tool) => isReadTool(tool.name)).length;
    const searches = run.length - reads;
    receipts.push({
      kind: 'exploration_receipt',
      reads,
      searches,
      label: explorationLabel(reads, searches),
      folded: true,
      members: run.map((tool) => ({
        id: tool.id,
        name: tool.name,
        target: memberTarget(tool),
        status: tool.status,
      })),
      seq: run[0].seq,
    });
    run = [];
  };
  for (const tool of tools) {
    if (isExplorationTool(tool.name)) {
      run.push(tool);
      continue;
    }
    flush();
  }
  flush();
  return receipts;
}

/** A failed call's readable failure, from the command's own output only. */
export interface FailureSummary {
  visible: true;
  exitCode: number | null;
  line: string;
}

/**
 * The failure a collapsed row must still show.
 *
 * `null` when the call did not fail. A cancelled or unknown call is not a
 * failure: it gets no failure line, exactly like the reference.
 */
export function failureSummary(
  tool: ToolCallView,
  failureLine: (preview: string) => string | null,
): FailureSummary | null {
  if (tool.status !== 'fail') return null;
  return {
    visible: true,
    exitCode: null,
    line: failureLine(tool.preview ?? '') ?? '',
  };
}

/** A confirmed Diff: the whole patch, never a summary of it. */
export interface ConfirmedDiff {
  kind: 'confirmed_diff';
  toolId: string;
  /** The runtime's confirmed patch text, verbatim. */
  patch: string;
  /** Every changed line the reader must be able to see. */
  lines: string[];
  added: number;
  removed: number;
  seq: number;
}

export function confirmedDiffs(tools: readonly ToolCallView[]): ConfirmedDiff[] {
  const out: ConfirmedDiff[] = [];
  for (const tool of tools) {
    if (!isEditTool(tool.name)) continue;
    if (tool.status !== 'done') continue;
    const patch = tool.appliedDiff;
    if (!patch) continue;
    const lines = patch.split('\n');
    out.push({
      kind: 'confirmed_diff',
      toolId: tool.id,
      patch,
      lines,
      added: lines.filter((line) => line.startsWith('+') && !line.startsWith('+++')).length,
      removed: lines.filter((line) => line.startsWith('-') && !line.startsWith('---')).length,
      seq: tool.seq,
    });
  }
  return out;
}

/** A completed Thought as the conversation presents it: folded, openable. */
export interface FoldedThought {
  kind: 'thought';
  id: string;
  elapsedMs: number;
  folded: true;
  body: string;
  seq: number;
}

export function foldedThoughts(thoughts: readonly ThoughtView[]): FoldedThought[] {
  return thoughts
    .filter((thought) => thought.text.trim() !== '')
    .map((thought) => ({
      kind: 'thought' as const,
      id: thought.id,
      elapsedMs: thought.elapsedMs,
      folded: true as const,
      body: thought.text,
      seq: thought.seq,
    }));
}

/** A month of the turn's process timeline: Thoughts, receipts, rounds. */
export type TurnBlock =
  | { kind: 'thought'; thought: FoldedThought; seq: number }
  | {
      kind: 'receipt';
      receipt: ExplorationReceipt;
      /** The fold's hidden Thoughts: revealed when the receipt opens. */
      thoughts: FoldedThought[];
      seq: number;
    }
  | { kind: 'round'; round: ExecutionRoundViewLike; seq: number };

/** The shape this module needs from a round: the execution contract's own. */
export type ExecutionRoundViewLike = ExecutionRoundView;

/**
 * The turn's process in arrival order: completed Thoughts interleaved with the
 * rounds that ran between them, and a read-only run rendered as ONE receipt.
 *
 * A round whose calls are all read-only exploration (and at least two of them)
 * becomes a receipt; anything else stays a round. The Thoughts whose arrival
 * falls inside a receipt's span belong to that fold — the reference hides them
 * there, and opening the receipt restores them.
 */
export function turnBlocks(
  thoughts: readonly FoldedThought[],
  rounds: readonly ExecutionRoundViewLike[],
): TurnBlock[] {
  const blocks: TurnBlock[] = [];
  const folds: { receipt: ExplorationReceipt; first: number; last: number; block: TurnBlock }[] = [];
  for (const round of rounds) {
    const tools = round.tools;
    const explorations = groupExploration(tools);
    if (tools.length >= 2 && explorations.length === 1 && explorations[0].members.length === tools.length) {
      const receipt = explorations[0];
      const block: TurnBlock = {
        kind: 'receipt',
        receipt,
        thoughts: [],
        seq: receipt.seq,
      };
      const last = tools[tools.length - 1].seq;
      folds.push({ receipt, first: receipt.seq, last, block });
      blocks.push(block);
      continue;
    }
    blocks.push({ kind: 'round', round, seq: tools[0]?.seq ?? 0 });
  }
  for (const thought of thoughts) {
    const fold = folds.find((candidate) => thought.seq > candidate.first && thought.seq < candidate.last);
    if (fold && fold.block.kind === 'receipt') {
      fold.block.thoughts.push(thought);
      continue;
    }
    blocks.push({ kind: 'thought', thought, seq: thought.seq });
  }
  blocks.sort((a, b) => a.seq - b.seq);
  return blocks;
}
