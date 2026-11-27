// Presentation and form rules for the MCP settings page.
//
// The runtime is the only authority for configuration, connection state and
// the discovered catalog. This module never derives "connected" from a
// configured row, never parses prose to decide a state, and never holds a
// secret value: environment entries are name references in both directions.
// The IPC boundary and the runtime validate the same request again.

const STATE_VIEW = Object.freeze({
  disabled: { label: '已停用', tone: 'muted' },
  not_connected: { label: '已配置，未连接', tone: 'muted' },
  connecting: { label: '正在连接', tone: 'run' },
  connected: { label: '已连接', tone: 'ok' },
  degraded: { label: '已断开', tone: 'warn' },
  failed: { label: '连接失败', tone: 'danger' },
  unknown: { label: '状态不可确认', tone: 'muted' },
});

const TRANSPORT_LABEL = Object.freeze({
  stdio: 'stdio',
  streamable_http: 'Streamable HTTP',
});

const ADMISSION_VIEW = Object.freeze({
  admitted: { label: '可调用', tone: 'ok' },
  requires_approval: { label: '调用需审批', tone: 'warn' },
  requires_network_grant: { label: '需要网络授权', tone: 'warn' },
  unknown: { label: '准入未确认', tone: 'muted' },
});

export function emptyMcpView() {
  return {
    servers: [],
    loading: false,
    loaded: false,
    error: null,
    actionError: null,
    notice: null,
    busy: null,
    selected: null,
    sessionId: null,
    listOutstanding: 0,
    refreshQueued: false,
    lastListQuery: null,
  };
}

export function mcpStateView(server) {
  const entry = STATE_VIEW[server?.state] ?? { label: String(server?.state ?? '未知'), tone: 'muted' };
  return { ...entry, state: server?.state ?? null };
}

export function mcpStateDetail(server) {
  switch (server?.state) {
    case 'disabled': return '已在配置中停用；Runtime 不会连接或发现它的工具。';
    case 'not_connected': return '已启用，但 Runtime 尚未完成一次连接尝试。';
    case 'connecting': return 'Runtime 正在执行握手与工具发现；当前答案不是终态。';
    case 'connected': {
      const tools = server.tool_count;
      const count = Number.isInteger(tools) ? `${tools} 个工具` : '工具数未知';
      return server.protocol ? `握手完成，协议 ${server.protocol}，${count}。` : `握手完成，${count}。`;
    }
    case 'degraded': return '此前的连接已不可用；工具目录已撤回，Runtime 会在下一次请求或重连时重新尝试。';
    case 'failed': return server.last_error ? `最近一次连接尝试失败：${server.last_error}` : '最近一次连接尝试失败。';
    case 'unknown': return 'Runtime 无法观察这个 Server 的状态。';
    default: return '';
  }
}

export function mcpTransportLabel(transport) {
  return TRANSPORT_LABEL[transport] ?? String(transport ?? '未知');
}

export function mcpAdmissionView(admission) {
  return ADMISSION_VIEW[admission] ?? { label: String(admission ?? '未提供'), tone: 'muted' };
}

export function mcpCounts(servers = []) {
  return {
    configured: servers.length,
    connected: servers.filter(server => server.state === 'connected').length,
    tools: servers.reduce((sum, server) => sum + (Number.isInteger(server.tool_count) ? server.tool_count : 0), 0),
  };
}

export function mcpToolRequired(tool) {
  const required = tool?.input_schema?.required;
  if (!Array.isArray(required)) return [];
  return required.filter(name => typeof name === 'string' && name.length > 0).slice(0, 32);
}

export function mcpToolSchemaText(tool) {
  if (tool?.input_schema?.['x-leveler-truncated'] === true) return 'Schema 已被 Runtime 截断，无法展示完整定义。';
  try {
    return JSON.stringify(tool?.input_schema ?? {}, null, 2);
  } catch {
    return 'Schema 无法序列化。';
  }
}

/** Why the last attempt failed, in the runtime's own category. The runtime
 *  prefixes a discovery failure explicitly; everything else is a connection or
 *  handshake failure whose redacted detail is the evidence. */
export function mcpFailureCategory(message) {
  if (typeof message !== 'string' || !message) return null;
  return message.startsWith('tools/list failed:') ? '工具发现失败' : '连接或初始化失败';
}

function validName(value) {
  return typeof value === 'string' && /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/.test(value.trim());
}

function normalizeEnvName(raw) {
  return String(raw ?? '').trim().replace(/^\$/, '');
}

function validEnvName(value) {
  return /^[A-Z_][A-Z0-9_]*$/.test(value);
}

function validEnvKey(value) {
  return /^[A-Za-z_][A-Za-z0-9_]*$/.test(value);
}

function validHeaderName(value) {
  return /^[A-Za-z0-9_][A-Za-z0-9_-]*$/.test(value);
}

/** Parse `KEY=SOURCE` lines. A bare `KEY` means the environment variable of the
 * same name. Only names travel: a value that does not look like a name is
 * refused instead of silently becoming configuration. */
export function parseNameRefs(text, { kind = 'env' } = {}) {
  const entries = [];
  const lines = String(text ?? '').split('\n');
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index].replace(/\r$/, '').trim();
    if (!line || line.startsWith('#')) continue;
    const at = line.indexOf('=');
    const key = (at < 0 ? line : line.slice(0, at)).trim();
    const source = normalizeEnvName(at < 0 ? key : line.slice(at + 1));
    if (!key) return { error: `第 ${index + 1} 行缺少名称` };
    const keyValid = kind === 'header' ? validHeaderName(key) : validEnvKey(key);
    if (!keyValid) return { error: `第 ${index + 1} 行的名称不合法` };
    if (kind === 'header' && key.toLowerCase() === 'authorization') {
      return { error: 'Authorization 请使用 Bearer Token 环境变量，不要作为普通 Header' };
    }
    if (!source || !validEnvName(source)) {
      return { error: `第 ${index + 1} 行的来源必须是环境变量名（大写字母、数字、下划线），不要填写密钥值` };
    }
    if (entries.some(entry => entry.key === key)) return { error: `第 ${index + 1} 行的名称重复` };
    entries.push({ key, source });
  }
  return { entries };
}

export function formatNameRefs(refs = []) {
  return refs.map(reference => `${reference.key}=${reference.source}`).join('\n');
}

function validHttpUrl(raw) {
  const text = String(raw ?? '').trim();
  if (!text || text.length > 2048 || /\s/.test(text)) return null;
  let url;
  try { url = new URL(text); } catch { return null; }
  if (!['http:', 'https:'].includes(url.protocol)) return null;
  if (url.username || url.password || url.search) return null;
  return text;
}

/** Convert the editor form into the wire declaration. Only the fields of the
 *  selected transport are emitted; values held by the hidden transport are not
 *  carried across a switch. */
export function specFromForm(form = {}) {
  const name = String(form.name ?? '').trim();
  if (!validName(name)) return { error: '服务名只能使用字母、数字、`-`、`_`、`.`，以字母或数字开头，最多 64 字节' };
  const enabled = form.enabled !== false;
  if (form.transport === 'stdio') {
    const command = String(form.command ?? '').trim();
    if (!command) return { error: 'stdio 服务需要启动命令' };
    if (/\s/.test(command)) return { error: '启动命令是单个程序路径，不能包含空格；参数请填在下方' };
    if (command.length > 4096) return { error: '启动命令超过长度上限' };
    const args = String(form.argsText ?? '')
      .split('\n')
      .map(line => line.replace(/\r$/, ''))
      .filter(line => line.length > 0);
    if (args.some(argument => argument.length > 8192)) return { error: '单个参数超过长度上限' };
    if (args.length > 256) return { error: '参数数量超过上限' };
    const env = parseNameRefs(form.envText, { kind: 'env' });
    if (env.error) return { error: `环境变量：${env.error}` };
    return {
      spec: {
        name,
        transport: 'stdio',
        command,
        args,
        env: env.entries,
        enabled,
      },
    };
  }
  if (form.transport === 'streamable_http') {
    const url = validHttpUrl(form.url);
    if (!url) return { error: 'URL 必须是不带凭据和查询串的 http(s) 地址' };
    const headers = parseNameRefs(form.headersText, { kind: 'header' });
    if (headers.error) return { error: `Headers：${headers.error}` };
    const bearer = normalizeEnvName(form.bearerTokenEnv);
    if (bearer && !validEnvName(bearer)) return { error: 'Bearer Token 环境变量名不合法' };
    return {
      spec: {
        name,
        transport: 'streamable_http',
        url,
        headers: headers.entries,
        ...(bearer ? { bearer_token_env: bearer } : {}),
        enabled,
      },
    };
  }
  return { error: '请选择 Runtime 支持的传输方式' };
}

/** Start an edit form from the runtime's declaration (name references only). */
export function formFromSpec(spec = {}) {
  return {
    name: spec.name ?? '',
    transport: spec.transport ?? 'stdio',
    command: spec.command ?? '',
    argsText: (spec.args ?? []).join('\n'),
    envText: formatNameRefs(spec.env ?? []),
    url: spec.url ?? '',
    headersText: formatNameRefs(spec.headers ?? []),
    bearerTokenEnv: spec.bearer_token_env ?? '',
    enabled: spec.enabled !== false,
  };
}

/** One-line success wording for a management intent. */
export function mcpIntentLabel(intent) {
  return {
    list: '已刷新',
    reload: '已重新加载配置',
    reconnect: '连接测试完成',
    add: '已添加服务',
    update: '已保存服务',
    remove: '已删除服务',
    set_enabled: '已更新启用状态',
  }[intent] ?? '已更新';
}

/** The answer this page accepts: only its own query on the selected session. */
export function mcpAnswer(frame, sessionId) {
  if (frame?.event !== 'runtime' || frame?.data?.type !== 'mcp_managed') return null;
  if (!sessionId || frame.data.requester_session_id !== sessionId) return null;
  const queryId = typeof frame.data.query_id === 'string' ? frame.data.query_id : null;
  if (!queryId) return null;
  return { queryId, response: frame.data.response ?? null };
}

export function mcpRows(response) {
  return Array.isArray(response?.servers) ? response.servers : [];
}

export function mcpResponseError(response) {
  return response?.status === 'error' ? (response.message || 'Runtime 拒绝了这次 MCP 操作') : null;
}

/** Whether a list answer may replace the rows currently shown. A newer list
 *  query already owns the page; an older answer is a fact from a past instant,
 *  not the current projection. */
export function mcpListAnswerIsCurrent(view, queryId) {
  return view?.lastListQuery === queryId;
}

/** Find a server row by identity. Never fall back to a display string. */
export function mcpServerByName(servers, name) {
  return servers.find(server => server.name === name) ?? null;
}
