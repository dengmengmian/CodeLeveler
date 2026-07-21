// REST 客户端：GET /api/health、POST /api/sessions、GET /api/sessions/:id/snapshot。
// 全部带 Authorization: Bearer（网关也接受 ?token=，但头部更干净）。

import type {
  CreateSessionRequest,
  PermissionProfile,
  ModelRef,
  ProjectInfo,
  SessionBootstrap,
  SessionId,
  UiSessionSnapshot,
} from '../types/protocol';
import { getToken } from './token';

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: {
      Authorization: `Bearer ${getToken()}`,
      ...(init?.body ? { 'Content-Type': 'application/json' } : {}),
      ...init?.headers,
    },
  });
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new ApiError(res.status, body || res.statusText);
  }
  return (await res.json()) as T;
}

export function health(): Promise<{ ok: boolean }> {
  return request<{ ok: boolean }>('/api/health');
}

export function createSession(
  goal: string,
  model: ModelRef | null,
  mode: PermissionProfile,
  project?: string,
): Promise<SessionBootstrap> {
  const body: CreateSessionRequest = { goal, model, mode, ...(project ? { project } : {}) };
  return request<SessionBootstrap>('/api/sessions', {
    method: 'POST',
    body: JSON.stringify(body),
  });
}

// ── 多项目管理（聚合层）──────────────────────────────────────────────

export function listProjects(): Promise<{ projects: ProjectInfo[] }> {
  return request<{ projects: ProjectInfo[] }>('/api/projects');
}

export function addProject(path: string): Promise<{ project: ProjectInfo }> {
  return request<{ project: ProjectInfo }>('/api/projects', {
    method: 'POST',
    body: JSON.stringify({ path }),
  });
}

export interface FsEntry {
  name: string;
  path: string;
  is_repo: boolean;
  hidden: boolean;
}

export interface FsListing {
  path: string;
  parent: string | null;
  entries: FsEntry[];
}

/** 浏览服务端文件系统的某个目录（缺省从 $HOME 起）——「打开项目」选择器用。 */
export function listDir(path?: string): Promise<FsListing> {
  const query = path ? `?path=${encodeURIComponent(path)}` : '';
  return request<FsListing>(`/api/fs/list${query}`);
}

export function removeProject(path: string): Promise<void> {
  return request<void>(`/api/projects?path=${encodeURIComponent(path)}`, {
    method: 'DELETE',
  });
}

export function restartProject(path: string): Promise<void> {
  return request<void>('/api/projects/restart', {
    method: 'POST',
    body: JSON.stringify({ path }),
  });
}

export function fetchSnapshot(sessionId: SessionId): Promise<UiSessionSnapshot> {
  return request<UiSessionSnapshot>(
    `/api/sessions/${encodeURIComponent(sessionId)}/snapshot`,
  );
}

// ── 工作台数据接口（文件 / 搜索 / Git / 附件上传）────────────────────

export interface FileContent {
  path: string;
  content: string;
  truncated: boolean;
  total_lines: number;
}

export function readFile(sessionId: SessionId, path: string): Promise<FileContent> {
  return request<FileContent>(
    `/api/sessions/${encodeURIComponent(sessionId)}/file?path=${encodeURIComponent(path)}`,
  );
}

export function listFiles(
  sessionId: SessionId,
  prefix = '',
  limit = 2000,
): Promise<{ files: string[] }> {
  const params = new URLSearchParams({ prefix, limit: String(limit) });
  return request<{ files: string[] }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/files?${params}`,
  );
}

export interface SearchMatch {
  path: string;
  line: number;
  text: string;
}

export function searchFiles(
  sessionId: SessionId,
  q: string,
  limit = 100,
): Promise<{ matches: SearchMatch[] }> {
  const params = new URLSearchParams({ q, limit: String(limit) });
  return request<{ matches: SearchMatch[] }>(
    `/api/sessions/${encodeURIComponent(sessionId)}/search?${params}`,
  );
}

export interface GitStatusFile {
  path: string;
  status: 'modified' | 'added' | 'deleted' | 'renamed' | 'untracked';
  added: number;
  removed: number;
}

export interface GitStatus {
  branch: string | null;
  files: GitStatusFile[];
}

export function gitStatus(sessionId: SessionId): Promise<GitStatus> {
  return request<GitStatus>(`/api/sessions/${encodeURIComponent(sessionId)}/git-status`);
}

/** multipart 上传附件；AttachmentRef 随后经 WS attachment_added 事件到达 */
export async function uploadAttachment(sessionId: SessionId, file: File): Promise<void> {
  const form = new FormData();
  form.append('file', file, file.name);
  const res = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/attachments`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${getToken()}` },
    body: form,
  });
  if (!res.ok) {
    const body = await res.text().catch(() => '');
    throw new ApiError(res.status, body || res.statusText);
  }
}
