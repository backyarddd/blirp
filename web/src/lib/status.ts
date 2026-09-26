import type { Project, Session, SessionStatus } from './api/types';

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

const LIVE: ReadonlySet<SessionStatus> = new Set(['starting', 'working', 'idle', 'waiting']);

/** The agent process is (as far as the daemon knows) still running. */
export function isLive(status: SessionStatus): boolean {
  return LIVE.has(status);
}

/** A live PTY exists only for sessions blirp launched itself. */
export function hasTerminal(s: Pick<Session, 'origin' | 'status'>): boolean {
  return s.origin === 'blirp' && isLive(s.status);
}

/** Detached sessions can be resumed when the agent's own session id is known. */
export function canResume(s: Pick<Session, 'status' | 'agent_session_id'>): boolean {
  return (s.status === 'detached' || s.status === 'completed' || s.status === 'failed') && s.agent_session_id !== null;
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
  project: Project | undefined;
  sessions: Session[];
}

/** Groups sessions by project, preserving input order (groups ordered by their first session). */
export function groupSessions(sessions: readonly Session[], projects: ReadonlyMap<string, Project>): SessionGroup[] {
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
