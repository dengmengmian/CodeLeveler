// Small DOM and overlay primitives shared by the desktop views. Every overlay
// is a real element in the document (never native title/popup chrome), which
// keeps positioning and theming identical across platforms.

import { icon } from './icons.mjs';

/** @param {string} tag @param {string} [className] @param {string} [text] */
export function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/** @param {string} name @param {object} [options] */
export function iconNode(name, options) { return icon(name, options); }

/** @param {HTMLElement} parent @param {Node[]} children */
export function append(parent, ...children) {
  for (const child of children) if (child) parent.append(child);
  return parent;
}

/** A button with an icon and an accessible name. */
export function iconButton(name, label, { size = 16, className = 'icon-button', tooltip, keys, onClick } = {}) {
  const button = el('button', className);
  button.type = 'button';
  button.setAttribute('aria-label', label);
  if (tooltip) button.dataset.tooltip = tooltip;
  if (keys) button.dataset.keys = keys;
  button.append(icon(name, { size }));
  if (onClick) button.addEventListener('click', onClick);
  return button;
}

/* ------------------------------------------------------------------ tooltip */

let tooltipNode = null;
let tooltipAnchor = null;
let tooltipTimer = null;

function ensureTooltip() {
  if (tooltipNode) return tooltipNode;
  tooltipNode = el('div', 'ui-tooltip');
  tooltipNode.setAttribute('role', 'tooltip');
  tooltipNode.hidden = true;
  const label = el('span', 'ui-tooltip-label');
  const keys = el('span', 'ui-tooltip-keys');
  tooltipNode.append(label, keys);
  document.body.append(tooltipNode);
  return tooltipNode;
}

export function showTooltip(anchor) {
  if (!anchor?.isConnected || !anchor.dataset.tooltip) return;
  const node = ensureTooltip();
  node.querySelector('.ui-tooltip-label').textContent = anchor.dataset.tooltip;
  const parts = (anchor.dataset.keys ?? '').split(' ').filter(Boolean);
  node.querySelector('.ui-tooltip-keys').replaceChildren(...parts.map(key => el('kbd', undefined, key)));
  node.hidden = false;
  const rect = anchor.getBoundingClientRect();
  const box = node.getBoundingClientRect();
  let side = anchor.dataset.tooltipSide || (anchor.closest('.sidebar') ? 'right' : 'bottom');
  let left = 0;
  let top = 0;
  if (side === 'right') {
    left = rect.right + 10;
    top = rect.top + rect.height / 2 - box.height / 2;
    if (left + box.width > innerWidth - 8) { left = rect.left - box.width - 10; side = 'left'; }
  } else if (side === 'top') {
    left = rect.left + rect.width / 2 - box.width / 2;
    top = rect.top - box.height - 8;
  } else {
    left = rect.left + rect.width / 2 - box.width / 2;
    top = rect.bottom + 8;
    if (top + box.height > innerHeight - 8) { top = rect.top - box.height - 8; side = 'top'; }
  }
  left = Math.max(8, Math.min(innerWidth - box.width - 8, left));
  top = Math.max(8, Math.min(innerHeight - box.height - 8, top));
  node.dataset.side = side;
  node.style.left = `${Math.round(left)}px`;
  node.style.top = `${Math.round(top)}px`;
  if (tooltipAnchor !== anchor) {
    tooltipAnchor?.removeAttribute('aria-describedby');
    tooltipAnchor = anchor;
    anchor.setAttribute('aria-describedby', 'ui-tooltip-live');
  }
  node.id = 'ui-tooltip-live';
}

export function hideTooltip() {
  if (tooltipTimer) { clearTimeout(tooltipTimer); tooltipTimer = null; }
  if (tooltipNode) tooltipNode.hidden = true;
  tooltipAnchor?.removeAttribute('aria-describedby');
  tooltipAnchor = null;
}

function scheduleTooltip(anchor, delay = 450) {
  if (tooltipAnchor === anchor && tooltipNode && !tooltipNode.hidden) return;
  clearTimeout(tooltipTimer);
  tooltipTimer = setTimeout(() => showTooltip(anchor), delay);
}

export function installTooltips() {
  document.addEventListener('pointerover', event => {
    const anchor = event.target.closest?.('[data-tooltip]');
    if (!anchor || anchor.contains(event.relatedTarget)) return;
    scheduleTooltip(anchor);
  });
  document.addEventListener('pointerout', event => {
    const anchor = event.target.closest?.('[data-tooltip]');
    if (!anchor || anchor.contains(event.relatedTarget)) return;
    hideTooltip();
  });
  document.addEventListener('focusin', event => {
    const anchor = event.target.closest?.('[data-tooltip]');
    if (anchor && anchor.matches(':focus-visible')) scheduleTooltip(anchor, 0);
  });
  document.addEventListener('focusout', event => { if (event.target.closest?.('[data-tooltip]')) hideTooltip(); });
  document.addEventListener('pointerdown', hideTooltip, true);
  document.addEventListener('keydown', event => { if (event.key === 'Escape') hideTooltip(); }, true);
  window.addEventListener('resize', hideTooltip);
  document.addEventListener('scroll', hideTooltip, true);
}

/* -------------------------------------------------------------------- toast */

let toastHost = null;
let toastTimer = null;

/** @param {string} text @param {{tone?:'info'|'success'|'warning'|'error', holdMs?:number, action?:{label:string,onClick:()=>void}}} [options] */
export function toast(text, { tone = 'info', holdMs = 3000, action = null } = {}) {
  toastHost ??= el('div', 'toast-host');
  if (!toastHost.isConnected) document.body.append(toastHost);
  const node = el('div', 'toast');
  node.dataset.tone = tone;
  node.setAttribute('role', 'alert');
  const glyph = { success: 'checkCircle', error: 'xCircle', warning: 'alert', info: 'info' }[tone] ?? 'info';
  node.append(icon(glyph, { size: 16 }), el('span', 'toast-text', text));
  if (action) {
    const button = el('button', 'toast-action', action.label);
    button.type = 'button';
    button.addEventListener('click', () => { action.onClick(); node.remove(); });
    node.append(button);
  }
  toastHost.replaceChildren(node);
  removeAnimation(node);
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => dismissToast(node), holdMs);
}

function dismissToast(node) {
  if (!node.isConnected) return;
  node.classList.add('leaving');
  setTimeout(() => node.remove(), 180);
}

function removeAnimation(node) {
  node.classList.remove('entering');
  void node.offsetWidth;
  node.classList.add('entering');
}

/* --------------------------------------------------------------------- menu */

/**
 * @typedef {{label?:string, icon?:string, image?:string, shortcut?:string, disabled?:boolean,
 *   danger?:boolean, badge?:string, checked?:boolean, divider?:boolean,
 *   heading?:string, onSelect?:()=>void, title?:string}} MenuItem
 */

let activeMenu = null;

export function closeMenu() {
  if (!activeMenu) return;
  const hadFocus = activeMenu.node.contains(document.activeElement);
  activeMenu.node.remove();
  window.dispatchEvent(new Event('ui-overlay-change'));
  activeMenu.anchor?.setAttribute('aria-expanded', 'false');
  const { restoreFocus, anchor } = activeMenu;
  activeMenu = null;
  if ((restoreFocus || hadFocus) && anchor?.isConnected) anchor.focus();
}

/**
 * @param {{anchor:HTMLElement, items:MenuItem[], align?:'start'|'end'|'center',
 *   width?:number, side?:'bottom'|'top', className?:string}} options
 */
export function openMenu({ anchor, items, align = 'start', width = 220, side = 'bottom', className = '' }) {
  closeMenu();
  const node = el('div', `menu ${className}`.trim());
  node.setAttribute('role', 'menu');
  node.style.minWidth = `${width}px`;
  let focusable = [];
  for (const item of items) {
    if (item.divider) { node.append(el('div', 'menu-divider')); continue; }
    if (item.heading) { node.append(el('div', 'menu-heading', item.heading)); continue; }
    const button = el('button', 'menu-item');
    button.type = 'button';
    button.setAttribute('role', item.checked === undefined ? 'menuitem' : 'menuitemradio');
    if (item.checked !== undefined) button.setAttribute('aria-checked', String(!!item.checked));
    if (item.disabled) button.disabled = true;
    if (item.danger) button.dataset.danger = 'true';
    if (item.title) button.title = item.title;
    if (item.image) {
      const image = el('img', 'menu-image');
      image.src = item.image;
      image.alt = '';
      image.width = 16;
      image.height = 16;
      button.append(image);
    } else if (item.icon) button.append(icon(item.icon, { size: 16 }));
    else if (item.checked !== undefined) button.append(el('span', 'menu-check', item.checked ? '✓' : ''));
    else button.append(el('span', 'menu-check'));
    const label = el('span', 'menu-label', item.label ?? '');
    button.append(label);
    if (item.badge) {
      const badge = el('span', 'badge-unconnected', item.badge);
      button.append(badge);
    }
    if (item.shortcut) button.append(el('span', 'menu-shortcut', item.shortcut));
    if (!item.disabled) {
      button.addEventListener('click', () => {
        closeMenu();
        item.onSelect?.();
      });
      focusable.push(button);
    }
    node.append(button);
  }
  document.body.append(node);
  window.dispatchEvent(new Event('ui-overlay-change'));
  activeMenu = { node, anchor, restoreFocus: false };
  anchor.setAttribute('aria-expanded', 'true');
  const rect = anchor.getBoundingClientRect();
  const box = node.getBoundingClientRect();
  let left = align === 'end' ? rect.right - box.width : align === 'center' ? rect.left + rect.width / 2 - box.width / 2 : rect.left;
  let top = side === 'top' ? rect.top - box.height - 6 : rect.bottom + 6;
  if (side === 'bottom' && top + box.height > innerHeight - 8) { top = rect.top - box.height - 6; node.dataset.side = 'top'; }
  if (top < 8) top = 8;
  left = Math.max(8, Math.min(innerWidth - box.width - 8, left));
  node.style.left = `${Math.round(left)}px`;
  node.style.top = `${Math.round(top)}px`;
  const selected = focusable.find(button => button.getAttribute('aria-checked') === 'true') ?? focusable[0];
  selected?.focus({ preventScroll: true });
  node.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); closeMenu(); anchor.focus(); return; }
    if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
    event.preventDefault();
    const at = focusable.indexOf(document.activeElement);
    const next = event.key === 'Home' ? 0 : event.key === 'End' ? focusable.length - 1
      : (at + (event.key === 'ArrowDown' ? 1 : -1) + focusable.length) % focusable.length;
    focusable[next]?.focus();
  });
  setTimeout(() => {
    const onPointer = event => {
      if (!node.contains(event.target) && !anchor.contains(event.target)) {
        document.removeEventListener('pointerdown', onPointer, true);
        closeMenu();
      }
    };
    document.addEventListener('pointerdown', onPointer, true);
  }, 0);
  return node;
}

/* ------------------------------------------------------------------- dialog */

/** A confirm dialog rendered with native <dialog>. @param {{title:string, body:string, confirm?:string, cancel?:string, danger?:boolean}} options */
export function confirmDialog({ title, body, confirm = '确认', cancel = '取消', danger = false }) {
  return new Promise(resolve => {
    const dialog = el('dialog', 'modal confirm-modal');
    const form = el('form', 'modal-card');
    form.method = 'dialog';
    const head = el('div', 'modal-head');
    head.append(el('h2', undefined, title));
    const text = el('p', 'confirm-body', body);
    const actions = el('div', 'modal-actions');
    const cancelButton = el('button', 'button secondary', cancel);
    const confirmButton = el('button', `button ${danger ? 'danger' : 'primary'}`, confirm);
    cancelButton.type = 'button'; confirmButton.type = 'button';
    cancelButton.addEventListener('click', () => { dialog.close(); resolve(false); });
    confirmButton.addEventListener('click', () => { dialog.close(); resolve(true); });
    actions.append(cancelButton, confirmButton);
    form.append(head, text, actions);
    dialog.append(form);
    dialog.addEventListener('cancel', event => { event.preventDefault(); dialog.close(); resolve(false); });
    document.body.append(dialog);
    dialog.showModal();
    window.dispatchEvent(new Event('ui-overlay-change'));
    cancelButton.focus();
    dialog.addEventListener('close', () => {
      dialog.remove();
      const focusable = node => {
        if (!node || node === document.body || !node.isConnected) return false;
        const rect = node.getBoundingClientRect();
        return rect.width > 0 && rect.height > 0 && !node.closest('[inert]');
      };
      const target = [document.querySelector('#message'), document.querySelector('#new-session')].find(focusable);
      queueMicrotask(() => target?.focus());
      window.dispatchEvent(new Event('ui-overlay-change'));
    }, { once: true });
  });
}

/* ------------------------------------------------------------------ helpers */

/** Copy through the sandboxed preload, with visible success/failure. */
export async function copyText(api, text, { quiet = false, showFailure = false } = {}) {
  try {
    await api.copyText(String(text ?? ''));
    if (!quiet) toast('已复制', { tone: 'success', holdMs: 1600 });
    return true;
  } catch (error) {
    if (!quiet || showFailure) toast(`复制失败：${error.message}`, { tone: 'error', holdMs: 4000 });
    return false;
  }
}

/** Format a byte count for file rows. */
export function fileSize(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return '';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(bytes < 10240 ? 1 : 0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** Platform-aware label for shortcuts. */
export function shortcutLabel(mac, other) {
  return /darwin/.test(navigator.platform) ? mac : other;
}

/** Add a small "未接入" badge. */
export function unconnected(label = '未接入') {
  return el('span', 'badge-unconnected', label);
}

/** Trap Tab inside a modal element. */
export function trapTab(container, event) {
  if (event.key !== 'Tab') return;
  const focusable = [...container.querySelectorAll('button:not(:disabled),input:not(:disabled),select:not(:disabled),textarea:not(:disabled),[tabindex]:not([tabindex="-1"])')]
    .filter(node => node.getClientRects().length);
  if (!focusable.length) return;
  const first = focusable[0];
  const last = focusable.at(-1);
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last.focus(); }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first.focus(); }
}
