// WebSocket 客户端：/ws?session=<id>&token=<token>，JSON 文本帧。
// 断线指数退避重连（200ms 起步、封顶 5s）；重连成功后自动发 snapshot 帧整量重同步；
// 收到 resync_required 立即重连；未知帧/未知事件类型忽略不崩。

import type { ClientCommand, CommandId, DownFrame, SessionId, UpFrame } from '../types/protocol';

export type WsStatus = 'online' | 'connecting';

export interface WsCallbacks {
  /** 下行帧（event/snapshot/ack/error）；resync_required 由客户端内部消化。 */
  onFrame: (frame: DownFrame) => void;
  onStatus: (status: WsStatus) => void;
}

const BACKOFF_MIN_MS = 200;
const BACKOFF_MAX_MS = 5000;

export class WsClient {
  private ws: WebSocket | null = null;
  private sessionId: SessionId | null = null;
  private readonly token: string;
  private readonly callbacks: WsCallbacks;
  private attempts = 0;
  private disposed = false;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  /** 重连期间积攒的帧，恢复后按序补发。 */
  private outbox: UpFrame[] = [];

  constructor(token: string, callbacks: WsCallbacks) {
    this.token = token;
    this.callbacks = callbacks;
  }

  /** 切换目标会话；变化时立即重连（服务端会先推一帧该会话 snapshot）。 */
  setSession(sessionId: SessionId | null): void {
    if (sessionId === this.sessionId && this.ws) return;
    this.sessionId = sessionId;
    this.reconnectNow();
  }

  currentSession(): SessionId | null {
    return this.sessionId;
  }

  /** 连接（或保持）到当前会话。允许在 dispose 后重新启动（dev StrictMode 双跑）。 */
  connect(): void {
    this.disposed = false;
    if (this.ws) return;
    this.open();
  }

  /** 发送上行帧；未连通时入队，恢复后补发。返回是否立即发出。 */
  send(frame: UpFrame): boolean {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(frame));
      return true;
    }
    this.outbox.push(frame);
    return false;
  }

  dispose(): void {
    this.disposed = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.ws?.close();
    this.ws = null;
  }

  private reconnectNow(): void {
    if (this.disposed) return;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.ws) {
      this.ws.onclose = null;
      this.ws.close();
      this.ws = null;
    }
    this.open();
  }

  private open(): void {
    const params = new URLSearchParams({ token: this.token });
    if (this.sessionId) params.set('session', this.sessionId);
    const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws';
    const url = `${scheme}://${window.location.host}/ws?${params.toString()}`;
    this.callbacks.onStatus('connecting');

    const ws = new WebSocket(url);
    this.ws = ws;

    ws.onopen = () => {
      if (this.ws !== ws) return;
      this.attempts = 0;
      this.callbacks.onStatus('online');
      // 重连后整量重同步：服务端虽会因 session 参数主动推一帧，这里再显式
      // 要一次，两侧语义对齐、幂等。
      if (this.sessionId) {
        ws.send(JSON.stringify({ type: 'snapshot', session_id: this.sessionId } satisfies UpFrame));
      }
      const pending = this.outbox;
      this.outbox = [];
      for (const frame of pending) ws.send(JSON.stringify(frame));
    };

    ws.onmessage = (msg) => {
      if (this.ws !== ws) return;
      let frame: DownFrame;
      try {
        frame = JSON.parse(String(msg.data)) as DownFrame;
      } catch {
        return; // 非 JSON 帧：忽略
      }
      if (frame.type === 'resync_required') {
        // 服务端在 broadcast Lagged 后下发此帧并关闭连接 —— 立即重连重同步。
        this.reconnectNow();
        return;
      }
      this.callbacks.onFrame(frame);
    };

    ws.onclose = () => {
      if (this.ws !== ws || this.disposed) return;
      this.ws = null;
      this.callbacks.onStatus('connecting');
      const delay = Math.min(BACKOFF_MIN_MS * 2 ** this.attempts, BACKOFF_MAX_MS);
      this.attempts += 1;
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        if (!this.disposed) this.open();
      }, delay);
    };

    ws.onerror = () => {
      // 交给 onclose 统一走退避重连
      ws.close();
    };
  }
}

/** 生成一条 deliver 帧。command_id 默认随机 UUID；审批类调用方传固定 id 以幂等重试。 */
export function deliverFrame(
  sessionId: SessionId,
  command: ClientCommand,
  commandId?: CommandId,
): UpFrame {
  return {
    type: 'deliver',
    command_id: commandId ?? crypto.randomUUID(),
    session_id: sessionId,
    command,
  };
}
