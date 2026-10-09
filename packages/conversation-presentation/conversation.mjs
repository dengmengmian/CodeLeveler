// Conversation Presentation Contract — the shared presentation rules.
//
// ONE implementation of the conversation's presentation semantics, consumed by
// both JavaScript clients (the Web client and the Desktop renderer). The
// terminal is the reference implementation; this module and the terminal are
// both checked against the same corpus (`testdata/conversation_presentation/v1/`).
//
// Plain ESM on purpose: the Desktop renderer has no bundler, so the shared
// rules have to be importable exactly as they are. Nothing here touches React,
// the DOM, or any client's state container. It is the projection from runtime
// facts onto the semantic tree, and nothing else:
//
//   User → Thinking (live) → Thought (completed, folded) → Exploration receipt
//   (collapsed, reversible) → Tool row → Run (collapsed; failure visible) →
//   Confirmed Diff (always in full) → Narration → Final.
//
// What this module refuses to do:
// - it never rebuilds a Diff from a tool's arguments. `appliedDiff` is the
//   runtime's confirmed result; the requested patch is a different fact;
// - it never classifies a tool name into a product "stage" (no 项目检查 /
//   后端验证). Read-only exploration is a *presentation* fold over tool kinds
//   the runtime already ships, and the fold hides; it never rewrites;
// - it never invents a duration, a status or a failure reason.

/**
 * @typedef {object} ToolView
 * @property {string} id
 * @property {string} name
 * @property {string} arguments
 * @property {('run'|'done'|'fail'|'cancelled'|'unknown')} status
 * @property {string|null} [preview]
 * @property {string|null} [appliedDiff]
 * @property {number|null} [durationMs]
 * @property {number} seq
 * @property {string} [anchor]
 * @property {string} [task_id]
 *
 * @typedef {object} ThoughtView
 * @property {string} id
 * @property {string} text
 * @property {number} elapsedMs
 * @property {number} seq
 * @property {string} [anchor]
 *
 * @typedef {object} RoundView
 * @property {number|null} modelStep
 * @property {ToolView[]} tools
 * @property {string} status
 * @property {boolean} allOk
 * @property {string[][]} batches
 */

/** The read-only exploration tools: they observe, they change nothing.
 *
 * Presentation grouping only — never permission, never execution.
 * @type {readonly string[]}
 */
const EXPLORATION_TOOLS = [
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

/** @param {string} name @returns {boolean} */
export function isExplorationTool(name) {
  return EXPLORATION_TOOLS.includes(name);
}

/** Whether this call is the runtime's edit family (its diff is canonical).
 * @param {string} name @returns {boolean} */
export function isEditTool(name) {
  return name === 'apply_patch' || name === 'write_file' || name === 'edit_file';
}

/** @param {string} name @returns {boolean} */
/**
 * The settled-success spellings the clients use for a call that finished well.
 *
 * The corpus states the contract vocabulary (`ok` / `failed` / `running` /
 * `cancelled` / `unknown`); a client's own store may say `done` / `fail`. The
 * rules read either, so "did this confirm an edit?" answers the same question
 * on every client.
 * @type {readonly string[]}
 */
const SETTLED_OK = ['done', 'ok', 'success'];
/** @type {readonly string[]} */
const SETTLED_FAILED = ['fail', 'failed'];

function isReadTool(name) {
  return /read|view|list|glob|find_files/.test(name);
}

/** @param {ToolView} tool @returns {string} */
function memberTarget(tool) {
  try {
    const args = typeof tool.arguments === 'string' ? JSON.parse(tool.arguments) : tool.arguments;
    if (args && typeof args === 'object') {
      for (const key of ['path', 'pattern', 'query', 'glob', 'file']) {
        const value = args[key];
        if (typeof value === 'string') return value;
      }
    }
  } catch {
    // A malformed argument blob is the runtime's business; the receipt shows the
    // tool without a target rather than guessing one.
  }
  return '';
}

/** The receipt headline: `读取 N 个文件 · 搜索 M 次`, only the parts that exist.
 * @param {number} reads @param {number} searches @returns {string} */
export function explorationLabel(reads, searches) {
  const parts = [];
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
 *
 * @param {readonly ToolView[]} tools
 * @returns {Array<{kind:'exploration_receipt',reads:number,searches:number,label:string,folded:boolean,members:Array<{id:string,name:string,target:string,status:string}>,seq:number}>}
 */
export function groupExploration(tools) {
  const receipts = [];
  let run = [];
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
        seq: tool.seq,
      })),
      seq: run[0].seq,
      ...(run[0].anchor !== undefined ? { anchor: run[0].anchor } : {}),
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

/**
 * The failure a collapsed row must still show.
 *
 * `null` when the call did not fail: a cancelled or unknown call is not a
 * failure and gets no failure line, exactly like the reference.
 *
 * @param {ToolView} tool
 * @param {(preview: string) => string|null} failureLine
 * @returns {{visible:true,exitCode:number|null,line:string}|null}
 */
export function failureSummary(tool, failureLine) {
  if (!SETTLED_FAILED.includes(tool.status)) return null;
  return {
    visible: true,
    exitCode: null,
    line: failureLine(tool.preview ?? '') ?? '',
  };
}

/**
 * Within a confirmed patch, how many lines were added and removed.
 * Header markers (`+++`/`---`) are not changed lines.
 * @param {string} patch @returns {[number, number]}
 */
export function diffCounts(patch) {
  const lines = patch.split('\n');
  return [
    lines.filter((line) => line.startsWith('+') && !line.startsWith('+++')).length,
    lines.filter((line) => line.startsWith('-') && !line.startsWith('---')).length,
  ];
}

/** Every line of a confirmed diff. Full display is the contract, so this never
 * truncates. @param {string} patch @returns {string[]} */
export function diffLines(patch) {
  return patch.split('\n');
}

/**
 * The runtime's CONFIRMED diff for an edit, or `null`.
 *
 * A call that did not confirm an edit, or whose runtime reported no diff, has
 * nothing to show — a cancelled edit is not a change.
 *
 * @param {ToolView} tool @returns {string|null}
 */
export function confirmedDiff(tool) {
  if (!tool) return null;
  if (!SETTLED_OK.includes(tool.status)) return null;
  const patch = tool.appliedDiff;
  return typeof patch === 'string' && patch !== '' ? patch : null;
}

/**
 * Confirmed diffs, in arrival order, each with the whole patch and its counts.
 * @param {readonly ToolView[]} tools
 * @returns {Array<{kind:'confirmed_diff',toolId:string,patch:string,lines:string[],added:number,removed:number,seq:number}>}
 */
export function confirmedDiffs(tools) {
  const out = [];
  for (const tool of tools) {
    if (!isEditTool(tool.name)) continue;
    const patch = confirmedDiff(tool);
    if (!patch) continue;
    const [added, removed] = diffCounts(patch);
    out.push({
      kind: 'confirmed_diff',
      toolId: tool.id,
      patch,
      lines: diffLines(patch),
      added,
      removed,
      seq: tool.seq,
    });
  }
  return out;
}

/**
 * A completed Thought as the conversation presents it: folded, openable.
 * @param {readonly ThoughtView[]} thoughts
 * @returns {Array<{kind:'thought',id:string,elapsedMs:number,folded:true,body:string,seq:number}>}
 */
export function foldedThoughts(thoughts) {
  return (thoughts || [])
    .filter((thought) => (thought.text || '').trim() !== '')
    .map((thought) => ({
      kind: 'thought',
      id: thought.id,
      elapsedMs: thought.elapsedMs,
      folded: true,
      body: thought.text,
      seq: thought.seq,
      ...(thought.anchor !== undefined ? { anchor: thought.anchor } : {}),
    }));
}

/**
 * Whether the turn's exploration run is one receipt.
 * @param {readonly ToolView[]} tools @returns {boolean}
 */
function isReceiptRun(tools) {
  if (tools.length < 2 || tools.some(tool => tool.anchor !== tools[0].anchor || tool.task_id !== tools[0].task_id)) return false;
  const receipts = groupExploration(tools);
  return receipts.length === 1 && receipts[0].members.length === tools.length;
}

/**
 * The turn's process in arrival order: completed Thoughts interleaved with the
 * rounds that ran between them, and a read-only run rendered as ONE receipt.
 *
 * The Thoughts whose arrival falls inside a receipt's span belong to that fold:
 * the reference hides them there, and opening the receipt restores them.
 *
 * @param {readonly {kind:'thought',id:string,elapsedMs:number,folded:true,body:string,seq:number}[]} thoughts
 * @param {readonly RoundView[]} rounds
 * @returns {Array<{kind:'thought'|'receipt'|'round',[key:string]:unknown}>}
 */
export function turnBlocks(thoughts, rounds) {
  const blocks = [];
  const folds = [];
  // Model steps delimit execution rounds, not read-only presentation runs.
  // Only confirmed successful exploration may bridge that boundary; an error,
  // pending result, mutation or a different message/task anchor ends the run.
  let pending = [];
  const addRound = (round) => {
    const tools = round.tools || [];
    if (isReceiptRun(tools)) {
      const receipt = groupExploration(tools)[0];
      const block = { kind: 'receipt', receipt, thoughts: [], seq: receipt.seq };
      folds.push({ first: receipt.seq, last: tools[tools.length - 1].seq, block });
      blocks.push(block);
    } else {
      blocks.push({ kind: 'round', round, seq: (tools[0] && tools[0].seq) || 0 });
    }
  };
  const flush = () => {
    if (pending.length === 0) return;
    const tools = pending.flatMap(round => round.tools);
    if (tools.length >= 2) addRound({ ...pending[0], tools });
    else addRound(pending[0]);
    pending = [];
  };
  for (const round of rounds) {
    const tools = round.tools || [];
    const eligible = tools.length > 0 && tools.every(tool =>
      isExplorationTool(tool.name) && SETTLED_OK.includes(tool.status) &&
      tool.anchor === tools[0].anchor && tool.task_id === tools[0].task_id);
    const last = pending.length > 0 ? pending[pending.length - 1].tools.at(-1) : undefined;
    const first = tools[0];
    if (!eligible || (last && (last.anchor !== first.anchor || last.task_id !== first.task_id))) flush();
    if (eligible) pending.push(round);
    else addRound(round);
  }
  flush();
  const seenThoughtIds = new Set();
  for (const thought of thoughts) {
    if (seenThoughtIds.has(thought.id)) continue;
    seenThoughtIds.add(thought.id);
    const fold = folds.find((candidate) => thought.seq > candidate.first && thought.seq < candidate.last &&
      thought.anchor === candidate.block.receipt.anchor);
    if (fold) {
      fold.block.thoughts.push(thought);
      continue;
    }
    blocks.push({ kind: 'thought', thought, seq: thought.seq });
  }
  blocks.sort((a, b) => a.seq - b.seq);
  return blocks;
}

/** A reversible receipt restores members and Thoughts in their original order.
 * @param {{seq:number,members:readonly {id:string,seq?:number}[]}} receipt
 * @param {readonly {id:string,seq:number}[]} thoughts
 */
export function explorationEntries(receipt, thoughts) {
  return [
    ...receipt.members.map(member => ({ kind: 'member', member, seq: member.seq ?? receipt.seq })),
    ...thoughts.map(thought => ({ kind: 'thought', thought, seq: thought.seq })),
  ].sort((a, b) => a.seq - b.seq);
}

/** Runtime execution rows: they say HOW a command ran, never why it failed.
 * @type {readonly string[]} */
const RUNTIME_NOTE_TAGS = ['[execution policy] ', '[mutation rejected] ', '[note] '];

/** @param {string} line @returns {boolean} */
export function isRuntimeNote(line) {
  const trimmed = line.trimEnd();
  if (trimmed.startsWith('exit: ')) return true;
  if (trimmed.startsWith('--- ') && trimmed.endsWith(' ---')) return true;
  if (trimmed === '[timed out]') return true;
  if (trimmed.startsWith('[timed out after ') && trimmed.endsWith(']')) return true;
  return RUNTIME_NOTE_TAGS.some((tag) => trimmed.startsWith(tag));
}

/** The command's own output, with the runtime's execution rows removed.
 * @param {string} preview @returns {string[]} */
export function commandOutputBody(preview) {
  return preview
    .split('\n')
    .map((line) => line.trimEnd())
    .filter((line) => line.trim() !== '' && !isRuntimeNote(line));
}

/**
 * Whether a lowercased line says something FAILED, as opposed to counting zero
 * of them ("11 passed; 0 failed", "# fail 0") — the reference's own rule.
 * @param {string} lower @returns {boolean}
 */
function countsAFailure(lower) {
  const nonzero = (token) =>
    token === undefined || token === '' ? undefined : [...token].some((c) => c !== '0');
  const numberBefore = (at) => {
    const head = lower.slice(0, at).trimEnd();
    return nonzero(head.split(/[^0-9]/).pop() ?? '');
  };
  const numberAfter = (at) => {
    const tail = lower.slice(at).replace(/^[a-z:= ]+/i, '');
    return nonzero((tail.split(/[^0-9]/)[0] ?? '').trim());
  };
  let from = 0;
  for (;;) {
    const at = lower.indexOf('fail', from);
    if (at < 0) return false;
    if ((numberBefore(at) ?? numberAfter(at) ?? true)) return true;
    from = at + 4;
  }
}

/**
 * The line that says what actually failed: the first line reporting a failure
 * (`error…`, `FAIL`, `✗`, `panic`, or a NONZERO failure count), else the first
 * thing the command printed — never a runtime execution row.
 *
 * The reference implementation (`leveler-tui`'s `shell_failure_line`) is the
 * authority for this choice: a client that named a different line would be a
 * second presentation of the same fact.
 *
 * @param {string} preview @returns {string|null}
 */
export function failureLine(preview) {
  const lines = commandOutputBody(preview);
  const reportsFailure = (line) => {
    const lower = line.toLowerCase();
    return (
      lower.startsWith('error') ||
      lower.includes('panic') ||
      line.includes('\u2717') ||
      countsAFailure(lower)
    );
  };
  return lines.find(reportsFailure) ?? lines[0] ?? null;
}

/** Whether a tool names a command/shell call, whose preview mixes runtime rows.
 * @param {string} name @returns {boolean} */
export function isCommandTool(name) {
  return name === 'run_command' || name === 'shell_command';
}

/** The preview a row renders: a command's output without the runtime's rows.
 * @param {ToolView} tool @returns {string|null} */
export function displayPreview(tool) {
  if (tool.preview === null || tool.preview === undefined || tool.preview === '') return null;
  if (!isCommandTool(tool.name)) return tool.preview;
  const body = commandOutputBody(tool.preview).join('\n');
  return body === '' ? null : body;
}
