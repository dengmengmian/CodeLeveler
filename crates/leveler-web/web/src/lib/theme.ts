// 主题运行时：选择存 localStorage，实际生效主题写到 <html data-theme>。
// system 跟随操作系统（深→graphite，浅→paper）。无闪烁由 index.html 内联脚本
// 在首帧前先行写入 data-theme；此处负责运行期切换、持久化与系统主题监听。

export type ThemeChoice = 'system' | 'graphite' | 'midnight' | 'paper';
export type ResolvedTheme = 'graphite' | 'midnight' | 'paper';

const STORAGE_KEY = 'leveler.web.theme';

/** 快捷切换 / 设置卡片的展示顺序与文案。 */
export const THEME_OPTIONS: ReadonlyArray<{
  choice: ThemeChoice;
  label: string;
  desc: string;
}> = [
  { choice: 'system', label: '跟随系统', desc: '深色用 Graphite，浅色用 Paper' },
  { choice: 'graphite', label: 'Graphite', desc: '深灰蓝石墨 · 默认暗色' },
  { choice: 'midnight', label: 'Midnight', desc: '深蓝暗色 · 层级更分明' },
  { choice: 'paper', label: 'Paper', desc: '完整浅色主题' },
];

export function storedChoice(): ThemeChoice {
  const value = localStorage.getItem(STORAGE_KEY);
  if (value === 'graphite' || value === 'midnight' || value === 'paper' || value === 'system') {
    return value;
  }
  return 'system';
}

function prefersLight(): boolean {
  return window.matchMedia('(prefers-color-scheme: light)').matches;
}

/** 把选择解析为真正生效的主题（system → graphite/paper）。 */
export function resolveTheme(choice: ThemeChoice): ResolvedTheme {
  if (choice === 'system') return prefersLight() ? 'paper' : 'graphite';
  return choice;
}

function applyToDom(choice: ThemeChoice): void {
  document.documentElement.dataset.theme = resolveTheme(choice);
}

const listeners = new Set<() => void>();
let media: MediaQueryList | null = null;

function onSystemChange(): void {
  if (storedChoice() === 'system') {
    applyToDom('system');
    listeners.forEach((fn) => fn());
  }
}

/** 应用初始化：应用已存选择并挂上系统主题监听。 */
export function initTheme(): void {
  applyToDom(storedChoice());
  media = window.matchMedia('(prefers-color-scheme: light)');
  media.addEventListener('change', onSystemChange);
}

/** 运行期切换主题：立即生效 + 持久化 + 通知订阅者。 */
export function setThemeChoice(choice: ThemeChoice): void {
  localStorage.setItem(STORAGE_KEY, choice);
  applyToDom(choice);
  listeners.forEach((fn) => fn());
}

/** React 外部存储订阅（配合 useSyncExternalStore）。 */
export function subscribeTheme(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}
