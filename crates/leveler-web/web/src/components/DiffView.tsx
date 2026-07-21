// 独立 Diff 视图：中间工作区「对话 / 改动」切换后的改动面板。
// 数据来自 snapshot.diff + diff_updated；打开时主动 request_diff 刷新。
// patch 按行着色：+ 增 / − 删 / @@ 段落标 / 其余上下文。

import { useEffect } from 'react';
import { useAppState } from '../state/store';
import { useBridge } from '../state/bridge';

export function DiffView() {
  const current = useAppState().current;
  const bridge = useBridge();

  // 打开视图时拉一次最新 diff（runtime 不总是主动推）
  useEffect(() => {
    bridge.requestDiff();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

  const diff = current?.diff ?? null;
  const files = diff?.files ?? [];
  const totalAdd = files.reduce((n, f) => n + f.added, 0);
  const totalDel = files.reduce((n, f) => n + f.removed, 0);

  if (!current) {
    return (
      <div className="diffview">
        <div className="insp-empty">加载会话中…</div>
      </div>
    );
  }

  return (
    <div className="diffview">
      <div className="dv-inner">
        <div className="dv-head">
          <span className="changes-sum">
            <span className="n">{files.length} 个文件</span>
            <span className="add">+{totalAdd}</span>
            <span className="del">−{totalDel}</span>
          </span>
          <button className="dv-refresh" onClick={() => bridge.requestDiff()}>
            刷新
          </button>
        </div>
        {files.length === 0 && <div className="insp-empty">工作区干净，暂无变更。</div>}
        {files.map((f) => (
          <section className="dv-file" key={f.path}>
            <header className="dv-file-head">
              <span className="p">{f.path}</span>
              <span className="add">+{f.added}</span>
              <span className="del">−{f.removed}</span>
            </header>
            {f.patch ? (
              <pre className="dv-patch">
                {f.patch.split('\n').map((line, i) => {
                  const cls = line.startsWith('+')
                    ? 'add'
                    : line.startsWith('-')
                      ? 'del'
                      : line.startsWith('@@')
                        ? 'hunk'
                        : '';
                  return (
                    <div key={i} className={`dv-line ${cls}`}>
                      {line || ' '}
                    </div>
                  );
                })}
              </pre>
            ) : (
              <div className="insp-empty">无 patch（内容过大或未跟踪）。</div>
            )}
          </section>
        ))}
      </div>
    </div>
  );
}
