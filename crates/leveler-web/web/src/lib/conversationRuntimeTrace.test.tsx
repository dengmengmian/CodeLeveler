import { beforeEach, expect, it, vi } from 'vitest';
import { renderToStaticMarkup } from 'react-dom/server';
import { initialState, reducer, type AppState, type Action } from '../state/store';
import { RuntimeBridge } from './controller';
import { Timeline } from '../components/Timeline';
import { AgentRunBlock } from '../components/AgentRunBlock';
import traceSource from '../../../testdata/conversation_presentation/wire-v1/full-execution.json?raw';

let state: AppState;
vi.mock('../state/store', async (original) => ({
  ...await original<typeof import('../state/store')>(),
  useAppState: () => state,
  useAppDispatch: () => () => {},
}));
vi.mock('../state/bridge', () => ({ useBridge: () => ({}) }));

beforeEach(() => {
  Object.assign(globalThis, {
    window: { location: { href: 'http://localhost/', protocol: 'http:', host: 'localhost' }, history: { replaceState() {} } },
    sessionStorage: { getItem: () => '', setItem() {}, removeItem() {} },
    localStorage: { getItem: () => null, setItem() {}, removeItem() {} },
  });
  state = structuredClone(initialState);
  state.draft = false;
  const bridge = new RuntimeBridge((action: Action) => reducer(state, action), () => state);
  Object.assign(bridge, { ws: { send: () => true, setSession() {} } });
  // Captured actual WS frames, through the same entry point as the browser.
  for (const frame of JSON.parse(traceSource).frames) {
    (bridge as unknown as { handleFrame(frame: unknown): void }).handleFrame(frame);
  }
});

it('actual completed wire thoughts remain in the latest Timeline process slot', () => {
  expect(state.current?.thoughts).toHaveLength(4);
  expect(state.current?.thoughts.map(t => t.elapsedMs)).toEqual([0, 0, 0, 0]);
  const html = renderToStaticMarkup(<Timeline />);
  expect((html.match(/data-thought-id=/g) ?? []).length).toBe(4);
});

it('actual confirmed filesystem diff is full and inline while its execution round is collapsed', () => {
  const html = renderToStaticMarkup(<AgentRunBlock variant="process" />);
  expect(html).toContain('class="confirmed-diff" data-tool-id="c5"');
  expect(html).toContain('gpt-5.6');
  expect(html).toContain('gpt-6');
  expect(html).toContain('--- a/src/models.rs');
  expect(html).toContain('+++ b/src/models.rs');
});

it('a completed thought-only turn remains visible without any tool rows', () => {
  if (!state.current) throw new Error('actual session missing');
  state.current.tools = [];
  const html = renderToStaticMarkup(<AgentRunBlock variant="process" />);
  expect((html.match(/data-thought-id=/g) ?? []).length).toBe(4);
});

it('actual zero-duration wire segments survive a deferred React reducer commit', () => {
  if (!state.current) throw new Error('actual session missing');
  state.current.thoughts = [];
  state.current.reasoning = '';
  const pending: Action[] = [];
  const bridge = new RuntimeBridge(action => pending.push(action), () => state);
  const events = JSON.parse(traceSource).frames.filter((frame: {type: string}) => frame.type === 'event').map((frame: {event: unknown}) => frame.event);
  const delta = events.find((event: {type: string}) => event.type === 'reasoning_delta');
  const completed = events.find((event: {type: string}) => event.type === 'reasoning_completed');
  const apply = (event: unknown) => (bridge as unknown as {applyEvent(event: unknown): void}).applyEvent(event);
  // Same incoming batch, while getState still exposes the previous render.
  apply(delta);
  apply(completed);
  for (const action of pending) reducer(state, action);
  expect(state.current.thoughts).toHaveLength(1);
  expect(state.current.thoughts[0].text).toBe(delta.delta);
  expect(state.current.thoughts[0].elapsedMs).toBe(0);
});
