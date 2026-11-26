// Provider onboarding and management facts for the Desktop.
//
// Presentation only: no IPC, no DOM, no storage. The host (desktop-bridge →
// leveler-app::provider_config) owns configuration truth; these functions turn
// the facts it returns into labels and decide when the home surface must ask
// the person to finish setup. Keeping them here makes that policy testable
// without an Electron window.

/** The request identity every provider IPC carries.
 *
 * A session determines the environment (its workspace may contribute project
 * providers). Without one, the composer's selected folder does. */
export function providerContext({selection = null, sessionId = null} = {}) {
  return sessionId ? {selection: null, sessionId} : {selection: selection ?? null, sessionId: null};
}

/** Whether the effective configuration can start a turn at all.
 *
 * Only the structural fact is used: at least one model bound to a configured
 * provider. Credential state is reported per provider; a keyless local
 * endpoint is legitimate, so a missing key is not a reason to block here. */
export function configurationUsable(data) {
  return Boolean(data && data.configuration_complete && data.models?.length);
}

/** What the home surface should say about configuration, or `null` when there
 * is nothing to say. Status derives only from the host's structured answer. */
export function setupGuidance(state) {
  if (!state) return null;
  if (state.status === 'error') {
    return {
      tone: 'error',
      title: '无法读取 Provider 配置',
      detail: state.error || 'Host 未能读取配置文件',
      action: 'retry',
      secondary: 'open-config',
    };
  }
  const data = state.data;
  if (!data) return null;
  if (data.configuration_complete) return null;
  if (!(data.providers?.length)) {
    return {
      tone: 'setup',
      title: '配置模型 Provider',
      detail: 'CodeLeveler 需要至少一个 Provider 和可用模型才能开始任务。',
      action: 'add-provider',
      secondary: 'open-settings',
    };
  }
  if (!(data.models?.length)) {
    const provider = data.providers.find(candidate => candidate.model_count === 0) ?? data.providers[0];
    return {
      tone: 'setup',
      title: 'Provider 已配置，还缺模型',
      detail: `为 ${provider.id} 添加一个模型后即可开始。`,
      action: 'add-model',
      providerId: provider.id,
      secondary: 'open-settings',
    };
  }
  return {
    tone: 'warning',
    title: 'Provider 配置需要处理',
    detail: (data.issues ?? []).join('；') || '当前配置不完整',
    action: 'open-settings',
    secondary: 'retry',
  };
}

/** How a provider's credential state is stated. Never includes a key. */
export function providerCredentialText(provider) {
  if (provider?.api_key_configured) {
    if (provider.key_source === 'environment') return `环境变量 ${provider.api_key_env || '（未命名）'}`;
    return '已保存密钥';
  }
  if (provider?.api_key_env) return `环境变量 ${provider.api_key_env} 未设置`;
  return '未配置凭据';
}

/** Whether the provider's key can be used at all. */
export function providerHasCredential(provider) {
  return Boolean(provider?.api_key_configured);
}

/** Label, tone and retry policy for one host connection result. */
export function connectionPresentation(result) {
  const status = result?.status ?? 'error';
  const table = {
    ok: {label: '连接正常', tone: 'success', retryable: false},
    auth: {label: '认证失败', tone: 'error', retryable: false},
    network: {label: '网络不可达', tone: 'error', retryable: true},
    timeout: {label: '响应超时', tone: 'error', retryable: true},
    rate_limit: {label: '暂时限流', tone: 'warning', retryable: true},
    server: {label: '服务端错误', tone: 'error', retryable: true},
    model_not_found: {label: '模型不可用', tone: 'error', retryable: false},
    incomplete: {label: '配置不完整', tone: 'warning', retryable: false},
    unsupported: {label: '服务不支持模型列表', tone: 'warning', retryable: false},
    malformed: {label: '响应无法识别', tone: 'error', retryable: true},
    error: {label: '连接失败', tone: 'error', retryable: true},
  };
  const entry = table[status] ?? table.error;
  return {...entry, status, message: result?.message || entry.label, models: result?.models ?? []};
}

/** Client-side completeness check for the editor. The host re-validates every
 * field; this only prevents an obviously incomplete submission. */
export function providerFormError({mode, id, protocol, baseUrl, apiKey, apiKeyEnv, models}) {
  if (!['add', 'edit'].includes(mode)) return '未知的保存模式';
  if (mode === 'add' && !String(id ?? '').trim()) return '请填写 Provider ID';
  if (mode === 'add' && !/^[a-z0-9][a-z0-9._-]{0,63}$/.test(String(id ?? '').trim())) {
    return 'Provider ID 只能使用小写字母、数字、`-`、`_`、`.`';
  }
  if (!['openai_chat', 'openai_responses', 'anthropic_messages', 'gemini_generate_content'].includes(protocol)) return '请选择接口协议';
  const url = String(baseUrl ?? '').trim();
  // On an edit an empty field means "keep the stored value"; only a new
  // provider must state an endpoint.
  if (mode === 'add' && !url) return '请填写 Base URL';
  if (url && !/^https?:\/\//.test(url)) return 'Base URL 必须以 https:// 或 http:// 开头';
  if (url && /\s/.test(url)) return 'Base URL 不能包含空白';
  if (String(apiKey ?? '').length > 16 * 1024) return 'API Key 超过长度上限';
  if (apiKeyEnv && !/^[A-Za-z_][A-Za-z0-9_]*$/.test(apiKeyEnv)) return '环境变量名只能使用字母、数字和下划线';
  if (mode === 'add' && !(models ?? []).length) return '请选择或填写一个模型 ID';
  for (const model of models ?? []) {
    if (!String(model.id ?? '').trim()) return '模型 ID 不能为空';
    if (String(model.id).includes('/') || /\s/.test(model.id)) return '模型 ID 不能包含 `/` 或空白';
    if (model.context_window !== null && model.context_window !== undefined
      && (!Number.isInteger(model.context_window) || model.context_window < 1024 || model.context_window > 16777216)) {
      return '上下文窗口必须是 1024 到 16777216 之间的整数';
    }
  }
  return null;
}

/** Models the picker may offer, ordered and deduplicated by the host view. */
export function providerModelOptions(data) {
  return (data?.models ?? []).map(model => ({provider: model.provider, model: model.model, default: model.default === true}));
}

/** One-line runtime consistency statement for the settings page. */
export function runtimeConfigState(runtime) {
  if (!runtime || runtime.connected !== true) {
    return {tone: 'muted', text: 'Runtime 未连接；配置会在下次连接时加载'};
  }
  if (runtime.config_current === true) return {tone: 'done', text: 'Runtime 正在使用当前配置'};
  if (runtime.config_current === false) {
    return {tone: 'warning', text: 'Runtime 尚未应用最新配置', canReload: true};
  }
  return {tone: 'muted', text: '无法确认 Runtime 与当前文件是否一致（工作区不同）', canReload: true};
}

export function modelRefText(model) {
  return model ? `${model.provider}/${model.model}` : '';
}
