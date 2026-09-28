// Pin and archive: per-device marks on sessions and projects (never synced). Pinned items float to
// the top of the sidebar groups and the projects list; archived ones are left out of the sidebar,
// lists and the command palette until "Show archived" is on. Stored in localStorage like the other
// per-browser conveniences (prefs.ts); unavailable storage means no marks, never an error.

import type { ProjectSummary, Session } from './api/types.gen';

export type MarkKind = 'pinned' | 'archived';

const STORAGE: Record<MarkKind, string> = { pinned: 'blirp.pinned', archived: 'blirp.archived' };

/** Marks keep at most this many ids each; the oldest go first. */
export const MARKS_MAX = 2000;

/** The mark key of a session. */
export const sessionKey = (id: string): string => `s:${id}`;

/**
 * The mark key of a project. Every machine's Chats bucket is one "Chats" group in the sidebar, so
 * they share one key.
 */
export const projectKey = (p: Pick<ProjectSummary, 'id'> & { chats?: boolean }): string => (p.chats ? 'p:chats' : `p:${p.id}`);

/** Parse a stored list; anything malformed reads as no marks. */
export function parseMarks(raw: string | null): string[] {
  if (raw === null) return [];
  try {
    const v: unknown = JSON.parse(raw);
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string').slice(-MARKS_MAX) : [];
  } catch {
    return [];
  }
}

export function readMarks(kind: MarkKind): string[] {
  try {
    return parseMarks(localStorage.getItem(STORAGE[kind]));
  } catch {
    return [];
  }
}

export function writeMarks(kind: MarkKind, keys: Iterable<string>): void {
  try {
    localStorage.setItem(STORAGE[kind], JSON.stringify([...keys].slice(-MARKS_MAX)));
  } catch (e) {
    console.warn(`blirp: could not save ${kind} items`, e);
  }
}

/** Stable: marked items first, each part in its given order. */
export function markedFirst<T>(items: readonly T[], marked: (item: T) => boolean): T[] {
  const yes: T[] = [];
  const no: T[] = [];
  for (const it of items) (marked(it) ? yes : no).push(it);
  return [...yes, ...no];
}

/** A session is archived by its own mark or its project's (the whole group is put away). */
export function sessionArchived(
  s: Pick<Session, 'id' | 'project_id'>,
  archived: ReadonlySet<string>,
  projects: ReadonlyMap<string, ProjectSummary>,
): boolean {
  if (archived.has(sessionKey(s.id))) return true;
  const p = projects.get(s.project_id);
  return archived.has(p ? projectKey(p) : `p:${s.project_id}`);
}
