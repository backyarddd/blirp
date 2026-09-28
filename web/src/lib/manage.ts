// What the session and project actions do (actions.ts decides which are offered). Mutations go
// through the daemon; reversible ones (rename, move, pin, archive, delete to the Trash, restore)
// end in a toast with Undo, permanent ones (deleting a session) ask first in a dialog.
import { SvelteSet } from 'svelte/reactivity';
import { ApiError, api, errorMessage } from './api/client';
import type { OpenTarget, ProjectSummary, Session } from './api/types.gen';
import { app } from './app.svelte';
import { remoteRefusal } from './capabilities';
import { dialogs } from './dialogs.svelte';
import { bulkSummary, type ActionEnv, type BulkFailure, type ProjectOps, type SessionOps } from './actions';
import { projectKey, sessionKey, type MarkKind } from './marks';
import { navigate, nav } from './router.svelte';
import { href } from './router';
import { isChats, isLive, sessionTitle } from './status';

/** Projects the Projects page offers to move to Chats; their menus offer it too. */
export const chatCandidates = new SvelteSet<string>();

/** The environment the menus of this client are built for (reactive where it is read). */
export function actionEnv(): ActionEnv {
  return {
    control: app.control,
    admin: app.admin,
    selfId: app.selfId,
    agents: app.agents,
    pinned: app.pinned,
    archived: app.archived,
    handingOff: (id) => app.handoffFrom.has(id),
    chatCandidate: (id) => chatCandidates.has(id),
  };
}

const quoted = (s: Session): string => `"${sessionTitle(s)}"`;

/** Name of the machine that owns a session, for messages about it. */
async function ownerName(machineId: string): Promise<string> {
  const known = app.machineById.get(machineId)?.name;
  if (known) return known;
  try {
    const m = (await api.machines.list()).find((x) => x.id === machineId);
    if (m) return m.name;
  } catch {
    // Only for the message; fall through to a generic name.
  }
  return 'The machine that runs this session';
}

/**
 * Why an action on `s` failed, for a message. Stop, resume and delete of another machine's session
 * are forwarded to it: its refusal (offline, no control from here) names that machine and is not
 * taken as a change of this client's rights.
 */
export async function sessionFailure(s: Session, e: unknown): Promise<string> {
  const remote = s.machine_id !== app.selfId;
  const why = remote && e instanceof ApiError ? remoteRefusal(e.code, await ownerName(s.machine_id)) : null;
  if (why !== null) return why;
  app.noteForbidden(e);
  if (e instanceof ApiError && e.code === 'session_live') return 'it is running; stop it first';
  return errorMessage(e);
}

async function onSession<T>(s: Session, what: string, fn: () => Promise<T>): Promise<T | undefined> {
  try {
    return await fn();
  } catch (e) {
    app.toast(`Could not ${what}: ${await sessionFailure(s, e)}`);
    return undefined;
  }
}

export async function copyText(text: string, what: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    app.toast(`${what} copied`, 'info');
  } catch (e) {
    app.toast(`Could not copy: ${errorMessage(e)}`);
  }
}

/** What an Undo says when the item was changed again after the action (here or on another client). */
export const NOT_UNDONE = 'Not undone: it was changed again since.';

/** An Undo that runs only while `still()` holds (the item is as the action left it). */
function undoIf(still: () => boolean, run: () => void): { label: string; run: () => void } {
  return {
    label: 'Undo',
    run: () => {
      if (still()) run();
      else app.toast(NOT_UNDONE, 'info');
    },
  };
}

/** Rename a session; an empty title goes back to the default one. */
export async function renameSession(s: Session, title: string, undoable = true): Promise<boolean> {
  const next = title.trim() || null;
  if (next === (s.title?.trim() || null)) return true;
  const out = await onSession(s, 'rename the session', () => api.sessions.rename(s.id, next));
  if (!out) return false;
  app.upsertSession(out);
  app.toast(
    `Renamed to "${sessionTitle(out)}"`,
    'info',
    undoable
      ? undoIf(
          () => app.sessionById.get(out.id)?.title === out.title,
          () => void renameSession(out, s.title ?? '', false),
        )
      : undefined,
  );
  return true;
}

export type MoveTarget = { kind: 'chats' } | { kind: 'project'; id: string } | { kind: 'new'; name: string };

/** Where a session was before a move, as the move endpoint takes it (null: its machine's Chats). */
const moveBackTarget = (before: Session): string | null => (isChats(before.project_id, app.projectById) ? null : before.project_id);

/**
 * Move sessions (one, or a bulk selection) into a project, a new one, or Chats. One toast sums it
 * up, naming what failed, with Undo for what moved.
 */
export async function moveSessions(list: readonly Session[], target: MoveTarget): Promise<boolean> {
  let projectId: string | null = null;
  if (target.kind === 'new') {
    const p = await app.act(() => api.projects.create({ name: target.name }));
    if (!p) return false;
    app.upsertProject(p);
    projectId = p.id;
  } else if (target.kind === 'project') {
    projectId = target.id;
  }
  const results = await Promise.allSettled(list.map((s) => api.sessions.move(s.id, projectId)));
  const moved: { before: Session; after: Session }[] = [];
  const failures: BulkFailure[] = [];
  for (const [i, r] of results.entries()) {
    const before = list[i];
    if (!before) continue;
    if (r.status === 'fulfilled') {
      app.upsertSession(r.value);
      moved.push({ before, after: r.value });
    } else {
      failures.push({ title: sessionTitle(before), reason: await sessionFailure(before, r.reason) });
    }
  }
  const where = target.kind === 'chats' ? 'Chats' : `"${app.projectById.get(projectId ?? '')?.name ?? 'the project'}"`;
  const only = list.length === 1 ? list[0] : undefined;
  const text =
    only === undefined
      ? bulkSummary('Moved', 'Not moved', list.length, failures, ` to ${where}`)
      : failures.length === 0
        ? `Moved ${quoted(only)} to ${where}`
        : `Could not move ${quoted(only)}: ${failures[0]?.reason ?? 'unknown error'}`;
  const undo = moved.length > 0 ? { label: 'Undo', run: () => void moveBack(moved) } : undefined;
  app.toast(text, failures.length > 0 ? 'error' : 'info', undo);
  return failures.length === 0;
}

async function moveBack(all: readonly { before: Session; after: Session }[]): Promise<void> {
  // Sessions moved again since (here or on another client) stay where they are now.
  const moved = all.filter(({ after }) => app.sessionById.get(after.id)?.project_id === after.project_id);
  if (moved.length === 0) {
    app.toast(NOT_UNDONE, 'info');
    return;
  }
  const results = await Promise.allSettled(moved.map(({ before }) => api.sessions.move(before.id, moveBackTarget(before))));
  const failures: BulkFailure[] = all
    .filter((m) => !moved.includes(m))
    .map((m) => ({ title: sessionTitle(m.before), reason: 'moved again since' }));
  for (const [i, r] of results.entries()) {
    const m = moved[i];
    if (!m) continue;
    if (r.status === 'fulfilled') app.upsertSession(r.value);
    else failures.push({ title: sessionTitle(m.before), reason: await sessionFailure(m.after, r.reason) });
  }
  app.toast(
    bulkSummary('Moved', 'Not moved', all.length, failures, ' back'),
    failures.length === 0 ? 'info' : 'error',
  );
}

const MARK_TEXT: Record<MarkKind, [on: string, off: string]> = {
  pinned: ['Pinned', 'Unpinned'],
  archived: ['Archived', 'Unarchived'],
};

/** Pin or archive (on this device only), with Undo. */
export function markSessions(kind: MarkKind, list: readonly Session[], on: boolean, undoable = true): void {
  const set = kind === 'pinned' ? app.pinned : app.archived;
  const changed = list.filter((s) => set.has(sessionKey(s.id)) !== on);
  for (const s of changed) app.setMark(kind, sessionKey(s.id), on);
  if (changed.length === 0) return;
  const only = changed.length === 1 ? changed[0] : undefined;
  const what = only ? quoted(only) : `${changed.length} sessions`;
  const hint = kind === 'archived' && on && !app.showArchived ? '. Show archived lists it again.' : '';
  app.toast(
    `${MARK_TEXT[kind][on ? 0 : 1]} ${what}${hint}`,
    'info',
    undoable
      ? undoIf(
          () => changed.some((s) => set.has(sessionKey(s.id)) === on),
          // Only the ones still as this left them.
          () => markSessions(kind, changed.filter((s) => set.has(sessionKey(s.id)) === on), !on, false),
        )
      : undefined,
  );
}

export function markProject(kind: MarkKind, p: ProjectSummary, on: boolean, undoable = true): void {
  const key = projectKey(p);
  const set = kind === 'pinned' ? app.pinned : app.archived;
  if (set.has(key) === on) return;
  app.setMark(kind, key, on);
  const name = p.chats ? 'Chats' : `"${p.name}"`;
  const hint = kind === 'archived' && on && !app.showArchived ? '. Show archived lists it again.' : '';
  app.toast(
    `${MARK_TEXT[kind][on ? 0 : 1]} ${name}${hint}`,
    'info',
    undoable ? undoIf(() => set.has(key) === on, () => markProject(kind, p, !on, false)) : undefined,
  );
}

async function stopSession(s: Session): Promise<void> {
  const ok = await dialogs.confirm({
    title: 'Stop session?',
    body: `Stop ${quoted(s)}? This ends the agent process and everything it started. You can resume it later.`,
    confirm: 'Stop',
    danger: true,
  });
  if (ok) await onSession(s, 'stop the session', () => api.sessions.stop(s.id));
}

async function resumeSession(s: Session): Promise<void> {
  const out = await onSession(s, 'resume the session', () => api.sessions.resume(s.id));
  if (out) app.upsertSession(out);
}

const DELETE_NOTE =
  "This cannot be undone. The agent's own transcript file is not touched, and memory records the sessions produced stay.";

async function deleteSession(s: Session): Promise<boolean> {
  // The `session_deleted` event may unmount the caller before the reply arrives.
  const done = await onSession(s, 'delete the session', async () => {
    await api.sessions.delete(s.id);
    return true;
  });
  if (done) app.removeSession(s.id);
  return done === true;
}

async function removeSession(s: Session): Promise<void> {
  const ok = await dialogs.confirm({
    title: 'Delete session?',
    body: `${quoted(s)} is deleted for good on every synced machine, with its subagent sessions. ${DELETE_NOTE}`,
    confirm: 'Delete',
    danger: true,
  });
  if (ok && (await deleteSession(s))) app.toast('Session deleted', 'info');
}

/** How long "Stop and delete" waits for the stopped session to end. */
const STOP_WAIT_MS = 30_000;

/** Resolves once the session is no longer live (as pushed, or read back), false on timeout. */
async function waitEnded(id: string): Promise<boolean> {
  const deadline = Date.now() + STOP_WAIT_MS;
  let nextRead = Date.now() + 2000;
  while (Date.now() < deadline) {
    const s = app.sessionById.get(id);
    if (!s || !isLive(s.status)) return true;
    // The pushed update may be missed (a reconnect): ask now and then.
    if (Date.now() >= nextRead) {
      nextRead = Date.now() + 2000;
      try {
        const { children_count: _count, ...fresh } = await api.sessions.get(id);
        app.upsertSession(fresh);
        if (!isLive(fresh.status)) return true;
      } catch (e) {
        console.warn(`blirp: could not read session ${id}`, e);
      }
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  return false;
}

async function stopAndRemoveSession(s: Session): Promise<void> {
  const ok = await dialogs.confirm({
    title: 'Stop and delete session?',
    body: `${quoted(s)} is stopped (its agent process and everything it started end), then deleted for good on every synced machine, with its subagent sessions. ${DELETE_NOTE}`,
    confirm: 'Stop and delete',
    danger: true,
  });
  if (!ok) return;
  // stop answers 202 without a body: the result is `true` only when it did not fail.
  const stopped = await onSession(s, 'stop the session', async () => {
    await api.sessions.stop(s.id);
    return true;
  });
  if (stopped !== true) return;
  if (!(await waitEnded(s.id))) {
    app.toast(`${quoted(s)} did not stop within ${STOP_WAIT_MS / 1000} s, so it was not deleted. Delete it once it has ended.`);
    return;
  }
  if (await deleteSession(s)) app.toast('Session stopped and deleted', 'info');
}

/** Delete many sessions after one confirmation; one toast names what could not be deleted. */
export async function deleteSessions(list: readonly Session[]): Promise<boolean> {
  if (list.length === 0) return false;
  const ok = await dialogs.confirm({
    title: list.length === 1 ? 'Delete session?' : `Delete ${list.length} sessions?`,
    body: `These are deleted for good on every synced machine, with their subagent sessions. Running sessions are skipped: stop them first. ${DELETE_NOTE}`,
    list: list.map((s) => sessionTitle(s)),
    confirm: list.length === 1 ? 'Delete' : `Delete ${list.length}`,
    danger: true,
  });
  if (!ok) return false;
  const results = await Promise.allSettled(list.map((s) => api.sessions.delete(s.id)));
  const failures: BulkFailure[] = [];
  for (const [i, r] of results.entries()) {
    const s = list[i];
    if (!s) continue;
    if (r.status === 'fulfilled') app.removeSession(s.id);
    else failures.push({ title: sessionTitle(s), reason: await sessionFailure(s, r.reason) });
  }
  app.toast(bulkSummary('Deleted', 'Not deleted', list.length, failures), failures.length > 0 ? 'error' : 'info');
  return true;
}

async function removeWorktree(s: Session): Promise<void> {
  const ok = await dialogs.confirm({
    title: 'Remove worktree?',
    body: `Remove the git worktree of ${quoted(s)}? The folder is deleted; its branch is kept.`,
    confirm: 'Remove worktree',
    danger: true,
  });
  if (ok) await removeWorktreeNow(s, false);
}

/** `force` deletes uncommitted changes too (asked for in the dirty-worktree dialog). */
export async function removeWorktreeNow(s: Session, force: boolean): Promise<void> {
  try {
    app.upsertSession(await api.sessions.removeWorktree(s.id, force));
    dialogs.dirtyWorktree = null;
    app.toast('Worktree removed', 'info');
  } catch (e) {
    if (!force && e instanceof ApiError && e.code === 'worktree_dirty') {
      dialogs.dirtyWorktree = { session: s, message: e.message };
    } else {
      dialogs.dirtyWorktree = null;
      app.noteForbidden(e);
      app.toast(`Could not remove the worktree: ${errorMessage(e)}`);
    }
  }
}

export const sessionOps: SessionOps = {
  rename: (s) => {
    dialogs.renaming = { kind: 'session', session: s };
  },
  move: (s) => {
    dialogs.moving = [s];
  },
  setPinned: (s, on) => markSessions('pinned', [s], on),
  setArchived: (s, on) => markSessions('archived', [s], on),
  stop: (s) => void stopSession(s),
  resume: (s) => void resumeSession(s),
  fork: (s) => void app.launch({ continue_from: s.id, agent: s.agent }),
  continueIn: (s) => {
    dialogs.continuing = s;
  },
  // A shell in the session's folder, on its machine; the daemon files it under the same project.
  terminalHere: (s) =>
    void app.launch({ cwd: s.cwd, agent: 'shell', ...(s.machine_id !== app.selfId ? { machine: s.machine_id } : {}) }),
  open: (s, target) => void app.act(() => api.sessions.open(s.id, target)),
  copy: (text, what) => void copyText(text, what),
  removeWorktree: (s) => void removeWorktree(s),
  remove: (s) => void removeSession(s),
  stopAndRemove: (s) => void stopAndRemoveSession(s),
};

export async function renameProject(p: ProjectSummary, name: string, undoable = true): Promise<boolean> {
  const next = name.trim();
  if (!next || next === p.name) return next !== '';
  const out = await app.act(() => api.projects.rename(p.id, next));
  if (!out) return false;
  app.upsertProject(out);
  app.toast(
    `Renamed to "${out.name}"`,
    'info',
    undoable
      ? undoIf(
          () => app.projectById.get(out.id)?.name === out.name,
          () => void renameProject(out, p.name, false),
        )
      : undefined,
  );
  return true;
}

/** Register a folder on this machine; the error message for the form, or null when it worked. */
export async function addProjectFolder(p: ProjectSummary, path: string): Promise<string | null> {
  try {
    app.upsertProject(await api.projects.addFolder(p.id, path));
    app.toast('Folder added', 'info');
    return null;
  } catch (e) {
    app.noteForbidden(e);
    return errorMessage(e);
  }
}

export async function mergeProject(from: ProjectSummary, into: ProjectSummary): Promise<boolean> {
  const merged = await app.act(() => api.projects.merge(from.id, into.id), `Merged "${from.name}" into "${into.name}"`);
  if (!merged) return false;
  app.upsertProject(merged);
  // Its sessions now belong to the target; removing the project re-reads the list.
  app.removeProject(from.id);
  if (nav.route.name === 'project' && nav.route.projectId === from.id) navigate(href.project(merged.id));
  return true;
}

async function projectToChats(p: ProjectSummary): Promise<void> {
  const ok = await dialogs.confirm({
    title: 'Move to Chats?',
    body: `"${p.name}" is removed as a project: its sessions and summarized records move to Chats, and its folders are unregistered. Files on disk are not touched.`,
    confirm: 'Move to Chats',
  });
  if (!ok) return;
  const done = await app.act(async () => (await api.projects.toChats(p.id), true), `Moved "${p.name}" to Chats`);
  if (!done) return;
  chatCandidates.delete(p.id);
  app.removeProject(p.id);
  void app.refreshSessions();
}

async function deleteProject(p: ProjectSummary, ask = true): Promise<void> {
  if (ask) {
    const live = p.live_session_count > 0 ? ` Its ${p.live_session_count} running ${p.live_session_count === 1 ? 'session keeps' : 'sessions keep'} running, hidden with it.` : '';
    const ok = await dialogs.confirm({
      title: 'Delete project?',
      body:
        `"${p.name}" moves to the Trash: it is hidden with its sessions, and its folders are unregistered on every synced machine.${live} ` +
        'Files on disk are not touched, and its sessions and memory are kept. Restore it from the Trash to bring it back with its ' +
        "sessions and this machine's folders; folders on other machines are not restored.",
      confirm: 'Move to Trash',
      danger: true,
    });
    if (!ok) return;
  }
  const done = await app.act(async () => (await api.projects.remove(p.id), true));
  if (!done) return;
  app.removeProject(p.id);
  if (nav.route.name === 'project' && nav.route.projectId === p.id) navigate(href.projects());
  app.toast(
    `Moved "${p.name}" to the Trash`,
    'info',
    // Restored meanwhile (another client): nothing to undo.
    undoIf(() => !app.projectById.has(p.id), () => void restoreProject(p, false)),
  );
}

export async function restoreProject(p: ProjectSummary, undoable = true): Promise<ProjectSummary | undefined> {
  const out = await app.act(() => api.projects.restore(p.id));
  if (!out) return undefined;
  // Its sessions are listed again (also when it went to the Trash before this page loaded).
  app.projectRestored(out);
  app.toast(
    `Restored "${out.name}"`,
    'info',
    undoable ? undoIf(() => app.projectById.has(out.id), () => void deleteProject(out, false)) : undefined,
  );
  return out;
}

async function removeFolder(p: ProjectSummary, path: string): Promise<void> {
  const last = p.paths.length === 1;
  const ok = await dialogs.confirm({
    title: 'Remove folder?',
    body:
      `Remove ${path} from "${p.name}"? The folder itself is not touched, and the project keeps its sessions and memory.` +
      (last ? ' With no folder left, its new sessions start in a blirp workspace.' : ''),
    confirm: 'Remove folder',
    danger: true,
  });
  if (!ok) return;
  const out = await app.act(() => api.projects.removeFolder(p.id, path), 'Folder removed');
  if (out) app.upsertProject(out);
}

export const projectOps: ProjectOps = {
  newSession: (p) => app.openNewSession(p.chats ? null : p.id),
  rename: (p) => {
    if (!p.chats) dialogs.renaming = { kind: 'project', project: p };
  },
  setPinned: (p, on) => markProject('pinned', p, on),
  setArchived: (p, on) => markProject('archived', p, on),
  addFolder: (p) => {
    dialogs.addingFolder = p;
  },
  open: (p, target: OpenTarget, path) => void app.act(() => api.projects.open(p.id, target, path)),
  terminalHere: (p, path) => void app.launch({ project_id: p.id, agent: 'shell', ...(path ? { cwd: path } : {}) }),
  copy: (text, what) => void copyText(text, what),
  merge: (p) => {
    dialogs.merging = p;
  },
  toChats: (p) => void projectToChats(p),
  remove: (p) => void deleteProject(p),
  removeFolder: (p, path) => void removeFolder(p, path),
  restore: (p) => void restoreProject(p),
};
