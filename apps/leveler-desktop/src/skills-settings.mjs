// Presentation rules for the Skills surface.
//
// The Runtime's registry is the only authority for what exists, where it came
// from, and whether it is usable. This module never scans a directory, never
// decides a trust state, and never turns a display string back into a decision.
// It maps the typed `UiSkillEntry` / `UiSkillDetail` facts to the labels and
// slash-command entries the renderer paints.

const STATUS_VIEW = Object.freeze({
  available: { label: '可用', tone: 'ok' },
  untrusted: { label: '未信任', tone: 'warn' },
  disabled: { label: '已禁用', tone: 'muted' },
  invalid: { label: '定义无效', tone: 'danger' },
});

const SCOPE_LABEL = Object.freeze({
  project: '项目',
  user: '用户',
  builtin: '内置',
});

const SOURCE_LABEL = Object.freeze({
  native: 'CodeLeveler',
  codex: 'Codex 兼容',
  agent_skills: 'Agent Skills',
  claude: 'Claude 兼容',
  builtin: '内置',
});

const FAILURE_LABEL = Object.freeze({
  not_found: '没有找到这个技能',
  invalid: '技能定义无效',
  disabled: '技能已禁用',
  untrusted: '技能未信任，未加载',
  user_invocation_disabled: '此技能不允许用户调用',
  model_invocation_disabled: '此技能不允许模型调用',
  read: '技能内容读取失败',
  shadowed: '同名技能被高优先级定义覆盖',
  capability_unavailable: '当前回合没有技能能力',
});

export function skillStatusView(skill) {
  const entry = STATUS_VIEW[skill?.status] ?? { label: String(skill?.status ?? '未知'), tone: 'muted' };
  return { ...entry, status: skill?.status ?? null };
}

export function skillStatusDetail(skill) {
  switch (skill?.status) {
    case 'available':
      return '定义有效且已信任；调用时 Runtime 加载其正文。';
    case 'untrusted':
      return '技能存在，但当前内容尚未被信任。只有 Agent 实际使用它时才会请求授权；批准前正文不会进入模型上下文。';
    case 'disabled':
      return skill?.reason ? `此技能在技能偏好中被停用：${skill.reason}` : '此技能已被用户停用。';
    case 'invalid':
      return skill?.reason ? `定义无法加载：${skill.reason}` : '定义无法加载。';
    default:
      return '';
  }
}

export function skillScopeLabel(scope) {
  return SCOPE_LABEL[scope] ?? String(scope ?? '未知');
}

export function skillSourceLabel(source) {
  return SOURCE_LABEL[source] ?? String(source ?? '未知');
}

/** The exact token the user can type, honoring the product's reserved-name
 *  rule: a colliding bare name stays a product command and the Skill uses its
 *  qualified form. `reservedIds` is the client's own command surface; the
 *  runtime's `slash_reserved` covers names the product owns on every client. */
export function skillSlashName(skill, reservedIds = null) {
  if (!skill?.name) return '';
  if (skill.slash_reserved || reservedIds?.has?.(skill.name)) {
    return `${skill.scope}:${skill.name}`;
  }
  return skill.name;
}

/** Whether the composer may hand this Skill request to the Runtime.
 *
 * An untrusted package is sendable: the runtime raises the typed trust request
 * before staging the turn, so the user can decide. A disabled, invalid or
 * user-invocation-disabled package is refused here with the same fact the
 * runtime's delivery audit would report; sending it would only waste a turn.
 */
export function skillSlashState(skill) {
  if (!skill?.name) return { enabled: false, reason: '技能名称缺失' };
  if (skill.status === 'untrusted') return { enabled: true, reason: null };
  if (skill.status === 'available') {
    if (skill.user_invocable === false) {
      return { enabled: false, reason: '此技能设置了 user-invocable: false，只能由模型使用' };
    }
    return { enabled: true, reason: null };
  }
  if (skill.status === 'disabled') {
    return { enabled: false, reason: skill.reason ? `技能已禁用：${skill.reason}` : '技能已禁用' };
  }
  if (skill.status === 'invalid') {
    return { enabled: false, reason: skill.reason ? `技能定义无效：${skill.reason}` : '技能定义无效' };
  }
  return { enabled: false, reason: '技能状态未知，无法调用' };
}

/** Slash-menu entries for the Runtime's rows. Product command ids stay owned
 *  by the product: a Skill whose bare name collides is offered under its
 *  qualified form. */
export function skillCommandEntries(skills = [], reservedIds = null) {
  const entries = [];
  const seen = new Set();
  for (const skill of skills) {
    if (!skill?.name) continue;
    const id = skillSlashName(skill, reservedIds);
    if (!id || seen.has(id)) continue;
    seen.add(id);
    const state = skillSlashState(skill);
    entries.push({
      group: 'skills',
      id,
      label: id,
      icon: 'sparkle',
      kind: 'skill',
      enabled: state.enabled,
      reason: state.reason,
      description: skill.description ?? '',
    });
  }
  return entries.sort((left, right) => left.id.localeCompare(right.id));
}

export function skillFailureLabel(code) {
  return FAILURE_LABEL[code] ?? null;
}

/** The one-line runtime note for a turn's skill delivery.
 *
 * The event is emitted only when something needs saying: an explicit slash
 * invocation loaded, a selection was refused, or the host blocked delivery.
 * `null` means there is nothing to add to the transcript. */
export function skillDeliveryNotice(delivery) {
  const entries = Array.isArray(delivery?.entries) ? delivery.entries : [];
  const blocked = delivery?.blocked ?? null;
  const lines = [];
  for (const entry of entries) {
    if (!entry?.name) continue;
    const token = entry.requested_as || `/${entry.name}`;
    if (!entry.failure) {
      lines.push(`已加载技能 ${token}`);
      continue;
    }
    const reason = skillFailureLabel(entry.failure) ?? String(entry.failure);
    lines.push(`技能 ${token} 未加载：${reason}`);
  }
  if (blocked) {
    const reason = skillFailureLabel(blocked) ?? String(blocked);
    lines.unshift(`本回合的技能调用被拒绝：${reason}`);
  }
  return lines.length ? lines.join('；') : null;
}

/** The answer this page accepts: only its own query on the selected session. */
export function skillListAnswer(frame, sessionId) {
  if (frame?.event !== 'runtime' || frame?.data?.type !== 'skills_loaded') return null;
  if (!sessionId || frame.session_id !== sessionId) return null;
  const queryId = typeof frame.data.query_id === 'string' ? frame.data.query_id : null;
  if (!queryId) return null;
  return {
    queryId,
    skills: Array.isArray(frame.data.skills) ? frame.data.skills : [],
    problems: Array.isArray(frame.data.problems) ? frame.data.problems : [],
  };
}

export function skillDetailAnswer(frame, sessionId) {
  if (frame?.event !== 'runtime' || frame?.data?.type !== 'skill_loaded') return null;
  if (!sessionId || frame.session_id !== sessionId) return null;
  const queryId = typeof frame.data.query_id === 'string' ? frame.data.query_id : null;
  if (!queryId) return null;
  return {
    queryId,
    name: String(frame.data.name ?? ''),
    skill: frame.data.skill ?? null,
    error: frame.data.error ?? null,
  };
}

/** Rows grouped by what the user can do with them, preserving registry order
 *  inside each group. */
export function skillSections(skills = []) {
  const order = ['available', 'untrusted', 'disabled', 'invalid'];
  return order
    .map(status => ({
      status,
      label: STATUS_VIEW[status].label,
      tone: STATUS_VIEW[status].tone,
      rows: skills.filter(skill => skill?.status === status),
    }))
    .filter(section => section.rows.length > 0);
}

export function skillHasBody(detail) {
  return typeof detail?.body === 'string' && detail.body.length > 0;
}

/** Bundled files grouped for the inspector. The counts are the runtime's
 *  lists, not a directory walk performed by the client. */
export function skillBundledFiles(detail) {
  return [
    { label: '脚本', rows: detail?.scripts ?? [] },
    { label: '参考', rows: detail?.references ?? [] },
    { label: '其他', rows: detail?.other_files ?? [] },
  ].filter(group => group.rows.length > 0);
}

export function skillFlags(skill) {
  const flags = [];
  if (skill?.user_invocable === false) flags.push('仅模型调用');
  if (skill?.disable_model_invocation) flags.push('仅用户调用');
  if (skill?.slash_reserved) flags.push('使用限定斜杠名');
  if (skill?.argument_hint) flags.push(`参数：${skill.argument_hint}`);
  return flags;
}
