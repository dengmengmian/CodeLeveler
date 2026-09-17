// 工具调用分类与聚合：把一轮里的 ToolCallView 归并成「读 / 搜 / 命令 / 改动 / 其他」，
// 供对话内运行摘要生成「N 次操作 · 读取 N 个文件 · N 个非阻塞错误」这类一行统计。
// 纯展示层派生，不改协议。

import type { ToolCallView } from '../state/store';

export type ToolCategory = 'read' | 'search' | 'command' | 'edit' | 'other';

export function categorizeTool(name: string): ToolCategory {
  const n = name.toLowerCase();
  if (/apply_patch|edit|write|patch/.test(n)) return 'edit';
  if (/read|cat|open|view/.test(n)) return 'read';
  if (/search|grep|find|glob|list/.test(n)) return 'search';
  if (/bash|shell|exec|command|run|terminal|cargo|npm|git_(?!diff)/.test(n)) return 'command';
  return 'other';
}

export interface ToolStats {
  total: number;
  reads: number;
  searches: number;
  commands: number;
  edits: number;
  others: number;
  failures: number;
}

export function summarizeTools(tools: readonly ToolCallView[]): ToolStats {
  const stats: ToolStats = {
    total: tools.length,
    reads: 0,
    searches: 0,
    commands: 0,
    edits: 0,
    others: 0,
    failures: 0,
  };
  for (const t of tools) {
    if (t.status === 'fail') stats.failures += 1;
    switch (categorizeTool(t.name)) {
      case 'read':
        stats.reads += 1;
        break;
      case 'search':
        stats.searches += 1;
        break;
      case 'command':
        stats.commands += 1;
        break;
      case 'edit':
        stats.edits += 1;
        break;
      default:
        stats.others += 1;
    }
  }
  return stats;
}

/** 统计 → 一行人类可读摘要（「38 次操作 · 读取 16 个文件 · 1 个非阻塞错误」）。 */
export function statsLine(stats: ToolStats): string {
  const parts: string[] = [`${stats.total} 次操作`];
  if (stats.reads > 0) parts.push(`读取 ${stats.reads} 个文件`);
  if (stats.searches > 0) parts.push(`搜索代码 ${stats.searches} 次`);
  if (stats.commands > 0) parts.push(`执行命令 ${stats.commands} 次`);
  if (stats.edits > 0) parts.push(`修改 ${stats.edits} 处`);
  if (stats.failures > 0) parts.push(`${stats.failures} 个非阻塞错误`);
  return parts.join(' · ');
}

/** 取文本末尾 N 行（失败命令默认展开末尾输出）。 */
export function tailLines(text: string, n: number): string {
  const lines = text.replace(/\r/g, '').split('\n');
  return lines.length <= n ? text : lines.slice(lines.length - n).join('\n');
}

/** 秒数 → 「48s / 1m26s」。 */
export function formatSeconds(sec: number): string {
  if (sec < 60) return `${sec}s`;
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  return s > 0 ? `${m}m${String(s).padStart(2, '0')}s` : `${m}m`;
}
