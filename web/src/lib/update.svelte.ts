// Release status shown in the top bar, the update banner and Settings > About, and the
// "Update now" flow: the daemon starts `blirp update`, which stops it, replaces blirp and
// starts it again; this page waits for that and reloads.
import { ApiError, api, errorMessage } from './api/client';
import { app } from './app.svelte';
import type { UpdateStatus } from './api/types.gen';
import { bannerVersion, readDismissed, restartStep, writeDismissed, type Probe } from './update';

/** The daemon's cache decides when GitHub is actually asked (at most daily). */
const POLL_MS = 3 * 60 * 60 * 1000;
const RESTART_POLL_MS = 2000;
/** Downloads on a slow connection plus a restart; after this the page says what to do. */
const RESTART_TIMEOUT_MS = 10 * 60 * 1000;

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
  #timer: ReturnType<typeof setInterval> | undefined;

  /** Polls while the app shell is shown; returns the stop function. */
  start(): () => void {
    void this.refresh();
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
    void this.#waitForRestart(before);
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
      this.failure = step.failed;
      this.phase = 'failed';
      return;
    }
    this.phase = 'stuck';
  }
}

export const updates = new UpdateState();
