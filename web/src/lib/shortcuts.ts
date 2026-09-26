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

/** Where the SPA runs; terminal copy/paste keys follow that platform's native terminals. */
export type ClientPlatform = 'mac' | 'windows' | 'linux';

/**
 * Copy/paste chords inside a terminal pane. macOS: Cmd+C / Cmd+V. Windows and Linux: Ctrl+Shift+C /
 * Ctrl+Shift+V. Windows also pastes on plain Ctrl+V, like Windows Terminal; on Linux (and macOS)
 * plain Ctrl+V stays with the program (readline quoted insert, vim block select). Plain Ctrl+C is
 * always the program's interrupt. `paste` lets the browser raise its paste event.
 */
export function terminalClipboardKey(e: KeyLike, platform: ClientPlatform): 'copy' | 'paste' | null {
  if (e.altKey) return null;
  const key = e.key.toLowerCase();
  if (key !== 'c' && key !== 'v') return null;
  const chord =
    platform === 'mac'
      ? e.metaKey && !e.ctrlKey
      : e.ctrlKey && !e.metaKey && (e.shiftKey || (platform === 'windows' && key === 'v'));
  if (!chord) return null;
  return key === 'c' ? 'copy' : 'paste';
}

export function shortcutLabel(key: string, mac: boolean): string {
  return mac ? `⌘${key}` : `Ctrl+${key}`;
}
