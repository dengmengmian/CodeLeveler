import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  skillStatusView, skillStatusDetail, skillScopeLabel, skillSourceLabel,
  skillSlashName, skillSlashState, skillCommandEntries, skillDeliveryNotice,
  skillListAnswer, skillDetailAnswer, skillSections, skillBundledFiles, skillFlags,
} from '../src/skills-settings.mjs';

const skill = (over = {}) => ({
  name: 'deploy',
  scope: 'project',
  source: 'native',
  status: 'available',
  user_invocable: true,
  disable_model_invocation: false,
  slash_reserved: false,
  ...over,
});

test('every registry state has its own label and guidance', () => {
  assert.equal(skillStatusView({ status: 'available' }).label, '可用');
  assert.equal(skillStatusView({ status: 'untrusted' }).label, '未信任');
  assert.equal(skillStatusView({ status: 'untrusted' }).tone, 'warn');
  assert.equal(skillStatusView({ status: 'disabled' }).label, '已禁用');
  assert.equal(skillStatusView({ status: 'invalid' }).tone, 'danger');
  assert.match(skillStatusDetail({ status: 'untrusted' }), /不会进入模型上下文/);
  assert.match(skillStatusDetail({ status: 'invalid', reason: 'needs a description' }), /needs a description/);
  assert.equal(skillScopeLabel('project'), '项目');
  assert.equal(skillSourceLabel('agent_skills'), 'Agent Skills');
  assert.equal(skillSourceLabel('mystery'), 'mystery');
});

test('an untrusted Skill is sendable and will ask; a final refusal is not sent', () => {
  assert.deepEqual(skillSlashState(skill({ status: 'untrusted' })), { enabled: true, reason: null });
  assert.equal(skillSlashState(skill()).enabled, true);
  const disabled = skillSlashState(skill({ status: 'disabled', reason: 'disabled in preferences' }));
  assert.equal(disabled.enabled, false);
  assert.match(disabled.reason, /已禁用/);
  const modelOnly = skillSlashState(skill({ user_invocable: false }));
  assert.equal(modelOnly.enabled, false);
  assert.match(modelOnly.reason, /user-invocable/);
  assert.equal(skillSlashState(skill({ status: 'invalid', reason: 'bad frontmatter' })).enabled, false);
});

test('a reserved or client-owned bare name is offered under its qualified form', () => {
  assert.equal(skillSlashName(skill()), 'deploy');
  assert.equal(skillSlashName(skill({ slash_reserved: true })), 'project:deploy');
  assert.equal(skillSlashName(skill(), new Set(['deploy'])), 'project:deploy');
  const entries = skillCommandEntries([
    skill({ name: 'deploy' }),
    skill({ name: 'help', slash_reserved: true }),
    skill({ name: 'feedback' }),
    skill({ name: 'off', status: 'disabled', reason: 'off by user' }),
    skill({ name: 'bad', status: 'invalid', reason: 'broken' }),
  ], new Set(['feedback']));
  assert.deepEqual(entries.map(entry => entry.id), ['bad', 'deploy', 'off', 'project:feedback', 'project:help']);
  const byId = new Map(entries.map(entry => [entry.id, entry]));
  assert.equal(byId.get('deploy').kind, 'skill');
  assert.equal(byId.get('deploy').enabled, true);
  assert.equal(byId.get('off').enabled, false);
  assert.match(byId.get('off').reason, /off by user/);
  assert.equal(byId.get('project:help').enabled, true);
});

test('a delivery notice names exactly what the turn did', () => {
  assert.equal(skillDeliveryNotice({ entries: [] }), null);
  assert.equal(
    skillDeliveryNotice({ entries: [{ name: 'deploy', requested_as: '/deploy' }] }),
    '已加载技能 /deploy',
  );
  assert.equal(
    skillDeliveryNotice({ entries: [{ name: 'off', requested_as: '/off', failure: 'disabled' }] }),
    '技能 /off 未加载：技能已禁用',
  );
  assert.equal(
    skillDeliveryNotice({ entries: [], blocked: 'capability_unavailable' }),
    '本回合的技能调用被拒绝：当前回合没有技能能力',
  );
  assert.match(
    skillDeliveryNotice({
      entries: [
        { name: 'a', requested_as: '/a' },
        { name: 'b', requested_as: '/b', failure: 'untrusted' },
      ],
    }),
    /已加载技能 \/a；技能 \/b 未加载：技能未信任/,
  );
});

test('only this page’s own answer on the selected session is accepted', () => {
  const frame = (over = {}) => ({
    event: 'runtime',
    session_id: 's1',
    data: { type: 'skills_loaded', query_id: 'q2', skills: [skill()], problems: [], ...over },
  });
  assert.deepEqual(skillListAnswer(frame(), 's1')?.skills, [skill()]);
  assert.equal(skillListAnswer(frame(), 's2'), null);
  assert.equal(skillListAnswer(frame({ query_id: '' }), 's1'), null);
  assert.equal(skillListAnswer({ event: 'snapshot', data: {} }, 's1'), null);
  const detail = skillDetailAnswer({
    event: 'runtime',
    session_id: 's1',
    data: { type: 'skill_loaded', query_id: 'q3', name: 'deploy', skill: { entry: skill() }, error: null },
  }, 's1');
  assert.equal(detail.name, 'deploy');
  assert.equal(skillDetailAnswer({ event: 'runtime', session_id: 's2', data: { type: 'skill_loaded', query_id: 'q3' } }, 's1'), null);
});

test('sections and inspector facts preserve registry facts', () => {
  const sections = skillSections([
    skill({ name: 'b', status: 'untrusted' }),
    skill({ name: 'a', status: 'available' }),
    skill({ name: 'c', status: 'invalid' }),
  ]);
  assert.deepEqual(sections.map(section => section.status), ['available', 'untrusted', 'invalid']);
  assert.deepEqual(sections.map(section => section.rows.map(row => row.name)), [['a'], ['b'], ['c']]);
  assert.deepEqual(
    skillBundledFiles({ scripts: ['scripts/run.sh'], references: ['references/doc.md'], other_files: [] }),
    [
      { label: '脚本', rows: ['scripts/run.sh'] },
      { label: '参考', rows: ['references/doc.md'] },
    ],
  );
  assert.deepEqual(skillFlags(skill({ user_invocable: false, slash_reserved: true, argument_hint: '<env>' })), [
    '仅模型调用', '使用限定斜杠名', '参数：<env>',
  ]);
});
