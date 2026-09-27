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
