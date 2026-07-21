// 审批卡：钉在时间线流中。快捷键 y/s/a/n 在 App 全局键盘里处理。

import type { UiApprovalRequest } from '../types/protocol';
import { useBridge } from '../state/bridge';

export function ApprovalCard({ request }: { request: UiApprovalRequest }) {
  const bridge = useBridge();
  const shortId = request.id.length > 10 ? `${request.id.slice(0, 10)}…` : request.id;

  return (
    <div className="approval">
      <div className="a-head">
        <span>◍</span> APPROVAL REQUIRED
        <span style={{ marginLeft: 'auto', color: 'var(--text-tertiary)' }}>{shortId}</span>
      </div>
      <div className="a-body">
        {request.summary}
        {request.command && <pre>$ {request.command}</pre>}
        {request.risks.length > 0 && (
          <ul className="a-risks">
            {request.risks.map((r) => (
              <li key={r}>{r}</li>
            ))}
          </ul>
        )}
      </div>
      <div className="a-actions">
        <button className="abtn primary" onClick={() => bridge.decideApproval(request.id, 'approve_once')}>
          批准一次<kbd>y</kbd>
        </button>
        <button className="abtn" onClick={() => bridge.decideApproval(request.id, 'approve_session')}>
          本会话内批准<kbd>s</kbd>
        </button>
        <button className="abtn" onClick={() => bridge.decideApproval(request.id, 'approve_always')}>
          始终允许<kbd>a</kbd>
        </button>
        <button className="abtn danger" onClick={() => bridge.decideApproval(request.id, 'deny')}>
          拒绝<kbd>n</kbd>
        </button>
      </div>
    </div>
  );
}
