// 设置 → Agents：列出会话所在项目解析到的 agent 定义，新建 / 编辑 / 删除。
// 写操作的授权就是用户这次点击；校验与原子写入在 runtime，失败原因原样展示。

import { Fragment, useEffect, useState, type ReactNode } from 'react';
import {
  availabilityLabel,
  buildCreateDraft,
  buildEditDraft,
  CAPABILITY_LABEL,
  definitionProblem,
  deleteConfirmText,
  draftSummary,
  editFormFromDetail,
  groupAgents,
  newCreateForm,
  permissionSummary,
  REASONING_EFFORTS,
  SOURCE_HINT,
  SOURCE_LABEL,
  switchTemplate,
  TEMPLATES,
  type AgentTemplate,
  type CreateAgentForm,
  type EditAgentForm,
} from '../lib/agentDraft';
import { modelRefString } from '../lib/format';
import { useBridge } from '../state/bridge';
import { useAppState } from '../state/store';
import type { UiAgentCapability, UiAgentDraft, UiAgentEntry, UiAgentScope } from '../types/protocol';

type Modal =
  | { kind: 'create' }
  | { kind: 'edit'; name: string; scope: UiAgentScope }
  | { kind: 'duplicate'; name: string };

export function AgentsPanel() {
  const state = useAppState();
  const bridge = useBridge();
  const sessionId = state.current?.id ?? null;
  const agents = state.agents;
  const [modal, setModal] = useState<Modal | null>(null);

  useEffect(() => {
    if (sessionId) bridge.listAgents();
  }, [sessionId, bridge]);

  return (
    <div className="set-body agents-panel">
      <div className="set-sec">Agents</div>
      <div className="set-hint">子 Agent 的定义：项目 .leveler/agents、用户 ~/.leveler/agents 和内置。</div>
      {!sessionId ? (
        <div className="insp-empty">先进入一个会话：Agents 按会话所在项目读取。</div>
      ) : (
        <>
          <div className="ag-toolbar">
            <button type="button" className="mem-btn" onClick={() => setModal({ kind: 'create' })}>
              + 新建 Agent
            </button>
            <button type="button" className="mem-btn ghost" onClick={() => bridge.listAgents()}>
              刷新
            </button>
          </div>
          {!agents.loaded && <div className="insp-empty">读取中…</div>}
          {agents.loaded &&
            groupAgents(agents.entries).map((g) => (
              <div key={g.source} className="ag-group">
                <div className="ag-kicker" title={SOURCE_HINT[g.source]}>
                  {SOURCE_LABEL[g.source]} · {g.entries.length}
                </div>
                <div className="set-hint">{SOURCE_HINT[g.source]}</div>
                {g.entries.length === 0 && <div className="insp-empty">无</div>}
                {g.entries.map((e) => (
                  <AgentEntryRow
                    key={`${e.source}:${e.name}`}
                    entry={e}
                    onEdit={() =>
                      e.source !== 'builtin' && setModal({ kind: 'edit', name: e.name, scope: e.source })
                    }
                    onDuplicate={() => setModal({ kind: 'duplicate', name: e.name })}
                  />
                ))}
              </div>
            ))}
          {agents.problems.length > 0 && (
            <div className="ag-group">
              <div className="ag-kicker">无法识别</div>
              {agents.problems.map((p) => (
                <div key={p.location} className="agent-entry st-invalid">
                  <div className="ae-meta">{p.location}</div>
                  <div className="ae-error">{p.error}</div>
                </div>
              ))}
            </div>
          )}
        </>
      )}
      {modal?.kind === 'create' && <AgentCreate onClose={() => setModal(null)} />}
      {modal?.kind === 'edit' && (
        <AgentEditor mode="edit" name={modal.name} scope={modal.scope} onClose={() => setModal(null)} />
      )}
      {modal?.kind === 'duplicate' && (
        <AgentEditor mode="duplicate" name={modal.name} scope="project" onClose={() => setModal(null)} />
      )}
    </div>
  );
}

function AgentEntryRow({
  entry,
  onEdit,
  onDuplicate,
}: {
  entry: UiAgentEntry;
  onEdit: () => void;
  onDuplicate: () => void;
}) {
  const shadowed = entry.shadowed ?? [];
  const editable = entry.source !== 'builtin';
  return (
    <div className={`agent-entry st-${entry.status}`}>
      <div className="ae-head">
        <span className="ae-name">{entry.name}</span>
        <span className="ae-tag">{SOURCE_LABEL[entry.source]}</span>
        {entry.harness_only && <span className="ae-tag">harness only</span>}
        <span
          className={`ae-badge ${entry.status}`}
          title={entry.status === 'available' ? '定义有效，模型与 skills 已配置；不检查 API key' : undefined}
        >
          {availabilityLabel(entry)}
        </span>
      </div>
      {entry.description && <div className="ae-desc">{entry.description}</div>}
      <div className="ae-meta">
        model: {entry.model ?? 'inherit'} · {permissionSummary(entry)}
      </div>
      {shadowed.length > 0 && (
        <div className="ae-meta">
          覆盖了同名的 {shadowed.map((s) => SOURCE_LABEL[s.source]).join('、')} 定义
        </div>
      )}
      <div className="ae-ops">
        {editable && (
          <button type="button" className="mem-btn" onClick={onEdit}>
            编辑
          </button>
        )}
        {!editable && !entry.structural && entry.status !== 'invalid' && (
          <button type="button" className="mem-btn" onClick={onDuplicate}>
            复制为新 Agent
          </button>
        )}
        {!editable && <span className="ae-readonly">内置，只读</span>}
      </div>
    </div>
  );
}

// ── 弹层 ────────────────────────────────────────────────────────────

function AgentModal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  useEffect(() => {
    // 捕获阶段拦下 Esc：只关这一层，不连带关掉外面的设置弹层。
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape') return;
      e.stopImmediatePropagation();
      onClose();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [onClose]);
  return (
    <div className="set-backdrop agent-modal" onClick={onClose}>
      <div className="set" onClick={(e) => e.stopPropagation()}>
        <div className="set-head">
          <span className="set-title">{title}</span>
          <button type="button" className="fv-x" title="关闭（Esc）" onClick={onClose}>
            ✕
          </button>
        </div>
        <div className="set-body agent-form">{children}</div>
      </div>
    </div>
  );
}

/** 认领自己发出的写操作应答（按 query_id）；成功后提示并关闭。 */
function useMutation(onClose: () => void) {
  const bridge = useBridge();
  const last = useAppState().agents.lastMutation;
  const [sent, setSent] = useState<{ queryId: string; done: string } | null>(null);
  const result = sent && last?.queryId === sent.queryId ? last : null;
  useEffect(() => {
    if (result?.ok && sent) {
      bridge.notice(`Agent ${result.name} ${sent.done}`);
      onClose();
    }
  }, [result, sent, bridge, onClose]);
  return {
    pending: sent !== null && result === null,
    error: result && !result.ok ? (result.error ?? '操作失败（runtime 未给出原因）') : null,
    track: (queryId: string | null, done: string) => setSent(queryId ? { queryId, done } : null),
  };
}

function ModelSelect({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const models = (useAppState().current?.availableModels ?? []).map(modelRefString);
  const options = value && !models.includes(value) ? [value, ...models] : models;
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)}>
      <option value="">Auto（跟随会话模型）</option>
      {options.map((m) => (
        <option key={m} value={m}>
          {m}
        </option>
      ))}
    </select>
  );
}

function CapabilitySelect({ value, onChange }: { value: UiAgentCapability; onChange: (v: UiAgentCapability) => void }) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value as UiAgentCapability)}>
      {(Object.keys(CAPABILITY_LABEL) as UiAgentCapability[]).map((c) => (
        <option key={c} value={c}>
          {permissionSummary({ capability: c })}
        </option>
      ))}
    </select>
  );
}

function ScopeSelect({ value, onChange }: { value: UiAgentScope; onChange: (v: UiAgentScope) => void }) {
  return (
    <select value={value} onChange={(e) => onChange(e.target.value as UiAgentScope)}>
      <option value="project">项目（.leveler/agents）</option>
      <option value="user">用户（~/.leveler/agents）</option>
    </select>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="af-field">
      <span className="af-label">{label}</span>
      {children}
    </label>
  );
}

function Confirm({
  draft,
  scope,
  pending,
  onBack,
  onSend,
}: {
  draft: UiAgentDraft;
  scope: UiAgentScope;
  pending: boolean;
  onBack: () => void;
  onSend: () => void;
}) {
  return (
    <>
      <div className="set-hint">确认后写入定义文件。runtime 会再校验一遍。</div>
      <dl className="af-summary">
        {draftSummary(draft, scope).map(([k, v]) => (
          <Fragment key={k}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </Fragment>
        ))}
      </dl>
      <div className="af-ops">
        <button type="button" className="abtn" onClick={onBack} disabled={pending}>
          返回修改
        </button>
        <button type="button" className="abtn primary" onClick={onSend} disabled={pending}>
          {pending ? '提交中…' : '确认创建'}
        </button>
      </div>
    </>
  );
}

function AgentCreate({ onClose }: { onClose: () => void }) {
  const bridge = useBridge();
  const [form, setForm] = useState<CreateAgentForm>(newCreateForm);
  const [confirming, setConfirming] = useState(false);
  const { pending, error, track } = useMutation(onClose);
  const set = (patch: Partial<CreateAgentForm>) => setForm((f) => ({ ...f, ...patch }));
  const draft = buildCreateDraft(form);

  return (
    <AgentModal title="新建 Agent" onClose={onClose}>
      {error && <div className="ae-error">{error}</div>}
      {confirming ? (
        <Confirm
          draft={draft}
          scope={form.scope}
          pending={pending}
          onBack={() => setConfirming(false)}
          onSend={() => track(bridge.createAgent(form.scope, draft), '已创建')}
        />
      ) : (
        <>
          <Field label="名称">
            <input value={form.name} placeholder="security-reviewer" onChange={(e) => set({ name: e.target.value })} />
          </Field>
          <Field label="这个 Agent 做什么？">
            <input
              value={form.description}
              placeholder="一句话说明，用于派发时挑选"
              onChange={(e) => set({ description: e.target.value })}
            />
          </Field>
          <Field label="模板">
            <select
              value={form.template}
              onChange={(e) => setForm((f) => switchTemplate(f, e.target.value as AgentTemplate))}
            >
              {(Object.keys(TEMPLATES) as AgentTemplate[]).map((t) => (
                <option key={t} value={t}>
                  {TEMPLATES[t].label}
                </option>
              ))}
            </select>
          </Field>
          <Field label="模型">
            <ModelSelect value={form.model} onChange={(model) => set({ model })} />
          </Field>
          <Field label="权限">
            <CapabilitySelect value={form.capability} onChange={(capability) => set({ capability })} />
          </Field>
          <Field label="保存到">
            <ScopeSelect value={form.scope} onChange={(scope) => set({ scope })} />
          </Field>
          <Field label="说明（instructions）">
            <textarea rows={6} value={form.instructions} onChange={(e) => set({ instructions: e.target.value })} />
          </Field>
          <div className="af-ops">
            <button type="button" className="abtn" onClick={onClose}>
              取消
            </button>
            <button
              type="button"
              className="abtn primary"
              disabled={!form.name.trim()}
              onClick={() => setConfirming(true)}
            >
              下一步
            </button>
          </div>
        </>
      )}
    </AgentModal>
  );
}

/** 编辑项目 / 用户 agent；mode=duplicate 时以内置 persona 为底稿新建一个。 */
function AgentEditor({
  mode,
  name,
  scope: initialScope,
  onClose,
}: {
  mode: 'edit' | 'duplicate';
  name: string;
  scope: UiAgentScope;
  onClose: () => void;
}) {
  const bridge = useBridge();
  const loaded = useAppState().agents.detail[name];
  const problem = loaded?.agent ? definitionProblem(loaded.agent) : null;
  const [form, setForm] = useState<EditAgentForm | null>(null);
  const [scope, setScope] = useState<UiAgentScope>(initialScope);
  const [hint, setHint] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<UiAgentDraft | null>(null);
  const { pending, error, track } = useMutation(onClose);

  useEffect(() => {
    bridge.getAgent(name);
  }, [bridge, name]);

  useEffect(() => {
    if (form || !loaded?.agent || definitionProblem(loaded.agent)) return;
    const base = editFormFromDetail(loaded.agent);
    setForm(mode === 'duplicate' ? { ...base, name: `${base.name}-copy` } : base);
  }, [loaded, form, mode]);

  const set = (patch: Partial<EditAgentForm>) => setForm((f) => (f ? { ...f, ...patch } : f));
  const title = mode === 'edit' ? `编辑 Agent · ${name}` : `复制 ${name} 为新 Agent`;

  const save = () => {
    if (!form) return;
    const out = buildEditDraft(form);
    if ('error' in out) {
      setHint(out.error);
      return;
    }
    setHint(null);
    if (mode === 'duplicate') setConfirm(out.draft);
    else track(bridge.updateAgent(scope, out.draft), '已保存');
  };

  const remove = () => {
    if (window.confirm(deleteConfirmText(name, scope, loaded?.agent?.entry.location))) {
      track(bridge.deleteAgent(scope, name), '已删除');
    }
  };

  return (
    <AgentModal title={title} onClose={onClose}>
      {error && <div className="ae-error">{error}</div>}
      {hint && <div className="ae-error">{hint}</div>}
      {!form && (loaded?.error || problem) && (
        <>
          {loaded?.error && <div className="ae-error">{loaded.error}</div>}
          {problem && (
            <>
              <div className="ae-meta">
                {SOURCE_LABEL[scope]} · {loaded?.agent?.entry.location ?? ''}
              </div>
              <div className="ae-error">这个定义无效，不能派发：{problem}</div>
              <div className="set-hint">直接修改上面目录里的 agent.yaml / instructions.md 修复，或删除它。</div>
            </>
          )}
          {mode === 'edit' && (
            // 读不出来的定义也要能删掉，否则 UI 里没有出路。
            <div className="af-ops">
              <button type="button" className="abtn danger" onClick={remove} disabled={pending}>
                删除
              </button>
            </div>
          )}
        </>
      )}
      {!form && !loaded?.error && !problem && <div className="insp-empty">读取定义中…</div>}
      {form && confirm && (
        <Confirm
          draft={confirm}
          scope={scope}
          pending={pending}
          onBack={() => setConfirm(null)}
          onSend={() => track(bridge.createAgent(scope, confirm), '已创建')}
        />
      )}
      {form && !confirm && (
        <>
          {mode === 'duplicate' ? (
            <>
              <Field label="新名称">
                <input value={form.name} onChange={(e) => set({ name: e.target.value })} />
              </Field>
              <Field label="保存到">
                <ScopeSelect value={scope} onChange={setScope} />
              </Field>
            </>
          ) : (
            <div className="ae-meta">
              {SOURCE_LABEL[scope]} · {loaded?.agent?.entry.location ?? ''}
            </div>
          )}
          <Field label="描述">
            <input value={form.description} onChange={(e) => set({ description: e.target.value })} />
          </Field>
          <Field label="权限">
            <CapabilitySelect value={form.capability} onChange={(capability) => set({ capability })} />
          </Field>
          <Field label="模型">
            <ModelSelect value={form.model} onChange={(model) => set({ model })} />
          </Field>
          <Field label="推理强度">
            <select value={form.reasoningEffort} onChange={(e) => set({ reasoningEffort: e.target.value })}>
              <option value="">inherit</option>
              {form.reasoningEffort && !(REASONING_EFFORTS as readonly string[]).includes(form.reasoningEffort) && (
                <option value={form.reasoningEffort}>{form.reasoningEffort}</option>
              )}
              {REASONING_EFFORTS.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
          </Field>
          <Field label="工具（逗号分隔；留空 = 该权限的全部工具）">
            <input value={form.toolsText} onChange={(e) => set({ toolsText: e.target.value })} />
          </Field>
          <Field label="可写目录 write_roots（逗号分隔）">
            <input value={form.writeRootsText} onChange={(e) => set({ writeRootsText: e.target.value })} />
          </Field>
          <Field label="Skills（逗号分隔）">
            <input value={form.skillsText} onChange={(e) => set({ skillsText: e.target.value })} />
          </Field>
          <div className="af-pair">
            <Field label="max_rounds">
              <input inputMode="numeric" value={form.maxRounds} onChange={(e) => set({ maxRounds: e.target.value })} />
            </Field>
            <Field label="max_duration_secs">
              <input
                inputMode="numeric"
                value={form.maxDurationSecs}
                onChange={(e) => set({ maxDurationSecs: e.target.value })}
              />
            </Field>
          </div>
          <Field label="说明（instructions）">
            <textarea rows={10} value={form.instructions} onChange={(e) => set({ instructions: e.target.value })} />
          </Field>
          <div className="af-ops">
            {mode === 'edit' && (
              <button type="button" className="abtn danger" onClick={remove} disabled={pending}>
                删除
              </button>
            )}
            <button type="button" className="abtn" onClick={onClose}>
              取消
            </button>
            <button type="button" className="abtn primary" onClick={save} disabled={pending || !form.name.trim()}>
              {pending ? '提交中…' : mode === 'edit' ? '保存' : '下一步'}
            </button>
          </div>
        </>
      )}
    </AgentModal>
  );
}
