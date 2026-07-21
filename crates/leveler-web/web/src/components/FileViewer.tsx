// 文件查看弹层：全局唯一，经 context 触发（文件引用 chip、左栏文件/搜索/Git 面板）。
// 数据走 REST /api/sessions/{id}/file；带行号，目标行高亮并滚动到位。

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { useAppState } from '../state/store';
import { readFile, type FileContent } from '../lib/api';

interface FileViewerTarget {
  path: string;
  line: number | null;
}

type OpenFile = (path: string, line?: number) => void;

const FileViewerContext = createContext<OpenFile | null>(null);

export function useOpenFile(): OpenFile {
  const open = useContext(FileViewerContext);
  if (!open) throw new Error('useOpenFile 必须在 FileViewerProvider 内使用');
  return open;
}

export function FileViewerProvider({ children }: { children: ReactNode }) {
  const sessionId = useAppState().current?.id ?? null;
  const [target, setTarget] = useState<FileViewerTarget | null>(null);

  const open = useCallback<OpenFile>((path, line) => {
    setTarget({ path, line: line ?? null });
  }, []);

  return (
    <FileViewerContext.Provider value={open}>
      {children}
      {target && (
        <FileViewerModal sessionId={sessionId} target={target} onClose={() => setTarget(null)} />
      )}
    </FileViewerContext.Provider>
  );
}

function FileViewerModal({
  sessionId,
  target,
  onClose,
}: {
  sessionId: string | null;
  target: FileViewerTarget;
  onClose: () => void;
}) {
  const [file, setFile] = useState<FileContent | null>(null);
  const [error, setError] = useState<string | null>(null);
  const bodyRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    setFile(null);
    setError(null);
    if (!sessionId) {
      setError('没有活动会话，无法定位仓库。');
      return;
    }
    let cancelled = false;
    readFile(sessionId, target.path)
      .then((f) => {
        if (!cancelled) setFile(f);
      })
      .catch((err: unknown) => {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, target.path]);

  // 目标行高亮并滚动到位
  useEffect(() => {
    if (!file || target.line === null) return;
    const el = bodyRef.current?.querySelector(`[data-line="${target.line}"]`);
    el?.scrollIntoView({ block: 'center' });
  }, [file, target.line]);

  // Esc 关闭
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  const lines = file?.content.split('\n') ?? [];

  return (
    <div className="fv-backdrop" onClick={onClose}>
      <div className="fv" onClick={(e) => e.stopPropagation()}>
        <div className="fv-head">
          <span className="fv-path">{target.path}</span>
          {file?.truncated && <span className="fv-flag">已截断</span>}
          <button className="fv-x" onClick={onClose} title="关闭 (Esc)">
            ✕
          </button>
        </div>
        <div className="fv-body" ref={bodyRef}>
          {error && <div className="insp-empty">{error}</div>}
          {!error && !file && <div className="insp-empty">加载中…</div>}
          {file &&
            lines.map((text, i) => (
              <div
                key={i}
                data-line={i + 1}
                className={`fv-line${target.line === i + 1 ? ' target' : ''}`}
              >
                <span className="ln">{i + 1}</span>
                <span className="lc">{text || ' '}</span>
              </div>
            ))}
        </div>
      </div>
    </div>
  );
}
