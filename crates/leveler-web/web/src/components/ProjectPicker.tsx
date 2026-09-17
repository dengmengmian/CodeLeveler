// 「打开项目」目录选择器弹层：浏览服务端文件系统逐级点选一个仓库目录。
// 浏览器拿不到真实绝对路径，所以走 /api/fs/list 让服务端逐目录返回子目录，
// 用户点进去、选中，再交给 bridge.addProject 注册。

import { useCallback, useEffect, useState } from 'react';
import { useBridge } from '../state/bridge';
import { listDir, type FsListing } from '../lib/api';

export function ProjectPicker({ onClose }: { onClose: () => void }) {
  const bridge = useBridge();
  const [dir, setDir] = useState<FsListing | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showHidden, setShowHidden] = useState(false);

  const load = useCallback((path?: string) => {
    setError(null);
    listDir(path)
      .then(setDir)
      .catch((err: unknown) => setError(err instanceof Error ? err.message : String(err)));
  }, []);

  // 初次打开：从 $HOME 起。
  useEffect(() => {
    load();
  }, [load]);

  // ESC 关闭。
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose]);

  const open = async (path: string) => {
    if (busy) return;
    setBusy(true);
    const ok = await bridge.addProject(path);
    setBusy(false);
    if (ok) onClose();
  };

  const entries = (dir?.entries ?? []).filter((e) => showHidden || !e.hidden);

  return (
    <div className="pp-backdrop" onClick={onClose}>
      <div className="pp" onClick={(e) => e.stopPropagation()}>
        <div className="pp-head">
          <span className="pp-title">打开项目</span>
          <button className="fv-x" title="关闭（Esc）" onClick={onClose}>
            ✕
          </button>
        </div>

        <div className="pp-bar">
          <button
            className="pp-up"
            title="上级目录"
            disabled={!dir?.parent}
            onClick={() => dir?.parent && load(dir.parent)}
          >
            ↑
          </button>
          <span className="pp-path" title={dir?.path}>
            {dir?.path ?? '加载中…'}
          </span>
        </div>

        {error && <div className="pp-empty">{error}</div>}
        {!error && !dir && <div className="pp-empty">加载中…</div>}
        {dir && entries.length === 0 && <div className="pp-empty">此目录下没有子文件夹。</div>}

        <div className="pp-list">
          {entries.map((e) => (
            <div className="pp-row" key={e.path}>
              <button className="pp-name" title={e.path} onClick={() => load(e.path)}>
                <span className="pp-ico">{e.is_repo ? '◆' : '▸'}</span>
                <span className="pp-label">{e.name}</span>
                {e.is_repo && <span className="pp-git">git</span>}
              </button>
              <button className="pp-open" disabled={busy} onClick={() => open(e.path)}>
                打开
              </button>
            </div>
          ))}
        </div>

        <div className="pp-foot">
          <label className="pp-hidden">
            <input
              type="checkbox"
              checked={showHidden}
              onChange={(e) => setShowHidden(e.target.checked)}
            />
            显示隐藏目录
          </label>
          <button
            className="pp-confirm"
            disabled={!dir || busy}
            onClick={() => dir && open(dir.path)}
          >
            {busy ? '打开中…' : '打开当前目录'}
          </button>
        </div>
      </div>
    </div>
  );
}
