import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  emptyMcpView, mcpStateView, mcpStateDetail, mcpTransportLabel, mcpAdmissionView,
  mcpCounts, mcpToolRequired, mcpToolSchemaText, mcpFailureCategory, parseNameRefs,
  formatNameRefs, specFromForm, formFromSpec, mcpIntentLabel, mcpAnswer, mcpRows,
  mcpResponseError, mcpListAnswerIsCurrent,
} from '../src/mcp-settings.mjs';

test('a configured MCP server is never presented as connected', () => {
  assert.equal(mcpStateView({ state: 'not_connected' }).label, '已配置，未连接');
  assert.equal(mcpStateView({ state: 'connected' }).label, '已连接');
  assert.equal(mcpStateView({ state: 'failed' }).tone, 'danger');
  assert.equal(mcpStateView({ state: 'degraded' }).tone, 'warn');
  assert.equal(mcpStateView({ state: 'disabled' }).label, '已停用');
  assert.equal(mcpStateView({ state: 'connecting' }).tone, 'run');
  assert.equal(mcpStateView({ state: 'unknown' }).label, '状态不可确认');
  assert.match(mcpStateDetail({ state: 'failed', last_error: 'tools/list failed: boom' }), /tools\/list failed/);
  assert.match(mcpStateDetail({ state: 'not_connected' }), /尚未完成一次连接尝试/);
});

test('MCP row facts keep server, tool and transport identities distinct', () => {
  const servers = [
    { state: 'connected', tool_count: 3 },
    { state: 'not_connected', tool_count: null },
    { state: 'failed', tool_count: 1 },
  ];
  assert.deepEqual(mcpCounts(servers), { configured: 3, connected: 1, tools: 4 });
  assert.equal(mcpTransportLabel('streamable_http'), 'Streamable HTTP');
  assert.equal(mcpAdmissionView('requires_network_grant').label, '需要网络授权');
  assert.equal(mcpAdmissionView('admitted').tone, 'ok');
  const tool = { input_schema: { type: 'object', required: ['query', 7, '', 'limit'] } };
  assert.deepEqual(mcpToolRequired(tool), ['query', 'limit']);
  assert.match(mcpToolSchemaText({ input_schema: { 'x-leveler-truncated': true } }), /截断/);
  assert.match(mcpToolSchemaText(tool), /"required"/);
});

test('environment references are names, never values', () => {
  assert.deepEqual(parseNameRefs('GITHUB_TOKEN\nOTHER=$SOURCE\n# comment\n\n').entries, [
    { key: 'GITHUB_TOKEN', source: 'GITHUB_TOKEN' },
    { key: 'OTHER', source: 'SOURCE' },
  ]);
  assert.match(parseNameRefs('TOKEN=sk-live-secret-value').error, /环境变量名/);
  assert.match(parseNameRefs('TOKEN=').error, /环境变量名/);
  assert.match(parseNameRefs('bad key=VALUE').error, /名称不合法/);
  assert.match(parseNameRefs('TOKEN=A\nTOKEN=B').error, /重复/);
  assert.match(parseNameRefs('Authorization=TOKEN', { kind: 'header' }).error, /Bearer Token/);
  assert.equal(formatNameRefs([{ key: 'A', source: 'B' }]), 'A=B');
});

test('the form builds only the selected transport declaration', () => {
  const stdio = specFromForm({
    name: 'echo', transport: 'stdio', command: ' /usr/bin/python3 ',
    argsText: '-u\n/tmp/echo.py\n', envText: 'API_KEY=API_KEY',
    url: 'https://ignored.example/mcp', headersText: 'X-Ignored=IGNORED',
    bearerTokenEnv: 'IGNORED_TOKEN', enabled: true,
  });
  assert.equal(stdio.error, undefined);
  assert.deepEqual(stdio.spec, {
    name: 'echo', transport: 'stdio', command: '/usr/bin/python3',
    args: ['-u', '/tmp/echo.py'], env: [{ key: 'API_KEY', source: 'API_KEY' }], enabled: true,
  });
  const http = specFromForm({
    name: 'remote', transport: 'streamable_http', url: ' https://example.com/mcp ',
    command: 'ignored', argsText: 'ignored', envText: 'IGNORED=IGNORED',
    headersText: 'X-API-Key=SERVICE_KEY\nAuthorization=TOKEN', bearerTokenEnv: '$SERVICE_TOKEN',
    enabled: false,
  });
  assert.match(http.error, /Bearer Token/);
  const ok = specFromForm({
    name: 'remote', transport: 'streamable_http', url: 'https://example.com/mcp',
    command: 'ignored', argsText: 'ignored', envText: 'IGNORED=IGNORED',
    headersText: 'X-API-Key=SERVICE_KEY', bearerTokenEnv: '$SERVICE_TOKEN', enabled: false,
  });
  assert.deepEqual(ok.spec, {
    name: 'remote', transport: 'streamable_http', url: 'https://example.com/mcp',
    headers: [{ key: 'X-API-Key', source: 'SERVICE_KEY' }],
    bearer_token_env: 'SERVICE_TOKEN', enabled: false,
  });
  for (const form of [
    { name: '', transport: 'stdio', command: 'x' },
    { name: '../escape', transport: 'stdio', command: 'x' },
    { name: 'ok', transport: 'stdio', command: ' ' },
    { name: 'ok', transport: 'stdio', command: 'npx -y server' },
    { name: 'ok', transport: 'streamable_http', url: 'ftp://example.com/mcp' },
    { name: 'ok', transport: 'streamable_http', url: 'https://user:pass@example.com/mcp' },
    { name: 'ok', transport: 'streamable_http', url: 'https://example.com/mcp?token=x' },
    { name: 'ok', transport: 'streamable_http', url: 'https://example.com/mcp', bearerTokenEnv: 'lowercase' },
    { name: 'ok', transport: 'sse', url: 'https://example.com/mcp' },
  ]) assert.ok(specFromForm(form).error, JSON.stringify(form));
});

test('editing starts from the runtime declaration and does not echo values', () => {
  const spec = {
    name: 'remote', transport: 'streamable_http', url: 'https://example.com/mcp',
    headers: [{ key: 'X-API-Key', source: 'SERVICE_KEY' }],
    bearer_token_env: 'SERVICE_TOKEN', enabled: true,
  };
  const form = formFromSpec(spec);
  assert.equal(form.headersText, 'X-API-Key=SERVICE_KEY');
  assert.equal(form.bearerTokenEnv, 'SERVICE_TOKEN');
  assert.deepEqual(specFromForm(form).spec, {
    name: 'remote', transport: 'streamable_http', url: 'https://example.com/mcp',
    headers: [{ key: 'X-API-Key', source: 'SERVICE_KEY' }],
    bearer_token_env: 'SERVICE_TOKEN', enabled: true,
  });
});

test('only the page-owned answer on the selected session is accepted', () => {
  const view = { ...emptyMcpView(), lastListQuery: 'q2' };
  const frame = (over = {}) => ({
    event: 'runtime',
    session_id: 's1',
    data: { type: 'mcp_managed', requester_session_id: 's1', query_id: 'q2', response: { status: 'snapshot', servers: [] }, ...over },
  });
  assert.deepEqual(mcpAnswer(frame(), 's1'), { queryId: 'q2', response: { status: 'snapshot', servers: [] } });
  assert.equal(mcpAnswer(frame(), 's2'), null);
  assert.equal(mcpAnswer(frame({ requester_session_id: 's2' }), 's1'), null);
  assert.equal(mcpAnswer(frame({ query_id: '' }), 's1'), null);
  assert.equal(mcpAnswer({ event: 'snapshot', data: {} }, 's1'), null);
  assert.equal(mcpListAnswerIsCurrent(view, 'q2'), true);
  assert.equal(mcpListAnswerIsCurrent(view, 'q1'), false);
  assert.deepEqual(mcpRows({ status: 'changed', servers: [{ name: 'x' }] }), [{ name: 'x' }]);
  assert.equal(mcpResponseError({ status: 'error', message: 'refused' }), 'refused');
  assert.equal(mcpResponseError({ status: 'snapshot', servers: [] }), null);
});

test('failure and intent wording are derived from structured fields', () => {
  assert.equal(mcpFailureCategory('tools/list failed: timeout'), '工具发现失败');
  assert.equal(mcpFailureCategory('spawn: not found'), '连接或初始化失败');
  assert.equal(mcpFailureCategory(null), null);
  assert.equal(mcpIntentLabel('reconnect'), '连接测试完成');
  assert.equal(mcpIntentLabel('unknown'), '已更新');
});
