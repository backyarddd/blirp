import type { ProjectSummary, Session, SessionStatus } from './api/types.gen';

export type StatusTone = 'working' | 'idle' | 'waiting' | 'completed' | 'failed' | 'detached';

export interface StatusInfo {
  label: string;
  tone: StatusTone;
  pulse: boolean;
}

const INFO: Record<SessionStatus, StatusInfo> = {
  starting: { label: 'Starting', tone: 'working', pulse: true },
  working: { label: 'Working', tone: 'working', pulse: true },
  idle: { label: 'Idle', tone: 'idle', pulse: false },
  waiting: { label: 'Waiting', tone: 'waiting', pulse: false },
  completed: { label: 'Completed', tone: 'completed', pulse: false },
  failed: { label: 'Failed', tone: 'failed', pulse: false },
  detached: { label: 'Detached', tone: 'detached', pulse: false },
};

export function statusInfo(status: SessionStatus): StatusInfo {
  return INFO[status];
}

const STOPPED: StatusInfo = { label: 'Stopped', tone: 'idle', pulse: false };

/** Status chip for a session: an ended session the user stopped reads "Stopped". */
export function sessionStatusInfo(s: Pick<Session, 'status' | 'stopped_by_user'>): StatusInfo {
  return s.stopped_by_user && !isLive(s.status) ? STOPPED : INFO[s.status];
}

/**
 * Subagent sessions ingested from an agent's transcript (§8). `continue_from` sessions also
 * carry a parent but are launched by blirp and stay top-level.
 */
export function isSubagent(s: Pick<Session, 'origin' | 'parent_session_id'>): boolean {
  return s.origin === 'external' && s.parent_session_id !== null;
}

const LIVE: ReadonlySet<SessionStatus> = new Set(['starting', 'working', 'idle', 'waiting']);

/** The agent process is (as far as the daemon knows) still running. */
export function isLive(status: SessionStatus): boolean {
  return LIVE.has(status);
}

/** A live PTY exists only for sessions blirp launched itself. */
export function hasTerminal(s: Pick<Session, 'origin' | 'status'>): boolean {
  return s.origin === 'blirp' && isLive(s.status);
}

/**
 * Ended sessions can be resumed: with the agent's own session id the agent resumes its
 * conversation; blirp-launched sessions without one relaunch fresh in the same folder (§7).
 */
export function canResume(s: Pick<Session, 'status' | 'agent_session_id' | 'origin'>): boolean {
  return !isLive(s.status) && (s.agent_session_id !== null || s.origin === 'blirp');
}

/** Status transitions worth a browser notification when the tab is unfocused. */
export function notifiableTransition(prev: SessionStatus | undefined, next: SessionStatus): boolean {
  if (prev === next) return false;
  return next === 'waiting' || next === 'completed' || next === 'failed';
}

const AGENT_NAMES: Record<string, string> = {
  claude: 'Claude Code',
  codex: 'Codex',
  opencode: 'opencode',
  pi: 'pi',
  gemini: 'Gemini CLI',
  cursor: 'Cursor CLI',
  amp: 'Amp',
  aider: 'Aider',
  dsh: 'DeepSeek Harness',
  shell: 'Shell',
};

export function agentLabel(agent: string): string {
  if (agent.startsWith('custom:')) return agent.slice('custom:'.length);
  return AGENT_NAMES[agent] ?? agent;
}

export function basename(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function sessionTitle(s: Pick<Session, 'title' | 'agent' | 'cwd'>): string {
  return s.title?.trim() || `${agentLabel(s.agent)} in ${basename(s.cwd)}`;
}

export interface SessionGroup {
  projectId: string;
  name: string;
  project: ProjectSummary | undefined;
  sessions: Session[];
}

/** Groups sessions by project, preserving input order (groups ordered by their first session). */
export function groupSessions(sessions: readonly Session[], projects: ReadonlyMap<string, ProjectSummary>): SessionGroup[] {
  const groups = new Map<string, SessionGroup>();
  for (const s of sessions) {
    let g = groups.get(s.project_id);
    if (!g) {
      const project = projects.get(s.project_id);
      g = { projectId: s.project_id, name: project?.name ?? 'Unknown project', project, sessions: [] };
      groups.set(s.project_id, g);
    }
    g.sessions.push(s);
  }
  return [...groups.values()];
}

/**
 * How long another machine's live session counts as live without a replicated update (the
 * daemon's `REMOTE_LIVE_MS`): a replica cannot tell a quiet session from a machine that went away.
 */
export const REMOTE_LIVE_MS = 30 * 60_000;

/** A live status reported by another machine that has not been updated for `REMOTE_LIVE_MS`. */
export function isStale(
  s: Pick<Session, 'status' | 'machine_id' | 'last_activity_at'>,
  selfId: string | null,
  now: number,
): boolean {
  return isLive(s.status) && selfId !== null && s.machine_id !== selfId && now - s.last_activity_at > REMOTE_LIVE_MS;
}

/** Sorts first in lists: live, and (on another machine) recently updated. */
export function isPinned(s: Session, selfId: string | null, now: number): boolean {
  return isLive(s.status) && !isStale(s, selfId, now);
}

/**
 * The daemon's list order (`GET /api/sessions`): pinned sessions first, since a process is
 * attached and the user can act on them even after hours of idling, then most recent activity
 * (a long-running session started yesterday but working now is not buried), then id.
 */
export function sessionOrder(selfId: string | null, now = Date.now()): (a: Session, b: Session) => number {
  return (a, b) => {
    const pinned = Number(isPinned(b, selfId, now)) - Number(isPinned(a, selfId, now));
    if (pinned !== 0) return pinned;
    if (a.last_activity_at !== b.last_activity_at) return b.last_activity_at - a.last_activity_at;
    return a.id < b.id ? 1 : a.id > b.id ? -1 : 0;
  };
}

/** Sessions a project group in the sidebar shows before "Show N more". */
export const GROUP_PREVIEW = 5;

/**
 * The first `limit` sessions of a group, plus any later pinned or `keepId` (selected) session, so
 * one busy project does not push every other project off screen. `hidden` is how many are left.
 */
export function previewSessions(
  sessions: readonly Session[],
  limit: number,
  keepId: string | null,
  selfId: string | null,
  now = Date.now(),
): { shown: Session[]; hidden: number } {
  const shown = sessions.filter((s, i) => i < limit || s.id === keepId || isPinned(s, selfId, now));
  return { shown, hidden: sessions.length - shown.length };
}
