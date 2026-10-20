// Trajectory projection: durable runtime events become a turn-aware ledger.
// This module is presentation-only. It never invents timing, token usage, or
// system-prompt content that the runtime did not record; absent data is
// represented as `null` and surfaced by the view as unavailable.

const LABELS = {
  user: '用户',
  assistant: '助手',
  tool: '工具',
  context: '上下文',
  compaction: '压缩',
  failure: '失败',
  shell: '用户命令',
  subagent: '子智能体',
};

function number(value, fallback = 0) {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function messageKind(event) {
  return event?.message?.kind === 'runtime_notice' ? 'context' : 'user';
}

function textOf(value) {
  return typeof value === 'string' ? value : '';
}

function usageOf(usage) {
  if (!usage || typeof usage !== 'object') return null;
  const input = Number(usage.input_tokens);
  const output = Number(usage.output_tokens);
  const cached = Number(usage.cached_input_tokens);
  return {
    input: Number.isFinite(input) ? input : null,
    output: Number.isFinite(output) ? output : null,
    cached: Number.isFinite(cached) ? cached : 0,
    ...(Number.isFinite(Number(usage.reasoning_tokens))
      ? { reasoning: Number(usage.reasoning_tokens) }
      : {}),
  };
}

/**
 * Project the runtime's durable history entries into observation records.
 * @param {readonly {turn_elapsed_ms?:number,turn_start?:boolean,event:object}[]} entries
 * @param {{usage?:object}} [options] live token usage for the latest request
 */
export function projectTrajectory(entries, options = {}) {
  const records = [];
  const tools = new Map();
  let turn = 1;
  let request = 0;
  let lastModelStep = 0;
  let reasoning = null;
  let assistant = null;

  const closeReasoning = status => {
    if (!reasoning) return;
    reasoning.status = status;
    if (status === 'interrupted' && reasoning.text.trim() === '') {
      const index = records.indexOf(reasoning);
      if (index >= 0) records.splice(index, 1);
    }
    reasoning = null;
  };

  const findTool = id => tools.get(String(id ?? '')) ?? null;

  for (const [index, entry] of (entries ?? []).entries()) {
    const event = entry?.event ?? entry;
    if (!event || typeof event !== 'object') continue;
    const at = number(entry.turn_elapsed_ms, 0);
    if (entry.turn_start) {
      closeReasoning('interrupted');
      if (records.length > 0) turn += 1;
      request = 0;
      lastModelStep = 0;
      assistant = null;
    }
    const type = event.type;
    if (type === 'user_message_added') {
      const message = event.message ?? {};
      records.push({
        id: `user:${message.id ?? index}`,
        turn, request: 0, kind: messageKind(event),
        label: LABELS[messageKind(event)],
        subtype: 'message',
        text: textOf(message.text),
        images: Number(message.images) || 0,
        startedMs: at, endedMs: at, durationMs: null,
        status: 'completed',
        usage: null, ttftMs: null, seq: index,
      });
    } else if (type === 'reasoning_started') {
      closeReasoning('interrupted');
      request = Math.max(request, lastModelStep + 1);
      reasoning = {
        id: `reasoning:${turn}:${request}:${index}`,
        turn, request, kind: 'assistant', label: LABELS.assistant,
        subtype: 'reasoning', text: '',
        startedMs: at, endedMs: at, durationMs: null,
        status: 'running', usage: null, ttftMs: null, seq: index,
      };
      records.push(reasoning);
    } else if (type === 'reasoning_delta' && reasoning) {
      reasoning.text += textOf(event.delta);
      reasoning.endedMs = at;
    } else if (type === 'reasoning_completed' && reasoning) {
      reasoning.endedMs = at;
      reasoning.durationMs = Number.isFinite(Number(event.elapsed_ms)) ? Number(event.elapsed_ms) : null;
      closeReasoning('completed');
    } else if (type === 'assistant_message_started') {
      const id = String(event.message_id ?? '');
      assistant = {
        id: `assistant:${id || index}`,
        turn, request: Math.max(request, lastModelStep + 1),
        kind: 'assistant', label: LABELS.assistant, subtype: 'text', text: '',
        startedMs: at, endedMs: at, firstDeltaMs: null, durationMs: null,
        status: 'running', usage: null, ttftMs: null, seq: index,
      };
      request = assistant.request;
      records.push(assistant);
    } else if (type === 'assistant_text_delta') {
      if (!assistant) {
        const id = String(event.message_id ?? '');
        assistant = {
          id: `assistant:${id || index}`,
          turn, request: Math.max(request, lastModelStep + 1),
          kind: 'assistant', label: LABELS.assistant, subtype: 'text', text: '',
          startedMs: at, endedMs: at, firstDeltaMs: at, durationMs: null,
          status: 'running', usage: null, ttftMs: null, seq: index,
        };
        request = assistant.request;
        records.push(assistant);
      }
      if (assistant.firstDeltaMs === null) {
        assistant.firstDeltaMs = at;
        assistant.ttftMs = Math.max(0, at - assistant.startedMs);
      }
      assistant.text += textOf(event.delta);
      assistant.endedMs = at;
    } else if (type === 'assistant_message_completed') {
      if (assistant) {
        assistant.endedMs = at;
        assistant.durationMs = assistant.firstDeltaMs === null
          ? null
          : Math.max(0, at - assistant.firstDeltaMs);
        assistant.status = 'completed';
        assistant = null;
      }
    } else if (type === 'assistant_attempt_reset') {
      if (assistant) {
        assistant.status = 'interrupted';
        assistant.endedMs = at;
        assistant = null;
      }
    } else if (type === 'tool_call_started') {
      closeReasoning('completed');
      const id = String(event.id ?? '');
      if (Number.isFinite(Number(event.model_step))) {
        lastModelStep = Math.max(lastModelStep, Number(event.model_step));
        request = Number(event.model_step);
      } else {
        request = Math.max(request, 1);
      }
      const record = {
        id: `tool:${id || index}`,
        turn, request, kind: 'tool', label: LABELS.tool, subtype: 'call',
        callId: id, toolName: textOf(event.name),
        arguments: textOf(event.arguments), preview: '',
        startedMs: at, endedMs: null, durationMs: null,
        status: 'running', ok: null, exitCode: null, appliedDiff: null,
        usage: null, ttftMs: null, seq: index,
      };
      tools.set(id, record);
      records.push(record);
    } else if (type === 'tool_call_completed') {
      const id = String(event.id ?? '');
      let record = findTool(id);
      if (!record) {
        record = {
          id: `tool:${id || index}`,
          turn, request: Math.max(request, 1), kind: 'tool', label: LABELS.tool, subtype: 'call',
          callId: id, toolName: '', arguments: '', preview: '',
          startedMs: null, endedMs: at, durationMs: null,
          status: 'unknown', ok: null, exitCode: null, appliedDiff: null,
          usage: null, ttftMs: null, seq: index,
        };
        tools.set(id, record);
        records.push(record);
      }
      record.endedMs = at;
      record.durationMs = Number.isFinite(Number(event.duration_ms)) ? Number(event.duration_ms) : null;
      record.preview = textOf(event.preview);
      record.ok = event.ok === true;
      record.status = event.stop ? 'stopped' : event.ok === true ? 'success' : 'failed';
      record.exitCode = Number.isFinite(Number(event.exit_code)) ? Number(event.exit_code) : null;
      record.appliedDiff = typeof event.applied_diff === 'string' ? event.applied_diff : null;
    } else if (type === 'context_compacted') {
      records.push({
        id: `compaction:${index}`, turn, request: 0, kind: 'compaction', label: LABELS.compaction,
        subtype: 'compaction', text: `上下文已压缩 ${Number(event.from) || 0} → ${Number(event.to) || 0} 条`,
        startedMs: at, endedMs: at, durationMs: null, status: 'completed',
        usage: null, ttftMs: null, seq: index,
      });
    } else if (type === 'user_shell_started') {
      const id = String(event.execution_id ?? index);
      const record = {
        id: `shell:${id}`, turn, request: 0, kind: 'shell', label: LABELS.shell,
        subtype: 'shell', text: textOf(event.command), preview: '',
        startedMs: at, endedMs: null, durationMs: null, status: 'running',
        usage: null, ttftMs: null, seq: index,
      };
      tools.set(`shell:${id}`, record);
      records.push(record);
    } else if (type === 'user_shell_output' && tools.has(`shell:${String(event.execution_id)}`)) {
      const record = tools.get(`shell:${String(event.execution_id)}`);
      record.preview += textOf(event.chunk);
    } else if (type === 'user_shell_exited' && tools.has(`shell:${String(event.execution_id)}`)) {
      const record = tools.get(`shell:${String(event.execution_id)}`);
      record.endedMs = at;
      record.durationMs = Number.isFinite(Number(event.duration_ms)) ? Number(event.duration_ms) : null;
      record.status = textOf(event.status) || (event.exit_code === 0 ? 'success' : 'failed');
      record.exitCode = Number.isFinite(Number(event.exit_code)) ? Number(event.exit_code) : null;
    } else if (type === 'turn_failed') {
      records.push({
        id: `failure:${index}`, turn, request, kind: 'failure', label: LABELS.failure,
        subtype: 'failure', text: textOf(event.failure?.message) || textOf(event.error) || 'Runtime 执行失败',
        startedMs: at, endedMs: at, durationMs: null, status: 'failed',
        usage: null, ttftMs: null, seq: index,
      });
    } else if (type === 'sub_agent_updated' || type === 'sub_agent_state_changed') {
      const id = String(event.id ?? index);
      let record = records.find(item => item.kind === 'subagent' && item.subagentId === id);
      if (!record) {
        record = {
          id: `subagent:${id}`, turn, request, kind: 'subagent', label: LABELS.subagent,
          subtype: 'subagent', subagentId: id, text: '',
          startedMs: at, endedMs: at, durationMs: null, status: 'running',
          usage: null, ttftMs: null, seq: index,
        };
        records.push(record);
      }
      record.text = textOf(event.title) || textOf(event.nickname) || record.text;
      if (event.done === true) record.status = event.ok === true ? 'completed' : 'failed';
      record.endedMs = at;
    } else if (type === 'sub_agent_activity') {
      const id = String(event.id ?? '');
      const record = records.find(item => item.kind === 'subagent' && item.subagentId === id);
      if (record) {
        record.text = [textOf(event.tool), textOf(event.preview)].filter(Boolean).join(' · ') || record.text;
        record.endedMs = at;
      }
    }
  }
  closeReasoning('running');
  if (assistant) assistant.status = 'running';

  // Attach live token usage to the latest assistant record only. Historical
  // requests remain unavailable rather than inheriting a later measurement.
  const usage = usageOf(options.usage);
  if (usage) {
    const target = [...records].reverse().find(record => record.kind === 'assistant' && record.subtype === 'text');
    if (target) target.usage = usage;
  }

  const turns = [];
  for (const record of records) {
    let bucket = turns.find(item => item.index === record.turn);
    if (!bucket) {
      bucket = { index: record.turn, records: [] };
      turns.push(bucket);
    }
    bucket.records.push(record);
  }
  for (const bucket of turns) {
    const starts = bucket.records.map(record => record.startedMs).filter(value => value !== null);
    const ends = bucket.records.map(record => record.endedMs ?? record.startedMs).filter(value => value !== null);
    bucket.startMs = starts.length ? Math.min(...starts) : null;
    bucket.endMs = ends.length ? Math.max(...ends) : null;
  }

  return {
    records,
    turns,
    system: { available: false, reason: 'Runtime 未记录系统提示词' },
    usageAvailable: usage !== null,
  };
}

/** Return the tail window of a ledger, anchored at the newest record. */
export function trajectoryWindow(records, limit) {
  const total = records.length;
  const size = Number.isInteger(limit) && limit > 0 ? limit : 50;
  if (total <= size) return { records, total, hasOlder: false };
  return { records: records.slice(-size), total, hasOlder: true };
}

/** Return the ids whose recorded text, arguments, result, or tool name match the query. */
export function searchTrajectory(records, query) {
  const needle = String(query ?? '').trim().toLowerCase();
  if (!needle) return new Set();
  const matches = new Set();
  for (const record of records) {
    const haystack = [
      record.label, record.toolName, record.arguments, record.text, record.preview,
    ].filter(Boolean).join('\n').toLowerCase();
    if (haystack.includes(needle)) matches.add(record.id);
  }
  return matches;
}

/** Project records into three timeline lanes, using recorded spans or equal slots. */
export function buildTimeline(records, actualDuration = false) {
  const laneOf = record => record.kind === 'tool' || record.kind === 'shell'
    ? 'tool'
    : record.kind === 'assistant' ? 'model' : 'input';
  if (actualDuration) {
    let total = 0;
    const items = records.map(record => {
      const start = number(record.startedMs, 0);
      const end = Math.max(start, number(record.endedMs ?? record.startedMs, start));
      total = Math.max(total, end);
      return { id: record.id, kind: record.kind, lane: laneOf(record), start, end };
    });
    return { items, total: Math.max(1, total) };
  }
  // Equal-width mode lays each lane out contiguously; actual-duration mode
  // keeps the recorded turn-relative positions and spans.
  const counts = { input: 0, model: 0, tool: 0 };
  const items = [];
  for (const record of records) {
    const lane = laneOf(record);
    const start = counts[lane];
    counts[lane] += 1;
    items.push({ id: record.id, kind: record.kind, lane, start, end: start + 1 });
  }
  return { items, total: Math.max(1, counts.input, counts.model, counts.tool) };
}

/** Compact duration text for the ledger; null/negative values stay unavailable. */
export function formatDuration(ms) {
  if (ms === null || ms === undefined || !Number.isFinite(Number(ms)) || Number(ms) < 0) return null;
  const value = Number(ms);
  if (value < 1000) return `${Math.round(value)} 毫秒`;
  if (value < 60_000) return `${(value / 1000).toFixed(value < 10_000 ? 1 : 0)} 秒`;
  return `${Math.floor(value / 60_000)} 分 ${Math.round((value % 60_000) / 1000)} 秒`;
}
