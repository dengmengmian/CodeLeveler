import type { RailNav } from '../state/store';

/** Sidebar primary destinations. Changes / Execution live in the workspace tabs. */
export const SIDEBAR_NAV: ReadonlyArray<{ id: Exclude<RailNav, 'settings'>; label: string }> = [
  { id: 'sessions', label: 'Conversations' },
  { id: 'files', label: 'Files' },
  { id: 'search', label: 'Search' },
];

/** @deprecated Use SIDEBAR_NAV. Kept as an alias for existing imports during the shell migration. */
export const RAIL_NAV = SIDEBAR_NAV;
