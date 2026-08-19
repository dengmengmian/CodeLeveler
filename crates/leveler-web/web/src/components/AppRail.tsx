// 48px first-level application rail. No lists, no project content.

import { useAppDispatch, useAppState, type RailNav } from '../state/store';

const ITEMS: ReadonlyArray<{ id: RailNav; label: string; glyph: string }> = [
  { id: 'sessions', label: 'Sessions', glyph: '◉' },
  { id: 'workspace', label: 'Workspace', glyph: '▣' },
  { id: 'search', label: 'Search', glyph: '⌕' },
  { id: 'changes', label: 'Changes', glyph: '⑂' },
  { id: 'activity', label: 'Activity', glyph: '◎' },
  { id: 'settings', label: 'Settings', glyph: '⚙' },
];

export function AppRail() {
  const { railNav, connection } = useAppState();
  const dispatch = useAppDispatch();

  return (
    <nav className="app-rail" aria-label="Application">
      <div className="app-rail-brand" title={`CodeLeveler web · v${__APP_VERSION__}`}>
        CL
      </div>
      <div className="app-rail-items">
        {ITEMS.map((item) => (
          <button
            key={item.id}
            type="button"
            className={`app-rail-btn${railNav === item.id ? ' on' : ''}`}
            title={item.label}
            aria-label={item.label}
            aria-current={railNav === item.id ? 'page' : undefined}
            onClick={() => dispatch({ type: 'set_rail_nav', nav: item.id })}
          >
            <span aria-hidden="true">{item.glyph}</span>
          </button>
        ))}
      </div>
      <div
        className={`app-rail-led${connection === 'online' ? '' : ' off'}`}
        title={connection === 'online' ? 'Daemon 已连接' : 'Daemon 重连中'}
      />
    </nav>
  );
}
