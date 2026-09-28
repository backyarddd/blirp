// Global shortcuts (ARCHITECTURE §14). A focused terminal owns every key, like VS Code's integrated
// terminal: only this allowlist (VS Code's commandsToSkipShell) reaches the app from inside it. On
// Windows/Linux the plain Ctrl chords belong to the shell/TUI (Ctrl+K, Ctrl+W, Ctrl+T, Ctrl+Left are
// all readline keys) and Ctrl+Shift+Left/Right select words (PSReadLine), so inside a terminal the
// app shortcuts are Ctrl+Shift+letter, like Windows Terminal, and Ctrl+PageUp/PageDown switch
// sessions, like VS Code's previous/next editor. macOS uses Cmd everywhere; Cmd never reaches the PTY.

export type ShortcutAction = 'palette' | 'new' | 'grid' | 'prev' | 'next' | 'close';

export interface KeyLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
}

const LETTERS: Record<string, ShortcutAction> = {
  k: 'palette',
  t: 'new',
  g: 'grid',
  w: 'close',
};

const SWITCH: Record<string, ShortcutAction> = {
  arrowleft: 'prev',
  arrowright: 'next',
  pageup: 'prev',
  pagedown: 'next',
};

export function matchShortcut(e: KeyLike, mac: boolean, inTerminal: boolean): ShortcutAction | null {
  if (e.altKey) return null;
  if (mac ? !e.metaKey || e.ctrlKey : !e.ctrlKey || e.metaKey) return null;
  const key = e.key.toLowerCase();
  const letter = LETTERS[key];
  const switcher = SWITCH[key];
  if (!mac && inTerminal) {
    if (letter) return e.shiftKey ? letter : null;
    return !e.shiftKey && (key === 'pageup' || key === 'pagedown') ? (switcher ?? null) : null;
  }
  return letter ?? switcher ?? null;
}

/** Where the SPA runs; terminal copy/paste keys follow that platform, as in VS Code. */
export type ClientPlatform = 'mac' | 'windows' | 'linux';

/**
 * Copy/paste chords inside a terminal pane, VS Code's defaults: copy is Cmd+C on macOS and
 * Ctrl+Shift+C elsewhere, and only with a selection (without one the key goes to the program).
 * Windows also copies on plain Ctrl+C while text is selected, clearing the selection (`copy-clear`);
 * with nothing selected Ctrl+C stays the program's interrupt. Paste is Cmd+V, Ctrl+Shift+V, and plain
 * Ctrl+V on Windows; plain Ctrl+V goes to the program on Linux and macOS (readline quoted insert, vim
 * block select), where Shift+Insert pastes on Linux. `paste` lets the browser raise its paste event.
 */
export function terminalClipboardKey(
  e: KeyLike,
  platform: ClientPlatform,
  hasSelection: boolean,
): 'copy' | 'copy-clear' | 'paste' | null {
  if (e.altKey) return null;
  const key = e.key.toLowerCase();
  if (platform === 'mac') {
    if (!e.metaKey || e.ctrlKey || e.shiftKey) return null;
    if (key === 'c') return hasSelection ? 'copy' : null;
    return key === 'v' ? 'paste' : null;
  }
  if (e.metaKey) return null;
  if (platform === 'linux' && key === 'insert' && e.shiftKey && !e.ctrlKey) return 'paste';
  if (!e.ctrlKey) return null;
  if (key === 'c') {
    if (!hasSelection) return null;
    if (e.shiftKey) return 'copy';
    return platform === 'windows' ? 'copy-clear' : null;
  }
  if (key === 'v') return e.shiftKey || platform === 'windows' ? 'paste' : null;
  return null;
}

export function shortcutLabel(key: string, mac: boolean): string {
  return mac ? `⌘${key}` : `Ctrl+${key}`;
}
