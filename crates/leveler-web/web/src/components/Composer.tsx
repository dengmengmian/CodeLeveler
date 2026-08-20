// 输入舱：轻量消息队列 + 斜杠命令面板 + 运行设置/模型弹层 + 发送。
// 控制区分两层：第一层输入内容；第二层执行设置 —— 左侧附件/上下文，
// 中间「权限 · 模式」合并入口，右侧模型 + 发送；停止由顶部全局状态栏负责。
// 交互：Enter 发送、Shift+Enter 换行、/ 唤起命令、回合进行中发送排队。

import {
  ArrowDown,
  ArrowUp,
  Image as ImageIcon,
  Paperclip,
  X,
} from 'lucide-react';
import { useCallback, useEffect, useRef, useState } from 'react';
import { uploadAttachment } from '../lib/api';
import {
  collaborationLabel,
  modelLabel,
  modelRefString,
  reasoningLabel,
  workProfileLabel,
} from '../lib/format';
import { CTRL_ICON } from '../lib/icons';
import { runConfigCompact } from '../lib/runConfig';
import {
  filterSlashCommands,
  groupSlashCommands,
  slashTarget,
  type SlashPopup,
} from '../lib/slashCommands';
import { useBridge } from '../state/bridge';
import { useAppState } from '../state/store';
import type { ModelRef, PermissionProfile } from '../types/protocol';

type Popup = SlashPopup | 'run' | null;
type ActiveCommand = 'btw' | null;

type PickerItem = {
  key: string;
  label: string;
  desc: string;
  current?: boolean;
  run: () => void;
};

const PERMISSIONS: ReadonlyArray<{
  profile: PermissionProfile;
  label: string;
  desc: string;
  tag: string;
  color: string;
}> = [
  { profile: 'request_approval', label: '逐次确认', desc: '每个写操作都询问', tag: '最严', color: 'var(--warning)' },
  { profile: 'assisted', label: '辅助模式', desc: '低风险自动，高风险询问', tag: '推荐', color: 'var(--accent)' },
  { profile: 'full_access', label: '完全访问', desc: '全部自动执行，不询问', tag: '危险', color: 'var(--danger)' },
];

/** 产品轴选项（wire 值 → 描述）。含义以 runtime 为准，这里只解释。 */
const WORK_OPTIONS: ReadonlyArray<readonly [string, string]> = [
  ['economy', '省着用：更少的探索与验证轮次'],
  ['balanced', '默认：探索、实现、验证均衡'],
  ['delivery', '交付：更充分的验证与收口'],
];

const COLLAB_OPTIONS: ReadonlyArray<readonly [string, string]> = [
  ['chat', '普通交互执行'],
  ['plan', '只读出方案，确认后再执行'],
  ['goal', '目标闭环：自动验证直至完成'],
];

export function Composer() {
  const state = useAppState();
  const bridge = useBridge();
  const [text, setText] = useState('');
  const [popup, setPopup] = useState<Popup>(null);
  const [slashIndex, setSlashIndex] = useState(0);
  const [pickerIndex, setPickerIndex] = useState(0);
  const [activeCommand, setActiveCommand] = useState<ActiveCommand>(null);
  const [uploading, setUploading] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  const current = state.current;
  const turnActive = current?.turnActive ?? false;
  const queue = state.queue.filter((q) => q.sessionId === current?.id);
  const workProfile = current?.workProfile ?? 'balanced';
  const collaboration = current?.collaboration ?? 'chat';
  const reasoning = reasoningLabel(current?.reasoningEffort ?? null);
  const models = current?.availableModels ?? [];
  const currentModelRef = current?.model ? modelRefString(current.model) : null;
  const attachments = state.pendingAttachments;

  const slashOpen = !activeCommand && text.startsWith('/');
  const query = slashOpen ? text.slice(1).toLowerCase() : '';
  const hits = slashOpen && !text.includes(' ') ? filterSlashCommands(query) : [];
  const showSlash = hits.length > 0;
  const slashGroups = query.trim() ? [{ label: '', items: hits }] : groupSlashCommands(hits);

  // 点击组件外关闭弹层
  useEffect(() => {
    if (!popup) return;
    const onDocClick = (e: MouseEvent) => {
      if (!(e.target as HTMLElement).closest('.perm-wrap')) setPopup(null);
    };
    document.addEventListener('click', onDocClick);
    return () => document.removeEventListener('click', onDocClick);
  }, [popup]);

  useEffect(() => {
    setPickerIndex(0);
  }, [popup]);

  // 通知条 6s 自动消失
  useEffect(() => {
    if (!state.notice) return;
    const t = setTimeout(() => bridge.dismissNotice(), 6000);
    return () => clearTimeout(t);
  }, [state.notice, bridge]);

  // 空状态快捷操作注入的起手语：填入输入框、聚焦、把光标放到末尾，随后清空 seed。
  useEffect(() => {
    if (state.composerSeed === null) return;
    setText(state.composerSeed);
    bridge.seedComposer(null);
    requestAnimationFrame(() => {
      const ta = taRef.current;
      if (ta) {
        ta.focus();
        ta.selectionStart = ta.selectionEnd = ta.value.length;
        ta.style.height = 'auto';
        ta.style.height = `${ta.scrollHeight}px`;
      }
    });
  }, [state.composerSeed, bridge]);

  const autosize = useCallback(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = 'auto';
    ta.style.height = `${ta.scrollHeight}px`;
  }, []);

  const send = useCallback(() => {
    const value = taRef.current?.value ?? text;
    if (!value.trim()) {
      taRef.current?.focus();
      return;
    }
    if (activeCommand === 'btw') {
      bridge.sendBtw(value);
      setActiveCommand(null);
    } else {
      void bridge.sendUserMessage(value);
    }
    setText('');
    requestAnimationFrame(autosize);
  }, [bridge, text, autosize, activeCommand]);

  const pickSlash = useCallback(
    (cmd: string) => {
      const target = slashTarget(cmd);
      setText('');
      setSlashIndex(0);
      if (!target) {
        taRef.current?.focus();
        return;
      }
      if (target.kind === 'action') {
        bridge.runSlash(target.command);
      } else if (target.kind === 'selector' || target.kind === 'entity-picker') {
        setPopup(target.popup);
      } else if (target.kind === 'navigation') {
        if (target.dest === 'diff') bridge.openChanges();
        if (target.dest === 'memory') bridge.openMemory();
      } else if (target.kind === 'input-mode') {
        setActiveCommand('btw');
      }
      taRef.current?.focus();
      requestAnimationFrame(autosize);
    },
    [bridge, autosize],
  );

  const pickerItems: PickerItem[] = (() => {
    if (popup === 'model') {
      return models.map((m) => {
        const ref = modelRefString(m);
        return {
          key: ref,
          label: m.model,
          desc: m.provider,
          current: ref === currentModelRef,
          run: () => {
            bridge.setModel(m);
            setPopup(null);
          },
        };
      });
    }
    if (popup === 'work') {
      return WORK_OPTIONS.map(([w, desc]) => ({
        key: w,
        label: workProfileLabel(w),
        desc,
        current: workProfile === w,
        run: () => {
          bridge.setAxes(w, collaboration);
          setPopup(null);
        },
      }));
    }
    if (popup === 'collab') {
      return COLLAB_OPTIONS.map(([c, desc]) => ({
        key: c,
        label: collaborationLabel(c),
        desc,
        current: collaboration === c,
        run: () => {
          bridge.setAxes(workProfile, c);
          setPopup(null);
        },
      }));
    }
    if (popup === 'perm') {
      return PERMISSIONS.map((p) => ({
        key: p.profile,
        label: p.label,
        desc: p.desc,
        current: current?.permission === p.profile,
        run: () => {
          bridge.setPermission(p.profile);
          setPopup(null);
        },
      }));
    }
    if (popup === 'checkpoint') {
      return (current?.checkpoints ?? []).map((c) => ({
        key: c.id,
        label: c.label || `#${c.ordinal}`,
        desc: `#${c.ordinal}`,
        run: () => {
          bridge.restoreCheckpoint(c.id);
          setPopup(null);
        },
      }));
    }
    return [];
  })();

  const pickerTitle =
    popup === 'model'
      ? 'Model'
      : popup === 'work'
        ? 'Work Profile'
        : popup === 'collab'
          ? 'Collaboration'
          : popup === 'perm'
            ? 'Permission'
            : popup === 'checkpoint'
              ? 'Restore checkpoint'
              : null;

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === 'Escape') {
      if (showSlash) {
        setText('');
        e.preventDefault();
        return;
      }
      if (popup) {
        setPopup(null);
        e.preventDefault();
        return;
      }
      if (activeCommand) {
        setActiveCommand(null);
        e.preventDefault();
        return;
      }
      if (turnActive) {
        bridge.cancelTurn();
        e.preventDefault();
      }
      return;
    }
    if (showSlash) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSlashIndex((i) => Math.min(i + 1, hits.length - 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSlashIndex((i) => Math.max(i - 1, 0));
        return;
      }
      if (e.key === 'Tab' || e.key === 'Enter') {
        e.preventDefault();
        pickSlash(hits[Math.min(slashIndex, hits.length - 1)].command);
        return;
      }
    }
    if (popup && popup !== 'run' && pickerItems.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setPickerIndex((i) => Math.min(i + 1, pickerItems.length - 1));
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setPickerIndex((i) => Math.max(i - 1, 0));
        return;
      }
      if (e.key === 'Tab' || e.key === 'Enter') {
        e.preventDefault();
        pickerItems[Math.min(pickerIndex, pickerItems.length - 1)]?.run();
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  };

  const pickFiles = () => {
    if (!current) {
      bridge.notice('先进入会话再添加附件');
      return;
    }
    fileRef.current?.click();
  };

  const onFilesChosen = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const files = e.target.files;
    e.target.value = ''; // 允许重复选同一文件
    if (!files || !current) return;
    setUploading(true);
    try {
      for (const file of Array.from(files)) {
        await uploadAttachment(current.id, file);
      }
    } catch (err) {
      bridge.notice(`附件上传失败：${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setUploading(false);
    }
  };

  return (
    <div className="composer-wrap">
      <div className="composer">
        {state.notice && (
          <div className="notice">
            <span>{state.notice}</span>
            <button className="n-x" onClick={() => bridge.dismissNotice()} title="关闭">
              <X {...CTRL_ICON} aria-hidden="true" />
            </button>
          </div>
        )}

        {queue.length > 0 && (
          <div className="queue">
            {queue.length > 1 && <div className="q-head">消息队列 · {queue.length}</div>}
            {queue.map((q, i) => (
              <div className="q-row" key={q.id}>
                <span className="q-lead">{queue.length === 1 ? '↳ 下一条：' : `${i + 1}`}</span>
                <span className="q-text">{q.text}</span>
                {queue.length > 1 && (
                  <span className="q-ops">
                    <button
                      className="q-op"
                      title="上移"
                      disabled={i === 0}
                      onClick={() => bridge.moveQueued(q.id, -1)}
                    >
                      <ArrowUp {...CTRL_ICON} aria-hidden="true" />
                    </button>
                    <button
                      className="q-op"
                      title="下移"
                      disabled={i === queue.length - 1}
                      onClick={() => bridge.moveQueued(q.id, 1)}
                    >
                      <ArrowDown {...CTRL_ICON} aria-hidden="true" />
                    </button>
                  </span>
                )}
                <button className="q-x" onClick={() => bridge.cancelQueued(q.id)} title="取消排队">
                  <X {...CTRL_ICON} aria-hidden="true" />
                </button>
              </div>
            ))}
          </div>
        )}

        {attachments.length > 0 && (
          <div className="attach-row">
            {attachments.map((a) => (
              <span className="attach-chip" key={a.id} title={`${a.name} · ${a.mime_type}`}>
                {a.kind === 'image' ? (
                  <ImageIcon {...CTRL_ICON} aria-hidden="true" />
                ) : (
                  <Paperclip {...CTRL_ICON} aria-hidden="true" />
                )}{' '}
                {a.name}
                <button
                  className="attach-x"
                  title="从待发列表移除"
                  onClick={() => bridge.removeAttachment(a.id)}
                >
                  <X {...CTRL_ICON} aria-hidden="true" />
                </button>
              </span>
            ))}
          </div>
        )}

        <div className="box-outer">
          {showSlash && (
            <div className="slash-pop" role="listbox" aria-label="命令">
              {slashGroups.map((g) => (
                <div key={g.label || 'hits'} className="slash-group">
                  {g.label ? <div className="slash-group-h">{g.label}</div> : null}
                  {g.items.map((c) => {
                    const idx = hits.indexOf(c);
                    return (
                      <button
                        key={c.command}
                        type="button"
                        role="option"
                        aria-selected={idx === Math.min(slashIndex, hits.length - 1)}
                        className={`slash-item${idx === Math.min(slashIndex, hits.length - 1) ? ' sel' : ''}`}
                        onMouseEnter={() => setSlashIndex(idx)}
                        onClick={() => pickSlash(c.command)}
                      >
                        <span className="slash-cmd">{c.command}</span>
                        <span className="slash-desc">{c.description}</span>
                      </button>
                    );
                  })}
                </div>
              ))}
            </div>
          )}

          <div className="box">
            {activeCommand === 'btw' && (
              <div className="cmd-row">
                <span className="cmd-chip">
                  /btw
                  <button
                    type="button"
                    className="cmd-chip-x"
                    title="退出侧问"
                    aria-label="退出侧问"
                    onClick={() => setActiveCommand(null)}
                  >
                    <X {...CTRL_ICON} aria-hidden="true" />
                  </button>
                </span>
              </div>
            )}
            <textarea
              ref={taRef}
              rows={2}
              value={text}
              placeholder={
                activeCommand === 'btw'
                  ? '侧问 · 不打断当前回合'
                  : turnActive
                    ? '回合运行中，可继续输入并加入队列…… ( / 唤起命令 )'
                    : state.draft
                      ? '你想让 CodeLeveler 做什么？'
                      : '告诉 Agent 要完成什么，或输入 / 查看命令'
              }
              onChange={(e) => {
                const v = e.target.value;
                if (!activeCommand && (v === '/btw ' || v.startsWith('/btw '))) {
                  setActiveCommand('btw');
                  setText(v.slice('/btw '.length));
                  setSlashIndex(0);
                  requestAnimationFrame(autosize);
                  return;
                }
                setText(v);
                setSlashIndex(0);
                autosize();
              }}
              onKeyDown={onKeyDown}
            />
            <div className="c-bar">
              <input
                ref={fileRef}
                type="file"
                multiple
                hidden
                onChange={(e) => void onFilesChosen(e)}
              />
              <button
                type="button"
                className="c-icon-btn"
                title={current ? '附件' : '先进入会话再添加附件'}
                aria-label="附件"
                disabled={uploading}
                onClick={pickFiles}
              >
                <Paperclip {...CTRL_ICON} aria-hidden="true" />
                <span>{uploading ? '上传中…' : '附件'}</span>
              </button>
              <span className="spacer" />

              <span className="perm-wrap">
                <button
                  type="button"
                  className="c-chip run-summary"
                  title="运行配置"
                  aria-haspopup="dialog"
                  aria-expanded={popup === 'run'}
                  onClick={(e) => {
                    e.stopPropagation();
                    setPopup(popup === 'run' ? null : 'run');
                  }}
                >
                  {runConfigCompact({
                    modelLabel: modelLabel(current?.model),
                    workProfile,
                  })}
                </button>
                {popup === 'run' && (
                  <div className="pop pop-right run-pop" role="dialog" aria-label="Run Configuration">
                    <div className="pop-head">Run Configuration</div>
                    <div className="run-sec">Model</div>
                    {models.length === 0 && <div className="insp-empty">暂无可用模型</div>}
                    {models.map((m: ModelRef) => {
                      const ref = modelRefString(m);
                      const isCurrent = ref === currentModelRef;
                      return (
                        <button
                          key={ref}
                          type="button"
                          className={`pop-item${isCurrent ? ' sel' : ''}`}
                          onClick={() => bridge.setModel(m)}
                        >
                          <span className="cmd">{m.model}</span>
                          <span className="desc">{m.provider}</span>
                        </button>
                      );
                    })}
                    {reasoning && (
                      <>
                        <div className="run-sec">Reasoning</div>
                        <div className="run-readonly">{reasoning}</div>
                      </>
                    )}
                    <div className="run-sec">Work Profile</div>
                    {WORK_OPTIONS.map(([w, desc]) => (
                      <button
                        key={w}
                        type="button"
                        className={`pop-item${workProfile === w ? ' sel' : ''}`}
                        onClick={() => bridge.setAxes(w, collaboration)}
                      >
                        <span className="cmd">{workProfileLabel(w)}</span>
                        <span className="desc">{desc}</span>
                      </button>
                    ))}
                    <div className="run-sec">Collaboration</div>
                    {COLLAB_OPTIONS.map(([c, desc]) => (
                      <button
                        key={c}
                        type="button"
                        className={`pop-item${collaboration === c ? ' sel' : ''}`}
                        onClick={() => bridge.setAxes(workProfile, c)}
                      >
                        <span className="cmd">{collaborationLabel(c)}</span>
                        <span className="desc">{desc}</span>
                      </button>
                    ))}
                    <div className="run-sec">Permission</div>
                    {PERMISSIONS.map((p) => (
                      <button
                        key={p.profile}
                        type="button"
                        className={`pop-item${current?.permission === p.profile ? ' sel' : ''}`}
                        onClick={() => bridge.setPermission(p.profile)}
                      >
                        <span className="cmd" style={{ color: p.color }}>
                          {p.label}
                        </span>
                        <span className="desc">{p.desc}</span>
                      </button>
                    ))}
                  </div>
                )}
                {pickerTitle && (
                  <div className="pop pop-right run-pop" role="listbox" aria-label={pickerTitle}>
                    <div className="pop-head">{pickerTitle}</div>
                    {pickerItems.length === 0 && (
                      <div className="insp-empty">
                        {popup === 'checkpoint' ? '暂无检查点' : popup === 'model' ? '暂无可用模型' : '暂无选项'}
                      </div>
                    )}
                    {pickerItems.map((it, i) => (
                      <button
                        key={it.key}
                        type="button"
                        role="option"
                        aria-selected={i === Math.min(pickerIndex, pickerItems.length - 1)}
                        className={`pop-item${it.current || i === Math.min(pickerIndex, pickerItems.length - 1) ? ' sel' : ''}`}
                        onMouseEnter={() => setPickerIndex(i)}
                        onClick={it.run}
                      >
                        <span className="cmd">{it.label}</span>
                        <span className="desc">{it.desc}</span>
                      </button>
                    ))}
                  </div>
                )}
              </span>

              <button
                type="button"
                className="send-btn"
                title={turnActive ? '加入队列' : '发送'}
                aria-label={turnActive ? '加入队列' : '发送'}
                onClick={send}
              >
                <ArrowUp size={18} strokeWidth={2} aria-hidden="true" />
              </button>
            </div>
          </div>
        </div>
        <div className="hint">
          <kbd>Enter</kbd> 发送 · <kbd>Shift+Enter</kbd> 换行 · <kbd>/</kbd> 命令 · 回合进行中发送将
          <b>排队</b> · <kbd>Esc</kbd> 关闭面板 / 退出命令 / 取消回合
        </div>
      </div>
    </div>
  );
}
