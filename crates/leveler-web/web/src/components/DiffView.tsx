// 改动审阅工作区：左侧 changed files，右侧单文件 unified diff。
// 数据来自 snapshot.diff + diff_updated；打开时 request_diff。

import { useEffect, useMemo, useState } from 'react';
import { useAppDispatch, useAppState } from '../state/store';
import { useBridge } from '../state/bridge';
import { completionTruth, trustLabel } from '../lib/completionTruth';
import { groupChangeFiles } from '../lib/changeFiles';
import {
  nextPath,
  numberPatch,
  prevPath,
  shouldCollapsePatch,
  type PatchLine,
} from '../lib/diffWorkspace';
import type { UiDiffFile } from '../types/protocol';

export function DiffView() {
  const current = useAppState().current;
  const focus = useAppState().diffFocus;
  const dispatch = useAppDispatch();
  const bridge = useBridge();

  useEffect(() => {
    bridge.requestDiff();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

  const files = current?.diff?.files ?? [];
  const paths = files.map((f) => f.path);
  const selected = (focus && paths.includes(focus) ? focus : null) ?? paths[0] ?? null;
  const file = files.find((f) => f.path === selected) ?? null;
  const grouped = groupChangeFiles(files);
  const truth = completionTruth(current);
  const totalAdd = files.reduce((n, f) => n + f.added, 0);
  const totalDel = files.reduce((n, f) => n + f.removed, 0);

  const select = (path: string) => dispatch({ type: 'focus_diff', path });

  if (!current) {
    return (
      <div className="diffview">
        <div className="insp-empty">加载会话中…</div>
      </div>
    );
  }

  return (
    <div className="diffview">
      <div className="dv-toolbar">
        <span className="changes-sum">
          <span className="n">{files.length} files</span>
          <span className="add">+{totalAdd}</span>
          <span className="del">−{totalDel}</span>
        </span>
        {truth && (
          <span className={`ch-truth tone-${truth.tone}`}>
            {truth.glyph} {truth.title}
            <span className="ch-trust">{trustLabel(truth.trust)}</span>
          </span>
        )}
        <span className="dv-nav">
          <button
            type="button"
            className="dv-refresh"
            disabled={!selected}
            onClick={() => selected && select(prevPath(paths, selected) ?? selected)}
          >
            Previous
          </button>
          <button
            type="button"
            className="dv-refresh"
            disabled={!selected}
            onClick={() => selected && select(nextPath(paths, selected) ?? selected)}
          >
            Next
          </button>
          <button type="button" className="dv-refresh" onClick={() => bridge.requestDiff()}>
            Refresh
          </button>
        </span>
      </div>
      {files.length === 0 ? (
        <div className="insp-empty">工作区干净，暂无变更。</div>
      ) : (
        <div className="dv-split">
          <nav className="dv-files" aria-label="Changed files">
            <FileGroup title="Modified" files={grouped.modified} selected={selected} onSelect={select} />
            <FileGroup title="Added" files={grouped.added} selected={selected} onSelect={select} />
            <FileGroup title="Deleted" files={grouped.deleted} selected={selected} onSelect={select} />
          </nav>
          <section className="dv-pane">
            {file && (
              <>
                <header className="dv-sticky">
                  <span className="p">{file.path}</span>
                  <span className="add">+{file.added}</span>
                  <span className="del">−{file.removed}</span>
                </header>
                {file.patch ? (
                  <PatchBody patch={file.patch} />
                ) : (
                  <div className="insp-empty">无 patch（二进制、过大或未跟踪）。</div>
                )}
              </>
            )}
          </section>
        </div>
      )}
      {truth && <ChangesFooter truth={truth} />}
    </div>
  );
}

function FileGroup({
  title,
  files,
  selected,
  onSelect,
}: {
  title: string;
  files: UiDiffFile[];
  selected: string | null;
  onSelect: (path: string) => void;
}) {
  if (files.length === 0) return null;
  return (
    <div className="dv-group">
      <div className="dv-group-h">
        {title} · {files.length}
      </div>
      {files.map((f) => (
        <button
          key={f.path}
          type="button"
          className={`dv-file-item${f.path === selected ? ' on' : ''}`}
          onClick={() => onSelect(f.path)}
        >
          <span className="p">{f.path}</span>
          <span className="nums">
            <span className="add">+{f.added}</span>
            <span className="del">−{f.removed}</span>
          </span>
        </button>
      ))}
    </div>
  );
}

function ChangesFooter({ truth }: { truth: NonNullable<ReturnType<typeof completionTruth>> }) {
  const dispatch = useAppDispatch();
  const bridge = useBridge();
  return (
    <footer className="ch-foot">
      <div className="ch-facts">
        {truth.facts.map((f) => (
          <span key={f}>{f}</span>
        ))}
      </div>
      <div className="ch-acts">
        {truth.pending !== 'none' && (
          <button type="button" className="dv-refresh" onClick={() => dispatch({ type: 'set_inspector', open: true })}>
            在任务面板处理
          </button>
        )}
        {truth.recoveryHint && (
          <button type="button" className="dv-refresh" onClick={() => bridge.rerunLast()}>
            {truth.recoveryHint}
          </button>
        )}
      </div>
    </footer>
  );
}

function PatchBody({ patch }: { patch: string }) {
  const lines = useMemo(() => numberPatch(patch), [patch]);
  const large = shouldCollapsePatch(lines.length);
  const [open, setOpen] = useState(!large);
  if (large && !open) {
    return (
      <div className="dv-collapse">
        <span>大 patch · {lines.length} 行</span>
        <button type="button" className="dv-refresh" onClick={() => setOpen(true)}>
          展开
        </button>
      </div>
    );
  }
  return (
    <pre className="dv-patch">
      {lines.map((line, i) => (
        <PatchRow key={i} line={line} />
      ))}
    </pre>
  );
}

function PatchRow({ line }: { line: PatchLine }) {
  return (
    <div className={`dv-line ${line.kind}`}>
      <span className="ln old">{line.oldNo ?? ''}</span>
      <span className="ln new">{line.newNo ?? ''}</span>
      <span className="tx">{line.text || ' '}</span>
    </div>
  );
}
