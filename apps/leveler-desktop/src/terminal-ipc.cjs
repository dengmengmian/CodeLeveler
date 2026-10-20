// Fixed RPC adapter only. Runtime/ExecutionHost owns PTYs and validates grants.
function installTerminalIpc({ handle, bridge }) {
  const terminals = new Map();
  const methods = { create: 'create_terminal', write: 'write_terminal', resize: 'resize_terminal', close: 'close_terminal', observe: 'observe_terminal' };
  for (const [action, method] of Object.entries(methods)) {
    handle(`desktop:terminal-${action}`, async request => {
      if (!request || typeof request !== 'object' || Array.isArray(request)) throw new Error('Invalid terminal request');
      const fields = ['session_id', 'terminal_id', ...({ create: ['cols', 'rows'], write: ['data_base64'], resize: ['cols', 'rows'], observe: ['cursor', 'max_bytes'] }[action] ?? [])];
      if (Object.keys(request).some(key => !fields.includes(key))) throw new Error('Invalid terminal request fields');
      if (typeof request.session_id !== 'string' || !request.session_id || request.session_id.length > 128 || typeof request.terminal_id !== 'string' || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(request.terminal_id)) throw new Error('Invalid terminal identity');
      if (action === 'write' && (typeof request.data_base64 !== 'string' || request.data_base64.length > 32768 || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(request.data_base64))) throw new Error('Invalid terminal input');
      if (['create', 'resize'].includes(action) && !['cols', 'rows'].every(field => Number.isInteger(request[field]) && request[field] >= 1 && request[field] <= 1000)) throw new Error('Invalid terminal dimensions');
      if (action === 'observe' && (!Number.isSafeInteger(request.cursor) || request.cursor < 0 || !Number.isInteger(request.max_bytes) || request.max_bytes < 1 || request.max_bytes > 65536)) throw new Error('Invalid terminal cursor');
      const key = `${request.session_id}:${request.terminal_id}`;
      // Register before writing: delivery failure can mean create happened.
      if (action === 'create') terminals.set(key, { session_id: request.session_id, terminal_id: request.terminal_id });
      const result = await bridge.request(method, request);
      if (action === 'close' && ['exited', 'killed'].includes(result?.status)) terminals.delete(key);
      return result;
    });
  }
  return async () => {
    const results = await Promise.allSettled([...terminals.values()].map(params => bridge.request('close_terminal', params)));
    for (const result of results) {
      if (result.status === 'rejected' || !['exited', 'killed'].includes(result.value?.status)) console.warn('Terminal shutdown not confirmed:', result.status === 'rejected' ? result.reason.message : result.value?.status);
    }
  };
}
module.exports = { installTerminalIpc };
