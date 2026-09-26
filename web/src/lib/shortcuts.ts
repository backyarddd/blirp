// Global shortcuts (ARCHITECTURE §14). Inside a terminal on Windows/Linux the plain Ctrl
// chords belong to the shell/TUI (Ctrl+K, Ctrl+W, Ctrl+T, Ctrl+Left are all readline keys),
// so there the app shortcuts need Ctrl+Shift, like Windows Terminal. macOS uses Cmd everywhere.

export type ShortcutAction = 'palette' | 'new' | 'grid' | 'prev' | 'next' | 'close';

export interface KeyLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
}

const KEYS: Record<string, ShortcutAction> = {
  k: 'palette',
  t: 'new',
  g: 'grid',
  w: 'close',
  arrowleft: 'prev',
  arrowright: 'next',
};

export function matchShortcut(e: KeyLike, mac: boolean, inTerminal: boolean): ShortcutAction | null {
  if (e.altKey) return null;
  if (mac ? !e.metaKey || e.ctrlKey : !e.ctrlKey || e.metaKey) return null;
  if (!mac && inTerminal && !e.shiftKey) return null;
  return KEYS[e.key.toLowerCase()] ?? null;
}

export function shortcutLabel(key: string, mac: boolean): string {
  return mac ? `⌘${key}` : `Ctrl+${key}`;
}
