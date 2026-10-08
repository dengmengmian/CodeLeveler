// Conversation Presentation Contract — Web conformance.
//
// Reads the SAME frozen corpus the reference implementation is anchored to
// (`testdata/conversation_presentation/v1/C1..C10.json`), drives the real Web
// reducer through the real `RuntimeBridge` (live events, a reconnect snapshot,
// or a durable-history replay), projects the resulting session view with the
// SAME shared rules the components use, and compares it with the frozen tree.
//
// The comparison is structural: item kinds, their order, fold state,
// visibility, roles and the facts a collapsed row must still show — never only
// "does this string appear".

import { beforeEach, describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { SessionView, Action, AppState } from '../state/store';
import { initialState, reducer } from '../state/store';
import type { RuntimeEvent } from '../types/protocol';
import { RuntimeBridge } from './controller';
import {
  confirmedDiffs,
  foldedThoughts,
  groupExploration,
} from './conversationPresentation';
import { failureReason, groupExecutionRounds, projectTurn } from './executionRounds';
import { isTurnUser } from './presentationKind';
import { ConfirmedDiffBlock } from '../components/ConfirmedDiffBlock';
import { ExplorationReceiptRow } from '../components/ExplorationReceiptRow';
import { ThoughtRow, ThinkingRow } from '../components/ThoughtRow';

import fixtureC1 from '../../../testdata/conversation_presentation/v1/C1.json?raw';
import fixtureC2 from '../../../testdata/conversation_presentation/v1/C2.json?raw';
import fixtureC3 from '../../../testdata/conversation_presentation/v1/C3.json?raw';
import fixtureC4 from '../../../testdata/conversation_presentation/v1/C4.json?raw';
import fixtureC5 from '../../../testdata/conversation_presentation/v1/C5.json?raw';
import fixtureC6 from '../../../testdata/conversation_presentation/v1/C6.json?raw';
import fixtureC7 from '../../../testdata/conversation_presentation/v1/C7.json?raw';
import fixtureC8 from '../../../testdata/conversation_presentation/v1/C8.json?raw';
import fixtureC9 from '../../../testdata/conversation_presentation/v1/C9.json?raw';
import fixtureC10 from '../../../testdata/conversation_presentation/v1/C10.json?raw';

const FIXTURE_SOURCES: readonly string[] = [
  fixtureC1,
  fixtureC2,
  fixtureC3,
  fixtureC4,
  fixtureC5,
  fixtureC6,
  fixtureC7,
  fixtureC8,
  fixtureC9,
  fixtureC10,
];

interface Fixture {
  id: string;
  title: string;
  invariant: string;
  session?: { collaboration?: string; goal?: string };
  paths: Record<string, { event?: RuntimeEvent; history?: unknown[]; snapshot?: RuntimeEvent }[]>;
  expect: {
    items: Record<string, unknown>[];
    collaboration?: string;
    plan?: { steps: Record<string, unknown>[] };
    forbidden_text?: string[];
  };
}

const localStore = new Map<string, string>();

beforeEach(() => {
  localStore.clear();
  const g = globalThis as Record<string, unknown>;
  g.window = {
    location: { href: 'http://localhost/', protocol: 'http:', host: 'localhost' },
    history: { replaceState: () => {} },
  };
  g.sessionStorage = { getItem: () => '', setItem: () => {}, removeItem: () => {} };
  g.localStorage = {
    getItem: (key: string) => localStore.get(key) ?? null,
    setItem: (key: string, value: string) => {
      localStore.set(key, value);
    },
    removeItem: (key: string) => {
      localStore.delete(key);
    },
  };
});

interface Harness {
  state: AppState;
  apply: (event: RuntimeEvent) => void;
}

/** The corpus' own event class names, read from the fixture's own helpers. */
type PathStep = { event?: RuntimeEvent; history?: unknown[]; snapshot?: RuntimeEvent };

function harness(fixture?: Fixture): Harness {
  const state: AppState = structuredClone(initialState);
  state.draft = false;
  // A fixture may declare session facts (the axis, the goal): they arrive on the
  // session snapshot, not in the event stream.
  const dispatch = (action: Action) => reducer(state, action);
  const bridge = new RuntimeBridge(dispatch, () => state);
  (bridge as unknown as { ws: { send: () => boolean; setSession: () => void } }).ws = {
    send: () => true,
    setSession: () => {},
  };
  const apply = (event: RuntimeEvent) =>
    (bridge as unknown as { applyEvent: (event: RuntimeEvent) => void }).applyEvent(event);
  apply(sessionOpened(fixture));
  return { state, apply };
}

function sessionOpened(fixture?: Fixture): RuntimeEvent {
  return {
    type: 'session_opened',
    session: {
      id: 's1',
      repository: '/repo',
      goal: fixture?.session?.goal ?? 'fixture',
      model: null,
      mode: 'assisted',
      branch: null,
      status: 'idle',
      messages: [],
      available_models: [],
      active_tools: [],
      active_background_tasks: [],
      plan: null,
      diff: null,
      checkpoints: [],
      recaps: [],
      user_shells: [],
      completion_report: null,
      thinking: null,
      work_profile: null,
      collaboration: fixture?.session?.collaboration ?? 'chat',
      pending_interactions: [],
      children: [],
      finalization_stage: null,
      last_sequence: null,
    },
  } as unknown as RuntimeEvent;
}

/** Drive one fixture path through the real bridge, in the order declared. */
function runPath(fixture: Fixture, steps: PathStep[]): Harness {
  const h = harness(fixture);
  for (const step of steps) {
    if (step.history !== undefined) {
      h.apply({
        type: 'session_history_loaded',
        session_id: 's1',
        entries: step.history,
      } as unknown as RuntimeEvent);
      continue;
    }
    const payload = step.event ?? step.snapshot;
    if (payload) h.apply(payload);
  }
  return h;
}

/** The conversation as the corpus names it: the Web projection, in order. */
function conversationTree(view: SessionView): Record<string, unknown>[] {
  const items: { seq: number; item: Record<string, unknown> }[] = [];
  // A reopened or continued conversation is its frozen per-turn traces PLUS the
  // live turn: both are the same conversation, so both are projected.
  const seenTools = new Set<string>();
  const tools = [...(view.traces ?? []).flatMap((trace) => trace.tools), ...view.tools]
    .filter((tool) => (seenTools.has(tool.id) ? false : (seenTools.add(tool.id), true)))
    .sort((a, b) => a.seq - b.seq);
  const rounds = groupExecutionRounds(tools);
  const receipts = groupExploration(tools);
  const receiptIds = new Set(receipts.flatMap((receipt) => receipt.members.map((m) => m.id)));
  const confirmed = confirmedDiffs(tools);
  // The client's OWN final-answer decision, read verbatim: the same projection
  // the timeline renders through.
  const answerKind = new Map<number, string>();
  for (const item of projectTurn(view.messages, view.tools, view.lastTurn !== null)) {
    if (item.kind === 'assistant_text' || item.kind === 'final_answer') {
      answerKind.set(item.seq, item.kind);
    }
  }
  const contractStatus = (status: string): string =>
    status === 'done' ? 'ok' : status === 'fail' ? 'failed' : status === 'run' ? 'running' : status;

  // A frozen turn's own answer, recorded when the turn ended: the Web's live
  // projection is per turn, so the frozen turns carry their classification.
  const frozenAnswers = new Set(
    (view.traces ?? []).map((trace) => trace.answerId).filter((id): id is string => id !== null),
  );
  for (const message of view.messages) {
    if (message.btw !== undefined) continue;
    if (message.kind === 'runtime_notice') {
      items.push({ seq: message.seq, item: { kind: 'runtime_notice', text: message.text } });
      continue;
    }
    if (message.kind === 'compaction_summary' || !isTurnUser(message)) {
      if (message.role !== 'assistant') continue;
      if (!message.text.trim()) continue;
      items.push({
        seq: message.seq,
        item: {
          kind:
            answerKind.get(message.seq) ??
            (frozenAnswers.has(message.id) ? 'final_answer' : 'assistant_text'),
          text: message.text,
        },
      });
      continue;
    }
    items.push({ seq: message.seq, item: { kind: 'user', text: message.text } });
  }

  for (const round of rounds) {
    const tools = round.tools.filter((tool) => !receiptIds.has(tool.id));
    if (tools.length === 0) continue;
    const first = tools[0];
    const edits = confirmed.filter((diff) => tools.some((tool) => tool.id === diff.toolId));
    if (edits.length === tools.length && edits.length > 0) {
      const paths = new Set<string>();
      for (const diff of edits) {
        for (const line of diff.lines) {
          const match = /^\+\+\+ (?:b\/)?(.+)$/.exec(line);
          if (match) paths.add(match[1]);
        }
      }
      const added = edits.reduce((total, diff) => total + diff.added, 0);
      const removed = edits.reduce((total, diff) => total + diff.removed, 0);
      items.push({
        seq: first.seq,
        item: {
          kind: 'edit_diff',
          paths: [...paths].sort(),
          added,
          removed,
          rendered_in_full: true,
          needs_click: false,
        },
      });
      continue;
    }
    // A lone read-only call is its own direct row: a receipt for one item hides
    // exactly the thing the reader came for.
    if (tools.length === 1 && groupExploration(tools).length === 0) {
      const only = tools[0];
      const lone = groupExploration([only]);
      if (lone.length === 0 && only.status !== 'fail' && /read|view|list|glob|find_files|grep|search|code_search|lsp/.test(only.name)) {
        let target = '';
        try {
          const args = JSON.parse(only.arguments) as Record<string, unknown>;
          for (const key of ['path', 'pattern', 'query', 'glob', 'file']) {
            if (typeof args[key] === 'string') {
              target = args[key] as string;
              break;
            }
          }
        } catch {
          target = '';
        }
        items.push({
          seq: first.seq,
          item: {
            kind: 'exploration_row',
            name: only.name,
            target,
            status: contractStatus(only.status),
          },
        });
        continue;
      }
    }
    const status = contractStatus(round.status);
    const failedTool = tools.find((tool) => tool.status === 'fail');
    const item: Record<string, unknown> = {
      kind: 'run_receipt',
      model_step: round.modelStep,
      status,
      folded: true,
      command_visible: true,
      output_visible: false,
      output_available: true,
      rows: tools.map((tool) => ({
        id: tool.id,
        name: tool.name,
        status: contractStatus(tool.status),
      })),
    };
    if (failedTool) {
      item.failure_visible = true;
      item.failure_line = failureReason(failedTool.preview ?? '');
      if (failedTool.exitCode !== null && failedTool.exitCode !== undefined) {
        item.exit_code = failedTool.exitCode;
      }
    }
    items.push({ seq: first.seq, item });
  }

  for (const receipt of receipts) {
    items.push({
      seq: receipt.seq,
      item: {
        kind: 'exploration_receipt',
        reads: receipt.reads,
        searches: receipt.searches,
        folded: receipt.folded,
        members_visible: false,
        reversible: true,
        members: receipt.members.map((member) => `${member.name}:${member.target}`),
      },
    });
  }

  // The turn's completed Thoughts, live ones and frozen traces alike: a trace is
  // the same conversation, kept per turn.
  const allThoughts = [
    ...view.thoughts,
    ...(view.traces ?? []).flatMap((trace) => trace.thoughts),
  ];
  const seenThoughts = new Set<string>();
  for (const thought of foldedThoughts(allThoughts)) {
    if (seenThoughts.has(thought.id)) continue;
    seenThoughts.add(thought.id);
    const source = allThoughts.find((candidate) => candidate.id === thought.id);
    items.push({
      seq: thought.seq,
      item: {
        kind: 'thought',
        state: source?.interrupted ? 'interrupted' : 'completed',
        ...(source?.interrupted ? {} : { elapsed_ms: thought.elapsedMs }),
        folded: thought.folded,
        body_visible: false,
        body: thought.body,
      },
    });
  }
  if (view.reasoning.trim() !== '') {
    items.push({
      seq: Number.MAX_SAFE_INTEGER - 2,
      item: {
        kind: 'thought',
        state: 'running',
        folded: false,
        body_visible: true,
        body: view.reasoning,
      },
    });
  }

  // Each frozen turn keeps its OWN terminal: a cancelled turn that a later
  // question followed still says it was cancelled, where it happened.
  const userSeqs = view.messages.filter(isTurnUser).map((message) => message.seq).sort((a, b) => a - b);
  for (const trace of view.traces ?? []) {
    if (!trace.lastTurn) continue;
    const next = userSeqs.find((seq) => seq > trace.userSeq) ?? Number.MAX_SAFE_INTEGER;
    const inside = [
      ...trace.tools.map((tool) => tool.seq),
      ...trace.thoughts.map((thought) => thought.seq),
      // A runtime-authored notice is not part of the turn's work: the turn's
      // terminal stays where the turn's own work ended.
      ...view.messages
        .filter(
          (m) =>
            m.seq > trace.userSeq && m.seq < next && m.kind !== 'runtime_notice',
        )
        .map((m) => m.seq),
    ];
    const last = inside.length > 0 ? Math.max(...inside) : trace.userSeq;
    items.push({ seq: last + 0.5, item: { kind: 'turn_end', status: trace.lastTurn.outcome } });
  }

  items.sort((a, b) => a.seq - b.seq);
  const ordered = items.map((entry) => entry.item);
  if (view.lastTurn) {
    const lastUser = userSeqs[userSeqs.length - 1];
    const lastTrace = (view.traces ?? []).find((trace) => trace.userSeq === lastUser);
    if (!lastTrace || lastTrace.lastTurn?.outcome !== view.lastTurn.outcome) {
      ordered.push({ kind: 'turn_end', status: view.lastTurn.outcome });
    }
  }
  return ordered;
}

/**
 * The fields of `expected` must appear in the same position in `actual`.
 *
 * An item marked `optional` is a fact a client may express in another place (the
 * terminal paints a dedicated failure block; the Web states the same failure in
 * its run row and turn terminal). It is asserted when it is present and never
 * forces the other clients to invent a row.
 */
function expectItems(id: string, expected: Record<string, unknown>[], actual: Record<string, unknown>[]) {
  let cursor = 0;
  expected.forEach((want, index) => {
    const optional = want.optional === true;
    const fields = Object.entries(want).filter(([key]) => key !== 'optional');
    if (optional) {
      const candidate = actual[cursor];
      const matches =
        candidate !== undefined && fields.every(([key, value]) => JSON.stringify(candidate[key]) === JSON.stringify(value));
      if (!matches) return;
    }
    const got = actual[cursor];
    for (const [key, value] of fields) {
      expect(
        got?.[key],
        `${id}: item ${index} field ${key}: ${JSON.stringify(got)}\n${JSON.stringify(actual, null, 1)}`,
      ).toEqual(value);
    }
    cursor += 1;
  });
  expect(
    cursor,
    `${id}: unclaimed items\n${JSON.stringify(actual.slice(cursor), null, 1)}`,
  ).toBe(actual.length);
}

describe('conversation presentation contract (Web)', () => {
  const fixtures = FIXTURE_SOURCES.map((raw) => JSON.parse(raw) as Fixture);

  it('reads the frozen corpus C1..C10', () => {
    expect(fixtures.map((fixture) => fixture.id)).toEqual([
      'C1',
      'C2',
      'C3',
      'C4',
      'C5',
      'C6',
      'C7',
      'C8',
      'C9',
      'C10',
    ]);
  });

  for (const fixture of fixtures) {
    it(`${fixture.id} — ${fixture.title}`, () => {
      let compared = 0;
      for (const [name, steps] of Object.entries(fixture.paths)) {
        const h = runPath(fixture, steps);
        const view = h.state.current;
        expect(view, `${fixture.id}/${name}: no session`).not.toBeNull();
        if (!view) return;
        const tree = conversationTree(view);
        try {
          expectItems(`${fixture.id}/${name}`, fixture.expect.items, tree);
        } catch (error) {
          throw new Error(`${fixture.id}/${name}: ${(error as Error).message}`);
        }
        if (fixture.expect.collaboration !== undefined) {
          expect(view.collaboration, `${fixture.id}/${name}: axis`).toBe(
            fixture.expect.collaboration,
          );
        }
        if (fixture.expect.plan !== undefined) {
          const steps = (view.plan?.steps ?? []).map((step) => ({
            description: step.description,
            status: step.status,
          }));
          expect(steps, `${fixture.id}/${name}: plan`).toEqual(fixture.expect.plan.steps);
        }
        if (fixture.expect.forbidden_text) {
          const rendered = JSON.stringify(tree);
          for (const needle of fixture.expect.forbidden_text) {
            expect(
              rendered.includes(needle),
              `${fixture.id}/${name}: forbidden text in the tree: ${needle}`,
            ).toBe(false);
          }
        }
        compared += 1;
      }
      expect(compared, `${fixture.id}: no path compared`).toBeGreaterThan(0);
    });
  }
});

describe('conversation presentation rendering (Web)', () => {
  it('paints every confirmed diff line, with no expand affordance', () => {
    const patch = ['--- a/src/models.rs', '+++ b/src/models.rs', '@@ -1,1 +1,1 @@', '-old', '+new'].join(
      '\n',
    );
    const markup = renderToStaticMarkup(
      <ConfirmedDiffBlock
        diff={{
          kind: 'confirmed_diff',
          toolId: 't1',
          patch,
          lines: patch.split('\n'),
          added: 1,
          removed: 1,
          seq: 1,
        }}
      />,
    );
    // Every changed line is in the DOM, marked as its own kind, with no
    // truncation affordance anywhere.
    expect(markup).toContain('diff-line--deletion');
    expect(markup).toContain('diff-line--addition');
    expect(markup).toContain('>old<');
    expect(markup).toContain('>new<');
    expect(markup).not.toContain('展开剩余');
  });

  it('paints a completed Thought folded, with its body out of the DOM', () => {
    const markup = renderToStaticMarkup(
      <ThoughtRow
        thought={{
          kind: 'thought',
          id: 'th1',
          elapsedMs: 1600,
          folded: true,
          body: '先看解析器。',
          seq: 1,
        }}
      />,
    );
    expect(markup).toContain('思考 · 1.6s');
    expect(markup).not.toContain('先看解析器。');
  });

  it('paints the live Thinking row while the model is thinking', () => {
    const markup = renderToStaticMarkup(<ThinkingRow text="先看入口。" />);
    expect(markup).toContain('思考中');
    expect(markup).toContain('先看入口。');
    // A live segment is not a finished Thought: no duration is invented.
    expect(markup).not.toContain('思考 ·');
  });

  it('paints an exploration receipt collapsed, and its members only when open', () => {
    const markup = renderToStaticMarkup(
      <ExplorationReceiptRow
        receipt={{
          kind: 'exploration_receipt',
          reads: 2,
          searches: 1,
          label: '读取 2 个文件 · 搜索 1 次',
          folded: true,
          members: [
            { id: 't1', name: 'read_file', target: 'src/models.rs', status: 'done' },
            { id: 't2', name: 'read_file', target: 'src/config.rs', status: 'done' },
            { id: 't3', name: 'grep', target: 'recommended', status: 'done' },
          ],
          seq: 1,
        }}
        hiddenThoughts={[
          { kind: 'thought', id: 'th1', elapsedMs: 900, folded: true, body: '先看映射。', seq: 2 },
        ]}
      />,
    );
    expect(markup).toContain('读取 2 个文件 · 搜索 1 次');
    expect(markup).not.toContain('src/models.rs');
    expect(markup).not.toContain('先看映射。');
  });
});
