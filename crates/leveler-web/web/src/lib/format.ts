// 展示层小工具：时间、模型标签、工具调用摘要、会话状态点。

import type { ModelRef, PermissionProfile } from '../types/protocol';

/** HH:MM:SS（24 小时制）。 */
export function formatClock(d: Date = new Date()): string {
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** 会话列表的 "x 分钟前 / 昨天 / N 天前"；解析不了就原样截断显示。 */
export function formatRelative(raw: string): string {
  const t = Date.parse(raw);
  if (Number.isNaN(t)) return raw.length > 16 ? raw.slice(0, 16) : raw;
  const diff = Date.now() - t;
  if (diff < 60_000) return '刚刚';
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  const day = 86_400_000;
  if (diff < day) return `${Math.floor(diff / 3_600_000)}h ago`;
  if (diff < 2 * day) return '昨天';
  if (diff < 7 * day) return `${Math.floor(diff / day)} 天前`;
  if (diff < 14 * day) return '上周';
  return `${Math.floor(diff / (7 * day))} 周前`;
}

/** 模型选择器里显示的标签。 */
export function modelLabel(model: ModelRef | null | undefined): string {
  return model ? model.model : '默认模型';
}

/** ModelRef 的完整 `provider/model` 形式。 */
export function modelRefString(model: ModelRef): string {
  return `${model.provider}/${model.model}`;
}

/** 从仓库路径取项目名（最后一段）。 */
export function repoShortName(repository: string): string {
  const trimmed = repository.replace(/\/+$/, '');
  const name = trimmed.split('/').pop();
  return name || repository || '当前项目';
}

/** 会话状态 → 轨道上的状态点。 */
export function statusDot(status: string): { cls: 'run' | 'wait' | 'idle'; label: string } {
  const s = status.toLowerCase();
  if (s.includes('run') || s.includes('busy') || s.includes('active')) {
    return { cls: 'run', label: 'RUNNING' };
  }
  if (s.includes('wait') || s.includes('pending') || s.includes('approval')) {
    return { cls: 'wait', label: 'WAITING' };
  }
  return { cls: 'idle', label: status ? status.toUpperCase().slice(0, 12) : 'DONE' };
}

/**
 * 工具调用的一行摘要：`read <path>` / `bash <command>` / `grep "<pattern>"`。
 * arguments 是压缩 JSON，解析失败就回退到截断原文。
 */
export function toolSummary(name: string, argumentsJson: string): { verb: string; main: string } {
  let main = '';
  try {
    const args: unknown = JSON.parse(argumentsJson);
    if (args && typeof args === 'object') {
      const rec = args as Record<string, unknown>;
      const first =
        rec.path ?? rec.file ?? rec.command ?? rec.pattern ?? rec.query ?? rec.url ?? rec.content;
      if (typeof first === 'string') main = first;
      else {
        const keys = Object.keys(rec);
        if (keys.length > 0) {
          const v = rec[keys[0]];
          main = typeof v === 'string' ? v : JSON.stringify(v);
        }
      }
    }
  } catch {
    main = argumentsJson;
  }
  if (main.length > 80) main = `${main.slice(0, 77)}…`;
  return { verb: name, main };
}

/** 工具耗时展示：12ms / 3.2s。 */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

/** 千分位缩写 token 数：48.2k / 960。 */
export function formatTokens(n: number): string {
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`;
  return String(n);
}

export interface PermissionMeta {
  label: string;
  cls: 'p-ask' | 'p-assist' | 'p-full';
}

export function permissionMeta(profile: PermissionProfile): PermissionMeta {
  switch (profile) {
    case 'request_approval':
      return { label: '逐次确认', cls: 'p-ask' };
    case 'full_access':
      return { label: '完全访问', cls: 'p-full' };
    case 'assisted':
      return { label: '辅助模式', cls: 'p-assist' };
  }
}

/** agent 执行模式的中文标签。 */
export function agentModeLabel(mode: 'direct' | 'plan'): string {
  return mode === 'plan' ? '计划模式' : '直接执行';
}
