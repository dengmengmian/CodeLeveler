// Session Truth: one projection for Conversation, Changes, and Inspector.
// Does not invent protocol fields. Sources: lastTurn, verification, diff,
// completionReport. Diff totals and report file counts stay distinct.

import type { SessionView } from '../state/store';
import { artifactTotals } from './changeFiles';
import { presentTurnEnd, type TurnTone } from './turn';

export type CompletionKind = 'idle' | 'running' | 'waiting' | 'success' | 'warning' | 'failure';
export type TrustState = 'verified' | 'unverified' | 'failed' | 'pending' | 'n/a';
export type VerifyState = 'passed' | 'failed' | 'incomplete' | 'running' | 'none';

export type ArtifactSource = 'diff' | 'report' | 'none';

export interface ArtifactFacts {
  files: number;
  /** Exact diff totals only. Null when the count came from completionReport. */
  added: number | null;
  removed: number | null;
  source: ArtifactSource;
}

export interface CompletionTruth {
  kind: CompletionKind;
  title: string;
  glyph: string;
  tone: TurnTone;
  detail: string | null;
  trust: TrustState;
  verify: VerifyState;
  changesApplied: boolean;
  artifacts: ArtifactFacts;
  pending: 'approval' | 'clarification' | 'none';
  recoveryHint: string | null;
  facts: string[];
}

export function sessionArtifacts(s: SessionView): ArtifactFacts {
  if (s.diff) {
    const files = s.diff.files;
    if (files.length > 0) {
      const totals = artifactTotals(files);
      return { files: totals.files, added: totals.added, removed: totals.removed, source: 'diff' };
    }
    return { files: 0, added: null, removed: null, source: 'none' };
  }
  const reported = s.completionReport?.files_changed ?? 0;
  if (reported > 0) {
    return { files: reported, added: null, removed: null, source: 'report' };
  }
  return { files: 0, added: null, removed: null, source: 'none' };
}

export function completionTruth(s: SessionView | null): CompletionTruth | null {
  if (!s) return null;

  const artifacts = sessionArtifacts(s);
  const changesApplied = artifacts.source !== 'none';
  const verify = verifyState(s);

  if (s.pendingApprovals.length > 0) {
    return {
      kind: 'waiting',
      title: '等待确认',
      glyph: '⚠',
      tone: 'warn',
      detail: s.pendingApprovals[0]?.summary ?? null,
      trust: 'pending',
      verify,
      changesApplied,
      artifacts,
      pending: 'approval',
      recoveryHint: null,
      facts: ['需要你确认后才能继续', factChanges(artifacts), factVerify(verify)],
    };
  }
  if (s.pendingClarifications.length > 0) {
    return {
      kind: 'waiting',
      title: '需要回答',
      glyph: '⚠',
      tone: 'warn',
      detail: s.pendingClarifications[0]?.question ?? null,
      trust: 'pending',
      verify,
      changesApplied,
      artifacts,
      pending: 'clarification',
      recoveryHint: null,
      facts: ['需要补充信息', factChanges(artifacts), factVerify(verify)],
    };
  }
  if (s.turnActive) {
    return {
      kind: 'running',
      title: s.activity ?? '正在运行',
      glyph: '●',
      tone: 'success',
      detail: null,
      trust: 'n/a',
      verify: s.verification?.passed === null && (s.verification?.checks.length ?? 0) > 0 ? 'running' : verify,
      changesApplied,
      artifacts,
      pending: 'none',
      recoveryHint: null,
      facts: [factChanges(artifacts), factVerify(verify)],
    };
  }
  if (!s.lastTurn) {
    return {
      kind: 'idle',
      title: '空闲',
      glyph: '·',
      tone: 'muted',
      detail: null,
      trust: 'n/a',
      verify,
      changesApplied,
      artifacts,
      pending: 'none',
      recoveryHint: null,
      facts: [factChanges(artifacts), factVerify(verify)],
    };
  }

  const p = presentTurnEnd(s.lastTurn);
  const kind = kindFrom(s, p.tone);
  const trust = trustFrom(s, kind, verify);
  const recovery =
    s.lastTurn.outcome === 'failed' || s.lastTurn.outcome === 'truncated' || s.lastTurn.outcome === 'cancelled'
      ? '可重试上一轮'
      : null;
  return {
    kind,
    title: p.label,
    glyph: p.glyph,
    tone: p.tone,
    detail: p.detail,
    trust,
    verify,
    changesApplied,
    artifacts,
    pending: 'none',
    recoveryHint: recovery,
    facts: [
      factChanges(artifacts),
      factVerify(verify),
      pendingNone(kind),
    ].filter(Boolean) as string[],
  };
}

function verifyState(s: SessionView): VerifyState {
  const v = s.verification;
  if (v?.passed === true) return 'passed';
  if (v?.passed === false) return 'failed';
  if (v && v.passed == null && v.checks.some((c) => c.status === 'running')) return 'running';
  const detail = s.lastTurn?.detail ?? '';
  if (s.lastTurn?.outcome === 'incomplete' && detail.startsWith('failed gate(s)')) return 'failed';
  if (s.lastTurn?.outcome === 'unverified' && detail === 'no_automatic_verification') return 'incomplete';
  if (s.lastTurn?.outcome === 'unverified' && detail === 'no_code_changes') return 'none';
  if (v && v.checks.length > 0 && v.passed == null) return 'incomplete';
  return 'none';
}

function kindFrom(s: SessionView, tone: TurnTone): CompletionKind {
  const outcome = s.lastTurn?.outcome;
  if (tone === 'error' || outcome === 'failed' || outcome === 'truncated') return 'failure';
  if (tone === 'muted' || outcome === 'cancelled') return 'failure';
  if (tone === 'warn') return 'warning';
  if (tone === 'calm' && s.lastTurn?.detail === 'no_automatic_verification') return 'warning';
  if (tone === 'calm') return 'success';
  return 'success';
}

function trustFrom(s: SessionView, kind: CompletionKind, verify: VerifyState): TrustState {
  if (kind === 'waiting') return 'pending';
  if (verify === 'passed' && (s.lastTurn?.outcome === 'completed' || s.lastTurn?.outcome === 'answered')) {
    return 'verified';
  }
  if (verify === 'failed') return 'failed';
  if (verify === 'incomplete' || s.lastTurn?.outcome === 'unverified') return 'unverified';
  if (kind === 'failure') return 'failed';
  if (kind === 'success') return 'n/a';
  return 'unverified';
}

function factChanges(a: ArtifactFacts): string {
  if (a.source === 'none') return '未改仓库';
  if (a.source === 'report') return `${a.files} files changed`;
  return `${a.files} files  +${a.added} −${a.removed}`;
}

function factVerify(v: VerifyState): string {
  switch (v) {
    case 'passed':
      return '验证通过';
    case 'failed':
      return '验证未通过';
    case 'incomplete':
      return '验证未完成';
    case 'running':
      return '验证进行中';
    case 'none':
      return '无自动验证';
  }
}

function pendingNone(kind: CompletionKind): string | null {
  if (kind === 'success') return '无需等待操作';
  return null;
}

export function trustLabel(t: TrustState): string {
  switch (t) {
    case 'verified':
      return 'Verified';
    case 'unverified':
      return 'Unverified';
    case 'failed':
      return 'Failed';
    case 'pending':
      return 'Action required';
    case 'n/a':
      return '—';
  }
}
