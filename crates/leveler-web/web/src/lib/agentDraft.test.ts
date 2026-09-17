// Agents 管理页的纯逻辑：表单 → UiAgentDraft、权限摘要、分组、子 agent 显示名。
// runtime 才是校验权威；这里只保证 UI 发出的形状忠实于用户填的内容。

import { describe, expect, it } from 'vitest';
import type { UiAgentDetail, UiAgentEntry } from '../types/protocol';
import {
  availabilityLabel,
  buildCreateDraft,
  buildEditDraft,
  childDisplayName,
  definitionProblem,
  deleteConfirmText,
  draftSummary,
  editFormFromDetail,
  groupAgents,
  newCreateForm,
  parseList,
  permissionSummary,
  shortFingerprint,
  switchTemplate,
  TEMPLATES,
} from './agentDraft';

function entry(over: Partial<UiAgentEntry> = {}): UiAgentEntry {
  return { name: 'a', source: 'project', status: 'available', ...over };
}

describe('create form → draft', () => {
  it('a new form starts from the read-only template in project scope with Auto model', () => {
    const form = newCreateForm();
    expect(form.template).toBe('read_only');
    expect(form.capability).toBe('read_only');
    expect(form.scope).toBe('project');
    expect(form.model).toBe('');
    expect(form.instructions).toBe(TEMPLATES.read_only.instructions);
  });

  it('Auto model is omitted, empty tools become undefined, name/description trimmed', () => {
    const draft = buildCreateDraft({
      ...newCreateForm(),
      name: '  security-reviewer ',
      description: ' 查安全问题 ',
      template: 'blank',
      capability: 'writer',
      tools: [],
      instructions: '保留\n  原样  ',
    });
    expect(draft).toEqual({
      name: 'security-reviewer',
      description: '查安全问题',
      capability: 'writer',
      skills: [],
      write_roots: [],
      instructions: '保留\n  原样  ',
    });
    expect('model' in draft).toBe(false);
    expect('tools' in draft).toBe(false);
  });

  it('a chosen model is sent as provider/model', () => {
    const draft = buildCreateDraft({ ...newCreateForm(), name: 'x', model: 'moonshot/k2' });
    expect(draft.model).toBe('moonshot/k2');
  });

  it('templates set capability, tools and instructions', () => {
    const reviewer = switchTemplate(newCreateForm(), 'reviewer');
    expect(reviewer.capability).toBe('read_only');
    expect(reviewer.tools).toEqual(['read_file', 'grep', 'git_diff', 'find_files', 'list_files']);
    expect(buildCreateDraft({ ...reviewer, name: 'r' }).tools).toEqual([
      'read_file',
      'grep',
      'git_diff',
      'find_files',
      'list_files',
    ]);
    const worker = switchTemplate(newCreateForm(), 'worker');
    expect(worker.capability).toBe('scoped_writer');
    expect(worker.tools).toEqual([]);
    expect(worker.instructions).toBe(TEMPLATES.worker.instructions);
    const blank = switchTemplate(newCreateForm(), 'blank');
    expect(blank.capability).toBe('writer');
    expect(blank.instructions).toBe('');
  });

  it('switching template keeps instructions the user already edited', () => {
    const edited = { ...newCreateForm(), instructions: '我自己写的' };
    expect(switchTemplate(edited, 'worker').instructions).toBe('我自己写的');
  });

  it('the confirmation summary names scope, authority, write scope, tools, model and skills', () => {
    const draft = buildCreateDraft({ ...switchTemplate(newCreateForm(), 'reviewer'), name: 'r' });
    expect(draftSummary(draft, 'user')).toEqual([
      ['名称', 'r'],
      ['位置', '用户（~/.leveler/agents）'],
      ['读写权限', '只读 · 不改文件'],
      ['可写范围', '不写文件'],
      ['工具', 'read_file, grep, git_diff, find_files, list_files'],
      ['模型', 'inherit'],
      ['Skills', '无'],
    ]);
    const writer = buildCreateDraft({ ...newCreateForm(), name: 'w', capability: 'writer', tools: [], model: 'openai/gpt-5' });
    const rows = new Map(draftSummary({ ...writer, write_roots: ['src/'] }, 'project'));
    expect(rows.get('位置')).toBe('项目（.leveler/agents）');
    expect(rows.get('可写范围')).toBe('只在 src/ 下');
    expect(rows.get('工具')).toBe('能力默认全部工具');
    expect(rows.get('模型')).toBe('openai/gpt-5');
  });
});

describe('edit form', () => {
  const detail: UiAgentDetail = {
    entry: entry({
      name: 'sec',
      source: 'user',
      description: 'd',
      capability: 'scoped_writer',
      model: 'moonshot/k2',
      reasoning_effort: 'high',
      skills: ['audit'],
      tools: ['read_file', 'edit_file'],
      write_roots: ['src/', 'tests/'],
      max_rounds: 20,
      max_duration_secs: null,
    }),
    instructions: '  第一行\n\n第二行  ',
  };

  it('round-trips a definition verbatim', () => {
    const form = editFormFromDetail(detail);
    expect(form.toolsText).toBe('read_file, edit_file');
    expect(form.writeRootsText).toBe('src/, tests/');
    const out = buildEditDraft(form);
    expect(out).toEqual({
      draft: {
        name: 'sec',
        description: 'd',
        capability: 'scoped_writer',
        model: 'moonshot/k2',
        reasoning_effort: 'high',
        skills: ['audit'],
        tools: ['read_file', 'edit_file'],
        write_roots: ['src/', 'tests/'],
        max_rounds: 20,
        instructions: '  第一行\n\n第二行  ',
      },
    });
  });

  it('empty tools / model / effort / limits are omitted', () => {
    const form = { ...editFormFromDetail(detail), toolsText: ' ', model: '', reasoningEffort: '', maxRounds: '' };
    const out = buildEditDraft(form);
    if (!('draft' in out)) throw new Error('expected a draft');
    expect('tools' in out.draft).toBe(false);
    expect('model' in out.draft).toBe(false);
    expect('reasoning_effort' in out.draft).toBe(false);
    expect('max_rounds' in out.draft).toBe(false);
  });

  it('a limit that is not a whole number is reported, not silently dropped', () => {
    const out = buildEditDraft({ ...editFormFromDetail(detail), maxRounds: '2.5' });
    expect(out).toEqual({ error: 'max_rounds 需要是正整数' });
  });

  it('duplicating a built-in persona takes its definition under a new name', () => {
    const persona: UiAgentDetail = {
      entry: entry({ name: 'code-reviewer', source: 'builtin', capability: 'read_only', description: '审查' }),
      instructions: '你是代码评审者。',
    };
    const form = editFormFromDetail(persona);
    expect(form.name).toBe('code-reviewer');
    expect(form.instructions).toBe('你是代码评审者。');
  });

  it('parseList splits on commas and newlines', () => {
    expect(parseList(' a, b\nc,, ')).toEqual(['a', 'b', 'c']);
  });
});

describe('permission summary', () => {
  it('says in plain words whether the agent can change files', () => {
    expect(permissionSummary({ capability: 'read_only' })).toBe('只读 · 不改文件');
    expect(permissionSummary({ capability: 'writer' })).toBe('可写 · 自己认领要改的文件');
    expect(permissionSummary({ capability: 'scoped_writer' })).toBe('限定文件可写 · 只改派发时交给它的文件');
  });

  it('adds write roots when present', () => {
    expect(permissionSummary({ capability: 'writer', write_roots: ['src/', 'docs/'] })).toBe(
      '可写 · 自己认领要改的文件 · 仅限 src/, docs/',
    );
  });

  it('an invalid definition has no capability to summarise', () => {
    expect(permissionSummary({ capability: null })).toBe('—');
  });
});

describe('registry listing', () => {
  it('groups by source in precedence order: Project / User / Built-in', () => {
    const groups = groupAgents([
      entry({ name: 'p', source: 'project' }),
      entry({ name: 'b', source: 'builtin' }),
      entry({ name: 'u', source: 'user' }),
    ]);
    expect(groups.map((g) => [g.source, g.entries.map((e) => e.name)])).toEqual([
      ['project', ['p']],
      ['user', ['u']],
      ['builtin', ['b']],
    ]);
  });

  it('availability carries the runtime reason and claims no more than configuration', () => {
    // runtime 只检查模型/skills 是否已配置，不检查 API key：不能说「可用」。
    expect(availabilityLabel(entry())).toBe('已配置');
    expect(availabilityLabel(entry({ status: 'unavailable', reason: 'model x: no key' }))).toBe(
      '不可用：model x: no key',
    );
    expect(availabilityLabel(entry({ status: 'invalid', reason: 'bad yaml' }))).toBe('无效：bad yaml');
  });

  it('an invalid definition is reported, not edited as if it were blank', () => {
    expect(definitionProblem({ entry: entry({ status: 'invalid', reason: 'unknown tool `x`' }) })).toBe(
      'unknown tool `x`',
    );
    expect(definitionProblem({ entry: entry({ status: 'invalid' }) })).toBe('定义无效（runtime 未给出原因）');
    expect(definitionProblem({ entry: entry({ status: 'unavailable', reason: 'no model' }), instructions: 'x' })).toBeNull();
    expect(definitionProblem({ entry: entry(), instructions: 'x' })).toBeNull();
  });

  it('the delete confirmation names the agent, its scope and the files it removes', () => {
    expect(deleteConfirmText('sec', 'project', '/repo/.leveler/agents/sec')).toBe(
      '删除项目 Agent「sec」？\n将从项目中移除 /repo/.leveler/agents/sec（agent.yaml 与 instructions.md）。\n正在运行的子 Agent 不受影响。',
    );
    expect(deleteConfirmText('sec', 'user', null)).toBe(
      '删除用户 Agent「sec」？\n将从本机 ~/.leveler/agents 移除，所有项目都不再可用。\n正在运行的子 Agent 不受影响。',
    );
  });

  it('short fingerprint is the first 8 characters', () => {
    expect(shortFingerprint('0123456789abcdef')).toBe('01234567');
  });

  it('a child spawned from an agent is named nickname · agent', () => {
    expect(childDisplayName('Curie', 'security-reviewer')).toBe('Curie · security-reviewer');
    expect(childDisplayName('Curie', null)).toBe('Curie');
  });
});
