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
 * With a machine's presence unknown (standalone, not connected to the hub, or an older hub), how
 * long its live session counts as live without a replicated update (the daemon's `REMOTE_LIVE_MS`).
 */
export const REMOTE_LIVE_MS = 30 * 60_000;

/** What decides whether another machine's live status is current. */
export interface LiveContext {
  selfId: string | null;
  /** Presence by machine id (`GET /api/machines`); `online` null or missing: unknown. */
  machines: ReadonlyMap<string, { online: boolean | null }>;
  now: number;
}

/**
 * Why another machine's live status cannot be trusted: `offline` (its machine is not connected to
 * the hub, so nothing corrects the last status it reported) or `stale` (presence unknown and no
 * update for `REMOTE_LIVE_MS`); null when it can, or the session is not live.
 */
export function remoteLiveState(
  s: Pick<Session, 'status' | 'machine_id' | 'last_activity_at'>,
  ctx: LiveContext,
): 'offline' | 'stale' | null {
  if (!isLive(s.status) || ctx.selfId === null || s.machine_id === ctx.selfId) return null;
  const online = ctx.machines.get(s.machine_id)?.online ?? null;
  if (online !== null) return online ? null : 'offline';
  return ctx.now - s.last_activity_at > REMOTE_LIVE_MS ? 'stale' : null;
}

/** Sorts first in lists: live, with a status that can be trusted. */
export function isPinned(s: Session, ctx: LiveContext): boolean {
  return isLive(s.status) && remoteLiveState(s, ctx) === null;
}

/**
 * The daemon's list order (`GET /api/sessions`): pinned sessions first, since a process is
 * attached and the user can act on them even after hours of idling, then most recent activity
 * (a long-running session started yesterday but working now is not buried), then id.
 */
export function sessionOrder(ctx: LiveContext): (a: Session, b: Session) => number {
  return (a, b) => {
    const pinned = Number(isPinned(b, ctx)) - Number(isPinned(a, ctx));
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
  ctx: LiveContext,
): { shown: Session[]; hidden: number } {
  const shown = sessions.filter((s, i) => i < limit || s.id === keepId || isPinned(s, ctx));
  return { shown, hidden: sessions.length - shown.length };
}
