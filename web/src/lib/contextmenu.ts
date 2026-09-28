// Context menus: `use:contextmenu` on an item opens its actions at the pointer on a right-click and
// at the item on the ContextMenu key or Shift+F10; F2 renames it where that applies. One menu
// host (ContextMenuHost.svelte) shows them. Terminal panes are never given one: xterm.js owns
// right-click there (copy/paste, selection), so an event from inside `.xterm` is left alone.
import type { ActionReturn } from 'svelte/action';
import type { MenuItem } from './menu';

export interface ContextMenuOptions {
  /** Read when the menu opens, so it reflects the item's current state. Empty: no menu. */
  items: () => MenuItem[];
  /** Accessible name of the menu, e.g. the item's title. */
  label: string;
  /** F2 on the focused item. */
  rename?: (() => void) | undefined;
}

type Opener = (items: MenuItem[], label: string, x: number, y: number, from: HTMLElement) => void;

let opener: Opener | null = null;

/** The host registers how to open the menu; returns the unregister function. */
export function registerContextMenu(open: Opener): () => void {
  opener = open;
  return () => {
    if (opener === open) opener = null;
  };
}

/** Events from inside a terminal pane belong to the terminal. */
export function inTerminal(target: EventTarget | null): boolean {
  return target instanceof Element && target.closest('.xterm') !== null;
}

/** The keys that open a context menu from the keyboard. */
export function isMenuKey(e: Pick<KeyboardEvent, 'key' | 'shiftKey' | 'ctrlKey' | 'altKey' | 'metaKey'>): boolean {
  if (e.ctrlKey || e.altKey || e.metaKey) return false;
  return e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10');
}

export function contextmenu(node: HTMLElement, options: ContextMenuOptions): ActionReturn<ContextMenuOptions> {
  let opts = options;

  const open = (x: number, y: number): boolean => {
    const items = opts.items();
    if (items.length === 0 || opener === null) return false;
    opener(items, opts.label, x, y, node);
    return true;
  };

  const onContextMenu = (e: MouseEvent): void => {
    if (inTerminal(e.target) || e.defaultPrevented) return;
    // The ContextMenu key and Shift+F10 fire this too; some browsers give no position then.
    const r = node.getBoundingClientRect();
    const fromKeyboard = e.clientX === 0 && e.clientY === 0;
    const x = fromKeyboard ? r.left + 12 : e.clientX;
    const y = fromKeyboard ? r.top + Math.min(r.height, 28) : e.clientY;
    // The innermost item's menu wins (a session card inside a group).
    if (open(x, y)) e.preventDefault();
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    if (inTerminal(e.target) || e.defaultPrevented) return;
    // Only for the item itself or its own links and buttons, never for a field inside it.
    if (e.target instanceof HTMLElement && e.target.matches('input, textarea, select, [contenteditable]')) return;
    if (e.key === 'F2' && !e.shiftKey && !e.ctrlKey && !e.altKey && !e.metaKey && opts.rename) {
      e.preventDefault();
      opts.rename();
      return;
    }
    if (isMenuKey(e)) {
      const r = node.getBoundingClientRect();
      if (open(r.left + 12, r.top + Math.min(r.height, 28))) e.preventDefault();
    }
  };

  node.addEventListener('contextmenu', onContextMenu);
  node.addEventListener('keydown', onKeyDown);
  return {
    update(next) {
      opts = next;
    },
    destroy() {
      node.removeEventListener('contextmenu', onContextMenu);
      node.removeEventListener('keydown', onKeyDown);
    },
  };
}
