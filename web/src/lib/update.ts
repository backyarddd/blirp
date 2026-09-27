// Update notice and "Update now" logic, kept free of Svelte state so it can be tested.
import type { UpdateStatus } from './api/types.gen';

/** localStorage key: the release whose banner this browser dismissed. */
export const DISMISSED_KEY = 'blirp.update.dismissed';

/**
 * The release the banner offers: an available one this browser has not dismissed. Dismissing
 * hides the banner for that version only; the next release shows it again.
 */
export function bannerVersion(status: UpdateStatus | null, dismissed: string | null): string | null {
  if (!status?.available || !status.latest) return null;
  return status.latest === dismissed ? null : status.latest;
}

/** Storage can be unavailable (private mode, blocked site data): then nothing was dismissed. */
export function readDismissed(storage: () => Storage | null = () => localStorage): string | null {
  try {
    return storage()?.getItem(DISMISSED_KEY) ?? null;
  } catch {
    return null;
  }
}

/** Remembers a dismissal; without storage it lasts for this page only. */
export function writeDismissed(version: string, storage: () => Storage | null = () => localStorage): void {
  try {
    storage()?.setItem(DISMISSED_KEY, version);
  } catch (e) {
    console.warn('blirp: could not remember the dismissed update', e);
  }
}

/** One poll of the daemon while the updater runs. */
export type Probe =
  | { kind: 'unreachable' }
  /** A 401: a new daemon answers (every start issues a new token). */
  | { kind: 'unauthorized' }
  | { kind: 'status'; status: UpdateStatus };

export type Step = 'wait' | 'reload' | { failed: string };

/** sessionStorage key: an update this window started, so a reload can still show its result. */
export const PENDING_KEY = 'blirp.update.pending';
/** How long after a reload a started update's result is still looked for. */
export const PENDING_MS = 15 * 60 * 1000;

export interface PendingUpdate {
  /** `finished_at` of the update log's last entry before it started, or null. */
  before: number | null;
  target: string;
  /** Unix ms. */
  startedAt: number;
}

export function savePending(p: PendingUpdate, storage: () => Storage | null): void {
  try {
    storage()?.setItem(PENDING_KEY, JSON.stringify(p));
  } catch (e) {
    console.warn('blirp: could not remember the running update', e);
  }
}

export function clearPending(storage: () => Storage | null): void {
  try {
    storage()?.removeItem(PENDING_KEY);
  } catch {
    // Blocked storage holds nothing to clear.
  }
}

/** The update this window started, when it is recent and well-formed. */
export function loadPending(storage: () => Storage | null, now: number = Date.now()): PendingUpdate | null {
  let raw: string | null;
  try {
    raw = storage()?.getItem(PENDING_KEY) ?? null;
  } catch {
    return null;
  }
  if (raw === null) return null;
  try {
    const v: unknown = JSON.parse(raw);
    if (typeof v !== 'object' || v === null) return null;
    const o = v as { before?: unknown; target?: unknown; startedAt?: unknown };
    const before = o.before === null || typeof o.before === 'number' ? o.before : undefined;
    if (before === undefined || typeof o.target !== 'string' || typeof o.startedAt !== 'number') return null;
    if (now - o.startedAt > PENDING_MS) return null;
    return { before, target: o.target, startedAt: o.startedAt };
  } catch {
    return null;
  }
}

/**
 * What to do after `probe`, given whether the daemon was unreachable since the update started
 * (`wentDown`: the updater stopped it) and the update log's last entry before it started
 * (`before`, its `finished_at`). A daemon that answers after being down is the restarted one
 * (new or, after a failure, the old version): reload, which signs the desktop app in again.
 * A new failure while the daemon never went down means the updater stopped before touching
 * anything (download, signature).
 */
export function restartStep(probe: Probe, wentDown: boolean, before: number | null): Step {
  if (probe.kind === 'unreachable') return 'wait';
  if (probe.kind === 'unauthorized' || wentDown) return 'reload';
  const last = probe.status.last_update;
  if (!last || last.finished_at === before) return 'wait';
  return last.ok ? 'reload' : { failed: last.error ?? 'the update failed' };
}

/**
 * Semver order of `a` and `b` (negative, 0, positive): numeric `major.minor.patch`, a
 * prerelease (`1.0.0-rc.1`) before its release. Prerelease tags compare as text; build
 * metadata is ignored. Unparseable versions compare equal, so they never prompt anything.
 */
export function compareVersions(a: string, b: string): number {
  const parse = (v: string): { core: number[]; pre: string } | null => {
    const m = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+.*)?$/.exec(v.trim());
    return m ? { core: [Number(m[1]), Number(m[2]), Number(m[3])], pre: m[4] ?? '' } : null;
  };
  const x = parse(a);
  const y = parse(b);
  if (!x || !y) return 0;
  for (let i = 0; i < 3; i++) {
    const d = (x.core[i] ?? 0) - (y.core[i] ?? 0);
    if (d !== 0) return d;
  }
  if (x.pre === y.pre) return 0;
  if (!x.pre) return 1;
  if (!y.pre) return -1;
  return x.pre < y.pre ? -1 : 1;
}

/**
 * After an update the daemon runs a newer version than what shows it. In the desktop app
 * (`desktop`: the shell's version) the app itself is older: relaunch it. Otherwise this page
 * was loaded from the old daemon (`ui`: the SPA's build version): reloading gets the new UI.
 */
export function staleNotice(daemon: string | undefined, ui: string, desktop: string | null): 'restart' | 'reload' | null {
  if (!daemon) return null;
  if (desktop !== null && compareVersions(daemon, desktop) > 0) return 'restart';
  return compareVersions(daemon, ui) > 0 ? 'reload' : null;
}

/** sessionStorage key: the desktop shell's version, for this window only. */
export const DESKTOP_KEY = 'blirp.desktop';

export interface FragmentEnv {
  location: Pick<Location, 'hash' | 'pathname' | 'search'>;
  history: Pick<History, 'replaceState' | 'state'>;
  storage: () => Storage | null;
}

/**
 * The desktop shell signs its window in with `#token=...&app=<its version>`. Takes `app` out of
 * the fragment (the rest stays for the token bootstrap) into this window's sessionStorage.
 */
export function takeDesktopVersion(env: FragmentEnv): void {
  const { hash, pathname, search } = env.location;
  if (!hash.startsWith('#')) return;
  const params = new URLSearchParams(hash.slice(1));
  const version = params.get('app');
  if (version === null) return;
  params.delete('app');
  const rest = params.toString();
  env.history.replaceState(env.history.state, '', `${pathname}${search}${rest ? `#${rest}` : ''}`);
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) return;
  try {
    env.storage()?.setItem(DESKTOP_KEY, version);
  } catch (e) {
    console.warn('blirp: could not remember the desktop app version', e);
  }
}

/** The desktop shell's version, or null in a browser (or with blocked storage). */
export function desktopVersion(storage: () => Storage | null): string | null {
  try {
    return storage()?.getItem(DESKTOP_KEY) ?? null;
  } catch {
    return null;
  }
}

/** The shell's navigation handler relaunches the app on this path (app/src-tauri/src/lib.rs). */
export const DESKTOP_RESTART_PATH = '/desktop/restart';
