// Last open conversation. Refresh should reopen it, like a conversation page,
// not dump the user onto a new-task hero.

const STORAGE_KEY = 'leveler.web.lastSession';

export function loadLastSession(): string | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    const id = value?.trim() ?? '';
    return id.length > 0 ? id : null;
  } catch {
    return null;
  }
}

export function saveLastSession(id: string | null): void {
  try {
    if (!id) localStorage.removeItem(STORAGE_KEY);
    else localStorage.setItem(STORAGE_KEY, id);
  } catch {
    // private mode / missing localStorage
  }
}
