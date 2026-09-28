// What can be done to a session or a project, as menu items: the context menus, the "⋯" buttons,
// the session toolbar, the project header and the command palette all show these. Which items
// appear follows this client's rights (§11) and the item's state; what they do is `ops`
// (manage.ts), so the gating here is plain data and tested on its own.
import Pencil from '@lucide/svelte/icons/pencil';
import FolderInput from '@lucide/svelte/icons/folder-input';
import Pin from '@lucide/svelte/icons/pin';
import PinOff from '@lucide/svelte/icons/pin-off';
import Archive from '@lucide/svelte/icons/archive';
import ArchiveRestore from '@lucide/svelte/icons/archive-restore';
import Play from '@lucide/svelte/icons/play';
import Square from '@lucide/svelte/icons/square';
import GitFork from '@lucide/svelte/icons/git-fork';
import ArrowRightLeft from '@lucide/svelte/icons/arrow-right-left';
import SquareTerminal from '@lucide/svelte/icons/square-terminal';
import FolderOpen from '@lucide/svelte/icons/folder-open';
import Code from '@lucide/svelte/icons/code';
import Copy from '@lucide/svelte/icons/copy';
import FolderX from '@lucide/svelte/icons/folder-x';
import Trash from '@lucide/svelte/icons/trash-2';
import Plus from '@lucide/svelte/icons/plus';
import FolderPlus from '@lucide/svelte/icons/folder-plus';
import GitMerge from '@lucide/svelte/icons/git-merge';
import MessagesSquare from '@lucide/svelte/icons/messages-square';
import RotateCcw from '@lucide/svelte/icons/rotate-ccw';
import type { AgentInfo, OpenTarget, ProjectSummary, Session } from './api/types.gen';
import { SEPARATOR, tidy, type MenuItem } from './menu';
import { projectKey, sessionKey } from './marks';
import { canResume, isLive, isSubagent } from './status';

/** What the menus depend on besides the item itself. */
export interface ActionEnv {
  /** Launch, stop, resume, delete, move, rename (§11). */
  control: boolean;
  /** Delete, merge and restore projects; open folders on this machine's desktop. */
  admin: boolean;
  /** This machine's id: folders and worktrees of other machines are not opened from here. */
  selfId: string | null;
  agents: readonly Pick<AgentInfo, 'id' | 'resume_command'>[];
  pinned: ReadonlySet<string>;
  archived: ReadonlySet<string>;
  /** A new session from this session is being prepared (the daemon may summarize it first). */
  handingOff: (sessionId: string) => boolean;
  /** Projects offered to move to Chats (they look like chats). */
  chatCandidate: (projectId: string) => boolean;
}

export interface SessionOps {
  rename(s: Session): void;
  move(s: Session): void;
  setPinned(s: Session, on: boolean): void;
  setArchived(s: Session, on: boolean): void;
  stop(s: Session): void;
  resume(s: Session): void;
  fork(s: Session): void;
  continueIn(s: Session): void;
  terminalHere(s: Session): void;
  open(s: Session, target: OpenTarget): void;
  copy(text: string, what: string): void;
  removeWorktree(s: Session): void;
  remove(s: Session): void;
  stopAndRemove(s: Session): void;
}

export interface ProjectOps {
  newSession(p: ProjectSummary): void;
  rename(p: ProjectSummary): void;
  setPinned(p: ProjectSummary, on: boolean): void;
  setArchived(p: ProjectSummary, on: boolean): void;
  addFolder(p: ProjectSummary): void;
  open(p: ProjectSummary, target: OpenTarget, path: string): void;
  terminalHere(p: ProjectSummary, path: string | null): void;
  copy(text: string, what: string): void;
  merge(p: ProjectSummary): void;
  toChats(p: ProjectSummary): void;
  remove(p: ProjectSummary): void;
  removeFolder(p: ProjectSummary, path: string): void;
  restore(p: ProjectSummary): void;
}

/** `claude --resume <id>`: resumes the agent's own session outside blirp; null when it has none. */
export function resumeCommand(
  s: Pick<Session, 'agent' | 'agent_session_id'>,
  agents: readonly Pick<AgentInfo, 'id' | 'resume_command'>[],
): string | null {
  const cmd = agents.find((a) => a.id === s.agent)?.resume_command;
  if (!cmd || cmd.length === 0 || s.agent_session_id === null) return null;
  return [...cmd, s.agent_session_id].join(' ');
}

/** The folder a project's "Open" and "Copy path" use here: its first folder, else its workspace. */
export function projectPath(p: ProjectSummary): string | null {
  return p.paths.find((x) => x.local)?.path ?? p.workspace;
}

export function sessionActions(s: Session, env: ActionEnv, ops: SessionOps): MenuItem[] {
  const live = isLive(s.status);
  const own = s.origin === 'blirp';
  const local = env.selfId !== null && s.machine_id === env.selfId;
  const child = isSubagent(s);
  const pinned = env.pinned.has(sessionKey(s.id));
  const archived = env.archived.has(sessionKey(s.id));
  const resume = resumeCommand(s, env.agents);
  const items: MenuItem[] = [];
  const add = (on: boolean, item: MenuItem): void => {
    if (on) items.push(item);
  };
  add(env.control, { label: 'Rename…', hint: 'F2', icon: Pencil, onselect: () => ops.rename(s) });
  // Subagent sessions move with their parent and are never listed on their own.
  add(env.control && !child, { label: 'Move…', icon: FolderInput, onselect: () => ops.move(s) });
  add(!child, { label: pinned ? 'Unpin' : 'Pin', icon: pinned ? PinOff : Pin, onselect: () => ops.setPinned(s, !pinned) });
  add(!child, {
    label: archived ? 'Unarchive' : 'Archive',
    icon: archived ? ArchiveRestore : Archive,
    onselect: () => ops.setArchived(s, !archived),
  });
  items.push(SEPARATOR);
  add(env.control && canResume(s), { label: 'Resume', icon: Play, onselect: () => ops.resume(s) });
  add(env.control && live && own, { label: 'Stop', icon: Square, onselect: () => ops.stop(s) });
  items.push(SEPARATOR);
  add(env.control, {
    label: 'Start new session from this session',
    icon: GitFork,
    disabled: env.handingOff(s.id),
    onselect: () => ops.fork(s),
  });
  add(env.control, { label: 'Continue in…', icon: ArrowRightLeft, disabled: env.handingOff(s.id), onselect: () => ops.continueIn(s) });
  add(env.control, { label: 'Open terminal here', icon: SquareTerminal, onselect: () => ops.terminalHere(s) });
  // A window on this machine's desktop: its own clients, its own sessions.
  add(env.admin && local, { label: 'Open folder', icon: FolderOpen, onselect: () => ops.open(s, 'folder') });
  add(env.admin && local, { label: 'Open in editor', icon: Code, onselect: () => ops.open(s, 'editor') });
  items.push(SEPARATOR);
  items.push({ label: 'Copy session id', icon: Copy, onselect: () => ops.copy(s.id, 'Session id') });
  if (resume !== null) items.push({ label: 'Copy resume command', icon: Copy, onselect: () => ops.copy(resume, 'Resume command') });
  items.push({ label: 'Copy folder path', icon: Copy, onselect: () => ops.copy(s.cwd, 'Folder path') });
  items.push(SEPARATOR);
  // The worktree lives on the session's machine; the daemon refuses others.
  add(env.control && local && !live && s.worktree !== null, { label: 'Remove worktree…', icon: FolderX, onselect: () => ops.removeWorktree(s) });
  // Running sessions are refused (409 `session_live`): stop first, which a blirp session can do here.
  add(env.control && !live, { label: 'Delete…', icon: Trash, danger: true, onselect: () => ops.remove(s) });
  add(env.control && live && own, { label: 'Stop and delete…', icon: Trash, danger: true, onselect: () => ops.stopAndRemove(s) });
  return tidy(items);
}

export function projectActions(p: ProjectSummary, env: ActionEnv, ops: ProjectOps): MenuItem[] {
  const pinned = env.pinned.has(projectKey(p));
  const archived = env.archived.has(projectKey(p));
  const path = projectPath(p);
  const items: MenuItem[] = [];
  const add = (on: boolean, item: MenuItem): void => {
    if (on) items.push(item);
  };
  add(env.control, { label: p.chats ? 'New chat' : 'New session', icon: Plus, onselect: () => ops.newSession(p) });
  add(env.control, {
    label: 'Rename…',
    hint: p.chats ? 'Chats keeps its name' : 'F2',
    icon: Pencil,
    disabled: p.chats,
    onselect: () => ops.rename(p),
  });
  items.push({ label: pinned ? 'Unpin' : 'Pin', icon: pinned ? PinOff : Pin, onselect: () => ops.setPinned(p, !pinned) });
  add(!p.chats, {
    label: archived ? 'Unarchive' : 'Archive',
    icon: archived ? ArchiveRestore : Archive,
    onselect: () => ops.setArchived(p, !archived),
  });
  items.push(SEPARATOR);
  add(env.control && !p.chats, { label: 'Add folder…', icon: FolderPlus, onselect: () => ops.addFolder(p) });
  if (path !== null && !p.chats) {
    add(env.admin, { label: 'Open folder', icon: FolderOpen, onselect: () => ops.open(p, 'folder', path) });
    add(env.admin, { label: 'Open in editor', icon: Code, onselect: () => ops.open(p, 'editor', path) });
  }
  add(env.control && !p.chats, { label: 'Open terminal here', icon: SquareTerminal, onselect: () => ops.terminalHere(p, null) });
  items.push(SEPARATOR);
  if (path !== null && !p.chats) items.push({ label: 'Copy folder path', icon: Copy, onselect: () => ops.copy(path, 'Folder path') });
  add(!p.chats, { label: 'Copy project id', icon: Copy, onselect: () => ops.copy(p.id, 'Project id') });
  items.push(SEPARATOR);
  add(env.admin && !p.chats, { label: 'Merge into…', icon: GitMerge, onselect: () => ops.merge(p) });
  add(env.control && !p.chats && env.chatCandidate(p.id), { label: 'Move to Chats…', icon: MessagesSquare, onselect: () => ops.toChats(p) });
  add(env.admin && !p.chats, { label: 'Delete…', icon: Trash, danger: true, onselect: () => ops.remove(p) });
  return tidy(items);
}

/** One of a project's folders on this machine (the project header lists them). */
export function folderActions(p: ProjectSummary, path: string, env: ActionEnv, ops: ProjectOps): MenuItem[] {
  return tidy([
    ...(env.admin
      ? [
          { label: 'Open folder', icon: FolderOpen, onselect: () => ops.open(p, 'folder', path) },
          { label: 'Open in editor', icon: Code, onselect: () => ops.open(p, 'editor', path) },
        ]
      : []),
    ...(env.control ? [{ label: 'Open terminal here', icon: SquareTerminal, onselect: () => ops.terminalHere(p, path) }] : []),
    { label: 'Copy folder path', icon: Copy, onselect: () => ops.copy(path, 'Folder path') },
    SEPARATOR,
    ...(env.control ? [{ label: 'Remove folder…', icon: FolderX, danger: true, onselect: () => ops.removeFolder(p, path) }] : []),
  ]);
}

/** A project in the Trash. */
export function trashActions(p: ProjectSummary, env: ActionEnv, ops: ProjectOps): MenuItem[] {
  return tidy([
    ...(env.admin ? [{ label: 'Restore', icon: RotateCcw, onselect: () => ops.restore(p) }] : []),
    { label: 'Copy project id', icon: Copy, onselect: () => ops.copy(p.id, 'Project id') },
  ]);
}

export interface BulkFailure {
  title: string;
  reason: string;
}

/** Most failures a bulk summary names; the rest are counted. */
export const BULK_NAMED = 3;

/**
 * One toast for a bulk action: "Deleted 3 sessions." or "Moved 2 of 3 sessions to Chats. Not moved:
 * "a" (it is running; stop it first)." `where` follows the count (" to Chats").
 */
export function bulkSummary(
  verb: string,
  notVerb: string,
  total: number,
  failures: readonly BulkFailure[],
  where = '',
): string {
  const noun = (n: number): string => (n === 1 ? 'session' : 'sessions');
  const ok = total - failures.length;
  if (failures.length === 0) return `${verb} ${total} ${noun(total)}${where}.`;
  const named = failures
    .slice(0, BULK_NAMED)
    .map((f) => `"${f.title}" (${f.reason})`)
    .join('; ');
  const more = failures.length > BULK_NAMED ? `; and ${failures.length - BULK_NAMED} more` : '';
  const head = ok === 0 ? `No ${noun(2)} ${verb.toLowerCase()}${where}.` : `${verb} ${ok} of ${total} ${noun(total)}${where}.`;
  return `${head} ${notVerb}: ${named}${more}.`;
}
