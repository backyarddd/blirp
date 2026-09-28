// Terminal appearance and behavior, per browser (like the theme). Defaults are VS Code's integrated
// terminal's, except the font size (blirp's panes are denser) and Option as Meta on macOS (on, so
// Option+Enter and Option+letter reach TUIs as Alt chords, which Claude Code's terminal setup turns
// on in VS Code too).
import { writePref } from '../prefs';

export type CursorStyle = 'block' | 'underline' | 'bar';

export interface TerminalSettings {
  fontSize: number;
  lineHeight: number;
  letterSpacing: number;
  cursorStyle: CursorStyle;
  cursorBlink: boolean;
  macOptionIsMeta: boolean;
}

export const TERMINAL_DEFAULTS: Readonly<TerminalSettings> = Object.freeze({
  fontSize: 13,
  lineHeight: 1,
  letterSpacing: 0,
  cursorStyle: 'block',
  cursorBlink: false,
  macOptionIsMeta: true,
});

export const LIMITS = {
  fontSize: { min: 6, max: 40, step: 1 },
  lineHeight: { min: 1, max: 3, step: 0.1 },
  letterSpacing: { min: -5, max: 20, step: 1 },
} as const;

const KEY = 'blirp.terminal';
const STYLES: readonly CursorStyle[] = ['block', 'underline', 'bar'];

const clamp = (v: unknown, { min, max }: { min: number; max: number }, fallback: number): number =>
  typeof v === 'number' && Number.isFinite(v) ? Math.min(max, Math.max(min, v)) : fallback;

/** Stored settings, each field checked on its own so one bad value keeps the rest. */
export function parseTerminalSettings(raw: string | null): TerminalSettings {
  let v: Record<string, unknown> = {};
  try {
    const parsed: unknown = raw === null ? {} : JSON.parse(raw);
    if (typeof parsed === 'object' && parsed !== null) v = parsed as Record<string, unknown>;
  } catch {
    // Not JSON: defaults.
  }
  const d = TERMINAL_DEFAULTS;
  return {
    fontSize: Math.round(clamp(v.fontSize, LIMITS.fontSize, d.fontSize)),
    lineHeight: clamp(v.lineHeight, LIMITS.lineHeight, d.lineHeight),
    letterSpacing: Math.round(clamp(v.letterSpacing, LIMITS.letterSpacing, d.letterSpacing)),
    cursorStyle: STYLES.includes(v.cursorStyle as CursorStyle) ? (v.cursorStyle as CursorStyle) : d.cursorStyle,
    cursorBlink: typeof v.cursorBlink === 'boolean' ? v.cursorBlink : d.cursorBlink,
    macOptionIsMeta: typeof v.macOptionIsMeta === 'boolean' ? v.macOptionIsMeta : d.macOptionIsMeta,
  };
}

function load(): TerminalSettings {
  try {
    return parseTerminalSettings(localStorage.getItem(KEY));
  } catch {
    return { ...TERMINAL_DEFAULTS };
  }
}

export const terminalSettings: TerminalSettings = $state(load());

export function setTerminalSettings(patch: Partial<TerminalSettings>): void {
  const next = parseTerminalSettings(JSON.stringify({ ...terminalSettings, ...patch }));
  Object.assign(terminalSettings, next);
  writePref(KEY, JSON.stringify(next));
}

export function resetTerminalSettings(): void {
  setTerminalSettings({ ...TERMINAL_DEFAULTS });
}
