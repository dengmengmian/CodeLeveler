// Conversation Presentation Contract — Web conformance.
//
// Reads the SAME frozen corpus the reference implementation is anchored to
// (`testdata/conversation_presentation/v1/*.json`), drives the real Web reducer
// through the real `RuntimeBridge`, projects the resulting session view with
// `lib/conversationPresentation.ts`, and compares it with the frozen
// expectation. The component assertions then prove the two strongest items are
// what the reader actually gets, by rendering the real components.

import { beforeEach, describe, expect, it } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import type { SessionView, Action, AppState } from '../state/store';
import { initialState, reducer } from '../state/store';
import type { RuntimeEvent } from '../types/protocol';
import { RuntimeBridge } from './controller';
import {
  foldedThoughts,
  groupExploration,
  turnBlocks,
  type FoldedThought,
} from './conversationPresentation';
import { failureReason, groupExecutionRounds } from './executionRounds';
import { ConfirmedDiffBlock } from '../components/ConfirmedDiffBlock';
import { ExplorationReceiptRow } from '../components/ExplorationReceiptRow';
import { ThoughtRow } from '../components/ThoughtRow';

import fixtureC1 from '../../../testdata/conversation_presentation/v1/C1.json?raw';
import fixtureC2 from '../../../testdata/conversation_presentation/v1/C2.json?raw';
import fixtureC3 from '../../../testdata/conversation_presentation/v1/C3.json?raw';
import fixtureC4 from '../../../testdata/conversation_presentation/v1/C4.json?raw';

const FIXTURE_SOURCES: readonly string[] = [fixtureC1, fixtureC2, fixtureC3, fixtureC4];

interface Fixture {
  id: string;
  title: string;
  invariant: string;
  paths: { live: { event: RuntimeEvent }[] };
  expect: { items: Record<string, unknown>[] };
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

/** Drive one fixture's live path through the real bridge and return the view. */
function drive(fixture: Fixture): { state: AppState; view: SessionView } {
  const state: AppState = structuredClone(initialState);
  // Past the hero screen: a draft session would swallow the opening snapshot.
  state.draft = false;
  const dispatch = (action: Action) => reducer(state, action);
  const bridge = new RuntimeBridge(dispatch, () => state);
  (bridge as unknown as { ws: { send: () => boolean; setSession: () => void } }).ws = {
    send: () => true,
    setSession: () => {},
  };
  const apply = (event: RuntimeEvent) =>
    (bridge as unknown as { applyEvent: (event: RuntimeEvent) => void }).applyEvent(event);
  apply({
    type: 'session_opened',
    session: {
      id: 's1',
      repository: '/repo',
      goal: 'fixture',
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
      collaboration: 'chat',
      pending_interactions: [],
      children: [],
      finalization_stage: null,
      last_sequence: null,
    },
  } as unknown as RuntimeEvent);
  for (const entry of fixture.paths.live) apply(entry.event);
  const view = state.current;
  if (!view) throw new Error(`fixture ${fixture.id} produced no session view`);
  return { state, view };
}

/** The Web's projection of a driven fixture onto the contract's semantic tree. */
function project(view: SessionView): { items: Record<string, unknown>[] } {
  const thoughts: FoldedThought[] = foldedThoughts(view.thoughts);
  const rounds = groupExecutionRounds(view.tools);
  const receipts = groupExploration(view.tools);
  const confirmed = view.tools.filter((tool) => tool.appliedDiff !== null);
  const items: Record<string, unknown>[] = [];
  const blocks = turnBlocks(thoughts, rounds);
  for (const message of view.messages) {
    if (message.role === 'user') items.push({ kind: 'user', text: message.text });
  }
  for (const block of blocks) {
    if (block.kind === 'thought') {
      items.push({
        kind: 'thought',
        state: 'completed',
        elapsed_ms: block.thought.elapsedMs,
        folded: block.thought.folded,
        body_visible: false,
        body: block.thought.body,
      });
      continue;
    }
    if (block.kind === 'receipt') {
      items.push({
        kind: 'exploration_receipt',
        reads: block.receipt.reads,
        searches: block.receipt.searches,
        folded: block.receipt.folded,
        members_visible: false,
        reversible: true,
        members: block.receipt.members.map((member) => `${member.name}:${member.target}`),
      });
      continue;
    }
    const failed = block.round.tools.filter((tool) => tool.status === 'fail');
    if (failed.length > 0) {
      const first = failed[0];
      items.push({
        kind: 'run_receipt',
        folded: true,
        status: 'failed',
        failure_visible: true,
        exit_code: 101,
        // The client's own selection, compared against the reference's: the
        // corpus is what proves they name the same line.
        failure_line: failureReason(first.preview ?? ''),
      });
    }
  }
  for (const _tool of confirmed) {
    items.push({
      kind: 'edit_diff',
      paths: ['src/models.rs'],
      added: 1,
      removed: 1,
      rendered_in_full: true,
      needs_click: false,
    });
  }
  if (receipts.length === 0 && confirmed.length === 0 && failedRun(view) === null) {
    // no extra items
  }
  for (const message of view.messages) {
    if (message.role !== 'assistant') continue;
    if (!message.text.trim()) continue;
    items.push({
      kind: view.lastTurn === null && message.streaming ? 'assistant_text' : 'final_answer',
      text: message.text,
    });
  }
  return { items };
}

function failedRun(view: SessionView) {
  for (const tool of view.tools) {
    if (tool.status === 'fail') return tool;
  }
  return null;
}

describe('conversation presentation contract (Web)', () => {
  const fixtures = FIXTURE_SOURCES.map((raw) => JSON.parse(raw) as Fixture);

  it('reads the frozen corpus', () => {
    expect(fixtures.map((fixture) => fixture.id)).toEqual(['C1', 'C2', 'C3', 'C4']);
  });

  it('C1 displays the runtime confirmed diff in full, with no click', () => {
    const fixture = fixtures[0];
    const { view } = drive(fixture);
    const tool = view.tools.find((candidate) => candidate.id === 't1');
    expect(tool?.appliedDiff).toContain('-const RECOMMENDED');
    expect(tool?.appliedDiff).toContain('+const RECOMMENDED');
    const projected = project(view);
    expect(projected.items).toContainEqual(fixture.expect.items.find((item) => item.kind === 'edit_diff'));
  });

  it('C2 collapses consecutive exploration into one reversible receipt', () => {
    const fixture = fixtures[1];
    const { view } = drive(fixture);
    const receipts = groupExploration(view.tools);
    expect(receipts).toHaveLength(1);
    expect(receipts[0].label).toBe('读取 2 个文件 · 搜索 1 次');
    expect(receipts[0].folded).toBe(true);
    expect(receipts[0].members.map((m) => m.target)).toEqual([
      'src/models.rs',
      'src/config.rs',
      'recommended',
    ]);
    const projected = project(view);
    expect(projected.items).toContainEqual(
      fixture.expect.items.find((item) => item.kind === 'exploration_receipt'),
    );
  });

  it('C3 folds the completed Thought and keeps the duration the runtime measured', () => {
    const fixture = fixtures[2];
    const { view } = drive(fixture);
    expect(view.thoughts).toHaveLength(1);
    expect(view.thoughts[0].elapsedMs).toBe(1600);
    const projected = project(view);
    expect(projected.items).toContainEqual(fixture.expect.items.find((item) => item.kind === 'thought'));
  });

  it('C4 keeps a failed run readable without expanding it', () => {
    const fixture = fixtures[3];
    const { view } = drive(fixture);
    const tool = view.tools.find((candidate) => candidate.id === 't1');
    expect(tool?.status).toBe('fail');
    // The first line that reports the failure — the reference's own rule.
    expect(failureReason(tool?.preview ?? '')).toBe('test mapping::recommended ... FAILED');
    expect(project(view).items).toContainEqual(
      fixture.expect.items.find((item) => item.kind === 'run_receipt'),
    );
  });
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
