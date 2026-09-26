// Per-browser conveniences only. Storage can be unavailable (private mode, blocked site
// data), in which case reads fall back to the default and writes are dropped with a warning.

export function readPref<T extends string>(key: string, allowed: readonly T[], fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    return v !== null && (allowed as readonly string[]).includes(v) ? (v as T) : fallback;
  } catch {
    return fallback;
  }
}

export function writePref(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch (e) {
    console.warn(`blirp: could not persist preference ${key}`, e);
  }
}

export const isMac: boolean =
  typeof navigator !== 'undefined' && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

/** The browser's own platform (not the session's machine): terminal key conventions follow it. */
export const isWindows: boolean =
  typeof navigator !== 'undefined' && /Win/.test(navigator.platform || navigator.userAgent);
