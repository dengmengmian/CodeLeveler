// Agents 管理页的纯逻辑：表单 → UiAgentDraft、权限摘要、分组、显示名。
// runtime 才是校验权威（未知工具、保留名、read_only 带写权限……），这里
// 不重复它的规则，只把用户填的内容忠实地变成 wire 形状。

import type {
  UiAgentCapability,
  UiAgentDetail,
  UiAgentDraft,
  UiAgentEntry,
  UiAgentScope,
  UiAgentSource,
} from '../types/protocol';

export type AgentTemplate = 'read_only' | 'worker' | 'reviewer' | 'blank';

interface TemplateDefaults {
  label: string;
  capability: UiAgentCapability;
  tools: string[];
  instructions: string;
}

/** 模板只给角色本身的起点文字；运行时规则由 runtime 注入，不写进来。 */
export const TEMPLATES: Record<AgentTemplate, TemplateDefaults> = {
  read_only: {
    label: '只读调查（Explorer 类）',
    capability: 'read_only',
    tools: [],
    instructions:
      '你负责调查代码库：沿真实执行路径找到入口、流程和关键文件，给出结论和依据（文件:行）。',
  },
  worker: {
    label: '实现（Worker）',
    capability: 'scoped_writer',
    tools: [],
    instructions:
      '你负责在交给你的文件里完成实现：先读懂现有代码，做最小必要改动，最后说明改了什么、怎么验证。',
  },
  reviewer: {
    label: '评审（Reviewer 类）',
    capability: 'read_only',
    tools: ['read_file', 'grep', 'git_diff', 'find_files', 'list_files'],
    instructions:
      '你负责评审改动：按正确性、安全、性能、可读性的顺序找真实问题，每条指到文件:行，区分必改 / 建议 / 疑问。',
  },
  blank: { label: '空白', capability: 'writer', tools: [], instructions: '' },
};

export const CAPABILITY_LABEL: Record<UiAgentCapability, string> = {
  read_only: '只读',
  writer: '可写',
  scoped_writer: '限定文件可写',
};

export const REASONING_EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max'] as const;

export interface CreateAgentForm {
  name: string;
  description: string;
  template: AgentTemplate;
  /** `provider/model`；空串 = Auto（不写，跟随父会话模型）。 */
  model: string;
  capability: UiAgentCapability;
  tools: string[];
  scope: UiAgentScope;
  instructions: string;
}

export function newCreateForm(): CreateAgentForm {
  const t = TEMPLATES.read_only;
  return {
    name: '',
    description: '',
    template: 'read_only',
    model: '',
    capability: t.capability,
    tools: [...t.tools],
    scope: 'project',
    instructions: t.instructions,
  };
}

/** 换模板：权限和工具跟着换；说明文字只在用户没改过时替换。 */
export function switchTemplate(form: CreateAgentForm, template: AgentTemplate): CreateAgentForm {
  const next = TEMPLATES[template];
  const untouched =
    form.instructions.trim() === '' || form.instructions === TEMPLATES[form.template].instructions;
  return {
    ...form,
    template,
    capability: next.capability,
    tools: [...next.tools],
    instructions: untouched ? next.instructions : form.instructions,
  };
}

export function buildCreateDraft(form: CreateAgentForm): UiAgentDraft {
  const draft: UiAgentDraft = {
    name: form.name.trim(),
    description: form.description.trim(),
    capability: form.capability,
    skills: [],
    write_roots: [],
    instructions: form.instructions,
  };
  if (form.model.trim()) draft.model = form.model.trim();
  if (form.tools.length > 0) draft.tools = [...form.tools];
  return draft;
}

/** 编辑器表单：列表字段用逗号文本，数字字段用输入框原文。 */
export interface EditAgentForm {
  name: string;
  description: string;
  capability: UiAgentCapability;
  model: string;
  reasoningEffort: string;
  toolsText: string;
  writeRootsText: string;
  skillsText: string;
  maxRounds: string;
  maxDurationSecs: string;
  instructions: string;
}

export function parseList(text: string): string[] {
  return text
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

export function editFormFromDetail(detail: UiAgentDetail): EditAgentForm {
  const e = detail.entry;
  return {
    name: e.name,
    description: e.description ?? '',
    capability: e.capability ?? 'read_only',
    model: e.model ?? '',
    reasoningEffort: e.reasoning_effort ?? '',
    toolsText: (e.tools ?? []).join(', '),
    writeRootsText: (e.write_roots ?? []).join(', '),
    skillsText: (e.skills ?? []).join(', '),
    maxRounds: e.max_rounds != null ? String(e.max_rounds) : '',
    maxDurationSecs: e.max_duration_secs != null ? String(e.max_duration_secs) : '',
    instructions: detail.instructions ?? '',
  };
}

function parseLimit(raw: string, field: string): number | undefined | { error: string } {
  const text = raw.trim();
  if (!text) return undefined;
  const n = Number(text);
  if (!Number.isInteger(n) || n <= 0) return { error: `${field} 需要是正整数` };
  return n;
}

/** 数字字段不是正整数时直接报出来，而不是悄悄丢掉 —— wire 上它只能是整数。 */
export function buildEditDraft(form: EditAgentForm): { draft: UiAgentDraft } | { error: string } {
  const maxRounds = parseLimit(form.maxRounds, 'max_rounds');
  if (typeof maxRounds === 'object') return maxRounds;
  const maxDuration = parseLimit(form.maxDurationSecs, 'max_duration_secs');
  if (typeof maxDuration === 'object') return maxDuration;
  const draft: UiAgentDraft = {
    name: form.name.trim(),
    description: form.description.trim(),
    capability: form.capability,
    skills: parseList(form.skillsText),
    write_roots: parseList(form.writeRootsText),
    instructions: form.instructions,
  };
  if (form.model.trim()) draft.model = form.model.trim();
  if (form.reasoningEffort.trim()) draft.reasoning_effort = form.reasoningEffort.trim();
  const tools = parseList(form.toolsText);
  if (tools.length > 0) draft.tools = tools;
  if (maxRounds !== undefined) draft.max_rounds = maxRounds;
  if (maxDuration !== undefined) draft.max_duration_secs = maxDuration;
  return { draft };
}

// ── 展示 ────────────────────────────────────────────────────────────

const CAPABILITY_SUMMARY: Record<UiAgentCapability, string> = {
  read_only: 'read-only',
  writer: 'writes what it claims',
  scoped_writer: 'writes given files',
};

export function permissionSummary(e: {
  capability?: UiAgentCapability | null;
  write_roots?: string[];
}): string {
  if (!e.capability) return '—';
  const base = CAPABILITY_SUMMARY[e.capability];
  const roots = e.write_roots ?? [];
  return roots.length > 0 ? `${base} · only under ${roots.join(', ')}` : base;
}

const SCOPE_LABEL: Record<UiAgentScope, string> = {
  project: '项目（.leveler/agents）',
  user: '用户（~/.leveler/agents）',
};

/** 发送前给用户确认的摘要（名称、位置、读写权限、可写范围、工具、模型、skills）。 */
export function draftSummary(draft: UiAgentDraft, scope: UiAgentScope): Array<[string, string]> {
  const roots = draft.write_roots ?? [];
  const writeScope =
    draft.capability === 'read_only'
      ? '不写文件'
      : roots.length > 0
        ? `只在 ${roots.join(', ')} 下`
        : draft.capability === 'scoped_writer'
          ? '派发时交给它的文件'
          : '它声明要写的文件';
  const skills = draft.skills ?? [];
  return [
    ['名称', draft.name],
    ['位置', SCOPE_LABEL[scope]],
    ['读写权限', CAPABILITY_SUMMARY[draft.capability]],
    ['可写范围', writeScope],
    ['工具', draft.tools && draft.tools.length > 0 ? draft.tools.join(', ') : '能力默认全部工具'],
    ['模型', draft.model || 'inherit'],
    ['Skills', skills.length > 0 ? skills.join(', ') : '无'],
  ];
}

export const SOURCE_LABEL: Record<UiAgentSource, string> = {
  builtin: 'Built-in',
  project: 'Project',
  user: 'User',
};

export function groupAgents(
  entries: readonly UiAgentEntry[],
): Array<{ source: UiAgentSource; entries: UiAgentEntry[] }> {
  return (['builtin', 'project', 'user'] as const).map((source) => ({
    source,
    entries: entries.filter((e) => e.source === source),
  }));
}

export function availabilityLabel(e: UiAgentEntry): string {
  if (e.status === 'available') return 'available';
  return e.reason ? `${e.status}: ${e.reason}` : e.status;
}

export function shortFingerprint(fingerprint: string): string {
  return fingerprint.slice(0, 8);
}

export function childDisplayName(nickname: string, agentName: string | null | undefined): string {
  return agentName ? `${nickname} · ${agentName}` : nickname;
}
