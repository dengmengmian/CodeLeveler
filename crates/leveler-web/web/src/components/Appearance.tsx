// 外观 / 主题控件：顶栏快捷切换入口 + 设置弹层的「外观」预览卡片。
// 主题状态来自 lib/theme.ts（外部存储），组件通过 useSyncExternalStore 订阅，
// 切换立即生效并持久化。

import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import {
  THEME_OPTIONS,
  storedChoice,
  setThemeChoice,
  subscribeTheme,
  type ThemeChoice,
} from '../lib/theme';

function useThemeChoice(): ThemeChoice {
  return useSyncExternalStore(subscribeTheme, storedChoice, () => 'system');
}

/** 预览卡片的代表色（展示各主题外观，与 app.css 的调色板对应）。 */
const SWATCH: Record<'graphite' | 'midnight' | 'paper', { app: string; surface: string; accent: string; text: string }> = {
  graphite: { app: '#11161d', surface: '#19222d', accent: '#9ddd48', text: '#e8edf4' },
  midnight: { app: '#0f1520', surface: '#192434', accent: '#8fda45', text: '#edf3fb' },
  paper: { app: '#f4f6f8', surface: '#ffffff', accent: '#6fae1f', text: '#1b2530' },
};

function swatchFor(choice: ThemeChoice) {
  return SWATCH[choice === 'system' ? 'graphite' : choice];
}

// ── 顶栏快捷切换 ────────────────────────────────────────────────────

/** Header ⋯：外观 / 设置。不占主栏权重。 */
export function MoreMenu() {
  const [open, setOpen] = useState(false);
  const [settings, setSettings] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    window.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('click', onDocClick);
      window.removeEventListener('keydown', onKey);
    };
  }, [open]);

  return (
    <div className="more-menu" ref={wrapRef}>
      <button
        type="button"
        className="th-btn more-btn"
        title="更多"
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => setOpen((v) => !v)}
      >
        ⋯
      </button>
      {open && (
        <div className="th-pop" role="menu">
          <button
            type="button"
            className="th-item"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              setSettings(true);
            }}
          >
            <span className="th-name">Appearance</span>
          </button>
          <button
            type="button"
            className="th-item"
            role="menuitem"
            onClick={() => {
              setOpen(false);
              setSettings(true);
            }}
          >
            <span className="th-name">Settings</span>
          </button>
          <button type="button" className="th-item" role="menuitem" disabled>
            <span className="th-name">Keyboard Shortcuts</span>
          </button>
        </div>
      )}
      {settings && <SettingsModal onClose={() => setSettings(false)} />}
    </div>
  );
}

export function ThemeMenu() {
  const choice = useThemeChoice();
  const [open, setOpen] = useState(false);
  const wrapRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('click', onDocClick);
    return () => document.removeEventListener('click', onDocClick);
  }, [open]);

  const label = THEME_OPTIONS.find((t) => t.choice === choice)?.label ?? '主题';

  return (
    <div className="theme-menu" ref={wrapRef}>
      <button className="th-btn" title="切换主题" onClick={() => setOpen((v) => !v)}>
        <span className="th-dot" style={{ background: swatchFor(choice).accent }} />
        <span>{label}</span>
        <span className="caret">▾</span>
      </button>
      {open && (
        <div className="th-pop">
          {THEME_OPTIONS.map((t) => (
            <button
              key={t.choice}
              className={`th-item${choice === t.choice ? ' sel' : ''}`}
              onClick={() => {
                setThemeChoice(t.choice);
                setOpen(false);
              }}
            >
              <span className="th-swatch">
                <i style={{ background: swatchFor(t.choice).app }} />
                <i style={{ background: swatchFor(t.choice).surface }} />
                <i style={{ background: swatchFor(t.choice).accent }} />
              </span>
              <span className="th-name">{t.label}</span>
              {choice === t.choice && <span className="th-cur">当前</span>}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

// ── 设置弹层：外观配置区（预览卡片）───────────────────────────────────

export function SettingsButton() {
  const [open, setOpen] = useState(false);
  return (
    <>
      <button className="set-gear" title="设置" onClick={() => setOpen(true)}>
        ⚙
      </button>
      {open && <SettingsModal onClose={() => setOpen(false)} />}
    </>
  );
}

/** Theme cards for the Settings sidebar (no extra API). */
export function AppearancePanel() {
  const choice = useThemeChoice();
  return (
    <div className="set-body">
      <div className="set-sec">外观</div>
      <div className="set-hint">主题立即生效并保存到本机，刷新后保持。</div>
      <div className="theme-cards">
        {THEME_OPTIONS.map((t) => {
          const sw = swatchFor(t.choice);
          const active = choice === t.choice;
          return (
            <button
              key={t.choice}
              className={`theme-card${active ? ' active' : ''}`}
              onClick={() => setThemeChoice(t.choice)}
            >
              <span
                className="tc-preview"
                style={{ background: sw.app, borderColor: sw.surface }}
              >
                <i className="tc-side" style={{ background: sw.surface }} />
                <i className="tc-bar" style={{ background: sw.accent }} />
                <i className="tc-line" style={{ background: sw.text, opacity: 0.5 }} />
                <i className="tc-line short" style={{ background: sw.text, opacity: 0.3 }} />
              </span>
              <span className="tc-meta">
                <span className="tc-name">
                  {t.label}
                  {active && <span className="tc-cur">当前</span>}
                </span>
                <span className="tc-desc">{t.desc}</span>
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}

function SettingsModal({ onClose }: { onClose: () => void }) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div className="set-backdrop" onClick={onClose}>
      <div className="set" onClick={(e) => e.stopPropagation()}>
        <div className="set-head">
          <span className="set-title">设置</span>
          <button className="fv-x" title="关闭（Esc）" onClick={onClose}>
            ✕
          </button>
        </div>
        <AppearancePanel />
      </div>
    </div>
  );
}
