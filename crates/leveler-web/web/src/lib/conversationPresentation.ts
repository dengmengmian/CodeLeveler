// Conversation Presentation Contract — the Web client's typed surface.
//
// The RULES live once, in
// `packages/conversation-presentation/conversation.mjs`, because the Web client
// and the Desktop renderer must present the same conversation and the Desktop
// has no bundler to compile TypeScript with. This file only types the shared
// rules for this client's own view models; it holds no semantics of its own.
//
// The reference implementation is the terminal, and every surface (terminal,
// Web, Desktop) is checked against the same corpus in
// `testdata/conversation_presentation/v1/`.

import * as shared from '../../../../../packages/conversation-presentation/conversation.mjs';
import type { ThoughtView, ToolCallView } from '../state/store';
import type { ExecutionRoundView } from './executionRounds';

export interface ExplorationMember {
  seq?: number;
  id: string;
  name: string;
  target: string;
  status: ToolCallView['status'];
}

/** The merged receipt consecutive read-only exploration collapses into. */
export interface ExplorationReceipt {
  kind: 'exploration_receipt';
  reads: number;
  searches: number;
  /** The receipt's headline, exactly the reference wording. */
  label: string;
  /** Collapsed by default; the reader opens it. */
  folded: boolean;
  members: ExplorationMember[];
  /** Arrival stamp of the first member, so the receipt keeps its place. */
  seq: number;
}

/** A failed call's readable failure, from the command's own output only. */
export interface FailureSummary {
  visible: true;
  exitCode: number | null;
  line: string;
}

/** A confirmed Diff: the whole patch, never a summary of it. */
export interface ConfirmedDiff {
  kind: 'confirmed_diff';
  toolId: string;
  patch: string;
  lines: string[];
  added: number;
  removed: number;
  seq: number;
}

/** A completed Thought as the conversation presents it: folded, openable. */
export interface FoldedThought {
  anchor?: string;
  kind: 'thought';
  id: string;
  elapsedMs: number;
  folded: true;
  body: string;
  seq: number;
}

export type TurnBlock =
  | { kind: 'thought'; thought: FoldedThought; seq: number }
  | { kind: 'receipt'; receipt: ExplorationReceipt; thoughts: FoldedThought[]; seq: number }
  | { kind: 'round'; round: ExecutionRoundView; seq: number };

export function isExplorationTool(name: string): boolean {
  return shared.isExplorationTool(name);
}

export function isEditTool(name: string): boolean {
  return shared.isEditTool(name);
}

export function explorationLabel(reads: number, searches: number): string {
  return shared.explorationLabel(reads, searches);
}

export function groupExploration(tools: readonly ToolCallView[]): ExplorationReceipt[] {
  return shared.groupExploration(tools as never) as ExplorationReceipt[];
}

export function failureSummary(
  tool: ToolCallView,
  failureLine: (preview: string) => string | null,
): FailureSummary | null {
  return shared.failureSummary(tool as never, failureLine) as FailureSummary | null;
}

export function diffCounts(patch: string): [number, number] {
  return shared.diffCounts(patch);
}

export function diffLines(patch: string): string[] {
  return shared.diffLines(patch);
}

export function confirmedDiff(tool: ToolCallView): string | null {
  return shared.confirmedDiff(tool as never);
}

export function confirmedDiffs(tools: readonly ToolCallView[]): ConfirmedDiff[] {
  return shared.confirmedDiffs(tools as never) as ConfirmedDiff[];
}

export function foldedThoughts(thoughts: readonly ThoughtView[]): FoldedThought[] {
  return shared.foldedThoughts(thoughts as never) as FoldedThought[];
}

export function turnBlocks(
  thoughts: readonly FoldedThought[],
  rounds: readonly ExecutionRoundView[],
): TurnBlock[] {
  return shared.turnBlocks(thoughts as never, rounds as never) as TurnBlock[];
}

export function explorationEntries(receipt: ExplorationReceipt, thoughts: readonly FoldedThought[]) {
  return shared.explorationEntries(receipt as never, thoughts as never) as (
    | { kind: 'member'; member: ExplorationMember; seq: number }
    | { kind: 'thought'; thought: FoldedThought; seq: number }
  )[];
}
