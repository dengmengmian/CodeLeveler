// Generic line icons drawn for CodeLeveler from the reference's visual grammar.
// Stroke weight, optical size and joins follow the reference measurements; the
// path data is authored here rather than imported from the reference project.

const NS = 'http://www.w3.org/2000/svg';

/** @type {Record<string, string[]>} */
const paths = {
  panel: ['M4 5.5h16v13H4z', 'M9.5 5.5v13'],
  plus: ['M12 5v14', 'M5 12h14'],
  minus: ['M5 12h14'],
  more: ['M5 12h.01', 'M12 12h.01', 'M19 12h.01'],
  search: ['M10.5 4a6.5 6.5 0 1 0 0 13a6.5 6.5 0 0 0 0-13', 'M16 16l4 4'],
  sliders: ['M4 7h16', 'M4 17h16', 'M8.5 4.5v5', 'M15.5 14.5v5'],
  folder: ['M3.5 7.5h6.5l1.7-2.5h8.8v13.5h-17z'],
  folderPlus: ['M3.5 7.5h6.5l1.7-2.5h8.8v13.5h-17z', 'M12 11v5', 'M9.5 13.5h5'],
  folderOpen: ['M3.5 7.5h6.5l1.7-2.5h8.8v4H3.5z', 'M3.5 7.5v11h14.8l2.2-8H5.7z'],
  chevronDown: ['M6 9.5l6 6 6-6'],
  chevronRight: ['M9.5 6l6 6-6 6'],
  chevronUp: ['M6 14.5l6-6 6 6'],
  chevronLeft: ['M14.5 6l-6 6 6 6'],
  arrowUp: ['M12 19V5', 'M5.5 11.5L12 5l6.5 6.5'],
  arrowDown: ['M12 5v14', 'M5.5 12.5L12 19l6.5-6.5'],
  arrowLeft: ['M19 12H5', 'M11.5 5.5L5 12l6.5 6.5'],
  arrowRight: ['M5 12h14', 'M12.5 5.5L19 12l-6.5 6.5'],
  refresh: ['M20 8a8 8 0 1 0 1 6', 'M20 3v5h-5'],
  external: ['M14 4h6v6', 'M20 4l-9 9', 'M10 4H4v16h16v-6'],
  close: ['M6 6l12 12', 'M18 6L6 18'],
  copy: ['M9 8h11v13H9z', 'M5 16H3V3h11v2'],
  thumbUp: ['M7 11l4-7a1.6 1.6 0 0 1 3 0v4h4.2a2 2 0 0 1 2 2.4l-1.2 6A2 2 0 0 1 17 18H7z', 'M3 11h4v7H3z'],
  thumbDown: ['M7 13l4 7a1.6 1.6 0 0 0 3 0v-4h4.2a2 2 0 0 0 2-2.4l-1.2-6A2 2 0 0 0 17 6H7z', 'M3 6h4v7H3z'],
  check: ['M5 12.5l4.2 4.2L19 7'],
  checkCircle: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M8 12l2.8 2.8L16.5 9'],
  xCircle: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M9 9l6 6', 'M15 9l-6 6'],
  alert: ['M12 3.5l9 16.5H3z', 'M12 9v5', 'M12 17.2v.01'],
  info: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M12 10.5v6', 'M12 7.5v.01'],
  settings: ['M12 15.2a3.2 3.2 0 1 0 0-6.4a3.2 3.2 0 0 0 0 6.4', 'M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1.03 1.56V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 9 19.35a1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.64 15a1.7 1.7 0 0 0-1.56-1.03H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.65 9a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.64h.08A1.7 1.7 0 0 0 10.1 3.08V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1.03 1.56 1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.7 1.7 0 0 0-.34 1.87V9c.2.62.72 1.03 1.38 1.03H21a2 2 0 1 1 0 4h-.09A1.7 1.7 0 0 0 19.4 15z'],
  plugins: ['M9 4.5h3a1.5 1.5 0 1 1 3 0v3h3.5v4a1.5 1.5 0 1 1 0 3v4H4.5v-4a1.5 1.5 0 1 1 0-3v-4H9z'],
  automation: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M10 8.5l5 3.5-5 3.5z'],
  compass: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M15.8 8.2l-2.2 5.4-5.4 2.2 2.2-5.4z'],
  file: ['M6 3.5h7.5L18 8v12.5H6z', 'M13.5 3.5V8H18'],
  fileText: ['M6 3.5h7.5L18 8v12.5H6z', 'M13.5 3.5V8H18', 'M9 12h6', 'M9 15.5h6'],
  fileCode: ['M6 3.5h7.5L18 8v12.5H6z', 'M13.5 3.5V8H18', 'M10 12l-2 2.5 2 2.5', 'M14 12l2 2.5-2 2.5'],
  fileImage: ['M6 3.5h7.5L18 8v12.5H6z', 'M13.5 3.5V8H18', 'M9 17l2.5-3 2 2 1.5-1.8 2 2.8', 'M10 11.5h.01'],
  terminal: ['M4 5.5h16v13H4z', 'M7.5 9.5l3 2.5-3 2.5', 'M12.5 15h4'],
  globe: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M3.5 12h17', 'M12 3.5c2.8 3 2.8 14 0 17', 'M12 3.5c-2.8 3-2.8 14 0 17'],
  columns: ['M4 5.5h16v13H4z', 'M12 5.5v13'],
  expand: ['M9 4H4v5', 'M15 4h5v5', 'M20 15v5h-5', 'M4 15v5h5'],
  restore: ['M4 9h5V4', 'M20 9h-5V4', 'M20 15h-5v5', 'M4 15h5v5'],
  minimize: ['M6 4.5v13', 'M18 4.5v13', 'M10 12h4'],
  send: ['M12 19V5', 'M5.5 11.5L12 5l6.5 6.5'],
  stop: ['M7.5 7.5h9v9h-9z'],
  paperclip: ['M20 11.5l-7.8 7.8a5 5 0 0 1-7.1-7.1l8-8a3.4 3.4 0 0 1 4.8 4.8l-8 8a1.8 1.8 0 0 1-2.5-2.5l7.3-7.3'],
  image: ['M4 5.5h16v13H4z', 'M4 15l4.5-4.5 3.5 3.5 2.5-2.5L20 16', 'M9 9.5h.01'],
  shield: ['M12 3l8 3v6c0 5-8 9-8 9s-8-4-8-9V6z'],
  shieldCheck: ['M12 3l8 3v6c0 5-8 9-8 9s-8-4-8-9V6z', 'M8.5 12l2.5 2.5 4.5-5'],
  lock: ['M6.5 10.5h11v9h-11z', 'M8.5 10.5V8a3.5 3.5 0 0 1 7 0v2.5'],
  edit: ['M5 16.5l-1 3.5 3.5-1L20 6.5 17.5 4z', 'M14.5 7l2.5 2.5'],
  trash: ['M4 6.5h16', 'M6.5 6.5V20h11V6.5', 'M9.5 6.5v-2h5v2', 'M10 10.5v6', 'M14 10.5v6'],
  pin: ['M9 4h6l-1 5 3 3v2H7v-2l3-3z', 'M12 14v6'],
  archive: ['M3.5 5h17v4h-17z', 'M5.5 9v11h13V9', 'M10 12.5h4'],
  fork: ['M7 4v6a3 3 0 0 0 3 3h4a3 3 0 0 1 3 3v4', 'M17 4v6a3 3 0 0 1-3 3h-4a3 3 0 0 0-3 3v4', 'M5.5 3h3v3h-3z', 'M15.5 3h3v3h-3z', 'M15.5 18h3v3h-3z', 'M5.5 18h3v3h-3z'],
  chat: ['M4 5.5h16v11H9l-5 4z'],
  message: ['M4 5.5h16v11H9l-5 4z', 'M8 9h8', 'M8 12.5h5'],
  bot: ['M7 8.5h10v8H7z', 'M12 4.5v4', 'M9.5 12h.01', 'M14.5 12h.01', 'M9 16.5v3', 'M15 16.5v3'],
  brain: ['M9.2 4.5A2.7 2.7 0 0 0 6.5 7.2v.3A2.7 2.7 0 0 0 5.5 13a2.7 2.7 0 0 0 2.7 4.1A2.7 2.7 0 0 0 11 19.5V4.5z', 'M14.8 4.5a2.7 2.7 0 0 1 2.7 2.7v.3a2.7 2.7 0 0 1 1 5.5 2.7 2.7 0 0 1-2.7 4.1 2.7 2.7 0 0 1-2.8 2.4V4.5z'],
  code: ['M8.5 8L4.5 12l4 4', 'M15.5 8l4 4-4 4', 'M13.5 5l-3 14'],
  diff: ['M6 4.5v15', 'M6 7h7', 'M6 17h7', 'M16.5 4.5v7', 'M13 8h7'],
  layers: ['M12 3.5l8.5 4.5-8.5 4.5L3.5 8z', 'M3.5 12l8.5 4.5 8.5-4.5', 'M3.5 16l8.5 4.5 8.5-4.5'],
  pulse: ['M3 12h4l2.5-6 5 12 2.5-6h4'],
  history: ['M3.5 4v5h5', 'M3.5 9a8.5 8.5 0 1 1 1 8', 'M12 7.5v5l3 2'],
  clock: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M12 7v5.5l3.5 2'],
  sun: ['M12 7.5a4.5 4.5 0 1 0 0 9a4.5 4.5 0 0 0 0-9', 'M12 2.5v2', 'M12 19.5v2', 'M4.9 4.9l1.4 1.4', 'M17.7 17.7l1.4 1.4', 'M2.5 12h2', 'M19.5 12h2', 'M4.9 19.1l1.4-1.4', 'M17.7 6.3l1.4-1.4'],
  moon: ['M20 14.5a8 8 0 1 1-10.5-10.4a7 7 0 0 0 10.5 10.4z'],
  monitor: ['M3.5 4.5h17v12h-17z', 'M9 20h6', 'M12 16.5V20'],
  globe2: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M3.5 12h17'],
  puzzle: ['M9 4.5h3a1.5 1.5 0 1 1 3 0v3h3.5v4a1.5 1.5 0 1 1 0 3v4H4.5v-4a1.5 1.5 0 1 1 0-3v-4H9z'],
  sparkle: ['M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8z', 'M18 15l.8 2.2L21 18l-2.2.8L18 21l-.8-2.2L15 18l2.2-.8z'],
  command: ['M9 6a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3v12a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3z'],
  list: ['M8 6h12', 'M8 12h12', 'M8 18h12', 'M4 6h.01', 'M4 12h.01', 'M4 18h.01'],
  grid: ['M4 4h7v7H4z', 'M13 4h7v7h-7z', 'M4 13h7v7H4z', 'M13 13h7v7h-7z'],
  sort: ['M8 4v16', 'M4.5 16.5L8 20l3.5-3.5', 'M16 20V4', 'M12.5 7.5L16 4l3.5 3.5'],
  eye: ['M2.5 12S6 6.5 12 6.5 21.5 12 21.5 12 18 17.5 12 17.5 2.5 12 2.5 12z', 'M12 9.5a2.5 2.5 0 1 0 0 5a2.5 2.5 0 0 0 0-5'],
  wrap: ['M4 6h16', 'M4 12h12.5a2.5 2.5 0 0 1 0 5H12', 'M14 15l-2.5 2.5L14 20', 'M4 18h4'],
  zoomIn: ['M10.5 4a6.5 6.5 0 1 0 0 13a6.5 6.5 0 0 0 0-13', 'M16 16l4 4', 'M10.5 7.5v6', 'M7.5 10.5h6'],
  zoomOut: ['M10.5 4a6.5 6.5 0 1 0 0 13a6.5 6.5 0 0 0 0-13', 'M16 16l4 4', 'M7.5 10.5h6'],
  download: ['M12 3.5v12', 'M7 11l5 5 5-5', 'M4 20.5h16'],
  upload: ['M12 20.5v-12', 'M7 12l5-5 5 5', 'M4 3.5h16'],
  user: ['M12 12a4 4 0 1 0 0-8a4 4 0 0 0 0 8', 'M4.5 20.5a7.5 7.5 0 0 1 15 0'],
  logout: ['M14 4.5H5.5v15H14', 'M10 12h10', 'M17 8.5l3.5 3.5-3.5 3.5'],
  feedback: ['M4 5.5h16v11H9l-5 4z', 'M9 9.5l6 4-6 4z'],
  warning: ['M12 3.5l9 16.5H3z', 'M12 9v5', 'M12 17.2v.01'],
  folderTree: ['M3.5 7.5h6.5l1.7-2.5h8.8v13.5h-17z', 'M9 11h6', 'M9 14h4'],
  branch: ['M6 4v16', 'M6 7h7a5 5 0 0 1 5 5v4', 'M6 4.5h.01', 'M18 13v4', 'M15.5 19a2.5 2.5 0 1 0 5 0a2.5 2.5 0 0 0-5 0'],
  clockArrow: ['M12 3.5a8.5 8.5 0 1 0 7.8 12', 'M12 7.5v5l3 2', 'M19 3.5v5h-5'],
  question: ['M12 3.5a8.5 8.5 0 1 0 0 17a8.5 8.5 0 0 0 0-17', 'M9.5 9a2.5 2.5 0 1 1 3.7 2.3c-.8.4-1.2 1-1.2 1.8v.4', 'M12 17v.01'],
};

/**
 * @param {string} name
 * @param {{size?:number, stroke?:number, className?:string}} [options]
 * @returns {SVGElement}
 */
export function icon(name, {size = 16, stroke = 1.5, className = ''} = {}) {
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('width', String(size));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'none');
  svg.setAttribute('stroke', 'currentColor');
  svg.setAttribute('stroke-width', String(stroke));
  svg.setAttribute('stroke-linecap', 'round');
  svg.setAttribute('stroke-linejoin', 'round');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  if (className) svg.setAttribute('class', className);
  for (const d of paths[name] ?? paths.file) {
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

/** Brand mark: the CL monogram from the CodeLeveler brand asset. */
export function brandMark({ size = 24, className = '' } = {}) {
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', '0 0 64 64');
  svg.setAttribute('width', String(Math.round(size * 64 / 44)));
  svg.setAttribute('height', String(size));
  svg.setAttribute('fill', 'currentColor');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  if (className) svg.setAttribute('class', className);
  for (const d of [
    'M27 9A23 23 0 1 0 27 55L27 47A15 15 0 1 1 27 17Z',
    'M29 22H37V54H29Z M29 46H51V54H29Z M29 38H47V46H29Z M29 30H43V38H29Z',
  ]) {
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('d', d);
    svg.append(path);
  }
  return svg;
}

/** File-type glyph used by the files, changes and deliverables views. */
export function fileIcon(name) {
  const lower = String(name || '').toLowerCase();
  const kind = /\.(png|jpe?g|gif|webp|svg|bmp|ico)$/.test(lower)
    ? 'fileImage'
    : /\.(ts|tsx|js|jsx|mjs|cjs|rs|py|go|java|c|h|cpp|cs|rb|php|swift|kt|sh|zsh|bash)$/.test(lower)
      ? 'fileCode'
      : 'fileText';
  const color = /\.(png|jpe?g|gif|webp|svg|bmp|ico)$/.test(lower)
    ? 'var(--file-icon-image)'
    : /\.(ts|tsx|js|jsx|mjs|cjs)$/.test(lower)
      ? 'var(--file-icon-code)'
      : /\.(json|yaml|yml|toml)$/.test(lower)
        ? 'var(--file-icon-config)'
        : 'var(--file-icon-text)';
  const node = icon(kind, { size: 16 });
  node.style.color = color;
  return node;
}
