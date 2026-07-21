// 输入舱：消息队列 + 斜杠命令面板 + 运行设置/模型弹层 + 发送/停止。
// 控制区分两层：第一层输入内容；第二层执行设置 —— 左侧附件/上下文，
// 中间「权限 · 模式」合并入口，右侧模型 + 发送；停止仅运行中出现。
// 交互：Enter 发送、Shift+Enter 换行、/ 唤起命令、回合进行中发送排队。

import { useCallback, useEffect, useRef, useState } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { agentModeLabel, modelLabel, modelRefString, permissionMeta } from '../lib/format';
import { uploadAttachment } from '../lib/api';
import type { AgentMode } from '../state/store';
import type { ModelRef, PermissionProfile } from '../types/protocol';

/** 斜杠命令（cmd, 描述, 对应 ClientCommand 变体） */
const SLASH: ReadonlyArray<readonly [string, string, string]> = [
  ['/model', '切换模型', 'SelectModel'],
  ['/mode', '切换 agent 模式', 'SetAgentMode'],
  ['/perm', '切换权限档位', 'SetPermissionProfile'],
  ['/compact', '压缩上下文', 'CompactContext'],
  ['/clear', '清空对话', 'ClearConversation'],
  ['/diff', '查看当前变更', 'RequestDiff'],
  ['/checkpoint', '回滚到检查点', 'RestoreCheckpoint'],
  ['/memory', '查看 / 遗忘项目记忆', 'ListMemory / ForgetMemory'],
  ['/cancel', '取消当前回合', 'CancelCurrentTurn'],
  ['/btw', '侧问（不打断当前回合）', 'Btw'],
];

/** 选中后立即执行的命令（无参数） */
const EXEC_IMMEDIATELY = new Set(['/compact', '/clear', '/diff', '/cancel', '/memory']);
/** 选中后打开弹层的命令 → 弹层名 */
const OPEN_POPUP: Record<string, Popup> = {
  '/model': 'model',
  '/mode': 'settings',
  '/perm': 'settings',
};

type Popup = 'settings' | 'model' | null;

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

const MODES: ReadonlyArray<readonly [AgentMode, string]> = [
  ['direct', '直接执行，单 agent 循环'],
  ['plan', '先出计划，确认后执行'],
];

export function Composer() {
  const state = useAppState();
  const bridge = useBridge();
  const [text, setText] = useState('');
  const [popup, setPopup] = useState<Popup>(null);
  const [slashIndex, setSlashIndex] = useState(0);
  const [uploading, setUploading] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);

  const current = state.current;
  const turnActive = current?.turnActive ?? false;
  const queue = state.queue.filter((q) => q.sessionId === current?.id);

  // 斜杠面板命中项
  const slashOpen = text.startsWith('/');
  const query = slashOpen ? text.slice(1).toLowerCase() : '';
  const hits = slashOpen ? SLASH.filter((c) => c[0].slice(1).startsWith(query)) : [];
  const showSlash = slashOpen && hits.length > 0 && !text.includes(' ');

  // 点击组件外关闭弹层
  useEffect(() => {
    if (!popup) return;
    const onDocClick = (e: MouseEvent) => {
      if (!(e.target as HTMLElement).closest('.perm-wrap')) setPopup(null);
    };
    document.addEventListener('click', onDocClick);
    return () => document.removeEventListener('click', onDocClick);
  }, [popup]);

  // 通知条 6s 自动消失
  useEffect(() => {
    if (!state.notice) return;
    const t = setTimeout(() => bridge.dismissNotice(), 6000);
    return () => clearTimeout(t);
  }, [state.notice, bridge]);

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
    void bridge.sendUserMessage(value);
    setText('');
    requestAnimationFrame(autosize);
  }, [bridge, text, autosize]);

  const pickSlash = useCallback(
    (cmd: string) => {
      if (EXEC_IMMEDIATELY.has(cmd)) {
        setText('');
        bridge.runSlash(cmd);
      } else if (cmd in OPEN_POPUP) {
        setText('');
        setPopup(OPEN_POPUP[cmd]);
      } else {
        setText(`${cmd} `);
      }
      taRef.current?.focus();
      requestAnimationFrame(autosize);
    },
    [bridge, autosize],
  );

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
        pickSlash(hits[Math.min(slashIndex, hits.length - 1)][0]);
        return;
      }
    }
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      send();
    }
  };

  const perm = permissionMeta(current?.permission ?? 'assisted');
  const mode: AgentMode = current?.agentMode ?? 'direct';
  const models = current?.availableModels ?? [];
  const currentModelRef = current?.model ? modelRefString(current.model) : null;
  const attachments = state.pendingAttachments;

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
              ✕
            </button>
          </div>
        )}

        {queue.length > 0 && (
          <div className="queue">
            {queue.map((q) => (
              <div className="q-item" key={q.id}>
                <span className="q-tag">QUEUED</span>
                <span className="q-text">{q.text}</span>
                <button className="q-x" onClick={() => bridge.cancelQueued(q.id)} title="取消排队">
                  ✕
                </button>
              </div>
            ))}
          </div>
        )}

        {attachments.length > 0 && (
          <div className="attach-row">
            {attachments.map((a) => (
              <span className="attach-chip" key={a.id} title={`${a.name} · ${a.mime_type}`}>
                {a.kind === 'image' ? '🖼' : '📎'} {a.name}
                <button
                  className="attach-x"
                  title="从待发列表移除"
                  onClick={() => bridge.removeAttachment(a.id)}
                >
                  ✕
                </button>
              </span>
            ))}
          </div>
        )}

        <div className="box-outer">
          {showSlash && (
            <div className="slash-pop">
              <div className="pop-head">命令 · 输入继续过滤，↑↓ 选择，Tab 补全</div>
              <div>
                {hits.map((c, idx) => (
                  <button
                    key={c[0]}
                    className={`pop-item${idx === Math.min(slashIndex, hits.length - 1) ? ' sel' : ''}`}
                    onMouseEnter={() => setSlashIndex(idx)}
                    onClick={() => pickSlash(c[0])}
                  >
                    <span className="cmd">{c[0]}</span>
                    <span className="desc">{c[1]}</span>
                    <span className="cur">{c[2]}</span>
                  </button>
                ))}
              </div>
            </div>
          )}

          <div className="box">
            <textarea
              ref={taRef}
              rows={2}
              value={text}
              placeholder={
                turnActive
                  ? '回合进行中…发送将排队 ( / 唤起命令 )'
                  : '告诉 Agent 要完成什么，或输入 / 查看命令'
              }
              onChange={(e) => {
                setText(e.target.value);
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
                className="c-chip"
                title={current ? '上传附件（随下一条消息发送）' : '先进入会话再添加附件'}
                disabled={uploading}
                onClick={pickFiles}
              >
                {uploading ? '上传中…' : '＋ 附件'}
              </button>
              <button className="c-chip" title="上下文引用暂未开放" disabled>
                @ 上下文
              </button>

              <span className="perm-wrap">
                <button
                  className={`c-chip perm-btn ${perm.cls}`}
                  title="运行设置：权限档位 + 执行模式"
                  onClick={(e) => {
                    e.stopPropagation();
                    setPopup(popup === 'settings' ? null : 'settings');
                  }}
                >
                  <span className="shield">◈</span>
                  <span>{perm.label}</span>
                  <span className="sep">·</span>
                  <span>{agentModeLabel(mode)}</span>
                  <span className="caret">▴</span>
                </button>
                {popup === 'settings' && (
                  <div className="pop">
                    <div className="pop-head">权限档位 · 全局生效</div>
                    {PERMISSIONS.map((p) => (
                      <button
                        key={p.profile}
                        className={`pop-item${current?.permission === p.profile ? ' sel' : ''}`}
                        onClick={() => {
                          bridge.setPermission(p.profile);
                        }}
                      >
                        <span className="cmd" style={{ color: p.color }}>
                          {p.label}
                        </span>
                        <span className="desc">{p.desc}</span>
                        <span className="cur">{current?.permission === p.profile ? '当前' : p.tag}</span>
                      </button>
                    ))}
                    <div className="pop-head">执行模式</div>
                    {MODES.map(([m, desc]) => (
                      <button
                        key={m}
                        className={`pop-item${mode === m ? ' sel' : ''}`}
                        onClick={() => {
                          bridge.setAgentMode(m);
                        }}
                      >
                        <span className="cmd">{agentModeLabel(m)}</span>
                        <span className="desc">{desc}</span>
                        <span className="cur">{mode === m ? '当前' : ''}</span>
                      </button>
                    ))}
                  </div>
                )}
              </span>

              <span className="spacer" />

              <span className="perm-wrap">
                <button
                  className="c-chip"
                  onClick={(e) => {
                    e.stopPropagation();
                    setPopup(popup === 'model' ? null : 'model');
                  }}
                >
                  <b style={{ color: 'var(--text-primary)', fontWeight: 500 }}>{modelLabel(current?.model)}</b>{' '}
                  <span className="caret">▴</span>
                </button>
                {popup === 'model' && (
                  <div className="pop pop-right">
                    <div className="pop-head">模型 · 来自 snapshot.available_models</div>
                    {models.length === 0 && (
                      <button className="pop-item" disabled>
                        <span className="desc">暂无可用模型</span>
                      </button>
                    )}
                    {models.map((m: ModelRef) => {
                      const ref = modelRefString(m);
                      const isCurrent = ref === currentModelRef;
                      return (
                        <button
                          key={ref}
                          className={`pop-item${isCurrent ? ' sel' : ''}`}
                          onClick={() => {
                            bridge.setModel(m);
                            setPopup(null);
                          }}
                        >
                          <span className="cmd">{m.model}</span>
                          <span className="desc">{m.provider}</span>
                          <span className="cur">{isCurrent ? '当前' : ''}</span>
                        </button>
                      );
                    })}
                  </div>
                )}
              </span>

              {turnActive && (
                <button className="c-chip stop" title="取消当前回合 (Esc)" onClick={() => bridge.cancelTurn()}>
                  ■ 停止
                </button>
              )}
              <button className="send" onClick={send}>
                {turnActive ? '排队 ⏎' : '发送 ⏎'}
              </button>
            </div>
          </div>
        </div>
        <div className="hint">
          <kbd>Enter</kbd> 发送 · <kbd>Shift+Enter</kbd> 换行 · <kbd>/</kbd> 命令 · 回合进行中发送将
          <b>排队</b> · <kbd>Esc</kbd> 取消当前回合
        </div>
      </div>
    </div>
  );
}
