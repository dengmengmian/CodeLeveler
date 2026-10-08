// Types for the shared conversation-presentation rules.
//
// The implementation is plain ESM (`conversation.mjs`) because the Desktop
// renderer has no bundler; this declaration file is what the TypeScript client
// sees.

export interface ToolView {
  id: string;
  name: string;
  arguments: string;
  /** The corpus' contract vocabulary, or a client's own spelling of it
   *  (`run`/`done`/`fail`). The rules read either. */
  status: string;
  preview?: string | null;
  appliedDiff?: string | null;
  durationMs?: number | null;
  seq: number;
}

export interface ThoughtView {
  id: string;
  text: string;
  elapsedMs: number;
  seq: number;
}

export interface RoundView {
  modelStep: number | null;
  tools: ToolView[];
  status: string;
  allOk: boolean;
  batches: string[][];
}

export interface ExplorationMember {
  id: string;
  name: string;
  target: string;
  status: string;
}

export interface ExplorationReceipt {
  kind: 'exploration_receipt';
  reads: number;
  searches: number;
  label: string;
  folded: boolean;
  members: ExplorationMember[];
  seq: number;
}

export interface FailureSummary {
  visible: true;
  exitCode: number | null;
  line: string;
}

export interface ConfirmedDiff {
  kind: 'confirmed_diff';
  toolId: string;
  patch: string;
  lines: string[];
  added: number;
  removed: number;
  seq: number;
}

export interface FoldedThought {
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
  | { kind: 'round'; round: RoundView; seq: number };

export function isExplorationTool(name: string): boolean;
export function isEditTool(name: string): boolean;
export function explorationLabel(reads: number, searches: number): string;
export function groupExploration(tools: readonly ToolView[]): ExplorationReceipt[];
export function failureSummary(
  tool: ToolView,
  failureLine: (preview: string) => string | null,
): FailureSummary | null;
export function diffCounts(patch: string): [number, number];
export function diffLines(patch: string): string[];
export function confirmedDiff(tool: ToolView): string | null;
export function confirmedDiffs(tools: readonly ToolView[]): ConfirmedDiff[];
export function foldedThoughts(thoughts: readonly ThoughtView[]): FoldedThought[];
export function turnBlocks(
  thoughts: readonly FoldedThought[],
  rounds: readonly RoundView[],
): TurnBlock[];

export function isRuntimeNote(line: string): boolean;
export function commandOutputBody(preview: string): string[];
export function failureLine(preview: string): string | null;
export function isCommandTool(name: string): boolean;
export function displayPreview(tool: ToolView): string | null;
