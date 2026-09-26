// Machines a session can run on (cloud sessions): badges, recent folders per machine and the
// sessions this client had open, restored when the app is opened again.
import type { Machine, ProjectSummary, Session } from './api/types.gen';

export interface RemoteMachine {
  id: string;
  name: string;
  /** The hub: sessions there are "cloud" sessions. */
  cloud: boolean;
}

/** Badge data for a session's machine; null when it runs on this machine. */
export function remoteMachine(
  machineId: string,
  selfId: string | null,
  machines: ReadonlyMap<string, Machine>,
  hubId: string | null,
): RemoteMachine | null {
  if (selfId === null || machineId === selfId) return null;
  const m = machines.get(machineId);
  return {
    id: machineId,
    name: m?.name ?? `machine ${machineId.slice(0, 8)}`,
    cloud: machineId === hubId || m?.role === 'hub',
  };
}

/**
 * Folders sessions ran in on `machineId`, newest first, then that machine's registered project
 * folders. Sessions and projects replicate, so another machine's folders are known here.
 */
export function recentFolders(
  sessions: readonly Session[],
  projects: readonly ProjectSummary[],
  machineId: string,
  limit = 8,
): string[] {
  const out: string[] = [];
  const add = (p: string): void => {
    if (p && !out.includes(p)) out.push(p);
  };
  const own = sessions.filter((s) => s.machine_id === machineId && !s.worktree);
  for (const s of [...own].sort((a, b) => b.last_activity_at - a.last_activity_at)) add(s.cwd);
  for (const p of projects) for (const path of p.paths) if (path.machine_id === machineId) add(path.path);
  return out.slice(0, limit);
}

/** The project's folder on `machineId`, if it has one there. */
export function folderOn(project: ProjectSummary | undefined, machineId: string): string | null {
  return project?.paths.find((p) => p.machine_id === machineId)?.path ?? null;
}

const OPEN_KEY = 'blirp.openSessions';
const OPEN_MAX = 12;

/** Sessions this client had open, most recent first. Storage may be unavailable (private mode). */
export function readOpenSessions(): string[] {
  try {
    const raw = localStorage.getItem(OPEN_KEY);
    const v: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(0, OPEN_MAX) : [];
  } catch {
    return [];
  }
}

function writeOpenSessions(ids: readonly string[]): void {
  try {
    localStorage.setItem(OPEN_KEY, JSON.stringify(ids.slice(0, OPEN_MAX)));
  } catch (e) {
    console.warn('blirp: could not remember open sessions', e);
  }
}

export function withOpened(ids: readonly string[], id: string): string[] {
  return [id, ...ids.filter((x) => x !== id)].slice(0, OPEN_MAX);
}

export function rememberOpenSession(id: string): void {
  const ids = readOpenSessions();
  if (ids[0] !== id) writeOpenSessions(withOpened(ids, id));
}

export function forgetOpenSession(id: string): void {
  const ids = readOpenSessions();
  if (ids.includes(id)) writeOpenSessions(ids.filter((x) => x !== id));
}

/** The session to show when the app opens without one: the most recent one that still exists. */
export function sessionToRestore(ids: readonly string[], known: ReadonlyMap<string, Session>): string | null {
  return ids.find((id) => known.has(id)) ?? null;
}
