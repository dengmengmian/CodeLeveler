//! 协议入口。
//!
//! client-protocol 部分（ClientCommand / RuntimeEvent / UiSessionSnapshot 及其
//! 全部引用类型）**自动生成**自 Rust schema，见 `protocol.gen.ts`（事实源链：
//! Rust types → schemas/*.schema.json → npm run gen:protocol）。手写区只保留
//! web 网关自己的契约：WS 帧、多项目聚合层、REST DTO —— 它们的事实源是
//! crates/leveler-web，不在 client-protocol schema 里。

import type { ClientCommand, ModelRef, PermissionProfile, RuntimeEvent, SessionId, UiSessionSnapshot } from './protocol.gen';

export * from './protocol.gen';

/** 网关 deliver 帧的命令回执 id（web 网关概念，不在 client-protocol 里）。 */
export type CommandId = string;

/** turn 终态事件集合（驱动消息队列出队与终态渲染）。 */
export const TURN_TERMINAL_TYPES: ReadonlySet<RuntimeEvent['type']> = new Set([
  'turn_completed',
  'turn_answered',
  'turn_truncated',
  'turn_incomplete',
  'turn_completed_unverified',
  'turn_failed',
  'turn_cancelled',
] satisfies ReadonlyArray<RuntimeEvent['type']>);

// ── WS 帧（与 leveler-web 网关的契约，见 crates/leveler-web/src/ws.rs）──
/** 上行：浏览器 → 服务端。 */
export type UpFrame =
  | { type: 'deliver'; command_id: CommandId; session_id: SessionId; command: ClientCommand }
  | { type: 'snapshot'; session_id: SessionId };

/** 下行：服务端 → 浏览器。 */
export type DownFrame =
  | { type: 'event'; event: RuntimeEvent }
  | { type: 'snapshot'; session: UiSessionSnapshot }
  | { type: 'ack'; command_id: CommandId }
  | { type: 'error'; code: string; message: string; command_id: CommandId | null }
  | { type: 'project_status'; path: string; status: ProjectStatus }
  | { type: 'resync_required'; session_id: SessionId };

// ── 多项目（leveler-web 聚合层 REST）─────────────────────────────────
export type ProjectStatus = 'online' | 'starting' | 'offline';

export interface ProjectInfo {
  path: string;
  name: string;
  status: ProjectStatus;
  sessions: number;
}

// ── leveler-local-transport：REST DTO ───────────────────────────────
/** CreateSessionRequest（leveler-local-transport/src/lib.rs；`project` 是 WebUI 聚合层扩展） */
export interface CreateSessionRequest {
  goal: string;
  model: ModelRef | null;
  mode: PermissionProfile;
  project?: string;
}

/** SessionBootstrap = POST /api/sessions 的响应。 */
export interface SessionBootstrap {
  session: UiSessionSnapshot;
  context_window: number;
}
