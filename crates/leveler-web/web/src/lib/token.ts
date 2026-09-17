// 访问令牌：启动 URL 携带 `?token=`，读取后转存 sessionStorage 并清掉地址栏参数。
// 令牌只在内存/会话存储中存活，不落盘。

const STORAGE_KEY = 'leveler.web.token';

let cached: string | null = null;

/** 取当前令牌；首次调用时从地址栏收割。 */
export function getToken(): string {
  if (cached !== null) return cached;
  const url = new URL(window.location.href);
  const fromUrl = url.searchParams.get('token');
  if (fromUrl) {
    sessionStorage.setItem(STORAGE_KEY, fromUrl);
    url.searchParams.delete('token');
    // 清掉地址栏里的令牌，避免随截图/分享外泄
    window.history.replaceState(null, '', url.pathname + url.search + url.hash);
    cached = fromUrl;
    return cached;
  }
  cached = sessionStorage.getItem(STORAGE_KEY) ?? '';
  return cached;
}

/** 清空令牌（例如服务端 401 后允许用户重新粘贴带 token 的 URL）。 */
export function clearToken(): void {
  cached = null;
  sessionStorage.removeItem(STORAGE_KEY);
}
