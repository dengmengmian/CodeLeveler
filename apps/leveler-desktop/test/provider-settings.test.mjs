import test from 'node:test';
import assert from 'node:assert/strict';
import {
  configurationUsable, connectionPresentation, modelRefText, providerContext,
  providerCredentialText, providerFormError, runtimeConfigState, setupGuidance,
} from '../src/provider-settings.mjs';

const provider = (overrides = {}) => ({
  id: 'deepseek', protocol: 'openai_chat', base_url: 'https://api.deepseek.com',
  api_key_env: 'DEEPSEEK_API_KEY', api_key_configured: true, key_source: 'config',
  model_count: 1, source: 'global', editable: true, ...overrides,
});
const data = (overrides = {}) => ({
  configuration_complete: true,
  providers: [provider()],
  models: [{provider: 'deepseek', model: 'deepseek-flash', thinking: null, default: true}],
  issues: [],
  ...overrides,
});

test('a home without providers asks for one and never for a terminal command', () => {
  const guidance = setupGuidance({status: 'ready', data: data({providers: [], models: [], configuration_complete: false})});
  assert.equal(guidance.action, 'add-provider');
  assert.match(guidance.title, /Provider/);
  assert.doesNotMatch(JSON.stringify(guidance), /leveler login|终端/);
});

test('a configured provider without models names the provider to extend', () => {
  const guidance = setupGuidance({
    status: 'ready',
    data: data({models: [], configuration_complete: false, providers: [provider({model_count: 0})]}),
  });
  assert.equal(guidance.action, 'add-model');
  assert.equal(guidance.providerId, 'deepseek');
  assert.match(guidance.detail, /deepseek/);
});

test('an orphan model is a warning with the runtime issue text', () => {
  const guidance = setupGuidance({
    status: 'ready',
    data: data({configuration_complete: false, issues: ['模型 `m` 绑定的 Provider `x` 未配置']}),
  });
  assert.equal(guidance.action, 'open-settings');
  assert.equal(guidance.tone, 'warning');
  assert.match(guidance.detail, /未配置/);
});

test('a complete configuration says nothing and can start a turn', () => {
  assert.equal(setupGuidance({status: 'ready', data: data()}), null);
  assert.equal(configurationUsable(data()), true);
  assert.equal(configurationUsable(data({configuration_complete: false})), false);
  assert.equal(configurationUsable(data({models: []})), false);
});

test('a host read failure offers recovery instead of a blank state', () => {
  const guidance = setupGuidance({status: 'error', data: null, error: 'TOML parse error at line 2'});
  assert.equal(guidance.action, 'retry');
  assert.equal(guidance.secondary, 'open-config');
  assert.match(guidance.detail, /line 2/);
});

test('credential text distinguishes a stored key, an env var, and a missing one', () => {
  assert.equal(providerCredentialText(provider()), '已保存密钥');
  assert.equal(providerCredentialText(provider({key_source: 'environment'})), '环境变量 DEEPSEEK_API_KEY');
  assert.equal(providerCredentialText(provider({api_key_configured: false, key_source: 'none'})), '环境变量 DEEPSEEK_API_KEY 未设置');
  assert.equal(providerCredentialText(provider({api_key_configured: false, key_source: 'none', api_key_env: null})), '未配置凭据');
  for (const value of ['已保存密钥', providerCredentialText(provider({key_source: 'environment'}))]) {
    assert.doesNotMatch(value, /sk-/);
  }
});

test('connection statuses map to distinct labels and retry policy', () => {
  const auth = connectionPresentation({status: 'auth', message: '认证失败'});
  assert.equal(auth.label, '认证失败');
  assert.equal(auth.retryable, false);
  assert.equal(connectionPresentation({status: 'network'}).retryable, true);
  assert.equal(connectionPresentation({status: 'timeout'}).label, '响应超时');
  assert.equal(connectionPresentation({status: 'rate_limit'}).tone, 'warning');
  assert.equal(connectionPresentation({status: 'model_not_found'}).retryable, false);
  assert.equal(connectionPresentation({status: 'incomplete'}).label, '配置不完整');
  assert.equal(connectionPresentation({}).status, 'error');
});

test('form validation requires the fields the host requires, not more', () => {
  const base = {mode: 'add', id: 'p', protocol: 'openai_chat', baseUrl: 'https://e', apiKey: null, apiKeyEnv: '', models: [{id: 'm', context_window: null}]};
  assert.equal(providerFormError(base), null);
  assert.match(providerFormError({...base, id: ''}), /Provider ID/);
  assert.match(providerFormError({...base, id: 'P Upper'}), /小写/);
  assert.match(providerFormError({...base, protocol: 'x'}), /协议/);
  assert.match(providerFormError({...base, baseUrl: ''}), /Base URL/);
  assert.match(providerFormError({...base, baseUrl: 'ftp://e'}), /Base URL/);
  assert.match(providerFormError({...base, models: [{id: 'a/b', context_window: null}]}), /模型 ID/);
  assert.match(providerFormError({...base, models: [{id: 'm', context_window: 1}]}), /上下文窗口/);
  assert.equal(providerFormError({...base, models: [{id: 'm', context_window: 200000}]}), null);
  // An edit may leave the endpoint empty: it means "keep the stored value".
  assert.equal(providerFormError({...base, mode: 'edit', id: 'p', baseUrl: '', models: []}), null);
});

test('a session context wins over the composer selection', () => {
  assert.deepEqual(providerContext({selection: 'folder-1', sessionId: 's-1'}), {selection: null, sessionId: 's-1'});
  assert.deepEqual(providerContext({selection: 'folder-1', sessionId: null}), {selection: 'folder-1', sessionId: null});
  assert.deepEqual(providerContext(), {selection: null, sessionId: null});
  assert.equal(modelRefText({provider: 'p', model: 'm'}), 'p/m');
});

test('runtime consistency is stated only from structured facts', () => {
  assert.equal(runtimeConfigState(null).text, 'Runtime 未连接；配置会在下次连接时加载');
  assert.equal(runtimeConfigState({connected: true, config_current: true}).tone, 'done');
  const stale = runtimeConfigState({connected: true, config_current: false});
  assert.equal(stale.canReload, true);
  assert.match(stale.text, /尚未应用/);
  const unknown = runtimeConfigState({connected: true, config_current: null});
  assert.equal(unknown.tone, 'muted');
  assert.equal(unknown.canReload, true);
});
