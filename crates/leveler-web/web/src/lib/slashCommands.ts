export type SlashKind = 'action' | 'selector' | 'entity-picker' | 'input-mode' | 'navigation';
export type SlashGroup = 'run' | 'conversation' | 'workspace' | 'context';

export interface SlashCommand {
  command: string;
  description: string;
  kind: SlashKind;
  group: SlashGroup;
  groupLabel: string;
}

export const SLASH_COMMANDS: readonly SlashCommand[] = [
  { command: '/model', description: '切换模型', kind: 'selector', group: 'run', groupLabel: 'RUN CONFIGURATION' },
  { command: '/work-mode', description: '工作档', kind: 'selector', group: 'run', groupLabel: 'RUN CONFIGURATION' },
  { command: '/collab', description: '协作档', kind: 'selector', group: 'run', groupLabel: 'RUN CONFIGURATION' },
  { command: '/perm', description: '权限档位', kind: 'selector', group: 'run', groupLabel: 'RUN CONFIGURATION' },
  { command: '/clear', description: '开始新对话（当前会话留在列表）', kind: 'action', group: 'conversation', groupLabel: 'CONVERSATION' },
  { command: '/btw', description: '侧问 · 不打断当前回合', kind: 'input-mode', group: 'conversation', groupLabel: 'CONVERSATION' },
  { command: '/cancel', description: '取消当前回合', kind: 'action', group: 'conversation', groupLabel: 'CONVERSATION' },
  { command: '/diff', description: '查看 Changes', kind: 'navigation', group: 'workspace', groupLabel: 'WORKSPACE' },
  { command: '/checkpoint', description: '回滚到检查点', kind: 'entity-picker', group: 'workspace', groupLabel: 'WORKSPACE' },
  { command: '/compact', description: '压缩上下文', kind: 'action', group: 'context', groupLabel: 'CONTEXT' },
  { command: '/memory', description: '打开项目记忆', kind: 'navigation', group: 'context', groupLabel: 'CONTEXT' },
];

export function filterSlashCommands(query: string): SlashCommand[] {
  const q = query.trim().toLowerCase().replace(/^\//, '');
  if (!q) return [...SLASH_COMMANDS];
  return SLASH_COMMANDS.filter((c) => c.command.slice(1).startsWith(q) || c.description.toLowerCase().includes(q));
}

export function groupSlashCommands(
  items: readonly SlashCommand[],
): Array<{ label: string; items: SlashCommand[] }> {
  const order: SlashGroup[] = ['run', 'conversation', 'workspace', 'context'];
  const labels: Record<SlashGroup, string> = {
    run: 'RUN CONFIGURATION',
    conversation: 'CONVERSATION',
    workspace: 'WORKSPACE',
    context: 'CONTEXT',
  };
  return order
    .map((g) => ({ label: labels[g], items: items.filter((i) => i.group === g) }))
    .filter((g) => g.items.length > 0);
}

export function slashByCommand(command: string): SlashCommand | undefined {
  return SLASH_COMMANDS.find((c) => c.command === command);
}

export type SlashPopup = 'model' | 'work' | 'collab' | 'perm' | 'checkpoint';

export type SlashTarget =
  | { kind: 'action'; command: string }
  | { kind: 'selector'; popup: Exclude<SlashPopup, 'checkpoint'> }
  | { kind: 'entity-picker'; popup: 'checkpoint' }
  | { kind: 'input-mode' }
  | { kind: 'navigation'; dest: 'diff' | 'memory' };

const SELECTOR_POPUP: Record<string, Exclude<SlashPopup, 'checkpoint'>> = {
  '/model': 'model',
  '/work-mode': 'work',
  '/collab': 'collab',
  '/perm': 'perm',
};

/** Map a slash command to the Web GUI interaction. Not a CLI string rewrite. */
export function slashTarget(command: string): SlashTarget | null {
  const def = slashByCommand(command);
  if (!def) return null;
  if (def.kind === 'action') return { kind: 'action', command };
  if (def.kind === 'selector') {
    const popup = SELECTOR_POPUP[command];
    return popup ? { kind: 'selector', popup } : null;
  }
  if (def.kind === 'entity-picker') return { kind: 'entity-picker', popup: 'checkpoint' };
  if (def.kind === 'input-mode') return { kind: 'input-mode' };
  if (command === '/diff') return { kind: 'navigation', dest: 'diff' };
  if (command === '/memory') return { kind: 'navigation', dest: 'memory' };
  return null;
}
