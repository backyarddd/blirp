// Presentation helpers for project file sync (docs/project-files.md).
import type { CopyState, FilesRoot, ProjectFiles, Reason } from './api/types.gen';

/** "0 B", "512 B", "1.5 KB", "12 MB", "1.2 GB" (powers of 1024, one decimal under 10). */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 1024) return `${Math.max(0, Math.round(Number.isFinite(n) ? n : 0))} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

export const REASON_LABELS: Record<Reason, string> = {
  ignored: 'Ignored',
  secret: 'Secrets',
  too_large: 'Too large',
  unsupported: 'Cannot sync',
};

export const STATE_LABELS: Record<CopyState, string> = {
  waiting: 'Waiting',
  scanning: 'Scanning',
  idle: 'Up to date',
  uploading: 'Uploading',
  paused: 'Paused',
  off: 'Off',
  too_large: 'Too large',
  busy: 'Waiting for git',
  error: 'Error',
  never_synced: 'Never synced',
  held_deletes: 'Paused: files disappeared',
};

/**
 * The root a new session elsewhere should start from: the most recently updated one on the hub
 * (a project may have a folder on several machines).
 */
export function pickHubRoot(files: ProjectFiles | null | undefined): FilesRoot | null {
  let best: FilesRoot | null = null;
  for (const r of files?.roots ?? []) {
    if (!r.hub || r.hub.files === 0) continue;
    if (!best?.hub || r.hub.updated_at > best.hub.updated_at) best = r;
  }
  return best;
}

/** Live conflict copies over every root of a project. */
export function conflictCount(files: ProjectFiles | null | undefined): number {
  return (files?.roots ?? []).reduce((n, r) => n + (r.hub?.conflicts ?? 0), 0);
}
