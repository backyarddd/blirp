// Release status shown in the top bar, the update banner and Settings > About, and the
// "Update now" flow: the daemon starts `blirp update`, which stops it, replaces blirp and
// starts it again; this page waits for that and reloads.
import { ApiError, api, errorMessage } from './api/client';
import { app } from './app.svelte';
import type { UpdateStatus } from './api/types.gen';
import {
  DESKTOP_RESTART_PATH,
  bannerVersion,
  clearPending,
  desktopVersion,
  loadPending,
  readDismissed,
  savePending,
  restartStep,
  staleNotice,
  writeDismissed,
  type Probe,
} from './update';

/** The daemon's cache decides when GitHub is actually asked (at most daily). */
const POLL_MS = 3 * 60 * 60 * 1000;
const RESTART_POLL_MS = 2000;
/** Downloads on a slow connection plus a restart; after this the page says what to do. */
const RESTART_TIMEOUT_MS = 10 * 60 * 1000;
/** After the reload the updater may still be writing its log entry (it restarts the daemon first). */
const AFTER_RELOAD_MS = 2 * 60 * 1000;
/** This window's own session: survives the reload that follows an update. */
const windowStorage = (): Storage => sessionStorage;

export type UpdatePhase = 'idle' | 'updating' | 'failed' | 'stuck';

class UpdateState {
  status: UpdateStatus | null = $state.raw(null);
  /** The daemon could not be asked (not: GitHub could not; that is `status.error`). */
  loadError: string | null = $state(null);
  checking = $state(false);
  /** "Update now" was clicked and the daemon has not answered yet. */
  starting = $state(false);
  phase: UpdatePhase = $state('idle');
  /** Why the last "Update now" failed (from the updater's log). */
  failure: string | null = $state(null);
  /** The release being installed. */
  target: string | null = $state(null);
  #dismissed: string | null = $state(readDismissed());
  banner: string | null = $derived(bannerVersion(this.status, this.#dismissed));
  /**
   * The daemon was updated past what shows it: relaunch the desktop app, or reload this page.
   * Recomputed when health changes (sessionStorage is read then, after main.ts filled it).
   */
  stale: 'restart' | 'reload' | null = $derived(
    staleNotice(app.health?.version, __APP_VERSION__, desktopVersion(() => sessionStorage)),
  );
  /** Hidden for this page only; it comes back after a reload while still stale. */
  staleDismissed = $state(false);
  #timer: ReturnType<typeof setInterval> | undefined;

  /** Polls while the app shell is shown; returns the stop function. */
  start(): () => void {
    const pending = loadPending(windowStorage);
    if (pending && this.phase === 'idle') void this.#afterReload(pending.before);
    else void this.refresh();
    clearInterval(this.#timer);
    this.#timer = setInterval(() => void this.refresh(), POLL_MS);
    return () => clearInterval(this.#timer);
  }

  async refresh(): Promise<void> {
    try {
      this.status = await api.update();
      this.loadError = null;
    } catch (e) {
      this.loadError = errorMessage(e);
    }
  }

  /** "Check now" (admin): the daemon asks GitHub unless it did in the last minute. */
  async check(): Promise<void> {
    this.checking = true;
    try {
      this.status = await api.updateCheck();
      this.loadError = null;
    } catch (e) {
      this.loadError = errorMessage(e);
    } finally {
      this.checking = false;
    }
  }

  /** "Restart app": the desktop shell relaunches itself on this navigation (and cancels it). */
  restartApp(): void {
    location.assign(DESKTOP_RESTART_PATH);
  }

  dismiss(): void {
    const v = this.status?.latest;
    if (!v) return;
    this.#dismissed = v;
    writeDismissed(v);
  }

  /** "Update now" (local clients of a script install), after a confirmation. */
  async updateNow(): Promise<void> {
    const version = this.status?.latest;
    if (!version || this.starting || this.phase === 'updating') return;
    if (!confirm(`Update blirp to ${version}? The daemon restarts, so running sessions end (you can resume them).`)) return;
    const before = this.status?.last_update?.finished_at ?? null;
    this.starting = true;
    try {
      await api.updateApply();
    } catch (e) {
      app.noteForbidden(e);
      app.toast(`Could not start the update: ${errorMessage(e)}`);
      return;
    } finally {
      this.starting = false;
    }
    this.failure = null;
    this.target = version;
    this.phase = 'updating';
    savePending({ before, target: version, startedAt: Date.now() }, windowStorage);
    void this.#waitForRestart(before);
  }

  /** The page reloaded after an update this window started: show its result if it failed. */
  async #afterReload(before: number | null): Promise<void> {
    const deadline = Date.now() + AFTER_RELOAD_MS;
    for (;;) {
      await this.refresh();
      const status = this.status;
      const step = status ? restartStep({ kind: 'status', status }, false, before) : 'wait';
      if (step !== 'wait' || Date.now() > deadline) {
        clearPending(windowStorage);
        if (typeof step === 'object') {
          this.failure = step.failed;
          this.phase = 'failed';
        }
        return;
      }
      await new Promise((r) => setTimeout(r, RESTART_POLL_MS));
    }
  }

  async #waitForRestart(before: number | null): Promise<void> {
    const deadline = Date.now() + RESTART_TIMEOUT_MS;
    let wentDown = false;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, RESTART_POLL_MS));
      let probe: Probe;
      try {
        probe = { kind: 'status', status: await api.update() };
      } catch (e) {
        if (e instanceof ApiError && e.unauthorized) probe = { kind: 'unauthorized' };
        else if (e instanceof ApiError && e.status === 0) probe = { kind: 'unreachable' };
        // Anything else (a 5xx while shutting down): not decisive, ask again.
        else continue;
      }
      if (probe.kind === 'unreachable') wentDown = true;
      const step = restartStep(probe, wentDown, before);
      if (step === 'wait') continue;
      if (step === 'reload') {
        // The desktop app signs a reloaded page in to the new daemon; a browser tab shows
        // "Sign in required" (run `blirp open`).
        location.reload();
        return;
      }
      if (probe.kind === 'status') this.status = probe.status;
      clearPending(windowStorage);
      this.failure = step.failed;
      this.phase = 'failed';
      return;
    }
    this.phase = 'stuck';
  }
}

export const updates = new UpdateState();
