// Execution Presentation Contract v1 — Web conformance.
//
// Reads the SAME fixtures the reference implementation is frozen against
// (`testdata/execution_presentation/v1/*.json`), drives the real Web reducer
// through `RuntimeBridge.applyEvent`, projects the resulting session view onto
// the contract's semantic tree, and compares it with the frozen expectation.
//
// A path that needs durable history replay is DEFERRED here, not silently
// skipped: the Web client has no `query_session_history` consumer yet, so a
// session opened from a snapshot cannot rebuild its finished tool rounds. See
// contract v1 §I10 (fixture C10) and the Web test
// `a snapshot-opened session cannot rebuild durable tool rounds yet`.

import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import type { Action, AppState, SessionView } from '../state/store';
import { initialState, reducer } from '../state/store';
import type { RuntimeEvent } from '../types/protocol';
import { RuntimeBridge } from './controller';
import { groupExecutionRounds, projectTurn } from './executionRounds';
import { foldedThoughts, turnBlocks } from './conversationPresentation';
import { ExplorationReceiptRow } from '../components/ExplorationReceiptRow';
import { ToolCallRow } from '../components/ToolCallRow';
import { isTurnUser } from './presentationKind';

/**
 * The SAME corpus the reference implementation is frozen against, imported as
 * raw text so the test reads the committed JSON rather than a copy.
 */
import fixtureC1 from '../../../testdata/execution_presentation/v1/C1.json?raw';
import fixtureC2 from '../../../testdata/execution_presentation/v1/C2.json?raw';
import fixtureC3 from '../../../testdata/execution_presentation/v1/C3.json?raw';
import fixtureC4 from '../../../testdata/execution_presentation/v1/C4.json?raw';
import fixtureC5 from '../../../testdata/execution_presentation/v1/C5.json?raw';
import fixtureC6 from '../../../testdata/execution_presentation/v1/C6.json?raw';
import fixtureC7 from '../../../testdata/execution_presentation/v1/C7.json?raw';
import fixtureC8 from '../../../testdata/execution_presentation/v1/C8.json?raw';
import fixtureC9 from '../../../testdata/execution_presentation/v1/C9.json?raw';
import fixtureC10 from '../../../testdata/execution_presentation/v1/C10.json?raw';
import fixtureC11 from '../../../testdata/execution_presentation/v1/C11.json?raw';
import fixtureC12 from '../../../testdata/execution_presentation/v1/C12.json?raw';
import fixtureC13 from '../../../testdata/execution_presentation/v1/C13.json?raw';
import fixtureC14 from '../../../testdata/execution_presentation/v1/C14.json?raw';

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
  fixtureC11,
  fixtureC12,
  fixtureC13,
  fixtureC14,
];

// Render the real tool rows without a live app dispatch provider.
vi.mock('../state/store', async (original) => ({
  ...await original<typeof import('../state/store')>(),
  useAppDispatch: () => () => {},
}));

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
  apply: (ev: RuntimeEvent) => void;
}

function harness(): Harness {
  const state: AppState = structuredClone(initialState);
  // Past the hero screen: a draft session would swallow the opening snapshot.
  state.draft = false;
  const dispatch = (action: Action) => reducer(state, action);
  const bridge = new RuntimeBridge(dispatch, () => state);
  (bridge as unknown as { ws: { send: () => boolean; setSession: () => void } }).ws = {
    send: () => true,
    setSession: () => {},
  };
  const apply = (ev: RuntimeEvent) =>
    (bridge as unknown as { applyEvent: (ev: RuntimeEvent) => void }).applyEvent(ev);
  return { state, apply };
}

/** Web tool status -> the contract's frozen lifecycle vocabulary. */
function contractStatus(status: string): string {
  switch (status) {
    case 'run':
      return 'running';
    case 'done':
      return 'ok';
    case 'fail':
      return 'failed';
    default:
      return status;
  }
}

/** The Web projection of one session onto the contract's semantic tree. */
function project(session: SessionView) {
  const turnEnded = session.lastTurn !== null;
  const ordered: Array<{ seq: number; item: Record<string, unknown> }> = [];
  for (const item of projectTurn(session.messages, session.tools, turnEnded)) {
    if (item.kind === 'execution_round') {
      const round = item.round;
      ordered.push({
        seq: item.seq,
        item: {
          kind: 'execution_round',
          model_step: round.modelStep,
          status: round.status,
          all_ok: round.allOk,
          batches: round.batches,
          tools: round.tools.map((t) => ({
            id: t.id,
            name: t.name,
            status: contractStatus(t.status),
          })),
        },
      });
      continue;
    }
    ordered.push({ seq: item.seq, item: { kind: item.kind, text: item.text } });
  }
  // A runtime-authored message is a note, never user speech (§I13).
  for (const message of session.messages) {
    if (message.kind !== 'runtime_notice') continue;
    ordered.push({ seq: message.seq, item: { kind: 'note', text: message.text } });
  }
  ordered.sort((a, b) => a.seq - b.seq);
  const items = ordered.map((entry) => entry.item);
  if (session.lastTurn) {
    // The turn terminal is presentation state in the Web view (the footer),
    // not a transcript message: it still belongs to the semantic tree.
    items.push({ kind: 'turn_end', status: session.lastTurn.outcome });
  }
  const user_texts = session.messages.filter(isTurnUser).map((m) => m.text);
  // Reasoning is live chrome in the store, never a transcript message.
  const reasoning_visible =
    session.reasoning !== '' && session.messages.some((m) => m.text.includes(session.reasoning));
  return { items, user_texts, reasoning_visible };
}

/** Actual collapsed exploration and tool-failure component output. */
function renderedText(session: SessionView): string {
  const blocks = turnBlocks(foldedThoughts(session.thoughts), groupExecutionRounds(session.tools));
  const markup = blocks.map((block) => {
    if (block.kind === 'receipt') {
      return renderToStaticMarkup(createElement(ExplorationReceiptRow, {
        receipt: block.receipt,
        hiddenThoughts: block.thoughts,
      }));
    }
    if (block.kind === 'round') {
      return block.round.tools.map((tool) =>
        renderToStaticMarkup(createElement(ToolCallRow, { tool }))).join(' ');
    }
    return '';
  }).join(' ');
  // Component boundaries carry the visual spacing between glyphs and labels.
  return markup.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
}

interface Fixture {
  id: string;
  title: string;
  paths: Record<string, Array<Record<string, unknown>>>;
  expect?: unknown;
  rendered?: { contains?: string[]; excludes?: string[] };
}

function fixtures(): Fixture[] {
  const parsed = FIXTURE_SOURCES.map((raw) => JSON.parse(raw) as Fixture);
  return parsed.sort((a, b) => a.id.localeCompare(b.id, undefined, { numeric: true }));
}

const DEFAULT_SNAPSHOT: RuntimeEvent = {
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
  },
};

function runPath(steps: Array<Record<string, unknown>>): Harness {
  const h = harness();
  h.apply(DEFAULT_SNAPSHOT);
  for (const step of steps) {
    // A durable-history step is answered the way the runtime answers a client's
    // own query: one `session_history_loaded` carrying normalized events. The
    // client replays them through the SAME reducer the live stream uses.
    if (step.history !== undefined) {
      h.apply({
        type: 'session_history_loaded',
        session_id: 's1',
        entries: step.history as never,
      } as RuntimeEvent);
      continue;
    }
    const payload = step.event ?? step.snapshot;
    if (!payload) continue;
    h.apply(payload as RuntimeEvent);
  }
  return h;
}

describe('execution presentation contract v1 (web)', () => {
  for (const fixture of fixtures()) {
    it(`${fixture.id} — ${fixture.title}`, () => {
      let compared = 0;
      for (const [name, steps] of Object.entries(fixture.paths)) {
        const h = runPath(steps);
        const current = h.state.current;
        expect(current, `${fixture.id}/${name}: no session`).not.toBeNull();
        if (!current) return;
        expect(project(current), `${fixture.id}/${name}`).toEqual(fixture.expect);
        if (fixture.rendered?.contains) {
          const text = renderedText(current);
          for (const needle of fixture.rendered.contains) {
            expect(text, `${fixture.id}/${name}: missing ${needle}`).toContain(needle);
          }
        }
        if (fixture.rendered?.excludes) {
          const text = renderedText(current);
          for (const needle of fixture.rendered.excludes) {
            expect(text, `${fixture.id}/${name}: leaked ${needle}`).not.toContain(needle);
          }
        }
        compared += 1;
      }
      expect(compared, `${fixture.id}: every declared path is deferred`).toBeGreaterThan(0);
    });
  }
});

describe('durable history replay (the gap this closed)', () => {
  it('a reopened session replays its whole conversation, not the model context', () => {
    // Before the replay consumer existed, a session opened from a snapshot
    // showed `snapshot.messages` — the ACTIVE MODEL CONTEXT, which `/compact`
    // trims to one summary row. The durable log is what a reopen must paint.
    const compacted = {
      type: 'session_opened',
      session: {
        ...(DEFAULT_SNAPSHOT as unknown as { session: Record<string, unknown> }).session,
        // Everything `/compact` leaves in the model context.
        messages: [{ id: 'summary', role: 'user', text: '对话摘要（已压缩历史）：前面聊过解析器。' }],
      },
    } as unknown as RuntimeEvent;
    const history = [
      { turn_start: true, event: { type: 'user_message_added', message: { id: 'u1', role: 'user', text: '看下解析器' } } },
      { turn_start: false, event: { type: 'assistant_message_started', message_id: 'm1' } },
      { turn_start: false, event: { type: 'assistant_text_delta', message_id: 'm1', delta: '解析器没问题。' } },
      { turn_start: false, event: { type: 'assistant_message_completed', message_id: 'm1' } },
      { turn_start: false, event: { type: 'turn_answered' } },
      { turn_start: true, event: { type: 'context_compacted', from: 4, to: 1 } },
    ];
    const h = harness();
    h.apply(compacted);
    // The snapshot alone cannot see the conversation: only the summary row.
    expect(h.state.current?.messages.map((m) => m.text)).toEqual([
      '对话摘要（已压缩历史）：前面聊过解析器。',
    ]);
    h.apply({
      type: 'session_history_loaded',
      session_id: 's1',
      entries: history as never,
    } as RuntimeEvent);
    const texts = h.state.current?.messages.map((m) => m.text) ?? [];
    expect(texts).toContain('看下解析器');
    expect(texts).toContain('解析器没问题。');
    // The internal summary is a context artifact, never a conversation row.
    expect(texts.some((text) => text.includes('对话摘要（已压缩历史）'))).toBe(false);
    expect(h.state.current?.lastTurn?.outcome).toBe('answered');
  });

  it('an empty history answer keeps the snapshot the client already has', () => {
    const h = harness();
    h.apply({
      type: 'session_opened',
      session: {
        ...(DEFAULT_SNAPSHOT as unknown as { session: Record<string, unknown> }).session,
        messages: [{ id: 'm1', role: 'assistant', text: '你好。' }],
      },
    } as unknown as RuntimeEvent);
    h.apply({
      type: 'session_history_loaded',
      session_id: 's1',
      entries: [],
    } as RuntimeEvent);
    expect(h.state.current?.messages.map((m) => m.text)).toEqual(['你好。']);
  });
});
