import { describe, expect, it } from 'vitest';
import {
  COMPACTION_SUMMARY_PREFIX,
  compactionBodyMarkdown,
  isCompactionSummaryText,
  isTurnUser,
  presentationKindOf,
} from './presentationKind';

describe('compaction presentation', () => {
  it('recognizes the runtime-stamped prefix, not a UI-invented heuristic', () => {
    expect(COMPACTION_SUMMARY_PREFIX).toBe('对话摘要（已压缩历史）');
    expect(isCompactionSummaryText(`${COMPACTION_SUMMARY_PREFIX}：\n## 简报`)).toBe(true);
    expect(isCompactionSummaryText('看看这个项目')).toBe(false);
  });

  it('strips the prefix so Markdown is the body', () => {
    expect(compactionBodyMarkdown(`${COMPACTION_SUMMARY_PREFIX}：\n## 会话简报\n- a`)).toBe(
      '## 会话简报\n- a',
    );
  });
});

describe('presentationKindOf', () => {
  it('uses the live btw field, not text.startsWith', () => {
    expect(presentationKindOf({ role: 'assistant', text: 'hello', btw: '这是什么' })).toBe('btw');
    expect(presentationKindOf({ role: 'user', text: '/btw 这是什么' })).toBe('normal');
    expect(presentationKindOf({ role: 'user', text: '## 不要把我变标题' })).toBe('normal');
  });

  it('projects compaction from the stamped prefix on user messages', () => {
    expect(
      presentationKindOf({
        role: 'user',
        text: `${COMPACTION_SUMMARY_PREFIX}：\n## 简报`,
      }),
    ).toBe('compaction_summary');
  });
});

describe('isTurnUser', () => {
  it('does not treat compaction summaries as a user prompt', () => {
    expect(isTurnUser({ role: 'user', kind: 'compaction_summary' })).toBe(false);
    expect(isTurnUser({ role: 'user' })).toBe(true);
    expect(isTurnUser({ role: 'user', btw: 'q' })).toBe(false);
  });
});
