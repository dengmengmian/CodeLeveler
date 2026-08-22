// Conversation presentation kind. Canonical facts:
// - BTW: ChatMessage.btw from RuntimeEvent.btw_* (not persisted in transcript).
// - Compaction: runtime stamps COMPACTION_SUMMARY_PREFIX on the replacement
//   user message (leveler_client_protocol::COMPACTION_SUMMARY_PREFIX). Same
//   contract the TUI uses. Not a UI-invented Chinese heuristic.

/** Stamped by the runtime on the message that replaces compacted history. */
export const COMPACTION_SUMMARY_PREFIX = '对话摘要（已压缩历史）';

export type PresentationKind = 'normal' | 'btw' | 'compaction_summary';

export function isCompactionSummaryText(text: string): boolean {
  return text.startsWith(COMPACTION_SUMMARY_PREFIX);
}

/** Markdown body after the runtime prefix. */
export function compactionBodyMarkdown(text: string): string {
  if (!isCompactionSummaryText(text)) return text;
  return text.slice(COMPACTION_SUMMARY_PREFIX.length).replace(/^：\s*/, '').trimStart();
}

export function isTurnUser(m: {
  role: string;
  btw?: string;
  kind?: PresentationKind;
}): boolean {
  return m.role === 'user' && m.btw === undefined && m.kind !== 'compaction_summary';
}

export function presentationKindOf(m: {
  role: string;
  text: string;
  btw?: string;
  kind?: PresentationKind;
}): PresentationKind {
  if (m.btw !== undefined) return 'btw';
  if (m.kind === 'compaction_summary') return 'compaction_summary';
  if (m.role === 'user' && isCompactionSummaryText(m.text)) return 'compaction_summary';
  return 'normal';
}
